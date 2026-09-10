//! 集成测试:本地 fixture MCP server(python3,JSON-RPC over stdio)走
//! McpServerPort 完整链路——连接 → 工具发现 → 调用回填 → isError 降级。

use dsh_agent_loop::CancelToken;
use dsh_agent_loop::tools::{ToolCallRequest, ToolPort};
use dsh_mcp::{McpServerConfig, McpServerPort};
use serde_json::json;

const FIXTURE: &str = r#"
import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    m = json.loads(line)
    if "id" not in m:
        continue
    method = m.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "0"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"tools": [
            {"name": "echo", "description": "回声",
             "inputSchema": {"type": "object"}}]}})
    elif method == "tools/call":
        args = (m.get("params") or {}).get("arguments") or {}
        send({"jsonrpc": "2.0", "id": m["id"], "result": {
            "content": [{"type": "text", "text": "pong"}],
            "isError": bool(args.get("fail"))}})
"#;

fn start_port(fail: bool) -> McpServerPort {
    let dir = std::env::temp_dir().join(format!("dsh-mcp-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("fixture.py");
    std::fs::write(&script, FIXTURE).unwrap();
    let config = McpServerConfig {
        server_name: "fixture".into(),
        command: "python3".into(),
        args: vec![script.display().to_string()],
        env: Default::default(),
        cwd: None,
        tool_call_timeout: std::time::Duration::from_secs(10),
    };
    let _ = fail;
    McpServerPort::start(config, CancelToken::new(), None)
}

async fn wait_tools(port: &mut McpServerPort) {
    for _ in 0..50 {
        if !port.specs().is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("工具未在期限内被发现");
}

#[tokio::test]
async fn stdio_fixture_connect_discover_and_call() {
    let mut port = start_port(false);
    wait_tools(&mut port).await;
    assert_eq!(
        port.specs()[0]["function"]["name"],
        "mcp__fixture__echo",
        "公共名照源 mcp__<server>__<raw>"
    );
    let out = ToolPort::execute(
        &mut port,
        &ToolCallRequest {
            name: "mcp__fixture__echo".into(),
            arguments: json!({"msg": "hi"}),
        },
    )
    .await;
    assert!(out.success, "{:?}", out.output);
    assert_eq!(out.output, "pong");
}

/// isError = true → 工具失败结果(内容仍投影,供模型诊断)
#[tokio::test]
async fn stdio_fixture_is_error_marks_failure() {
    let mut port = start_port(true);
    wait_tools(&mut port).await;
    let out = ToolPort::execute(
        &mut port,
        &ToolCallRequest {
            name: "mcp__fixture__echo".into(),
            arguments: json!({"fail": true}),
        },
    )
    .await;
    assert!(!out.success, "isError 应转失败:{:?}", out.output);
    assert_eq!(out.output, "pong");
}

/// cancel → 停机:service 被取走取消,后续调用报「未连接」(不再等超时)
#[tokio::test]
async fn stdio_fixture_cancel_shuts_down() {
    let mut port = start_port(false);
    wait_tools(&mut port).await;
    port.shutdown();
    // 停机是异步清理:轮询到调用不再成功(service 已取消)
    for _ in 0..50 {
        let out = ToolPort::execute(
            &mut port,
            &ToolCallRequest {
                name: "mcp__fixture__echo".into(),
                arguments: json!({}),
            },
        )
        .await;
        if !out.success {
            assert!(
                out.output == "cancelled"
                    || out.output.contains("未连接")
                    || out.output.contains("不可用"),
                "{:?}",
                out.output
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("cancel 后调用仍成功,停机未生效");
}
