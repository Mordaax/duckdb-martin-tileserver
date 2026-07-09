//! Integration tests exercising the pure-logic modules through the public
//! library API (no DuckDB / network required). These complement the in-module
//! unit tests and confirm the crate's surface composes as intended.

use duckdb_martin_tileserver::cache::{TileCache, TileKey};
use duckdb_martin_tileserver::compress::{accepts_gzip, gzip};
use duckdb_martin_tileserver::config::{Config, GeometryEncoding};
use duckdb_martin_tileserver::tile::Tile;

// The SQL-builder tests below exercise `duck::sql`, which only exists when the
// `duckdb-backend` feature is on. They are plain string builders (no live DB),
// so they run fine in a normal `cargo test`, but are skipped under
// `--no-default-features` where the module isn't compiled.
#[cfg(feature = "duckdb-backend")]
use duckdb_martin_tileserver::config::DataSource;
#[cfg(feature = "duckdb-backend")]
use duckdb_martin_tileserver::duck::sql;

#[test]
fn tile_math_and_cache_compose() {
    // Compute a tile envelope, then cache a fake rendered tile keyed by it.
    let t = Tile::new(12, 3229, 2032); // Singapore
    let b = t.mercator_bounds();
    assert!(b.max_x > b.min_x && b.max_y > b.min_y);

    let mut cache = TileCache::new(16);
    let key = TileKey::new("sg_rainfall", t.z, t.x, t.y);
    assert!(cache.get(&key).is_none());
    cache.put(key.clone(), b"fake-mvt".to_vec());
    assert_eq!(cache.get(&key), Some(b"fake-mvt".to_vec()));
}

#[cfg(feature = "duckdb-backend")]
#[test]
fn config_drives_sql_generation() {
    let yaml = r#"
server:
  listen: "127.0.0.1:3000"
sources:
  sg_rainfall:
    description: "Singapore rainfall sensors"
    data:
      type: parquet
      path: "demo/sg_flood_sensors.parquet"
    table: sg_flood_sensors
    geometry_column: geom
    srid: 4326
    properties: [station_id, rainfall_mm]
    minzoom: 0
    maxzoom: 16
"#;
    let cfg = Config::from_yaml_str(yaml).expect("config parses");
    let src = &cfg.sources["sg_rainfall"];

    // Load statement should reference read_parquet on the configured path.
    let load = sql::load_statement(src).expect("parquet has a load stmt");
    assert!(load.contains("read_parquet('demo/sg_flood_sensors.parquet')"));

    // The MVT query should be parameterized and reference the MVT functions.
    let q = sql::mvt_query(src);
    assert!(q.contains("ST_AsMVTGeom"));
    assert!(q.contains("ST_AsMVT(mvtgeom"));
    assert_eq!(q.matches('?').count(), 4, "four envelope bind params");
    assert!(
        q.contains("ST_Transform"),
        "4326 source must transform to 3857"
    );

    // SRID 3857 should skip the transform.
    let mut merc = src.clone();
    merc.srid = 3857;
    let q2 = sql::mvt_query(&merc);
    assert!(!q2.contains("ST_Transform"));
}

#[test]
fn gzip_compresses_and_round_trips_an_mvt_blob() {
    // Use a realistic-ish MVT-shaped blob (repeated structure compresses well).
    let mvt: Vec<u8> = (0..2048).map(|i| (i % 13) as u8).collect();
    assert!(accepts_gzip(Some("gzip, deflate")));
    let gz = gzip(&mvt).expect("gzip");
    assert_eq!(&gz[..2], &[0x1f, 0x8b], "gzip magic");
    assert!(gz.len() < mvt.len(), "should shrink");

    // Decompress to confirm fidelity.
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut out = Vec::new();
    GzDecoder::new(&gz[..]).read_to_end(&mut out).unwrap();
    assert_eq!(out, mvt);
}

#[test]
fn multi_source_config_routes_to_distinct_endpoints() {
    // The real config.yaml ships two sources (GeoJSON + GeoParquet). Parsing it
    // should yield both, with distinct tables and the parquet one declaring WKB
    // geometry encoding. This is the multi-source routing contract (each becomes
    // its own /{source}/{z}/{x}/{y} route).
    let yaml = std::fs::read_to_string("config.yaml").expect("read config.yaml");
    let cfg = Config::from_yaml_str(&yaml).expect("config.yaml parses");
    assert!(
        cfg.sources.len() >= 2,
        "expected >= 2 sources, got {}",
        cfg.sources.len()
    );
    let geojson = &cfg.sources["sg_rainfall"];
    let parquet = &cfg.sources["sg_rainfall_pq"];
    assert_ne!(geojson.table, parquet.table, "distinct backing tables");
    assert_eq!(parquet.geometry_encoding, GeometryEncoding::Wkb);
    assert_eq!(geojson.geometry_encoding, GeometryEncoding::Native);
}

#[test]
fn geoparquet_config_parses_with_wkb_encoding() {
    let yaml = r#"
sources:
  pq:
    description: "GeoParquet analytics source"
    data:
      type: parquet
      path: "demo/sg_flood_sensors.parquet"
    table: sensors_pq
    geometry_column: geom
    geometry_encoding: wkb
    srid: 4326
    properties: [station_id, rainfall_mm]
    minzoom: 0
    maxzoom: 16
"#;
    let cfg = Config::from_yaml_str(yaml).expect("parses");
    let s = &cfg.sources["pq"];
    assert_eq!(s.geometry_encoding, GeometryEncoding::Wkb);
    assert_eq!(s.table, "sensors_pq");
}

#[cfg(feature = "duckdb-backend")]
#[test]
fn geoparquet_wkb_source_decodes_via_geomfromwkb() {
    // SQL builders only exist with the duckdb-backend feature, but they're pure
    // string builders (no live DB), so they run in a normal `cargo test`.
    let yaml = r#"
sources:
  pq:
    data:
      type: parquet
      path: "demo/sg_flood_sensors.parquet"
    table: sensors_pq
    geometry_column: geom
    geometry_encoding: wkb
    srid: 4326
"#;
    let cfg = Config::from_yaml_str(yaml).unwrap();
    let src = &cfg.sources["pq"];
    let q = sql::mvt_query(src);
    assert!(
        q.contains("ST_GeomFromWKB"),
        "WKB-encoded parquet geometry must be decoded: {q}"
    );
    assert!(q.contains("ST_Transform"), "4326 -> 3857");
    assert!(q.contains("::BOX_2D"), "AsMVTGeom needs a BOX_2D envelope");
}

#[cfg(feature = "duckdb-backend")]
#[test]
fn geojson_source_uses_st_read() {
    let yaml = r#"
sources:
  s:
    data:
      type: geojson
      path: "demo/sg_flood_sensors.geojson"
    table: t
"#;
    let cfg = Config::from_yaml_str(yaml).unwrap();
    let load = sql::load_statement(&cfg.sources["s"]).unwrap();
    assert!(load.contains("ST_Read('demo/sg_flood_sensors.geojson')"));
    assert!(matches!(cfg.sources["s"].data, DataSource::Geojson { .. }));
}
