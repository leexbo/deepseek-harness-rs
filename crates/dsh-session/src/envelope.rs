//! 事件信封:纯数据契约。
//!
//! 语义要点(与格式无关):
//! - `SESSION_FORMAT_VERSION`:格式变更即递增,旧日志拒绝重建;
//! - seq 连续(见 [`crate::log::EventLog`] 的运行时强制);
//! - `ignorable` 未知事件守卫:读取方遇到未登记且未标记 ignorable 的事件类型
//!   **必须拒绝重建**(见 [`decode_envelope`]),防止静默损坏;
//! - `surface_op` 仅存在于 surface 事件(user/message、assistant/message、tool/result);
//! - `source_event_seqs` 存在于可归因事件(surface 三件 + `audit/call`,
//!   E5 统一归因集,见 [`crate::events::ATTRIBUTED_EVENT_TYPES`])。

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 会话日志格式版本。
///
/// 独立项目无旧日志可读义务;版本号唯一职责是让「本版本写入的日志」与
/// 「读取方支持的版本」可对齐——不匹配即拒绝,不做迁移。
pub const SESSION_FORMAT_VERSION: u32 = 1;

/// 事件信封解码错误
#[derive(Debug, Error, PartialEq)]
pub enum EnvelopeError {
    /// 未登记的事件类型且未标 ignorable——读取方拒绝重建(防静默损坏)
    #[error("unknown event type `{0}` is not marked ignorable; refusing to interpret the log")]
    UnknownNotIgnorable(String),
    /// 已登记但不可归因的事件携带 sourceEventSeqs——引用链合法性违反
    #[error("event type `{0}` is not attributable but carries sourceEventSeqs; refusing")]
    MisattributedSources(String),
    /// JSON 层错误
    #[error("envelope decode failed: {0}")]
    Decode(String),
}

/// 事件信封。`data` 载荷按 `type` 判别(强类型联合见 [`crate::events::SessionEventData`])。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// 事件类型(判别键)
    pub r#type: String,
    /// 连续序号,从 1 起
    pub seq: u64,
    /// 毫秒 Unix 时间戳(经显式时钟 import 注入,组件内不得直接读时钟)
    pub time: i64,
    /// 事件载荷(按 type 判别的 JSON)
    pub data: serde_json::Value,
    /// surface 事件的操作标识(仅 user/message、assistant/message、tool/result)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface_op: Option<String>,
    /// 引用链:本事件派生自哪些事件(仅可归因事件:surface 三件 + audit/call)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_event_seqs: Option<Vec<u64>>,
    /// 未知类型守卫:未登记类型必须为 true 才可被读取方跳过
    #[serde(default)]
    pub ignorable: bool,
}

impl EventEnvelope {
    /// 构造不可忽略事件(seq 由 [`crate::log::EventLog::append`] 校验)
    pub fn new(r#type: &str, time: i64, data: serde_json::Value) -> Self {
        Self {
            r#type: r#type.to_string(),
            seq: 0,
            time,
            data,
            surface_op: None,
            source_event_seqs: None,
            ignorable: false,
        }
    }

    /// 构造可忽略事件(未知于旧读取方时不阻断重建)
    pub fn new_ignorable(r#type: &str, time: i64, data: serde_json::Value) -> Self {
        Self {
            ignorable: true,
            ..Self::new(r#type, time, data)
        }
    }

    /// 构造带归因引用链的事件(可归因事件专用:audit/call 等,E5)。
    pub fn new_attributed(
        r#type: &str,
        time: i64,
        data: serde_json::Value,
        source_event_seqs: Vec<u64>,
    ) -> Self {
        Self {
            source_event_seqs: Some(source_event_seqs),
            ..Self::new(r#type, time, data)
        }
    }
}

/// 读取方入口:解码并执行未知类型守卫与归因守卫。
///
/// 已登记类型(见 [`crate::events::KNOWN_EVENT_TYPES`])直接通过;
/// 未登记类型仅当 `ignorable == true` 时通过(载荷保留原样,投影自行决定忽略)。
/// 归因守卫仅约束已登记类型:不可归因事件携带 `sourceEventSeqs` 即拒绝
/// (未知 ignorable 事件不约束——前向兼容,新版本写入的归因事件旧读取方可跳过)。
pub fn decode_envelope(raw: &serde_json::Value) -> Result<EventEnvelope, EnvelopeError> {
    let envelope: EventEnvelope =
        serde_json::from_value(raw.clone()).map_err(|e| EnvelopeError::Decode(e.to_string()))?;
    let known = crate::events::KNOWN_EVENT_TYPES.contains(&envelope.r#type.as_str());
    if !known && !envelope.ignorable {
        return Err(EnvelopeError::UnknownNotIgnorable(envelope.r#type));
    }
    if known
        && envelope.source_event_seqs.is_some()
        && !crate::events::ATTRIBUTED_EVENT_TYPES.contains(&envelope.r#type.as_str())
    {
        return Err(EnvelopeError::MisattributedSources(envelope.r#type));
    }
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(r#type: &str, ignorable: bool) -> serde_json::Value {
        json!({
            "type": r#type,
            "seq": 1,
            "time": 0,
            "data": {},
            "ignorable": ignorable,
        })
    }

    #[test]
    fn known_type_decodes() {
        let ev = decode_envelope(&raw("user/message", false)).expect("known");
        assert_eq!(ev.seq, 1);
    }

    /// 队列 splice 已登记:含 agent/inbox/spliced 的日志整份可读
    /// (漏登记导致含 splice 会话拒读——重开即「会话没有加载」)
    #[test]
    fn inbox_spliced_is_known() {
        let ev = decode_envelope(&raw("agent/inbox/spliced", false)).expect("known");
        assert_eq!(ev.r#type, "agent/inbox/spliced");
    }

    /// hooks 桥事件对已登记:含 hook/invoked·result 的会话日志整份可读
    /// (漏登记导致 turso FTS 索引同步失败 + 检索工具拒读——
    /// 2026-09-12 现场实测,与 agent/inbox/spliced 同一教训)
    #[test]
    fn hook_event_pair_is_known() {
        let invoked = decode_envelope(&raw("hook/invoked", false)).expect("known");
        assert_eq!(invoked.r#type, "hook/invoked");
        let result = decode_envelope(&raw("hook/result", false)).expect("known");
        assert_eq!(result.r#type, "hook/result");
    }

    #[test]
    fn unknown_ignorable_decodes() {
        assert!(decode_envelope(&raw("future/thing", true)).is_ok());
    }

    #[test]
    fn unknown_not_ignorable_refused() {
        let err = decode_envelope(&raw("future/thing", false)).unwrap_err();
        assert_eq!(
            err,
            EnvelopeError::UnknownNotIgnorable("future/thing".to_string())
        );
    }

    #[test]
    fn ignorable_defaults_false() {
        // 缺省 ignorable 即 false:未知事件默认拒绝(继承「Absent means required」)
        let mut v = raw("future/thing", false);
        v.as_object_mut().unwrap().remove("ignorable");
        assert!(matches!(
            decode_envelope(&v),
            Err(EnvelopeError::UnknownNotIgnorable(_))
        ));
    }

    #[test]
    fn known_unattributed_type_with_sources_refused() {
        // 归因守卫:不可归因的已登记事件携带 sourceEventSeqs 即拒绝
        let mut v = raw("turn/start", false);
        v["source_event_seqs"] = json!([1]);
        assert!(matches!(
            decode_envelope(&v),
            Err(EnvelopeError::MisattributedSources(t)) if t == "turn/start"
        ));
    }

    #[test]
    fn audit_call_with_sources_decodes() {
        let mut v = raw("audit/call", false);
        v["source_event_seqs"] = json!([2]);
        v["data"] = json!({ "boundary": "llm", "operation": "request", "detail": {} });
        let ev = decode_envelope(&v).expect("audit/call 可归因");
        assert_eq!(ev.source_event_seqs.as_deref(), Some(&[2u64][..]));
    }

    #[test]
    fn unknown_ignorable_with_sources_passes() {
        // 前向兼容:未知 ignorable 事件带归因链不约束
        let mut v = raw("future/audit", true);
        v["source_event_seqs"] = json!([1]);
        assert!(decode_envelope(&v).is_ok());
    }
}
