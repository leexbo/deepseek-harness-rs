//! session_query 工具族(五件套):
//!
//! - `session_search`:全库检索,每会话取最强命中(首个;命中按
//!   会话/seq 升序);
//! - `session_event_search`:单会话检索;
//! - `session_trace`:会话血缘(session/forked 链:祖先 + 直接后代);
//! - `session_event_trace`:单事件溯源(sourceEventSeqs 归因链);
//! - `session_event_read`:单事件全文 + 邻居摘要。
//!
//! 宿主面经 [`SessionQueryPort`] 注入(检索是 async IO,trait 以
//! boxed future 暴露);实现方:dsh-core AppHost。

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use dsh_agent_loop::{ToolCallRequest, ToolOutput, ToolPort};

/// 检索/血缘端口(宿主注入)
pub trait SessionQueryPort: Send + Sync {
    /// 全库或单会话检索(None = 全库);返回宿主 search_sessions 的
    /// `{query, hits:[{sessionId,seq,kind,content}]}` 形状
    fn search(
        &self,
        query: &str,
        limit: usize,
        session: Option<&str>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>>;

    /// 会话血缘(session_trace 形状)
    fn trace_session(
        &self,
        session: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>>;

    /// 单事件溯源(event_trace 形状)
    fn trace_event(
        &self,
        session: &str,
        seq: u64,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>>;

    /// 单事件全文 + 邻居(event_read 形状)
    fn read_event(
        &self,
        session: &str,
        seq: u64,
        before: usize,
        after: usize,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>>;
}

/// session_query 工具族
pub struct SessionQueryTool {
    port: Arc<dyn SessionQueryPort>,
    /// 当前会话 id(检索默认作用域标记)
    current: String,
}

impl SessionQueryTool {
    /// 构造(port = 宿主检索面;current = 归属会话 id)
    pub fn new(port: Arc<dyn SessionQueryPort>, current: &str) -> Self {
        Self {
            port,
            current: current.to_string(),
        }
    }
}

/// 五个工具的 spec(未实装的过滤面不加——显式精简)
fn specs() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "session_search",
                "description": "Search prior sessions in the caller workspace and return the strongest matching event from each session.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Full-text query (substring semantics; multiple terms are OR)." },
                        "limit": { "type": "integer", "description": "Max hits (default 20)." }
                    },
                    "required": ["query"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "session_event_search",
                "description": "Search prior events in one session.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session": { "type": "string", "description": "Target session id." },
                        "query": { "type": "string", "description": "Full-text query." },
                        "limit": { "type": "integer", "description": "Max hits (default 20)." }
                    },
                    "required": ["session", "query"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "session_trace",
                "description": "Read the session lineage around one session, including complete ancestor and descendant relationships.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session": { "type": "string", "description": "Target session id." }
                    },
                    "required": ["session"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "session_event_trace",
                "description": "Read the source attribution chain for one event in a session.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session": { "type": "string", "description": "Target session id." },
                        "seq": { "type": "integer", "description": "Target event sequence number." }
                    },
                    "required": ["session", "seq"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "session_event_read",
                "description": "Read one full unabridged event and optional neighboring event summaries from a session.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session": { "type": "string", "description": "Target session id." },
                        "seq": { "type": "integer", "description": "Target event sequence number." },
                        "before": { "type": "integer", "description": "Number of preceding events to summarize. Omit for none." },
                        "after": { "type": "integer", "description": "Number of following events to summarize. Omit for none." }
                    },
                    "required": ["session", "seq"]
                }
            }
        }),
    ]
}

/// session_search 呈现:每会话最强命中(首个)+ 命中计数
fn format_session_search(out: &Value, current: &str) -> String {
    let Some(hits) = out["hits"].as_array() else {
        return "(no hits)".into();
    };
    // 每会话取首个(升序 = 最强),同时统计该会话命中数
    let mut best: BTreeMap<&str, (&Value, usize)> = BTreeMap::new();
    for h in hits {
        let sid = h["sessionId"].as_str().unwrap_or_default();
        if sid == current {
            continue; // 排除调用会话自身
        }
        let e = best.entry(sid).or_insert((h, 0));
        e.1 += 1;
    }
    if best.is_empty() {
        return "(no prior-session hits)".into();
    }
    let mut lines = vec![];
    for (sid, (h, n)) in best {
        let content = h["content"].as_str().unwrap_or_default();
        let preview: String = content.chars().take(120).collect();
        lines.push(format!("{sid} (seq {}, {} hits): {preview}", h["seq"], n));
    }
    lines.join("\n")
}

impl ToolPort for SessionQueryTool {
    fn specs(&self) -> Vec<Value> {
        specs()
    }

    fn execute(
        &mut self,
        call: &ToolCallRequest,
    ) -> impl std::future::Future<Output = ToolOutput> + Send {
        let port = Arc::clone(&self.port);
        let current = self.current.clone();
        let name = call.name.clone();
        let args: Value = call
            .arguments
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| call.arguments.clone());
        async move {
            let fail = |msg: String| ToolOutput {
                output: msg,
                success: false,
                ..Default::default()
            };
            let str_arg = |k: &str| args[k].as_str().map(String::from);
            let int_arg = |k: &str, d: usize| args[k].as_u64().map(|v| v as usize).unwrap_or(d);
            match name.as_str() {
                "session_search" => {
                    let Some(query) = str_arg("query") else {
                        return fail("query required".into());
                    };
                    let limit = int_arg("limit", 20).max(1);
                    match port.search(&query, limit, None).await {
                        Ok(out) => ToolOutput {
                            output: format_session_search(&out, &current),
                            success: true,
                            ..Default::default()
                        },
                        Err(e) => fail(e),
                    }
                }
                "session_event_search" => {
                    let (Some(session), Some(query)) = (str_arg("session"), str_arg("query"))
                    else {
                        return fail("session and query required".into());
                    };
                    let limit = int_arg("limit", 20).max(1);
                    match port.search(&query, limit, Some(&session)).await {
                        Ok(out) => {
                            let hits = out["hits"].as_array().cloned().unwrap_or_default();
                            let lines: Vec<String> = hits
                                .iter()
                                .map(|h| {
                                    let preview: String = h["content"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .chars()
                                        .take(160)
                                        .collect();
                                    format!("seq {} [{}]: {preview}", h["seq"], h["kind"])
                                })
                                .collect();
                            ToolOutput {
                                output: if lines.is_empty() {
                                    "(no hits)".into()
                                } else {
                                    lines.join("\n")
                                },
                                success: true,
                                ..Default::default()
                            }
                        }
                        Err(e) => fail(e),
                    }
                }
                "session_trace" => {
                    let Some(session) = str_arg("session") else {
                        return fail("session required".into());
                    };
                    match port.trace_session(&session).await {
                        Ok(v) => ToolOutput {
                            output: serde_json::to_string_pretty(&v).unwrap_or_default(),
                            success: true,
                            ..Default::default()
                        },
                        Err(e) => fail(e),
                    }
                }
                "session_event_trace" => {
                    let (Some(session), Some(seq)) = (str_arg("session"), args["seq"].as_u64())
                    else {
                        return fail("session and seq required".into());
                    };
                    match port.trace_event(&session, seq).await {
                        Ok(v) => ToolOutput {
                            output: serde_json::to_string_pretty(&v).unwrap_or_default(),
                            success: true,
                            ..Default::default()
                        },
                        Err(e) => fail(e),
                    }
                }
                "session_event_read" => {
                    let (Some(session), Some(seq)) = (str_arg("session"), args["seq"].as_u64())
                    else {
                        return fail("session and seq required".into());
                    };
                    let (before, after) = (int_arg("before", 0), int_arg("after", 0));
                    match port.read_event(&session, seq, before, after).await {
                        Ok(v) => ToolOutput {
                            output: serde_json::to_string_pretty(&v).unwrap_or_default(),
                            success: true,
                            ..Default::default()
                        },
                        Err(e) => fail(e),
                    }
                }
                other => fail(format!("unknown tool {other}")),
            }
        }
    }
}

// —— 测试端口(内存假实现)——

/// 内存检索端口(测试/演示)
pub struct InMemoryQueryPort {
    /// search_sessions 形状的检索结果
    pub search_result: Value,
}

impl SessionQueryPort for InMemoryQueryPort {
    fn search(
        &self,
        _query: &str,
        _limit: usize,
        _session: Option<&str>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>> {
        let out = self.search_result.clone();
        Box::pin(async move { Ok(out) })
    }

    fn trace_session(
        &self,
        _session: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>> {
        Box::pin(async move { Ok(json!({ "session": "s", "ancestors": [], "children": [] })) })
    }

    fn trace_event(
        &self,
        _session: &str,
        _seq: u64,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>> {
        Box::pin(async move { Ok(json!({ "sourceEventSeqs": [] })) })
    }

    fn read_event(
        &self,
        _session: &str,
        _seq: u64,
        _before: usize,
        _after: usize,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>> {
        Box::pin(async move { Ok(json!({ "event": {} })) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: Value) -> ToolCallRequest {
        ToolCallRequest {
            name: name.into(),
            arguments: args,
        }
    }

    /// 五工具声明齐 + session_search 每会话最强命中/排除自身
    #[tokio::test]
    async fn five_specs_and_session_search_shape() {
        let mut tool = SessionQueryTool::new(
            Arc::new(InMemoryQueryPort {
                search_result: json!({
                    "hits": [
                        { "sessionId": "ws/a", "seq": 1, "kind": "user", "content": "甲会话命中一" },
                        { "sessionId": "ws/a", "seq": 5, "kind": "assistant", "content": "甲会话命中二" },
                        { "sessionId": "ws/b", "seq": 2, "kind": "user", "content": "乙会话命中" },
                        { "sessionId": "ws/cur", "seq": 3, "kind": "user", "content": "当前会话(应排除)" }
                    ]
                }),
            }),
            "ws/cur",
        );
        assert_eq!(tool.specs().len(), 5, "五件套");

        let out = tool
            .execute(&call("session_search", json!({ "query": "命中" })))
            .await;
        assert!(out.success);
        assert!(
            out.output.contains("ws/a (seq 1, 2 hits)"),
            "{}",
            out.output
        );
        assert!(out.output.contains("ws/b (seq 2, 1 hits)"));
        assert!(!out.output.contains("ws/cur"), "调用会话自身应排除");
    }

    /// 参数缺失 → 失败输出
    #[tokio::test]
    async fn missing_args_fail() {
        let mut tool = SessionQueryTool::new(
            Arc::new(InMemoryQueryPort {
                search_result: json!({ "hits": [] }),
            }),
            "ws/x",
        );
        let out = tool.execute(&call("session_search", json!({}))).await;
        assert!(!out.success);
    }
}
