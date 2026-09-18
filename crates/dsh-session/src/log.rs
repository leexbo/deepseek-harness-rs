//! 内存事件日志:seq 连续的运行时强制 + 查询 + 快照(重放底座)。
//!
//! 对应 WIT `dsh:session/event-log`;wasm 组件导出经本模块实现。
//! 追加式:事件一经 append 不可变,重放确定性由此而来(时钟经注入,信封即纯数据)。

use thiserror::Error;

use crate::envelope::{EnvelopeError, EventEnvelope, SESSION_FORMAT_VERSION, decode_envelope};

/// 日志操作错误
#[derive(Debug, Error, PartialEq)]
pub enum LogError {
    /// seq 不连续
    #[error("session event seq {actual} is not contiguous; expected {expected}")]
    NotContiguous {
        /// 实际收到的 seq
        actual: u64,
        /// 期望的 seq
        expected: u64,
    },
    /// 持久化失败(sink 写盘错误;事件不入内存)
    #[error("session event persistence failed: {0}")]
    Durability(String),
    /// 信封层错误
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
}

/// 持久化汇:append 时在**调用方日志锁内**被调,把带 seq 的信封写入
/// 介质。返回 Err = 落盘失败(事件整体被拒,内存/介质不分叉)
pub type DurabilitySink = Box<dyn Fn(&EventEnvelope) -> Result<(), String> + Send>;

/// 内存追加式事件日志。
///
/// `next_seq` 从 1 起;`append` 拒绝不连续事件(fail-fast 而非静默跳号)。
#[derive(Default)]
pub struct EventLog {
    events: Vec<EventEnvelope>,
    sink: Option<DurabilitySink>,
}

impl std::fmt::Debug for EventLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventLog")
            .field("high_water", &self.high_water())
            .field("durability", &self.sink.is_some())
            .finish_non_exhaustive()
    }
}

impl EventLog {
    /// 空日志
    pub fn new() -> Self {
        Self::default()
    }

    /// 装配持久化汇(替换语义;attach 装配点调用一次)
    pub fn set_durability_sink(&mut self, sink: DurabilitySink) {
        self.sink = Some(sink);
    }

    /// 是否已装配持久化汇
    pub fn has_durability(&self) -> bool {
        self.sink.is_some()
    }

    /// 下一个期望 seq(已 append 数 + 1;seq 从 1 起)
    pub fn next_seq(&self) -> u64 {
        self.events.len() as u64 + 1
    }

    /// 高水位(最新已 append seq)
    pub fn high_water(&self) -> u64 {
        self.events.len() as u64
    }

    /// 追加事件:seq 为 0 时自动分配,非 0 时必须与期望连续。
    pub fn append(&mut self, mut ev: EventEnvelope) -> Result<u64, LogError> {
        let expected = self.next_seq();
        if ev.seq == 0 {
            ev.seq = expected;
        } else if ev.seq != expected {
            return Err(LogError::NotContiguous {
                actual: ev.seq,
                expected,
            });
        }
        if let Some(sink) = &self.sink {
            sink(&ev).map_err(LogError::Durability)?;
        }
        self.events.push(ev);
        Ok(expected)
    }

    /// 按 seq 取事件(seq 从 1 起)
    pub fn get(&self, seq: u64) -> Option<&EventEnvelope> {
        self.events.get(seq.checked_sub(1)? as usize)
    }

    /// 按类型过滤(空过滤 = 全部)
    pub fn query(&self, type_filter: Option<&str>) -> Vec<&EventEnvelope> {
        self.events
            .iter()
            .filter(|ev| type_filter.is_none_or(|t| ev.r#type == t))
            .collect()
    }

    /// 全部事件
    pub fn iter(&self) -> std::slice::Iter<'_, EventEnvelope> {
        self.events.iter()
    }

    /// 快照:头部 + 事件数组(持久化形态;格式版本在头部)。
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "version": SESSION_FORMAT_VERSION,
            "events": self.events,
        })
    }

    /// 从快照重建(读取方守卫:每个事件都过 [`decode_envelope`])。
    pub fn from_snapshot(raw: &serde_json::Value) -> Result<Self, LogError> {
        let mut log = Self::new();
        for raw_ev in raw["events"].as_array().unwrap_or(&Vec::new()) {
            let ev = decode_envelope(raw_ev)?;
            log.append(ev)?;
        }
        Ok(log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn user_msg(content: &str) -> EventEnvelope {
        EventEnvelope::new("user/message", 0, json!({ "content": content }))
    }

    #[test]
    fn append_assigns_contiguous_seq() {
        let mut log = EventLog::new();
        assert_eq!(log.append(user_msg("a")).unwrap(), 1);
        assert_eq!(log.append(user_msg("b")).unwrap(), 2);
        assert_eq!(log.high_water(), 2);
    }

    #[test]
    fn append_rejects_gap() {
        let mut log = EventLog::new();
        let mut ev = user_msg("a");
        ev.seq = 7;
        assert_eq!(
            log.append(ev),
            Err(LogError::NotContiguous {
                actual: 7,
                expected: 1
            })
        );
    }

    #[test]
    fn snapshot_roundtrip_is_deterministic() {
        // 重放确定性:同日志两次「快照→重建」状态相等
        let mut log = EventLog::new();
        log.append(user_msg("a")).unwrap();
        log.append(EventEnvelope::new(
            "assistant/message",
            1,
            json!({"content": "b"}),
        ))
        .unwrap();
        let r1 = EventLog::from_snapshot(&log.snapshot()).unwrap();
        let r2 = EventLog::from_snapshot(&log.snapshot()).unwrap();
        assert_eq!(r1.snapshot(), r2.snapshot());
        assert_eq!(r1.snapshot(), log.snapshot());
    }

    #[test]
    fn rebuild_refuses_unknown_not_ignorable() {
        // 读取方守卫经 from_snapshot 生效:伪造含未知未标事件的快照被拒
        let bad = json!({
            "version": SESSION_FORMAT_VERSION,
            "events": [
                { "type": "user/message", "seq": 1, "time": 0, "data": {}, "ignorable": false },
                { "type": "evil/unknown", "seq": 2, "time": 0, "data": {}, "ignorable": false },
            ],
        });
        assert!(EventLog::from_snapshot(&bad).is_err());
    }

    #[test]
    fn query_filters_by_type() {
        let mut log = EventLog::new();
        log.append(user_msg("a")).unwrap();
        log.append(EventEnvelope::new(
            "assistant/message",
            1,
            json!({"content": "b"}),
        ))
        .unwrap();
        assert_eq!(log.query(Some("user/message")).len(), 1);
        assert_eq!(log.query(None).len(), 2);
    }

    /// 持久化汇:锁内被调,收到已定 seq 的信封(单写权威核心)
    #[test]
    fn durability_sink_receives_sequenced_envelope() {
        let seen: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let mut log = EventLog::new();
        log.set_durability_sink(Box::new(move |ev| {
            seen2.lock().unwrap().push(ev.seq);
            Ok(())
        }));
        log.append(user_msg("a")).unwrap();
        log.append(user_msg("b")).unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![1, 2], "sink 收到定序信封");
        assert!(log.has_durability());
    }

    /// 落盘失败 = 事件被拒(内存不落账);恢复后按期望 seq 续写
    #[test]
    fn durability_failure_rejects_event_atomically() {
        let ok = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let ok2 = ok.clone();
        let mut log = EventLog::new();
        log.set_durability_sink(Box::new(move |_ev| {
            if ok2.load(std::sync::atomic::Ordering::Relaxed) {
                Ok(())
            } else {
                Err("disk gone".into())
            }
        }));
        log.append(user_msg("a")).unwrap();
        ok.store(false, std::sync::atomic::Ordering::Relaxed);
        let err = log.append(user_msg("b")).unwrap_err();
        assert!(matches!(err, LogError::Durability(_)));
        assert_eq!(log.high_water(), 1, "失败事件不入内存");
        ok.store(true, std::sync::atomic::Ordering::Relaxed);
        log.append(user_msg("b")).unwrap();
        assert_eq!(log.high_water(), 2, "恢复后按期望 seq 续写");
    }

    /// 双线程并发 append(汇持文件写):文件行序 == seq 序。
    /// 单写权威行为锁——汇在调用方日志锁内执行,文件写被同一锁串行
    #[test]
    fn concurrent_append_file_order_matches_seq_order() {
        let dir = std::env::temp_dir().join(format!(
            "dsh-log-order-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let file = std::fs::File::create(&path).unwrap();
        let file = Arc::new(Mutex::new(file));
        let log = Arc::new(Mutex::new(EventLog::new()));
        {
            let f = file.clone();
            log.lock().unwrap().set_durability_sink(Box::new(move |ev| {
                use std::io::Write as _;
                let mut w = f.lock().unwrap();
                serde_json::to_writer(&mut *w, ev).unwrap();
                w.write_all(b"\n").unwrap();
                w.flush().unwrap();
                Ok(())
            }));
        }
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let log = log.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        log.lock()
                            .unwrap()
                            .append(EventEnvelope::new(
                                "user/message",
                                0,
                                serde_json::json!({ "content": format!("t{t}-{i}") }),
                            ))
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let seqs: Vec<u64> = text
            .lines()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["seq"].as_u64().unwrap()
            })
            .collect();
        assert_eq!(seqs.len(), 100);
        for (ix, s) in seqs.iter().enumerate() {
            assert_eq!(*s, (ix + 1) as u64, "文件行序应等于 seq 序");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
