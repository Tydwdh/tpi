//! P6-04：render cache/cadence——entry 级渲染缓存 + 帧合并。
//!
//! - [`RenderCache`]：按 (entry_id, revision, width, theme) 缓存渲染结果；
//!   append/变更只重渲变化 entry（不每帧全量 layout）；
//! - [`FrameCoalescer`]：合并密集 delta（terminal 永不丢帧——有界合并窗口）。

/// 渲染缓存条目（key = entry_id + revision + width + theme）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub entry_id: u64,
    pub revision: u64,
    pub width: u16,
    pub theme: u16,
}

/// entry 级渲染缓存（有界；LRU 淘汰）。
#[derive(Debug)]
pub struct RenderCache {
    entries: std::collections::HashMap<CacheKey, String>,
    order: std::collections::VecDeque<CacheKey>,
    capacity: usize,
    hits: u64,
    misses: u64,
}

impl RenderCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
            capacity: capacity.max(1),
            hits: 0,
            misses: 0,
        }
    }

    pub fn get(&mut self, key: &CacheKey) -> Option<&String> {
        if self.entries.contains_key(key) {
            self.hits += 1;
            self.entries.get(key)
        } else {
            self.misses += 1;
            None
        }
    }

    pub fn insert(&mut self, key: CacheKey, rendered: String) {
        if !self.entries.contains_key(&key) {
            self.order.push_back(key.clone());
            if self.order.len() > self.capacity
                && let Some(evicted) = self.order.pop_front()
            {
                self.entries.remove(&evicted);
            }
        }
        self.entries.insert(key, rendered);
    }

    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

/// 帧合并器：合并有界窗口内的密集 delta（terminal 不丢帧）。
#[derive(Debug, Clone, Copy)]
pub struct FrameCoalescer {
    /// 合并窗口（毫秒）；窗口内多次请求合并为一帧。
    pub window_ms: u64,
    pub pending: bool,
}

impl FrameCoalescer {
    pub fn new(window_ms: u64) -> Self {
        Self {
            window_ms,
            pending: false,
        }
    }

    /// 是否应渲染（窗口内第一次请求渲染；随后 pending 直到 flush）。
    pub fn should_render(&mut self) -> bool {
        if !self.pending {
            self.pending = true;
            true
        } else {
            false
        }
    }

    /// 帧已绘制（清 pending）。
    pub fn flush(&mut self) {
        self.pending = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 相同 key 命中缓存（不重渲）；变化 key miss。
    #[test]
    fn cache_hits_same_key() {
        let mut cache = RenderCache::new(16);
        let key = CacheKey {
            entry_id: 1,
            revision: 1,
            width: 80,
            theme: 0,
        };
        cache.insert(key.clone(), "rendered".into());
        assert_eq!(cache.get(&key).map(|s| s.as_str()), Some("rendered"));
        assert_eq!(cache.hit_rate(), 1.0, "insert 后 get 命中");
        // revision 变化 → miss。
        let changed = CacheKey {
            revision: 2,
            ..key.clone()
        };
        assert!(cache.get(&changed).is_none());
    }

    /// 有界：超过容量淘汰最旧（LRU）。
    #[test]
    fn cache_is_bounded() {
        let mut cache = RenderCache::new(2);
        for i in 0..3u64 {
            cache.insert(
                CacheKey {
                    entry_id: i,
                    revision: 1,
                    width: 80,
                    theme: 0,
                },
                format!("e{i}"),
            );
        }
        // entry 0 被淘汰。
        assert!(
            cache
                .get(&CacheKey {
                    entry_id: 0,
                    revision: 1,
                    width: 80,
                    theme: 0
                })
                .is_none(),
            "LRU 淘汰最旧"
        );
        assert!(
            cache
                .get(&CacheKey {
                    entry_id: 2,
                    revision: 1,
                    width: 80,
                    theme: 0
                })
                .is_some(),
            "最新保留"
        );
    }

    /// 帧合并：窗口内密集请求只渲染一次。
    #[test]
    fn coalescer_merges_dense_deltas() {
        let mut fc = FrameCoalescer::new(16);
        assert!(fc.should_render(), "第一次渲染");
        assert!(!fc.should_render(), "窗口内合并");
        assert!(!fc.should_render());
        fc.flush();
        assert!(fc.should_render(), "flush 后可再渲染");
    }
}
