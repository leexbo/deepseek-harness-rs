//! goal 工具:会话目标增改查。
//!
//! 与 todo_write 同构(整表快照 → goal/state):目标 = 用户可感知的会话级
//! 事实,全量快照入日志;单边界规则下工具缓冲、engine 追加;
//! 恢复 = 懒读最近一条 goal/state。

use std::sync::{Arc, Mutex};

use dsh_agent_loop::{ToolCallRequest, ToolOutput, ToolPort};
use dsh_session::EventLog;
use serde_json::{Value, json};

/// goal 工具:目标列表状态机 + 日志恢复
pub struct GoalTool {
    /// 共享会话日志(只读:恢复最近 goal/state)
    log: Arc<Mutex<EventLog>>,
    goals: Vec<dsh_session::GoalItem>,
    restored: bool,
    /// 缓冲的持久状态事件(engine 于 tool/result 后取走)
    pending: Vec<(String, Value)>,
}

impl GoalTool {
    /// 以共享日志构建(与 engine/闸门同一日志实例)
    pub fn new(log: Arc<Mutex<EventLog>>) -> Self {
        Self {
            log,
            goals: Vec::new(),
            restored: false,
            pending: Vec::new(),
        }
    }

    /// 懒恢复:读最近一条 goal/state(无则视为空列表)
    fn restore_if_needed(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let Ok(log) = self.log.lock() else {
            return;
        };
        let Some(ev) = log.iter().rev().find(|e| e.r#type == "goal/state") else {
            return;
        };
        let Some(items) = ev.data["goals"].as_array() else {
            return;
        };
        self.goals = items
            .iter()
            .filter_map(|item| {
                Some(dsh_session::GoalItem {
                    id: item["id"].as_u64()?,
                    text: item["text"].as_str()?.to_string(),
                    done: item["done"].as_bool()?,
                    paused: item["paused"].as_bool().unwrap_or(false),
                })
            })
            .collect();
    }

    /// 渲染当前目标列表
    fn render(&self) -> String {
        if self.goals.is_empty() {
            return "(no goals)".into();
        }
        self.goals
            .iter()
            .map(|g| {
                let mark = if g.done { "x" } else { " " };
                format!("{}. [{}] {}", g.id, mark, g.text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 缓冲全量快照(engine 追加为 goal/state 事件)
    fn snapshot(&mut self) {
        let goals: Vec<Value> = self
            .goals
            .iter()
            .map(|g| json!({ "id": g.id, "text": g.text, "done": g.done }))
            .collect();
        self.pending
            .push(("goal/state".into(), json!({ "goals": goals })));
    }
}

impl ToolPort for GoalTool {
    fn specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "goal",
                "description": "Track session-level goals (user-facing outcomes, coarser than todos). add: state a goal; complete: mark done; list: show all.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["add", "complete", "list"], "description": "Operation to perform" },
                        "text": { "type": "string", "description": "Goal description (action=add)" },
                        "id": { "type": "integer", "description": "Goal id (action=complete)" }
                    },
                    "required": ["action"],
                },
            },
        })]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        if call.name != "goal" {
            return ToolOutput {
                output: format!("unknown tool: {}", call.name),
                success: false,
                ..Default::default()
            };
        }
        self.restore_if_needed();
        let fail = |msg: String| ToolOutput {
            output: msg,
            success: false,
            ..Default::default()
        };
        let arguments: Value = if let Some(s) = call.arguments.as_str() {
            serde_json::from_str(s).unwrap_or(json!({}))
        } else {
            call.arguments.clone()
        };
        let Some(action) = arguments["action"].as_str() else {
            return fail("goal requires arguments.action (add/complete/list)".into());
        };
        match action {
            "add" => {
                let Some(text) = arguments["text"].as_str() else {
                    return fail("goal add requires arguments.text (string)".into());
                };
                let id = self.goals.iter().map(|g| g.id).max().unwrap_or(0) + 1;
                self.goals.push(dsh_session::GoalItem {
                    id,
                    text: text.to_string(),
                    done: false,
                    paused: false,
                });
                self.snapshot();
                ToolOutput {
                    output: format!("added goal {id}: {text}\n{}", self.render()),
                    success: true,
                    ..Default::default()
                }
            }
            "complete" => {
                let Some(id) = arguments["id"].as_u64() else {
                    return fail("goal complete requires arguments.id (integer)".into());
                };
                let Some(goal) = self.goals.iter_mut().find(|g| g.id == id) else {
                    return fail(format!("goal {id} not found"));
                };
                goal.done = true;
                self.snapshot();
                ToolOutput {
                    output: format!("completed goal {id}\n{}", self.render()),
                    success: true,
                    ..Default::default()
                }
            }
            "list" => ToolOutput {
                output: self.render(),
                success: true,
                ..Default::default()
            },
            other => fail(format!("unknown action: {other}")),
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

    fn call(args: Value) -> ToolCallRequest {
        ToolCallRequest {
            name: "goal".into(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn add_complete_list_and_restore() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut t = GoalTool::new(Arc::clone(&log));
        let out = t
            .execute(&call(json!({ "action": "add", "text": "ship WP-6" })))
            .await;
        assert!(out.success);
        assert!(out.output.contains("1. [ ] ship WP-6"));

        let out = t
            .execute(&call(json!({ "action": "complete", "id": 1 })))
            .await;
        assert!(out.output.contains("1. [x] ship WP-6"));

        let missing = t
            .execute(&call(json!({ "action": "complete", "id": 9 })))
            .await;
        assert!(!missing.success);

        assert_eq!(t.take_state_events().len(), 2);

        // 状态事件真实入日志后,新实例恢复
        let ev = EventEnvelope::new(
            "goal/state",
            0,
            json!({ "goals": [ { "id": 1, "text": "ship WP-6", "done": true } ] }),
        );
        log.lock().unwrap().append(ev).unwrap();
        let mut revived = GoalTool::new(Arc::clone(&log));
        let out = revived.execute(&call(json!({ "action": "list" }))).await;
        assert!(out.output.contains("1. [x] ship WP-6"));
    }
}
