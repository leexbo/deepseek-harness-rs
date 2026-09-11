//! MCP 客户端桥:连接外部 MCP server(stdio / streamable-http),把其工具
//! 桥接进工具面。
//!
//! 语义对齐源 packages/mcp/mcp-client:公共名 `mcp__<server>__<raw>`(归一化
//! 与 64 上限加哈希消歧)、raw name 走 tools/call 线路、整代原子换带、
//! list_changed 触发重同步、内容投影(text 合并 / 占位 / 图片附件桥降级)。
//! 生命周期:装配登记、后台连接(不阻塞 attach);断线自动重连——指数退避
//! 500ms×2ⁿ 封顶 30s,每 outage 预算 10 次,稳定 ≥30s(= 最长退避间隔)的
//! 连接在下次断线时重置预算;预算耗尽注销该 server 全部工具,恢复 = 用户
//! 改配置/启停(设置页开关即重启)。
//! 图片桥:结果 image 块经准入链(mime 白名单 → canonical base64 双查 →
//! 批量原子落存)进附件存储,以持久引用随 tool/result 上行(零字节进日志);
//! 任一环节失败整批降级诊断文本,原始 base64 永不进模型上下文。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dsh_agent_loop::CancelToken;
use dsh_agent_loop::tools::{ToolCallRequest, ToolOutput, ToolPort};
use dsh_session::attachments::{ImageAttachmentRef, ImageMediaType};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientInfo, ContentBlock, Implementation,
};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, Peer, RoleClient};
use serde_json::{Value, json};

// ── 命名 ──────────────────────────────────────────────────────────────────

/// 公共名长度上限(与 provider 模型面工具名约束一致,源 MAX_PUBLIC_NAME_LENGTH)
const MAX_PUBLIC_NAME_LENGTH: usize = 64;
/// 哈希消歧后缀长度(源:sha256 前 12 hex)
const HASH_SUFFIX_LEN: usize = 12;

/// 公共工具名:`mcp__<server>__<raw>`。
///
/// 非法字符归一为 `_`;归一化有损或超长时截断并追加
/// `_<sha256(server\0raw) 前 12 hex>` 消歧——身份是 (server, raw) 二元组,
/// 公共名只进模型面,raw name 永远走 tools/call 线路(不反解)。
pub fn public_tool_name(server: &str, raw: &str) -> String {
    let combined = format!("mcp__{server}__{raw}");
    let normalized: String = combined
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let lossy = normalized != combined;
    if !lossy && normalized.len() <= MAX_PUBLIC_NAME_LENGTH {
        return normalized;
    }
    let hash = hex_sha256_12(&format!("{server}\0{raw}"));
    let base_len = MAX_PUBLIC_NAME_LENGTH - HASH_SUFFIX_LEN - 1;
    let mut base = normalized;
    base.truncate(base_len);
    format!("{base}_{hash}")
}

fn hex_sha256_12(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(&digest[..HASH_SUFFIX_LEN / 2])
}

// ── env 清洗 ──────────────────────────────────────────────────────────────

/// 敏感键剔除(照源 SENSITIVE_ENV_PATTERN /KEY|PASSWORD|SECRET|TOKEN/i)
fn is_sensitive_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.contains("KEY")
        || upper.contains("PASSWORD")
        || upper.contains("SECRET")
        || upper.contains("TOKEN")
}

/// stdio 子进程环境:base(父环境)剔除敏感键与 `DSH_*` 后,合并显式
/// env(显式优先)。纯函数——base 由调用方收集(测试免环境突变)。
pub fn scrub_env(
    base: &BTreeMap<String, String>,
    extra: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = base
        .iter()
        .filter(|(k, _)| !k.starts_with("DSH") && !is_sensitive_key(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (k, v) in extra {
        env.insert(k.clone(), v.clone());
    }
    env
}

// ── 内容投影与图片准入 ────────────────────────────────────────────────────

/// 降级文本模板(源 verbatim)
fn image_unavailable_text(media_type_display: &str, reason: &str) -> String {
    format!(
        "[image unavailable: {media_type_display}; {reason}; raw image data remains available to programmatic callers]"
    )
}

const IMAGE_MT_MISSING: &str = "unknown media type";
/// mime 白名单拒绝理由(源逐字)
const REASON_NOT_WHITELISTED: &str = "the declared media type is not PNG, JPEG, WebP, or GIF";
/// canonical base64 拒绝理由(源逐字)
const REASON_NOT_CANONICAL: &str = "the image data is not canonical base64";
/// 同批其余图片的连带拒绝理由(源逐字)
const REASON_SIBLING_INVALID: &str = "another image in the same result was invalid";
/// 无附件存储(源逐字)
const REASON_NO_STORE: &str = "no attachment store is mounted";
/// isError 前置拒绝(源 projectContent 默认 image projector 逐字)
const REASON_NOT_ADMITTED: &str = "this result was not admitted to durable model context";

/// 一张 MCP 结果图片的解码段中间形态
struct ImageCandidate {
    /// 诊断显示名(声明缺失 = "unknown media type")
    media_type_display: String,
    media_type: Option<ImageMediaType>,
    /// 解码后的原始字节(解码段通过时有效)
    bytes: Vec<u8>,
    /// 解码段拒绝理由;None = 通过
    reason: Option<String>,
}

impl ImageCandidate {
    fn new(data: &str, mime_type: &str) -> Self {
        let display = if mime_type.is_empty() {
            IMAGE_MT_MISSING
        } else {
            mime_type
        };
        let mut cand = Self {
            media_type_display: display.to_string(),
            media_type: None,
            bytes: Vec::new(),
            reason: None,
        };
        match ImageMediaType::parse(mime_type) {
            Some(mt) => cand.media_type = Some(mt),
            None => {
                cand.reason = Some(REASON_NOT_WHITELISTED.into());
                return cand;
            }
        }
        if !is_canonical_base64(data) {
            cand.reason = Some(REASON_NOT_CANONICAL.into());
            return cand;
        }
        use base64::Engine as _;
        // canonical 已验,解码必成;防御性 unwrap_or_default
        cand.bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap_or_default();
        cand
    }
}

/// canonical base64 双查(源 CANONICAL_BASE64 正则 + roundtrip):
/// 字符集 [A-Za-z0-9+/]、长度 4 的倍数、padding 只在尾部且 ≤2,再
/// decode→encode 恒等——拒绝空白与 URL-safe 别名,不做宽松归一。
fn is_canonical_base64(data: &str) -> bool {
    if data.is_empty() {
        return true;
    }
    let bytes = data.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return false;
    }
    let mut pad = 0;
    for (i, b) in bytes.iter().enumerate() {
        let ok = b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/';
        if !ok {
            if *b == b'=' && i + 2 >= bytes.len() {
                pad += 1;
            } else {
                return false;
            }
        }
    }
    if pad > 2 {
        return false;
    }
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()
        .map(|raw| base64::engine::general_purpose::STANDARD.encode(&raw) == data)
        .unwrap_or(false)
}

/// MCP 图片落存输入(宿主把 dsh-host AttachmentStore 适配成本 trait 注入,
/// 避免 dsh-mcp 反向依赖宿主 crate)
pub struct BridgeImageInput {
    pub data: Vec<u8>,
    pub media_type: ImageMediaType,
    pub name: Option<String>,
}

/// 图片存储口:批量原子准入(全批先验证后写入,任一失败零落盘)。
/// Err 文案进降级诊断(`image admission rejected the result: …`)。
pub trait ImageStorePort: Send + Sync {
    fn save(&self, images: Vec<BridgeImageInput>) -> Result<Vec<ImageAttachmentRef>, String>;
}

/// tools/call 结果的模型面投影(文本 + 已落存图片引用)
pub struct PreparedContent {
    pub output: String,
    pub images: Vec<ImageAttachmentRef>,
}

/// tools/call 结果投影(照源 prepareImageProjection 三段式,每段全有或
/// 全无):isError 先行拒绝(不落存)→ 解码段(mime/canonical,任一无效
/// 整批降级,其余块理由 `another image in the same result was invalid`)→
/// 落存段(批量原子)。image 位置不占文本行:引用随 `ToolOutput.images`
/// 上行,由方言层在 tool 消息里序列化为真图(照源「base64 永不进模型
/// 上下文」)。
pub fn prepare_content(
    result: &CallToolResult,
    store: Option<&dyn ImageStorePort>,
) -> PreparedContent {
    let is_error = result.is_error == Some(true);
    let mut lines: Vec<String> = Vec::new();
    let mut candidates: Vec<ImageCandidate> = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text(t) => lines.push(t.text.clone()),
            ContentBlock::Image(i) => {
                candidates.push(ImageCandidate::new(&i.data, &i.mime_type));
            }
            ContentBlock::ResourceLink(r) => {
                lines.push(format!("Resource link: {} ({})", r.name, r.uri));
            }
            ContentBlock::Resource(res) => {
                let uri = match &res.resource {
                    rmcp::model::ResourceContents::TextResourceContents { uri, .. } => uri,
                    rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => uri,
                    _ => "(unknown)",
                };
                lines.push(format!("[embedded resource: {uri} — 未支持]"));
            }
            ContentBlock::Audio(a) => {
                lines.push(format!("[audio: {} — 未支持]", a.mime_type));
            }
            _ => lines.push("[unsupported content]".into()),
        }
    }

    let images: Vec<ImageAttachmentRef> = if candidates.is_empty() {
        Vec::new()
    } else if is_error {
        // isError 前置拒绝:不做任何图片持久化(照源)
        for c in &candidates {
            lines.push(image_unavailable_text(
                &c.media_type_display,
                REASON_NOT_ADMITTED,
            ));
        }
        Vec::new()
    } else if candidates.iter().any(|c| c.reason.is_some()) {
        // 解码段任一无效:整批降级,合法成员给连带理由
        for c in &candidates {
            let reason = c
                .reason
                .clone()
                .unwrap_or_else(|| REASON_SIBLING_INVALID.to_string());
            lines.push(image_unavailable_text(&c.media_type_display, &reason));
        }
        Vec::new()
    } else {
        match store {
            None => {
                for c in &candidates {
                    lines.push(image_unavailable_text(
                        &c.media_type_display,
                        REASON_NO_STORE,
                    ));
                }
                Vec::new()
            }
            Some(store) => {
                let batch = candidates
                    .iter()
                    .map(|c| BridgeImageInput {
                        data: c.bytes.clone(),
                        media_type: c.media_type.expect("解码段通过必有类型"),
                        name: None,
                    })
                    .collect();
                match store.save(batch) {
                    Ok(refs) => refs,
                    Err(e) => {
                        for c in &candidates {
                            lines.push(image_unavailable_text(
                                &c.media_type_display,
                                &format!("image admission rejected the result: {e}"),
                            ));
                        }
                        Vec::new()
                    }
                }
            }
        }
    };

    let output = if lines.is_empty() {
        "(tool returned no model-visible content)".into()
    } else {
        lines.join("\n")
    };
    PreparedContent { output, images }
}

// ── 服务器配置 ────────────────────────────────────────────────────────────

/// 传输形态(源 config 判别字段:显式 `transport`,非「有 url 即 http」
/// 启发式)
#[derive(Debug, Clone, PartialEq)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        /// 显式 env(与清洗后的父环境合并,显式优先)
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    StreamableHttp {
        /// MCP endpoint URL
        url: String,
        /// 附加请求头,原样透传(源 headers dict 无 scrub/大小写处理;
        /// 鉴权约定 = 用户自带 Authorization)
        headers: BTreeMap<String, String>,
    },
}

/// MCP server 配置
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerConfig {
    /// server 名(公共名成分;同一会话内唯一)
    pub server_name: String,
    pub transport: McpTransport,
    /// 单次调用超时(默认 60s,照源 toolCallTimeoutMs)
    pub tool_call_timeout: Duration,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            server_name: String::new(),
            transport: McpTransport::Stdio {
                command: String::new(),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
            },
            tool_call_timeout: Duration::from_secs(60),
        }
    }
}

// ── 重连策略(源 scheduleReconnect 语义;纯函数便于测试)──────────────

/// 初始退避(源 RECONNECT_DEFAULTS.initialDelayMs)
pub const RECONNECT_INITIAL: Duration = Duration::from_millis(500);
/// 退避封顶 = 稳定窗口(源 maxDelayMs;连接存活 ≥ 此值视为结束上一轮 outage)
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// 每 outage 重连尝试预算(源 maxAttempts)
pub const RECONNECT_MAX_ATTEMPTS: u32 = 10;

/// 断线后的重连决策:稳定窗口重置 → 计数 → 预算判 → 退避延迟。
/// 返回 None = 预算耗尽放弃。稳定判定在**下次断线时**:上一代连接存活
/// ≥ [`RECONNECT_MAX`] → 预算清零(短暂成功的 crash-loop 不重置,照源)。
fn reconnect_decision(
    failed_attempts: &mut u32,
    connected_at: &mut Option<Instant>,
    now: Instant,
) -> Option<Duration> {
    if let Some(t) = connected_at
        && now.duration_since(*t) >= RECONNECT_MAX
    {
        *failed_attempts = 0;
    }
    *connected_at = None;
    *failed_attempts += 1;
    if *failed_attempts > RECONNECT_MAX_ATTEMPTS {
        return None;
    }
    // initial × 2^(n-1) 封顶 max;位移上限防溢出(封顶后恒为 max)
    let shift = (*failed_attempts - 1).min(20);
    let delay = RECONNECT_INITIAL
        .checked_mul(1 << shift)
        .unwrap_or(RECONNECT_MAX)
        .min(RECONNECT_MAX);
    Some(delay)
}

// ── 状态与端口 ────────────────────────────────────────────────────────────

/// 连接状态(execute 等待 Ready;失败文案可直出)
#[derive(Debug, Clone, PartialEq)]
pub enum McpStatus {
    Connecting,
    Ready,
    /// 断线重连中(工具保留旧代,调用失败;设置页可见进度)
    Reconnecting,
    Failed(String),
}

/// 工具代条目:公共名 → raw name + 模型面 spec
struct ToolEntry {
    public_name: String,
    raw_name: String,
    spec: Value,
}

struct PortState {
    /// 当前工具代(specs 同步读;整代原子换带)
    tools: std::sync::RwLock<Vec<ToolEntry>>,
    /// 状态广播(execute 等待 Ready;通知行消费 Failed)
    status_tx: tokio::sync::watch::Sender<McpStatus>,
    status_rx: tokio::sync::watch::Receiver<McpStatus>,
    /// tools/list_changed → 重同步触发
    resync: Arc<tokio::sync::Notify>,
    /// 就绪后的对端句柄(call_tool / 重同步用;None = 未就绪/已断线)
    peer: tokio::sync::Mutex<Option<Peer<RoleClient>>>,
}

impl PortState {
    fn new() -> Arc<Self> {
        let (status_tx, status_rx) = tokio::sync::watch::channel(McpStatus::Connecting);
        Arc::new(Self {
            tools: std::sync::RwLock::new(Vec::new()),
            status_tx,
            status_rx,
            resync: Arc::new(tokio::sync::Notify::new()),
            peer: tokio::sync::Mutex::new(None),
        })
    }

    fn set_status(&self, status: McpStatus) {
        let _ = self.status_tx.send(status);
    }

    /// 整代原子换带(同名 raw 重复 → 整列表无效,保留上一代,照源)
    fn replace_generation(&self, server: &str, tools: Vec<rmcp::model::Tool>) {
        let mut entries: Vec<ToolEntry> = Vec::with_capacity(tools.len());
        let mut seen = std::collections::HashSet::new();
        for tool in tools {
            if !seen.insert(tool.name.to_string()) {
                return; // 同名 raw 重复:整列表判无效
            }
            let public = public_tool_name(server, &tool.name);
            let spec = json!({
                "type": "function",
                "function": {
                    "name": public,
                    "description": tool.description.as_deref().unwrap_or_default(),
                    "parameters": Value::Object((*tool.input_schema).clone()),
                },
            });
            entries.push(ToolEntry {
                public_name: public,
                raw_name: tool.name.to_string(),
                spec,
            });
        }
        *self.tools.write().expect("tools 锁中毒") = entries;
    }

    fn specs(&self) -> Vec<Value> {
        self.tools
            .read()
            .expect("tools 锁中毒")
            .iter()
            .map(|e| e.spec.clone())
            .collect()
    }
}

/// 连接状态事件(宿主注入:通知行/状态呈现)
#[derive(Debug, Clone)]
pub enum McpStatusEvent {
    /// 已发起连接(initialize 进行中)
    Connecting,
    /// 连接 + 工具清单就绪
    Ready,
    /// 断线重连中(attempt = 本 outage 内第几次尝试;RS 原生:源只写日志,
    /// RS 有 mcp/status 通道与设置页,重连进度可见)
    Reconnecting {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
    /// 失败(启动/握手/清单拉取/预算耗尽;含错误文案)
    Failed(String),
}

/// 状态回调(宿主注入)
pub type StatusCallback = Arc<dyn Fn(McpStatusEvent) + Send + Sync>;

/// MCP server 工具端口:每 server 一个实例,持后台连接。
/// 轻量句柄(状态在 `Arc<PortState>`),Clone 共享同一连接。
#[derive(Clone)]
pub struct McpServerPort {
    /// server 名(状态/通知文案用)
    pub server_name: String,
    tool_call_timeout: Duration,
    cancel: CancelToken,
    state: Arc<PortState>,
    /// 图片落存口(None = 结果图片降级为 no-store 诊断文本)
    image_store: Option<Arc<dyn ImageStorePort>>,
}

impl McpServerPort {
    /// 装配期构造:登记配置并后台启动连接(不阻塞装配;须在 tokio
    /// runtime 上下文中调用)。`cancel` 触发即停机:连接各 await 点退出;
    /// 已就绪则等待服务清理。
    pub fn start(
        config: McpServerConfig,
        cancel: CancelToken,
        on_status: Option<StatusCallback>,
        image_store: Option<Arc<dyn ImageStorePort>>,
    ) -> Self {
        let state = PortState::new();
        tokio::spawn(connect_loop(
            Arc::clone(&state),
            config.clone(),
            cancel.clone(),
            on_status,
        ));
        Self {
            server_name: config.server_name,
            tool_call_timeout: config.tool_call_timeout,
            cancel,
            state,
            image_store,
        }
    }

    /// 当前状态(通知行/诊断消费)
    pub fn status(&self) -> McpStatus {
        self.state.status_rx.borrow().clone()
    }

    /// 触发停机:连接任务退出(连接中直接中断;已就绪则经 RunningService
    /// Drop 清理)。池移除端口时调用。
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }

    /// 本端口是否声明了该公共工具名(池路由用)
    pub fn has_tool(&self, public_name: &str) -> bool {
        self.state
            .tools
            .read()
            .expect("tools 锁中毒")
            .iter()
            .any(|e| e.public_name == public_name)
    }
}

// ── 端口池 ────────────────────────────────────────────────────────────────

/// 宿主级端口池:server id → 端口,所有会话共享(一个 server 一条连接)。
/// 作为单个聚合 [`ToolPort`] 装进工具面——specs 聚合与按名路由都是动态的,
/// 端口增删/工具代换带后下一轮 `specs()` 自然生效,会话无需重装配。
#[derive(Clone, Default)]
pub struct McpPoolPort {
    ports: Arc<std::sync::RwLock<BTreeMap<String, McpServerPort>>>,
}

impl McpPoolPort {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记/替换端口(id 同名即替换;旧端口由调用方负责 cancel)
    pub fn upsert(&self, id: impl Into<String>, port: McpServerPort) {
        self.ports
            .write()
            .expect("ports 锁中毒")
            .insert(id.into(), port);
    }

    /// 移除端口(返回句柄供调用方 cancel 停机;不存在返回 None)
    pub fn remove(&self, id: &str) -> Option<McpServerPort> {
        self.ports.write().expect("ports 锁中毒").remove(id)
    }

    /// 当前已登记的 server id(诊断/测试)
    pub fn server_ids(&self) -> Vec<String> {
        self.ports
            .read()
            .expect("ports 锁中毒")
            .keys()
            .cloned()
            .collect()
    }
}

impl ToolPort for McpPoolPort {
    fn specs(&self) -> Vec<Value> {
        self.ports
            .read()
            .expect("ports 锁中毒")
            .values()
            .flat_map(|p| p.specs())
            .collect()
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        // 按公共名找声明者,clone 句柄后即放锁(执行不跨 await 持锁)
        let owner = {
            let ports = self.ports.read().expect("ports 锁中毒");
            ports.values().find(|p| p.has_tool(&call.name)).cloned()
        };
        match owner {
            Some(mut port) => port.execute(call).await,
            None => ToolOutput {
                output: format!("unknown tool: {}", call.name),
                success: false,
                ..Default::default()
            },
        }
    }
}

// ── 连接监督 ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ServerHandler {
    resync: Arc<tokio::sync::Notify>,
}

impl ClientHandler for ServerHandler {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.client_info = Implementation::new("dsh-mcp", env!("CARGO_PKG_VERSION"));
        info
    }

    async fn on_tool_list_changed(&self, _context: rmcp::service::NotificationContext<RoleClient>) {
        self.resync.notify_one();
    }
}

/// streamable-http 传输:headers 原样透传走 reqwest default_headers
/// (rmcp config 的 custom_headers 拒 `authorization` 等保留头,reqwest
/// 层无此限制,照源「用户自带鉴权头」);连接池/重定向对齐 rmcp 默认形态。
fn http_transport(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<StreamableHttpClientTransport<reqwest::Client>, String> {
    reqwest::Url::parse(url).map_err(|e| format!("MCP url 无效: {e}"))?;
    let mut header_map = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| format!("MCP header 名无效 {k:?}: {e}"))?;
        let value = reqwest::header::HeaderValue::from_str(v)
            .map_err(|e| format!("MCP header 值无效 {k:?}: {e}"))?;
        header_map.insert(name, value);
    }
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(header_map)
        .build()
        .map_err(|e| format!("MCP http 客户端构建失败: {e}"))?;
    Ok(StreamableHttpClientTransport::with_client(
        client,
        StreamableHttpClientTransportConfig::with_uri(url),
    ))
}

enum GenerationError {
    Cancelled,
    /// 瞬态失败(握手/清单拉取/连接中断):进重连决策
    Failed(String),
    /// 确定性配置错误(URL/header 非法、可执行文件缺失):重试纯浪费,
    /// 立即 Failed 停机;用户改配置后 sync 即重启端口
    Permanent(String),
}

/// 起一代:构建传输(按配置判别)→ 握手 → 首次全量工具同步。
/// 失败不触碰状态(监督循环统一处置)。
async fn run_generation(
    config: &McpServerConfig,
    state: &Arc<PortState>,
    cancel: &CancelToken,
) -> Result<
    (
        RunningService<RoleClient, ServerHandler>,
        Vec<rmcp::model::Tool>,
    ),
    GenerationError,
> {
    let handler = ServerHandler {
        resync: Arc::clone(&state.resync),
    };
    let running: RunningService<RoleClient, ServerHandler> = match &config.transport {
        McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let parent: BTreeMap<String, String> = std::env::vars().collect();
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args).env_clear();
            for (k, v) in scrub_env(&parent, env) {
                cmd.env(k, v);
            }
            if let Some(cwd) = cwd {
                cmd.current_dir(cwd);
            }
            // spawn 失败即时终态;transport drop 杀子进程(ChildWithCleanup
            // Drop 防僵尸)
            let transport = TokioChildProcess::new(cmd)
                .map_err(|e| GenerationError::Permanent(format!("MCP server 启动失败: {e}")))?;
            tokio::select! {
                r = rmcp::service::serve_client(handler, transport) => r
                    .map_err(|e| GenerationError::Failed(format!("MCP 连接失败: {e}")))?,
                _ = cancel.cancelled() => return Err(GenerationError::Cancelled),
            }
        }
        McpTransport::StreamableHttp { url, headers } => {
            let transport = http_transport(url, headers).map_err(GenerationError::Permanent)?;
            tokio::select! {
                r = rmcp::service::serve_client(handler, transport) => r
                    .map_err(|e| GenerationError::Failed(format!("MCP 连接失败: {e}")))?,
                _ = cancel.cancelled() => return Err(GenerationError::Cancelled),
            }
        }
    };
    // 首次全量同步(list_all_tools 内部排干分页)
    let tools = tokio::select! {
        r = running.list_all_tools() => r
            .map_err(|e| GenerationError::Failed(format!("MCP 工具清单拉取失败: {e}")))?,
        _ = cancel.cancelled() => return Err(GenerationError::Cancelled),
    };
    Ok((running, tools))
}

/// 连接监督循环:起代 → 就绪(换带 + peer + 稳定计时)→ 服务至断线 →
/// 重连决策(退避/预算)→ 循环;cancel 任意点退出。
async fn connect_loop(
    state: Arc<PortState>,
    config: McpServerConfig,
    cancel: CancelToken,
    on_status: Option<StatusCallback>,
) {
    let notify = |event: McpStatusEvent| {
        match &event {
            McpStatusEvent::Failed(error) => {
                state.set_status(McpStatus::Failed(error.clone()));
            }
            McpStatusEvent::Connecting => {
                state.set_status(McpStatus::Connecting);
            }
            McpStatusEvent::Reconnecting { .. } => {
                state.set_status(McpStatus::Reconnecting);
            }
            McpStatusEvent::Ready => {
                state.set_status(McpStatus::Ready);
            }
        }
        if let Some(cb) = &on_status {
            cb(event);
        }
    };

    let mut failed_attempts: u32 = 0;
    let mut connected_at: Option<Instant> = None;
    loop {
        notify(McpStatusEvent::Connecting);
        match run_generation(&config, &state, &cancel).await {
            Err(GenerationError::Cancelled) => {
                *state.peer.lock().await = None;
                return;
            }
            Err(GenerationError::Permanent(e)) => {
                // 配置型错误:不重连,立即终态
                *state.peer.lock().await = None;
                notify(McpStatusEvent::Failed(e.clone()));
                eprintln!("[dsh-mcp {}] permanent failure: {e}", config.server_name);
                return;
            }
            Err(GenerationError::Failed(e)) => {
                *state.peer.lock().await = None;
                eprintln!(
                    "[dsh-mcp {}] connection attempt failed: {e}",
                    config.server_name
                );
            }
            Ok((running, tools)) => {
                state.replace_generation(&config.server_name, tools);
                *state.peer.lock().await = Some(running.peer().clone());
                connected_at = Some(Instant::now());
                notify(McpStatusEvent::Ready);

                // 服务期:list_changed 重同步(经 peer;失败保留上一代,照源)
                // / 断线观测(waiting 返回 = transport close 已发生)/ cancel。
                // waiting 消费 RunningService;cancel 路径靠 Drop 清理。
                let mut death = Box::pin(running.waiting());
                loop {
                    tokio::select! {
                        _ = state.resync.notified() => {
                            let peer = state.peer.lock().await;
                            if let Some(peer) = peer.as_ref()
                                && let Ok(tools) = peer.list_all_tools().await
                            {
                                state.replace_generation(&config.server_name, tools);
                            }
                        }
                        _ = &mut death => {
                            *state.peer.lock().await = None;
                            eprintln!("[dsh-mcp {}] connection lost", config.server_name);
                            break;
                        }
                        _ = cancel.cancelled() => {
                            *state.peer.lock().await = None;
                            return;
                        }
                    }
                }
            }
        }
        // ── 重连决策(断线/失败共用;源 scheduleReconnect 语义)──
        let Some(delay) =
            reconnect_decision(&mut failed_attempts, &mut connected_at, Instant::now())
        else {
            // 预算耗尽:注销该 server 全部工具(照源),恢复 = 设置页
            // 改配置/启停(开关即重启,优于源的 reload)
            state.replace_generation(&config.server_name, Vec::new());
            let msg = format!(
                "MCP server 连续 {RECONNECT_MAX_ATTEMPTS} 次重连失败,已放弃(工具已注销);可在设置中修改配置或重启该 server"
            );
            eprintln!(
                "[dsh-mcp {}] giving up after {RECONNECT_MAX_ATTEMPTS} consecutive failed reconnect attempts",
                config.server_name
            );
            notify(McpStatusEvent::Failed(msg));
            return;
        };
        notify(McpStatusEvent::Reconnecting {
            attempt: failed_attempts,
            max_attempts: RECONNECT_MAX_ATTEMPTS,
            delay_ms: delay.as_millis() as u64,
        });
        eprintln!(
            "[dsh-mcp {}] reconnecting in {}ms (attempt {failed_attempts}/{RECONNECT_MAX_ATTEMPTS})",
            config.server_name,
            delay.as_millis()
        );
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = cancel.cancelled() => return,
        }
    }
}

// ── ToolPort 实现 ─────────────────────────────────────────────────────────

impl ToolPort for McpServerPort {
    fn specs(&self) -> Vec<Value> {
        self.state.specs()
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        // 等待就绪(Connecting/Reconnecting → Ready/Failed;超时 = 调用超时
        // 上限——重连期间调用挂起到超时或就绪,工具不摘除,照源)
        let deadline = tokio::time::Instant::now() + self.tool_call_timeout;
        let mut status_rx = self.state.status_rx.clone();
        loop {
            match status_rx.borrow().clone() {
                McpStatus::Ready => break,
                McpStatus::Failed(e) => {
                    return ToolOutput {
                        output: format!("MCP server 不可用: {e}"),
                        success: false,
                        ..Default::default()
                    };
                }
                McpStatus::Connecting | McpStatus::Reconnecting => {}
            }
            let wait = tokio::time::sleep_until(deadline);
            tokio::select! {
                _ = wait => {
                    return ToolOutput {
                        output: format!("MCP server 连接超时({:?})", self.tool_call_timeout),
                        success: false,
                        ..Default::default()
                    };
                }
                changed = status_rx.changed() => {
                    if changed.is_err() {
                        return ToolOutput {
                            output: "MCP server 状态通道已关闭".into(),
                            success: false,
                            ..Default::default()
                        };
                    }
                }
                _ = self.cancel.cancelled() => {
                    return ToolOutput { output: "cancelled".into(), success: false, ..Default::default() };
                }
            }
        }

        let arguments = if let Some(s) = call.arguments.as_str() {
            serde_json::from_str::<Value>(s).unwrap_or(json!({}))
        } else {
            call.arguments.clone()
        };
        // 非对象兜底 `{}`:让 server 报具体参数错误(照源)
        let arguments = if arguments.is_object() {
            arguments
        } else {
            json!({})
        };
        // 公共名 → raw name:查当前工具代(照源「公共名永不反解」——
        // 身份在注册时确定,字符串拆分对 raw 含 `__` 的工具会错)
        let raw_name = {
            let tools = self.state.tools.read().expect("tools 锁中毒");
            match tools.iter().find(|e| e.public_name == call.name) {
                Some(e) => e.raw_name.clone(),
                None => {
                    return ToolOutput {
                        output: format!("unknown tool: {}", call.name),
                        success: false,
                        ..Default::default()
                    };
                }
            }
        };

        let peer = self.state.peer.lock().await;
        let Some(peer) = peer.as_ref() else {
            return ToolOutput {
                output: "MCP server 未连接".into(),
                success: false,
                ..Default::default()
            };
        };
        let call_future = peer.call_tool(
            CallToolRequestParams::new(raw_name.to_string())
                .with_arguments(arguments.as_object().cloned().unwrap_or_default()),
        );
        let result = tokio::select! {
            r = tokio::time::timeout(self.tool_call_timeout, call_future) => match r {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => {
                    return ToolOutput {
                        output: format!("MCP 调用失败: {e}"),
                        success: false,
                        ..Default::default()
                    };
                }
                Err(_) => {
                    return ToolOutput {
                        output: format!("MCP 调用超时({:?})", self.tool_call_timeout),
                        success: false,
                        ..Default::default()
                    };
                }
            },
            _ = self.cancel.cancelled() => {
                return ToolOutput { output: "cancelled".into(), success: false, ..Default::default() };
            }
        };

        // isError → 工具失败(内容仍投影供诊断;图片不落存,照源前置拒绝)
        let prepared = prepare_content(&result, self.image_store.as_deref());
        let success = result.is_error != Some(true);
        ToolOutput {
            output: prepared.output,
            success,
            images: prepared.images,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 命名:干净情形原样;非法字符归一;超长截断 + 哈希消歧
    #[test]
    fn public_tool_name_normalizes_and_disambiguates() {
        // 干净:原样
        assert_eq!(
            public_tool_name("filesystem", "read_file"),
            "mcp__filesystem__read_file"
        );
        // 非法字符归一为 _(有损 → 哈希后缀)
        let lossy = public_tool_name("my server", "tool.name");
        assert!(lossy.starts_with("mcp__my_server__tool_name_"), "{lossy}");
        assert!(lossy.len() <= MAX_PUBLIC_NAME_LENGTH, "{lossy}");
        // 同 server+raw 幂等
        assert_eq!(lossy, public_tool_name("my server", "tool.name"));
        // 超 64:截断 + 哈希,总长不超
        let long_raw = "t".repeat(100);
        let long = public_tool_name("srv", &long_raw);
        assert!(long.len() <= MAX_PUBLIC_NAME_LENGTH, "{long}");
        assert!(long.ends_with(char::is_alphanumeric), "{long}");
    }

    /// env 清洗:敏感键与 DSH_* 剔除,显式 env 优先
    #[test]
    fn scrub_env_removes_sensitive_and_dsh_keys() {
        let mut base = BTreeMap::new();
        base.insert("DSH_PROBE_X".into(), "1".into());
        base.insert("MY_API_KEY".into(), "secret".into());
        base.insert("SAFE_VAR".into(), "keep".into());
        let mut extra = BTreeMap::new();
        extra.insert("SERVER_TOKEN".into(), "explicit".into());
        let env = scrub_env(&base, &extra);
        assert!(!env.contains_key("DSH_PROBE_X"));
        assert!(!env.contains_key("MY_API_KEY"));
        assert_eq!(env.get("SAFE_VAR").map(String::as_str), Some("keep"));
        // 显式 env 覆盖清洗(用户主动声明的注入不受敏感键过滤影响)
        assert_eq!(
            env.get("SERVER_TOKEN").map(String::as_str),
            Some("explicit")
        );
    }

    // ── 内容投影与图片准入 ───────────────────────────────────────

    /// 记录式假存储(批量契约:全收或按预设拒)
    struct FakeStore {
        result: Result<Vec<ImageAttachmentRef>, String>,
        saved: Mutex<usize>,
    }
    impl FakeStore {
        fn ok() -> Self {
            Self {
                result: Ok(vec![ImageAttachmentRef {
                    attachment_id: "sha256:abc".into(),
                    media_type: ImageMediaType::Png,
                    bytes: 4,
                    width: 1,
                    height: 1,
                    name: None,
                }]),
                saved: Mutex::new(0),
            }
        }
        fn reject(msg: &str) -> Self {
            Self {
                result: Err(msg.to_string()),
                saved: Mutex::new(0),
            }
        }
    }
    impl ImageStorePort for FakeStore {
        fn save(&self, images: Vec<BridgeImageInput>) -> Result<Vec<ImageAttachmentRef>, String> {
            *self.saved.lock().unwrap() += images.len();
            self.result.clone()
        }
    }

    /// 纯文本投影:text 合并 / resource_link / 空 / audio / embedded
    /// (首批矩阵保留面)
    #[test]
    fn project_content_text_matrix() {
        use rmcp::model::ResourceContents;
        let r =
            CallToolResult::success(vec![ContentBlock::text("行一"), ContentBlock::text("行二")]);
        let p = prepare_content(&r, None);
        assert_eq!(p.output, "行一\n行二");
        assert!(p.images.is_empty());
        let r = CallToolResult::success(vec![ContentBlock::resource_link(
            rmcp::model::Resource::new("file:///x", "x"),
        )]);
        assert!(
            prepare_content(&r, None)
                .output
                .contains("Resource link: x (file:///x)")
        );
        let r = CallToolResult::success(vec![]);
        assert_eq!(
            prepare_content(&r, None).output,
            "(tool returned no model-visible content)"
        );
        let r = CallToolResult::success(vec![ContentBlock::audio("ZmFrZQ==", "audio/wav")]);
        assert!(
            prepare_content(&r, None)
                .output
                .contains("audio: audio/wav")
        );
        let r = CallToolResult::success(vec![ContentBlock::resource(ResourceContents::text(
            "正文",
            "file:///r",
        ))]);
        assert!(prepare_content(&r, None).output.contains("file:///r"));
    }

    /// isError 前置拒绝:图片不落存(存储调用次数 0),降级理由逐字
    #[test]
    fn is_error_rejects_images_before_persistence() {
        let store = FakeStore::ok();
        let result = CallToolResult::error(vec![ContentBlock::image("aGVsbG8xMjM0", "image/png")]);
        let p = prepare_content(&result, Some(&store));
        assert_eq!(*store.saved.lock().unwrap(), 0, "isError 零落盘");
        assert!(p.output.contains(
            "[image unavailable: image/png; this result was not admitted to durable model context; raw image data remains available to programmatic callers]"
        ));
    }

    /// mime 白名单:缺声明 = unknown media type;白名单外逐字理由
    #[test]
    fn image_mime_whitelist() {
        let r = CallToolResult::success(vec![ContentBlock::image(
            "aGVsbG8xMjM0",
            "application/octet-stream",
        )]);
        let p = prepare_content(&r, None);
        assert!(p.output.contains(
            "[image unavailable: application/octet-stream; the declared media type is not PNG, JPEG, WebP, or GIF; raw image data remains available to programmatic callers]"
        ));
        let r = CallToolResult::success(vec![ContentBlock::image("aGVsbG8xMjM0", "")]);
        let p = prepare_content(&r, None);
        assert!(p.output.contains("[image unavailable: unknown media type;"));
    }

    /// canonical base64 双查:URL-safe 别名与空白拒;roundtrip 恒等
    #[test]
    fn canonical_base64_double_check() {
        // URL-safe(- _)非法;空白非法
        let r = CallToolResult::success(vec![ContentBlock::image("a-b_", "image/png")]);
        let p = prepare_content(&r, None);
        assert!(p.output.contains("the image data is not canonical base64"));
        // 合法形态:roundtrip 恒等 → 进落存段(无 store → no-store 理由)
        let r = CallToolResult::success(vec![ContentBlock::image("aGVsbG8xMjM0", "image/png")]);
        let p = prepare_content(&r, None);
        assert!(p.output.contains("no attachment store is mounted"));
    }

    /// 整批原子:批内一张无效,全部降级且合法成员给连带理由;存储零调用
    #[test]
    fn invalid_sibling_degrades_whole_batch_without_storing() {
        let store = FakeStore::ok();
        let r = CallToolResult::success(vec![
            ContentBlock::text("前文"),
            ContentBlock::image("aGVsbG8xMjM0", "image/png"),
            ContentBlock::image("!!!", "image/png"),
        ]);
        let p = prepare_content(&r, Some(&store));
        assert!(p.output.contains("前文"));
        assert!(
            p.output
                .contains("another image in the same result was invalid")
        );
        assert_eq!(*store.saved.lock().unwrap(), 0, "任一无效零落盘");
        assert!(p.images.is_empty());
    }

    /// 全部合法:引用返回、无降级行;存储拒绝 → 整批降级含拒绝文案
    #[test]
    fn valid_batch_stores_and_rejection_degrades() {
        let store = FakeStore::ok();
        let r = CallToolResult::success(vec![ContentBlock::image("aGVsbG8xMjM0", "image/png")]);
        let p = prepare_content(&r, Some(&store));
        assert_eq!(p.images.len(), 1);
        assert_eq!(p.images[0].attachment_id, "sha256:abc");

        let store = FakeStore::reject("too large");
        let r = CallToolResult::success(vec![ContentBlock::image("aGVsbG8xMjM0", "image/png")]);
        let p = prepare_content(&r, Some(&store));
        assert!(p.images.is_empty());
        assert!(
            p.output
                .contains("image admission rejected the result: too large")
        );
    }

    // ── 重连策略 ────────────────────────────────────────────────

    /// 退避序列:500ms ×2ⁿ 封顶 30s
    #[test]
    fn reconnect_backoff_sequence() {
        let mut failed = 0;
        let mut connected = None;
        let t0 = Instant::now();
        let mut delays = Vec::new();
        for _ in 0..12 {
            match reconnect_decision(&mut failed, &mut connected, t0) {
                Some(d) => delays.push(d),
                None => break,
            }
        }
        assert_eq!(
            delays,
            vec![
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ],
            "10 次预算,封顶 30s"
        );
        // 预算耗尽
        assert!(reconnect_decision(&mut failed, &mut connected, t0).is_none());
    }

    /// 稳定重置:存活 ≥ 30s 的连接在下次断线时清零预算;
    /// 短暂成功(< 30s)不重置——crash-loop 照常耗尽
    #[test]
    fn reconnect_stability_window_resets_budget() {
        let mut failed = 0u32;
        let mut connected = None;
        let t0 = Instant::now();
        // 首代崩溃(未连上)
        assert!(reconnect_decision(&mut failed, &mut connected, t0).is_some());
        assert_eq!(failed, 1);
        // 连上 5s 就崩:不重置
        connected = Some(t0 + Duration::from_secs(5));
        assert!(
            reconnect_decision(&mut failed, &mut connected, t0 + Duration::from_secs(6)).is_some()
        );
        assert_eq!(failed, 2, "短暂成功不重置");
        // 连上 40s 后崩:重置 → 本 outage 首败
        connected = Some(t0 + Duration::from_secs(100));
        let d = reconnect_decision(&mut failed, &mut connected, t0 + Duration::from_secs(140))
            .expect("重置后重新有预算");
        assert_eq!(d, RECONNECT_INITIAL);
        assert_eq!(failed, 1);
    }

    // ── 端口/池(首批保留面)───────────────────────────────────

    /// replace_generation:同名 raw 重复 → 整列表无效(保留上一代)
    #[test]
    fn replace_generation_rejects_duplicate_raw_names() {
        let state = PortState::new();
        fn mk(name: &str) -> rmcp::model::Tool {
            rmcp::model::Tool::new(name.to_owned(), "", serde_json::Map::new())
        }
        state.replace_generation("srv", vec![mk("a"), mk("b")]);
        assert_eq!(state.specs().len(), 2);
        // 重复 raw(a):整代无效 → 上一代保留
        state.replace_generation("srv", vec![mk("a"), mk("a")]);
        assert_eq!(state.specs().len(), 2, "应保留上一代");
    }

    /// 池:specs 跨端口聚合 + 按公共名路由到声明者 + 未知名软失败 + 移除
    #[tokio::test]
    async fn pool_aggregates_specs_and_routes_by_name() {
        fn port_with(server: &str, tools: &[&str]) -> McpServerPort {
            let state = PortState::new();
            state.replace_generation(
                server,
                tools
                    .iter()
                    .map(|t| rmcp::model::Tool::new((*t).to_owned(), "", serde_json::Map::new()))
                    .collect(),
            );
            state.set_status(McpStatus::Ready);
            McpServerPort {
                server_name: server.to_string(),
                tool_call_timeout: Duration::from_secs(1),
                cancel: CancelToken::new(),
                state,
                image_store: None,
            }
        }
        let mut pool = McpPoolPort::new();
        pool.upsert("a", port_with("a", &["x"]));
        pool.upsert("b", port_with("b", &["y"]));

        let names: Vec<String> = pool
            .specs()
            .iter()
            .filter_map(|s| s["function"]["name"].as_str().map(String::from))
            .collect();
        assert_eq!(
            names,
            vec!["mcp__a__x".to_string(), "mcp__b__y".to_string()]
        );

        // 路由到声明者:b 的 y(peer 未连 → 端口级错误,而非 unknown)
        let out = ToolPort::execute(
            &mut pool,
            &ToolCallRequest {
                name: "mcp__b__y".into(),
                arguments: json!({}),
            },
        )
        .await;
        assert!(!out.success);
        assert!(out.output.contains("MCP server 未连接"), "{}", out.output);

        // 未知名:池级软失败
        let out = ToolPort::execute(
            &mut pool,
            &ToolCallRequest {
                name: "mcp__a__nope".into(),
                arguments: json!({}),
            },
        )
        .await;
        assert_eq!(out.output, "unknown tool: mcp__a__nope");

        // 移除后 specs 即时收敛(下一轮聚合生效)
        assert!(pool.remove("a").is_some());
        assert_eq!(pool.server_ids(), vec!["b".to_string()]);
        assert_eq!(pool.specs().len(), 1);
    }
}
