//! DuckDB spatial data layer.
//!
//! Responsibilities:
//!   1. Open an (in-process) DuckDB connection and `INSTALL spatial; LOAD spatial;`.
//!   2. Materialize each configured source (load GeoParquet / GeoJSON, or attach
//!      an existing DuckDB file).
//!   3. On a tile request, run a parameterized spatial query that:
//!        a. computes the tile's Web-Mercator envelope,
//!        b. selects features intersecting it,
//!        c. transforms+clips geometry into MVT tile space and encodes the tile
//!           with DuckDB's own `ST_AsMVTGeom` + `ST_AsMVT` aggregate.
//!
//! ## MVT strategy & fallback
//!
//! The **primary path** uses DuckDB spatial's MVT functions directly:
//!
//! ```sql
//! WITH bounds AS (SELECT ? AS minx, ? AS miny, ? AS maxx, ? AS maxy),
//! mvtgeom AS (
//!   SELECT
//!     ST_AsMVTGeom(
//!       ST_Transform(t.geom, 'EPSG:4326', 'EPSG:3857'),   -- to mercator
//!       {extent}, {buffer}, ...                            -- tile space
//!     ) AS geom,
//!     t.col1, t.col2, ...
//!   FROM tbl t, bounds b
//!   WHERE ST_Intersects(
//!     ST_Transform(t.geom, 'EPSG:4326', 'EPSG:3857'),
//!     ST_MakeEnvelope(b.minx, b.miny, b.maxx, b.maxy))
//! )
//! SELECT ST_AsMVT(mvtgeom, '{layer}', {extent}, 'geom') FROM mvtgeom;
//! ```
//!
//! `ST_AsMVT` is an aggregate that returns the encoded protobuf `BLOB` for the
//! whole layer. We read column 0 of the single result row as `Vec<u8>`.
//!
//! **Fallback path** (`src/mvt_fallback.rs`): some builds of the DuckDB spatial
//! extension ship `ST_AsMVTGeom` but not the `ST_AsMVT` *aggregate*. In that
//! case we select clipped geometries as WKB plus their properties and encode the
//! MVT in pure Rust with `geozero`. `Self::tile_mvt` tries the DuckDB aggregate
//! first and transparently falls back. The tradeoff: the fallback round-trips
//! geometry as WKB and encodes in-process (a bit more CPU/alloc per tile) but
//! removes the dependency on a specific spatial-extension version.

use duckdb::Connection;

use crate::config::{DataSource, SourceConfig};
use crate::tile::Tile;

/// Errors from the data layer.
#[derive(Debug, thiserror::Error)]
pub enum DuckError {
    #[error("duckdb error: {0}")]
    Duck(#[from] duckdb::Error),
    #[error("mvt fallback error: {0}")]
    Mvt(String),
    #[error("unknown source: {0}")]
    UnknownSource(String),
}

/// SQL builders, separated out so they can be unit-tested without a live DB.
pub mod sql {
    use crate::config::SourceConfig;

    /// Quote/escape a SQL string literal (single-quote doubling).
    pub fn quote_literal(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }

    /// Quote a SQL identifier (double-quote doubling) to be injection-safe even
    /// though config is operator-controlled.
    pub fn quote_ident(s: &str) -> String {
        format!("\"{}\"", s.replace('"', "\"\""))
    }

    /// Build the `CREATE TABLE ... AS SELECT ...` statement that loads a source's
    /// data at startup. Returns `None` for sources that need no load (`Table`,
    /// `Duckdb` - those are attached/queried in place).
    pub fn load_statement(src: &SourceConfig) -> Option<String> {
        use crate::config::DataSource::*;
        match &src.data {
            Parquet { path } => Some(format!(
                "CREATE TABLE IF NOT EXISTS {tbl} AS SELECT * FROM read_parquet({p});",
                tbl = quote_ident(&src.table),
                p = quote_literal(path),
            )),
            Geojson { path } => Some(format!(
                "CREATE TABLE IF NOT EXISTS {tbl} AS SELECT * FROM ST_Read({p});",
                tbl = quote_ident(&src.table),
                p = quote_literal(path),
            )),
            Duckdb { .. } | Table => None,
        }
    }

    /// The raw geometry column expression, decoding it from WKB first if the
    /// source declares its geometry column is stored as WKB bytes (the common
    /// GeoParquet-on-disk case). For native GEOMETRY columns this is just the
    /// qualified column reference.
    pub fn decoded_geom_expr(src: &SourceConfig, alias: &str) -> String {
        use crate::config::GeometryEncoding::*;
        let col = format!("{alias}.{}", quote_ident(&src.geometry_column));
        match src.geometry_encoding {
            Native => col,
            Wkb => format!("ST_GeomFromWKB({col})"),
        }
    }

    /// The geometry expression, decoded from WKB if needed and transformed to
    /// Web Mercator (EPSG:3857) if the source SRID differs. DuckDB's
    /// `ST_Transform` takes textual CRS names.
    pub fn mercator_geom_expr(src: &SourceConfig, alias: &str) -> String {
        let geom = decoded_geom_expr(src, alias);
        if src.srid == 3857 {
            geom
        } else {
            format!(
                "ST_Transform({geom}, 'EPSG:{srid}', 'EPSG:3857', always_xy := true)",
                srid = src.srid
            )
        }
    }

    /// A `BOX_2D` literal for the tile envelope, built from the four bound
    /// params. `ST_AsMVTGeom` expects a `BOX_2D`, and `ST_Intersects` accepts a
    /// `ST_Extent(...)` box widened to a polygon; building the box once (rather
    /// than aggregating `ST_Extent(ST_MakeEnvelope(...))` over the bounds CTE)
    /// is both correct and cheaper.
    pub fn tile_box_expr() -> &'static str {
        "{'min_x': b.minx, 'min_y': b.miny, 'max_x': b.maxx, 'max_y': b.maxy}::BOX_2D"
    }

    /// Build the primary MVT query (DuckDB `ST_AsMVT` aggregate path).
    ///
    /// Bound parameters, in order: `minx, miny, maxx, maxy` (mercator meters).
    pub fn mvt_query(src: &SourceConfig) -> String {
        let geom = mercator_geom_expr(src, "t");
        let props: String = src
            .properties
            .iter()
            .map(|p| format!(", t.{}", quote_ident(p)))
            .collect();

        format!(
            "WITH bounds AS (SELECT ?::DOUBLE AS minx, ?::DOUBLE AS miny, \
                                    ?::DOUBLE AS maxx, ?::DOUBLE AS maxy), \
             mvtgeom AS ( \
               SELECT ST_AsMVTGeom({geom}, {box}, \
                          {extent}, {buffer}, true) AS geom{props} \
               FROM {tbl} t, bounds b \
               WHERE ST_Intersects({geom}, \
                          ST_MakeEnvelope(b.minx, b.miny, b.maxx, b.maxy)) \
             ) \
             SELECT ST_AsMVT(mvtgeom, {layer}, {extent}, 'geom') AS tile \
             FROM mvtgeom \
             WHERE mvtgeom.geom IS NOT NULL;",
            geom = geom,
            box = tile_box_expr(),
            tbl = quote_ident(&src.table),
            layer = quote_literal(src.layer()),
            extent = src.extent,
            buffer = src.buffer,
            props = props,
        )
    }

    /// Build the fallback query: select clipped MVT geometry as WKB plus the
    /// requested property columns, to be encoded by the pure-Rust encoder.
    ///
    /// Bound parameters, in order: `minx, miny, maxx, maxy`.
    pub fn mvt_fallback_query(src: &SourceConfig) -> String {
        let geom = mercator_geom_expr(src, "t");
        let props: String = src
            .properties
            .iter()
            .map(|p| format!(", t.{} AS {}", quote_ident(p), quote_ident(p)))
            .collect();

        format!(
            "WITH bounds AS (SELECT ?::DOUBLE AS minx, ?::DOUBLE AS miny, \
                                    ?::DOUBLE AS maxx, ?::DOUBLE AS maxy) \
             SELECT ST_AsWKB( \
                        ST_AsMVTGeom({geom}, {box}, \
                            {extent}, {buffer}, true)) AS geom{props} \
             FROM {tbl} t, bounds b \
             WHERE ST_Intersects({geom}, \
                        ST_MakeEnvelope(b.minx, b.miny, b.maxx, b.maxy));",
            geom = geom,
            box = tile_box_expr(),
            tbl = quote_ident(&src.table),
            extent = src.extent,
            buffer = src.buffer,
            props = props,
        )
    }
}

/// A live DuckDB handle plus the source it serves. The `duckdb` crate's
/// `Connection` is not `Sync`, so the server wraps this in a `Mutex` (or one
/// per worker). For a PoC a single mutex-guarded connection is sufficient.
pub struct DuckSource {
    pub name: String,
    pub config: SourceConfig,
    conn: Connection,
    /// Whether the DuckDB `ST_AsMVT` aggregate is available (probed once).
    has_native_mvt: bool,
}

impl DuckSource {
    /// Open a fresh in-process DuckDB, load spatial, and materialize the source.
    pub fn open(name: &str, config: SourceConfig) -> Result<Self, DuckError> {
        let conn = match &config.data {
            DataSource::Duckdb { path } => Connection::open(path)?,
            _ => Connection::open_in_memory()?,
        };
        Self::init_spatial(&conn)?;
        if let Some(stmt) = sql::load_statement(&config) {
            tracing::info!(source = name, "loading source data");
            conn.execute_batch(&stmt)?;
        }
        let has_native_mvt = Self::probe_native_mvt(&conn);
        tracing::info!(source = name, native_mvt = has_native_mvt, "source ready");
        Ok(Self {
            name: name.to_string(),
            config,
            conn,
            has_native_mvt,
        })
    }

    /// Install + load the spatial extension. `INSTALL` is a no-op if already
    /// present; in offline environments a pre-bundled extension is used.
    fn init_spatial(conn: &Connection) -> Result<(), DuckError> {
        conn.execute_batch("INSTALL spatial; LOAD spatial;")?;
        Ok(())
    }

    /// Probe whether the `ST_AsMVT` aggregate exists in this spatial build.
    fn probe_native_mvt(conn: &Connection) -> bool {
        // duckdb_functions() lists all registered functions.
        let q = "SELECT count(*) FROM duckdb_functions() \
                 WHERE lower(function_name) = 'st_asmvt';";
        match conn.query_row(q, [], |row| row.get::<_, i64>(0)) {
            Ok(n) => n > 0,
            Err(_) => false,
        }
    }

    /// Render a tile to MVT protobuf bytes. Returns an empty `Vec` for an empty
    /// (no-feature) tile, which is a valid 0-byte MVT.
    pub fn tile_mvt(&self, tile: Tile) -> Result<Vec<u8>, DuckError> {
        let b = tile.mercator_bounds();
        let params = [b.min_x, b.min_y, b.max_x, b.max_y];

        if self.has_native_mvt {
            self.tile_mvt_native(&params)
        } else {
            self.tile_mvt_fallback(&params)
        }
    }

    /// Primary path: let DuckDB's `ST_AsMVT` aggregate produce the blob.
    fn tile_mvt_native(&self, params: &[f64; 4]) -> Result<Vec<u8>, DuckError> {
        let q = sql::mvt_query(&self.config);
        let mut stmt = self.conn.prepare(&q)?;
        let bytes: Option<Vec<u8>> = stmt
            .query_row(
                duckdb::params![params[0], params[1], params[2], params[3]],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .or_else(|e| match e {
                // No rows (empty tile) -> empty MVT.
                duckdb::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(bytes.unwrap_or_default())
    }

    /// Fallback path: pull clipped WKB + properties and encode MVT in Rust.
    fn tile_mvt_fallback(&self, params: &[f64; 4]) -> Result<Vec<u8>, DuckError> {
        let q = sql::mvt_fallback_query(&self.config);
        let mut stmt = self.conn.prepare(&q)?;

        let prop_names = self.config.properties.clone();
        let mut rows = stmt.query(duckdb::params![params[0], params[1], params[2], params[3]])?;

        let mut features: Vec<crate::mvt_fallback::FeatureRow> = Vec::new();
        while let Some(row) = rows.next()? {
            let wkb: Option<Vec<u8>> = row.get(0)?;
            let wkb = match wkb {
                Some(w) if !w.is_empty() => w,
                _ => continue, // clipped away
            };
            let mut props = Vec::with_capacity(prop_names.len());
            for (i, name) in prop_names.iter().enumerate() {
                let v = crate::mvt_fallback::read_value(row, i + 1)?;
                props.push((name.clone(), v));
            }
            features.push(crate::mvt_fallback::FeatureRow { wkb, props });
        }

        crate::mvt_fallback::encode_tile(self.config.layer(), self.config.extent, &features)
            .map_err(DuckError::Mvt)
    }

    /// Approximate feature count (for diagnostics / TileJSON enrichment).
    pub fn feature_count(&self) -> Result<i64, DuckError> {
        let q = format!(
            "SELECT count(*) FROM {};",
            sql::quote_ident(&self.config.table)
        );
        Ok(self.conn.query_row(&q, [], |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::sql::*;
    use crate::config::{DataSource, GeometryEncoding, SourceConfig};

    fn src() -> SourceConfig {
        SourceConfig {
            description: None,
            data: DataSource::Parquet {
                path: "demo/sg.parquet".into(),
            },
            table: "sensors".into(),
            layer_name: None,
            geometry_column: "geom".into(),
            geometry_encoding: GeometryEncoding::Native,
            srid: 4326,
            properties: vec!["station_id".into(), "rainfall_mm".into()],
            minzoom: 0,
            maxzoom: 16,
            bounds: None,
            extent: 4096,
            buffer: 64,
        }
    }

    #[test]
    fn quote_literal_escapes() {
        assert_eq!(quote_literal("a'b"), "'a''b'");
        assert_eq!(quote_literal("/tmp/x.parquet"), "'/tmp/x.parquet'");
    }

    #[test]
    fn quote_ident_escapes() {
        assert_eq!(quote_ident("geom"), "\"geom\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn load_statement_for_parquet() {
        let s = load_statement(&src()).unwrap();
        assert!(s.contains("CREATE TABLE IF NOT EXISTS \"sensors\""));
        assert!(s.contains("read_parquet('demo/sg.parquet')"));
    }

    #[test]
    fn load_statement_for_geojson() {
        let mut c = src();
        c.data = DataSource::Geojson {
            path: "demo/sg.geojson".into(),
        };
        let s = load_statement(&c).unwrap();
        assert!(s.contains("ST_Read('demo/sg.geojson')"));
    }

    #[test]
    fn load_statement_none_for_table() {
        let mut c = src();
        c.data = DataSource::Table;
        assert!(load_statement(&c).is_none());
    }

    #[test]
    fn mercator_expr_transforms_4326() {
        let e = mercator_geom_expr(&src(), "t");
        assert!(e.contains("ST_Transform"));
        assert!(e.contains("EPSG:4326"));
        assert!(e.contains("EPSG:3857"));
    }

    #[test]
    fn mercator_expr_passthrough_3857() {
        let mut c = src();
        c.srid = 3857;
        let e = mercator_geom_expr(&c, "t");
        assert!(!e.contains("ST_Transform"));
        assert_eq!(e, "t.\"geom\"");
    }

    #[test]
    fn wkb_encoding_wraps_geom_from_wkb() {
        // GeoParquet-on-disk: geom stored as WKB blob -> must decode first.
        let mut c = src();
        c.geometry_encoding = GeometryEncoding::Wkb;
        c.srid = 3857; // isolate the decode from the transform
        let e = mercator_geom_expr(&c, "t");
        assert_eq!(e, "ST_GeomFromWKB(t.\"geom\")");

        // With a transform too, the WKB decode is nested inside ST_Transform.
        c.srid = 4326;
        let e2 = mercator_geom_expr(&c, "t");
        assert!(e2.contains("ST_GeomFromWKB(t.\"geom\")"));
        assert!(e2.contains("ST_Transform"));
    }

    #[test]
    fn native_encoding_does_not_decode() {
        let c = src(); // Native by default
        assert_eq!(decoded_geom_expr(&c, "t"), "t.\"geom\"");
    }

    #[test]
    fn queries_use_box2d_envelope_for_asmvtgeom() {
        // ST_AsMVTGeom takes a BOX_2D; we must not feed it an aggregate
        // ST_Extent(ST_MakeEnvelope(...)) in scalar position.
        let q = mvt_query(&src());
        assert!(q.contains("::BOX_2D"), "AsMVTGeom needs a BOX_2D envelope");
        assert!(
            !q.contains("ST_Extent"),
            "should not aggregate ST_Extent for a single tile box"
        );
        // ST_Intersects still uses a polygon envelope.
        assert!(q.contains("ST_MakeEnvelope"));
        let f = mvt_fallback_query(&src());
        assert!(f.contains("::BOX_2D"));
    }

    #[test]
    fn mvt_query_has_expected_pieces() {
        let q = mvt_query(&src());
        assert!(q.contains("ST_AsMVTGeom"));
        assert!(q.contains("ST_AsMVT(mvtgeom"));
        assert!(q.contains("ST_Intersects"));
        assert!(q.contains("ST_MakeEnvelope"));
        assert!(q.contains("'sensors'")); // layer name == table
        assert!(q.contains("4096")); // extent
        assert!(q.contains("t.\"station_id\""));
        assert!(q.contains("t.\"rainfall_mm\""));
        // four bind params
        assert_eq!(q.matches('?').count(), 4);
    }

    #[test]
    fn fallback_query_selects_wkb() {
        let q = mvt_fallback_query(&src());
        assert!(q.contains("ST_AsWKB"));
        assert!(q.contains("ST_AsMVTGeom"));
        assert_eq!(q.matches('?').count(), 4);
    }
}
