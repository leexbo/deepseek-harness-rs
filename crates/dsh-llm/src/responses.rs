//! 通用 OpenAI Responses API 引擎:body 骨架(model/stream/input/
//! instructions/tools/temperature + 思考强度经 [`ResponsesExt`] 钩子)、
//! 内部消息方言 → input items 翻译、流事件解析([`ResponsesMapper`])
//! 只写一次;方言差异经 [`ResponsesExt`] 声明。
//!
//! 流语义(DeepSeek 官方文档核实,OpenAI Responses 同构):
//! 增量 `response.output_text.delta` / `response.reasoning_text.delta` /
//! `response.function_call_arguments.delta`;终结 `response.completed` /
//! `response.incomplete` / `response.failed` 携带完整 response 对象
//! (usage 在其中);**没有 `data: [DONE]`**(兼容网关可能发,仍兜底)。

use serde_json::{Value, json};

use dsh_agent_loop::RequestHeader;

use crate::adapters::ProviderAdapter;
use crate::attachments::{AttachmentSource, strip_images_for_summary};
use crate::ext::ResponsesExt;
use crate::streaming::{FrameMapper, StreamEvent};

/// 通用 Responses 方言适配器:`E` 声明差异,本实现声明共性。
#[derive(Debug, Default)]
pub struct GenericResponsesAdapter<E: ResponsesExt> {
    ext: E,
}

impl<E: ResponsesExt> GenericResponsesAdapter<E> {
    /// 以方言差异点构建
    pub fn new(ext: E) -> Self {
        Self { ext }
    }
}

impl<E: ResponsesExt> ProviderAdapter for GenericResponsesAdapter<E> {
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
        _images: &dyn AttachmentSource,
    ) -> Value {
        // system → instructions;消息 → input items;
        // assistant 工具调用 → function_call item;tool 消息 → function_call_output item。
        // user 块数组先经图片降级(Responses 方言无多模态翻译,图 → 占位文本)
        let degraded = strip_images_for_summary(messages);
        let mut input: Vec<Value> = Vec::new();
        if let Some(items) = degraded.as_array() {
            for message in items {
                match message["role"].as_str() {
                    Some("assistant") => {
                        let text = message["content"].as_str().unwrap_or_default();
                        if !text.is_empty() {
                            input.push(json!({
                                "role": "assistant",
                                "content": [ { "type": "output_text", "text": text } ],
                            }));
                        }
                        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                            for call in calls {
                                input.push(json!({
                                    "type": "function_call",
                                    "call_id": call.get("id").cloned()
                                        .unwrap_or(Value::String(String::new())),
                                    "name": call["name"],
                                    "arguments": call["arguments"],
                                }));
                            }
                        }
                    }
                    Some("tool") => {
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": message
                                .get("id")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map(String::from)
                                .unwrap_or_else(|| message["call"].to_string()),
                            "output": message["output"],
                        }));
                    }
                    Some("user") => {
                        // 降级后的块数组折叠回字符串(Responses 的 user 输入面)
                        let text = match message["content"] {
                            Value::Array(ref blocks) => blocks
                                .iter()
                                .filter_map(|b| b["text"].as_str())
                                .collect::<String>(),
                            _ => message["content"].as_str().unwrap_or_default().to_string(),
                        };
                        input.push(json!({ "role": "user", "content": text }));
                    }
                    _ => input.push(message.clone()),
                }
            }
        }
        let mut body = json!({
            "model": header.model,
            "temperature": header.temperature,
            "stream": true,
            "input": input,
        });
        if !header.system.is_empty() {
            // instructions 被服务端插为第一条 system 消息
            body["instructions"] = json!(header.system);
        }
        if !header.tools.is_empty() {
            // Responses 形状:扁平 {type, name, description, parameters}
            let tools: Vec<Value> = header
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t["function"]["name"],
                        "description": t["function"]["description"],
                        "parameters": t["function"]["parameters"],
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }
        if let Some(effort) = &header.reasoning_effort
            && let Some((key, value)) = self.ext.effort_wire(effort)
        {
            body[key] = value;
        }
        body
    }

    fn mapper(&self) -> Box<dyn FrameMapper> {
        Box::new(ResponsesMapper::new())
    }
}

/// Responses 流映射器。
///
/// 增量:`response.output_text.delta` → Chunk;`response.reasoning_text.delta`
/// → Reasoning;`response.function_call_arguments.delta` 按 item_id 累积。
/// 终止:`response.completed` / `response.incomplete` 的 `response` 是完整
/// 定稿(output items + usage)——以定稿为准冲刷 AssistantMessage 与
/// Usage;`response.failed` → Other(策略层失败通道);`[DONE]` 兜底。
#[derive(Debug, Default)]
pub struct ResponsesMapper {
    /// 按 item_id 累积的 function_call arguments(定稿前)
    pending_args: Vec<(String, String)>,
    /// 已从 output_item.done 定稿的 function_call
    tool_calls: Vec<Value>,
    /// 累积文本(仅 chunk 展示;冲刷以 completed 定稿为准)
    content: String,
    /// completed 是否已冲刷
    flushed: bool,
    /// Done 是否已发出(终止事件只发一次:completed 与 [DONE] 谁先到谁发)
    done: bool,
}

impl ResponsesMapper {
    /// 构建空映射器
    pub fn new() -> Self {
        Self::default()
    }

    fn flush(&mut self) -> Vec<StreamEvent> {
        if self.flushed {
            return Vec::new();
        }
        self.flushed = true;
        let calls = std::mem::take(&mut self.tool_calls);
        let content = std::mem::take(&mut self.content);
        vec![StreamEvent::AssistantMessage(json!({
            "content": content,
            "tool_calls": calls,
        }))]
    }

    /// 从 response.completed / output_item.done 的 item 提取 function_call
    fn finalize_item(&mut self, item: &Value) {
        if item["type"] == "function_call" {
            // item 的 arguments 是完整字符串;优先于增量累积
            self.tool_calls.push(json!({
                "id": item["call_id"],
                "name": item["name"],
                "arguments": item["arguments"],
            }));
        }
    }
}

impl FrameMapper for ResponsesMapper {
    fn frame(&mut self, data: &str) -> Vec<StreamEvent> {
        if self.done {
            return Vec::new();
        }
        if data == "[DONE]" {
            // DeepSeek responses 无 [DONE];兼容网关可能发,兜底
            let mut events = self.flush();
            events.push(StreamEvent::Done);
            self.done = true;
            return events;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return vec![StreamEvent::Other(Value::String(data.to_string()))];
        };
        match v["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => {
                let text = v["delta"].as_str().unwrap_or_default();
                self.content.push_str(text);
                vec![StreamEvent::Chunk(text.to_string())]
            }
            "response.reasoning_text.delta" => {
                let text = v["delta"].as_str().unwrap_or_default();
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![StreamEvent::Reasoning(text.to_string())]
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = v["item_id"].as_str().unwrap_or_default().to_string();
                let partial = v["delta"].as_str().unwrap_or_default();
                if let Some((_, args)) = self.pending_args.iter_mut().find(|(id, _)| *id == item_id)
                {
                    args.push_str(partial);
                } else {
                    self.pending_args.push((item_id, partial.to_string()));
                }
                Vec::new()
            }
            "response.output_item.done" => {
                let item = &v["item"];
                // function_call 定稿(arguments 完整);message 忽略(completed 兜底)
                self.finalize_item(item);
                Vec::new()
            }
            "response.completed" | "response.incomplete" => {
                // 定稿冲刷:以 response.output 为准;usage 挂在同一 response 对象
                let mut events = Vec::new();
                if let Some(response) = v["response"].as_object() {
                    if let Some(output) = response.get("output").and_then(Value::as_array) {
                        let final_calls: Vec<Value> = output
                            .iter()
                            .filter(|item| item["type"] == "function_call")
                            .map(|item| {
                                json!({
                                    "id": item["call_id"],
                                    "name": item["name"],
                                    "arguments": item["arguments"],
                                })
                            })
                            .collect();
                        if !final_calls.is_empty() {
                            self.tool_calls = final_calls;
                        }
                        // 文本以定稿为准(增量可能有细微差异)
                        if let Some(text) = output.iter().find_map(|item| {
                            item["content"].as_array().and_then(|blocks| {
                                blocks
                                    .iter()
                                    .find(|b| b["type"] == "output_text")
                                    .and_then(|b| b["text"].as_str())
                            })
                        }) {
                            self.content = text.to_string();
                        }
                    }
                    if let Some(usage) = response.get("usage")
                        && usage.is_object()
                    {
                        events.push(StreamEvent::Usage(crate::usage::normalize_responses(usage)));
                    }
                }
                events.extend(self.flush());
                events.push(StreamEvent::Done);
                self.done = true;
                events
            }
            "response.failed" | "response.error" => vec![StreamEvent::Other(v)],
            "error" => vec![StreamEvent::Other(v)],
            _ => Vec::new(),
        }
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        // 兜底:断流未收到 completed
        let pending: Vec<Value> = std::mem::take(&mut self.pending_args)
            .into_iter()
            .map(|(id, args)| json!({ "id": id, "name": "", "arguments": args }))
            .collect();
        if !pending.is_empty() {
            self.tool_calls.extend(pending);
        }
        if !self.tool_calls.is_empty() || !self.content.is_empty() {
            return self.flush();
        }
        Vec::new()
    }
}
