//! todo_write 工具:任务列表整表替换。
//!
//! 模型每次调用发送**完整列表**,REPLACE 语义——无部分更新、无逐条
//! 编辑;描述明示「开工前每步一条」的工作流引导。状态入日志
//! (`todo/write` 全量快照):瞬态放内存会破坏崩溃恢复,且 todo 是
//! 用户可感知事实。单边界规则:工具不直接写日志——变更时缓冲
//! (type, data),engine 在 tool/result 后经 [`ToolPort::take_state_events`]
//! 取走追加;崩溃恢复 = 懒读共享日志最近一条 todo/write。

use std::sync::{Arc, Mutex};

use dsh_agent_loop::{ToolCallRequest, ToolOutput, ToolPort};
use dsh_session::EventLog;
use serde_json::{Value, json};

/// 合法状态集
const STATUSES: [&str; 3] = ["pending", "in_progress", "completed"];

/// 工具描述(含 parallel 段:standard 预设带 subagent/jobs 并行工作流)
const DESCRIPTION: &str = "Record and update a structured task list for the current work. Send the ENTIRE \
list every call — it REPLACES the previous list (there are no partial updates, \
no per-item edits). Use it to plan multi-step work and show progress: add one \
todo per concrete step before you start. Mark every todo being actively worked \
on `in_progress` — several at once when work genuinely runs in parallel (e.g. \
concurrent subagents or background commands), one for sequential work; while \
work remains, at least one task should be `in_progress`. Mark a todo \
`completed` the moment it is done (do not batch completions), and allow no \
`in_progress` item only once all work is complete. Skip the list for trivial \
single-step tasks. Statuses: `pending` (not started), `in_progress` (being \
worked on now), `completed` (finished).";

/// todo_write 工具:整表替换状态机 + 日志恢复
pub struct TodoWriteTool {
    /// 共享会话日志(只读:恢复最近 todo/write)
    log: Arc<Mutex<EventLog>>,
    /// 当前任务列表(首次执行前懒恢复)
    todos: Vec<dsh_session::TodoItem>,
    restored: bool,
    /// 缓冲的持久状态事件(engine 于 tool/result 后取走)
    pending: Vec<(String, Value)>,
}

impl TodoWriteTool {
    /// 以共享日志构建(与 engine/闸门同一日志实例)
    pub fn new(log: Arc<Mutex<EventLog>>) -> Self {
        Self {
            log,
            todos: Vec::new(),
            restored: false,
            pending: Vec::new(),
        }
    }

    /// 懒恢复:读最近一条 todo/write(无则视为空列表)
    fn restore_if_needed(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let Ok(log) = self.log.lock() else {
            return;
        };
        let Some(ev) = log.iter().rev().find(|e| e.r#type == "todo/write") else {
            return;
        };
        let Some(items) = ev.data["todos"].as_array() else {
            return;
        };
        self.todos = items
            .iter()
            .filter_map(|item| {
                Some(dsh_session::TodoItem {
                    content: item["content"].as_str()?.to_string(),
                    status: item["status"].as_str()?.to_string(),
                })
            })
            .collect();
    }

    /// 渲染当前列表为工具输出
    fn render(&self, pending: u64, in_progress: u64, completed: u64) -> String {
        format!(
            "Updated todo list: {pending} pending, {in_progress} in progress, {completed} completed."
        )
    }

    /// 缓冲一条全量快照(engine 追加为 todo/write 事件)
    fn snapshot(&mut self) {
        let todos: Vec<Value> = self
            .todos
            .iter()
            .map(|t| json!({ "content": t.content, "status": t.status }))
            .collect();
        self.pending
            .push(("todo/write".into(), json!({ "todos": todos })));
    }

    /// 校验并归一模型写入的整表(toTodoList 语义:非空唯一 content +
    /// 状态枚举已在 schema 边界,此处补内容约束)
    fn canonicalize(raw: &[Value]) -> Result<Vec<dsh_session::TodoItem>, String> {
        let mut todos = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for item in raw {
            let Some(content) = item["content"].as_str() else {
                return Err("invalid todo: `content` must be a string".into());
            };
            let content = content.trim();
            if content.is_empty() {
                return Err("invalid todo: `content` must be a non-empty string".into());
            }
            if !seen.insert(content.to_string()) {
                return Err(format!("invalid todos: duplicate content {content:?}"));
            }
            let Some(status) = item["status"].as_str() else {
                return Err("invalid todo: `status` must be a string".into());
            };
            if !STATUSES.contains(&status) {
                return Err(format!(
                    "invalid status {status}; must be one of {STATUSES:?}"
                ));
            }
            todos.push(dsh_session::TodoItem {
                content: content.to_string(),
                status: status.to_string(),
            });
        }
        Ok(todos)
    }

    async fn run(&mut self, call: &ToolCallRequest) -> ToolOutput {
        self.restore_if_needed();
        let fail = |msg: String| ToolOutput {
            output: msg,
            success: false,
            ..Default::default()
        };
        // arguments 容忍 JSON 字符串(与 BashTool/FileTools 同策略)
        let arguments: Value = if let Some(s) = call.arguments.as_str() {
            serde_json::from_str(s).unwrap_or(json!({}))
        } else {
            call.arguments.clone()
        };
        let Some(raw) = arguments["todos"].as_array() else {
            return fail("todo_write requires arguments.todos (array)".into());
        };
        let todos = match Self::canonicalize(raw) {
            Ok(t) => t,
            Err(msg) => return fail(msg),
        };
        self.todos = todos;
        self.snapshot();
        let count = |status: &str| self.todos.iter().filter(|t| t.status == status).count() as u64;
        ToolOutput {
            output: self.render(count("pending"), count("in_progress"), count("completed")),
            success: true,
            ..Default::default()
        }
    }
}

impl ToolPort for TodoWriteTool {
    fn specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "todo_write",
                "description": DESCRIPTION,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "todos": {
                            "type": "array",
                            "description": "The COMPLETE task list, replacing any previous list.",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "content": { "type": "string", "description": "What the task is — a short imperative line." },
                                    "status": { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "pending (not started) | in_progress (now) | completed (done)." }
                                },
                                "required": ["content", "status"]
                            }
                        }
                    },
                    "required": ["todos"],
                },
            },
        })]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        if call.name != "todo_write" {
            return ToolOutput {
                output: format!("unknown tool: {}", call.name),
                success: false,
                ..Default::default()
            };
        }
        self.run(call).await
    }

    fn take_state_events(&mut self) -> Vec<(String, Value)> {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::EventEnvelope;

    fn tool() -> TodoWriteTool {
        TodoWriteTool::new(Arc::new(Mutex::new(EventLog::new())))
    }

    fn call(args: Value) -> ToolCallRequest {
        ToolCallRequest {
            name: "todo_write".into(),
            arguments: args,
        }
    }

    fn todos(items: &[(&str, &str)]) -> Value {
        json!({ "todos": items.iter().map(|(c, s)| json!({
            "content": c, "status": s
        })).collect::<Vec<_>>() })
    }

    #[tokio::test]
    async fn whole_list_replacement_semantics() {
        let mut t = tool();
        let out = t
            .execute(&call(todos(&[
                ("write tests", "pending"),
                ("review", "in_progress"),
            ])))
            .await;
        assert!(out.success);
        assert_eq!(
            out.output,
            "Updated todo list: 1 pending, 1 in progress, 0 completed."
        );

        // 整表替换:第二次调用不含 "write tests" → 它消失
        let out = t.execute(&call(todos(&[("review", "completed")]))).await;
        assert!(out.success);
        assert_eq!(
            out.output,
            "Updated todo list: 0 pending, 0 in progress, 1 completed."
        );
    }

    #[tokio::test]
    async fn rejects_invalid_lists() {
        let mut t = tool();
        // 空 content / 纯空白
        let bad = t.execute(&call(todos(&[("  ", "pending")]))).await;
        assert!(!bad.success);
        // 重复 content
        let bad = t
            .execute(&call(todos(&[("dup", "pending"), ("dup", "pending")])))
            .await;
        assert!(!bad.success);
        // 坏状态枚举
        let bad = t.execute(&call(todos(&[("x", "done")]))).await;
        assert!(!bad.success);
        // 缺 todos 数组
        let bad = t.execute(&call(json!({ "action": "add" }))).await;
        assert!(!bad.success);
    }

    #[tokio::test]
    async fn mutations_buffer_state_events() {
        let mut t = tool();
        t.execute(&call(todos(&[("a", "pending")]))).await;
        t.execute(&call(todos(&[("a", "completed"), ("b", "pending")])))
            .await;
        let events = t.take_state_events();
        // 每次变更缓冲一条快照
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "todo/write");
        assert_eq!(events[0].1["todos"][0]["content"], "a");
        // 条目无 id({content, status})
        assert!(events[0].1["todos"][0].get("id").is_none());
        assert!(t.take_state_events().is_empty(), "取走即清空");
    }

    #[tokio::test]
    async fn restores_from_last_todo_write_in_log() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        {
            let mut l = log.lock().unwrap();
            l.append(EventEnvelope::new(
                "todo/write",
                0,
                json!({ "todos": [ { "content": "survive", "status": "in_progress" } ] }),
            ))
            .unwrap();
            l.append(EventEnvelope::new(
                "todo/write",
                0,
                json!({ "todos": [ { "content": "latest", "status": "pending" } ] }),
            ))
            .unwrap();
        }
        let mut t = TodoWriteTool::new(Arc::clone(&log));
        let out = t.execute(&call(todos(&[("latest", "completed")]))).await;
        // 最近一条快照生效(整表替换语义,无需折叠):恢复到 latest 后整表覆写
        assert!(out.success);
        let events = t.take_state_events();
        assert_eq!(
            events.last().unwrap().1["todos"].as_array().unwrap().len(),
            1
        );
        assert_eq!(events.last().unwrap().1["todos"][0]["status"], "completed");
    }
}
