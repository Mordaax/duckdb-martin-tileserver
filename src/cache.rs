//! A small in-memory LRU tile cache keyed by `(source, z, x, y)`.
//!
//! Rendered MVT blobs are relatively expensive to produce (a DuckDB spatial
//! query per tile), and map clients re-request the same tiles constantly as the
//! user pans/zooms. A bounded LRU keeps hot tiles in memory without unbounded
//! growth. The cache stores the already-encoded protobuf bytes.
//!
//! This module is pure logic (no DuckDB / no async I/O) so it is unit-testable
//! without the heavy native build. Thread-safety for the async server is layered
//! on top via `SharedTileCache` (a `Mutex`-wrapped handle).

use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;

/// Cache key. `Vec<u8>` of bytes is the value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub source: String,
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileKey {
    pub fn new(source: impl Into<String>, z: u8, x: u32, y: u32) -> Self {
        Self {
            source: source.into(),
            z,
            x,
            y,
        }
    }
}

/// A bounded LRU cache of encoded tiles. Not thread-safe on its own; wrap in
/// [`SharedTileCache`] for concurrent use.
pub struct TileCache {
    inner: LruCache<TileKey, Vec<u8>>,
    hits: u64,
    misses: u64,
}

impl TileCache {
    /// Create a cache holding at most `capacity` tiles. A capacity of 0 is
    /// promoted to 1 (LruCache requires non-zero), effectively a 1-slot cache.
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity >= 1");
        Self {
            inner: LruCache::new(cap),
            hits: 0,
            misses: 0,
        }
    }

    /// Look up a tile, promoting it to most-recently-used on a hit.
    pub fn get(&mut self, key: &TileKey) -> Option<Vec<u8>> {
        match self.inner.get(key) {
            Some(bytes) => {
                self.hits += 1;
                Some(bytes.clone())
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// Insert (or overwrite) a tile, evicting the least-recently-used entry if
    /// the cache is full.
    pub fn put(&mut self, key: TileKey, bytes: Vec<u8>) {
        self.inner.put(key, bytes);
    }

    /// Number of tiles currently held.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.inner.cap().get()
    }

    /// `(hits, misses)` counters.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    /// Drop everything (e.g. after a config reload).
    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

/// Thread-safe handle used by the axum server. A `Mutex` is plenty here: tile
/// lookups are O(1) and the critical section is just a hash-map probe + clone.
pub struct SharedTileCache(Mutex<TileCache>);

impl SharedTileCache {
    pub fn new(capacity: usize) -> Self {
        Self(Mutex::new(TileCache::new(capacity)))
    }

    pub fn get(&self, key: &TileKey) -> Option<Vec<u8>> {
        self.0.lock().expect("tile cache mutex poisoned").get(key)
    }

    pub fn put(&self, key: TileKey, bytes: Vec<u8>) {
        self.0
            .lock()
            .expect("tile cache mutex poisoned")
            .put(key, bytes);
    }

    pub fn stats(&self) -> (u64, u64) {
        self.0.lock().expect("tile cache mutex poisoned").stats()
    }

    pub fn len(&self) -> usize {
        self.0.lock().expect("tile cache mutex poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(source: &str, z: u8, x: u32, y: u32) -> TileKey {
        TileKey::new(source, z, x, y)
    }

    #[test]
    fn put_then_get_returns_bytes() {
        let mut c = TileCache::new(4);
        let key = k("rain", 12, 3229, 2032);
        assert!(c.get(&key).is_none());
        c.put(key.clone(), vec![1, 2, 3]);
        assert_eq!(c.get(&key), Some(vec![1, 2, 3]));
    }

    #[test]
    fn distinct_keys_do_not_collide() {
        let mut c = TileCache::new(8);
        c.put(k("a", 1, 0, 0), vec![10]);
        c.put(k("b", 1, 0, 0), vec![20]); // same z/x/y, different source
        c.put(k("a", 2, 0, 0), vec![30]); // same source, different z
        assert_eq!(c.get(&k("a", 1, 0, 0)), Some(vec![10]));
        assert_eq!(c.get(&k("b", 1, 0, 0)), Some(vec![20]));
        assert_eq!(c.get(&k("a", 2, 0, 0)), Some(vec![30]));
    }

    #[test]
    fn evicts_least_recently_used() {
        let mut c = TileCache::new(2);
        c.put(k("s", 0, 0, 0), vec![1]);
        c.put(k("s", 0, 0, 1), vec![2]);
        // Touch the first so it becomes most-recently-used.
        assert_eq!(c.get(&k("s", 0, 0, 0)), Some(vec![1]));
        // Insert a third -> the LRU (0,0,1) should be evicted, not (0,0,0).
        c.put(k("s", 0, 0, 2), vec![3]);
        assert_eq!(c.len(), 2);
        assert!(c.get(&k("s", 0, 0, 1)).is_none(), "0,0,1 should be evicted");
        assert_eq!(c.get(&k("s", 0, 0, 0)), Some(vec![1]));
        assert_eq!(c.get(&k("s", 0, 0, 2)), Some(vec![3]));
    }

    #[test]
    fn overwrite_updates_value() {
        let mut c = TileCache::new(4);
        let key = k("s", 5, 1, 1);
        c.put(key.clone(), vec![1]);
        c.put(key.clone(), vec![9, 9]);
        assert_eq!(c.get(&key), Some(vec![9, 9]));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn hit_miss_stats_tracked() {
        let mut c = TileCache::new(4);
        let key = k("s", 0, 0, 0);
        let _ = c.get(&key); // miss
        c.put(key.clone(), vec![1]);
        let _ = c.get(&key); // hit
        let _ = c.get(&key); // hit
        let _ = c.get(&k("s", 9, 9, 9)); // miss
        assert_eq!(c.stats(), (2, 2));
    }

    #[test]
    fn zero_capacity_promoted_to_one() {
        let mut c = TileCache::new(0);
        assert_eq!(c.capacity(), 1);
        c.put(k("s", 0, 0, 0), vec![1]);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn shared_cache_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SharedTileCache>();
        let c = SharedTileCache::new(2);
        let key = k("s", 0, 0, 0);
        c.put(key.clone(), vec![7]);
        assert_eq!(c.get(&key), Some(vec![7]));
    }
}
