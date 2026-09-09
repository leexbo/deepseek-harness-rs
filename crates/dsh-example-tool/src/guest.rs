//! 示例工具组件的 guest 实现:lifecycle + tools 接口。
//!
//! 状态经 `static Mutex` 持有(init 时的 config):单实例单 Store,
//! 宿主装载器即隔离边界(与 dsh-session guest 同模式)。

use std::sync::Mutex;

use crate::exports::dsh::plugin::lifecycle;
use crate::exports::dsh::tools::tools;

static CONFIG: Mutex<Option<serde_json::Value>> = Mutex::new(None);

/// 读 init 时的 config(guest 信任宿主已按 config-schema 校验)
fn config() -> Result<serde_json::Value, String> {
    CONFIG
        .lock()
        .map_err(|_| "config lock poisoned (guest bug)".to_string())?
        .clone()
        .ok_or_else(|| "not initialized: lifecycle.init first".to_string())
}

/// 组件导出实现
pub struct ExampleToolComponent;

impl lifecycle::Guest for ExampleToolComponent {
    fn init(config: Vec<u8>) -> Result<(), String> {
        let value: serde_json::Value = serde_json::from_slice(&config)
            .map_err(|e| format!("config is not valid JSON: {e}"))?;
        // guest 侧再验一次 required(纵深防御;宿主已按 config-schema 校验)
        if value["label"].as_str().is_none() {
            return Err("config missing required field: label".to_string());
        }
        let mut slot = CONFIG
            .lock()
            .map_err(|_| "config lock poisoned (guest bug)".to_string())?;
        *slot = Some(value);
        Ok(())
    }

    async fn dispose() -> Result<(), String> {
        // 本组件无持久资源(纯函数工具):config 丢弃即可
        let mut slot = CONFIG
            .lock()
            .map_err(|_| "config lock poisoned (guest bug)".to_string())?;
        *slot = None;
        Ok(())
    }

    fn config_schema() -> Result<Vec<u8>, String> {
        Ok(serde_json::to_vec(&serde_json::json!({
            "type": "object",
            "required": ["label"],
            "properties": {
                "label": { "type": "string" },
            },
        }))
        .expect("schema 序列化不可失败"))
    }
}

impl tools::Guest for ExampleToolComponent {
    fn describe() -> Vec<tools::ToolSpec> {
        vec![
            tools::ToolSpec {
                name: "echo_config".to_string(),
                description: "Echo the component config (from lifecycle.init) and the call input.".to_string(),
                input_schema: serde_json::to_vec(&serde_json::json!({
                    "type": "object",
                    "properties": { "message": { "type": "string" } },
                }))
                .expect("schema 序列化不可失败"),
            },
            tools::ToolSpec {
                name: "spin".to_string(),
                description: "Spin forever (epoch hard-stop test material).".to_string(),
                input_schema: serde_json::to_vec(&serde_json::json!({
                    "type": "object",
                    "properties": { "note": { "type": "string" } },
                }))
                .expect("schema 序列化不可失败"),
            },
        ]
    }

    fn execute(name: String, input: Vec<u8>) -> Result<Vec<u8>, String> {
        let config = config()?;
        match name.as_str() {
            "echo_config" => {
                let input: serde_json::Value = serde_json::from_slice(&input)
                    .map_err(|e| format!("input is not valid JSON: {e}"))?;
                serde_json::to_vec(&serde_json::json!({
                    "config": config,
                    "input": input,
                }))
                .map_err(|e| format!("serialize output: {e}"))
            }
            "spin" => {
                // 纯死循环:宿主 epoch 预算到期 trap(E4 硬停的测试物料)
                loop {
                    std::hint::spin_loop();
                }
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }
}
