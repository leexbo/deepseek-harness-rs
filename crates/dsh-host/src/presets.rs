//! preset 声明式组合:k8s 形态 YAML manifest。
//!
//! preset = **组件装配清单**(mount list):每行装载一个组件,config 任意
//! 值透传(在树组件 = 构造参数;wasm 工具组件 = `lifecycle.init(config)` 前
//! 经 `config-schema()` 校验)。source 三态:在树注册名 / 本地 wasm 路径
//! (相对 workspace)/ OCI 引用(格式容纳,拉取随分发面启用)。
//!
//! manifest 形态参照 k8s:apiVersion/kind/metadata/spec 信封 + 多文档流
//! (`---` 分隔,逐文档按 kind 路由;空文档跳过)。信封不是仪式——OCI
//! 引用是分发语义,多资源形态下 kind 是文档路由键、apiVersion 是版本位、
//! metadata.name 是寻址名;preset 元数据与组件组合清单同住一个文件。
//! v1 只认单个 `kind: Preset` 文档,其余 kind
//! 拒绝。
//!
//! 分界不变:preset 只选**模型面**——组件装配
//! 与 prompt 段;registry/沙箱/持久化/模型路由(dialect/base_url/api_key)
//! 永远留在宿主面(CLI > dsh.toml > 默认),preset 触碰不到。
//!
//! 查找顺序:`<workspace>/presets/<id>.yaml`(用户自定义,可覆盖同名
//! 内置)→ 编译进二进制的内置 preset(`include_str!`,随发行版走)。

use std::path::Path;

use serde::Deserialize;

/// 内置 standard preset(presets/standard.yaml)
const BUILTIN_STANDARD: &str = include_str!("../../../presets/standard.yaml");
/// 内置 minimal preset(presets/minimal.yaml)
const BUILTIN_MINIMAL: &str = include_str!("../../../presets/minimal.yaml");

/// manifest 信封常量(v1 唯一认的取值;未知即拒绝)
pub const API_VERSION: &str = "dsh/v1";
/// manifest 信封常量
pub const KIND: &str = "Preset";

/// manifest 文档(k8s 形态信封;多文档流中每个 `---` 段一个)
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetManifest {
    /// API 版本(版本位跟资源契约走;唯一认 [`API_VERSION`])
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// 资源类型(多文档流的文档路由键;唯一认 [`KIND`])
    pub kind: String,
    /// 元数据(name 是资源寻址名,须与文件名 stem 一致)
    pub metadata: PresetMetadata,
    /// 资源本体(装配清单)
    pub spec: PresetSpec,
}

/// manifest 元数据
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetMetadata {
    /// preset 寻址名(必须与文件名 stem 一致)
    pub name: String,
    /// 展示名(源 preset.yml 的 name 角色;缺省回落 [`name`])
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
    /// 人读描述(桌面清单/设置下拉展示)
    pub description: String,
}

/// 资源本体
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetSpec {
    /// 组件装配清单:每行一个组件;未列出 = 不装载
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
}

/// 一个组件装载行
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountSpec {
    /// 组件引用三态:在树注册名(bash/files/persona/...)、本地 wasm 路径
    /// (相对 workspace;`./`、`/` 前缀或 `.wasm` 后缀)、OCI 引用
    /// (registry/name:tag@digest;拉取随分发面启用)
    pub source: String,
    /// 组件配置(任意 YAML 值;`config:` 留空归一为空对象)
    #[serde(default)]
    pub config: serde_json::Value,
}

impl MountSpec {
    /// 组件配置(归一形态:`config:` 缺省/留空 → 空 object)
    pub fn config_object(&self) -> serde_json::Value {
        if self.config.is_null() {
            serde_json::json!({})
        } else {
            self.config.clone()
        }
    }
}

impl PresetManifest {
    /// 解析 manifest 文本(多文档流:逐文档按 kind 路由;空文档跳过;
    /// v1 只认单个 `kind: Preset` 文档,多余非空文档拒绝)
    pub fn parse(text: &str, path: &str) -> Result<Self, PresetError> {
        let mut found: Option<Self> = None;
        for doc in serde_norway::Deserializer::from_str(text) {
            // 空文档(纯 `---` 段)跳过;k8s manifest 同语义
            let value = serde_norway::Value::deserialize(doc)
                .map_err(|source| PresetError::Invalid {
                    path: path.into(),
                    source,
                })?;
            if value.is_null() {
                continue;
            }
            let manifest = Self::deserialize(value)
                .map_err(|source| PresetError::Invalid {
                    path: path.into(),
                    source,
                })?;
            if found.is_some() {
                return Err(PresetError::Unsupported {
                    path: path.into(),
                    reason: "一个 preset 文件只承载一个 Preset 文档;多资源混布随分发面启用".into(),
                });
            }
            found = Some(manifest);
        }
        let manifest = found.ok_or_else(|| PresetError::Invalid {
            path: path.into(),
            source: serde::de::Error::custom("manifest 为空(无文档)"),
        })?;
        if manifest.api_version != API_VERSION {
            return Err(PresetError::Unsupported {
                path: path.into(),
                reason: format!(
                    "apiVersion {} 不受支持(唯一认 {API_VERSION})",
                    manifest.api_version
                ),
            });
        }
        if manifest.kind != KIND {
            return Err(PresetError::Unsupported {
                path: path.into(),
                reason: format!("kind {} 不受支持(唯一认 {KIND})", manifest.kind),
            });
        }
        Ok(manifest)
    }

    /// 校验寻址名:metadata.name 必须与文件名 stem(= preset id)一致
    pub fn check_name(&self, id: &str, path: &str) -> Result<(), PresetError> {
        if self.metadata.name != id {
            return Err(PresetError::NameMismatch {
                path: path.into(),
                name: self.metadata.name.clone(),
                expected: id.to_string(),
            });
        }
        Ok(())
    }

    /// 加载 preset:workspace `presets/<id>.yaml` 优先,内置兜底。
    /// name 与 id 不符即拒绝(寻址名一致性 fail-fast)
    pub fn load(workspace: &Path, id: &str) -> Result<Self, PresetError> {
        let user = workspace.join(format!("presets/{id}.yaml"));
        if user.is_file() {
            let text = std::fs::read_to_string(&user)
                .map_err(|_| PresetError::Unknown(id.to_string()))?;
            let path = user.display().to_string();
            let manifest = Self::parse(&text, &path)?;
            manifest.check_name(id, &path)?;
            return Ok(manifest);
        }
        let builtin = match id {
            "standard" => BUILTIN_STANDARD,
            "minimal" => BUILTIN_MINIMAL,
            _ => return Err(PresetError::Unknown(id.to_string())),
        };
        let manifest = Self::parse(builtin, "<builtin>")?;
        manifest.check_name(id, "<builtin>")?;
        Ok(manifest)
    }

    /// 指定 source 的 mount 行是否在场(在树组件开关判别)
    pub fn mounts(&self, source: &str) -> bool {
        self.spec.mounts.iter().any(|m| m.source == source)
    }

    /// 指定 source 的 mount 行(首个;组件行唯一性由装配器校验)
    pub fn mount(&self, source: &str) -> Option<&MountSpec> {
        self.spec.mounts.iter().find(|m| m.source == source)
    }
}

/// preset 加载错误
#[derive(Debug, thiserror::Error)]
pub enum PresetError {
    /// 未知 preset(用户与内置均未命中)
    #[error("unknown preset: {0} (looked in presets/{0}.yaml and built-ins)")]
    Unknown(String),
    /// manifest 解析失败(YAML 语法/字段类型/deny_unknown)
    #[error("invalid preset file {path}: {source}")]
    Invalid {
        /// 文件路径
        path: String,
        /// 解析错误
        source: serde_norway::Error,
    },
    /// manifest 信封不支持(未知 apiVersion/kind、多资源文档)
    #[error("unsupported manifest {path}: {reason}")]
    Unsupported {
        /// 文件路径
        path: String,
        /// 不支持原因
        reason: String,
    },
    /// 寻址名与文件不符
    #[error("preset name mismatch in {path}: metadata.name is {name}, expected {expected}")]
    NameMismatch {
        /// 文件路径
        path: String,
        /// manifest 声明的 name
        name: String,
        /// 期望 name(文件名 stem = preset id)
        expected: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dsh-preset-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn builtin_presets_parse() {
        let std_manifest = PresetManifest::parse(BUILTIN_STANDARD, "<test>").unwrap();
        assert_eq!(std_manifest.metadata.name, "standard");
        for source in [
            "persona",
            "bash",
            "files",
            "todo_write",
            "plan",
            "goal",
            "subagent",
            "jobs",
            "workflow",
            "session_query",
            "ask_user_question",
        ] {
            assert!(std_manifest.mounts(source), "{source} 应装载");
        }
        assert_eq!(std_manifest.spec.mounts.len(), 11, "standard 共 11 行");

        let minimal = PresetManifest::parse(BUILTIN_MINIMAL, "<test>").unwrap();
        assert!(minimal.mounts("persona"));
        assert!(minimal.mounts("bash"));
        assert!(minimal.mounts("files"));
        assert_eq!(minimal.spec.mounts.len(), 3, "minimal 共 3 行");
    }

    #[test]
    fn load_unknown_rejected() {
        let dir = temp("unknown");
        assert!(PresetManifest::load(&dir, "nope").is_err());
    }

    #[test]
    fn workspace_preset_overrides_builtin() {
        let dir = temp("override");
        let pdir = dir.join("presets");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("standard.yaml"),
            r#"
apiVersion: dsh/v1
kind: Preset
metadata:
  name: standard
  description: custom
spec:
  mounts:
    - source: bash
"#,
        )
        .unwrap();
        let manifest = PresetManifest::load(&dir, "standard").unwrap();
        assert_eq!(manifest.metadata.description, "custom");
        assert!(manifest.mounts("bash"));
        assert!(!manifest.mounts("files"), "自定义覆盖内置全部装载行");
    }

    #[test]
    fn persona_config_optional_and_passthrough() {
        let manifest = PresetManifest::parse(
            r#"
apiVersion: dsh/v1
kind: Preset
metadata:
  name: x
  description: d
spec:
  mounts:
    - source: persona
      config:
        identity: custom identity
        append: extra rules
    - source: bash
      config:
        timeout_ms: 300000
        nested:
          list: [1, 2, 3]
"#,
            "<test>",
        )
        .unwrap();
        let persona = manifest.mount("persona").unwrap();
        let cfg = persona.config_object();
        assert_eq!(cfg["identity"], "custom identity");
        assert_eq!(cfg["append"], "extra rules");
        let bash = manifest.mount("bash").unwrap();
        let cfg = bash.config_object();
        assert_eq!(cfg["timeout_ms"], 300000);
        assert_eq!(cfg["nested"]["list"][2], 3, "任意嵌套结构透传");

        // config 留空归一为空对象
        let bare = PresetManifest::parse(
            r#"
apiVersion: dsh/v1
kind: Preset
metadata:
  name: y
  description: d
spec:
  mounts:
    - source: files
"#,
            "<test>",
        )
        .unwrap();
        assert_eq!(
            bare.mount("files").unwrap().config_object(),
            serde_json::json!({})
        );
        assert!(bare.mount("files").unwrap().config.is_null());
    }

    #[test]
    fn deny_unknown_fields() {
        // spec 未知段 / mount 行未知键 / metadata 未知键 → 拒绝
        for text in [
            r#"
apiVersion: dsh/v1
kind: Preset
metadata:
  name: x
  description: d
spec:
  mounts:
    - source: bash
  tools: { bash: true }
"#,
            r#"
apiVersion: dsh/v1
kind: Preset
metadata:
  name: x
  description: d
  labels: { a: b }
spec:
  mounts:
    - source: bash
    disabled: false
"#,
        ] {
            assert!(PresetManifest::parse(text, "<test>").is_err(), "未知键应拒绝");
        }
    }

    #[test]
    fn multi_document_stream_routes_by_kind() {
        // 空文档跳过;两个 Preset 文档拒绝
        let dup = PresetManifest::parse(
            r#"
apiVersion: dsh/v1
kind: Preset
metadata:
  name: x
  description: d
spec:
  mounts:
    - source: bash
---
apiVersion: dsh/v1
kind: Preset
metadata:
  name: y
  description: d
spec:
  mounts: []
"#,
            "<test>",
        );
        assert!(dup.is_err(), "第二个 Preset 文档应拒绝");

        let with_blank = PresetManifest::parse(
            "---\napiVersion: dsh/v1\nkind: Preset\nmetadata:\n  name: x\n  description: d\nspec:\n  mounts: []\n---\n---\n",
            "<test>",
        )
        .unwrap();
        assert_eq!(with_blank.metadata.name, "x", "空文档跳过后正常解析");
    }

    #[test]
    fn unsupported_envelope_rejected() {
        for (text, why) in [
            (
                "apiVersion: dsh/v2\nkind: Preset\nmetadata:\n  name: x\n  description: d\nspec: {}",
                "未知 apiVersion",
            ),
            (
                "apiVersion: dsh/v1\nkind: Hook\nmetadata:\n  name: x\n  description: d\nspec: {}",
                "未知 kind",
            ),
        ] {
            assert!(PresetManifest::parse(text, "<test>").is_err(), "{why} 应拒绝");
        }
    }

    #[test]
    fn name_mismatch_rejected() {
        let dir = temp("mismatch");
        let pdir = dir.join("presets");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("alpha.yaml"),
            "apiVersion: dsh/v1\nkind: Preset\nmetadata:\n  name: beta\n  description: d\nspec: {}\n",
        )
        .unwrap();
        let err = PresetManifest::load(&dir, "alpha").unwrap_err();
        assert!(matches!(err, PresetError::NameMismatch { .. }), "name 不符应拒绝: {err}");
    }

    #[test]
    fn empty_manifest_rejected() {
        assert!(PresetManifest::parse("", "<test>").is_err());
        assert!(PresetManifest::parse("---\n---\n", "<test>").is_err());
    }
}
