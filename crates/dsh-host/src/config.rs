//! 配置子系统:TOML 主格式 + 类型化解析 + 插件配置 schema 校验。
//!
//! 「配置是纯数据」语义:无表达式求值面;
//! 类型化 serde 解析即校验(类型错误在反序列化期拒绝)。
//! 插件配置经 [`validate_config`] 按 JSON Schema 子集校验——
//! wasm 组件插件(config-schema() export)与 native 插件共用此路径。

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 配置错误
#[derive(Debug, Error, PartialEq)]
pub enum ConfigError {
    /// TOML 解析失败
    #[error("toml parse failed: {0}")]
    Parse(String),
    /// 读取失败(文件存在但不可读)
    #[error("config io: {0}")]
    Io(String),
    /// 类型不匹配/缺字段
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// dsh 宿主配置(dsh.toml;CLI 参数优先级更高)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DshConfig {
    /// 模型标识
    #[serde(default)]
    pub model: Option<String>,
    /// provider base URL
    #[serde(default)]
    pub base_url: Option<String>,
    /// 会话日志路径
    #[serde(default)]
    pub session: Option<String>,
    /// 工作目录(沙箱可写根/工具 cwd)
    #[serde(default)]
    pub workspace: Option<String>,
    /// provider 方言(openai-chat / anthropic / openai-responses;
    /// 缺省 openai-chat;未知值由 adapter_by_name fail-fast)
    #[serde(default)]
    pub dialect: Option<String>,
    /// 能力 preset 标识(模型面工具开关;缺省 standard)
    #[serde(default)]
    pub preset: Option<String>,
    /// 推理等级(low / high / max;缺省 = provider 默认)
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// 可选模型清单(模型选择菜单;缺省 = 内置默认)
    #[serde(default)]
    pub models: Option<Vec<String>>,
}

impl DshConfig {
    /// 从 TOML 文本解析(纯数据;类型即校验)
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// 加载配置文件:文件不存在 = 空配置(非错误);存在但解析失败 = 错误。
    /// 合并优先级由调用方决定(CLI 参数 > 配置文件 > 默认)。
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e.to_string())),
        }
    }

    /// DshConfig 的 JSON Schema(插件 SDK 文档/工具链用)
    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "model": { "type": "string" },
                "base_url": { "type": "string" },
                "session": { "type": "string" },
                "workspace": { "type": "string" },
                "dialect": { "type": "string" },
            },
        })
    }
}

/// 按 JSON Schema 子集校验插件配置。
///
/// 支持子集:`type`(object/string/number/boolean/array)、`properties`、
/// `required`。这是 native 阶段的契约面;完整 JSON Schema
/// (enum/format/组合词)在插件组件化时评估引入 jsonschema crate。
pub fn validate_config(
    schema: &serde_json::Value,
    config: &serde_json::Value,
) -> Result<(), ConfigError> {
    type_matches(schema, config).map_err(ConfigError::Invalid)
}

fn type_matches(schema: &serde_json::Value, value: &serde_json::Value) -> Result<(), String> {
    let expected = schema["type"].as_str().unwrap_or("object");
    let ok = match expected {
        "object" => value.is_object(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        other => return Err(format!("schema 使用了未支持的类型:{other}")),
    };
    if !ok {
        return Err(format!("类型不匹配:期望 {expected},得到 {value}"));
    }
    if expected == "object" {
        let obj = value.as_object().expect("checked");
        for key in schema["required"].as_array().unwrap_or(&Vec::new()) {
            let key = key.as_str().expect("required 数组元素必须是字符串");
            if !obj.contains_key(key) {
                return Err(format!("缺少必填字段:{key}"));
            }
        }
        if let Some(properties) = schema["properties"].as_object() {
            for (key, sub) in properties {
                if let Some(actual) = obj.get(key) {
                    type_matches(sub, actual)?;
                }
            }
        }
    }
    if expected == "array"
        && let (Some(item_schema), Some(items)) = (schema["items"].as_object(), value.as_array())
    {
        let item_schema = serde_json::Value::Object(item_schema.clone());
        for item in items {
            type_matches(&item_schema, item)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_roundtrip() {
        let cfg = DshConfig::from_toml("model = \"deepseek-chat\"\nsession = \"s.jsonl\"\n")
            .expect("parse");
        assert_eq!(cfg.model.as_deref(), Some("deepseek-chat"));
        assert_eq!(cfg.session.as_deref(), Some("s.jsonl"));
    }

    #[test]
    fn type_error_rejected() {
        // 类型即校验:model 必须是字符串
        assert!(DshConfig::from_toml("model = 42").is_err());
    }

    #[test]
    fn load_missing_file_is_empty_config() {
        let cfg = DshConfig::load(std::path::Path::new("/nonexistent/dsh.toml")).unwrap();
        assert_eq!(cfg, DshConfig::default());
    }

    #[test]
    fn load_parses_existing_file() {
        let dir = std::env::temp_dir().join(format!("dsh-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dsh.toml");
        std::fs::write(&path, "model = \"m\"\nworkspace = \"/tmp/w\"\n").unwrap();
        let cfg = DshConfig::load(&path).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("m"));
        assert_eq!(cfg.workspace.as_deref(), Some("/tmp/w"));
    }

    #[test]
    fn plugin_config_schema_validation() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["retries"],
            "properties": {
                "retries": { "type": "number" },
                "label": { "type": "string" },
            },
        });
        // 合法
        assert!(
            validate_config(&schema, &serde_json::json!({ "retries": 3, "label": "x" })).is_ok()
        );
        // 缺必填
        assert!(validate_config(&schema, &serde_json::json!({ "label": "x" })).is_err());
        // 类型不匹配
        assert!(validate_config(&schema, &serde_json::json!({ "retries": "three" })).is_err());
    }
}
