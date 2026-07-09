//! Tile coordinate math.
//!
//! Pure functions converting a slippy-map `z/x/y` tile address into its
//! Web Mercator (EPSG:3857) bounding box and its WGS84 (EPSG:4326) bounding
//! box. This module is intentionally free of any I/O or DuckDB dependency so it
//! can be unit-tested without the heavy native build.
//!
//! Conventions follow the OSM / XYZ slippy-map standard:
//!   * `z` is the zoom level (0..=~22),
//!   * `x` increases eastward (0 at the antimeridian going east),
//!   * `y` increases southward (0 at the north).
//!
//! Web Mercator world extent (meters), the canonical constant used by MVT:
//!   ±20037508.342789244

/// Half of the Web Mercator world extent in meters (the "origin shift").
pub const ORIGIN_SHIFT: f64 = 20_037_508.342_789_244;

/// Full Web Mercator world extent in meters.
pub const WORLD_EXTENT: f64 = 2.0 * ORIGIN_SHIFT;

/// Maximum absolute latitude representable in Web Mercator.
pub const MAX_LATITUDE: f64 = 85.051_128_779_806_59;

/// A tile address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tile {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

/// An axis-aligned bounding box `[min_x, min_y, max_x, max_y]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BBox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl BBox {
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }
}

impl Tile {
    pub fn new(z: u8, x: u32, y: u32) -> Self {
        Self { z, x, y }
    }

    /// The largest zoom level we accept. Real slippy-map pyramids stop well
    /// before this; the cap also keeps `2^z` from overflowing `u64` (a `1 << 64`
    /// shift would panic in debug builds).
    pub const MAX_ZOOM: u8 = 30;

    /// Number of tiles per axis at this zoom level: `2^z`. Saturates for
    /// out-of-range zooms instead of overflowing the shift.
    pub fn tiles_per_axis(z: u8) -> u64 {
        if z > Self::MAX_ZOOM {
            u64::MAX
        } else {
            1u64 << z
        }
    }

    /// Returns true if the tile address is in range for its zoom level. A zoom
    /// above [`Tile::MAX_ZOOM`] is rejected outright (no valid tile exists, and
    /// it would overflow the `2^z` shift).
    pub fn is_valid(&self) -> bool {
        if self.z > Self::MAX_ZOOM {
            return false;
        }
        let n = Tile::tiles_per_axis(self.z);
        (self.x as u64) < n && (self.y as u64) < n
    }

    /// Web Mercator (EPSG:3857) bounding box of this tile, in meters.
    ///
    /// At zoom `z` the world is divided into `2^z` columns/rows, each of width
    /// `WORLD_EXTENT / 2^z`. Column `x` starts at the western edge; row `y`
    /// counts from the *north*, so we flip on the Y axis.
    pub fn mercator_bounds(&self) -> BBox {
        let n = Tile::tiles_per_axis(self.z) as f64;
        let tile_size = WORLD_EXTENT / n;

        let min_x = -ORIGIN_SHIFT + (self.x as f64) * tile_size;
        let max_x = min_x + tile_size;

        // y=0 is the top (north / +ORIGIN_SHIFT), so subtract going south.
        let max_y = ORIGIN_SHIFT - (self.y as f64) * tile_size;
        let min_y = max_y - tile_size;

        BBox {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    /// WGS84 (EPSG:4326) lon/lat bounding box of this tile, in degrees.
    pub fn wgs84_bounds(&self) -> BBox {
        let merc = self.mercator_bounds();
        let (min_lon, min_lat) = mercator_to_lonlat(merc.min_x, merc.min_y);
        let (max_lon, max_lat) = mercator_to_lonlat(merc.max_x, merc.max_y);
        BBox {
            min_x: min_lon,
            min_y: min_lat,
            max_x: max_lon,
            max_y: max_lat,
        }
    }
}

/// Convert WGS84 lon/lat (degrees) to Web Mercator meters.
pub fn lonlat_to_mercator(lon: f64, lat: f64) -> (f64, f64) {
    let x = lon * ORIGIN_SHIFT / 180.0;
    let lat = lat.clamp(-MAX_LATITUDE, MAX_LATITUDE);
    let rad = lat.to_radians();
    let y = (std::f64::consts::FRAC_PI_4 + rad / 2.0).tan().ln()
        * (ORIGIN_SHIFT / std::f64::consts::PI);
    (x, y)
}

/// Convert Web Mercator meters to WGS84 lon/lat (degrees).
pub fn mercator_to_lonlat(x: f64, y: f64) -> (f64, f64) {
    let lon = x / ORIGIN_SHIFT * 180.0;
    let lat = (y / ORIGIN_SHIFT * 180.0).to_radians();
    let lat = (2.0 * lat.exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
    (lon, lat)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolerance for floating-point comparisons, in meters.
    const EPS_M: f64 = 1e-6;
    /// Tolerance for degree comparisons.
    const EPS_DEG: f64 = 1e-9;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn zoom0_covers_whole_world() {
        let t = Tile::new(0, 0, 0);
        let b = t.mercator_bounds();
        assert!(approx(b.min_x, -ORIGIN_SHIFT, EPS_M), "min_x={}", b.min_x);
        assert!(approx(b.min_y, -ORIGIN_SHIFT, EPS_M), "min_y={}", b.min_y);
        assert!(approx(b.max_x, ORIGIN_SHIFT, EPS_M), "max_x={}", b.max_x);
        assert!(approx(b.max_y, ORIGIN_SHIFT, EPS_M), "max_y={}", b.max_y);
    }

    #[test]
    fn zoom0_wgs84_bounds() {
        let t = Tile::new(0, 0, 0);
        let b = t.wgs84_bounds();
        assert!(approx(b.min_x, -180.0, 1e-6), "min_lon={}", b.min_x);
        assert!(approx(b.max_x, 180.0, 1e-6), "max_lon={}", b.max_x);
        // Mercator can't represent the poles; clamps near ±85.0511.
        assert!(approx(b.max_y, MAX_LATITUDE, 1e-6), "max_lat={}", b.max_y);
        assert!(approx(b.min_y, -MAX_LATITUDE, 1e-6), "min_lat={}", b.min_y);
    }

    #[test]
    fn zoom1_four_quadrants_tile_world() {
        // At z=1 there are 4 tiles. Their union should be the whole world and
        // each should be exactly a quarter (half-width, half-height).
        let quarter = WORLD_EXTENT / 2.0;
        for x in 0..2 {
            for y in 0..2 {
                let b = Tile::new(1, x, y).mercator_bounds();
                assert!(approx(b.width(), quarter, EPS_M), "width @{},{}", x, y);
                assert!(approx(b.height(), quarter, EPS_M), "height @{},{}", x, y);
            }
        }
        // Top-left tile (0,0) should sit at the NW corner.
        let tl = Tile::new(1, 0, 0).mercator_bounds();
        assert!(approx(tl.min_x, -ORIGIN_SHIFT, EPS_M));
        assert!(approx(tl.max_y, ORIGIN_SHIFT, EPS_M));
        // Bottom-right tile (1,1) should sit at the SE corner.
        let br = Tile::new(1, 1, 1).mercator_bounds();
        assert!(approx(br.max_x, ORIGIN_SHIFT, EPS_M));
        assert!(approx(br.min_y, -ORIGIN_SHIFT, EPS_M));
    }

    #[test]
    fn adjacent_tiles_share_edges() {
        // Tile (z=4, x=5, y=6) right edge == tile (z=4, x=6, y=6) left edge.
        let a = Tile::new(4, 5, 6).mercator_bounds();
        let b = Tile::new(4, 6, 6).mercator_bounds();
        assert!(approx(a.max_x, b.min_x, EPS_M));
        // Vertically adjacent: (y=6) bottom == (y=7) top.
        let c = Tile::new(4, 5, 7).mercator_bounds();
        assert!(approx(a.min_y, c.max_y, EPS_M));
    }

    #[test]
    fn known_tile_singapore() {
        // Singapore (103.8198E, 1.3521N) lives in slippy tile z=12, x=3229,
        // y=2032 (cross-checked against the standard OSM lon/lat->tile formula).
        // Verify the WGS84 bounds of that tile actually contain Singapore.
        let t = Tile::new(12, 3229, 2032);
        let b = t.wgs84_bounds();
        let lon = 103.8198;
        let lat = 1.3521;
        assert!(
            b.min_x <= lon && lon <= b.max_x,
            "lon {} not in [{},{}]",
            lon,
            b.min_x,
            b.max_x
        );
        assert!(
            b.min_y <= lat && lat <= b.max_y,
            "lat {} not in [{},{}]",
            lat,
            b.min_y,
            b.max_y
        );
    }

    #[test]
    fn mercator_lonlat_roundtrip() {
        for &(lon, lat) in &[
            (0.0, 0.0),
            (103.8198, 1.3521),
            (-122.4194, 37.7749),
            (151.2093, -33.8688),
        ] {
            let (mx, my) = lonlat_to_mercator(lon, lat);
            let (lon2, lat2) = mercator_to_lonlat(mx, my);
            assert!(approx(lon, lon2, EPS_DEG), "lon {} -> {}", lon, lon2);
            assert!(approx(lat, lat2, 1e-7), "lat {} -> {}", lat, lat2);
        }
    }

    #[test]
    fn tiles_per_axis_powers_of_two() {
        assert_eq!(Tile::tiles_per_axis(0), 1);
        assert_eq!(Tile::tiles_per_axis(1), 2);
        assert_eq!(Tile::tiles_per_axis(10), 1024);
        assert_eq!(Tile::tiles_per_axis(20), 1_048_576);
    }

    #[test]
    fn validity_checks() {
        assert!(Tile::new(1, 1, 1).is_valid());
        assert!(!Tile::new(1, 2, 0).is_valid());
        assert!(!Tile::new(1, 0, 2).is_valid());
        assert!(Tile::new(0, 0, 0).is_valid());
    }

    #[test]
    fn extreme_zoom_is_rejected_without_panicking() {
        // A z beyond MAX_ZOOM must be rejected, not overflow the 2^z shift.
        assert!(!Tile::new(31, 0, 0).is_valid());
        assert!(!Tile::new(255, 0, 0).is_valid());
        // tiles_per_axis must not panic for out-of-range zoom.
        assert_eq!(Tile::tiles_per_axis(64), u64::MAX);
        assert_eq!(Tile::tiles_per_axis(255), u64::MAX);
        // The boundary zoom is still valid.
        assert!(Tile::new(Tile::MAX_ZOOM, 0, 0).is_valid());
    }
}
