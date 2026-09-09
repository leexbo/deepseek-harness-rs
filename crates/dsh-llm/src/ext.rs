//! chat 方言差异点([`ChatExt`])——「通用层 + 差异点」分层的声明面。
//!
//! 通用 chat 引擎([`crate::chat::GenericChatAdapter`])统一请求体骨架、
//! 消息翻译与流解析;方言差异收敛在此:名字 / 端点 / usage 流选项等
//! **声明式常量**,与发送前最后一刻的 wire 改写钩子。新增兼容 provider =
//! 实现(或复用)一个 ext,通用层不动——`stream_options.include_usage`
//! 漏发这类事故(usage 全缺)在结构上不再可能:
//! 字段由常量声明、通用层统一执行,不散落各方言的手写 body 里。

use serde_json::{Value, json};

use dsh_agent_loop::RequestHeader;

/// OpenAI Chat Completions 兼容系的方言差异点。
pub trait ChatExt: Send + Sync {
    /// 方言名(配置/CLI 的 `dialect` 字段值)
    const NAME: &'static str;
    /// 请求端点(相对 base_url,不含前导 `/`)
    const ENDPOINT: &'static str = "chat/completions";
    /// 流式请求携带 `stream_options.include_usage`(服务端在流末帧
    /// 附带 usage;缺它 usage 全缺)
    const STREAM_INCLUDE_USAGE: bool = true;

    /// 序列化后、发送前的 wire 改写钩子:`header` 里尚未进 body 的请求面
    /// (如 `reasoning_effort`)在此注入方言私有字段。默认无改写。
    fn finalize_request_body(&self, _body: &mut Value, _header: &RequestHeader) {}
}

/// DeepSeek chat 方言(V4 系思考控制)。
///
/// 思考控制 llm-deepseek serialize wire 形态:`thinking` 与 `reasoning_effort`
/// 是**两个顶层字段**——嵌进 thinking 对象不合服务端 schema、被丢弃,
/// 模型不思考。
#[derive(Debug, Default)]
pub struct DeepSeekChatExt;

impl ChatExt for DeepSeekChatExt {
    const NAME: &'static str = "deepseek-chat";

    fn finalize_request_body(&self, body: &mut Value, header: &RequestHeader) {
        if let Some(effort) = &header.reasoning_effort {
            body["thinking"] = json!({ "type": "enabled" });
            body["reasoning_effort"] = json!(effort);
        }
    }
}

/// OpenAI 兼容净版 chat 方言(无私有字段)。
#[derive(Debug, Default)]
pub struct OpenAiChatExt;

impl ChatExt for OpenAiChatExt {
    const NAME: &'static str = "openai-chat";
}

/// Responses API 的方言差异点(通用引擎见 [`crate::responses`])。
pub trait ResponsesExt: Send + Sync {
    /// 方言名(配置/CLI 的 `dialect` 字段值)
    const NAME: &'static str;
    /// 请求端点(相对 base_url,不含前导 `/`)
    const ENDPOINT: &'static str = "responses";

    /// `reasoning_effort` 的 wire 表达:返回 (顶层键名, 值) 合并进请求体;
    /// `None` = 该方言不发思考强度。
    ///
    /// DeepSeek 与 OpenAI 的 Responses 形态同形:`{"reasoning": {"effort":
    /// …}}`(DeepSeek 官方文档 2026-09-2 核实:"effort supported;
    /// summary accepted but no summary is generated")。仍保留为钩子:
    /// 差异点的声明位置在,后续方言不因通用层改写而被动漂移。
    fn effort_wire(&self, effort: &str) -> Option<(String, Value)> {
        Some(("reasoning".into(), json!({ "effort": effort })))
    }
}

/// DeepSeek Responses 方言(未配置 dialect 时的默认;事件流无 `[DONE]`,usage 挂
/// `response.completed`——见 [`crate::responses`] 映射器说明)。
#[derive(Debug, Default)]
pub struct DeepSeekResponsesExt;

impl ResponsesExt for DeepSeekResponsesExt {
    const NAME: &'static str = "deepseek-responses";
}

/// OpenAI Responses 方言。
#[derive(Debug, Default)]
pub struct OpenAiResponsesExt;

impl ResponsesExt for OpenAiResponsesExt {
    const NAME: &'static str = "openai-responses";
}

/// Anthropic Messages API 的方言差异点(通用引擎见 [`crate::anthropic`])。
pub trait AnthropicExt: Send + Sync {
    /// 方言名(配置/CLI 的 `dialect` 字段值)
    const NAME: &'static str;
    /// 请求端点(相对 base_url,不含前导 `/`)
    const ENDPOINT: &'static str = "v1/messages";

    /// `max_tokens` 必填值(Anthropic 无默认;可按 model 细分)
    fn max_tokens(&self, _model: &str) -> u32 {
        4096
    }

    /// 鉴权头(官方 API:x-api-key + version;Bedrock/Vertex 等兼容面覆写)
    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        vec![
            ("x-api-key".into(), api_key.to_string()),
            ("anthropic-version".into(), "2023-06-01".into()),
        ]
    }
}

/// Anthropic 官方 Messages API 方言。
#[derive(Debug, Default)]
pub struct StandardAnthropicExt;

impl AnthropicExt for StandardAnthropicExt {
    const NAME: &'static str = "anthropic";
}
