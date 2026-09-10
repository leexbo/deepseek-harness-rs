//! Provider 方言适配器(`register-adapter` 接缝的 native 形态)。
//!
//! 宿主只提供 transport 原语(连接池/HTTP/SSE 帧提取,见 [`crate::http`]);
//! **方言差异**(请求体形状 / 鉴权头 / 流事件语义)收口在
//! [`ProviderAdapter`],每个方言一个实现:
//! - [`crate::chat::GenericChatAdapter`]:`POST {base}/chat/completions`
//!   (OpenAI 兼容系;差异点经 [`crate::ext::ChatExt`] 声明——
//!   `deepseek-chat` / `openai-chat`)
//! - [`crate::anthropic::GenericAnthropicAdapter`]:`POST {base}/v1/messages`
//!   (Claude Messages API;差异点经 [`crate::ext::AnthropicExt`] 声明)
//! - [`crate::responses::GenericResponsesAdapter`]:`POST {base}/responses`
//!   (Responses API;差异点经 [`crate::ext::ResponsesExt`] 声明——
//!   `deepseek-responses` / `openai-responses`)
//!
//! 内部消息方言(日志派生:扁平 tool_calls、`{output, call, id}` tool 消息)
//! 在 `build_request` 处翻译为各 wire 方言——**日志与不变式比对始终用
//! 内部形状**(「模型可见 ⟺ 已记录」的比对面不随 provider 变化)。
//!
//! 流事件映射在各 [`FrameMapper`] 实现:同样的帧序列必然产出同样的
//! 事件序列(重放确定性)。新增方言 = 新增 adapter(或 chat/responses 系
//! 新增 ext)+ 契约测试,不改宿主传输层。

use serde_json::Value;

use dsh_agent_loop::RequestHeader;

use crate::attachments::AttachmentSource;
use crate::chat::GenericChatAdapter;
use crate::ext::{DeepSeekChatExt, OpenAiChatExt};
use crate::streaming::FrameMapper;

/// provider 方言适配器(连接语义之外的全部方言差异)
pub trait ProviderAdapter: Send + Sync {
    /// 方言名(配置/CLI 选择用)
    fn name(&self) -> &'static str;
    /// 请求端点(相对 base_url,不含前导 `/`)
    fn endpoint(&self) -> &'static str;
    /// 鉴权头(OpenAI 系 Bearer;Anthropic x-api-key + version)
    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)>;
    /// 构造 wire 请求体(内部方言 header/messages → provider 方言;
    /// `images` = 附件字节来源,user 面图片块在此时组 data URL)
    fn build_request(
        &self,
        header: &RequestHeader,
        messages: &Value,
        images: &dyn AttachmentSource,
    ) -> Value;
    /// 新建流映射器(每请求一个;可带跨帧累积状态)
    fn mapper(&self) -> Box<dyn FrameMapper>;
}

// ============================================================
// 方言注册表(配置名 → adapter)
// ============================================================

/// 按方言名构造 adapter(配置/CLI 的 `dialect` 字段入口)。
///
/// 未知名字返回 None(调用方 fail-fast,不静默回退默认方言)。
pub fn adapter_by_name(name: &str) -> Option<Box<dyn ProviderAdapter>> {
    match name {
        "deepseek-responses" => Some(Box::new(crate::responses::GenericResponsesAdapter::new(
            crate::ext::DeepSeekResponsesExt,
        ))),
        "openai-responses" => Some(Box::new(crate::responses::GenericResponsesAdapter::new(
            crate::ext::OpenAiResponsesExt,
        ))),
        "deepseek-chat" => Some(Box::new(GenericChatAdapter::new(DeepSeekChatExt))),
        "openai-chat" => Some(Box::new(GenericChatAdapter::new(OpenAiChatExt))),
        "anthropic" => Some(Box::new(crate::anthropic::GenericAnthropicAdapter::new(
            crate::ext::StandardAnthropicExt,
        ))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachments::NoAttachments;
    use dsh_agent_loop::RequestHeader;

    /// llm-deepseek serialize 的 wire 锁:`thinking` 与
    /// `reasoning_effort` 是**两个顶层字段**,`stream_options.include_usage`
    /// 必发。reasoning_effort 若嵌进 thinking 对象则不合服务端 schema、
    /// 被丢弃 → 模型不思考;缺 include_usage → 流末帧无 usage,
    /// token 用量全缺(request-done 只剩 ttftMs)。
    #[test]
    fn deepseek_chat_wire_thinking_and_usage() {
        let header = RequestHeader {
            model: "deepseek-v4-flash-vision-exp".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: Some("max".into()),
            tools: vec![],
        };
        let body = GenericChatAdapter::new(DeepSeekChatExt).build_request(
            &header,
            &serde_json::json!([{ "role": "user", "content": "hi" }]),
            &NoAttachments,
        );
        assert_eq!(body["thinking"], serde_json::json!({ "type": "enabled" }));
        assert_eq!(body["reasoning_effort"], serde_json::json!("max"));
        assert_eq!(
            body["thinking"]["reasoning_effort"],
            serde_json::Value::Null,
            "reasoning_effort 不得嵌进 thinking 对象"
        );
        assert_eq!(
            body["stream_options"]["include_usage"],
            serde_json::json!(true)
        );
    }

    /// 净版 chat 方言(剥 DeepSeek 私参):即使请求面带 reasoning_effort
    /// 也不得发方言私有字段;usage 流选项是通用层职责,照发。
    #[test]
    fn openai_chat_plain_strips_private_fields() {
        let header = RequestHeader {
            model: "gpt-test".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: Some("max".into()),
            tools: vec![],
        };
        let body = GenericChatAdapter::new(OpenAiChatExt).build_request(
            &header,
            &serde_json::json!([{ "role": "user", "content": "hi" }]),
            &NoAttachments,
        );
        assert_eq!(body["thinking"], serde_json::Value::Null);
        assert_eq!(body["reasoning_effort"], serde_json::Value::Null);
        assert_eq!(
            body["stream_options"]["include_usage"],
            serde_json::json!(true)
        );
    }
}
