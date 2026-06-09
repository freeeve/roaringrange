//! A bounded LRU cache of fetched byte ranges, keyed by `(source, offset, len)`.
//!
//! The browser reader installs one shared instance (see [`crate::wasm`]) that every [`RangeFetch`]
//! GET consults, so range reads are memoized up to a byte budget the client sets from JS. This is a
//! pure latency/bandwidth win for repeated access — letter-by-letter typeahead re-requests the same
//! trigram/term posting ranges on each keystroke, and a split-set browse re-hits the same split
//! headers; the cache serves those without another network round-trip. Eviction is strict LRU until
//! the total cached payload is back under the budget.
//!
//! Single-threaded by design (the wasm runtime is single-threaded), so the wasm layer wraps it in
//! `Rc<RefCell<_>>`; nothing here needs `Send`/`Sync`. It is plain Rust with no deps, so it unit
//! tests on the host target.
//!
//! [`RangeFetch`]: crate::fetch::RangeFetch

use std::collections::{BTreeMap, HashMap};

/// Cache key: the fetch source (e.g. the object URL) plus the exact requested range. Two different
/// sources never collide, and only an identical `(offset, len)` re-read hits — which is exactly the
/// repeated request a re-typed query produces.
type Key = (String, u64, usize);

/// A byte-budgeted LRU cache over fetched ranges. See the module docs.
pub struct RangeCache {
    /// key -> (payload, recency clock at last access).
    map: HashMap<Key, (Vec<u8>, u64)>,
    /// recency clock -> key, ordered, so the least-recently-used key is `recency.first`.
    recency: BTreeMap<u64, Key>,
    /// Sum of cached payload lengths (the budgeted quantity; key overhead is not counted).
    bytes: usize,
    /// Eviction budget in bytes; `0` disables caching.
    max_bytes: usize,
    /// Monotonic access counter driving LRU order.
    clock: u64,
    hits: u64,
    misses: u64,
}

impl RangeCache {
    /// Creates an empty cache that holds at most `max_bytes` of payload (`0` = caching disabled).
    pub fn new(max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            recency: BTreeMap::new(),
            bytes: 0,
            max_bytes,
            clock: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// Current byte budget.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Resizes the budget, evicting the least-recently-used entries if the new budget is smaller.
    pub fn set_max_bytes(&mut self, max_bytes: usize) {
        self.max_bytes = max_bytes;
        self.evict_to_budget();
    }

    /// Cached payload for `(source, offset, len)`, refreshing its recency, or `None` on a miss.
    /// Increments the hit/miss counters. A clone is returned because the caller owns the bytes; the
    /// copy is a memcpy, trivially cheaper than the network read it replaces.
    pub fn get(&mut self, source: &str, offset: u64, len: usize) -> Option<Vec<u8>> {
        if self.max_bytes == 0 {
            return None;
        }
        let key: Key = (source.to_string(), offset, len);
        match self.map.get_mut(&key) {
            Some((bytes, last)) => {
                self.recency.remove(last);
                self.clock += 1;
                *last = self.clock;
                self.recency.insert(self.clock, key);
                self.hits += 1;
                Some(bytes.clone())
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// Inserts the payload for `(source, offset, len)` as most-recently-used, then evicts down to
    /// the budget. A payload larger than the whole budget is not cached (it would evict everything
    /// and still not fit), and an empty payload is skipped (a zero-length read never re-fetches).
    pub fn insert(&mut self, source: &str, offset: u64, len: usize, bytes: Vec<u8>) {
        if self.max_bytes == 0 || bytes.is_empty() || bytes.len() > self.max_bytes {
            return;
        }
        let key: Key = (source.to_string(), offset, len);
        if let Some((old, last)) = self.map.remove(&key) {
            self.bytes -= old.len();
            self.recency.remove(&last);
        }
        self.clock += 1;
        self.bytes += bytes.len();
        self.recency.insert(self.clock, key.clone());
        self.map.insert(key, (bytes, self.clock));
        self.evict_to_budget();
    }

    /// Evicts least-recently-used entries until the cached payload fits the budget.
    fn evict_to_budget(&mut self) {
        while self.bytes > self.max_bytes {
            let Some((&clock, _)) = self.recency.iter().next() else {
                break;
            };
            let key = self.recency.remove(&clock).expect("recency entry");
            if let Some((bytes, _)) = self.map.remove(&key) {
                self.bytes -= bytes.len();
            }
        }
    }

    /// `(payload bytes, entry count, cumulative hits, cumulative misses)` — for a JS-side readout of
    /// cache effectiveness.
    pub fn stats(&self) -> (usize, usize, u64, u64) {
        (self.bytes, self.map.len(), self.hits, self.misses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_then_miss_counts_and_returns_bytes() {
        let mut c = RangeCache::new(1024);
        assert_eq!(c.get("u", 0, 3), None);
        c.insert("u", 0, 3, vec![1, 2, 3]);
        assert_eq!(c.get("u", 0, 3), Some(vec![1, 2, 3]));
        // Different source or range is a distinct key.
        assert_eq!(c.get("v", 0, 3), None);
        assert_eq!(c.get("u", 0, 4), None);
        let (bytes, entries, hits, misses) = c.stats();
        assert_eq!((bytes, entries, hits), (3, 1, 1));
        assert_eq!(misses, 3);
    }

    #[test]
    fn evicts_least_recently_used_past_budget() {
        let mut c = RangeCache::new(6);
        c.insert("u", 0, 2, vec![0, 0]); // A
        c.insert("u", 2, 2, vec![0, 0]); // B
        c.insert("u", 4, 2, vec![0, 0]); // C -> at budget (6)
        assert!(c.get("u", 0, 2).is_some()); // touch A (now MRU)
        c.insert("u", 6, 2, vec![0, 0]); // D -> evicts LRU = B
        assert!(c.get("u", 0, 2).is_some(), "A retained (touched)");
        assert!(c.get("u", 2, 2).is_none(), "B evicted");
        assert!(c.get("u", 4, 2).is_some(), "C retained");
        assert!(c.get("u", 6, 2).is_some(), "D retained");
        assert!(c.stats().0 <= 6);
    }

    #[test]
    fn shrinking_budget_evicts_and_zero_disables() {
        let mut c = RangeCache::new(100);
        c.insert("u", 0, 4, vec![0; 4]);
        c.insert("u", 4, 4, vec![0; 4]);
        c.set_max_bytes(4);
        assert_eq!(c.stats().0, 4, "shrink evicted down to one entry");
        c.set_max_bytes(0);
        assert_eq!(c.stats().1, 0, "zero budget evicts everything");
        c.insert("u", 0, 4, vec![0; 4]);
        assert_eq!(c.get("u", 0, 4), None, "zero budget caches nothing");
    }

    #[test]
    fn payload_larger_than_budget_is_not_cached() {
        let mut c = RangeCache::new(4);
        c.insert("u", 0, 8, vec![0; 8]);
        assert_eq!(c.stats().1, 0);
    }
}
