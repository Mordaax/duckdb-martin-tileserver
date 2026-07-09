# Architecture

This document describes the internal design of the DuckDB MVT tile server: the
module breakdown, the request→tile flow, the MVT encoding approach, the
`TileSource` abstraction and how it aligns with Martin's upstream `Source` trait,
caching, and what is deliberately deferred.

---

## 1. Module breakdown

```
src/
├── lib.rs            crate root; re-exports modules, defines feature gating
├── main.rs           binary entrypoint (tokio runtime, config load, serve, bench)
│
│   ── DuckDB-free core (always compiles; `cargo test --no-default-features`) ──
├── tile.rs           slippy z/x/y → Web Mercator/WGS84 bbox; lon/lat ⇄ mercator
├── mvt_fallback.rs   WKB parser + MVT geometry-command + protobuf encoder
├── cache.rs          bounded LRU tile cache + thread-safe handle
├── compress.rs       gzip tile compression + Accept-Encoding negotiation
├── config.rs         config.yaml model, defaults, validation (serde_yaml)
│
│   ── DuckDB layer (feature = "duckdb-backend", default on) ──
├── duck.rs           DuckDB connection, spatial load, SQL builders, query path
└── server.rs         axum router, handlers, TileJSON, static demo serving
```

**Why the split.** The `duckdb` crate's `bundled` feature compiles DuckDB from
C++ — slow and toolchain-heavy. By keeping all the algorithmically interesting,
correctness-critical logic (coordinate math, MVT wire encoding, cache eviction,
config parsing) in DuckDB-free modules behind a feature gate, the PoC always has
a compilable, fully unit-tested core regardless of whether the heavy native
build succeeds in a given environment. `duck` and `server` (plus the one
DuckDB-typed helper `mvt_fallback::read_value`) are `#[cfg(feature =
"duckdb-backend")]`.

---

## 2. Request → tile flow

```
                          GET /sg_rainfall/12/3229/2032
                                      │
                  ┌───────────────────▼────────────────────┐
                  │ server.rs :: tile() handler             │
                  │  • strip .mvt/.pbf suffix, parse y      │
                  │  • read Accept-Encoding (gzip?)         │
                  │  • look up SourceConfig by {source}     │
                  │    (404 if no such named source)        │
                  │  • zoom in [minzoom,maxzoom]? else 204   │
                  │  • Tile{z,x,y}.is_valid()? else 400      │
                  └───────────────────┬────────────────────┘
                                      │
                       cache.get(TileKey{source,z,x,y})   ← stores RAW mvt bytes
                          │ hit                      │ miss
                          ▼                          ▼
                 (gzip if requested)    spawn_blocking (DuckDB is sync/blocking)
                 X-Tile-Cache: hit                     │
                                          ┌────────────▼──────────────┐
                                          │ duck.rs :: tile_mvt(Tile)  │
                                          │  • tile.mercator_bounds()  │ ← tile.rs
                                          │    → (minx,miny,maxx,maxy) │
                                          │  • has_native_mvt ?        │
                                          └──────┬──────────────┬──────┘
                                       native    │              │  fallback
                                                 ▼              ▼
                              ST_AsMVT(ST_AsMVTGeom(...))   ST_AsWKB(ST_AsMVTGeom(...))
                              → single BLOB (the tile)      → N rows of (wkb, props…)
                                                 │              │
                                                 │              ▼
                                                 │     mvt_fallback::encode_tile()  ← mvt_fallback.rs
                                                 │      WKB → MVT commands → protobuf
                                                 ▼              ▼
                                          ┌──────────────────────────┐
                                          │ Vec<u8> MVT bytes         │
                                          └────────────┬─────────────┘
                                                       ▼
                                       cache.put(key, bytes.clone())   ← raw bytes
                                                       ▼
                                  if Accept-Encoding: gzip  → compress::gzip()
                                                       ▼      + Content-Encoding: gzip
                                  200 application/x-protobuf  (or 204 if empty)
```

**Why the cache holds raw (uncompressed) bytes.** Compression is per-response, not
per-cache-entry, so a single cached tile serves both gzip-capable browsers and
plain `curl` without storing two copies. The gzip step (`compress::gzip`) and the
`Accept-Encoding` negotiation (`compress::accepts_gzip`, handling comma lists and
`q=0`) live in the DuckDB-free `compress` module so they unit-test without the
native build.

**WKB-encoded GeoParquet.** A source can declare `geometry_encoding: wkb` when its
geometry column is stored as WKB bytes (the common GeoParquet-on-disk case, e.g.
the sample `demo/sg_flood_sensors.parquet`). The query layer then wraps the column
in `ST_GeomFromWKB(...)` before transforming/clipping; native `GEOMETRY` columns
(the `ST_Read` GeoJSON case) skip that. See `duck::sql::decoded_geom_expr`.

**Tile envelope as `BOX_2D`.** `ST_AsMVTGeom`'s bounds argument is a `BOX_2D`. We
build it directly as a struct literal `{'min_x':…}::BOX_2D` from the four bound
params (`duck::sql::tile_box_expr`) rather than aggregating
`ST_Extent(ST_MakeEnvelope(…))` in scalar position — correct and cheaper.
`ST_Intersects` still takes a polygon envelope (`ST_MakeEnvelope`).

The tile envelope is computed in **Web Mercator meters** because that is the CRS
MVT tiles live in and what `ST_MakeEnvelope`/`ST_AsMVTGeom` expect. Source
geometries stored in WGS84 (the common GeoParquet/GeoJSON case) are
`ST_Transform`-ed to EPSG:3857 inside the query; sources already in 3857 skip the
transform (see `duck::sql::mercator_geom_expr`).

---

## 3. MVT encoding approach

There are two encode paths; both end in identical MVT bytes.

### 3a. Native path (preferred)

DuckDB spatial does everything:

```sql
WITH bounds AS (SELECT ?::DOUBLE AS minx, ?::DOUBLE AS miny,
                       ?::DOUBLE AS maxx, ?::DOUBLE AS maxy),
mvtgeom AS (
  SELECT ST_AsMVTGeom(<geom→3857>,
             {'min_x':b.minx,'min_y':b.miny,'max_x':b.maxx,'max_y':b.maxy}::BOX_2D,
             4096, 64, true) AS geom, <props…>
  FROM <table> t, bounds b
  WHERE ST_Intersects(<geom→3857>, ST_MakeEnvelope(b.minx,b.miny,b.maxx,b.maxy))
)
SELECT ST_AsMVT(mvtgeom, '<layer>', 4096, 'geom') AS tile
FROM mvtgeom WHERE mvtgeom.geom IS NOT NULL;
```

`<geom→3857>` is `geometry_column` (or `ST_GeomFromWKB(geometry_column)` for
`geometry_encoding: wkb`), `ST_Transform`-ed to 3857 unless the source is already
3857. `ST_AsMVTGeom(geom, box, extent, buffer, clip)` clips to the tile and flips
to the MVT integer grid (origin top-left, Y-down, `[0,extent]`); its second
argument is a `BOX_2D`, built directly from the bound params. `ST_AsMVT(...)` is
an **aggregate** that returns the finished protobuf `BLOB`. We read column 0 of
the single result row.

### 3b. Fallback path (pure Rust, always-testable)

Some builds of the spatial extension ship `ST_AsMVTGeom` but **not** the
`ST_AsMVT` aggregate. On startup `duck::DuckSource::probe_native_mvt` checks
`duckdb_functions()` for `st_asmvt`; if absent, `tile_mvt` switches to:

1. **SQL:** `ST_AsWKB(ST_AsMVTGeom(...))` — DuckDB still does the clip + tile-space
   transform, but returns each geometry as **WKB** alongside its property columns.
2. **Rust (`mvt_fallback.rs`):** parse each WKB geometry (already in tile integer
   space, Y-down), emit the MVT command/parameter integers, and hand-encode the
   `Tile { Layer { features, keys, values, extent } }` protobuf.

The Rust encoder is deliberately **dependency-free**: the MVT geometry encoding
and protobuf wire format are small, stable, published specs, and a hand-rolled
encoder keeps the fallback compilable and unit-testable with **zero** native
dependencies. (We considered `geozero::mvt::MvtWriter`; its write API has shifted
across 0.11–0.15 releases, making it fragile to pin in a PoC. Swapping it in
later is a localized change confined to this one file.)

Encoding specifics, all unit-tested in `mvt_fallback::tests`:
- **Command integer:** `(id & 0x7) | (count << 3)` — MoveTo=1, LineTo=2,
  ClosePath=7.
- **Parameter integers:** delta from the running cursor, **zig-zag** encoded
  `((v << 1) ^ (v >> 31))`.
- **Points / MultiPoint:** single `MoveTo(count=n)`.
- **LineStrings:** `MoveTo(1)` then `LineTo(n-1)` per line.
- **Polygons:** `MoveTo`+`LineTo`+`ClosePath` per ring; the WKB closing-duplicate
  vertex is dropped because ClosePath implies it.
- **Protobuf:** manual varint / length-delimited writer; deduped key/value
  dictionaries per layer; `version=2`, `extent=4096`.
- **WKB reader** handles little/big-endian, ISO + EWKB Z/M flags, and
  Point/Line/Polygon + Multi* + GeometryCollection.

### Tile math (`tile.rs`)

Pure functions, no I/O: `Tile{z,x,y}` → Web Mercator bbox (world extent
±20037508.342789244 m, Y flipped for slippy north-origin), → WGS84 bbox, and
`lonlat_to_mercator` / `mercator_to_lonlat`. Unit tests cover z=0 world bounds,
z=1 quadrant tiling, edge-sharing between adjacent tiles, a known Singapore tile
(z=12 x=3229 y=2032) containing 103.8198E/1.3521N, lon/lat⇄mercator round-trips,
and `2^z` tile counts.

---

## 4. The `TileSource` trait and alignment with Martin

This PoC's tile producer is `duck::DuckSource`, exposing the essential
tile-source surface:

```rust
// Conceptual shape (DuckSource implements these inherent methods today; a real
// integration would put them behind a trait — see below).
impl DuckSource {
    fn open(name: &str, config: SourceConfig) -> Result<Self, DuckError>;  // build
    fn tile_mvt(&self, tile: Tile) -> Result<Vec<u8>, DuckError>;          // get_tile
    // TileJSON is derived from SourceConfig in server::tilejson()         // get_tilejson
}
```

### Martin's upstream `Source` trait (abridged)

Martin (`martin-core` / `martin::source`) defines roughly:

```rust
#[async_trait]
pub trait Source: Send + Sync {
    fn get_id(&self) -> &str;
    fn get_tilejson(&self) -> &TileJSON;
    fn get_tile_info(&self) -> TileInfo;            // format=mvt, encoding
    async fn get_tile(&self, xyz: TileCoord, opt: Option<&UrlQuery>)
        -> MartinResult<TileData>;                  // TileData == Vec<u8>
    fn clone_source(&self) -> Box<dyn Source>;
    // …
}
```

### Trait-by-trait mapping

| Martin `Source` | This PoC | Notes |
|---|---|---|
| `get_id()` | `DuckSource::name` | source key from `config.yaml` |
| `get_tile(xyz)` → `TileData` | `tile_mvt(Tile) -> Vec<u8>` | core method; same in/out shape |
| `get_tilejson()` | `server::tilejson()` builds TileJSON 3.0.0 from `SourceConfig` | would move into the impl |
| `get_tile_info()` | implicit: MVT / `application/x-protobuf`, `Content-Encoding: gzip` when negotiated | Martin's `TileInfo` carries `format=mvt` + `encoding`; the same shape |
| multiple named sources | `AppState.sources: BTreeMap<name, Mutex<DuckSource>>`; one route per name | Martin keeps a `TileSources` map keyed by id; same routing model |
| content encoding | `compress` module gzips per response from `Accept-Encoding` | Martin negotiates gzip/brotli centrally; this demonstrates the contract |
| `async` + `Send + Sync` | handler uses `spawn_blocking`; `Connection` is `Send` not `Sync`, wrapped in `Mutex` | a real impl uses a connection pool (e.g. `r2d2`/`bb8`) for `Sync` + concurrency |
| `clone_source()` | not implemented (single instance) | pooling makes this natural |
| config / auto-discovery | `config.rs` (`SourceConfig`, `DataSource` + `GeometryEncoding` enums) | mirrors Martin's per-backend config block |

The intentional design choice is that **`get_tile` is the only hot-path method**
and it already matches Martin's `TileCoord → Vec<u8>` contract. The rest is
plumbing (TileJSON, info, cloning) that a real upstream `impl Source for
DuckSource` would fill in. The URL shape (`/{source}/{z}/{x}/{y}`), TileJSON
auto-config, axum+tokio runtime, and LRU cache were all chosen to match Martin so
the integration is additive rather than a rewrite.

---

## 5. Caching

`cache.rs` is a bounded **LRU** keyed by `TileKey { source, z, x, y }` storing the
already-encoded MVT `Vec<u8>`:

- `TileCache` (single-thread logic, unit-tested: put/get, LRU eviction order,
  overwrite, hit/miss stats, zero-capacity→1) plus `SharedTileCache` — a
  `Mutex<TileCache>` handle that is `Send + Sync` for the async server.
- Capacity is `server.cache_size` from config (default 512 tiles).
- The handler checks the cache before rendering and populates it after; responses
  carry `X-Tile-Cache: hit|miss`.

**Why a Mutex is fine:** the critical section is an O(1) hashmap probe + clone of
the byte buffer; rendering itself happens **outside** the lock on
`spawn_blocking`. **Martin-style integration:** Martin has its own
`main_cache` (a `moka`/`OptMainCache` keyed by source + coords); a real backend
would *not* ship its own cache but plug `DuckSource` in beneath Martin's cache
layer. This module documents and demonstrates the contract (key shape, value =
encoded bytes, eviction) that such integration relies on.

---

## 6. Concurrency model

- One DuckDB `Connection` per source, each behind its own `Mutex` (the `duckdb`
  `Connection` is `Send` but not `Sync`).
- Tile rendering runs on `tokio::task::spawn_blocking` so a slow query never
  stalls the async reactor.
- For higher concurrency a per-source **connection pool** (one connection per
  worker) is the natural next step — DuckDB supports multiple read connections to
  the same database; that is also what makes `clone_source()` straightforward.

---

## 7. Implemented in this pass

Features added on top of the original tile-math/MVT/cache core:

- **GeoParquet source** (`DataSource::Parquet` + `geometry_encoding: wkb`):
  `read_parquet(...)` at load, `ST_GeomFromWKB(...)` to decode the WKB geometry
  column. A real sample `demo/sg_flood_sensors.parquet` ships in the repo.
- **Config-driven multiple named sources**: `config.yaml` lists several sources;
  each gets a `/{name}` TileJSON and a `/{name}/{z}/{x}/{y}` route; `/catalog`
  lists them all. The demo serves the same data from GeoJSON *and* GeoParquet.
- **Gzip tile compression** (`compress` module): `Accept-Encoding: gzip` →
  compressed body + `Content-Encoding: gzip`; cache stays uncompressed.
- **`bench` subcommand**: times tile generation per source across a small tile
  pyramid and prints a millisecond table (the DuckDB side of the PostGIS-vs-DuckDB
  table; PostGIS numbers remain deferred).
- **Robustness**: extreme-zoom rejection without `2^z` shift overflow
  (`Tile::MAX_ZOOM`), `BOX_2D` envelope correctness, and tests for all of the
  above that run DuckDB-free where possible.

## 8. Deferred (documented, not implemented here)

The following work is outside the scope of this prototype:

1. **Full upstream Martin trait integration.** Implement Martin's real
   `Source`/`async-trait` for `DuckSource`, land it as a `martin::source::duckdb`
   module with config-schema + auto-discovery, swap the inherent methods for the
   trait, and use Martin's own cache/router instead of the ones demonstrated
   here. Requires building against the Martin workspace and matching its current
   trait signatures (which evolve), so it is documented rather than vendored.

2. **Real PostGIS-vs-DuckDB benchmark numbers.** The harness and methodology are
   specified in `README.md` (fixed tile list, cache on/off, p50/p95 latency,
   tiles/sec, bytes/tile parity). Producing numbers needs both backends
   provisioned with the same data + indexes; deferred to avoid fabricated
   figures.

3. **Remote GeoParquet via `httpfs`.** Local GeoParquet is now implemented; DuckDB
   can also `read_parquet` directly from `s3://` / `https://` via the `httpfs`
   extension. The `DataSource::Parquet` path already takes an arbitrary path
   string, so remote GeoParquet is a small config + `INSTALL httpfs; LOAD httpfs;`
   extension step — left as a documented extension point (a compelling DuckDB-only
   capability vs. PostGIS).

4. **Connection pooling & multi-connection concurrency** (see §6) and
   **attribute-type-aware TileJSON `fields`** (currently advertised generically
   because column SQL types aren't introspected).
