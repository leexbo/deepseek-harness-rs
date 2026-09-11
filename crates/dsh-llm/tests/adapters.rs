//! Provider 方言契约测试:anthropic / openai-responses。
//!
//! 断言面(每方言):
//! - build_request:内部消息方言 → wire 形状(system 位置、tools 形状、
//!   工具调用往返的 assistant/tool 消息翻译);
//! - 流映射器:真实 SSE 事件序列 → 宿主流事件(工具调用累积、
//!   冲刷为内部方言 AssistantMessage);
//! - HTTP 层:endpoint 拼接与鉴权头(anthropic 的 x-api-key)。

use std::sync::{Arc, Mutex};

use dsh_agent_loop::RequestHeader;
use dsh_llm::adapters::ProviderAdapter;
use dsh_llm::anthropic::GenericAnthropicAdapter;
use dsh_llm::ext::{DeepSeekResponsesExt, OpenAiResponsesExt, StandardAnthropicExt};
use dsh_llm::http::{HttpTransport, ProviderConfig};
use dsh_llm::responses::GenericResponsesAdapter;
use dsh_llm::streaming::{StreamEvent, StreamMode};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn header(system: &str, tools: Vec<Value>) -> RequestHeader {
    RequestHeader {
        model: "test-model".into(),
        system: system.into(),
        temperature: 0.0,
        reasoning_effort: None,
        tools,
    }
}

fn bash_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "bash",
            "description": "run a command",
            "parameters": { "type": "object", "required": ["command"] },
        },
    })
}

fn tool_roundtrip_messages() -> Value {
    json!([
        { "role": "user", "content": "run it" },
        { "role": "assistant", "content": "", "tool_calls": [
            { "id": "toolu_1", "name": "bash", "arguments": "{\"command\":\"echo hi\"}" },
        ]},
        { "role": "tool", "output": "hi", "call": 6, "id": "toolu_1" },
    ])
}

// ============================================================
// Anthropic Messages
// ============================================================

#[test]
fn anthropic_request_shape() {
    let adapter = GenericAnthropicAdapter::new(StandardAnthropicExt);
    assert_eq!(adapter.endpoint(), "v1/messages");
    let auth = adapter.auth_headers("sk-ant");
    assert_eq!(auth[0], ("x-api-key".to_string(), "sk-ant".to_string()));
    assert!(auth.iter().any(|(k, _)| k == "anthropic-version"));

    let body = adapter.build_request(
        &header("be brief", vec![bash_tool()]),
        &tool_roundtrip_messages(),
        &dsh_llm::attachments::NoAttachments,
    );
    // system 顶层;max_tokens 必带;tools 扁平 input_schema
    assert_eq!(body["system"], "be brief");
    assert!(body["max_tokens"].as_u64().is_some());
    assert_eq!(body["tools"][0]["name"], "bash");
    assert!(body["tools"][0].get("input_schema").is_some());
    // assistant → tool_use block;tool → user 角色的 tool_result
    assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(
        body["messages"][1]["content"][0]["input"],
        json!({"command": "echo hi"})
    );
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_1");
}

/// 结果图片(MCP 图片桥)的 wire 形状:chat 方言 tool content 升级为
/// [text, image_url] 块数组;anthropic tool_result content 为 [text,
/// base64 image];字节缺席降级 offload 占位
#[test]
fn tool_message_with_images_wire_shape() {
    use dsh_llm::attachments::{AttachmentSource, OFFLOADED_IMAGE_TEXT};
    struct Fixed;
    impl AttachmentSource for Fixed {
        fn image_bytes(&self, _id: &str) -> Option<Vec<u8>> {
            Some(vec![1, 2, 3])
        }
    }
    let messages = json!([
        { "role": "tool", "output": "截图如下", "call": 6, "id": "t1",
          "images": [ { "type": "image", "attachment": {
              "attachmentId": "sha256:abc", "mediaType": "image/png",
              "bytes": 3, "width": 1, "height": 1 } } ] },
    ]);

    // chat 方言:text + image_url data URL(messages[0] = system)
    let chat = dsh_llm::adapters::adapter_by_name("deepseek-chat").unwrap();
    let body = chat.build_request(&header("s", vec![]), &messages, &Fixed);
    let content = &body["messages"][1]["content"];
    assert!(content.is_array(), "{content}");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "截图如下");
    assert_eq!(content[1]["type"], "image_url");
    let url = content[1]["image_url"]["url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"), "{url}");
    assert!(url.ends_with("AQID"), "{url}");

    // anthropic 方言:text + base64 source block
    let ant = GenericAnthropicAdapter::new(StandardAnthropicExt);
    let body = ant.build_request(&header("s", vec![]), &messages, &Fixed);
    let blocks = &body["messages"][0]["content"][0]["content"];
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "截图如下");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    assert_eq!(blocks[1]["source"]["data"], "AQID");

    // 字节缺席:降级占位文本(chat 形态;messages[1] = tool 消息)
    let body = chat.build_request(
        &header("s", vec![]),
        &messages,
        &dsh_llm::attachments::NoAttachments,
    );
    let content = &body["messages"][1]["content"];
    assert!(
        content
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["text"] == OFFLOADED_IMAGE_TEXT),
        "{content}"
    );
}

#[test]
fn anthropic_stream_mapping_with_tool_use() {
    let mut mapper = GenericAnthropicAdapter::<StandardAnthropicExt>::default().mapper();
    let mut events = Vec::new();
    for frame in [
        r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Le"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"t me"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash","input":{}}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"echo hi\"}"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#,
        r#"{"type":"message_stop"}"#,
    ] {
        events.extend(mapper.frame(frame));
    }
    assert_eq!(
        events,
        vec![
            StreamEvent::Usage(json!({"input_tokens": 10})),
            StreamEvent::Chunk("Le".into()),
            StreamEvent::Chunk("t me".into()),
            StreamEvent::Usage(json!({"output_tokens": 42})),
            StreamEvent::AssistantMessage(json!({
                "content": "Let me",
                "tool_calls": [ {
                    "id": "toolu_1", "name": "bash",
                    "arguments": "{\"command\":\"echo hi\"}"
                } ],
            })),
            StreamEvent::Done,
        ]
    );
}

#[tokio::test]
async fn anthropic_http_endpoint_and_auth() {
    let body = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n\
                event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
                event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = Arc::clone(&captured);
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        cap.lock().unwrap().extend_from_slice(&buf[..n]);
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        );
        socket.write_all(reply.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });

    let mut transport = HttpTransport::with_adapter(
        ProviderConfig {
            base_url: format!("http://{addr}"),
            api_key: "sk-ant-test".into(),
            stream_mode: StreamMode::Sse,
        },
        Box::new(GenericAnthropicAdapter::new(StandardAnthropicExt)),
    )
    .unwrap();
    use dsh_agent_loop::LlmTransport;
    let events = transport
        .stream(
            &header("", vec![]),
            &json!([{ "role": "user", "content": "hi" }]),
        )
        .await
        .expect("stream");

    // 请求行打到 v1/messages;鉴权头 x-api-key
    let raw = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    let request_line = raw.lines().next().unwrap();
    assert!(request_line.contains("/v1/messages"), "got: {request_line}");
    assert!(raw.contains("x-api-key: sk-ant-test"), "鉴权头缺失");

    // Anthropic SSE(event: 行 + data: 帧)正确映射
    // (末尾 Usage 为 transport 附带的首 token 指标,mock 下 0ms)
    assert_eq!(
        events,
        vec![
            LlmEventLike::chunk("hi"),
            LlmEventLike::assistant("hi"),
            LlmEventLike::done(),
            LlmEventLike(dsh_agent_loop::LlmEvent::Usage(
                serde_json::json!({ "ttftMs": 0 }),
            )),
        ]
        .into_iter()
        .map(Into::into)
        .collect::<Vec<dsh_agent_loop::LlmEvent>>()
    );
}

/// HTTP 层锁:deepseek-responses 打到 `{base}/responses`,Bearer 鉴权,
/// reasoning/usage 事件穿过 HTTP 层到达 LlmEvent(含 transport 合成的
/// ttftMs 尾帧;Responses 形态无 stream_options,usage 来自终结帧)。
#[tokio::test]
async fn deepseek_responses_http_endpoint_and_events() {
    let body = "data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\" hmm\"}\n\n\
                data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
                data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}],\"usage\":{\"input_tokens\":7,\"output_tokens\":2}}}\n\n";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = Arc::clone(&captured);
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        cap.lock().unwrap().extend_from_slice(&buf[..n]);
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        );
        socket.write_all(reply.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });

    let mut transport = HttpTransport::with_adapter(
        ProviderConfig {
            base_url: format!("http://{addr}"),
            api_key: "sk-ds-test".into(),
            stream_mode: StreamMode::Sse,
        },
        dsh_llm::adapter_by_name("deepseek-responses").unwrap(),
    )
    .unwrap();
    use dsh_agent_loop::LlmTransport;
    let events = transport
        .stream(
            &header("", vec![]),
            &json!([{ "role": "user", "content": "hi" }]),
        )
        .await
        .expect("stream");

    let raw = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    let request_line = raw.lines().next().unwrap();
    assert!(
        request_line.contains("POST /responses"),
        "got: {request_line}"
    );
    assert!(
        raw.contains("authorization: Bearer sk-ds-test"),
        "Bearer 鉴权头缺失"
    );
    // 请求体:responses 形态不带 stream_options(chat 形态专属)
    assert!(raw.contains("\"input\""), "responses 请求体缺 input");
    assert!(
        !raw.contains("stream_options"),
        "responses 不发 stream_options"
    );

    assert_eq!(
        events,
        vec![
            LlmEventLike(dsh_agent_loop::LlmEvent::Reasoning(" hmm".into())),
            LlmEventLike::chunk("hi"),
            LlmEventLike(dsh_agent_loop::LlmEvent::Usage(json!({
                "input_tokens": 7, "output_tokens": 2
            }))),
            LlmEventLike::assistant("hi"),
            LlmEventLike::done(),
            LlmEventLike(dsh_agent_loop::LlmEvent::Usage(json!({ "ttftMs": 0 }),)),
        ]
        .into_iter()
        .map(Into::into)
        .collect::<Vec<dsh_agent_loop::LlmEvent>>()
    );
}

/// 测试辅助:构造期望 LlmEvent(仅本文件断言用)
struct LlmEventLike(dsh_agent_loop::LlmEvent);

impl LlmEventLike {
    fn chunk(s: &str) -> Self {
        Self(dsh_agent_loop::LlmEvent::Chunk(s.into()))
    }
    fn assistant(content: &str) -> Self {
        Self(dsh_agent_loop::LlmEvent::AssistantMessage(json!({
            "content": content, "tool_calls": []
        })))
    }
    fn done() -> Self {
        Self(dsh_agent_loop::LlmEvent::Done)
    }
}

impl From<LlmEventLike> for dsh_agent_loop::LlmEvent {
    fn from(v: LlmEventLike) -> Self {
        v.0
    }
}

// ============================================================
// OpenAI Responses
// ============================================================

#[test]
fn responses_request_shape() {
    let adapter = GenericResponsesAdapter::new(OpenAiResponsesExt);
    assert_eq!(adapter.endpoint(), "responses");
    let body = adapter.build_request(
        &header("be brief", vec![bash_tool()]),
        &tool_roundtrip_messages(),
        &dsh_llm::attachments::NoAttachments,
    );
    // system → instructions;tools 扁平;assistant → function_call item;
    // tool → function_call_output item
    assert_eq!(body["instructions"], "be brief");
    assert_eq!(body["tools"][0]["name"], "bash");
    assert!(
        body["tools"][0].get("function").is_none(),
        "Responses 是扁平形状"
    );
    assert_eq!(body["input"][1]["type"], "function_call");
    assert_eq!(body["input"][1]["call_id"], "toolu_1");
    assert_eq!(body["input"][1]["arguments"], "{\"command\":\"echo hi\"}");
    assert_eq!(body["input"][2]["type"], "function_call_output");
    assert_eq!(body["input"][2]["output"], "hi");
}

#[test]
fn responses_stream_mapping_with_function_call() {
    let mut mapper = GenericResponsesAdapter::<OpenAiResponsesExt>::default().mapper();
    let mut events = Vec::new();
    for frame in [
        r#"{"type":"response.output_text.delta","delta":"Thin"}"#,
        r#"{"type":"response.output_text.delta","delta":"king…" }"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"command\":"}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"echo hi\"}"}"#,
        r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_9","name":"bash","arguments":"{\"command\":\"echo hi\"}"}}"#,
        r#"{"type":"response.completed","response":{"output":[
            {"type":"message","content":[{"type":"output_text","text":"Thinking…"}]},
            {"type":"function_call","call_id":"call_9","name":"bash","arguments":"{\"command\":\"echo hi\"}"}
        ]}}"#,
        "[DONE]",
    ] {
        events.extend(mapper.frame(frame));
    }
    assert_eq!(
        events,
        vec![
            StreamEvent::Chunk("Thin".into()),
            StreamEvent::Chunk("king…".into()),
            // completed 定稿:文本与 function_call 均以 response.output 为准
            StreamEvent::AssistantMessage(json!({
                "content": "Thinking…",
                "tool_calls": [ {
                    "id": "call_9", "name": "bash",
                    "arguments": "{\"command\":\"echo hi\"}"
                } ],
            })),
            StreamEvent::Done,
        ]
    );
    assert!(mapper.finish().is_empty(), "已冲刷,finish 无残余");
}

#[test]
fn adapter_by_name_resolves_and_rejects() {
    assert!(dsh_llm::adapters::adapter_by_name("deepseek-responses").is_some());
    assert!(dsh_llm::adapters::adapter_by_name("openai-responses").is_some());
    assert!(dsh_llm::adapters::adapter_by_name("deepseek-chat").is_some());
    assert!(dsh_llm::adapters::adapter_by_name("openai-chat").is_some());
    assert!(dsh_llm::adapters::adapter_by_name("anthropic").is_some());
    // 未知方言拒绝(fail-fast,不静默回退)
    assert!(dsh_llm::adapters::adapter_by_name("nope").is_none());
}

// ============================================================
// DeepSeek Responses(默认方言)
// ============================================================

/// wire 锁(依据 DeepSeek 官方 Responses API 文档):
/// 思考强度 = `reasoning: {effort}`(与 OpenAI Responses 同形;摘要中
/// 曾误记为 output_config,已按一手文档纠正);temperature 必发
/// (thinking mode 无效但 schema 合法);instructions 承载 system。
#[test]
fn deepseek_responses_request_shape() {
    let adapter = GenericResponsesAdapter::new(DeepSeekResponsesExt);
    let mut h = header("be brief", vec![]);
    h.reasoning_effort = Some("max".into());
    h.temperature = 0.7;
    let body = adapter.build_request(
        &h,
        &json!([{ "role": "user", "content": "hi" }]),
        &dsh_llm::attachments::NoAttachments,
    );
    assert_eq!(body["reasoning"]["effort"], "max");
    assert_eq!(body["temperature"], 0.7);
    assert_eq!(body["instructions"], "be brief");
    assert_eq!(body["stream"], true);
    assert!(
        body.get("output_config").is_none(),
        "effort 表达是 reasoning 对象,不是 output_config"
    );
}

/// 事件流锁(照官方文档事件语义):reasoning_text.delta → Reasoning;
/// completed 携带完整 response——文本/工具以 output 为准,usage 从中
/// 提取(Responses 形态无 stream_options,usage 天然在终结事件);
/// 无 [DONE] 也照常终结。
#[test]
fn deepseek_responses_stream_reasoning_and_usage() {
    let mut mapper = GenericResponsesAdapter::<DeepSeekResponsesExt>::default().mapper();
    let mut events = Vec::new();
    for frame in [
        r#"{"type":"response.created","response":{}}"#,
        r#"{"type":"response.reasoning_text.delta","delta":" thinking…"}"#,
        r#"{"type":"response.output_text.delta","delta":"Hel"}"#,
        r#"{"type":"response.output_text.delta","delta":"lo"}"#,
        r#"{"type":"response.completed","response":{
            "output":[{"type":"message","content":[{"type":"output_text","text":"Hello"}]}],
            "usage":{"input_tokens":10,"output_tokens":5,
                     "output_tokens_details":{"reasoning_tokens":3}}
        }}"#,
    ] {
        events.extend(mapper.frame(frame));
    }
    assert_eq!(
        events,
        vec![
            StreamEvent::Reasoning(" thinking…".into()),
            StreamEvent::Chunk("Hel".into()),
            StreamEvent::Chunk("lo".into()),
            // 终结帧:先 usage 再定稿消息,最后 Done
            StreamEvent::Usage(json!({
                "input_tokens": 10, "output_tokens": 5,
                "output_tokens_details": {"reasoning_tokens": 3}
            })),
            StreamEvent::AssistantMessage(json!({
                "content": "Hello", "tool_calls": []
            })),
            StreamEvent::Done,
        ]
    );
    assert!(mapper.finish().is_empty(), "已终结,finish 无残余");
}
