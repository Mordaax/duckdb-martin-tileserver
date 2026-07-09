# DuckDB tile server

A Rust HTTP server that reads geospatial data through DuckDB and returns Mapbox Vector Tiles. It supports GeoJSON, GeoParquet and DuckDB sources, with an LRU tile cache and a MapLibre demo map.

The project explores a DuckDB backend for [Martin](https://github.com/maplibre/martin). It follows Martin's source-and-router structure but runs as a standalone proof of concept; it has not been integrated upstream.

DuckDB's spatial extension handles the geometry queries. When the installed extension supports `ST_AsMVT`, DuckDB encodes the tile. Otherwise, the server reads WKB geometry and uses the Rust encoder in this repository.

## Architecture

```
HTTP GET /{source}/{z}/{x}/{y}
        │
        ▼
   axum router (src/server.rs)
        │  cache lookup ──hit──► return bytes (X-Tile-Cache: hit)
        │  miss
        ▼
   tile math (src/tile.rs)  z/x/y → Web Mercator envelope (EPSG:3857)
        │
        ▼
   DuckDB TileSource (src/duck.rs)         ┌─ native:  ST_AsMVT(ST_AsMVTGeom(...))  → MVT blob
        │  INSTALL/LOAD spatial; query ────┤
        │                                  └─ fallback: ST_AsWKB(ST_AsMVTGeom(...)) → WKB rows
        ▼                                                         │
   MVT bytes ◄── pure-Rust encoder (src/mvt_fallback.rs) ◄───────┘
        │
        ▼
   LRU cache put (src/cache.rs) → HTTP response (application/x-protobuf)
```

See [`ARCHITECTURE.md`](./ARCHITECTURE.md) for the full module breakdown, the
`TileSource` trait alignment with Martin, the MVT encoding details, and the
deferred-work list.

### Modules

| File | Responsibility | Needs DuckDB? |
|------|----------------|:---:|
| `src/tile.rs` | slippy `z/x/y` → Web Mercator / WGS84 bbox, lon/lat ⇄ mercator | no |
| `src/mvt_fallback.rs` | pure-Rust WKB parser + MVT geometry-command + protobuf encoder | no |
| `src/cache.rs` | bounded LRU tile cache keyed by `(source, z, x, y)` | no |
| `src/config.rs` | `config.yaml` model + validation (serde_yaml) | no |
| `src/duck.rs` | DuckDB connection, spatial load, MVT SQL builders, query path | yes |
| `src/server.rs` | axum router, handlers, TileJSON, static demo serving | yes |
| `src/main.rs` | binary entrypoint | yes (runtime) |

## Build

The `bundled` feature of the `duckdb` crate compiles DuckDB from C++ source. The first build needs a C++ toolchain and can take several minutes.

You can test the tile math, MVT encoder, cache and configuration separately with `--no-default-features`:
```bash
export PATH="$HOME/.cargo/bin:$PATH"

# Fast path: pure-logic core (tile math, MVT encoder, cache, config).
# Does NOT compile DuckDB. ~20s.
cargo test --no-default-features

# Full crate (compiles bundled DuckDB — slow, needs a C++ compiler).
cargo test
cargo build --release
```

`tile`, `mvt_fallback`, `cache` and `config` are always built. `duck` and
`server` are gated behind the default `duckdb-backend` feature. Building with
`--no-default-features` skips the DuckDB compile entirely and still exercises the
core logic (coordinate math, zig-zag/varint MVT encoding, LRU eviction,
config parsing).

The recorded core test run passed 31 tests (30 library tests and one integration test). This command skips the server and DuckDB integration; it does not verify a full server build. See `ARCHITECTURE.md` for the implementation status.

## How to run the server

1. Generate the demo dataset (stdlib-only Python; writes a GeoJSON of ~48
   Singapore flood/rainfall sensor points, plus an optional GeoParquet if
   `pyarrow` is installed):

   ```bash
   python3 demo/gen_data.py
   ```

2. Build & run (this compiles bundled DuckDB the first time):

   ```bash
   export PATH="$HOME/.cargo/bin:$PATH"
   cargo run --release -- config.yaml
   ```

   On startup the server runs `INSTALL spatial; LOAD spatial;`, loads
   `demo/sg_flood_sensors.geojson` into an in-memory DuckDB table via `ST_Read`,
   and probes whether the installed spatial build has the `ST_AsMVT` aggregate
   (selecting the native vs. Rust-fallback encode path accordingly).

3. Hit the endpoints:

   ```bash
   curl -s http://localhost:3000/catalog | jq
   curl -s http://localhost:3000/sg_rainfall | jq            # TileJSON
   curl -s -o tile.pbf http://localhost:3000/sg_rainfall/12/3229/2032
   ls -l tile.pbf                                            # MVT bytes
   ```

### Routes

| Method & path | Returns |
|---|---|
| `GET /` | HTML index / health landing page |
| `GET /health` | `ok` (liveness) |
| `GET /catalog` | JSON list of configured sources |
| `GET /demo/` | the static MapLibre demo map |
| `GET /{source}` | TileJSON 3.0.0 (MapLibre auto-configures from this) |
| `GET /{source}/{z}/{x}/{y}` | MVT (`application/x-protobuf`); `.mvt`/`.pbf` suffixes accepted |

Empty (no-feature) tiles return `204 No Content`. Cached tiles carry
`X-Tile-Cache: hit`.

## View the demo map

The demo is a MapLibre GL JS page (`demo/index.html`) that loads the
`sg_rainfall` source via its TileJSON and renders the sensor points colored by
flood status (normal / elevated / flood-warning), with click popups.

- Served by the tile server: with the server running, open
  <http://localhost:3000/demo/>. The map requests tiles from the same server.
- Opened as a file: open `demo/index.html` directly; it falls back to
  `http://localhost:3000` for tiles (start the server first; CORS is permissive).

## Configuration (`config.yaml`)

```yaml
server:
  listen: "0.0.0.0:3000"
  cache_size: 512            # max encoded tiles in the LRU
sources:
  sg_rainfall:
    description: "Singapore rainfall & flood sensor network"
    data:
      type: geojson          # parquet | geojson | duckdb | table
      path: "demo/sg_flood_sensors.geojson"
    table: sg_flood_sensors
    layer_name: rainfall     # MVT layer id == MapLibre "source-layer"
    geometry_column: geom
    srid: 4326               # transformed to 3857 for MVT
    properties: [station_id, name, rainfall_mm, water_level_m, status]
    minzoom: 0
    maxzoom: 16
    bounds: [103.6, 1.20, 104.10, 1.48]
    extent: 4096
    buffer: 64
```

Switch `data.type` to `parquet` (and run `gen_data.py` with `pyarrow`) to serve
the same layer from GeoParquet via `read_parquet(...)`: the
GeoParquet/analytics path. A commented-out second source in `config.yaml`
shows exactly that.

## Benchmark plan

The point of comparison is tile-render latency and throughput for the same
data and tile pyramid, isolating the backend.

Harness:

- Same dataset loaded into (a) PostGIS and (b) DuckDB (from GeoParquet) and a
  DuckDB-on-Parquet variant (no import step).
- Replay a fixed list of `(z,x,y)` tiles covering the data extent at several zoom
  levels (e.g. z=8..16), cache disabled, then enabled, measuring p50/p95
  latency and tiles/sec. Reuse the same axum handler; swap only the `TileSource`.
- Metrics: cold vs. warm, single-tile latency, concurrent throughput
  (`oha`/`wrk` against `/{source}/{z}/{x}/{y}`), and bytes/tile (should match, same MVT).

The comparison should measure how PostGIS's spatial indexes and DuckDB's columnar scans perform on the same data. For GeoParquet, row-group pruning and the cost of importing data are worth measuring separately. The output should also be checked for equivalent geometry and attributes.

No PostGIS comparison results are included. Running the comparison requires both backends with the same dataset. See `ARCHITECTURE.md` for the remaining work.

## Martin integration

Martin routes tile requests to implementations of its `Source` trait. This prototype uses a similar structure: `duck::DuckSource` produces tile bytes, while the axum router handles requests, TileJSON metadata and caching.

`tile_mvt(Tile) -> Vec<u8>` corresponds to the tile-producing part of Martin's API. An upstream integration still needs to implement Martin's actual trait, configuration schema and source discovery. That work is deferred. See `ARCHITECTURE.md` for the mapping.

## License

MIT. See LICENSE.
