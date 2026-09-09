//! exit_plan_mode 工具:模型提交计划。
//!
//! 仅 plan 模式可调用(读共享日志最近一条 session/mode 判定);提交的
//! 计划经 take_state_events 落为 plan/submitted 事件,用户批准后成为
//! 活跃计划(plan/approved)。工具目录跨模式不变(request-cache 稳定),
//! plan 态的「禁止变更」由 prompt 约束承担。

use std::sync::{Arc, Mutex};

use dsh_agent_loop::{ToolCallRequest, ToolOutput, ToolPort};
use dsh_session::EventLog;
use serde_json::{Value, json};

/// plan 工具:exit_plan_mode
pub struct PlanTool {
    /// 共享会话日志(只读:当前模式)
    log: Arc<Mutex<EventLog>>,
    /// 缓冲的持久状态事件(plan/submitted)
    pending: Vec<(String, Value)>,
}

impl PlanTool {
    /// 以共享日志构建(与 engine/闸门同一日志实例)
    pub fn new(log: Arc<Mutex<EventLog>>) -> Self {
        Self {
            log,
            pending: Vec::new(),
        }
    }

    /// 当前模式:最近一条 session/mode(缺省 standard)
    fn current_mode(&self) -> String {
        let Ok(log) = self.log.lock() else {
            return "standard".into();
        };
        log.iter()
            .rev()
            .find(|e| e.r#type == "session/mode")
            .and_then(|e| e.data["mode"].as_str())
            .unwrap_or("standard")
            .to_string()
    }
}

impl ToolPort for PlanTool {
    fn specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "exit_plan_mode",
                "description": "Submit the plan for user approval. Only valid in plan mode; must be the only tool call in the response.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "plan": { "type": "string", "description": "Complete plan as markdown: goal, steps grouped by subsystem, tests, risks" }
                    },
                    "required": ["plan"],
                },
            },
        })]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        if call.name != "exit_plan_mode" {
            return ToolOutput {
                output: format!("unknown tool: {}", call.name),
                success: false,
                ..Default::default()
            };
        }
        let fail = |msg: String| ToolOutput {
            output: msg,
            success: false,
            ..Default::default()
        };
        if self.current_mode() != "plan" {
            return fail("exit_plan_mode is only valid in plan mode".into());
        }
        let arguments: Value = if let Some(s) = call.arguments.as_str() {
            serde_json::from_str(s).unwrap_or(json!({}))
        } else {
            call.arguments.clone()
        };
        let Some(plan) = arguments["plan"].as_str() else {
            return fail("exit_plan_mode requires arguments.plan (string)".into());
        };
        if plan.trim().is_empty() {
            return fail("plan must not be empty".into());
        }
        self.pending
            .push(("plan/submitted".into(), json!({ "plan": plan })));
        ToolOutput {
            output: "plan submitted for approval; stay in plan mode until the user approves".into(),
            success: true,
            ..Default::default()
        }
    }

    fn take_state_events(&mut self) -> Vec<(String, Value)> {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::EventEnvelope;

    fn call(plan: &str) -> ToolCallRequest {
        ToolCallRequest {
            name: "exit_plan_mode".into(),
            arguments: json!({ "plan": plan }),
        }
    }

    fn log_with_mode(mode: Option<&str>) -> Arc<Mutex<EventLog>> {
        let log = Arc::new(Mutex::new(EventLog::new()));
        if let Some(m) = mode {
            log.lock()
                .unwrap()
                .append(EventEnvelope::new("session/mode", 0, json!({ "mode": m })))
                .unwrap();
        }
        log
    }

    #[tokio::test]
    async fn rejects_outside_plan_mode() {
        let mut t = PlanTool::new(log_with_mode(None));
        let out = ToolPort::execute(&mut t, &call("# p")).await;
        assert!(!out.success);
        assert!(out.output.contains("plan mode"));

        let mut t = PlanTool::new(log_with_mode(Some("standard")));
        let out = ToolPort::execute(&mut t, &call("# p")).await;
        assert!(!out.success);
    }

    #[tokio::test]
    async fn submits_in_plan_mode() {
        let mut t = PlanTool::new(log_with_mode(Some("plan")));
        let out = ToolPort::execute(&mut t, &call("# fix the bug\n1. read code")).await;
        assert!(out.success, "{}", out.output);
        let events = t.take_state_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "plan/submitted");
        assert!(
            events[0].1["plan"]
                .as_str()
                .unwrap()
                .contains("fix the bug")
        );
        assert!(t.take_state_events().is_empty());
    }
}
