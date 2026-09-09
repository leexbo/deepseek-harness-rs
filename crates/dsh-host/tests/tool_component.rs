//! wasm 工具组件契约测试:WasmTool 桥装载/声明/执行/校验/硬停。
//!
//! 产物定位照 session_component 模式:环境变量覆盖或缺失现场构建,
//! 保证 `cargo test --workspace` 自洽。

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use dsh_agent_loop::{CancelToken, ToolCallRequest, ToolPort as _};
use dsh_host::WasmTool;

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

fn config() -> serde_json::Value {
    serde_json::json!({ "label": "test-lab" })
}

#[tokio::test]
async fn loads_describes_and_executes() {
    let mut tool = WasmTool::new(&example_wasm(), &config(), CancelToken::new())
        .expect("装载示例组件");

    // describe → OpenAI function 声明(装载期缓存)
    let specs = tool.specs();
    let names: Vec<&str> = specs
        .iter()
        .map(|s| s["function"]["name"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(names, vec!["echo_config", "spin"], "声明两工具");
    let echo = &specs[0];
    assert_eq!(echo["type"], "function");
    assert!(echo["function"]["parameters"]["properties"]["message"].is_object(),
        "input-schema 透传:{echo}");

    // execute json→json 往返(config 回显)
    let out = tool
        .execute(&ToolCallRequest {
            name: "echo_config".into(),
            arguments: serde_json::json!({ "message": "hello" }),
        })
        .await;
    assert!(out.success, "执行成功:{}", out.output);
    let value: serde_json::Value = serde_json::from_str(&out.output).expect("输出是 JSON");
    assert_eq!(value["config"]["label"], "test-lab", "config 透传进组件");
    assert_eq!(value["input"]["message"], "hello", "入参往返");

    // 未知工具名 → 组件级失败
    let out = tool
        .execute(&ToolCallRequest {
            name: "nope".into(),
            arguments: serde_json::json!({}),
        })
        .await;
    assert!(!out.success);
    assert!(out.output.contains("unknown tool"), "{}", out.output);
}

#[test]
fn config_schema_rejects_missing_required() {
    // 组件 config-schema 声明 label 必填:缺 label 在装载期拒绝(类型即校验)
    let Err(err) = WasmTool::new(
        &example_wasm(),
        &serde_json::json!({ "other": true }),
        CancelToken::new(),
    ) else {
        panic!("缺 required 应装载期拒绝");
    };
    assert!(
        err.to_string().contains("校验失败"),
        "应报 config 校验失败:{err}"
    );
}

#[tokio::test]
async fn spin_hard_stopped_by_epoch_budget() {
    // 死循环组件:epoch 预算(50 epochs ≈ 1s)到期 trap(E4 硬停兜底)
    let mut tool = WasmTool::new(&example_wasm(), &config(), CancelToken::new())
        .expect("装载")
        .with_epoch_budget(50);
    let started = Instant::now();
    let out = tool
        .execute(&ToolCallRequest {
            name: "spin".into(),
            arguments: serde_json::json!({ "note": "x" }),
        })
        .await;
    assert!(!out.success, "spin 应失败");
    assert!(
        !out.output.is_empty(),
        "trap 错误应带回执(wasm backtrace/interrupt):{}",
        out.output
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "硬停应在预算内(实际 {:?})",
        started.elapsed()
    );
}

#[tokio::test]
async fn cancel_abandons_wait_immediately() {
    // 取消 = 放弃等待即时返回 cancelled(不推全局 epoch,防误伤并发调用;
    // 组件本体在小预算内被硬停收尾)
    let cancel = CancelToken::new();
    let mut tool = WasmTool::new(&example_wasm(), &config(), cancel.clone())
        .expect("装载")
        .with_epoch_budget(50);
    let started = Instant::now();
    let handle = tokio::spawn(async move {
        tool.execute(&ToolCallRequest {
            name: "spin".into(),
            arguments: serde_json::json!({}),
        })
        .await
    });
    // 给 spawn_blocking 一点启动时间再取消,确保 select 走 cancel 分支
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let out = handle.await.expect("join");
    assert_eq!(out.output, "cancelled");
    assert!(!out.success);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "取消应即时返回(实际 {:?})",
        started.elapsed()
    );
}
