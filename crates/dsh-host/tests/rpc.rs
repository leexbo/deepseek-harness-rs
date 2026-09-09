//! JSON-RPC 网关测试:dispatch 语义 + stdio 行分帧回路。
//!
//! 断言链:
//! - turn 返回 assistantMessage/seqRange,期间每个事件以 `event` 通知下行;
//! - log 返回完整日志;attribution 返回 E5 归因链(llm 审计归因到 user/message);
//! - status 反映 phase 与高水位;
//! - 未知方法 -32601、缺参数 -32602;
//! - serve_stdio:双工流上请求→通知→响应的时序,shutdown 退出循环。

use dsh_agent_loop::{LlmEvent, RequestHeader, TransportError};
use dsh_host::JsonlBackend;
use dsh_host::rpc::{Gateway, serve_stdio};
use dsh_llm::FakeProvider;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn header() -> RequestHeader {
    RequestHeader {
        model: "test-model".into(),
        system: String::new(),
        temperature: 0.0,
        reasoning_effort: None,
        tools: Vec::new(),
    }
}

fn gateway(dir: &std::path::Path, script: Vec<Vec<LlmEvent>>) -> Gateway<FakeProvider> {
    let mut provider = FakeProvider::new();
    for events in script {
        provider.then(events);
    }
    let backend = JsonlBackend::create(dir.join("rpc.jsonl")).unwrap();
    Gateway::new(header(), provider, dsh_agent_loop::NoTools, backend)
}

#[tokio::test]
async fn dispatch_turn_log_attribution_status() {
    let dir = std::env::temp_dir().join(format!("dsh-rpc-dispatch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut gw = gateway(
        &dir,
        vec![vec![
            LlmEvent::Chunk("he".into()),
            LlmEvent::Chunk("llo".into()),
            LlmEvent::AssistantMessage(json!({ "content": "hello" })),
            LlmEvent::Done,
        ]],
    );

    // turn:结果 + 事件通知下行
    let (result, notifications) = gw
        .handle("turn", &json!({ "input": "hi" }))
        .await
        .expect("turn");
    assert_eq!(result["assistantMessage"], "hello");
    assert_eq!(result["seqRange"], json!([1, 10]));
    assert_eq!(
        notifications.len(),
        10,
        "每个落日志事件一条下行通知(含 llm 完成审计)"
    );
    assert_eq!(notifications[0]["method"], "event");
    assert_eq!(notifications[0]["params"]["type"], "turn/start");

    // log:完整快照(重放材料)
    let (log_result, _) = gw.handle("log", &Value::Null).await.expect("log");
    assert_eq!(log_result["highWater"], 10);
    assert_eq!(
        log_result["events"].as_array().expect("events 数组").len(),
        10
    );

    // attribution:E5 消费面——llm 审计归因到 user/message
    let (attr, _) = gw
        .handle("attribution", &Value::Null)
        .await
        .expect("attribution");
    let chain = attr["chain"].as_array().expect("chain");
    assert_eq!(chain.len(), 2, "llm 意图+完成两条审计");
    assert_eq!(chain[0]["audit"]["boundary"], "llm");
    assert_eq!(
        chain[0]["sources"][0][1], "user/message",
        "归因链经网关可查"
    );

    // status
    let (status, _) = gw.handle("status", &Value::Null).await.expect("status");
    assert_eq!(status["highWater"], 10);
    assert_eq!(status["phase"], "Idle");

    // 错误面
    let err = gw.handle("nope", &Value::Null).await.expect_err("未知方法");
    assert_eq!(err.code, -32601);
    let err = gw
        .handle("turn", &json!({ "input": 42 }))
        .await
        .expect_err("参数类型");
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn stdio_serve_loop_with_notifications_and_shutdown() {
    let dir = std::env::temp_dir().join(format!("dsh-rpc-stdio-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let gw = gateway(
        &dir,
        vec![vec![LlmEvent::AssistantMessage(json!({ "content": "ok" }))]],
    );

    let (client, server) = tokio::io::duplex(8192);
    let (read_half, mut write_half) = tokio::io::split(client);
    let mut read_half = tokio::io::BufReader::new(read_half);
    let (server_r, server_w) = tokio::io::split(server);
    let serve = tokio::spawn(serve_stdio(gw, server_r, server_w));

    // 发 turn 请求
    let request =
        json!({ "jsonrpc": "2.0", "id": 1, "method": "turn", "params": { "input": "go" } });
    write_half
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();

    // 逐行读:7 条 event 通知先到(无 chunk 脚本),随后是响应
    let mut lines = Vec::new();
    let mut buf = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        buf.clear();
        let n = tokio::time::timeout_at(deadline.into(), read_half.read_line(&mut buf))
            .await
            .expect("读超时")
            .unwrap();
        assert!(n > 0, "serve 不应在响应前 EOF");
        let value: Value = serde_json::from_str(buf.trim()).unwrap();
        if value.get("id").is_some() {
            lines.push(value);
            break;
        }
        lines.push(value);
    }
    let notifications: Vec<&Value> = lines.iter().filter(|v| v.get("method").is_some()).collect();
    assert_eq!(notifications.len(), 8, "下行通知先于响应(含 llm 完成审计)");
    let response = lines.last().unwrap();
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["assistantMessage"], "ok");

    // shutdown:响应后退出
    write_half
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"shutdown\"}\n")
        .await
        .unwrap();
    let mut final_line = String::new();
    read_half.read_line(&mut final_line).await.unwrap();
    let done: Value = serde_json::from_str(final_line.trim()).unwrap();
    assert_eq!(done["result"]["stopping"], true);
    drop(write_half); // 关闭写端 → serve 读到 EOF(若尚未因 shutdown 退出)
    serve.await.unwrap().unwrap();
}

#[tokio::test]
async fn parse_error_and_notification_ingress() {
    let dir = std::env::temp_dir().join(format!("dsh-rpc-parse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let gw = gateway(&dir, vec![]);

    let (client, server) = tokio::io::duplex(4096);
    let (read_half, mut write_half) = tokio::io::split(client);
    let mut read_half = tokio::io::BufReader::new(read_half);
    let (server_r, server_w) = tokio::io::split(server);
    let serve = tokio::spawn(serve_stdio(gw, server_r, server_w));

    // 非法 JSON → -32700,id null
    write_half.write_all(b"not json\n").await.unwrap();
    let mut line = String::new();
    read_half.read_line(&mut line).await.unwrap();
    let err: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(err["error"]["code"], -32700);
    assert_eq!(err["id"], Value::Null);

    // 入站通知(无 id):不回应(下一响应仍是 shutdown 的)
    write_half
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"log\"}\n")
        .await
        .unwrap();
    write_half
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"shutdown\"}\n")
        .await
        .unwrap();
    let mut line = String::new();
    read_half.read_line(&mut line).await.unwrap();
    let done: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(done["id"], 7, "通知不产生响应,直接读到 shutdown 响应");

    drop(write_half);
    serve.await.unwrap().unwrap();
}

/// 挂起直到取消的 transport(测网关并发 cancel)
struct HangingTransport {
    cancel: std::sync::Mutex<dsh_agent_loop::CancelToken>,
}

impl dsh_agent_loop::Summarizer for HangingTransport {
    fn summarize<'a>(
        &'a mut self,
        _header: &'a RequestHeader,
        _messages: &'a Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        Box::pin(async { Ok(String::new()) })
    }
}

impl dsh_agent_loop::LlmTransport for HangingTransport {
    fn stream(
        &mut self,
        _header: &RequestHeader,
        _messages: &Value,
    ) -> impl std::future::Future<Output = Result<Vec<LlmEvent>, TransportError>> + Send {
        let cancel = self.cancel.lock().expect("锁中毒").clone();
        async move {
            cancel.cancelled().await; // 挂起直到网关 cancel 方法触发
            Ok(Vec::new())
        }
    }
}

#[tokio::test]
async fn gateway_cancel_interrupts_running_turn() {
    use dsh_agent_loop::CancelToken;

    let dir = std::env::temp_dir().join(format!("dsh-rpc-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let token = CancelToken::new();
    let backend = JsonlBackend::create(dir.join("cancel.jsonl")).unwrap();
    let transport = HangingTransport {
        cancel: std::sync::Mutex::new(token.clone()),
    };
    // 网关自建令牌与外部令牌不同——把外部令牌接到网关:
    // 经 handle("cancel") 走网关自己的令牌,transport 挂在网关令牌上
    let mut gw = Gateway::new(header(), transport, dsh_agent_loop::NoTools, backend);
    gw.set_cancel_token(token);

    let (client, server) = tokio::io::duplex(8192);
    let (read_half, mut write_half) = tokio::io::split(client);
    let mut read_half = tokio::io::BufReader::new(read_half);
    let (server_r, server_w) = tokio::io::split(server);
    let serve = tokio::spawn(serve_stdio(gw, server_r, server_w));

    // turn 请求(turn 后台执行,读端不被阻塞)
    write_half
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"turn\",\"params\":{\"input\":\"go\"}}\n",
        )
        .await
        .unwrap();
    // 300ms 后发 cancel
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    write_half
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"cancel\"}\n")
        .await
        .unwrap();

    // 读到两条响应:cancel 的即时响应 + turn 的取消错误(-32603 cancelled)
    let mut responses = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while responses.len() < 2 {
        let mut line = String::new();
        let n = tokio::time::timeout_at(deadline.into(), read_half.read_line(&mut line))
            .await
            .expect("读超时")
            .unwrap();
        assert!(n > 0);
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        if v.get("id").is_some() {
            responses.push(v);
        }
    }
    let cancel_resp = responses
        .iter()
        .find(|v| v["id"] == 2)
        .expect("cancel 响应");
    assert_eq!(cancel_resp["result"]["cancelled"], true);
    let turn_resp = responses.iter().find(|v| v["id"] == 1).expect("turn 响应");
    assert_eq!(
        turn_resp["error"]["code"], -32603,
        "取消的 turn 以内部错误返回: {turn_resp}"
    );
    assert!(
        turn_resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cancelled")
    );

    // shutdown 退出
    write_half
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"shutdown\"}\n")
        .await
        .unwrap();
    drop(write_half);
    serve.await.unwrap().unwrap();
}

#[tokio::test]
async fn gateway_mode_and_approve_roundtrip() {
    // mode 切换与计划批准经网关方法入日志(单边界,engine 追加);
    // 非法 mode 值 -32602;无待批准计划时 approve 拒绝
    let dir = std::env::temp_dir().join(format!("dsh-rpc-mode-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut gw = gateway(&dir, vec![]);

    // 无待批准计划
    let err = gw.handle("approve", &json!({})).await.unwrap_err();
    assert_eq!(err.code, -32602);

    // 非法 mode
    let err = gw
        .handle("mode", &json!({ "mode": "chaos" }))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);

    // 进入 plan 态:事件入日志
    let (result, _) = gw.handle("mode", &json!({ "mode": "plan" })).await.unwrap();
    assert_eq!(result["mode"], "plan");

    // 提交计划(直接 append 模拟 exit_plan_mode 的状态事件)
    {
        let log = gw.log();
        let mut l = log.lock().unwrap();
        use dsh_session::EventEnvelope;
        l.append(EventEnvelope::new(
            "plan/submitted",
            0,
            json!({ "plan": "# the plan" }),
        ))
        .unwrap();
    }
    let (result, _) = gw.handle("approve", &json!({})).await.unwrap();
    assert_eq!(result["approved"], true);

    // 日志断言:mode ×2 + submitted + approved 顺序完整
    let log = gw.log();
    let l = log.lock().unwrap();
    let tail: Vec<&str> = l
        .iter()
        .filter(|e| {
            matches!(
                e.r#type.as_str(),
                "session/mode" | "plan/submitted" | "plan/approved"
            )
        })
        .map(|e| e.r#type.as_str())
        .collect();
    assert_eq!(
        tail,
        [
            "session/mode",
            "plan/submitted",
            "plan/approved",
            "session/mode"
        ]
    );
}
