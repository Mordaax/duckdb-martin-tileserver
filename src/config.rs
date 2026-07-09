//! Configuration model (parsed from `config.yaml` via `serde_yaml`).
//!
//! A config has global server settings plus a map of named *sources*. Each
//! source describes one queryable layer:
//!   * where its data comes from (a GeoParquet / GeoJSON / existing DuckDB file),
//!   * which table + geometry column to query,
//!   * the source SRID of the geometry,
//!   * the zoom range and any attribute columns to carry into the MVT.
//!
//! The pure parsing/validation logic here has no DuckDB dependency and is
//! unit-tested.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    /// Named sources, keyed by the path segment used in `/{source}/{z}/{x}/{y}`.
    pub sources: BTreeMap<String, SourceConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Public base URL used to build TileJSON `tiles` templates. If unset, the
    /// server derives it from the inbound request's Host header.
    #[serde(default)]
    pub public_url: Option<String>,
    /// Max number of encoded tiles held in the in-memory LRU cache.
    #[serde(default = "default_cache_size")]
    pub cache_size: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            public_url: None,
            cache_size: default_cache_size(),
        }
    }
}

fn default_listen() -> String {
    "0.0.0.0:3000".to_string()
}
fn default_cache_size() -> usize {
    512
}

/// How a source's data is made available to DuckDB.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum DataSource {
    /// Load a GeoParquet file at startup:
    /// `CREATE TABLE <table> AS SELECT * FROM read_parquet('<path>')`.
    Parquet { path: String },
    /// Load a GeoJSON/other OGR file at startup via the spatial reader:
    /// `CREATE TABLE <table> AS SELECT * FROM ST_Read('<path>')`.
    Geojson { path: String },
    /// Attach / open an existing DuckDB database file and query it directly.
    Duckdb { path: String },
    /// Query an existing table in the in-process DuckDB (no load step).
    Table,
}

/// How the geometry column is physically stored, so the query layer knows
/// whether it must decode it before running spatial functions.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GeometryEncoding {
    /// A native DuckDB `GEOMETRY` value (the usual case after `ST_Read`, or a
    /// GeoParquet column written by a spatial tool). Used directly.
    #[default]
    Native,
    /// The column holds WKB bytes (`BLOB`). The query wraps it in
    /// `ST_GeomFromWKB(...)`. This is the common GeoParquet-on-disk case where
    /// the geometry is stored as standard WKB (e.g. our sample `.parquet`).
    Wkb,
}

/// A single configured tile source.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SourceConfig {
    /// Human-readable name for TileJSON.
    #[serde(default)]
    pub description: Option<String>,
    /// Where the data lives / how to load it.
    pub data: DataSource,
    /// SQL table name to query (also used as the MVT layer name unless
    /// `layer_name` overrides it).
    pub table: String,
    /// Optional MVT layer name; defaults to `table`.
    #[serde(default)]
    pub layer_name: Option<String>,
    /// Geometry column in the table.
    #[serde(default = "default_geom_column")]
    pub geometry_column: String,
    /// How the geometry column is encoded on disk (native GEOMETRY vs WKB blob).
    #[serde(default)]
    pub geometry_encoding: GeometryEncoding,
    /// SRID of the stored geometry. Web Mercator MVT needs 3857; if the data is
    /// 4326 (the common case for GeoJSON / GeoParquet) the query transforms it.
    #[serde(default = "default_srid")]
    pub srid: u32,
    /// Attribute columns to include as MVT feature properties. Empty = none.
    #[serde(default)]
    pub properties: Vec<String>,
    #[serde(default = "default_minzoom")]
    pub minzoom: u8,
    #[serde(default = "default_maxzoom")]
    pub maxzoom: u8,
    /// Optional WGS84 bounds `[w, s, e, n]` for TileJSON. If unset, world.
    #[serde(default)]
    pub bounds: Option<[f64; 4]>,
    /// MVT extent (tile grid resolution). 4096 is the de-facto standard.
    #[serde(default = "default_extent")]
    pub extent: u32,
    /// Geometry buffer (in tile units) to avoid clipping artifacts at edges.
    #[serde(default = "default_buffer")]
    pub buffer: u32,
}

fn default_geom_column() -> String {
    "geom".to_string()
}
fn default_srid() -> u32 {
    4326
}
fn default_minzoom() -> u8 {
    0
}
fn default_maxzoom() -> u8 {
    14
}
fn default_extent() -> u32 {
    4096
}
fn default_buffer() -> u32 {
    64
}

impl SourceConfig {
    /// The MVT layer name (explicit override, else the table name).
    pub fn layer(&self) -> &str {
        self.layer_name.as_deref().unwrap_or(&self.table)
    }

    /// TileJSON bounds, defaulting to the whole world.
    pub fn tilejson_bounds(&self) -> [f64; 4] {
        self.bounds
            .unwrap_or([-180.0, -85.051_128_78, 180.0, 85.051_128_78])
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse YAML: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("config validation error: {0}")]
    Validation(String),
}

impl Config {
    /// Parse a config from a YAML string and validate it.
    pub fn from_yaml_str(s: &str) -> Result<Self, ConfigError> {
        let cfg: Config = serde_yaml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load a config from a file path on disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_yaml_str(&text)
    }

    /// Validate cross-field invariants.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.sources.is_empty() {
            return Err(ConfigError::Validation("no sources configured".into()));
        }
        for (name, src) in &self.sources {
            if src.minzoom > src.maxzoom {
                return Err(ConfigError::Validation(format!(
                    "source '{name}': minzoom ({}) > maxzoom ({})",
                    src.minzoom, src.maxzoom
                )));
            }
            if src.maxzoom > 24 {
                return Err(ConfigError::Validation(format!(
                    "source '{name}': maxzoom {} exceeds 24",
                    src.maxzoom
                )));
            }
            if src.extent == 0 {
                return Err(ConfigError::Validation(format!(
                    "source '{name}': extent must be > 0"
                )));
            }
            if src.table.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "source '{name}': table must not be empty"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
server:
  listen: "127.0.0.1:8080"
  cache_size: 256
sources:
  sg_rainfall:
    description: "Singapore rainfall sensors"
    data:
      type: parquet
      path: "demo/sg_flood_sensors.parquet"
    table: sg_flood_sensors
    geometry_column: geom
    srid: 4326
    properties: [station_id, name, rainfall_mm, water_level_m]
    minzoom: 0
    maxzoom: 16
    bounds: [103.6, 1.2, 104.1, 1.5]
"#;

    #[test]
    fn parses_sample_config() {
        let cfg = Config::from_yaml_str(SAMPLE).expect("parse");
        assert_eq!(cfg.server.listen, "127.0.0.1:8080");
        assert_eq!(cfg.server.cache_size, 256);
        assert_eq!(cfg.sources.len(), 1);
        let src = &cfg.sources["sg_rainfall"];
        assert_eq!(src.table, "sg_flood_sensors");
        assert_eq!(src.srid, 4326);
        assert_eq!(src.maxzoom, 16);
        assert_eq!(src.properties.len(), 4);
        assert_eq!(src.bounds, Some([103.6, 1.2, 104.1, 1.5]));
        // layer() falls back to table when layer_name is unset.
        assert_eq!(src.layer(), "sg_flood_sensors");
        // defaults applied
        assert_eq!(src.extent, 4096);
        assert_eq!(src.buffer, 64);
        match &src.data {
            DataSource::Parquet { path } => assert!(path.ends_with(".parquet")),
            other => panic!("expected parquet, got {other:?}"),
        }
    }

    #[test]
    fn applies_server_defaults() {
        let yaml = r#"
sources:
  s:
    data: { type: table }
    table: t
"#;
        let cfg = Config::from_yaml_str(yaml).expect("parse");
        assert_eq!(cfg.server.listen, "0.0.0.0:3000");
        assert_eq!(cfg.server.cache_size, 512);
        let s = &cfg.sources["s"];
        assert_eq!(s.geometry_column, "geom");
        assert_eq!(s.srid, 4326);
        assert_eq!(s.minzoom, 0);
        assert_eq!(s.maxzoom, 14);
        assert!(matches!(s.data, DataSource::Table));
    }

    #[test]
    fn layer_name_override() {
        let yaml = r#"
sources:
  s:
    data: { type: table }
    table: my_table
    layer_name: pretty_layer
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.sources["s"].layer(), "pretty_layer");
    }

    #[test]
    fn rejects_empty_sources() {
        let yaml = "sources: {}";
        let err = Config::from_yaml_str(yaml).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn rejects_inverted_zoom() {
        let yaml = r#"
sources:
  s:
    data: { type: table }
    table: t
    minzoom: 10
    maxzoom: 2
"#;
        let err = Config::from_yaml_str(yaml).unwrap_err();
        match err {
            ConfigError::Validation(m) => assert!(m.contains("minzoom")),
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn geometry_encoding_defaults_to_native_and_parses_wkb() {
        // Default (unspecified) is Native.
        let cfg = Config::from_yaml_str(SAMPLE).unwrap();
        assert_eq!(
            cfg.sources["sg_rainfall"].geometry_encoding,
            GeometryEncoding::Native
        );

        // GeoParquet-on-disk case: geometry stored as WKB blob.
        let yaml = r#"
sources:
  pq:
    data:
      type: parquet
      path: "demo/sg_flood_sensors.parquet"
    table: t
    geometry_column: geom
    geometry_encoding: wkb
    srid: 4326
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(
            cfg.sources["pq"].geometry_encoding,
            GeometryEncoding::Wkb
        );
    }

    #[test]
    fn parses_multiple_named_sources() {
        let yaml = r#"
sources:
  a:
    data: { type: geojson, path: "a.geojson" }
    table: ta
  b:
    data: { type: parquet, path: "b.parquet" }
    table: tb
    geometry_encoding: wkb
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.sources.len(), 2);
        assert!(matches!(cfg.sources["a"].data, DataSource::Geojson { .. }));
        assert!(matches!(cfg.sources["b"].data, DataSource::Parquet { .. }));
        assert_eq!(cfg.sources["b"].geometry_encoding, GeometryEncoding::Wkb);
    }

    #[test]
    fn default_bounds_is_world() {
        let yaml = r#"
sources:
  s:
    data: { type: table }
    table: t
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        let b = cfg.sources["s"].tilejson_bounds();
        assert_eq!(b[0], -180.0);
        assert_eq!(b[2], 180.0);
    }
}
