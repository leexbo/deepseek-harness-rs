//! 集成测试:streamable-http 传输(python3 stdlib 起本地 MCP HTTP 端点)。
//! 验证:url 连接 + 工具发现 + 调用;配置 headers 原样到达 server
//! (Authorization 经 `echo_auth` 工具回显断言);URL 无效即时失败。

use dsh_agent_loop::CancelToken;
use dsh_agent_loop::tools::{ToolCallRequest, ToolPort};
use dsh_mcp::{McpServerConfig, McpServerPort, McpTransport};
use serde_json::json;
use std::collections::BTreeMap;

/// 最小 streamable-http fixture:POST /mcp 收 JSON-RPC;initialize 回
/// 结果 + mcp-session-id 头;无 id(通知)回 202;tools/* 回工具清单与
/// 调用结果(echo_auth 回显收到的 Authorization);GET → 405(照 MCP
/// spec:不提供 SSE 流的 server 返回 405,client 须容忍)
const FIXTURE: &str = r#"
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LAST_AUTH = ["(none)"]

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def _send_json(self, code, obj, extra_headers=None):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for k, v in (extra_headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        self.send_response(405)
        self.end_headers()
    def do_DELETE(self):
        self.send_response(200)
        self.end_headers()
    def do_POST(self):
        auth = self.headers.get("Authorization")
        if auth:
            LAST_AUTH[0] = auth
        length = int(self.headers.get("Content-Length", "0"))
        try:
            m = json.loads(self.rfile.read(length) or b"{}")
        except Exception:
            self._send_json(400, {"jsonrpc": "2.0", "error": {"code": -32700, "message": "parse error"}})
            return
        method = m.get("method")
        if "id" not in m:
            self.send_response(202)
            self.end_headers()
            return
        if method == "initialize":
            self._send_json(200, {"jsonrpc": "2.0", "id": m["id"], "result": {
                "protocolVersion": m.get("params", {}).get("protocolVersion", "2024-11-05"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "http-fixture", "version": "0"}}},
                {"mcp-session-id": "fixture-session"})
        elif method == "tools/list":
            self._send_json(200, {"jsonrpc": "2.0", "id": m["id"], "result": {"tools": [
                {"name": "echo_auth", "description": "回显收到的 Authorization",
                 "inputSchema": {"type": "object"}}]}})
        elif method == "tools/call":
            self._send_json(200, {"jsonrpc": "2.0", "id": m["id"], "result": {
                "content": [{"type": "text", "text": "auth=" + LAST_AUTH[0]}],
                "isError": False}})
        else:
            self._send_json(200, {"jsonrpc": "2.0", "id": m["id"],
                                  "error": {"code": -32601, "message": "no method"}})

port = int(sys.argv[1])
ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#;

fn start_fixture() -> (std::process::Child, u16) {
    let dir = std::env::temp_dir().join(format!("dsh-mcp-http-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("http_fixture.py");
    std::fs::write(&script, FIXTURE).unwrap();
    // 先占端口再传给 python(避免竞态):绑 0 拿实际端口后释放
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let child = std::process::Command::new("python3")
        .arg(&script)
        .arg(port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("python3 可用");
    (child, port)
}

fn http_config(url: String, headers: BTreeMap<String, String>) -> McpServerConfig {
    McpServerConfig {
        server_name: "httpsrv".into(),
        transport: McpTransport::StreamableHttp { url, headers },
        tool_call_timeout: std::time::Duration::from_secs(10),
    }
}

async fn wait_tools(port: &mut McpServerPort) {
    for _ in 0..80 {
        if !port.specs().is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("HTTP 工具未在期限内被发现");
}

#[tokio::test]
async fn http_connect_discover_call_and_headers_passthrough() {
    let (mut child, port) = start_fixture();
    let mut headers = BTreeMap::new();
    headers.insert(
        "Authorization".to_string(),
        "Bearer e2e-test-token".to_string(),
    );
    let mut mcp_port = McpServerPort::start(
        http_config(format!("http://127.0.0.1:{port}/mcp"), headers),
        CancelToken::new(),
        None,
        None,
    );
    // 测试退出兜底杀 fixture
    let guard = KillGuard(&mut child);
    wait_tools(&mut mcp_port).await;
    assert_eq!(
        mcp_port.specs()[0]["function"]["name"],
        "mcp__httpsrv__echo_auth",
        "公共名照源 mcp__<server>__<raw>"
    );
    let out = ToolPort::execute(
        &mut mcp_port,
        &ToolCallRequest {
            name: "mcp__httpsrv__echo_auth".into(),
            arguments: json!({}),
        },
    )
    .await;
    assert!(out.success, "{}", out.output);
    // 配置 headers 原样到达 server(照源:headers dict 不 scrub 不改写)
    assert_eq!(
        out.output, "auth=Bearer e2e-test-token",
        "Authorization 原样透传"
    );
    drop(guard);
    mcp_port.shutdown();
}

#[tokio::test]
async fn http_invalid_url_fails_fast_with_status() {
    let mcp_port = McpServerPort::start(
        http_config("not a url".into(), BTreeMap::new()),
        CancelToken::new(),
        None,
        None,
    );
    // 无效 URL 在构建传输时即失败 → 状态 Failed(带文案),工具零声明
    for _ in 0..50 {
        if matches!(mcp_port.status(), dsh_mcp::McpStatus::Failed(_)) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("无效 URL 应快速落 Failed");
}

/// 测试退出兜底:杀 fixture 子进程
struct KillGuard<'a>(&'a mut std::process::Child);
impl Drop for KillGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
