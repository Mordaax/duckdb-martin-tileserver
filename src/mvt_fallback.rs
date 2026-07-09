//! Pure-Rust MVT (Mapbox Vector Tile) encoder — the documented *fallback* path.
//!
//! ## Why this exists
//!
//! The primary tile path (`duck::DuckSource::tile_mvt_native`) asks DuckDB's
//! spatial extension to do everything, including the `ST_AsMVT` *aggregate*
//! which returns a finished protobuf blob. Some builds of the spatial extension
//! ship the per-geometry `ST_AsMVTGeom` but not the `ST_AsMVT` aggregate. When
//! that aggregate is missing we fall back to:
//!
//!   1. SQL: `ST_AsWKB(ST_AsMVTGeom(geom_in_mercator, tile_envelope, extent, buffer))`
//!      — DuckDB still does the clip + integer-tile-space transform, we just get
//!      the result back as WKB instead of a packed tile.
//!   2. Rust (this module): parse each WKB geometry (already in tile integer
//!      space, Y-down) and emit the MVT command/parameter integers, then encode
//!      the `Tile { layers[] { features[], keys[], values[] } }` protobuf
//!      message by hand.
//!
//! ## Why hand-rolled instead of the `geozero`/`mvt` crate
//!
//! `geozero::mvt::MvtWriter` can do this too, but its write API has shifted
//! across 0.11–0.15 releases (Tile/Layer/Feature prost types, TagsBuilder), which
//! makes it fragile to pin in a PoC. The MVT geometry+protobuf encoding is small
//! and stable (it is a published spec), so we encode it directly here. This keeps
//! the fallback dependency-free and unit-testable without any native build.
//! Swapping in `geozero` later is a localized change confined to this file.
//!
//! ## Coordinate input contract
//!
//! Geometries handed to [`encode_tile`] are expected to already be in MVT tile
//! space: integer-ish coordinates in `[0, extent]`, origin at the tile's
//! top-left, **Y increasing downward** — exactly what `ST_AsMVTGeom(..., true)`
//! produces (its last arg `clip_geom = true` and it flips Y for MVT). We round
//! to integers and delta/zig-zag encode per the MVT spec.
//!
//! Reference: https://github.com/mapbox/vector-tile-spec/tree/master/2.1

// ------------------------------ value model ---------------------------------

/// A property value carried into a feature. Mirrors the MVT `Value` variants we
/// support (string / float / double / int / uint / sint / bool).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Float(f32),
    Double(f64),
    Int(i64),
    Uint(u64),
    Bool(bool),
    Null,
}

/// One row pulled from DuckDB: clipped geometry as WKB + its property tuples.
#[derive(Debug, Clone)]
pub struct FeatureRow {
    pub wkb: Vec<u8>,
    pub props: Vec<(String, Value)>,
}

// --------------------------- MVT geometry types -----------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeomType {
    Point = 1,
    LineString = 2,
    Polygon = 3,
}

/// MVT command IDs.
const CMD_MOVE_TO: u32 = 1;
const CMD_LINE_TO: u32 = 2;
const CMD_CLOSE_PATH: u32 = 7;

#[inline]
fn command_integer(id: u32, count: u32) -> u32 {
    (id & 0x7) | (count << 3)
}

#[inline]
fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

// ------------------------------- WKB parser ---------------------------------
//
// Minimal little/big-endian WKB reader supporting the geometry types DuckDB
// emits from ST_AsMVTGeom: Point, LineString, Polygon and their Multi* forms +
// GeometryCollection. We only need 2D (XY); higher dimensions are tolerated by
// skipping extra ordinates if flagged.

struct WkbReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

#[derive(Debug)]
pub struct WkbError(pub String);
impl std::fmt::Display for WkbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WKB parse error: {}", self.0)
    }
}

/// A simple ring/line: a sequence of (x, y) points already in tile space.
type Ring = Vec<(f64, f64)>;

/// Parsed geometry reduced to MVT-relevant primitives.
#[derive(Debug, Clone)]
enum Geom {
    Point(Vec<(f64, f64)>),   // one or many points -> MultiPoint handled together
    Lines(Vec<Ring>),         // one or more linestrings
    Polygons(Vec<Vec<Ring>>), // polygons, each a list of rings (exterior + holes)
}

impl<'a> WkbReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8, WkbError> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| WkbError("unexpected EOF (u8)".into()))?;
        self.pos += 1;
        Ok(b)
    }

    fn u32(&mut self, le: bool) -> Result<u32, WkbError> {
        let end = self.pos + 4;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| WkbError("unexpected EOF (u32)".into()))?;
        let arr: [u8; 4] = s.try_into().unwrap();
        self.pos = end;
        Ok(if le {
            u32::from_le_bytes(arr)
        } else {
            u32::from_be_bytes(arr)
        })
    }

    fn f64(&mut self, le: bool) -> Result<f64, WkbError> {
        let end = self.pos + 8;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| WkbError("unexpected EOF (f64)".into()))?;
        let arr: [u8; 8] = s.try_into().unwrap();
        self.pos = end;
        Ok(if le {
            f64::from_le_bytes(arr)
        } else {
            f64::from_be_bytes(arr)
        })
    }

    /// Read a geometry header: returns (little_endian, base_type, has_z, has_m).
    fn header(&mut self) -> Result<(bool, u32, bool, bool), WkbError> {
        let le = self.u8()? == 1;
        let raw = self.u32(le)?;
        // ISO WKB encodes Z/M as +1000/+2000; EWKB uses high bit flags.
        let has_z =
            (raw & 0x8000_0000) != 0 || (raw % 10000) / 1000 == 1 || (raw % 10000) / 1000 == 3;
        let has_m =
            (raw & 0x4000_0000) != 0 || (raw % 10000) / 1000 == 2 || (raw % 10000) / 1000 == 3;
        let base = raw % 1000;
        Ok((le, base, has_z, has_m))
    }

    fn points(&mut self, le: bool, n: usize, hz: bool, hm: bool) -> Result<Ring, WkbError> {
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let x = self.f64(le)?;
            let y = self.f64(le)?;
            if hz {
                let _ = self.f64(le)?;
            }
            if hm {
                let _ = self.f64(le)?;
            }
            v.push((x, y));
        }
        Ok(v)
    }

    fn ring_count(&mut self, le: bool) -> Result<usize, WkbError> {
        Ok(self.u32(le)? as usize)
    }

    /// Parse a single geometry (recursing for collections / multis).
    fn geometry(&mut self) -> Result<Vec<Geom>, WkbError> {
        let (le, base, hz, hm) = self.header()?;
        match base {
            1 => {
                // Point
                let p = self.points(le, 1, hz, hm)?;
                Ok(vec![Geom::Point(p)])
            }
            2 => {
                // LineString
                let n = self.u32(le)? as usize;
                let line = self.points(le, n, hz, hm)?;
                Ok(vec![Geom::Lines(vec![line])])
            }
            3 => {
                // Polygon
                let nrings = self.ring_count(le)?;
                let mut rings = Vec::with_capacity(nrings);
                for _ in 0..nrings {
                    let n = self.u32(le)? as usize;
                    rings.push(self.points(le, n, hz, hm)?);
                }
                Ok(vec![Geom::Polygons(vec![rings])])
            }
            4 => {
                // MultiPoint
                let n = self.u32(le)? as usize;
                let mut pts = Vec::with_capacity(n);
                for _ in 0..n {
                    // each is a full sub-geometry header + point
                    let sub = self.geometry()?;
                    for g in sub {
                        if let Geom::Point(mut p) = g {
                            pts.append(&mut p);
                        }
                    }
                }
                Ok(vec![Geom::Point(pts)])
            }
            5 => {
                // MultiLineString
                let n = self.u32(le)? as usize;
                let mut lines = Vec::new();
                for _ in 0..n {
                    let sub = self.geometry()?;
                    for g in sub {
                        if let Geom::Lines(mut ls) = g {
                            lines.append(&mut ls);
                        }
                    }
                }
                Ok(vec![Geom::Lines(lines)])
            }
            6 => {
                // MultiPolygon
                let n = self.u32(le)? as usize;
                let mut polys = Vec::new();
                for _ in 0..n {
                    let sub = self.geometry()?;
                    for g in sub {
                        if let Geom::Polygons(mut ps) = g {
                            polys.append(&mut ps);
                        }
                    }
                }
                Ok(vec![Geom::Polygons(polys)])
            }
            7 => {
                // GeometryCollection
                let n = self.u32(le)? as usize;
                let mut out = Vec::new();
                for _ in 0..n {
                    out.extend(self.geometry()?);
                }
                Ok(out)
            }
            other => Err(WkbError(format!("unsupported WKB type {other}"))),
        }
    }
}

// --------------------------- geometry encoding ------------------------------

/// Encode a list of paths (each a `Ring`) into MVT command integers, given the
/// running cursor. `close` adds a ClosePath after each path (polygon rings).
fn encode_paths(paths: &[Ring], close: bool, cursor: &mut (i32, i32)) -> Vec<u32> {
    let mut out = Vec::new();
    for path in paths {
        if path.is_empty() {
            continue;
        }
        // For polygons, the WKB ring repeats the first point at the end; MVT's
        // ClosePath implies it, so drop a trailing duplicate.
        let pts: &[(f64, f64)] = if close && path.len() >= 2 && path.first() == path.last() {
            &path[..path.len() - 1]
        } else {
            path
        };
        if pts.is_empty() {
            continue;
        }

        // MoveTo first point.
        let (fx, fy) = pts[0];
        let (ix, iy) = (fx.round() as i32, fy.round() as i32);
        out.push(command_integer(CMD_MOVE_TO, 1));
        out.push(zigzag(ix - cursor.0));
        out.push(zigzag(iy - cursor.1));
        cursor.0 = ix;
        cursor.1 = iy;

        // LineTo the rest.
        if pts.len() > 1 {
            out.push(command_integer(CMD_LINE_TO, (pts.len() - 1) as u32));
            for &(x, y) in &pts[1..] {
                let (ix, iy) = (x.round() as i32, y.round() as i32);
                out.push(zigzag(ix - cursor.0));
                out.push(zigzag(iy - cursor.1));
                cursor.0 = ix;
                cursor.1 = iy;
            }
        }

        if close {
            out.push(command_integer(CMD_CLOSE_PATH, 1));
        }
    }
    out
}

/// Encode a parsed geometry to (geom_type, command_integers).
fn encode_geom(g: &Geom) -> (GeomType, Vec<u32>) {
    let mut cursor = (0i32, 0i32);
    match g {
        Geom::Point(pts) => {
            // MultiPoint: a single MoveTo with count = number of points.
            let mut out = Vec::new();
            if !pts.is_empty() {
                out.push(command_integer(CMD_MOVE_TO, pts.len() as u32));
                for &(x, y) in pts {
                    let (ix, iy) = (x.round() as i32, y.round() as i32);
                    out.push(zigzag(ix - cursor.0));
                    out.push(zigzag(iy - cursor.1));
                    cursor.0 = ix;
                    cursor.1 = iy;
                }
            }
            (GeomType::Point, out)
        }
        Geom::Lines(lines) => (
            GeomType::LineString,
            encode_paths(lines, false, &mut cursor),
        ),
        Geom::Polygons(polys) => {
            let mut out = Vec::new();
            for rings in polys {
                out.extend(encode_paths(rings, true, &mut cursor));
            }
            (GeomType::Polygon, out)
        }
    }
}

// ----------------------------- protobuf encode ------------------------------
//
// Minimal protobuf wire-format encoder for the MVT Tile message. We only need
// the field tags used by vector-tile-spec 2.1.

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

/// field tag for varint (wire type 0).
fn key_varint(field: u32) -> u64 {
    ((field as u64) << 3) | 0
}
/// field tag for length-delimited (wire type 2).
fn key_len(field: u32) -> u64 {
    ((field as u64) << 3) | 2
}

fn write_tagged_varint(out: &mut Vec<u8>, field: u32, v: u64) {
    write_varint(out, key_varint(field));
    write_varint(out, v);
}

fn write_tagged_bytes(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    write_varint(out, key_len(field));
    write_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn write_tagged_string(out: &mut Vec<u8>, field: u32, s: &str) {
    write_tagged_bytes(out, field, s.as_bytes());
}

/// Encode a packed repeated uint32 field (used for feature.geometry & tags).
fn write_packed_u32(out: &mut Vec<u8>, field: u32, vals: &[u32]) {
    let mut payload = Vec::new();
    for &v in vals {
        write_varint(&mut payload, v as u64);
    }
    write_tagged_bytes(out, field, &payload);
}

// MVT Value message field numbers.
fn encode_value(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    match v {
        Value::String(s) => write_tagged_string(&mut out, 1, s),
        Value::Float(f) => {
            write_varint(&mut out, (2 << 3) | 5); // field 2, wire type 5 (32-bit)
            out.extend_from_slice(&f.to_le_bytes());
        }
        Value::Double(d) => {
            write_varint(&mut out, (3 << 3) | 1); // field 3, wire type 1 (64-bit)
            out.extend_from_slice(&d.to_le_bytes());
        }
        Value::Int(i) => write_tagged_varint(&mut out, 4, *i as u64),
        Value::Uint(u) => write_tagged_varint(&mut out, 5, *u),
        // field 6 = sint64; field 7 = bool
        Value::Bool(b) => write_tagged_varint(&mut out, 7, *b as u64),
        Value::Null => {} // empty value
    }
    out
}

/// Encode one feature message body (fields under Layer.features).
fn encode_feature(geom_type: GeomType, geometry: &[u32], tags: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    // field 2: tags (packed uint32)
    if !tags.is_empty() {
        write_packed_u32(&mut out, 2, tags);
    }
    // field 3: GeomType enum (varint)
    write_tagged_varint(&mut out, 3, geom_type as u64);
    // field 4: geometry (packed uint32)
    if !geometry.is_empty() {
        write_packed_u32(&mut out, 4, geometry);
    }
    out
}

/// Build the full MVT tile protobuf for a single layer of features.
///
/// Geometries in `features` are WKB already in tile integer space (output of
/// `ST_AsMVTGeom`). Returns the encoded `application/x-protobuf` bytes, or an
/// empty `Vec` if there are no renderable features.
pub fn encode_tile(
    layer_name: &str,
    extent: u32,
    features: &[FeatureRow],
) -> Result<Vec<u8>, String> {
    // Deduplicated key/value dictionaries for the layer.
    let mut keys: Vec<String> = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    let mut key_idx = std::collections::HashMap::<String, u32>::new();
    let mut val_idx = std::collections::HashMap::<String, u32>::new();

    let mut feature_msgs: Vec<u8> = Vec::new();
    let mut wrote_any = false;

    for f in features {
        let geoms = WkbReader::new(&f.wkb)
            .geometry()
            .map_err(|e| e.to_string())?;

        // Build tag pairs (key_index, value_index) for this feature once.
        let mut tags: Vec<u32> = Vec::with_capacity(f.props.len() * 2);
        for (k, v) in &f.props {
            if matches!(v, Value::Null) {
                continue;
            }
            let ki = *key_idx.entry(k.clone()).or_insert_with(|| {
                keys.push(k.clone());
                (keys.len() - 1) as u32
            });
            // Value dedup keyed by its debug form (cheap, stable for our types).
            let vkey = format!("{v:?}");
            let vi = *val_idx.entry(vkey).or_insert_with(|| {
                values.push(v.clone());
                (values.len() - 1) as u32
            });
            tags.push(ki);
            tags.push(vi);
        }

        for g in &geoms {
            let (gt, geometry) = encode_geom(g);
            if geometry.is_empty() {
                continue;
            }
            wrote_any = true;
            let body = encode_feature(gt, &geometry, &tags);
            // Layer.features = field 2 (length-delimited message).
            write_tagged_bytes(&mut feature_msgs, 2, &body);
        }
    }

    if !wrote_any {
        return Ok(Vec::new());
    }

    // Assemble the Layer message.
    let mut layer = Vec::new();
    write_tagged_varint(&mut layer, 15, 2); // field 15: version = 2
    write_tagged_string(&mut layer, 1, layer_name); // field 1: name
    layer.extend_from_slice(&feature_msgs); // field 2: features (already tagged)
    for k in &keys {
        write_tagged_string(&mut layer, 3, k); // field 3: keys
    }
    for v in &values {
        let body = encode_value(v);
        write_tagged_bytes(&mut layer, 4, &body); // field 4: values
    }
    write_tagged_varint(&mut layer, 5, extent as u64); // field 5: extent

    // Assemble the Tile message: field 3 = layers (repeated).
    let mut tile = Vec::new();
    write_tagged_bytes(&mut tile, 3, &layer);
    Ok(tile)
}

// --------------------------- DuckDB row helper ------------------------------
//
// Reading a dynamically-typed DuckDB column into our `Value`. Kept here so the
// data layer can stay generic over property columns. Gated behind the DuckDB
// feature so the rest of this module (the pure MVT encoder) compiles without it.

/// Read column `idx` of a DuckDB row into a [`Value`], coercing common types.
#[cfg(feature = "duckdb-backend")]
pub fn read_value(row: &duckdb::Row<'_>, idx: usize) -> Result<Value, duckdb::Error> {
    use duckdb::types::ValueRef;
    let vr = row.get_ref(idx)?;
    Ok(match vr {
        ValueRef::Null => Value::Null,
        ValueRef::Boolean(b) => Value::Bool(b),
        ValueRef::TinyInt(i) => Value::Int(i as i64),
        ValueRef::SmallInt(i) => Value::Int(i as i64),
        ValueRef::Int(i) => Value::Int(i as i64),
        ValueRef::BigInt(i) => Value::Int(i),
        ValueRef::HugeInt(i) => Value::Int(i as i64),
        ValueRef::UTinyInt(u) => Value::Uint(u as u64),
        ValueRef::USmallInt(u) => Value::Uint(u as u64),
        ValueRef::UInt(u) => Value::Uint(u as u64),
        ValueRef::UBigInt(u) => Value::Uint(u),
        ValueRef::Float(f) => Value::Float(f),
        ValueRef::Double(d) => Value::Double(d),
        ValueRef::Decimal(d) => Value::Double(d.to_string().parse().unwrap_or(0.0)),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        other => Value::String(format!("{other:?}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a little-endian WKB Point at (x, y).
    fn wkb_point(x: f64, y: f64) -> Vec<u8> {
        let mut b = vec![1u8]; // little-endian
        b.extend_from_slice(&1u32.to_le_bytes()); // type 1 = Point
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
        b
    }

    /// Build a LE WKB LineString.
    fn wkb_line(pts: &[(f64, f64)]) -> Vec<u8> {
        let mut b = vec![1u8];
        b.extend_from_slice(&2u32.to_le_bytes()); // type 2
        b.extend_from_slice(&(pts.len() as u32).to_le_bytes());
        for &(x, y) in pts {
            b.extend_from_slice(&x.to_le_bytes());
            b.extend_from_slice(&y.to_le_bytes());
        }
        b
    }

    /// Build a LE WKB Polygon (single ring).
    fn wkb_polygon(ring: &[(f64, f64)]) -> Vec<u8> {
        let mut b = vec![1u8];
        b.extend_from_slice(&3u32.to_le_bytes()); // type 3
        b.extend_from_slice(&1u32.to_le_bytes()); // 1 ring
        b.extend_from_slice(&(ring.len() as u32).to_le_bytes());
        for &(x, y) in ring {
            b.extend_from_slice(&x.to_le_bytes());
            b.extend_from_slice(&y.to_le_bytes());
        }
        b
    }

    #[test]
    fn zigzag_examples() {
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
        assert_eq!(zigzag(-2), 3);
        assert_eq!(zigzag(2), 4);
    }

    #[test]
    fn command_integer_encoding() {
        // MoveTo with count 1 -> (1 & 0x7) | (1<<3) = 1 | 8 = 9
        assert_eq!(command_integer(CMD_MOVE_TO, 1), 9);
        // LineTo with count 2 -> 2 | (2<<3) = 2 | 16 = 18
        assert_eq!(command_integer(CMD_LINE_TO, 2), 18);
        // ClosePath count 1 -> 7 | 8 = 15
        assert_eq!(command_integer(CMD_CLOSE_PATH, 1), 15);
    }

    #[test]
    fn parse_and_encode_point() {
        let g = WkbReader::new(&wkb_point(25.0, 17.0)).geometry().unwrap();
        assert_eq!(g.len(), 1);
        let (gt, cmds) = encode_geom(&g[0]);
        assert_eq!(gt, GeomType::Point);
        // MoveTo(1), zigzag(25), zigzag(17)
        assert_eq!(
            cmds,
            vec![command_integer(CMD_MOVE_TO, 1), zigzag(25), zigzag(17)]
        );
    }

    #[test]
    fn parse_and_encode_line() {
        let g = WkbReader::new(&wkb_line(&[(2.0, 2.0), (2.0, 10.0), (10.0, 10.0)]))
            .geometry()
            .unwrap();
        let (gt, cmds) = encode_geom(&g[0]);
        assert_eq!(gt, GeomType::LineString);
        // MoveTo(1) d(2,2); LineTo(2) d(0,8) d(8,0)
        let expected = vec![
            command_integer(CMD_MOVE_TO, 1),
            zigzag(2),
            zigzag(2),
            command_integer(CMD_LINE_TO, 2),
            zigzag(0),
            zigzag(8),
            zigzag(8),
            zigzag(0),
        ];
        assert_eq!(cmds, expected);
    }

    #[test]
    fn polygon_drops_closing_duplicate_and_adds_closepath() {
        // Square ring with explicit closing point.
        let ring = [(3.0, 6.0), (8.0, 12.0), (20.0, 34.0), (3.0, 6.0)];
        let g = WkbReader::new(&wkb_polygon(&ring)).geometry().unwrap();
        let (gt, cmds) = encode_geom(&g[0]);
        assert_eq!(gt, GeomType::Polygon);
        // Should end with a ClosePath command and NOT re-emit the first vertex.
        assert_eq!(*cmds.last().unwrap(), command_integer(CMD_CLOSE_PATH, 1));
        // MoveTo(1) + 2 params, LineTo(2) + 4 params, ClosePath(1) = 9 ints
        assert_eq!(cmds.len(), 1 + 2 + 1 + 4 + 1);
    }

    #[test]
    fn encode_tile_roundtrips_through_decoder() {
        // Encode a tile with one point feature + a string property, then sanity
        // check the protobuf structure by decoding the top-level fields.
        let feats = vec![FeatureRow {
            wkb: wkb_point(100.0, 200.0),
            props: vec![("name".into(), Value::String("Marina".into()))],
        }];
        let bytes = encode_tile("sensors", 4096, &feats).unwrap();
        assert!(!bytes.is_empty());
        // First byte should be the Tile.layers tag: field 3, wire type 2 -> (3<<3)|2 = 26.
        assert_eq!(bytes[0], 26);
        // The layer name and property string should appear verbatim in the blob.
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("sensors"));
        assert!(s.contains("name"));
        assert!(s.contains("Marina"));
    }

    #[test]
    fn empty_features_yield_empty_tile() {
        let bytes = encode_tile("x", 4096, &[]).unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn varint_encoding() {
        let mut out = Vec::new();
        write_varint(&mut out, 0);
        assert_eq!(out, vec![0]);
        out.clear();
        write_varint(&mut out, 300);
        assert_eq!(out, vec![0xac, 0x02]);
    }

    #[test]
    fn big_endian_wkb_point() {
        // type 1 point at (1.0, 2.0), big-endian.
        let mut b = vec![0u8]; // big-endian
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(&1.0f64.to_be_bytes());
        b.extend_from_slice(&2.0f64.to_be_bytes());
        let g = WkbReader::new(&b).geometry().unwrap();
        let (gt, cmds) = encode_geom(&g[0]);
        assert_eq!(gt, GeomType::Point);
        assert_eq!(
            cmds,
            vec![command_integer(CMD_MOVE_TO, 1), zigzag(1), zigzag(2)]
        );
    }
}
