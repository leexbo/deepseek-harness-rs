//! 通用 OpenAI Chat Completions 引擎:兼容 provider 共有的部分只写一次
//! ——body 骨架(model/temperature/stream/messages + usage 流选项)、
//! 内部消息方言 → wire 翻译(assistant.tool_calls 展开、tool 消息归位、
//! user 图片 → data URL)、流事件解析([`OpenAiChatMapper`]);
//! 方言差异经 [`ChatExt`] 声明(常量 + `finalize_request_body` 钩子)。

use serde_json::{Value, json};

use dsh_agent_loop::RequestHeader;

use crate::adapters::ProviderAdapter;
use crate::attachments::{AttachmentSource, OFFLOADED_IMAGE_TEXT, image_data_url};
use crate::ext::ChatExt;
use crate::streaming::{FrameMapper, OpenAiChatMapper};

/// 通用 chat 方言适配器:`E` 声明差异,本实现声明共性。
#[derive(Debug, Default)]
pub struct GenericChatAdapter<E: ChatExt> {
    ext: E,
}

impl<E: ChatExt> GenericChatAdapter<E> {
    /// 以方言差异点构建
    pub fn new(ext: E) -> Self {
        Self { ext }
    }
}

impl<E: ChatExt> ProviderAdapter for GenericChatAdapter<E> {
    fn name(&self) -> &'static str {
        E::NAME
    }

    fn endpoint(&self) -> &'static str {
        E::ENDPOINT
    }

    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        vec![("Authorization".into(), format!("Bearer {api_key}"))]
    }

    fn build_request(
        &self,
        header: &RequestHeader,
        messages: &Value,
        images: &dyn AttachmentSource,
    ) -> Value {
        let mut full_messages: Vec<Value> = Vec::new();
        if !header.system.is_empty() {
            full_messages.push(json!({
                "role": "system",
                "content": header.system,
            }));
        }
        if let Some(items) = messages.as_array() {
            full_messages.extend(items.iter().cloned());
        }
        let full_messages: Vec<Value> = full_messages
            .into_iter()
            .map(|m| to_chat_wire(m, images))
            .collect();
        let mut body = json!({
            "model": header.model,
            "temperature": header.temperature,
            "stream": true,
            "messages": full_messages,
        });
        if E::STREAM_INCLUDE_USAGE {
            // 流末帧带 usage(llm-deepseek serialize;缺它 usage 全缺)
            body["stream_options"] = json!({ "include_usage": true });
        }
        if !header.tools.is_empty() {
            body["tools"] = json!(header.tools);
            body["tool_choice"] = json!("auto");
        }
        self.ext.finalize_request_body(&mut body, header);
        body
    }

    fn mapper(&self) -> Box<dyn FrameMapper> {
        Box::new(OpenAiChatMapper::new())
    }
}

/// 内部消息方言 → OpenAI Chat wire 方言:
/// - assistant.tool_calls[].{name,arguments} → {id, type, function.{name,arguments}}
/// - tool 消息 {output, call/id} → {content, tool_call_id}
/// - user 块数组内容(图片消息)→ parts:图片块经字节来源组
///   `image_url` data URL(缺席降级占位文本);纯文本 user 保持紧凑 string
fn to_chat_wire(message: Value, images: &dyn AttachmentSource) -> Value {
    match message["role"].as_str() {
        Some("assistant") => {
            let mut m = message;
            if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                m["tool_calls"] = Value::Array(
                    calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.get("id").cloned()
                                    .unwrap_or(Value::String(String::new())),
                                "type": "function",
                                "function": {
                                    "name": c["name"],
                                    "arguments": c["arguments"],
                                },
                            })
                        })
                        .collect(),
                );
            }
            m
        }
        Some("tool") => {
            // 结果图片(MCP 图片桥):content 升级为块数组(text + image_url),
            // 字节缺席降级 offload 占位(offload 哨兵文本块直通)
            let mut content = message["output"].clone();
            if let Some(imgs) = message["images"].as_array().filter(|a| !a.is_empty()) {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(text) = message["output"].as_str().filter(|s| !s.is_empty()) {
                    parts.push(json!({ "type": "text", "text": text }));
                }
                for img in imgs {
                    match img["type"].as_str() {
                        Some("image") => match image_data_url(img, images) {
                            Some(url) => parts
                                .push(json!({ "type": "image_url", "image_url": { "url": url } })),
                            None => {
                                parts.push(json!({ "type": "text", "text": OFFLOADED_IMAGE_TEXT }))
                            }
                        },
                        _ => parts.push(img.clone()),
                    }
                }
                content = Value::Array(parts);
            }
            json!({
                "role": "tool",
                // provider call id 优先;无 id 时用内部引用链序号兜底(字符串化)
                "tool_call_id": message
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| message["call"].to_string()),
                "content": content,
            })
        }
        Some("user") if message["content"].is_array() => json!({
            "role": "user",
            "content": chat_user_parts(&message["content"], images),
        }),
        _ => message,
    }
}

/// user 块数组 → OpenAI parts(text 块跳过空串;图片块 → image_url,
/// 字节缺席降级 offload 占位文本)
fn chat_user_parts(content: &Value, images: &dyn AttachmentSource) -> Value {
    Value::Array(
        content
            .as_array()
            .into_iter()
            .flatten()
            .map(|block| match block["type"].as_str() {
                Some("text") => block.clone(),
                Some("image") => match image_data_url(block, images) {
                    Some(url) => json!({ "type": "image_url", "image_url": { "url": url } }),
                    None => json!({ "type": "text", "text": OFFLOADED_IMAGE_TEXT }),
                },
                _ => block.clone(),
            })
            .filter(|p| {
                p["type"].as_str() != Some("text") || !p["text"].as_str().unwrap_or("").is_empty()
            })
            .collect(),
    )
}
