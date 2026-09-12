//! 真实 LLM HTTP transport(网络层 + provider adapters):reqwest + SSE。
//!
//! 传输层只拥有「连接语义」:连接池/TLS/鉴权头/endpoint 拼接/SSE 帧提取;
//! **方言差异**(请求体形状/流事件语义)在 [`crate::adapters::ProviderAdapter`]。
//! 默认 OpenAI Chat 方言([`HttpTransport::new`]);Claude Messages /
//! OpenAI Responses 经 [`HttpTransport::with_adapter`] 接入。
//! 策略(重试/限额)不在此层——那是插件组件的职责(LLM 传输分界)。

use dsh_agent_loop::{LlmEvent, LlmTransport, RequestHeader, TransportError};
use serde_json::Value;
use std::time::Duration;

use std::sync::Arc;

use crate::adapters::ProviderAdapter;
use crate::attachments::{
    AttachmentSource, MAX_REQUEST_IMAGE_BYTES, NoAttachments, offload_request_images,
    strip_images_for_summary,
};
use crate::chat::GenericChatAdapter;
use crate::ext::OpenAiChatExt;
use crate::streaming::{MappedDecoder, StreamMode};

/// reqwest 发送/读体错误归类:超时(连接超时/读超时)→ TIMEOUT,
/// 其余(连接失败/DNS/TLS/流中断)→ TRANSPORT
fn classify_reqwest(e: reqwest::Error) -> TransportError {
    if e.is_timeout() {
        TransportError::Timeout(e.to_string())
    } else {
        TransportError::Transport(e.to_string())
    }
}

/// 非 2xx 状态归类(httpErrorCode:401/403 → AUTH;400/413 →
/// INVALID_REQUEST;429 → RATE_LIMIT;≥500 → SERVER;其余未分类直通)
fn classify_status(
    status: reqwest::StatusCode,
    retry_after: Option<String>,
    body: String,
) -> TransportError {
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return TransportError::Auth {
            status: status.as_u16(),
            body,
        };
    }
    if matches!(
        status,
        reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::PAYLOAD_TOO_LARGE
    ) {
        return TransportError::InvalidRequest {
            status: status.as_u16(),
            body,
        };
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return TransportError::RateLimit {
            retry_after_ms: retry_after.as_deref().and_then(parse_retry_after_ms),
            body,
        };
    }
    if status.is_server_error() {
        return TransportError::Server {
            status: status.as_u16(),
            body,
        };
    }
    TransportError::Other(format!("provider {status}: {body}"))
}

/// Retry-After 解析:整秒数值(毫秒换算,≤0 视为缺失)。
/// HTTP 日期形式不支持(回落 None → 本地退避)
fn parse_retry_after_ms(v: &str) -> Option<u64> {
    v.trim()
        .parse::<u64>()
        .ok()
        .map(|secs| secs * 1000)
        .filter(|ms| *ms > 0)
}

/// 首块是否 SSE 帧(字段行 data:/event:/id:/retry: / 注释行;容忍空白前缀)
fn looks_like_sse(first: &[u8]) -> bool {
    let t = String::from_utf8_lossy(first);
    let t = t.trim_start();
    ["data:", "event:", "id:", "retry:", ":"]
        .iter()
        .any(|p| t.starts_with(p))
}

/// 2xx 但响应体不是 SSE:端点可能以 200 + JSON 错误体应答(bigmodel
/// 坏 key 实测形态)。体含 `error` 对象且 `code` 可解释为 HTTP 状态
/// → 按状态归类(401/403 → AUTH,不可重试);其余返回 None,交由
/// 引擎按空响应处理(既有语义)
fn classify_body_error(body: &str) -> Option<TransportError> {
    let trimmed = body.trim_start();
    if !trimmed.starts_with('{') {
        return None;
    }
    let v: Value = serde_json::from_str(body).ok()?;
    let code = v["error"]["code"]
        .as_u64()
        .or_else(|| v["error"]["code"].as_str()?.parse::<u64>().ok())?;
    let status = reqwest::StatusCode::from_u16(u16::try_from(code).ok()?).ok()?;
    Some(classify_status(status, None, body.to_string()))
}

/// 连接建立超时(TCP+TLS)
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 读超时:任意两次字节间的最长等待。覆盖「请求已发出但服务端零
/// 响应」与「流式中途断流」两类悬挂(实测大上下文请求经不稳网络
/// 路径可无限卡死,turn 永久悬挂且零反馈);活跃流式的 chunk 间隔
/// 远小于此,不受影响
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// provider 连接配置
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// API base URL(方言相关:openai-chat 形如 `https://api.deepseek.com/v1`;
    /// anthropic 形如 `https://api.anthropic.com`)
    pub base_url: String,
    /// API key(鉴权头形状由 adapter 决定)
    pub api_key: String,
    /// 流模式(provider 声明)
    pub stream_mode: StreamMode,
}

/// OpenAI 兼容流式 transport
pub struct HttpTransport {
    client: reqwest::Client,
    config: ProviderConfig,
    adapter: Box<dyn ProviderAdapter>,
    /// 图片附件字节来源(请求期组 data URL;缺省 = 无来源,图片降级占位)
    attachments: Arc<dyn AttachmentSource>,
}

impl HttpTransport {
    /// 以配置构建(默认 OpenAI Chat 方言;既有行为不变)
    pub fn new(config: ProviderConfig) -> Result<Self, String> {
        Self::with_adapter(config, Box::new(GenericChatAdapter::new(OpenAiChatExt)))
    }

    /// 以指定方言 adapter 构建(Claude Messages / OpenAI Responses 等)
    pub fn with_adapter(
        config: ProviderConfig,
        adapter: Box<dyn ProviderAdapter>,
    ) -> Result<Self, String> {
        Self::with_timeouts(config, adapter, CONNECT_TIMEOUT, READ_TIMEOUT)
    }

    /// 显式超时构建(测试注入短超时;语义同 [`Self::with_adapter`])
    pub fn with_timeouts(
        config: ProviderConfig,
        adapter: Box<dyn ProviderAdapter>,
        connect: Duration,
        read: Duration,
    ) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(connect)
            .read_timeout(read)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            config,
            adapter,
            attachments: Arc::new(NoAttachments),
        })
    }

    /// 注入图片附件字节来源(装配点;registry 传 dsh-host AttachmentStore)
    pub fn with_attachments(mut self, source: Arc<dyn AttachmentSource>) -> Self {
        self.attachments = source;
        self
    }

    /// 出网请求体:请求级 offload(超预算最旧图下车)后交方言翻译
    fn wire_request(&self, header: &RequestHeader, messages: &Value) -> Value {
        let mut msgs = messages.clone();
        offload_request_images(&mut msgs, MAX_REQUEST_IMAGE_BYTES);
        self.adapter
            .build_request(header, &msgs, self.attachments.as_ref())
    }
}

impl LlmTransport for HttpTransport {
    async fn stream(
        &mut self,
        header: &RequestHeader,
        messages: &Value,
    ) -> Result<Vec<LlmEvent>, TransportError> {
        let url = format!(
            "{}/{}",
            self.config.base_url.trim_end_matches('/'),
            self.adapter.endpoint()
        );
        let mut request = self
            .client
            .post(&url)
            .json(&self.wire_request(header, messages));
        for (name, value) in self.adapter.auth_headers(&self.config.api_key) {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(classify_reqwest)?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let body = response.text().await.unwrap_or_default();
            return Err(classify_status(status, retry_after, body));
        }

        // 流式消费:字节 → 帧提取 + 方言映射 → 事件
        // (记录首内容帧到达时刻——状态栏「首 token」指标的数据源)
        let started = std::time::Instant::now();
        let mut first_chunk_at: Option<std::time::Duration> = None;
        let mut decoder = MappedDecoder::new(self.config.stream_mode, self.adapter.mapper());
        let mut events = Vec::new();
        // 200 + 非 SSE 体(坏 key 时端点以 200 + JSON 错误体应答):整段
        // 攒为错误体,循环结束后归类
        let mut sse_confirmed = false;
        let mut error_body = String::new();
        let mut response = response;
        while let Some(chunk) = response.chunk().await.map_err(classify_reqwest)? {
            if !sse_confirmed {
                if !looks_like_sse(&chunk) {
                    error_body.push_str(&String::from_utf8_lossy(&chunk));
                    continue;
                }
                sse_confirmed = true;
            }
            for event in decoder.feed(&chunk) {
                if let Some(mapped) = map_event(event) {
                    if first_chunk_at.is_none() && matches!(mapped, LlmEvent::Chunk(_)) {
                        first_chunk_at = Some(started.elapsed());
                    }
                    events.push(mapped);
                }
            }
        }
        if !error_body.is_empty()
            && let Some(err) = classify_body_error(&error_body)
        {
            return Err(err);
        }
        for event in decoder.finish() {
            if let Some(mapped) = map_event(event) {
                events.push(mapped);
            }
        }
        if let Some(ttft) = first_chunk_at {
            events.push(LlmEvent::Usage(
                serde_json::json!({ "ttftMs": ttft.as_millis() as u64 }),
            ));
        }
        Ok(events)
    }

    /// 流式路径:SSE 帧解码后**逐事件送入 channel**(chunk 到达即下发,
    /// 引擎 select 逐条落档广播 → 前端逐 token 渲染)。请求/鉴权同 [`LlmTransport::stream`]。
    async fn stream_events(
        &mut self,
        header: &RequestHeader,
        messages: &Value,
        tx: tokio::sync::mpsc::UnboundedSender<LlmEvent>,
    ) -> Result<(), TransportError> {
        if std::env::var_os("DSH_PROBE").is_some() {
            eprintln!("[h1] http.stream_events 进入");
        }
        let url = format!(
            "{}/{}",
            self.config.base_url.trim_end_matches('/'),
            self.adapter.endpoint()
        );
        let mut request = self
            .client
            .post(&url)
            .json(&self.wire_request(header, messages));
        for (name, value) in self.adapter.auth_headers(&self.config.api_key) {
            request = request.header(name, value);
        }
        if std::env::var_os("DSH_PROBE").is_some() {
            eprintln!("[h2] send 前 url={url}");
        }
        let response = request.send().await.map_err(classify_reqwest)?;
        if std::env::var_os("DSH_PROBE").is_some() {
            eprintln!("[h3] 响应头到达 {}", response.status());
        }
        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let body = response.text().await.unwrap_or_default();
            return Err(classify_status(status, retry_after, body));
        }

        let started = std::time::Instant::now();
        let mut first_chunk_at: Option<std::time::Duration> = None;
        let mut decoder = MappedDecoder::new(self.config.stream_mode, self.adapter.mapper());
        // 200 + 非 SSE 体:同 collect 路径,攒错误体归类
        let mut sse_confirmed = false;
        let mut error_body = String::new();
        let mut response = response;
        while let Some(chunk) = response.chunk().await.map_err(classify_reqwest)? {
            if !sse_confirmed {
                if !looks_like_sse(&chunk) {
                    error_body.push_str(&String::from_utf8_lossy(&chunk));
                    continue;
                }
                sse_confirmed = true;
            }
            for event in decoder.feed(&chunk) {
                if let Some(mapped) = map_event(event) {
                    if std::env::var_os("DSH_PROBE").is_some() {
                        let tag = match &mapped {
                            LlmEvent::Chunk(_) => "chunk",
                            LlmEvent::Reasoning(_) => "reason",
                            LlmEvent::Done => "done",
                            _ => "other",
                        };
                        eprintln!("[t1] +{}ms {tag}", started.elapsed().as_millis());
                    }
                    if first_chunk_at.is_none() && matches!(mapped, LlmEvent::Chunk(_)) {
                        first_chunk_at = Some(started.elapsed());
                    }
                    let _ = tx.send(mapped);
                }
            }
        }
        if !error_body.is_empty()
            && let Some(err) = classify_body_error(&error_body)
        {
            return Err(err);
        }
        for event in decoder.finish() {
            if let Some(mapped) = map_event(event) {
                let _ = tx.send(mapped);
            }
        }
        if let Some(ttft) = first_chunk_at {
            let _ = tx.send(LlmEvent::Usage(
                serde_json::json!({ "ttftMs": ttft.as_millis() as u64 }),
            ));
        }
        Ok(())
    }
}

/// 宿主流事件 → loop 端口事件(Other 丢弃;策略层可按需扩展)
fn map_event(event: crate::streaming::StreamEvent) -> Option<LlmEvent> {
    use crate::streaming::StreamEvent;
    match event {
        StreamEvent::Chunk(s) => Some(LlmEvent::Chunk(s)),
        StreamEvent::Reasoning(s) => Some(LlmEvent::Reasoning(s)),
        StreamEvent::AssistantMessage(v) => Some(LlmEvent::AssistantMessage(v)),
        StreamEvent::Usage(v) => Some(LlmEvent::Usage(v)),
        StreamEvent::Done => Some(LlmEvent::Done),
        StreamEvent::Other(v) => Some(LlmEvent::Failure(v)),
    }
}

/// 一次性摘要调用:复用同一 adapter/client,替换 system 与
/// tools(一次性请求不携带工具目录),累积流式文本为摘要
impl dsh_agent_loop::Summarizer for HttpTransport {
    fn summarize<'a>(
        &'a mut self,
        header: &'a RequestHeader,
        messages: &'a Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        Box::pin(async move {
            let one_shot = RequestHeader {
                model: header.model.clone(),
                system: "Summarize the conversation so far for a coding agent session. \
Keep key decisions, file paths, commands, and open tasks. Reply with the summary only."
                    .into(),
                temperature: 0.0,
                reasoning_effort: None,
                tools: Vec::new(),
            };
            // 折叠请求上限 30s:超时按折叠失败处理(engine 降级跳过);
            // 摘要 = 纯文本面(源 compaction text-only:图块降级占位,不背 base64)
            let events = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.stream(&one_shot, &strip_images_for_summary(messages)),
            )
            .await
            .map_err(|_| "summarize timeout after 30s".to_string())?
            .map_err(|e| e.to_string())?;
            let mut text = String::new();
            for event in events {
                match event {
                    LlmEvent::Chunk(delta) => text.push_str(&delta),
                    LlmEvent::AssistantMessage(m) => {
                        if let Some(c) = m["content"].as_str() {
                            text.push_str(c);
                        }
                    }
                    _ => {}
                }
            }
            Ok(text)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 极简 mock 服务器:接受一个连接,读请求,回固定 SSE 体(Connection: close 分帧)
    async fn spawn_sse_server(
        response_body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_clone = std::sync::Arc::clone(&captured);
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            // 读到请求头结束(简化:一次 read 足够小请求)
            let n = socket.read(&mut buf).await.unwrap();
            captured_clone.lock().unwrap().extend_from_slice(&buf[..n]);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{response_body}"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        (format!("http://{addr}"), captured)
    }

    #[tokio::test]
    async fn streams_openai_sse_over_http() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
                    data: [DONE]\n\n";
        let (base_url, captured) = spawn_sse_server(body).await;

        let mut transport = HttpTransport::new(ProviderConfig {
            base_url,
            api_key: "sk-test".into(),
            stream_mode: StreamMode::Sse,
        })
        .unwrap();
        let header = RequestHeader {
            model: "test-model".into(),
            system: "be brief".into(),
            temperature: 0.1,
            reasoning_effort: None,
            tools: Vec::new(),
        };
        let events = transport
            .stream(&header, &json!([{ "role": "user", "content": "hi" }]))
            .await
            .expect("stream");
        assert_eq!(
            events,
            vec![
                LlmEvent::Chunk("Hel".into()),
                LlmEvent::Chunk("lo".into()),
                LlmEvent::Done,
                // transport 附带的首 token 指标(mock 下为 0ms)
                LlmEvent::Usage(serde_json::json!({ "ttftMs": 0 })),
            ]
        );

        // 请求体断言:system 头部注入 + model/stream/temperature
        let raw = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        let body_start = raw.find("\r\n\r\n").expect("body") + 4;
        let sent: Value = serde_json::from_str(&raw[body_start..]).expect("json body");
        assert_eq!(sent["model"], "test-model");
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["messages"][0]["role"], "system");
        assert_eq!(sent["messages"][1]["role"], "user");
        assert!(sent.get("tools").is_none(), "无工具声明时不带 tools 键");
    }

    /// 悬挂服务端(接受连接后永不回包):读超时必须把 stream() 在
    /// 秒级转成 Err——否则 turn 永久悬挂零反馈(「能发不能收」的
    /// 传输层根因:大上下文请求经不稳网络路径可无限停滞)
    #[tokio::test]
    async fn stalled_server_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            // 接受后挂起:连接保持但零字节,模拟断流/无响应
            std::future::pending::<()>().await;
        });
        let mut transport = HttpTransport::with_timeouts(
            ProviderConfig {
                base_url: format!("http://{addr}"),
                api_key: "sk-test".into(),
                stream_mode: StreamMode::Sse,
            },
            Box::new(GenericChatAdapter::new(OpenAiChatExt)),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        let header = RequestHeader {
            model: "test-model".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: Vec::new(),
        };
        let started = std::time::Instant::now();
        let result = transport
            .stream(&header, &json!([{ "role": "user", "content": "hi" }]))
            .await;
        assert!(result.is_err(), "悬挂服务端必须以 Err 终止: {result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "读超时应秒级触发,实际 {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn sends_tools_and_parses_streamed_tool_calls() {
        // 工具闭环的 HTTP 侧:请求体带 tools/tool_choice;
        // 流式 delta.tool_calls 增量累积 → AssistantMessage(tool_calls)
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\
                    \"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"co\"}}]}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":\
                    {\"arguments\":\"mmand\\\":\\\"echo hi\\\"}\"}}]}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                    data: [DONE]\n\n";
        let (base_url, captured) = spawn_sse_server(body).await;

        let mut transport = HttpTransport::new(ProviderConfig {
            base_url,
            api_key: "sk-test".into(),
            stream_mode: StreamMode::Sse,
        })
        .unwrap();
        let header = RequestHeader {
            model: "test-model".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "parameters": { "type": "object" },
                },
            })],
        };
        let events = transport
            .stream(&header, &json!([{ "role": "user", "content": "run it" }]))
            .await
            .expect("stream");

        // 请求体:tools + tool_choice
        let raw = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        let body_start = raw.find("\r\n\r\n").expect("body") + 4;
        let sent: Value = serde_json::from_str(&raw[body_start..]).expect("json body");
        assert_eq!(sent["tools"][0]["function"]["name"], "bash");
        assert_eq!(sent["tool_choice"], "auto");

        // 响应流:累积为完整工具调用消息
        assert_eq!(
            events,
            vec![
                LlmEvent::AssistantMessage(json!({
                    "content": "",
                    "tool_calls": [ {
                        "id": "c1", "name": "bash",
                        "arguments": "{\"command\":\"echo hi\"}"
                    } ],
                })),
                LlmEvent::Done
            ]
        );
    }

    #[tokio::test]
    async fn error_status_surfaced() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let response =
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope";
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        let mut transport = HttpTransport::new(ProviderConfig {
            base_url: format!("http://{addr}"),
            api_key: "bad".into(),
            stream_mode: StreamMode::Sse,
        })
        .unwrap();
        let err = transport
            .stream(
                &RequestHeader {
                    model: "m".into(),
                    system: String::new(),
                    temperature: 0.0,
                    reasoning_effort: None,
                    tools: Vec::new(),
                },
                &json!([]),
            )
            .await
            .expect_err("401 必须报错");
        assert_eq!(err.code(), "AUTH", "401 归类鉴权失败: {err:?}");
        assert!(err.to_string().contains("401"), "got: {err}");
    }
}
