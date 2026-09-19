//! plan 状态折叠(纯函数;输入共享日志,无 IO 无时钟——重放确定)。

use liuma_session::{EventEnvelope, EventLog};
use serde_json::json;

/// plan 族事件信封构造(`ty` ∈ plan/submitted|approved|declined|cancelled;
/// `feedback` 仅 declined 携带,空白视同无)。落档与回声由各宿主评审 port
/// 负责(liuma-core/Gateway/CLI 共用此构造——单一事实来源)。
pub fn plan_envelope(ty: &str, plan: &str, feedback: Option<&str>, ts: i64) -> EventEnvelope {
    let mut data = json!({ "plan": plan });
    if let Some(fb) = feedback.filter(|t| !t.trim().is_empty()) {
        data["feedback"] = json!(fb);
    }
    EventEnvelope::new(ty, ts, data)
}

/// `session/mode` 事件信封(mode ∈ standard|plan)。
pub fn mode_envelope(mode: &str, ts: i64) -> EventEnvelope {
    EventEnvelope::new("session/mode", ts, json!({ "mode": mode }))
}

/// 当前模式:最近一条 `session/mode`(缺省 standard)。
pub fn current_mode(log: &EventLog) -> String {
    log.iter()
        .rev()
        .find(|e| e.r#type == "session/mode")
        .and_then(|e| e.data["mode"].as_str())
        .unwrap_or("standard")
        .to_string()
}

/// 从日志读 plan 态(最近一条 `session/mode` 与 `plan/approved`)。
///
/// 模式与活跃计划影响模型可见 prompt → 必须来自日志(重放一致)。
pub fn plan_state(log: &EventLog) -> (bool, Option<String>) {
    let mode = current_mode(log);
    let active = log
        .iter()
        .rev()
        .find(|e| e.r#type == "plan/approved")
        .and_then(|e| e.data["plan"].as_str().map(String::from));
    (mode == "plan", active)
}

/// 待审判定:最近一条 `plan/submitted` 且其后无终局
/// (approved / declined / cancelled)。
///
/// 冷恢复路径(驱动启动 re-ask)据此识别「崩溃时评审未收口」。
pub fn pending_plan(log: &EventLog) -> Option<String> {
    let submitted = log.iter().rev().find(|e| e.r#type == "plan/submitted")?;
    let plan = submitted.data["plan"].as_str()?.to_string();
    let resolved_after = log.iter().any(|e| {
        matches!(
            e.r#type.as_str(),
            "plan/approved" | "plan/declined" | "plan/cancelled"
        ) && e.seq > submitted.seq
    });
    if resolved_after { None } else { Some(plan) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liuma_session::EventEnvelope;
    use serde_json::json;

    fn log_of(events: &[(&str, serde_json::Value)]) -> EventLog {
        let mut log = EventLog::new();
        for (i, (ty, data)) in events.iter().enumerate() {
            log.append(EventEnvelope::new(ty, i as i64, data.clone()))
                .unwrap();
        }
        log
    }

    #[test]
    fn mode_defaults_to_standard() {
        assert_eq!(current_mode(&EventLog::new()), "standard");
        let log = log_of(&[("session/mode", json!({ "mode": "plan" }))]);
        assert_eq!(current_mode(&log), "plan");
    }

    #[test]
    fn plan_state_reads_mode_and_active_plan() {
        let log = log_of(&[
            ("session/mode", json!({ "mode": "plan" })),
            ("plan/approved", json!({ "plan": "# t" })),
            ("session/mode", json!({ "mode": "standard" })),
        ]);
        let (mode, active) = plan_state(&log);
        assert!(!mode);
        assert_eq!(active.as_deref(), Some("# t"));
    }

    #[test]
    fn pending_plan_until_terminal_event() {
        // 无提交 → 无待审
        assert!(pending_plan(&EventLog::new()).is_none());
        let mode = || ("session/mode", serde_json::json!({ "mode": "plan" }));
        // 提交后无终局 → 待审
        let submitted = [
            mode(),
            ("plan/submitted", serde_json::json!({ "plan": "# p" })),
        ];
        assert_eq!(pending_plan(&log_of(&submitted)).as_deref(), Some("# p"));
        // 三类终局都收口
        for terminal in ["plan/approved", "plan/declined", "plan/cancelled"] {
            let log = log_of(&[
                mode(),
                ("plan/submitted", serde_json::json!({ "plan": "# p" })),
                (terminal, serde_json::json!({ "plan": "# p" })),
            ]);
            assert!(pending_plan(&log).is_none(), "{terminal} 应收口待审");
        }
        // 终局后再次提交 → 重新待审
        let resubmitted = [
            mode(),
            ("plan/submitted", serde_json::json!({ "plan": "# p" })),
            ("plan/declined", serde_json::json!({ "plan": "# p" })),
            ("plan/submitted", serde_json::json!({ "plan": "# p2" })),
        ];
        assert_eq!(pending_plan(&log_of(&resubmitted)).as_deref(), Some("# p2"));
    }
}
