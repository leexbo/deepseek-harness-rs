//! 通用 Anthropic Messages API 引擎:body 骨架(system 顶层/messages
//! blocks/tools 扁平 input_schema/max_tokens)、内部消息方言 → wire 翻译
//! (assistant 文本+tool_use blocks、tool 消息 → user 角色tool_result、
//! user 图片 → base64 source)、流事件解析([`AnthropicMapper`])只写
//! 一次;方言差异(端点/鉴权/max_tokens)经 [`AnthropicExt`] 声明。

use base64::Engine as _;
use serde_json::{Value, json};

use dsh_agent_loop::RequestHeader;

use crate::adapters::ProviderAdapter;
use crate::attachments::{AttachmentSource, OFFLOADED_IMAGE_TEXT};
use crate::ext::AnthropicExt;
use crate::streaming::{FrameMapper, StreamEvent};

/// 通用 Anthropic 方言适配器:`E` 声明差异,本实现声明共性。
#[derive(Debug, Default)]
pub struct GenericAnthropicAdapter<E: AnthropicExt> {
    ext: E,
}

impl<E: AnthropicExt> GenericAnthropicAdapter<E> {
    /// 以方言差异点构建
    pub fn new(ext: E) -> Self {
        Self { ext }
    }
}

impl<E: AnthropicExt> ProviderAdapter for GenericAnthropicAdapter<E> {
    fn name(&self) -> &'static str {
        E::NAME
    }

    fn endpoint(&self) -> &'static str {
        E::ENDPOINT
    }

    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        self.ext.auth_headers(api_key)
    }

    fn build_request(
        &self,
        header: &RequestHeader,
        messages: &Value,
        images: &dyn AttachmentSource,
    ) -> Value {
        let wire_messages: Vec<Value> = messages
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .cloned()
                    .map(|m| to_anthropic_wire(m, images))
                    .collect()
            })
            .unwrap_or_default();
        let mut body = json!({
            "model": header.model,
            "max_tokens": self.ext.max_tokens(&header.model),
            "temperature": header.temperature,
            "stream": true,
            "messages": wire_messages,
        });
        if !header.system.is_empty() {
            // Anthropic 的 system 是顶层字段,不在 messages 内
            body["system"] = json!(header.system);
        }
        if !header.tools.is_empty() {
            // 形状差异:扁平 {name, description, input_schema},
            // 从内部 OpenAI 形状(type/function 嵌套)翻译
            let tools: Vec<Value> = header
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t["function"]["name"],
                        "description": t["function"]["description"],
                        "input_schema": t["function"]["parameters"],
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }
        body
    }

    fn mapper(&self) -> Box<dyn FrameMapper> {
        Box::new(AnthropicMapper::new())
    }
}

/// 内部消息方言 → Anthropic wire 方言:
/// - user → {role, content}(字符串 content 可接受;块数组 → blocks,
///   图片块经字节来源组 base64 source,缺席降级占位文本)
/// - assistant(content + tool_calls)→ 文本 block + tool_use blocks
/// - tool 消息 → user 角色的 tool_result block
fn to_anthropic_wire(message: Value, images: &dyn AttachmentSource) -> Value {
    match message["role"].as_str() {
        Some("assistant") => {
            let mut blocks: Vec<Value> = Vec::new();
            if let Some(text) = message["content"].as_str().filter(|s| !s.is_empty()) {
                blocks.push(json!({ "type": "text", "text": text }));
            }
            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let input: Value = if let Some(s) = call["arguments"].as_str() {
                        serde_json::from_str(s).unwrap_or(json!({}))
                    } else {
                        call["arguments"].clone()
                    };
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.get("id").cloned()
                            .unwrap_or(Value::String(String::new())),
                        "name": call["name"],
                        "input": input,
                    }));
                }
            }
            json!({ "role": "assistant", "content": blocks })
        }
        Some("tool") => json!({
            "role": "user",
            "content": [ {
                "type": "tool_result",
                // provider call id 优先;无 id 时用内部引用链序号兜底(字符串化)
                "tool_use_id": message
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| message["call"].to_string()),
                "content": message["output"],
            } ],
        }),
        Some("user") if message["content"].is_array() => json!({
            "role": "user",
            "content": anthropic_user_blocks(&message["content"], images),
        }),
        _ => message,
    }
}

/// user 块数组 → Anthropic content blocks(text 直通;图片 → base64
/// source block,字节缺席降级 offload 占位文本)
fn anthropic_user_blocks(content: &Value, images: &dyn AttachmentSource) -> Value {
    Value::Array(
        content
            .as_array()
            .into_iter()
            .flatten()
            .map(|block| match block["type"].as_str() {
                Some("text") => block.clone(),
                Some("image") => {
                    let a = &block["attachment"];
                    match (
                        a["attachmentId"]
                            .as_str()
                            .and_then(|id| images.image_bytes(id)),
                        a["mediaType"].as_str(),
                    ) {
                        (Some(bytes), Some(media_type)) => json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                            },
                        }),
                        _ => json!({ "type": "text", "text": OFFLOADED_IMAGE_TEXT }),
                    }
                }
                _ => block.clone(),
            })
            .filter(|b| {
                b["type"].as_str() != Some("text") || !b["text"].as_str().unwrap_or("").is_empty()
            })
            .collect(),
    )
}

/// Anthropic Messages 流映射器。
///
/// 事件(data.type):`content_block_start`(text/tool_use 注册块)→
/// `content_block_delta`(text_delta → Chunk;input_json_delta → 累积)→
/// `content_block_stop`(tool_use 块定稿)→ `message_stop`(冲刷完整
/// AssistantMessage + Done)。`message_start/message_delta` 的 usage → Usage。
#[derive(Debug, Default)]
pub struct AnthropicMapper {
    /// 按 index 注册的内容块(text / tool_use)
    blocks: Vec<Value>,
    /// 累积的文本(冲刷时作为 content)
    content: String,
    /// 已定稿的 tool_use 调用(内部方言扁平形状)
    tool_calls: Vec<Value>,
}

impl AnthropicMapper {
    /// 构建空映射器
    pub fn new() -> Self {
        Self::default()
    }

    fn flush(&mut self) -> Vec<StreamEvent> {
        let calls = std::mem::take(&mut self.tool_calls);
        let content = std::mem::take(&mut self.content);
        vec![StreamEvent::AssistantMessage(json!({
            "content": content,
            "tool_calls": calls,
        }))]
    }
}

impl FrameMapper for AnthropicMapper {
    fn frame(&mut self, data: &str) -> Vec<StreamEvent> {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return vec![StreamEvent::Other(Value::String(data.to_string()))];
        };
        match v["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let usage = v["message"]["usage"].clone();
                if usage.as_object().is_some_and(|o| !o.is_empty()) {
                    vec![StreamEvent::Usage(usage)]
                } else {
                    Vec::new()
                }
            }
            "content_block_start" => {
                let index = v["index"].as_u64().unwrap_or(0) as usize;
                let block = v["content_block"].clone();
                while self.blocks.len() <= index {
                    self.blocks.push(Value::Null);
                }
                self.blocks[index] = block;
                Vec::new()
            }
            "content_block_delta" => {
                let index = v["index"].as_u64().unwrap_or(0) as usize;
                match v["delta"]["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let text = v["delta"]["text"].as_str().unwrap_or_default();
                        self.content.push_str(text);
                        vec![StreamEvent::Chunk(text.to_string())]
                    }
                    "input_json_delta" => {
                        // tool_use 的 input 增量:追加到对应块的累积缓冲
                        let partial = v["delta"]["partial_json"].as_str().unwrap_or_default();
                        if let Some(block) = self.blocks.get_mut(index) {
                            let args = block["arguments"].as_str().unwrap_or_default().to_string();
                            block["arguments"] = Value::String(format!("{args}{partial}"));
                        }
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_stop" => {
                let index = v["index"].as_u64().unwrap_or(0) as usize;
                if let Some(block) = self.blocks.get(index).cloned()
                    && block["type"] == "tool_use"
                {
                    // input 的增量(input_json_delta)累积在 arguments 字符串;
                    // 无增量时为 "{}" (与内部方言一致:arguments 是 JSON 字符串)
                    let arguments = block["arguments"].as_str().filter(|s| !s.is_empty());
                    self.tool_calls.push(json!({
                        "id": block["id"],
                        "name": block["name"],
                        "arguments": arguments.unwrap_or("{}"),
                    }));
                }
                Vec::new()
            }
            "message_delta" => {
                // stop_reason 与增量 usage
                if v["usage"].is_object() {
                    vec![StreamEvent::Usage(v["usage"].clone())]
                } else {
                    Vec::new()
                }
            }
            "message_stop" => {
                let mut events = self.flush();
                events.push(StreamEvent::Done);
                events
            }
            "ping" | "content_block" => Vec::new(),
            _ => vec![StreamEvent::Other(v)],
        }
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        // 兜底:provider 未发 message_stop 即断流时冲刷
        if !self.tool_calls.is_empty() || !self.content.is_empty() {
            return self.flush();
        }
        Vec::new()
    }
}
