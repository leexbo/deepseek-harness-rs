//! 流式回归(「假流式」事故):不变式闸门曾漏覆写
//! `stream_events`,落到 trait 默认实现(攒完全量再整批发送)——
//! 所有真实流量在闸门处被攒批,UI 永远一次性出全文。本组测试
//! 锁住两层:HTTP transport 逐帧到达、闸门转发不回退成攒批。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dsh_agent_loop::{LlmEvent, LlmTransport, RequestHeader, TransportError};
use dsh_llm::streaming::StreamMode;
use dsh_llm::{HttpTransport, InvariantGate, ProviderConfig};
use dsh_session::EventLog;
use serde_json::{Value, json};

fn header() -> RequestHeader {
    RequestHeader {
        model: "test-model".into(),
        system: String::new(),
        temperature: 0.0,
        reasoning_effort: None,
        tools: Vec::new(),
    }
}

/// 带节奏的 mock SSE 服务器:n 个事件,每个间隔 interval
async fn spawn_paced_sse_server(events: usize, interval: Duration) -> String {
    use tokio::io::AsyncWriteExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let _ = tokio::time::timeout(Duration::from_secs(2), socket.readable()).await;
        let _ = socket.try_read(&mut buf);
        let resp =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        socket.write_all(resp).await.unwrap();
        socket.flush().await.unwrap();
        for i in 0..events {
            let evt =
                format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"块{i}\"}}}}]}}\n\n");
            socket.write_all(evt.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(interval).await;
        }
        socket.write_all(b"data: [DONE]\n\n").await.unwrap();
        socket.flush().await.unwrap();
        let _ = socket.shutdown().await;
    });
    format!("http://{addr}")
}

/// HTTP transport 逐帧到达:首事件必须显著早于流结束
#[tokio::test]
async fn http_transport_streams_incrementally() {
    let base = spawn_paced_sse_server(6, Duration::from_millis(120)).await;
    let mut transport = HttpTransport::new(ProviderConfig {
        base_url: base,
        api_key: "k".into(),
        stream_mode: StreamMode::Sse,
    })
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let messages = json!([]);
    let hdr = header();
    let t0 = std::time::Instant::now();
    let fut = transport.stream_events(&hdr, &messages, tx);
    tokio::pin!(fut);
    tokio::select! {
        r = &mut fut => {
            panic!("流已结束但未观察到增量事件: {r:?}(整流时长 ~720ms)");
        }
        ev = rx.recv() => {
            let first = t0.elapsed();
            assert!(ev.is_some(), "首事件应到达");
            // 首事件 < 360ms(半个流长);攒批实现会到 ~720ms 后才来
            assert!(
                first < Duration::from_millis(360),
                "首事件 +{first:?} 到达——疑似攒批(假流式回归)"
            );
        }
    }
}

/// 节奏内层 transport(记录调用)
struct PacedInner {
    calls: Arc<AtomicUsize>,
}

impl LlmTransport for PacedInner {
    async fn stream(
        &mut self,
        _header: &RequestHeader,
        _messages: &Value,
    ) -> Result<Vec<LlmEvent>, TransportError> {
        Err(TransportError::Other("闸门不得走攒批 stream 路径".into()))
    }

    async fn stream_events(
        &mut self,
        _header: &RequestHeader,
        _messages: &Value,
        tx: tokio::sync::mpsc::UnboundedSender<LlmEvent>,
    ) -> Result<(), TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        for i in 0..5 {
            let _ = tx.send(LlmEvent::Reasoning(format!("段{i}")));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }
}

impl dsh_agent_loop::Summarizer for PacedInner {
    fn summarize<'a>(
        &'a mut self,
        _header: &'a RequestHeader,
        _messages: &'a Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        Box::pin(async { Ok(String::new()) })
    }
}

/// 闸门转发必须保持内层的渐进节奏(曾漏覆写 → trait 默认攒批)
#[tokio::test]
async fn invariant_gate_forwards_progressively() {
    let calls = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut gate = InvariantGate::new(
        PacedInner {
            calls: calls.clone(),
        },
        Arc::clone(&log),
    );
    // 空日志派生 = 空消息;请求也空 → 校验通过
    let messages = json!([]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let hdr = header();
    let t0 = std::time::Instant::now();
    let fut = gate.stream_events(&hdr, &messages, tx);
    tokio::pin!(fut);
    tokio::select! {
        r = &mut fut => {
            panic!("闸门整批完成(+{:?}): {r:?}——转发退化为攒批", t0.elapsed());
        }
        ev = rx.recv() => {
            let first = t0.elapsed();
            assert!(ev.is_some());
            assert!(
                first < Duration::from_millis(300),
                "闸门后首事件 +{first:?}——转发退化为攒批(假流式回归)"
            );
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "内层应被调用一次");
}

/// 闸门校验语义不变:请求与派生不一致时,发起前即拒绝
#[tokio::test]
async fn invariant_gate_still_rejects_divergence() {
    let calls = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut gate = InvariantGate::new(
        PacedInner {
            calls: calls.clone(),
        },
        Arc::clone(&log),
    );
    // 空日志派生 = 空消息;请求带一条 → 违反「模型可见 ⟺ 已记录」
    let messages = json!([{ "role": "user", "content": "未记录的内容" }]);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = gate.stream_events(&header(), &messages, tx).await;
    assert!(result.is_err(), "派生不一致必须拒绝: {result:?}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "拒绝必须发生在发起前(内层不得被调用)"
    );
}
