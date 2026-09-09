//! 域内文本解析记忆化缓存(markdown 块 / ANSI 行 / 高亮窗口三处 parse
//! 缓存共同抽象):key + 内容哈希命中复用;短内容不入驻、超上限整体
//! 清空(简单防涨)。各域自持 `static`(key 撞车互不可见)。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// 记忆化缓存(键 = 调用方稳定 key;命中 = 同 key 且内容哈希一致)
pub(crate) struct MemoCache<V> {
    inner: Mutex<BTreeMap<String, (u64, Arc<V>)>>,
    cap: usize,
    min_len: usize,
}

impl<V> MemoCache<V> {
    /// `cap` 超限整体清空(简单防涨);`min_len` 以下内容不入缓存
    pub(crate) const fn new(cap: usize, min_len: usize) -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
            cap,
            min_len,
        }
    }

    /// 命中校验(同 key 同哈希);miss 返回 None,解析后经 [`Self::put`] 入缓存
    pub(crate) fn get(&self, key: &str, hash: u64) -> Option<Arc<V>> {
        let guard = self.inner.lock().expect("memo cache 锁中毒");
        match guard.get(key) {
            Some((h, v)) if *h == hash => Some(v.clone()),
            _ => None,
        }
    }

    /// 解析结果入缓存(长度门槛 + 容量清空防涨)
    pub(crate) fn put(&self, key: &str, hash: u64, value: Arc<V>, len: usize) {
        if len < self.min_len {
            return;
        }
        let mut guard = self.inner.lock().expect("memo cache 锁中毒");
        if guard.len() >= self.cap {
            guard.clear();
        }
        guard.insert(key.to_string(), (hash, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_on_key_and_hash() {
        let c = MemoCache::new(2, 2);
        assert!(c.get("a", 1).is_none());
        c.put("a", 1, Arc::new("x"), 3);
        assert_eq!(*c.get("a", 1).unwrap(), "x");
        assert!(c.get("a", 2).is_none(), "hash 变化应 miss");
    }

    #[test]
    fn clear_on_cap() {
        let c = MemoCache::new(2, 2);
        c.put("a", 1, Arc::new("x"), 3);
        c.put("b", 1, Arc::new("y"), 3);
        c.put("c", 1, Arc::new("z"), 3);
        assert!(c.get("a", 1).is_none(), "超 cap 应整体清空");
    }

    #[test]
    fn short_content_not_cached() {
        let c = MemoCache::new(2, 2);
        c.put("s", 1, Arc::new("s"), 1);
        assert!(c.get("s", 1).is_none(), "短内容不入缓存");
    }
}
