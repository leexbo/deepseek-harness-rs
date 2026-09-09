//! SSE + 裸流双模帧提取器与方言映射器(provider adapters)。
//!
//! - SSE 模式(OpenAI 兼容系/Anthropic/Responses):`data:` 行 + 空行分帧 +
//!   `[DONE]` 哨兵;`event:` 行按规范归属下一帧(Anthropic 方言需要,
//!   本层的帧提取保留它,语义解释在映射器)。
//! - 裸流模式(少量本地/开源网关):每行即一个 chunk。
//!
//! 分层:字节 → [`SseFramer`](帧)→ [`FrameMapper`](方言 → 流事件)。
//! 纯逻辑:字节进、事件出;UTF-8 跨块截断与 CRLF 均安全。
//! [`StreamDecoder`] 是 framer + OpenAI Chat 方言映射器的既有便捷组合。

use serde_json::Value;

/// 解码后的流事件(与 `dsh-agent-loop::LlmEvent` 对齐的宿主侧形态)
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// 增量文本(delta.content 或裸行)
    Chunk(String),
    /// 推理增量(delta.reasoning_content;DeepSeek 系思考内容)
    Reasoning(String),
    /// 完整消息帧(方言映射器物化;如携带 tool_calls 的 assistant 消息)
    AssistantMessage(Value),
    /// 用量统计帧
    Usage(Value),
    /// `[DONE]` 哨兵
    Done,
    /// 无法分类的 data 帧(保留原始载荷,策略层决定处置)
    Other(Value),
}

/// 流模式(provider 配置声明)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    /// `data:` 行 + 空行分帧 + `[DONE]`
    Sse,
    /// 裸行即 chunk
    Raw,
}

/// SSE data 帧映射器:provider 方言差异的收口。
///
/// 每个流请求一个实例(可带跨帧累积状态,如 tool_calls 增量);
/// 同样的帧序列必然产出同样的事件序列(重放确定性前提)。
pub trait FrameMapper: Send {
    /// 处理一个 data 帧载荷(SSE 模式;裸流模式不经过映射器)
    fn frame(&mut self, data: &str) -> Vec<StreamEvent>;
    /// 流终止冲刷(累积残余;默认无)
    fn finish(&mut self) -> Vec<StreamEvent> {
        Vec::new()
    }
}

/// 字节 → SSE data 帧(或裸流行)提取器。
#[derive(Debug)]
pub struct SseFramer {
    mode: StreamMode,
    buf: Vec<u8>,
    /// SSE 当前帧累积的 data 行
    frame_data: Vec<String>,
}

impl SseFramer {
    /// 按模式构建
    pub fn new(mode: StreamMode) -> Self {
        Self {
            mode,
            buf: Vec::new(),
            frame_data: Vec::new(),
        }
    }

    /// 喂入字节,产出完整帧(SSE 模式为 data 帧文本;裸流为行文本)
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop(); // '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(frame) = self.handle_line(&line) {
                frames.push(frame);
            }
        }
        frames
    }

    /// 流结束:冲刷未分帧的残余(SSE 模式的尾帧;裸流无残余)
    pub fn finish(&mut self) -> Vec<String> {
        let mut frames = Vec::new();
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            if let Some(frame) = self.handle_line(&line) {
                frames.push(frame);
            }
        }
        if !self.frame_data.is_empty() {
            let frame = self.frame_data.join("\n");
            self.frame_data.clear();
            frames.push(frame);
        }
        frames
    }

    fn handle_line(&mut self, line: &str) -> Option<String> {
        match self.mode {
            StreamMode::Raw => {
                if line.is_empty() {
                    None
                } else {
                    Some(line.to_string())
                }
            }
            StreamMode::Sse => {
                if let Some(data) = line.strip_prefix("data:") {
                    self.frame_data.push(data.trim_start().to_string());
                    None
                } else if line.is_empty() {
                    // 空行 = 帧边界
                    if self.frame_data.is_empty() {
                        return None;
                    }
                    let frame = self.frame_data.join("\n");
                    self.frame_data.clear();
                    Some(frame)
                } else {
                    // event:/id:/retry:/注释行:本层不解释
                    // (Anthropic 方言的 event 行与 data.type 冗余,映射器用 data.type)
                    None
                }
            }
        }
    }
}

/// OpenAI Chat Completions 方言映射器。
///
/// - `delta.content` → Chunk;usage 两种位置都识别;
/// - `delta.tool_calls` 增量累积(arguments 跨帧拼接、按 index 归位,
///   空 id/name 不覆盖首帧值);
/// - `finish_reason == "tool_calls"` 或 `[DONE]` 冲刷为完整 AssistantMessage
///   (内部方言:扁平 tool_calls `{id, name, arguments}`)。
#[derive(Debug, Default)]
pub struct OpenAiChatMapper {
    /// tool_calls 增量累积
    tool_calls: Vec<Value>,
    /// 累积的文本(冲刷 tool_calls 时作为 content)
    content: String,
}

impl OpenAiChatMapper {
    /// 构建空映射器
    pub fn new() -> Self {
        Self::default()
    }

    /// 冲刷累积的 tool_calls(清空状态;无累积返回空)
    fn flush_tool_calls(&mut self) -> Vec<StreamEvent> {
        if self.tool_calls.is_empty() {
            return Vec::new();
        }
        let calls = std::mem::take(&mut self.tool_calls);
        let content = std::mem::take(&mut self.content);
        vec![StreamEvent::AssistantMessage(serde_json::json!({
            "content": content,
            "tool_calls": calls,
        }))]
    }
}

impl FrameMapper for OpenAiChatMapper {
    fn frame(&mut self, frame: &str) -> Vec<StreamEvent> {
        if frame == "[DONE]" {
            let mut events = self.flush_tool_calls();
            events.push(StreamEvent::Done);
            return events;
        }
        let Ok(json) = serde_json::from_str::<Value>(frame) else {
            return vec![StreamEvent::Other(Value::String(frame.to_string()))];
        };
        // OpenAI 兼容:choices[0].delta
        if let Some(delta) = json["choices"][0]["delta"].as_object() {
            // tool_calls 增量:按 index 归位,id/name 首帧设置,arguments 拼接
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call["index"].as_u64().unwrap_or(0) as usize;
                    while self.tool_calls.len() <= index {
                        self.tool_calls.push(serde_json::json!({
                            "name": "", "arguments": ""
                        }));
                    }
                    let slot = &mut self.tool_calls[index];
                    // 部分 OpenAI 兼容 provider 的后续帧携带空 id/name
                    // (规范是省略字段):只在非空时覆盖,首帧值不被冲掉
                    if let Some(id) = call["id"].as_str().filter(|s| !s.is_empty()) {
                        slot["id"] = Value::String(id.to_string());
                    }
                    if let Some(name) = call["function"]["name"].as_str().filter(|s| !s.is_empty())
                    {
                        slot["name"] = Value::String(name.to_string());
                    }
                    if let Some(args) = call["function"]["arguments"].as_str() {
                        slot["arguments"] = Value::String(format!(
                            "{}{}",
                            slot["arguments"].as_str().unwrap_or_default(),
                            args
                        ));
                    }
                }
                return Vec::new();
            }
            // 推理增量(DeepSeek 系 thinking:delta.reasoning_content)
            if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
                && !reasoning.is_empty()
            {
                return vec![StreamEvent::Reasoning(reasoning.to_string())];
            }
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                if !self.tool_calls.is_empty() {
                    // 工具调用阶段的零星 content 记入累积(冲刷时带上)
                    self.content.push_str(content);
                }
                if !content.is_empty() {
                    return vec![StreamEvent::Chunk(content.to_string())];
                }
                // 空 content 增量帧:DeepSeek 系把 usage 挂在带 finish_reason 的
                // 帧里(delta.content = "")——继续检查 usage,否则 usage 被吞。
                // 真值守卫:OpenAI 规范实现(include_usage)的中间帧一律携带
                // "usage": null,仅流末帧是真实用量
                if let Some(usage) = json
                    .get("usage")
                    .or_else(|| json["choices"][0].get("usage"))
                    .filter(|u| u.is_object())
                {
                    return vec![StreamEvent::Usage(usage.clone())];
                }
                return Vec::new();
            }
            // 无 content 的 delta 帧:usage 可能在帧顶层或 choices[0] 内
            // (stream_options.include_usage 的末帧两种形态都有实现)
            if let Some(usage) = json
                .get("usage")
                .or_else(|| json["choices"][0].get("usage"))
                .filter(|u| u.is_object())
            {
                return vec![StreamEvent::Usage(usage.clone())];
            }
            // 工具调用终止:冲刷累积的 tool_calls 为完整消息
            if json["choices"][0]["finish_reason"].as_str() == Some("tool_calls") {
                return self.flush_tool_calls();
            }
            // 其余终止帧(stop/length 等,无载荷):纯终结信号,忽略。
            // 此前落 Other → LlmEvent::Failure,靠引擎静默掩盖;空响应
            // 收紧后暴露为假失败——本帧非错误,不应进失败通道
            if json["choices"][0].get("finish_reason").is_some() {
                return Vec::new();
            }
            return vec![StreamEvent::Other(json)];
        }
        if let Some(usage) = json.get("usage").filter(|u| u.is_object()) {
            return vec![StreamEvent::Usage(usage.clone())];
        }
        // 整消息形态(message 键)
        if json.get("message").is_some() {
            return vec![StreamEvent::AssistantMessage(json["message"].clone())];
        }
        vec![StreamEvent::Other(json)]
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        // 兜底:provider 省略 finish_reason 时在流终止冲刷累积的 tool_calls
        self.flush_tool_calls()
    }
}

/// 便捷组合:帧提取器 + 可插拔方言映射器(通用形态)。
pub struct MappedDecoder {
    framer: SseFramer,
    mapper: Box<dyn FrameMapper>,
}

impl MappedDecoder {
    /// 组合构建
    pub fn new(mode: StreamMode, mapper: Box<dyn FrameMapper>) -> Self {
        Self {
            framer: SseFramer::new(mode),
            mapper,
        }
    }

    /// 喂入字节,产出完整事件
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<StreamEvent> {
        let frames = self.framer.feed(bytes);
        self.map_frames(frames)
    }

    /// 流终止:冲刷帧残余与映射器累积
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        let frames = self.framer.finish();
        let mut events = self.map_frames(frames);
        events.extend(self.mapper.finish());
        events
    }

    fn map_frames(&mut self, frames: Vec<String>) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for frame in frames {
            match self.framer.mode {
                StreamMode::Raw => events.push(StreamEvent::Chunk(frame)),
                StreamMode::Sse => events.extend(self.mapper.frame(&frame)),
            }
        }
        events
    }
}

/// 既有便捷组合:帧提取器 + OpenAI Chat 方言(既有 API 与语义不变)。
pub struct StreamDecoder {
    inner: MappedDecoder,
}

impl StreamDecoder {
    /// 按模式构建(OpenAI Chat 方言)
    pub fn new(mode: StreamMode) -> Self {
        Self {
            inner: MappedDecoder::new(mode, Box::new(OpenAiChatMapper::new())),
        }
    }

    /// 喂入字节,产出完整事件
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<StreamEvent> {
        self.inner.feed(bytes)
    }

    /// 流终止冲刷
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        self.inner.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_openai_style_frames() {
        let mut d = StreamDecoder::new(StreamMode::Sse);
        let events = d.feed(
            b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"usage\":{\"total\":5}}]}\n\n\
             data: [DONE]\n\n",
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::Other(serde_json::json!({"choices":[{"delta":{"role":"assistant"}}]})),
                StreamEvent::Chunk("Hel".into()),
                StreamEvent::Chunk("lo".into()),
                StreamEvent::Usage(serde_json::json!({"total":5})),
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn sse_empty_content_usage_frame_not_swallowed() {
        // DeepSeek 系:usage 挂在带 finish_reason 的帧里且 delta.content = ""
        // (曾把该帧当空 chunk 消费,usage 丢失)
        let mut d = StreamDecoder::new(StreamMode::Sse);
        let events = d.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}],\
                  \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
             data: [DONE]\n\n",
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::Chunk("hi".into()),
                StreamEvent::Usage(serde_json::json!({
                    "prompt_tokens": 10,
                    "completion_tokens": 5
                })),
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn sse_null_usage_intermediate_frames_ignored() {
        // OpenAI 规范(include_usage):中间帧一律 "usage": null,仅末帧是真实
        // 用量。曾无守卫逐帧照发 Usage(Null),引擎用量槽被毒化后真实用量
        // 全被丢弃(统计条全零的回归源)
        let mut d = StreamDecoder::new(StreamMode::Sse);
        let events = d.feed(
            b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}],\"usage\":null}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":null}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}],\
                  \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
             data: [DONE]\n\n",
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::Chunk("hi".into()),
                // finish 挂载形态
                StreamEvent::Usage(serde_json::json!({
                    "prompt_tokens": 10,
                    "completion_tokens": 5
                })),
                // 尾随 usage-only 形态(choices:[])
                StreamEvent::Usage(serde_json::json!({
                    "prompt_tokens": 10,
                    "completion_tokens": 5
                })),
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn sse_survives_fragmented_utf8_and_crlf() {
        // 分块边界切在 UTF-8 字符中间 + CRLF 行尾
        let mut d = StreamDecoder::new(StreamMode::Sse);
        let payload = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\r\n\r\n";
        let bytes = payload.as_bytes();
        let mut events = Vec::new();
        for b in bytes {
            events.extend(d.feed(&[*b]));
        }
        assert_eq!(events, vec![StreamEvent::Chunk("你好".into())]);
    }

    #[test]
    fn sse_multi_data_lines_join_into_one_frame() {
        // 规范:同帧多 data: 行以 \n 连接后作为整体 data
        let mut d = StreamDecoder::new(StreamMode::Sse);
        let events = d.feed(b"data: {\"a\":\ndata: 1}\n\n");
        // 连接后是合法 JSON {"a":\n1} → 解析成功
        assert_eq!(
            events,
            vec![StreamEvent::Other(serde_json::json!({"a": 1}))]
        );
    }

    #[test]
    fn sse_tail_frame_flushed_on_finish() {
        let mut d = StreamDecoder::new(StreamMode::Sse);
        assert!(
            d.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"end\"}}]}\n")
                .is_empty()
        );
        assert_eq!(d.finish(), vec![StreamEvent::Chunk("end".into())]);
    }

    #[test]
    fn raw_mode_lines_are_chunks() {
        let mut d = StreamDecoder::new(StreamMode::Raw);
        let events = d.feed(b"foo\nbar\n\nbaz");
        assert_eq!(
            events,
            vec![
                StreamEvent::Chunk("foo".into()),
                StreamEvent::Chunk("bar".into())
            ]
        );
        assert_eq!(d.finish(), vec![StreamEvent::Chunk("baz".into())]);
    }

    #[test]
    fn sse_accumulates_tool_call_deltas_until_finish_reason() {
        // OpenAI 兼容流:首帧带 id/name,arguments 跨帧拼接,
        // 末帧 delta 空 + finish_reason=tool_calls
        let mut d = StreamDecoder::new(StreamMode::Sse);
        let events = d.feed(
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\
             \"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":\
             {\"arguments\":\"{\\\"command\\\":\\\"echo\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":\
             {\"arguments\":\" ok\\\"}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n",
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::AssistantMessage(serde_json::json!({
                    "content": "",
                    "tool_calls": [
                        { "id": "call_1", "name": "bash",
                          "arguments": "{\"command\":\"echo ok\"}" }
                    ],
                })),
                StreamEvent::Done,
            ],
            "arguments 跨帧拼接,finish_reason 冲刷为完整消息"
        );
        // 冲刷后状态清空:后续 finish 不再重复产出
        assert!(d.finish().is_empty());
    }

    #[test]
    fn sse_flushes_pending_tool_calls_on_done_without_finish_reason() {
        // provider 省略 finish_reason:[DONE] 兜底冲刷
        let mut d = StreamDecoder::new(StreamMode::Sse);
        let events = d.feed(
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":\
             {\"name\":\"bash\",\"arguments\":\"{}\"}}]}}]}\n\n\
             data: [DONE]\n\n",
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::AssistantMessage(serde_json::json!({
                    "content": "",
                    "tool_calls": [ { "name": "bash", "arguments": "{}" } ],
                })),
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn sse_empty_name_id_frames_do_not_clobber() {
        // 方言实测(OpenAI 兼容网关):首帧带 id/name,后续帧携带空值(须不覆盖)
        let mut d = StreamDecoder::new(StreamMode::Sse);
        let events = d.feed(
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_x\",\"type\":\"function\",\"index\":0,\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"\",\"type\":\"function\",\"index\":0,\"function\":{\"name\":\"\",\"arguments\":\"{}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        );
        let Some(StreamEvent::AssistantMessage(msg)) = events.first() else {
            panic!("应冲刷为完整消息");
        };
        assert_eq!(msg["tool_calls"][0]["id"], "call_x");
        assert_eq!(msg["tool_calls"][0]["name"], "bash");
    }

    #[test]
    fn sse_parallel_tool_calls_by_index() {
        let mut d = StreamDecoder::new(StreamMode::Sse);
        let events = d.feed(
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":\
             {\"name\":\"second\",\"arguments\":\"{}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":\
             {\"name\":\"first\",\"arguments\":\"{}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        );
        let Some(StreamEvent::AssistantMessage(msg)) = events.first() else {
            panic!("应冲刷为完整消息");
        };
        assert_eq!(msg["tool_calls"][0]["name"], "first");
        assert_eq!(msg["tool_calls"][1]["name"], "second");
    }
}
