//! 上下文构成启发式折叠(UI ContextMeter 分段条/图例的数据源)。
//!
//! 价格语言:
//! 固定密度 4 字符/令牌,内容块每块 +4 结构开销,每条消息 +4 角色开销。
//! 纯函数:输入 = 日志事件切片,输出 = 三段构成;直播与冷会话重放同源,
//! 面板数字不依赖会话存活。

use serde_json::Value;

use dsh_session::EventEnvelope;

/// 固定文本密度(estimate.ts `CHARS_PER_TOKEN`)
const CHARS_PER_TOKEN: u64 = 4;
/// 每块结构开销(`BLOCK_OVERHEAD`)
const BLOCK_OVERHEAD: u64 = 4;
/// 每条消息角色开销(`ROLE_OVERHEAD`)
const ROLE_OVERHEAD: u64 = 4;

/// 上下文构成三段(`contextBreakdown` 投影同形)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBreakdown {
    /// 最新请求信封的 system prompt 启发式价格
    pub system_tokens: u64,
    /// 最新请求信封的工具 schema 启发式价格
    pub tools_tokens: u64,
    /// 模型可见消息(对话/思考/工具结果/折叠摘要)累积启发式价格
    pub message_tokens: u64,
}

fn ceil_div(n: u64, d: u64) -> u64 {
    n.div_ceil(d)
}

/// 单条模型可见消息的价格:内容块(文本/工具调用) + 角色开销
fn message_tokens(data: &Value) -> u64 {
    let mut tokens = ROLE_OVERHEAD;
    if let Some(text) = data["content"].as_str() {
        tokens += ceil_div(text.chars().count() as u64, CHARS_PER_TOKEN) + BLOCK_OVERHEAD;
    }
    if let Some(calls) = data["tool_calls"].as_array() {
        for call in calls {
            let name = call["name"].as_str().unwrap_or_default();
            let args = serde_json::to_string(&call["arguments"]).unwrap_or_default();
            tokens += ceil_div(name.chars().count() as u64, CHARS_PER_TOKEN)
                + ceil_div(args.chars().count() as u64, CHARS_PER_TOKEN)
                + BLOCK_OVERHEAD;
        }
    }
    tokens
}

/// 从日志计算上下文构成。
///
/// - system/tools 取**最新** audit llm "request" 记录(引擎出网前落档的
///   字符长度;重放可得,与直播一致——「最新请求信封」语义)
/// - message 按事件类型折叠 user/message、assistant/message、思考、
///   工具结果与折叠摘要。折叠后历史不扣减(折叠预算 4M 字符下
///   触发极稀,启发式差量可接受)
pub fn context_breakdown<'a>(
    events: impl IntoIterator<Item = &'a EventEnvelope>,
) -> ContextBreakdown {
    let mut system_tokens = 0u64;
    let mut tools_tokens = 0u64;
    let mut message = 0u64;
    for ev in events {
        match ev.r#type.as_str() {
            "audit/call"
                if ev.data["boundary"].as_str() == Some("llm")
                    && ev.data["operation"].as_str() == Some("request") =>
            {
                if let Some(chars) = ev.data["detail"]["systemChars"].as_u64() {
                    system_tokens = ceil_div(chars, CHARS_PER_TOKEN) + ROLE_OVERHEAD;
                }
                if let Some(chars) = ev.data["detail"]["toolsChars"].as_u64() {
                    tools_tokens = ceil_div(chars, CHARS_PER_TOKEN) + BLOCK_OVERHEAD;
                }
            }
            "user/message" | "assistant/message" => {
                // 注入上下文(user/message + source.kind != "user")不计入
                // message_tokens:注入是旁车上下文,不占历史消息数;
                // 真实用户/助手消息照算。
                if ev.r#type == "user/message"
                    && ev.data["source"]["kind"].as_str().unwrap_or("user") != "user"
                {
                    continue;
                }
                message += message_tokens(&ev.data)
            }
            "assistant/reasoning" => {
                if let Some(text) = ev.data["text"].as_str() {
                    message += ceil_div(text.chars().count() as u64, CHARS_PER_TOKEN)
                        + BLOCK_OVERHEAD
                        + ROLE_OVERHEAD;
                }
            }
            "tool/result" => {
                if let Some(text) = ev.data["output"].as_str() {
                    message += ceil_div(text.chars().count() as u64, CHARS_PER_TOKEN)
                        + BLOCK_OVERHEAD
                        + ROLE_OVERHEAD;
                }
            }
            "compaction/summary" => {
                if let Some(text) = ev.data["summary"].as_str() {
                    message += ceil_div(text.chars().count() as u64, CHARS_PER_TOKEN)
                        + BLOCK_OVERHEAD
                        + ROLE_OVERHEAD;
                }
            }
            _ => {}
        }
    }
    ContextBreakdown {
        system_tokens,
        tools_tokens,
        message_tokens: message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(r#type: &str, data: Value) -> EventEnvelope {
        EventEnvelope::new(r#type, 0, data)
    }

    fn audit_llm_request(system_chars: u64, tools_chars: u64) -> EventEnvelope {
        ev(
            "audit/call",
            json!({
                "boundary": "llm",
                "operation": "request",
                "detail": { "systemChars": system_chars, "toolsChars": tools_chars },
            }),
        )
    }

    #[test]
    fn empty_log_yields_zero_breakdown() {
        let b = context_breakdown(&[]);
        assert_eq!(
            b,
            ContextBreakdown {
                system_tokens: 0,
                tools_tokens: 0,
                message_tokens: 0
            }
        );
    }

    #[test]
    fn header_chars_priced_at_4_chars_per_token() {
        let log = vec![
            ev("turn/start", json!({})),
            audit_llm_request(100, 200),
            ev("user/message", json!({ "content": "hello" })),
        ];
        // system = ceil(100/4) + 4(角色)= 29;tools = ceil(200/4) + 4(块)= 54
        let b = context_breakdown(&log);
        assert_eq!(b.system_tokens, 29);
        assert_eq!(b.tools_tokens, 54);
        // "hello" = ceil(5/4) + 4(块) + 4(角色)= 10
        assert_eq!(b.message_tokens, 10);
    }

    #[test]
    fn latest_request_envelope_wins() {
        let log = vec![audit_llm_request(100, 0), audit_llm_request(400, 100)];
        let b = context_breakdown(&log);
        assert_eq!(b.system_tokens, ceil_div(400, 4) + ROLE_OVERHEAD);
        assert_eq!(b.tools_tokens, ceil_div(100, 4) + BLOCK_OVERHEAD);
    }

    #[test]
    fn messages_fold_with_tool_calls_and_other_surface_types() {
        let log = vec![
            ev("user/message", json!({ "content": "abcd" })),
            ev(
                "assistant/message",
                json!({
                    "content": "efgh",
                    "tool_calls": [{
                        "name": "bash",
                        "arguments": { "command": "ls" },
                    }],
                }),
            ),
            ev("assistant/reasoning", json!({ "text": "ijkl" })),
            ev("tool/result", json!({ "output": "mnop" })),
            ev("compaction/summary", json!({ "summary": "qrst" })),
        ];
        let b = context_breakdown(&log);
        // user "abcd" = 1 + 4 + 4 = 9
        // assistant 内容 "efgh" = 9;工具调用 name "bash" = 1、
        //   args `{"command":"ls"}` 15 字符 = ceil(15/4)=4 + 块4 → 1+4+4 = 9
        //   → assistant 共 18
        // reasoning / tool/result / summary 各 9
        assert_eq!(b.message_tokens, 9 + 18 + 9 + 9 + 9);
    }

    #[test]
    fn non_request_audit_records_ignored() {
        let log = vec![ev(
            "audit/call",
            json!({ "boundary": "tool", "operation": "bash", "detail": {} }),
        )];
        let b = context_breakdown(&log);
        assert_eq!(b.system_tokens, 0);
        assert_eq!(b.tools_tokens, 0);
    }
}
