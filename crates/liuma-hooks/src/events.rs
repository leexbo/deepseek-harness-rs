//! events:hook/invoked · hook/result 载荷构造(源 hook-protocol/events.ts
//! 逐字对齐)。
//!
//! log-only、turn 封闭:事件对必须落在打开 turn 内(实现方判断;
//! SessionStart 等三个 emit 点在 turn 外运行不落记录)。decision 派生:
//! `output.decision ?? (continue===false ? 'stop' : 'pass')`;stderrSummary
//! trim + 截断(默认 500 字符,恰 500 不截,超出加 `…`)。

use crate::codec::HookOutput;
use serde_json::{Value, json};

/// 方言(源 HookDialect;hook/invoked.dialect 字段值)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDialect {
    ClaudeCode,
    Codex,
}

impl HookDialect {
    pub fn as_str(self) -> &'static str {
        match self {
            HookDialect::ClaudeCode => "claude-code",
            HookDialect::Codex => "codex",
        }
    }
}

/// stderrSummary 默认上限(源 DEFAULT_STDERR_SUMMARY_MAX_CHARS)
pub const DEFAULT_STDERR_SUMMARY_MAX_CHARS: usize = 500;

/// hook/invoked 载荷(源 HookInvocation;matcher 缺省时键整体省略)
#[derive(Debug, Clone)]
pub struct HookInvocation {
    pub turn: u64,
    pub point: String,
    pub dialect: HookDialect,
    /// 稳定 id 配对 invoked/result(形如 `claude-code:PreToolUse:3`)
    pub handler_id: String,
    pub matcher: Option<String>,
}

pub fn hook_invoked_payload(inv: &HookInvocation) -> Value {
    let mut v = json!({
        "turn": inv.turn,
        "point": inv.point,
        "dialect": inv.dialect.as_str(),
        "handlerId": inv.handler_id,
    });
    if let Some(m) = &inv.matcher {
        v["matcher"] = json!(m);
    }
    v
}

/// stderr 截断(源 summarizeStderr):trim、空⇒None、超 maxChars 截断加 `…`
pub fn summarize_stderr(stderr: &str, max_chars: usize) -> Option<String> {
    let t = stderr.trim();
    if t.is_empty() {
        return None;
    }
    let len = t.chars().count();
    if len > max_chars {
        let cut: String = t.chars().take(max_chars).collect();
        Some(format!("{cut}…"))
    } else {
        Some(t.to_string())
    }
}

/// hook/result 载荷(源 appendHookResult 派生规则;exitCode/stderrSummary
/// 缺席时键省略——零噪音惯例)
pub fn hook_result_payload(
    turn: u64,
    point: &str,
    handler_id: &str,
    output: &HookOutput,
    stderr_summary_max_chars: usize,
    duration_ms: i64,
) -> Value {
    let decision = match output.decision {
        Some(d) => d.as_str().to_string(),
        None => {
            if output.continue_ == Some(false) {
                "stop".to_string()
            } else {
                "pass".to_string()
            }
        }
    };
    let mut v = json!({
        "turn": turn,
        "point": point,
        "handlerId": handler_id,
        "decision": decision,
    });
    if let Some(code) = output.exit_code {
        v["exitCode"] = json!(code);
    }
    if let Some(summary) = summarize_stderr(&output.stderr, stderr_summary_max_chars) {
        v["stderrSummary"] = json!(summary);
    }
    v["durationMs"] = json!(duration_ms);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Decision;

    #[test]
    fn invoked_payload_omits_absent_matcher() {
        let inv = HookInvocation {
            turn: 2,
            point: "PreToolUse".into(),
            dialect: HookDialect::ClaudeCode,
            handler_id: "claude-code:PreToolUse:3".into(),
            matcher: Some("Bash".into()),
        };
        let v = hook_invoked_payload(&inv);
        assert_eq!(v["turn"], 2);
        assert_eq!(v["point"], "PreToolUse");
        assert_eq!(v["dialect"], "claude-code");
        assert_eq!(v["handlerId"], "claude-code:PreToolUse:3");
        assert_eq!(v["matcher"], "Bash");
        let inv = HookInvocation {
            matcher: None,
            ..inv
        };
        let v = hook_invoked_payload(&inv);
        assert!(v.get("matcher").is_none(), "matcher 缺省 = 键省略");
    }

    #[test]
    fn result_decision_derivation() {
        let base = |o: HookOutput| hook_result_payload(1, "Stop", "h", &o, 500, 10);
        // 显式 decision 胜过 continue:false 回退
        let o = HookOutput {
            decision: Some(Decision::Block),
            continue_: Some(false),
            ..Default::default()
        };
        assert_eq!(base(o)["decision"], "block");
        // continue:false ⇒ stop
        let o = HookOutput {
            continue_: Some(false),
            ..Default::default()
        };
        assert_eq!(base(o)["decision"], "stop");
        // 其余 ⇒ pass
        let o = HookOutput::default();
        assert_eq!(base(o)["decision"], "pass");
        // exit 2 block + exitCode 记录
        let o = crate::codec::parse_hook_output(Some(2), "", "boom", None);
        let v = hook_result_payload(1, "PreToolUse", "h", &o, 500, 42);
        assert_eq!(v["decision"], "block");
        assert_eq!(v["exitCode"], 2);
        assert_eq!(v["stderrSummary"], "boom");
        assert_eq!(v["durationMs"], 42);
    }

    #[test]
    fn stderr_summary_500_boundary_exclusive() {
        let x500 = "x".repeat(500);
        // 恰 500 原样
        assert_eq!(summarize_stderr(&x500, 500).unwrap(), x500);
        // 501 ⇒ 前 500 + …
        let x501 = format!("{x500}x");
        assert_eq!(summarize_stderr(&x501, 500).unwrap(), format!("{x500}…"));
        // 空白 ⇒ None
        assert_eq!(summarize_stderr("   ", 500), None);
        // 键省略:stderr 空 ⇒ 载荷无 stderrSummary
        let o = HookOutput::default();
        let v = hook_result_payload(1, "p", "h", &o, 500, 0);
        assert!(v.get("stderrSummary").is_none());
        assert!(v.get("exitCode").is_none());
    }
}
