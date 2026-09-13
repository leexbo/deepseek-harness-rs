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

    /// 200 + 非 SSE 的 JSON 错误体归类(端点以 200 应答错误的坏习惯
    /// 面;None = 非错误体,交由引擎按空响应处理)。默认 = OpenAI 兼容
    /// 系嵌套形态 `{"error":{…}}`
    fn body_error(&self, body: &str) -> Option<dsh_agent_loop::TransportError> {
        openai_style_body_error(body)
    }
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
    const NAME: &'static str = "openai-completions";
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

    /// 200 + 非 SSE 的 JSON 错误体归类(None = 非错误体,交由引擎按
    /// 空响应处理)。默认 = OpenAI 兼容系嵌套形态 `{"error":{…}}`
    fn body_error(&self, body: &str) -> Option<dsh_agent_loop::TransportError> {
        openai_style_body_error(body)
    }

    /// 方言是否接受 `input_image` 输入(用户消息图片 → data-URL part)。
    /// 默认 true(OpenAI Responses 规范);纯文本模型覆盖为 false →
    /// 图片回落占位文本降级
    fn supports_images(&self) -> bool {
        true
    }
}

/// DeepSeek Responses 方言(未配置 dialect 时的默认;事件流无 `[DONE]`,usage 挂
/// `response.completed`——见 [`crate::responses`] 映射器说明)。
#[derive(Debug, Default)]
pub struct DeepSeekResponsesExt;

impl ResponsesExt for DeepSeekResponsesExt {
    const NAME: &'static str = "deepseek-responses";

    /// DeepSeek 系为纯文本模型(官方无图片输入面)——图片降级占位文本
    fn supports_images(&self) -> bool {
        false
    }
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
    const NAME: &'static str = "anthropic-messages";
}

// ============================================================
// 200 + JSON 错误体归类(端点以 200 应答错误的坏习惯面)
// ============================================================

/// 请求体归类的错误钩子共享实现:OpenAI 兼容系嵌套形态
/// `{"error":{"code":…,"message":…}}`(chat/responses 家族契约;
/// DeepSeek 等同形)。code 落 HTTP 范围(400-599)→ 按状态归类
/// (401/403 → AUTH);平台自有码(如 1002)→ Other 带原始消息。
/// 识别不出错误结构 → None,交由引擎按空响应处理
fn openai_style_body_error(body: &str) -> Option<dsh_agent_loop::TransportError> {
    use crate::http::classify_status;

    let v: Value = serde_json::from_str(body).ok()?;
    let error = v["error"].as_object()?;
    let code = error["code"]
        .as_u64()
        .or_else(|| error["code"].as_str()?.parse::<u64>().ok());
    let message = error["message"].as_str().unwrap_or_default();
    match code {
        Some(code) => {
            let status = u16::try_from(code)
                .ok()
                .and_then(|c| reqwest::StatusCode::from_u16(c).ok())
                .filter(|s| (400..=599).contains(&s.as_u16()));
            Some(match status {
                Some(status) => classify_status(status, None, body.to_string()),
                None => dsh_agent_loop::TransportError::Other(format!(
                    "provider 错误 {code}: {message}"
                )),
            })
        }
        None => (!message.is_empty())
            .then(|| dsh_agent_loop::TransportError::Other(format!("provider 错误:{message}"))),
    }
}

/// GLM(智谱 bigmodel)Responses 方言。
///
/// 与 OpenAI Responses 请求面同形;差异在错误面——端点对坏 key 等
/// 以 **200 + application/json 顶层错误体**应答(2026-09-12 实测):
/// `{"code":401,"msg":"令牌已过期或验证不正确","success":false}`。
/// 不识别即落「空响应」被重试五次,认证错误必须经此归类为首轮即止。
#[derive(Debug, Default)]
pub struct GlmResponsesExt;

impl ResponsesExt for GlmResponsesExt {
    const NAME: &'static str = "glm-responses";

    fn body_error(&self, body: &str) -> Option<dsh_agent_loop::TransportError> {
        use crate::http::classify_status;

        let v: Value = serde_json::from_str(body).ok()?;
        let code = v["code"]
            .as_u64()
            .or_else(|| v["code"].as_str()?.parse::<u64>().ok())?;
        let msg = v["msg"]
            .as_str()
            .or(v["message"].as_str())
            .unwrap_or_default();
        let status = u16::try_from(code)
            .ok()
            .and_then(|c| reqwest::StatusCode::from_u16(c).ok())
            .filter(|s| (400..=599).contains(&s.as_u16()));
        Some(match status {
            Some(status) => classify_status(status, None, body.to_string()),
            None => dsh_agent_loop::TransportError::Other(format!("provider 错误 {code}: {msg}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent_loop::TransportError;

    /// GLM(bigmodel)错误面 wire 锁(2026-09-12 实测取证):坏 key 时
    /// 端点以 200 + application/json 顶层错误体应答,401 → AUTH
    /// (不可重试——此前落「空响应」被重试五次)
    #[test]
    fn glm_body_error_top_level_shape_is_auth() {
        let err = GlmResponsesExt
            .body_error(r#"{"code":401,"msg":"令牌已过期或验证不正确","success":false}"#)
            .expect("顶层错误体应归类");
        match err {
            TransportError::Auth { status, .. } => assert_eq!(status, 401),
            other => panic!("应归类 AUTH,实得 {other:?}"),
        }
        // 平台自有码(非 HTTP 范围)→ Other 带消息,一律不可重试
        let err = GlmResponsesExt
            .body_error(r#"{"code":1002,"msg":"鉴权失败","success":false}"#)
            .expect("平台码应归类");
        assert!(matches!(err, TransportError::Other(_)));
        assert!(!err.retryable());
    }

    /// OpenAI 兼容系默认错误面:嵌套 error.code(家族契约,DeepSeek 同形)
    #[test]
    fn openai_default_body_error_nested_shape() {
        let err = OpenAiResponsesExt
            .body_error(r#"{"error":{"code":"401","message":"bad key"}}"#)
            .expect("嵌套错误体应归类");
        assert!(matches!(err, TransportError::Auth { status: 401, .. }));
        // 非错误 JSON → None(空响应既有语义)
        assert_eq!(OpenAiResponsesExt.body_error(r#"{"ok":true}"#), None);
    }
}
