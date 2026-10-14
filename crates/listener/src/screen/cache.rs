use std::collections::HashMap;

/// Tiny TTL map for RPC-derived facts.
#[derive(Debug, Default)]
pub(crate) struct TimedCache<V: Clone> {
    ttl_secs: u64,
    entries: HashMap<String, (V, u64)>,
}

impl<V: Clone> TimedCache<V> {
    pub(crate) fn new(ttl_secs: u64) -> Self {
        Self {
            ttl_secs,
            entries: HashMap::new(),
        }
    }

    /// Hit (fresh) or `None` on miss/expiry.
    pub(crate) fn get(&self, key: &str, now_secs: u64) -> Option<V> {
        self.entries.get(key).and_then(|(value, at)| {
            if now_secs.saturating_sub(*at) <= self.ttl_secs {
                Some(value.clone())
            } else {
                None
            }
        })
    }

    pub(crate) fn put(&mut self, key: String, value: V, now_secs: u64) {
        // Drop expired entries on insert.
        let ttl = self.ttl_secs;
        self.entries
            .retain(|_, (_, at)| now_secs.saturating_sub(*at) <= ttl);
        // Hard cap as a second backstop.
        if self.entries.len() > 10_000
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(key, (value, now_secs));
    }
}
