//! 工具闭环 HTTP 端到端(CLI 装配出口门):真实 wire 形状驱动全链。
//!
//! 断言链(mock SSE 服务器,双响应):
//! - 第一响应流式 delta.tool_calls → engine 解出 bash 调用 → 沙箱执行
//!   (workspace 内落文件)→ tool/result 入日志;
//! - 第二响应最终文本 → turn 收尾;
//! - 两次请求体:带 tools 声明;第二次包含 assistant(tool_calls)
//!   与 tool 结果消息(全部来自日志派生,闸门强制);
//! - 日志含 audit/call 归因(llm ×2 + tool ×1,E5)。

use std::sync::{Arc, Mutex};

use dsh_agent_loop::{LoopEngine, RequestHeader};
use dsh_host::JsonlBackend;
use dsh_llm::streaming::StreamMode;
use dsh_llm::{FakeProvider, HttpTransport, InvariantGate, ProviderConfig};
use dsh_session::{EventEnvelope, EventLog};
use dsh_tools::BashTool;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn http_tool_round_trip_through_sandbox() {
    let dir = std::env::temp_dir().join(format!("dsh-cli-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("marker.txt");

    // 用 serde 构造 SSE 帧(避免手写 JSON 转义)
    let tool_args = serde_json::to_string(
        &json!({
            "command": format!("echo done > {} && echo ran-ok", marker.display()),
            "description": "Write marker file and echo confirmation",
        }),
    )
    .unwrap();
    let first = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({ "choices": [ { "delta": { "tool_calls": [ {
            "index": 0, "id": "c1", "type": "function",
            "function": { "name": "bash", "arguments": tool_args }
        } ] } } ] }),
        json!({ "choices": [ { "delta": {}, "finish_reason": "tool_calls" } ] }),
    );
    let second = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({ "choices": [ { "delta": { "content": "command finished" } } ] }),
        json!({ "choices": [ { "delta": {}, "finish_reason": "stop" } ] }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = Arc::clone(&captured);
    tokio::spawn(async move {
        for response in [first, second] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let n = socket.read(&mut buf).await.unwrap();
            let raw = String::from_utf8_lossy(&buf[..n]).to_string();
            let body_start = raw.find("\r\n\r\n").expect("body") + 4;
            if let Ok(v) = serde_json::from_str::<Value>(&raw[body_start..]) {
                cap.lock().unwrap().push(v);
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{response}"
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });

    let transport = HttpTransport::new(ProviderConfig {
        base_url: format!("http://{addr}"),
        api_key: "sk-test".into(),
        stream_mode: StreamMode::Sse,
    })
    .unwrap();
    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(
        RequestHeader {
            model: "test".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: Vec::new(),
        },
        Arc::clone(&log),
    );
    let mut gate = InvariantGate::new(transport, Arc::clone(&log));
    let mut bash = BashTool::new(&dir);
    let jsonl = JsonlBackend::create(dir.join("e2e.jsonl")).unwrap();
    let clock = || 0_i64;
    let mut sink = |ev: &EventEnvelope| {
        jsonl.append(ev).unwrap();
    };

    let outcome = engine
        .run_turn(
            "create the marker",
            None,
            &[],
            &[],
            &mut gate,
            &mut bash,
            &clock,
            &mut sink,
        )
        .await
        .expect("turn");

    // 沙箱真实执行:marker 落在 workspace 内
    assert_eq!(outcome.assistant_message, "command finished");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "done");

    // 请求体:两次都带 tools 声明;第二次为 wire 方言
    //(assistant tool_calls 嵌套化、tool 消息转 tool_call_id/content)
    let requests = captured.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["tools"][0]["function"]["name"], "bash");
    assert_eq!(requests[0]["tool_choice"], "auto");
    let second_messages = requests[1]["messages"].as_array().unwrap().clone();
    assert_eq!(second_messages[0]["role"], "user");
    assert_eq!(second_messages[1]["role"], "assistant");
    assert_eq!(
        second_messages[1]["tool_calls"][0]["function"]["name"],
        "bash"
    );
    assert_eq!(second_messages[1]["tool_calls"][0]["id"], "c1");
    assert_eq!(second_messages[2]["role"], "tool");
    assert_eq!(
        second_messages[2]["tool_call_id"], "c1",
        "provider call id 回传"
    );
    assert_eq!(
        second_messages[2]["content"], "ran-ok",
        "工具输出经日志派生回传"
    );

    // 日志:工具往返 + 审计归因
    let l = log.lock().unwrap();
    let types: Vec<&str> = l.iter().map(|e| e.r#type.as_str()).collect();
    assert!(types.contains(&"tool/call"));
    assert!(types.contains(&"tool/result"));
    assert_eq!(
        types.iter().filter(|t| **t == "audit/call").count(),
        6,
        "llm ×2(意图+完成)+ tool ×1(意图+完成)审计"
    );
}

/// specs 注入:engine turn 开始时 ToolPort::specs 覆盖 header 的工具声明
#[tokio::test]
async fn fake_provider_still_works_with_tools_specs() {
    // specs 注入 header:engine turn 开始时工具声明进入出网请求
    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(
        RequestHeader {
            model: "t".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: vec![json!({ "name": "stale" })],
        },
        Arc::clone(&log),
    );
    let mut provider = FakeProvider::new();
    provider.then(vec![dsh_agent_loop::LlmEvent::AssistantMessage(
        json!({ "content": "ok" }),
    )]);
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let dir = std::env::temp_dir().join(format!("dsh-specs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut bash = BashTool::new(&dir);
    let mut sink = |_ev: &EventEnvelope| {};
    engine
        .run_turn(
            "hi",
            None,
            &[],
            &[],
            &mut gate,
            &mut bash,
            &|| 0_i64,
            &mut sink,
        )
        .await
        .unwrap();
    // header.tools 被ToolPort::specs 覆盖(header 里的 stale 声明不残留)
    let (h, _) = &gate.inner().received[0];
    assert_eq!(h.tools.len(), 1);
    assert_eq!(h.tools[0]["function"]["name"], "bash");
}
