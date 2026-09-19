//! 审计事件溯源化(E5):跨边界调用记录入事件流,归因统一走 sourceEventSeqs。
//!
//! 机制(宿主记录跨边界调用):
//! - 宿主在每次跨边界调用(llm 出网 / 工具执行 / 子进程)前 append
//!   一条 `audit/call` 事件——**记录优先**,先落日志再执行;
//! - 审计事件携带 `sourceEventSeqs` 指向因果上游(如触发本次请求的
//!   user/message、携带 tool_calls 的 assistant/message),与 surface 事件
//!   共用同一引用链机制(「审计与 session 日志一套机制」);
//! - 审计不进消息面([`crate::events::message_from_event`] 返回 None),
//!   不影响「模型可见 ⟺ 已记录」不变式的比对侧;
//! - 归因可随会话重放:重放日志即重建完整审计链(见 [`attribution_chain`])。

use serde::Serialize;
use serde_json::Value;

use crate::EventEnvelope;
use crate::log::EventLog;

/// 审计边界:LLM 出网请求
pub const BOUNDARY_LLM: &str = "llm";
/// 审计边界:工具执行
pub const BOUNDARY_TOOL: &str = "tool";
/// 审计边界:子进程 spawn
pub const BOUNDARY_PROCESS: &str = "process";

/// 构造 `audit/call` 事件(宿主跨边界调用点的记录器入口)。
///
/// `source_event_seqs` 是因果上游(seq 必须已存在于日志——由
/// [`EventLog::append`] 的 seq 连续与调用方保证)。
pub fn audit_call_event(
    time: i64,
    boundary: &str,
    operation: &str,
    detail: Value,
    source_event_seqs: Vec<u64>,
) -> EventEnvelope {
    EventEnvelope::new_attributed(
        "audit/call",
        time,
        serde_json::json!({
            "boundary": boundary,
            "operation": operation,
            "detail": detail,
        }),
        source_event_seqs,
    )
}

/// 从日志解出的一条审计记录
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditRecord {
    /// 审计事件自身的 seq
    pub seq: u64,
    /// 跨越的边界(llm/tool/process)
    pub boundary: String,
    /// 操作名
    pub operation: String,
    /// 边界特有细节
    pub detail: Value,
    /// 因果上游 seq 列表
    pub source_seqs: Vec<u64>,
}

/// 单条审计的完整归因:审计记录 + 已解析到 (seq, 类型) 的上游事件
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Attribution {
    /// 审计记录本体
    pub audit: AuditRecord,
    /// 因果上游(解析后的 (seq, 事件类型);悬空引用在此暴露)
    pub sources: Vec<(u64, String)>,
}

/// 从日志提取全部审计记录(保序)。
pub fn audit_records(log: &EventLog) -> Vec<AuditRecord> {
    log.iter()
        .filter(|ev| ev.r#type == "audit/call")
        .map(|ev| AuditRecord {
            seq: ev.seq,
            boundary: ev.data["boundary"].as_str().unwrap_or_default().to_string(),
            operation: ev.data["operation"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            detail: ev.data["detail"].clone(),
            source_seqs: ev.source_event_seqs.clone().unwrap_or_default(),
        })
        .collect()
}

/// 归因链:全部审计记录 + 上游解析。
///
/// 悬空引用(seq 不在日志内)以 `(seq, "<missing>")` 暴露——
/// 调用方可据此拒绝,而不是静默丢失归因。
pub fn attribution_chain(log: &EventLog) -> Vec<Attribution> {
    audit_records(log)
        .into_iter()
        .map(|audit| {
            let sources = audit
                .source_seqs
                .iter()
                .map(|seq| match log.get(*seq) {
                    Some(ev) => (*seq, ev.r#type.clone()),
                    None => (*seq, "<missing>".to_string()),
                })
                .collect();
            Attribution { audit, sources }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn audit_roundtrip_and_attribution() {
        let mut log = EventLog::new();
        let user_seq = log
            .append(EventEnvelope::new(
                "user/message",
                0,
                json!({ "content": "hi" }),
            ))
            .unwrap();
        log.append(audit_call_event(
            1,
            BOUNDARY_LLM,
            "request",
            json!({ "model": "test" }),
            vec![user_seq],
        ))
        .unwrap();

        let chain = attribution_chain(&log);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].audit.boundary, BOUNDARY_LLM);
        assert_eq!(
            chain[0].sources,
            vec![(user_seq, "user/message".to_string())]
        );
    }

    #[test]
    fn dangling_reference_is_visible() {
        let mut log = EventLog::new();
        log.append(audit_call_event(
            0,
            BOUNDARY_TOOL,
            "bash",
            json!({}),
            vec![99],
        ))
        .unwrap();
        let chain = attribution_chain(&log);
        assert_eq!(chain[0].sources, vec![(99, "<missing>".to_string())]);
    }
}
