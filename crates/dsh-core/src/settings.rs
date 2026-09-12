//! 用户级设置存储。
//!
//! 承载运行时可变项:onboarding 完成态、provider 注册表([`ProviderEntry`)、
//! 工作区级默认([`WorkspaceDefaults`],projectKey 键控——setter 落盘目标)。
//! 文件为 `<DSH_RS_HOME|~/.dshrs>/settings.yaml`,与工作区 `dsh.toml`
//! 分层(合并序:内置默认 < 本设置 < 工作区 dsh.toml < 会话内存覆盖)。
//!
//! 写入原子(tmp + rename,同目录保证 POSIX 原子性);启动时损坏文件
//! 旁置备份后回落内置默认——设置可重配,不值得拒启(与 fail-closed
//! 不冲突:缺席凭据的失败发生在装配层,那里才拒绝)。运行中外部编辑
//! 经 [`SettingsStore::reload_if_changed`] 吸收:解析失败保持内存旧值
//! (编辑器半途保存不致配置清空)。

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 设置文件 schema 版本(结构性变更时递增;旧版本文件按损坏旁置,
/// 首个升级迁移需求出现时再写版本间迁移)
const SETTINGS_VERSION: u32 = 1;

/// provider 注册表条目(设置页 Models 区的管理对象)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// provider 标识(小写字母/数字/连字符;工作区默认与凭据引用按它关联)
    pub id: String,
    /// API base URL(`/models` 探测与请求同根)
    pub base_url: String,
    /// provider 方言(openai-chat / anthropic / openai-responses)
    pub dialect: String,
    /// 凭据引用(`env:NAME`;None = 走默认链,见 `credentials::resolve_credential`)
    pub credential_ref: Option<String>,
    /// 凭证明文(设置文件直存;设置页录入即写此处)
    #[serde(default)]
    pub api_key: Option<String>,
    /// 默认模型(None = 装配默认 + 探测清单回落)
    pub default_model: Option<String>,
    /// 卡片显示名(缺席 = 用 id)
    #[serde(default)]
    pub display_name: Option<String>,
    /// 用户圈定的可用模型清单(空 = 回落 `/models` 探测缓存;
    /// 聊天模型选择器读它)
    #[serde(default)]
    pub models: Vec<String>,
    /// 计费端点配置(缺席 = 不查余额/用量)
    #[serde(default)]
    pub billing: Option<BillingConfig>,
    /// 最近一次计费查询快照(持久化;重启后状态栏/卡片显示「N 小时前」)
    #[serde(default)]
    pub billing_cache: Option<BillingSnapshot>,
}

/// 计费端点配置:完全自定义 URL + JSON 提取路径(不内置适配)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BillingConfig {
    /// 展示形态:余额(金额+货币)或用量(5小时/7天 百分比+重置时间)
    pub kind: BillingKind,
    /// 查询 URL(GET;鉴权头按 provider 方言自动附带)
    pub url: String,
    /// 响应 JSON 提取路径
    pub paths: BillingPaths,
}

/// 计费展示形态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BillingKind {
    /// 余额(金额 + 货币)
    #[serde(rename = "balance")]
    Balance,
    /// 用量(5小时/7天 百分比 + 重置时间)
    #[serde(rename = "usage")]
    Usage,
}

/// JSON 提取路径集(缺席路径 = 对应项不展示)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BillingPaths {
    /// 余额金额路径,如 `balance_infos.0.total_balance`
    #[serde(default)]
    pub balance: Option<String>,
    /// 余额货币路径(缺席 = 不显示货币)
    #[serde(default)]
    pub currency: Option<String>,
    /// 5 小时用量百分比路径(0-100 数字;字符串数字也收)
    #[serde(default)]
    pub usage_5h: Option<String>,
    /// 7 天用量百分比路径
    #[serde(default)]
    pub usage_7d: Option<String>,
    /// 重置时间路径(原样字符串展示,如 `4d22h` 或 ISO 时间)
    #[serde(default)]
    pub resets: Option<String>,
}

/// 最近一次计费查询快照(持久化)。**tag = "kind"**:UI 按平铺的
/// kind 字段分流渲染(此前无 tag → 形状 {"Balance":{…}},UI 全读空 =
/// 「余额配置不生效」)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BillingSnapshot {
    /// 余额
    Balance {
        /// 抓取时刻(Unix 毫秒)
        fetched_at_ms: u64,
        /// 金额字符串(原样,如 "9.52")
        amount: String,
        /// 货币(如 "CNY";缺席 = 不显示)
        currency: Option<String>,
    },
    /// 用量
    Usage {
        /// 抓取时刻(Unix 毫秒)
        fetched_at_ms: u64,
        /// 5 小时窗口用量百分比(0-100)
        pct_5h: Option<u8>,
        /// 7 天窗口用量百分比(0-100)
        pct_7d: Option<u8>,
        /// 重置时间(原样字符串)
        resets: Option<String>,
    },
}

/// 极简 JSON 路径求值:点分段 + 数字段作数组下标
/// (`balance_infos.0.total_balance` → root["balance_infos"][0]["total_balance"])。
/// 任何一段缺失/类型不符 = None(计费展示缺席该项,不报错)
pub fn json_path<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = root;
    for seg in path.split('.') {
        if seg.is_empty() {
            return None;
        }
        match cur {
            serde_json::Value::Array(items) => {
                let ix: usize = seg.parse().ok()?;
                cur = items.get(ix)?;
            }
            serde_json::Value::Object(map) => {
                cur = map.get(seg)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}

/// 从 JSON 值取「数字百分比」:整数按 0-100 百分比;≤1 且带小数的值按
/// 占比 ×100(0.06 → 6%,两类代理的常见形态都收),字符串可带 `%`
pub fn json_percent(v: &serde_json::Value) -> Option<u8> {
    let raw: f64 = match v {
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::String(s) => s.trim().trim_end_matches('%').parse().ok()?,
        _ => return None,
    };
    let pct = if raw <= 1. && raw.fract() != 0. {
        raw * 100.
    } else {
        raw
    };
    Some(pct.clamp(0., 100.).round() as u8)
}

/// 工作区级默认(projectKey 键控;setter 落盘目标,冷装配读取)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDefaults {
    /// provider 标识(该工作区冷装配走它的 base_url/dialect/凭据)
    pub provider: Option<String>,
    /// 默认模型
    pub model: Option<String>,
    /// 默认权限预设(workspace-write / full-access;新会话 pin 时用)
    pub default_permission_preset: Option<String>,
    /// preset 标识
    pub preset: Option<String>,
    /// 推理等级(low / high / max)
    pub effort: Option<String>,
}

/// 设置文件整体
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsFile {
    /// schema 版本(读取侧不匹配即按损坏处理)
    pub version: u32,
    /// onboarding 完成态(首运行引导只在 false 时出现)
    pub onboarded: bool,
    /// provider 注册表(至少可解析出内置 deepseek;见 [`SettingsFile::provider`])
    pub providers: Vec<ProviderEntry>,
    /// 工作区默认(projectKey → 默认;缺失键 = 空默认)
    pub workspaces: HashMap<String, WorkspaceDefaults>,
    /// 工作区注册表:显示顺序的路径数组(默认工作区恒在列。
    /// 空数组 = 未初始化,宿主构造时导入旧 `.dsh-workspaces.json`)
    #[serde(default)]
    pub workspace_paths: Vec<String>,
    /// 工作区显示名覆盖(键 = basename;缺席 = 用 basename。
    /// 仅显示层——工作区身份恒为 basename,与路径绑定)
    #[serde(default)]
    pub workspace_titles: HashMap<String, String>,
    /// 运行中 Enter 行为(queue = 排队下一轮 / steer = 转向当前轮;
    /// 源 ui-conversation EnterBehaviorRow 对应物)
    #[serde(default = "default_busy_enter")]
    pub busy_enter: String,
    /// 界面语言偏好(源 locale LanguageRow 对应物;RS 现仅 zh)
    #[serde(default = "default_language")]
    pub language: String,
    /// 外观偏好(light / dark / system;源 ui-theme AppearanceRow 对应物)
    #[serde(default = "default_appearance")]
    pub appearance: String,
    /// MCP server 注册表(enabled 才会在 attach 时桥接;缺失 = 空)
    #[serde(default)]
    pub mcp_servers: Vec<McpServerEntry>,
    /// hooks 桥注册表(enabled 才会在 attach 时挂 HookPort;缺失 = 空)
    #[serde(default)]
    pub hook_bridges: Vec<HookBridgeEntry>,
}

/// hooks 桥注册表条目(源两桥插件 config 的 RS 注册表面)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HookBridgeEntry {
    /// 桥名(唯一)
    pub id: String,
    /// 方言:claude-code | codex
    pub dialect: String,
    /// hooks.json 路径(相对路径按进程启动 cwd 解析,照源)
    pub config_path: String,
    /// 是否随会话挂载
    pub enabled: bool,
    /// CC:替换 ${CLAUDE_PLUGIN_ROOT}
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_root: Option<String>,
    /// CC:替换 ${CLAUDE_PROJECT_DIR} 并注入 env(缺省 = 会话工作区)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    /// per-hook 缺省超时 ms(缺省 600000 照源)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_timeout_ms: Option<u64>,
    /// hook/result stderr 摘要上限(缺省 500 照源)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_summary_max_chars: Option<usize>,
}

impl Default for HookBridgeEntry {
    fn default() -> Self {
        Self {
            id: String::new(),
            dialect: "claude-code".to_string(),
            config_path: String::new(),
            enabled: true,
            plugin_root: None,
            project_dir: None,
            default_timeout_ms: None,
            stderr_summary_max_chars: None,
        }
    }
}

/// MCP server 注册表条目(stdio / streamable-http 双传输)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpServerEntry {
    /// server 名(工具公共名成分 `mcp__<id>__<tool>`;唯一)
    pub id: String,
    /// 是否随会话挂载
    pub enabled: bool,
    /// stdio 启动命令(http 条目留空)
    pub command: String,
    /// 启动参数
    #[serde(default)]
    pub args: Vec<String>,
    /// 附加环境变量(与清洗后的父环境合并,显式优先)
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 工作目录(缺省 = 继承)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 单次调用超时 ms(缺省 60000,照源 toolCallTimeoutMs)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_timeout_ms: Option<u64>,
    /// streamable-http endpoint(在 = http 条目,旧文件缺席 = stdio)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// http 附加请求头(原样透传;鉴权约定 = 用户自带 Authorization)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

impl McpServerEntry {
    /// 传输形态:有 url 即 streamable-http(源为显式判别字段;RS 以
    /// 「url 在场」为判别,JSON 导入侧认 transport/type 键)
    pub fn is_http(&self) -> bool {
        self.url.is_some()
    }
}

/// 解析 mcpServers JSON(兼容 Claude Code / Codex 形状):
/// `{"mcpServers": {"<名>": {"command","args","env","cwd"}}}`(stdio)或
/// `{"url","headers"}`(http;`transport`/`type` 键为 `"http"`/
/// `"streamable-http"` 显式认 http,`"sse"` 明确拒绝);单 server 亦可直接
/// 给 `{"id"|"name", ...}`。任一条目非法 → 整体拒绝(fail-closed,不做
/// 部分导入)。
pub fn parse_mcp_servers_json(text: &str) -> Result<Vec<McpServerEntry>, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("JSON 解析失败:{e}"))?;
    let map: serde_json::Map<String, Value> =
        if let Some(m) = v.get("mcpServers").and_then(|m| m.as_object()) {
            m.clone()
        } else if v.get("command").is_some()
            || v.get("url").is_some()
            || v.get("id").is_some()
            || v.get("name").is_some()
        {
            // 单 server 形态:名字取 id/name 字段;匿名报错
            let name = v
                .get("id")
                .or_else(|| v.get("name"))
                .and_then(|n| n.as_str())
                .ok_or_else(|| "单 server 形态需要 id 或 name 字段".to_string())?;
            let mut single = serde_json::Map::new();
            single.insert(name.to_string(), v.clone());
            single
        } else {
            return Err("缺少 mcpServers 映射".into());
        };
    if map.is_empty() {
        return Err("mcpServers 为空".into());
    }
    let mut out = Vec::new();
    for (name, spec) in &map {
        // 传输判定:显式 transport/type 键优先;否则 url 在场 = http
        let declared = spec
            .get("transport")
            .or_else(|| spec.get("type"))
            .and_then(|t| t.as_str())
            .map(str::to_ascii_lowercase);
        if matches!(declared.as_deref(), Some("sse")) {
            return Err(format!(
                "{name}: 暂不支持 SSE 传输(仅 stdio / streamable-http)"
            ));
        }
        let is_http = matches!(declared.as_deref(), Some("http") | Some("streamable-http"))
            || (declared.is_none() && spec.get("url").is_some());
        let url = spec.get("url").and_then(|u| u.as_str()).map(str::to_owned);
        if is_http {
            let url = url
                .filter(|u| !u.trim().is_empty())
                .ok_or_else(|| format!("{name}: http 传输需要 url"))?;
            let headers = spec
                .get("headers")
                .and_then(|h| h.as_object())
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                        .collect()
                })
                .unwrap_or_default();
            out.push(McpServerEntry {
                id: name.clone(),
                enabled: true,
                command: String::new(),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
                tool_call_timeout_ms: None,
                url: Some(url),
                headers,
            });
            continue;
        }
        let command = spec
            .get("command")
            .and_then(|c| c.as_str())
            .ok_or_else(|| format!("{name}: 缺少 command(http 传输需 url)"))?
            .to_string();
        let args = spec
            .get("args")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .map(|x| x.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let env = spec
            .get("env")
            .and_then(|e| e.as_object())
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let cwd = spec.get("cwd").and_then(|c| c.as_str()).map(str::to_owned);
        out.push(McpServerEntry {
            id: name.clone(),
            enabled: true,
            command,
            args,
            env,
            cwd,
            tool_call_timeout_ms: None,
            url: None,
            headers: BTreeMap::new(),
        });
    }
    Ok(out)
}

impl Default for McpServerEntry {
    fn default() -> Self {
        Self {
            id: String::new(),
            enabled: true,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            tool_call_timeout_ms: None,
            url: None,
            headers: BTreeMap::new(),
        }
    }
}

/// language 缺省值
fn default_language() -> String {
    "zh".into()
}

/// appearance 缺省值
fn default_appearance() -> String {
    "dark".into()
}

/// busy_enter 缺省值(排队)
fn default_busy_enter() -> String {
    "queue".into()
}

/// 内置默认 provider(凭据走默认环境变量名 `DEEPSEEK_API_KEY`)
pub fn builtin_provider() -> ProviderEntry {
    ProviderEntry {
        id: "deepseek".into(),
        base_url: "https://api.deepseek.com/v1".into(),
        dialect: "openai-chat".into(),
        credential_ref: None,
        api_key: None,
        default_model: None,
        display_name: None,
        models: Vec::new(),
        billing: None,
        billing_cache: None,
    }
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            onboarded: false,
            providers: vec![builtin_provider()],
            workspaces: HashMap::new(),
            workspace_paths: Vec::new(),
            workspace_titles: HashMap::new(),
            busy_enter: default_busy_enter(),
            language: default_language(),
            appearance: default_appearance(),
            mcp_servers: Vec::new(),
            hook_bridges: Vec::new(),
        }
    }
}

impl SettingsFile {
    /// 工作区默认(键缺失 = 空默认,不产生写入)
    pub fn workspace(&self, key: &str) -> WorkspaceDefaults {
        self.workspaces.get(key).cloned().unwrap_or_default()
    }

    /// 按 id 解析 provider:注册表命中 → 返回;缺失/None → 内置回落
    /// (保证调用方恒拿到可装配条目,引用悬空不炸装配)
    pub fn provider(&self, id: Option<&str>) -> ProviderEntry {
        if let Some(id) = id
            && let Some(found) = self.providers.iter().find(|p| p.id == id)
        {
            return found.clone();
        }
        self.providers
            .iter()
            .find(|p| p.id == "deepseek")
            .cloned()
            .unwrap_or_else(builtin_provider)
    }
}

/// 设置存储:进程内单副本(Mutex 串行化)+ 文件原子写 + 外部编辑吸收。
///
/// `open` 不写盘(打开应用不产生写副作用);首次 `update` 才落盘。
/// 运行中外部编辑器改动经 [`Self::reload_if_changed`] 吸收;`update`
/// 落盘前也会先吸收外部版本,UI 保存不覆盖外部编辑。
pub struct SettingsStore {
    path: PathBuf,
    inner: Mutex<SettingsFile>,
    /// 上次读入/写出的文件 mtime(毫秒;None = 文件尚不存在)。
    /// 外部编辑检测的快检依据
    loaded_mtime: Mutex<Option<u128>>,
}

impl SettingsStore {
    /// 打开存储:文件缺失 → 内置默认;读取/解析/版本不符 → 旁置
    /// `settings.corrupt-<ms>` 备份后用内置默认(留人工恢复路径)。
    pub fn open(path: PathBuf) -> Self {
        let file = Self::read_file(&path).unwrap_or_default();
        let loaded_mtime = file_mtime(&path);
        Self {
            path,
            inner: Mutex::new(file),
            loaded_mtime: Mutex::new(loaded_mtime),
        }
    }

    /// 读盘解析(启动路径)。损坏/版本不符 → 旁置备份后回落默认
    fn read_file(path: &std::path::Path) -> Option<SettingsFile> {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_norway::from_str::<SettingsFile>(&text) {
                Ok(f) if f.version == SETTINGS_VERSION => Some(f),
                _ => {
                    let backup = path.with_extension(format!(
                        "corrupt-{}",
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0)
                    ));
                    let _ = std::fs::rename(path, &backup);
                    eprintln!(
                        "[dsh-core] settings.yaml 损坏或版本不符,已旁置 {} 后回落默认",
                        backup.display()
                    );
                    Some(SettingsFile::default())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(SettingsFile::default()),
            Err(e) => {
                eprintln!("[dsh-core] settings.yaml 读取失败({e}),回落默认");
                Some(SettingsFile::default())
            }
        }
    }

    /// 当前快照(克隆)。落盘文件 mtime 比内存记载新时先吸收外部编辑
    /// (拉取式实时感知:打开设置页/装配点读取即最新)
    pub fn read(&self) -> SettingsFile {
        self.reload_if_changed();
        self.inner
            .lock()
            .expect("settings 锁中毒(宿主 bug)")
            .clone()
    }

    /// 外部编辑吸收:mtime 比上次读入/写出新 → 重读解析替换内存。
    /// 解析失败保持内存旧值 + 日志(编辑器半途保存不致配置清空);
    /// 旁置备份仅保留在启动 [`Self::open`] 语义
    pub fn reload_if_changed(&self) -> bool {
        let Some(mtime) = file_mtime(&self.path) else {
            return false;
        };
        let mut loaded = self.loaded_mtime.lock().expect("settings 锁中毒(宿主 bug)");
        if *loaded == Some(mtime) {
            return false;
        }
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[dsh-core] settings.yaml 重读失败({e}),保持内存旧值");
                return false;
            }
        };
        match serde_norway::from_str::<SettingsFile>(&text) {
            Ok(f) if f.version == SETTINGS_VERSION => {
                *self.inner.lock().expect("settings 锁中毒(宿主 bug)") = f;
                *loaded = Some(mtime);
                true
            }
            _ => {
                eprintln!("[dsh-core] settings.yaml 外部改动解析失败,保持内存旧值");
                false
            }
        }
    }

    /// 应用变更并原子落盘(草稿克隆上执行闭包;序列化或写盘失败时
    /// 整次更新作废——不留「盘上新内存旧」的分裂态)。落盘前先吸收
    /// 外部编辑,闭包在外部最新版上执行,UI 保存不覆盖外部改动
    pub fn update<R>(&self, f: impl FnOnce(&mut SettingsFile) -> R) -> anyhow::Result<R> {
        self.reload_if_changed();
        let mut guard = self.inner.lock().expect("settings 锁中毒(宿主 bug)");
        let mut draft = guard.clone();
        let out = f(&mut draft);
        let text = serde_norway::to_string(&draft)?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_file_name(format!(
            "{}.tmp",
            self.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("settings.yaml")
        ));
        std::fs::write(&tmp, &text)?;
        std::fs::rename(&tmp, &self.path)?;
        *guard = draft;
        *self.loaded_mtime.lock().expect("settings 锁中毒(宿主 bug)") = file_mtime(&self.path);
        Ok(out)
    }

    /// 存储路径(诊断/测试用)
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// 文件 mtime(自 epoch 的毫秒;缺失 = None)
fn file_mtime(path: &std::path::Path) -> Option<u128> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// MCP http 条目:JSON 导入(显式 transport / url 启发 / sse 拒)、
    /// serde roundtrip、旧文件缺字段兼容
    #[test]
    fn mcp_http_entries_parse_and_roundtrip() {
        // 显式 transport: http
        let entries = parse_mcp_servers_json(
            r#"{"mcpServers":{"remote":{"transport":"http","url":"https://host/mcp","headers":{"Authorization":"Bearer t"}}}}"#,
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_http());
        assert_eq!(entries[0].url.as_deref(), Some("https://host/mcp"));
        assert_eq!(
            entries[0].headers.get("Authorization").map(String::as_str),
            Some("Bearer t")
        );
        assert!(entries[0].command.is_empty());
        // url 启发(无 transport 键)
        let entries =
            parse_mcp_servers_json(r#"{"mcpServers":{"r2":{"url":"https://host/mcp"}}}"#).unwrap();
        assert!(entries[0].is_http());
        // type: sse → 明确拒绝
        let err = parse_mcp_servers_json(
            r#"{"mcpServers":{"r3":{"type":"sse","url":"https://host/sse"}}}"#,
        )
        .unwrap_err();
        assert!(err.contains("SSE"), "{err}");
        // http 缺 url → 拒
        assert!(parse_mcp_servers_json(r#"{"mcpServers":{"r4":{"transport":"http"}}}"#).is_err());
        // roundtrip:serde 落盘读回形态不变
        let text = serde_json::to_string(&entries[0]).unwrap();
        let back: McpServerEntry = serde_json::from_str(&text).unwrap();
        assert_eq!(back, entries[0]);
        // 旧文件形态(无 url/headers 字段)兼容
        let legacy: McpServerEntry = serde_json::from_value(serde_json::json!({
            "id": "old", "enabled": true, "command": "npx"
        }))
        .unwrap();
        assert!(!legacy.is_http());
        assert!(legacy.headers.is_empty());
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("dsh-settings-{tag}-{}", Uuid::new_v4().simple()))
    }

    /// 默认内容:内置 deepseek + 未 onboarding + 空工作区默认
    #[test]
    fn defaults_shape() {
        let f = SettingsFile::default();
        assert_eq!(f.version, 1);
        assert!(!f.onboarded);
        assert_eq!(f.providers, vec![builtin_provider()]);
        assert!(f.workspaces.is_empty());
        let p = f.provider(None);
        assert_eq!(p.id, "deepseek");
        assert_eq!(p.base_url, "https://api.deepseek.com/v1");
    }

    /// provider 解析:命中返回;悬空引用回落 deepseek;注册表被掏空回落内置
    #[test]
    fn provider_fallback() {
        let mut f = SettingsFile::default();
        let mut custom = builtin_provider();
        custom.id = "acme".into();
        custom.base_url = "https://acme.example/v1".into();
        f.providers.push(custom);
        assert_eq!(f.provider(Some("acme")).base_url, "https://acme.example/v1");
        assert_eq!(f.provider(Some("ghost")).id, "deepseek");
        f.providers.clear();
        assert_eq!(f.provider(None).id, "deepseek");
    }

    /// round-trip:open(缺失)默认 → update 落盘 → 重开读回
    #[test]
    fn roundtrip_and_cold_reload() {
        let path = temp_path("roundtrip");
        let store = SettingsStore::open(path.clone());
        assert_eq!(store.read(), SettingsFile::default());
        store
            .update(|s| {
                s.onboarded = true;
                s.workspaces.entry("--ws--".into()).or_default().model = Some("m1".into());
            })
            .expect("update 落盘");
        assert!(path.exists(), "首次 update 后文件应存在");
        let re = SettingsStore::open(path.clone());
        let f = re.read();
        assert!(f.onboarded);
        assert_eq!(f.workspace("--ws--").model.as_deref(), Some("m1"));
        assert_eq!(f.workspace("--other--").model, None, "键缺失 = 空默认");
        let _ = std::fs::remove_file(&path);
    }

    /// 损坏文件:旁置备份 + 回落默认,原路径不再阻塞
    #[test]
    fn corrupt_file_sidecar_and_default() {
        // 用目录包一层,保证文件名带 .yaml 后缀(与真实
        // ~/.dshrs/settings.yaml 的 with_extension 行为一致)
        let dir = temp_path("corrupt");
        std::fs::create_dir_all(&dir).expect("建目录");
        let path = dir.join("settings.yaml");
        std::fs::write(&path, "{ not yaml").expect("写损坏文件");
        let store = SettingsStore::open(path.clone());
        assert_eq!(store.read(), SettingsFile::default());
        let sidecar = dir
            .read_dir()
            .expect("读目录")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("settings.corrupt-")
            })
            .count();
        assert_eq!(sidecar, 1, "损坏文件应旁置一份备份");
        // 回落后可正常 update(覆盖损坏路径)
        store.update(|s| s.onboarded = true).expect("恢复写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 版本不符视同损坏(旁置 + 默认)
    #[test]
    fn version_mismatch_treated_as_corrupt() {
        let dir = temp_path("ver");
        std::fs::create_dir_all(&dir).expect("建目录");
        let path = dir.join("settings.yaml");
        let text = serde_norway::to_string(&SettingsFile::default())
            .expect("序列化默认")
            .replace("version: 1", "version: 99");
        std::fs::write(&path, text).expect("写旧版本文件");
        let store = SettingsStore::open(path.clone());
        assert_eq!(store.read().version, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 原子写:更新成功后无 .tmp 残留
    #[test]
    fn no_tmp_residue() {
        let path = temp_path("tmp");
        let store = SettingsStore::open(path.clone());
        store.update(|_| ()).expect("写");
        let tmp = path.with_file_name(format!(
            "{}.tmp",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
        ));
        assert!(!tmp.exists());
        let _ = std::fs::remove_file(&path);
    }

    /// 外部编辑吸收:编辑器写盘后 read() 拿到新值;解析失败保持内存
    /// 旧值(半途保存不致配置清空)
    #[test]
    fn external_edit_reloaded_on_read() {
        let path = temp_path("ext");
        let store = SettingsStore::open(path.clone());
        store
            .update(|s| {
                s.onboarded = true;
                s.providers.push(builtin_provider());
            })
            .expect("初版落盘");
        // 外部编辑器直接改文件(mtime 变)
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            &path,
            "version: 1\nonboarded: false\nworkspaces: {}\nproviders:\n  - id: acme\n    base_url: https://acme.example/v1\n    dialect: openai-chat\n    api_key: sk-ext\n",
        )
        .expect("外部写入");
        let f = store.read();
        assert!(!f.onboarded, "外部版本被吸收");
        assert_eq!(f.providers[0].api_key.as_deref(), Some("sk-ext"));
        // 解析失败 → 保持内存旧值
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "{ not yaml").expect("外部写坏");
        let f = store.read();
        assert_eq!(f.providers[0].api_key.as_deref(), Some("sk-ext"));
        assert_eq!(f.providers.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    /// update 前吸收外部版本:UI 保存不覆盖编辑器改动
    #[test]
    fn update_absorbs_external_edit_first() {
        let path = temp_path("absorb");
        let store = SettingsStore::open(path.clone());
        store.update(|s| s.onboarded = true).expect("初版落盘");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            &path,
            "version: 1\nonboarded: true\nworkspaces: {}\nproviders:\n  - id: acme\n    base_url: https://acme.example/v1\n    dialect: openai-chat\n",
        )
        .expect("外部加 provider");
        store
            .update(|s| s.workspaces.entry("--w--".into()).or_default().model = Some("m".into()))
            .expect("UI 保存");
        let f = SettingsStore::open(path.clone()).read();
        assert!(
            f.providers.iter().any(|p| p.id == "acme"),
            "外部 provider 不被 UI 保存覆盖"
        );
        assert_eq!(f.workspace("--w--").model.as_deref(), Some("m"));
        let _ = std::fs::remove_file(&path);
    }

    /// 并发 update 串行化:两线程各改不同字段,最终两者都生效
    #[test]
    fn concurrent_updates_serialize() {
        let path = temp_path("conc");
        let store = std::sync::Arc::new(SettingsStore::open(path.clone()));
        let a = std::sync::Arc::clone(&store);
        let b = std::sync::Arc::clone(&store);
        let (ta, tb) = (
            std::thread::spawn(move || a.update(|s| s.onboarded = true).expect("t1")),
            std::thread::spawn(move || {
                b.update(|s| {
                    s.workspaces.entry("--w--".into()).or_default().effort = Some("low".into())
                })
                .expect("t2")
            }),
        );
        ta.join().expect("t1 join");
        tb.join().expect("t2 join");
        let f = SettingsStore::open(path.clone()).read();
        assert!(f.onboarded);
        assert_eq!(f.workspace("--w--").effort.as_deref(), Some("low"));
        let _ = std::fs::remove_file(&path);
    }

    /// JSON 路径求值:点分段 + 数组下标,缺失/类型不符 = None
    #[test]
    fn json_path_walks_objects_and_arrays() {
        let v: serde_json::Value = serde_json::json!({
            "balance_infos": [ { "currency": "CNY", "total_balance": "9.52" } ],
            "usage": { "five_hour": { "utilization": 6 }, "resets_in": "4d22h" }
        });
        assert_eq!(
            json_path(&v, "balance_infos.0.total_balance").unwrap(),
            "9.52"
        );
        assert_eq!(json_path(&v, "usage.five_hour.utilization").unwrap(), 6);
        assert_eq!(json_path(&v, "usage.resets_in").unwrap(), "4d22h");
        assert!(json_path(&v, "balance_infos.5.total_balance").is_none());
        assert!(json_path(&v, "usage.nope").is_none());
        assert!(json_path(&v, "usage.five_hour.utilization.deeper").is_none());
    }

    /// 百分比提取:数字/带 % 字符串皆收,截断到 0-100
    #[test]
    fn json_percent_accepts_numbers_and_percent_strings() {
        assert_eq!(json_percent(&serde_json::json!(6)), Some(6));
        assert_eq!(json_percent(&serde_json::json!("7%")), Some(7));
        assert_eq!(json_percent(&serde_json::json!(0.06)), Some(6));
        assert_eq!(json_percent(&serde_json::json!(140)), Some(100));
        assert_eq!(json_percent(&serde_json::json!("abc")), None);
    }

    /// ProviderEntry 新字段 serde 往返:旧 JSON(缺新字段)自然落 default
    #[test]
    fn provider_entry_new_fields_roundtrip() {
        let entry = ProviderEntry {
            id: "glm".into(),
            base_url: "https://open.bigmodel.cn/api/anthropic".into(),
            dialect: "anthropic".into(),
            credential_ref: None,
            api_key: None,
            default_model: Some("glm-4.7".into()),
            display_name: Some("智谱 GLM".into()),
            models: vec!["glm-4.7".into(), "glm-4.7-flash".into()],
            billing: Some(BillingConfig {
                kind: BillingKind::Usage,
                url: "https://proxy.example/usage".into(),
                paths: BillingPaths {
                    usage_5h: Some("five_hour.utilization".into()),
                    usage_7d: Some("seven_day.utilization".into()),
                    resets: Some("resets_in".into()),
                    ..Default::default()
                },
            }),
            billing_cache: Some(BillingSnapshot::Usage {
                fetched_at_ms: 1_756_000_000_000,
                pct_5h: Some(6),
                pct_7d: Some(7),
                resets: Some("4d22h".into()),
            }),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["billing"]["kind"], "usage");
        assert_eq!(json["models"][0], "glm-4.7");
        let back: ProviderEntry = serde_json::from_value(json).unwrap();
        assert_eq!(back, entry);

        // 旧格式(无新字段)= 全 default,不报错
        let legacy: ProviderEntry = serde_json::from_value(serde_json::json!({
            "id": "deepseek",
            "base_url": "https://api.deepseek.com/v1",
            "dialect": "openai-chat"
        }))
        .unwrap();
        assert_eq!(legacy.display_name, None);
        assert!(legacy.models.is_empty());
        assert!(legacy.billing.is_none() && legacy.billing_cache.is_none());
    }
    #[test]
    fn hook_bridge_settings_roundtrip_persists() {
        // M4.2 现场复核:hookBridges 经 SettingsStore 落盘 → 重读持久
        // (桌面保存失败时先排除宿主持久化层)
        let dir = std::env::temp_dir().join(format!(
            "dsh-settings-m42-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let entry = HookBridgeEntry {
            id: "cc".into(),
            dialect: "claude-code".into(),
            config_path: "/tmp/demo.json".into(),
            enabled: true,
            ..Default::default()
        };
        {
            let store = SettingsStore::open(path.clone());
            store.update(|s| s.hook_bridges.push(entry)).unwrap();
        }
        let store2 = SettingsStore::open(path);
        let bridges = store2.read().hook_bridges;
        assert_eq!(bridges.len(), 1, "hookBridges 应随文件持久");
        assert_eq!(bridges[0].id, "cc");
        assert_eq!(bridges[0].dialect, "claude-code");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
