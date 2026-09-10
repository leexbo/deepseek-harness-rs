//! preset 装载外部 wasm 工具组件的端到端:workspace manifest → build_tools
//! → 模型面声明 + 执行往返。
//!
//! 引擎的调用管线由既有 13 工具的 e2e 锁定,此处锁「manifest 路径型
//! source → WasmTool 装载 → 声明进 ToolSet → 执行出结果」整条装载链。

use std::path::{Path, PathBuf};
use std::process::Command;

use dsh_agent_loop::{CancelToken, ToolCallRequest, ToolPort as _};
use dsh_app::{Resolved, build_tools, fresh_log};

/// 定位示例组件产物;缺失时现场构建(保证 workspace 测试自洽)
fn example_wasm() -> PathBuf {
    if let Ok(p) = std::env::var("DSH_EXAMPLE_TOOL_WASM") {
        return PathBuf::from(p);
    }
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/debug/dsh_example_tool.wasm");
    if !p.exists() {
        let status = Command::new("cargo")
            .args(["build", "-p", "dsh-example-tool", "--target", "wasm32-wasip2"])
            .status()
            .expect("spawn cargo");
        assert!(status.success(), "dsh-example-tool wasm 构建失败");
    }
    p
}

/// 建 temp workspace(自定义 manifest + 组件副本),返回 workspace 路径
fn workspace_with_wasm_mount() -> PathBuf {
    let ws = std::env::temp_dir().join(format!("dsh-wasm-mount-{}", std::process::id()));
    let presets = ws.join("presets");
    std::fs::create_dir_all(&presets).expect("mkdir presets");
    std::fs::copy(example_wasm(), ws.join("example.wasm")).expect("copy component");
    std::fs::write(
        presets.join("wasmtest.yaml"),
        r#"
apiVersion: dsh/v1
kind: Preset
metadata:
  name: wasmtest
  description: wasm 工具组件装载测试
spec:
  mounts:
    - source: persona
    - source: example.wasm
      config:
        label: e2e-label
"#,
    )
    .expect("write manifest");
    ws
}

fn resolved_at(ws: &Path, preset_id: &str) -> Resolved {
    let preset = dsh_host::PresetManifest::load(ws, preset_id).expect("加载 manifest");
    Resolved {
        model: "m".into(),
        base_url: "https://example.invalid".into(),
        session: ws.join("s.jsonl").display().to_string(),
        workspace: ws.to_path_buf(),
        dialect: "openai-chat".into(),
        reasoning_effort: None,
        models: None,
        preset,
    }
}

#[tokio::test]
async fn preset_mounts_external_wasm_tool() {
    let ws = workspace_with_wasm_mount();
    let resolved = resolved_at(&ws, "wasmtest");
    let log = fresh_log();
    let mut tools = build_tools(
        &resolved,
        "key",
        &log,
        &CancelToken::new(),
        false,
        "workspace-write",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("装配");

    // 模型面声明:外部组件的工具进 ToolSet
    let specs = tools.specs();
    assert!(
        specs.iter().any(|s| s["function"]["name"] == "echo_config"),
        "外部组件工具应声明:{specs:?}"
    );

    // 执行往返:config(label)经 init 透传进组件
    let out = tools        .execute(&ToolCallRequest {
            name: "echo_config".into(),
            arguments: serde_json::json!({ "message": "hi" }),
        })
        .await;
    assert!(out.success, "执行成功:{}", out.output);
    let value: serde_json::Value = serde_json::from_str(&out.output).expect("JSON 输出");
    assert_eq!(value["config"]["label"], "e2e-label", "manifest config 透传");
}

#[tokio::test]
async fn broken_wasm_mount_fails_fast_at_assembly() {
    // manifest 指向不存在/非组件文件 → 装配期 fail-fast
    let ws = std::env::temp_dir().join(format!("dsh-wasm-broken-{}", std::process::id()));
    let presets = ws.join("presets");
    std::fs::create_dir_all(&presets).expect("mkdir");
    std::fs::write(ws.join("not-a-component.wasm"), b"garbage").expect("写非组件文件");
    std::fs::write(
        presets.join("broken.yaml"),
        "apiVersion: dsh/v1\nkind: Preset\nmetadata:\n  name: broken\n  description: d\nspec:\n  mounts:\n    - source: not-a-component.wasm\n",
    )
    .expect("write manifest");
    let resolved = resolved_at(&ws, "broken");
    let log = fresh_log();
    let err = build_tools(
        &resolved,
        "key",
        &log,
        &CancelToken::new(),
        false,
        "workspace-write",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    assert!(err.is_err(), "非组件文件应装配期拒绝");
}
