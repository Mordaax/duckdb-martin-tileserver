//! `duckdb-martin-tileserver` — a proof-of-concept that serves Mapbox Vector
//! Tiles directly out of DuckDB's spatial extension, structured the way Martin's
//! own tile sources are (axum + tokio, a `Source`-like data layer, TileJSON).
//!
//! Usage:
//!   duckdb-martin-tileserver [path/to/config.yaml]   # run the tile server
//!   duckdb-martin-tileserver bench [config.yaml]     # time tile generation
//!
//! Defaults to `config.yaml` in the current directory.

// The runnable server requires the DuckDB backend (and the axum/tokio runtime
// it pulls in). When built with `--no-default-features` the binary still links
// but just prints how to enable the backend, so the DB-free core can be built
// and tested in isolation without a heavy DuckDB compile.
#[cfg(not(feature = "duckdb-backend"))]
fn main() {
    eprintln!(
        "duckdb-martin-tileserver was built with --no-default-features, so the\n\
         DuckDB-backed server is not compiled in. Rebuild with the default\n\
         features (which include `duckdb-backend`) to run the tile server:\n\
         \n    cargo run --release -- config.yaml\n"
    );
    std::process::exit(2);
}

#[cfg(feature = "duckdb-backend")]
use std::sync::Arc;

#[cfg(feature = "duckdb-backend")]
use duckdb_martin_tileserver::{config, duck, server, tile::Tile};
#[cfg(feature = "duckdb-backend")]
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[cfg(feature = "duckdb-backend")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Tracing: respect RUST_LOG, default to info.
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer())
        .init();

    // Subcommand dispatch. `bench [config]` profiles tile generation; otherwise
    // the first arg (if any) is a config path and we run the server.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("bench") {
        let config_path = args.get(2).cloned().unwrap_or_else(|| "config.yaml".to_string());
        return run_bench(&config_path);
    }

    let config_path = args
        .into_iter()
        .nth(1)
        .unwrap_or_else(|| "config.yaml".to_string());
    tracing::info!(config = %config_path, "loading configuration");

    let cfg = config::Config::load(&config_path)
        .map_err(|e| anyhow::anyhow!("config error ({config_path}): {e}"))?;

    tracing::info!(
        sources = cfg.sources.len(),
        listen = %cfg.server.listen,
        "configuration loaded"
    );

    // Open all DuckDB sources up front (fail fast on bad data/paths).
    let state = Arc::new(server::AppState::from_config(&cfg)?);

    // The demo map static files live next to the config by convention; default
    // to ./demo (overridable via the DEMO_DIR env var).
    let demo_dir = std::env::var("DEMO_DIR").unwrap_or_else(|_| "demo".to_string());
    let app = server::router(state, &demo_dir);

    let listener = tokio::net::TcpListener::bind(&cfg.server.listen).await?;
    tracing::info!(addr = %cfg.server.listen, "tile server listening");
    tracing::info!("try:   curl -s http://{}/catalog | jq", cfg.server.listen);

    axum::serve(listener, app).await?;
    Ok(())
}

/// `bench` subcommand: time tile generation for every source across a small
/// fixed pyramid of `(z,x,y)` tiles and print a millisecond table.
///
/// This isolates the **DuckDB backend** (tile math + spatial query + MVT
/// encode), with the HTTP layer and cache out of the picture. A PostGIS-vs-DuckDB
/// comparison remains deferred (it needs a provisioned PostGIS with the same
/// data) — this gives the DuckDB side of that table for real.
#[cfg(feature = "duckdb-backend")]
fn run_bench(config_path: &str) -> anyhow::Result<()> {
    use std::time::Instant;

    let cfg = config::Config::load(config_path)
        .map_err(|e| anyhow::anyhow!("config error ({config_path}): {e}"))?;

    println!("# DuckDB tile-generation benchmark");
    println!("# config: {config_path}");
    println!(
        "# (PostGIS comparison deferred — needs a provisioned PostGIS with the same data)\n"
    );

    for (name, sc) in &cfg.sources {
        let source = duck::DuckSource::open(name, sc.clone())
            .map_err(|e| anyhow::anyhow!("open source '{name}': {e}"))?;

        // A handful of tiles inside the Singapore extent across several zooms,
        // plus z0 (whole world) as a coarse case. Adjust freely.
        let tiles = bench_tiles(sc);

        println!("source: {name}");
        println!("  {:<14} {:>10} {:>8}", "tile z/x/y", "bytes", "ms");
        println!("  {:-<14} {:->10} {:->8}", "", "", "");

        let mut total_ms = 0.0f64;
        for t in &tiles {
            // One warm-up to load row groups / caches, then time the second run.
            let _ = source.tile_mvt(*t);
            let start = Instant::now();
            let bytes = match source.tile_mvt(*t) {
                Ok(b) => b,
                Err(e) => {
                    println!("  {:<14} {:>10} {:>8}  ERR: {e}", fmt_tile(t), "-", "-");
                    continue;
                }
            };
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            total_ms += ms;
            println!("  {:<14} {:>10} {:>8.3}", fmt_tile(t), bytes.len(), ms);
        }
        let n = tiles.len().max(1) as f64;
        println!("  mean: {:.3} ms/tile over {} tiles\n", total_ms / n, tiles.len());
    }
    Ok(())
}

#[cfg(feature = "duckdb-backend")]
fn fmt_tile(t: &Tile) -> String {
    format!("{}/{}/{}", t.z, t.x, t.y)
}

/// Pick a small set of tiles to benchmark for a source: the tile containing the
/// center of its bounds at several zoom levels (clamped to the source range),
/// plus the z0 world tile.
#[cfg(feature = "duckdb-backend")]
fn bench_tiles(sc: &config::SourceConfig) -> Vec<Tile> {
    let [w, s, e, n] = sc.tilejson_bounds();
    let clon = (w + e) / 2.0;
    let clat = (s + n) / 2.0;

    let mut tiles = vec![Tile::new(0, 0, 0)];
    for z in [8u8, 10, 12, 14, 16] {
        if z < sc.minzoom || z > sc.maxzoom {
            continue;
        }
        let (x, y) = lonlat_to_tile(clon, clat, z);
        tiles.push(Tile::new(z, x, y));
    }
    tiles
}

/// Standard slippy-map lon/lat -> tile x/y at zoom z (Web Mercator).
#[cfg(feature = "duckdb-backend")]
fn lonlat_to_tile(lon: f64, lat: f64, z: u8) -> (u32, u32) {
    let n = (1u64 << z) as f64;
    let x = ((lon + 180.0) / 360.0 * n).floor();
    let lat_rad = lat.to_radians();
    let y = ((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n)
        .floor();
    let max = (1u64 << z).saturating_sub(1) as f64;
    (x.clamp(0.0, max) as u32, y.clamp(0.0, max) as u32)
}
