//! Gzip compression for tile responses.
//!
//! MVT blobs are protobuf and compress well (typically 50-80% smaller), and the
//! MapLibre / Mapbox clients all accept `Content-Encoding: gzip`. When a request
//! advertises gzip support we compress the tile and set the header.
//!
//! This module is **DuckDB-free** so the `Accept-Encoding` parsing and the gzip
//! round-trip are unit-testable with `--no-default-features` (no heavy native
//! build). `flate2` is a pure-Rust dependency (miniz_oxide backend).

use std::io::Write;

use flate2::write::GzEncoder;
use flate2::Compression;

/// Return true if an `Accept-Encoding` header value advertises gzip support.
///
/// Handles comma-separated lists and `q=0` rejection, e.g.
/// `gzip, deflate, br` => true, `gzip;q=0` => false, `*` => true.
pub fn accepts_gzip(accept_encoding: Option<&str>) -> bool {
    let Some(value) = accept_encoding else {
        return false;
    };
    for part in value.split(',') {
        let mut it = part.split(';').map(str::trim);
        let coding = it.next().unwrap_or("").to_ascii_lowercase();
        if coding != "gzip" && coding != "*" {
            continue;
        }
        // Look for an explicit q=0 which disables this coding.
        let mut q_zero = false;
        for param in it {
            if let Some(q) = param.strip_prefix("q=") {
                if let Ok(qv) = q.parse::<f32>() {
                    if qv == 0.0 {
                        q_zero = true;
                    }
                }
            }
        }
        if !q_zero {
            return true;
        }
    }
    false
}

/// Gzip-compress a byte slice. Returns the compressed bytes.
pub fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::with_capacity(data.len() / 2 + 32), Compression::default());
    enc.write_all(data)?;
    enc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    #[test]
    fn accept_header_parsing() {
        assert!(accepts_gzip(Some("gzip")));
        assert!(accepts_gzip(Some("gzip, deflate, br")));
        assert!(accepts_gzip(Some("deflate, gzip;q=0.5")));
        assert!(accepts_gzip(Some("*")));
        assert!(accepts_gzip(Some("GZIP"))); // case-insensitive
        assert!(!accepts_gzip(Some("deflate, br")));
        assert!(!accepts_gzip(Some("gzip;q=0"))); // explicitly disabled
        assert!(!accepts_gzip(None));
        assert!(!accepts_gzip(Some("")));
    }

    #[test]
    fn gzip_roundtrips_and_shrinks_compressible_data() {
        // Highly compressible payload (repeated bytes) so we can assert shrink.
        let original = vec![42u8; 4096];
        let compressed = gzip(&original).unwrap();
        assert!(
            compressed.len() < original.len(),
            "compressed {} should be < original {}",
            compressed.len(),
            original.len()
        );
        // gzip magic bytes.
        assert_eq!(&compressed[..2], &[0x1f, 0x8b]);

        let mut dec = GzDecoder::new(&compressed[..]);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        assert_eq!(out, original);
    }

    #[test]
    fn gzip_roundtrips_empty() {
        let compressed = gzip(&[]).unwrap();
        let mut dec = GzDecoder::new(&compressed[..]);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
    }
}
