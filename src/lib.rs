//! Library crate for `duckdb-martin-tileserver`.
//!
//! Exposing the modules as a library (in addition to the binary in `main.rs`)
//! lets integration tests in `tests/` exercise the pure-logic modules
//! (`tile`, `cache`, `config`) without pulling in the binary's runtime, and
//! makes the crate reusable as an embeddable component.
//!
//! Module map:
//!   * [`tile`]         — slippy-map z/x/y -> Web Mercator / WGS84 bbox math.
//!   * [`cache`]        — bounded LRU tile cache keyed by (source, z, x, y).
//!   * [`compress`]     — gzip tile compression + `Accept-Encoding` parsing.
//!   * [`config`]       — `config.yaml` model + validation (serde_yaml).
//!   * [`mvt_fallback`] — pure-Rust MVT protobuf encoder (fallback path).
//!   * [`duck`]         — DuckDB spatial connection + MVT SQL builders.
//!   * [`server`]       — axum router, handlers, TileJSON.
//!
//! ## Feature gating
//!
//! `tile`, `cache`, `config` and `mvt_fallback` are **DuckDB-free** and always
//! compile. `duck` and `server` (and the DuckDB-typed helpers inside
//! `mvt_fallback`) are gated behind the `duckdb-backend` feature (on by
//! default). Building with `--no-default-features` gives the pure-logic core
//! without compiling the heavy bundled DuckDB C++ — useful for fast,
//! DB-independent unit/integration testing.

// Always-compilable, DuckDB-free core.
pub mod cache;
pub mod compress;
pub mod config;
pub mod mvt_fallback;
pub mod tile;

// DuckDB query layer + the axum server that depends on it.
#[cfg(feature = "duckdb-backend")]
pub mod duck;
#[cfg(feature = "duckdb-backend")]
pub mod server;
