//! axum HTTP server: routes, handlers, TileJSON, and the request flow that ties
//! tile math -> DuckDB SQL -> MVT -> cache together.
//!
//! Routes (mirroring Martin's URL shape):
//!   * `GET /`                              -> HTML index / health landing page
//!   * `GET /catalog`                       -> JSON list of configured sources
//!   * `GET /health`                        -> liveness probe
//!   * `GET /demo` and `GET /demo/*`        -> static demo map (MapLibre)
//!   * `GET /{source}`                      -> TileJSON for the source
//!   * `GET /{source}/{z}/{x}/{y}`          -> MVT tile (also `.mvt` / `.pbf`)
//!
//! Each source owns a mutex-guarded DuckDB connection (the `duckdb` crate's
//! `Connection` is `Send` but not `Sync`). Tile rendering is CPU/IO heavy and
//! blocking, so handlers run it on `tokio::task::spawn_blocking`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;

use crate::cache::{SharedTileCache, TileKey};
use crate::compress::{accepts_gzip, gzip};
use crate::config::{Config, SourceConfig};
use crate::duck::DuckSource;
use crate::tile::Tile;

/// Shared application state handed to every handler.
pub struct AppState {
    /// Per-source DuckDB connections (each guarded by its own mutex).
    pub sources: BTreeMap<String, Mutex<DuckSource>>,
    /// Per-source config (cheap to clone for TileJSON).
    pub configs: BTreeMap<String, SourceConfig>,
    pub cache: SharedTileCache,
    pub public_url: Option<String>,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    /// Build state by opening every configured source.
    pub fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        let mut sources = BTreeMap::new();
        let mut configs = BTreeMap::new();
        for (name, sc) in &cfg.sources {
            let ds = DuckSource::open(name, sc.clone())
                .map_err(|e| anyhow::anyhow!("failed to open source '{name}': {e}"))?;
            sources.insert(name.clone(), Mutex::new(ds));
            configs.insert(name.clone(), sc.clone());
        }
        Ok(Self {
            sources,
            configs,
            cache: SharedTileCache::new(cfg.server.cache_size),
            public_url: cfg.server.public_url.clone(),
        })
    }
}

/// Build the axum router.
///
/// `demo_dir` is the directory holding the static MapLibre demo (`index.html`
/// + sample data); it is served under `/demo`. Pass the path from config / CWD.
pub fn router(state: SharedState, demo_dir: impl Into<std::path::PathBuf>) -> Router {
    use tower_http::cors::CorsLayer;
    use tower_http::services::ServeDir;
    use tower_http::trace::TraceLayer;

    // Serve the demo map static files. `/demo` -> index.html (ServeDir appends
    // index.html for directory requests).
    let demo_service = ServeDir::new(demo_dir.into()).append_index_html_on_directories(true);

    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/catalog", get(catalog))
        .nest_service("/demo", demo_service)
        .route("/:source", get(tilejson))
        .route("/:source/:z/:x/:y", get(tile))
        .with_state(state)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
}

// ------------------------------- handlers -----------------------------------

/// Root landing page: doubles as a health/index endpoint. Lists configured
/// sources with quick links to their TileJSON and to the demo map.
async fn index(State(state): State<SharedState>) -> impl IntoResponse {
    let mut rows = String::new();
    for (name, sc) in &state.configs {
        let desc = sc.description.clone().unwrap_or_else(|| name.clone());
        rows.push_str(&format!(
            "<li><strong>{name}</strong> — {desc} \
             [<a href=\"/{name}\">TileJSON</a>] \
             <code>/{name}/{{z}}/{{x}}/{{y}}</code> \
             (z{min}–{max})</li>",
            name = name,
            desc = desc,
            min = sc.minzoom,
            max = sc.maxzoom,
        ));
    }
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>DuckDB Martin-style Tile Server</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:720px;margin:40px auto;padding:0 16px;line-height:1.5}}\
         code{{background:#eee;padding:1px 5px;border-radius:3px}}</style></head><body>\
         <h1>DuckDB → MVT Tile Server</h1>\
         <p>Mapbox Vector Tiles served from DuckDB spatial, Martin-style.</p>\
         <p><a href=\"/demo/\">▶ Open the demo map</a> · \
            <a href=\"/catalog\">/catalog</a> · <a href=\"/health\">/health</a></p>\
         <h2>Sources</h2><ul>{rows}</ul></body></html>"
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// List configured sources with a short summary.
async fn catalog(State(state): State<SharedState>) -> impl IntoResponse {
    let tiles: Vec<_> = state
        .configs
        .iter()
        .map(|(name, sc)| {
            json!({
                "id": name,
                "name": sc.description.clone().unwrap_or_else(|| name.clone()),
                "layer": sc.layer(),
                "minzoom": sc.minzoom,
                "maxzoom": sc.maxzoom,
                "bounds": sc.tilejson_bounds(),
                "content_type": "application/x-protobuf",
                "content_encoding": "gzip",
                "tilejson_url": format!("/{name}"),
                "tile_url": format!("/{name}/{{z}}/{{x}}/{{y}}"),
            })
        })
        .collect();
    Json(json!({ "tiles": tiles }))
}

/// TileJSON 3.0.0 document so MapLibre/Mapbox can auto-configure the layer.
async fn tilejson(
    State(state): State<SharedState>,
    Path(source): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(sc) = state.configs.get(&source) else {
        return not_found(&source);
    };

    let base = base_url(&state, &headers);
    let tiles_url = format!("{base}/{source}/{{z}}/{{x}}/{{y}}");
    let bounds = sc.tilejson_bounds();
    let center_lon = (bounds[0] + bounds[2]) / 2.0;
    let center_lat = (bounds[1] + bounds[3]) / 2.0;

    let doc = json!({
        "tilejson": "3.0.0",
        "name": sc.description.clone().unwrap_or_else(|| source.clone()),
        "scheme": "xyz",
        "tiles": [tiles_url],
        "minzoom": sc.minzoom,
        "maxzoom": sc.maxzoom,
        "bounds": bounds,
        "center": [center_lon, center_lat, sc.minzoom],
        "vector_layers": [
            {
                "id": sc.layer(),
                "minzoom": sc.minzoom,
                "maxzoom": sc.maxzoom,
                "fields": fields_object(sc),
            }
        ],
    });
    Json(doc).into_response()
}

/// Render (or serve cached) an MVT tile.
async fn tile(
    State(state): State<SharedState>,
    Path((source, z, x, raw_y)): Path<(String, u8, u32, String)>,
    headers: HeaderMap,
) -> Response {
    let want_gzip = accepts_gzip(
        headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|h| h.to_str().ok()),
    );
    // Strip an optional extension on the {y} segment (.mvt / .pbf).
    let y_str = raw_y
        .strip_suffix(".mvt")
        .or_else(|| raw_y.strip_suffix(".pbf"))
        .or_else(|| raw_y.strip_suffix(".vector.pbf"))
        .unwrap_or(&raw_y);
    let Ok(y) = y_str.parse::<u32>() else {
        return (StatusCode::BAD_REQUEST, "invalid y coordinate").into_response();
    };

    let Some(sc) = state.configs.get(&source) else {
        return not_found(&source);
    };

    if z < sc.minzoom || z > sc.maxzoom {
        return (StatusCode::NO_CONTENT, "zoom out of range").into_response();
    }

    let t = Tile::new(z, x, y);
    if !t.is_valid() {
        return (StatusCode::BAD_REQUEST, "tile coordinate out of range").into_response();
    }

    // Cache lookup. The cache stores *uncompressed* MVT bytes; compression (if
    // requested) happens per-response so the same cached tile serves both
    // gzip-capable and plain clients.
    let key = TileKey::new(source.clone(), z, x, y);
    if let Some(bytes) = state.cache.get(&key) {
        return mvt_response(bytes, true, want_gzip);
    }

    // Render off the async runtime: DuckDB calls are blocking.
    let state2 = state.clone();
    let source2 = source.clone();
    let result = tokio::task::spawn_blocking(move || {
        let guard = state2
            .sources
            .get(&source2)
            .expect("source existence checked above")
            .lock()
            .expect("duck source mutex poisoned");
        guard.tile_mvt(t)
    })
    .await;

    match result {
        Ok(Ok(bytes)) => {
            state.cache.put(key, bytes.clone());
            mvt_response(bytes, false, want_gzip)
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, source, z, x, y, "tile render failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("render error: {e}"),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "blocking task join failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

// ------------------------------- helpers ------------------------------------

/// Build an MVT HTTP response. An empty body is a valid empty tile -> 204.
///
/// If `want_gzip` is set the body is gzip-compressed and `Content-Encoding:
/// gzip` is advertised. The input `bytes` are always the raw (uncompressed) MVT;
/// the cache stores those, so the same tile can serve both gzip and plain
/// clients. If compression fails for any reason we fall back to the raw bytes.
fn mvt_response(bytes: Vec<u8>, cached: bool, want_gzip: bool) -> Response {
    if bytes.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/x-protobuf"),
    );
    headers.insert(
        header::HeaderName::from_static("x-tile-cache"),
        header::HeaderValue::from_static(if cached { "hit" } else { "miss" }),
    );

    let body = if want_gzip {
        match gzip(&bytes) {
            Ok(gz) => {
                headers.insert(
                    header::CONTENT_ENCODING,
                    header::HeaderValue::from_static("gzip"),
                );
                gz
            }
            Err(e) => {
                tracing::warn!(error = %e, "gzip failed; serving uncompressed");
                bytes
            }
        }
    } else {
        bytes
    };

    (StatusCode::OK, headers, body).into_response()
}

fn not_found(source: &str) -> Response {
    (StatusCode::NOT_FOUND, format!("unknown source: {source}")).into_response()
}

/// Determine the public base URL for TileJSON links.
fn base_url(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(u) = &state.public_url {
        return u.trim_end_matches('/').to_string();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:3000");
    // We don't terminate TLS here; assume http for the PoC.
    format!("http://{host}")
}

/// Map configured property columns to a TileJSON `fields` object. We don't know
/// SQL types without querying, so advertise them generically.
fn fields_object(sc: &SourceConfig) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for p in &sc.properties {
        m.insert(p.clone(), json!(""));
    }
    serde_json::Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DataSource;

    fn sc() -> SourceConfig {
        SourceConfig {
            description: Some("Singapore rainfall sensors".into()),
            data: DataSource::Table,
            table: "sensors".into(),
            layer_name: None,
            geometry_column: "geom".into(),
            geometry_encoding: crate::config::GeometryEncoding::Native,
            srid: 4326,
            properties: vec!["station_id".into(), "rainfall_mm".into()],
            minzoom: 0,
            maxzoom: 16,
            bounds: Some([103.6, 1.2, 104.1, 1.5]),
            extent: 4096,
            buffer: 64,
        }
    }

    #[test]
    fn fields_object_lists_properties() {
        let f = fields_object(&sc());
        let obj = f.as_object().unwrap();
        assert!(obj.contains_key("station_id"));
        assert!(obj.contains_key("rainfall_mm"));
        assert_eq!(obj.len(), 2);
    }

    #[test]
    fn base_url_prefers_public_url() {
        let state = AppState {
            sources: BTreeMap::new(),
            configs: BTreeMap::new(),
            cache: SharedTileCache::new(4),
            public_url: Some("https://tiles.example.com/".into()),
        };
        let h = HeaderMap::new();
        assert_eq!(base_url(&state, &h), "https://tiles.example.com");
    }

    #[test]
    fn mvt_response_empty_is_204() {
        let r = mvt_response(Vec::new(), false, false);
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn mvt_response_plain_has_no_content_encoding() {
        let r = mvt_response(vec![1, 2, 3, 4], false, false);
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r.headers().get(header::CONTENT_ENCODING).is_none());
        assert_eq!(
            r.headers().get("x-tile-cache").unwrap(),
            "miss"
        );
    }

    #[test]
    fn mvt_response_gzip_sets_content_encoding() {
        let r = mvt_response(vec![7u8; 256], true, true);
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            r.headers().get(header::CONTENT_ENCODING).unwrap(),
            "gzip"
        );
        assert_eq!(r.headers().get("x-tile-cache").unwrap(), "hit");
    }

    #[test]
    fn base_url_falls_back_to_host_header() {
        let state = AppState {
            sources: BTreeMap::new(),
            configs: BTreeMap::new(),
            cache: SharedTileCache::new(4),
            public_url: None,
        };
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "myhost:9000".parse().unwrap());
        assert_eq!(base_url(&state, &h), "http://myhost:9000");
    }
}
