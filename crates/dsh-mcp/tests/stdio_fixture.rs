//! 集成测试:本地 fixture MCP server(python3,JSON-RPC over stdio)走
//! McpServerPort 完整链路——连接 → 工具发现 → 调用回填 → isError 降级 →
//! 结果图片经准入链落存为持久引用(base64 不进模型面)。

use std::sync::{Arc, Mutex};

use dsh_agent_loop::CancelToken;
use dsh_agent_loop::tools::{ToolCallRequest, ToolPort};
use dsh_mcp::{BridgeImageInput, ImageStorePort, McpServerConfig, McpServerPort, McpTransport};
use dsh_session::attachments::ImageAttachmentRef as Ref;
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
             "inputSchema": {"type": "object"}},
            {"name": "image", "description": "返回一张 PNG 图片",
             "inputSchema": {"type": "object"}},
            {"name": "badimage", "description": "返回非 canonical base64 的图片",
             "inputSchema": {"type": "object"}}]}})
    elif method == "tools/call":
        params = m.get("params") or {}
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "image":
            # "hello1234" 的 canonical base64
            send({"jsonrpc": "2.0", "id": m["id"], "result": {
                "content": [
                    {"type": "text", "text": "截图如下"},
                    {"type": "image", "data": "aGVsbG8xMjM0", "mimeType": "image/png"}],
                "isError": False}})
        elif name == "badimage":
            send({"jsonrpc": "2.0", "id": m["id"], "result": {
                "content": [{"type": "image", "data": "a-b_ 不是 base64", "mimeType": "image/png"}],
                "isError": False}})
        else:
            send({"jsonrpc": "2.0", "id": m["id"], "result": {
                "content": [{"type": "text", "text": "pong"}],
                "isError": bool(args.get("fail"))}})
"#;

/// 记录式存储:全收(伪造引用)或按开关拒绝
struct RecordingStore {
    saved: Mutex<Vec<BridgeImageInput>>,
    fail: bool,
}
impl RecordingStore {
    fn new(fail: bool) -> Arc<Self> {
        Arc::new(Self {
            saved: Mutex::new(Vec::new()),
            fail,
        })
    }
}
impl ImageStorePort for RecordingStore {
    fn save(&self, images: Vec<BridgeImageInput>) -> Result<Vec<Ref>, String> {
        if self.fail {
            return Err("存储拒绝(测试)".into());
        }
        self.saved.lock().unwrap().extend(images);
        Ok(vec![Ref {
            attachment_id: format!("sha256:{}", "a".repeat(64)),
            media_type: dsh_session::attachments::ImageMediaType::Png,
            bytes: 9,
            width: 1,
            height: 1,
            name: None,
        }])
    }
}

fn start_port(store: Option<Arc<RecordingStore>>) -> McpServerPort {
    let dir = std::env::temp_dir().join(format!("dsh-mcp-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("fixture.py");
    std::fs::write(&script, FIXTURE).unwrap();
    let config = McpServerConfig {
        server_name: "fixture".into(),
        transport: McpTransport::Stdio {
            command: "python3".into(),
            args: vec![script.display().to_string()],
            env: Default::default(),
            cwd: None,
        },
        tool_call_timeout: std::time::Duration::from_secs(10),
    };
    McpServerPort::start(config, CancelToken::new(), None, store.map(|s| s as _))
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
    let mut port = start_port(None);
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
    let mut port = start_port(None);
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

/// 图片桥全链:image 工具结果 → 准入落存 → ToolOutput.images 持久引用,
/// base64 不出现在模型面文本;坏图整批降级诊断
#[tokio::test]
async fn stdio_fixture_image_bridge_and_degradation() {
    let store = RecordingStore::new(false);
    let mut port = start_port(Some(Arc::clone(&store)));
    wait_tools(&mut port).await;

    let out = ToolPort::execute(
        &mut port,
        &ToolCallRequest {
            name: "mcp__fixture__image".into(),
            arguments: json!({}),
        },
    )
    .await;
    assert!(out.success, "{}", out.output);
    assert_eq!(out.output, "截图如下");
    assert_eq!(out.images.len(), 1, "图片引用随结果上行");
    assert_eq!(
        out.images[0].attachment_id,
        format!("sha256:{}", "a".repeat(64))
    );
    assert_eq!(store.saved.lock().unwrap().len(), 1, "一张图已落存");
    assert!(!out.output.contains("aGVsbG8xMjM0"), "base64 不进模型面");

    // 坏图:整批降级,零落存
    let out = ToolPort::execute(
        &mut port,
        &ToolCallRequest {
            name: "mcp__fixture__badimage".into(),
            arguments: json!({}),
        },
    )
    .await;
    assert!(out.success);
    assert!(out.images.is_empty());
    assert!(
        out.output
            .contains("the image data is not canonical base64"),
        "{}",
        out.output
    );
    assert_eq!(store.saved.lock().unwrap().len(), 1, "坏图未落存");
}

/// cancel → 停机:peer 被清,后续调用报「未连接」(不再等超时)
#[tokio::test]
async fn stdio_fixture_cancel_shuts_down() {
    let mut port = start_port(None);
    wait_tools(&mut port).await;
    port.shutdown();
    // 停机是异步清理:轮询到调用不再成功(peer 已取消)
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
