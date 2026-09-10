//! MCP 客户端桥:连接外部 MCP server(stdio),把其工具桥接进工具面。
//!
//! 语义对齐源 packages/mcp/mcp-client:公共名 `mcp__<server>__<raw>`(归一化
//! 与 64 上限加哈希消歧)、raw name 走 tools/call 线路、整代原子换带、
//! list_changed 触发重同步、内容投影(text 合并 / 占位 / 降级)。
//! 生命周期:装配登记、后台连接(不阻塞 attach);server 生命周期绑会话槽;
//! 连接失败 = 工具不可见 + 状态可查(首批失败即止,不自动重连)。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use dsh_agent_loop::CancelToken;
use dsh_agent_loop::tools::{ToolCallRequest, ToolOutput, ToolPort};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientInfo, ContentBlock, Implementation,
};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, RoleClient};
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

// ── 内容投影 ──────────────────────────────────────────────────────────────

/// 内容块 → 模型可见文本(照源 projectContent 语义:合并/占位/降级)。
pub fn project_content(result: &CallToolResult) -> String {
    let mut lines: Vec<String> = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text(t) => lines.push(t.text.clone()),
            ContentBlock::Image(i) => lines.push(format!(
                "[image: {} — MCP 图片未桥接,内容已省略]",
                i.mime_type
            )),
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
    if lines.is_empty() {
        "(tool returned no model-visible content)".into()
    } else {
        lines.join("\n")
    }
}

// ── 服务器配置 ────────────────────────────────────────────────────────────

/// stdio MCP server 配置(首批仅 stdio;http 第二批扩 transport 字段)
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerConfig {
    /// server 名(公共名成分;同一会话内唯一)
    pub server_name: String,
    pub command: String,
    pub args: Vec<String>,
    /// 显式 env(与清洗后的父环境合并,显式优先)
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    /// 单次调用超时(默认 60s,照源 toolCallTimeoutMs)
    pub tool_call_timeout: Duration,
}

use std::time::Duration;

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            server_name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            tool_call_timeout: Duration::from_secs(60),
        }
    }
}

// ── 状态与端口 ────────────────────────────────────────────────────────────

/// 连接状态(execute 等待 Ready;失败文案可直出)
#[derive(Debug, Clone, PartialEq)]
pub enum McpStatus {
    Connecting,
    Ready,
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
    /// 连接就绪后的 RunningService(call_tool 用;None = 未就绪/已失败)
    service: tokio::sync::Mutex<Option<RunningService<RoleClient, ServerHandler>>>,
}

impl PortState {
    fn new() -> Arc<Self> {
        let (status_tx, status_rx) = tokio::sync::watch::channel(McpStatus::Connecting);
        Arc::new(Self {
            tools: std::sync::RwLock::new(Vec::new()),
            status_tx,
            status_rx,
            resync: Arc::new(tokio::sync::Notify::new()),
            service: tokio::sync::Mutex::new(None),
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
    /// 失败(启动/握手/清单拉取;含错误文案)
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
}

impl McpServerPort {
    /// 装配期构造:登记配置并后台启动连接(不阻塞装配;须在 tokio
    /// runtime 上下文中调用)。`cancel` 触发即停机:连接各 await 点退出
    /// (transport drop 杀子进程);已就绪则发 shutdown 并等待清理。
    pub fn start(
        config: McpServerConfig,
        cancel: CancelToken,
        on_status: Option<StatusCallback>,
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
        }
    }

    /// 当前状态(通知行/诊断消费)
    pub fn status(&self) -> McpStatus {
        self.state.status_rx.borrow().clone()
    }

    /// 触发停机:连接任务退出(连接中直接中断;已就绪则发 shutdown,
    /// transport drop 杀子进程)。池移除端口时调用。
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

// ── 连接循环 ──────────────────────────────────────────────────────────────

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
            McpStatusEvent::Ready => {
                state.set_status(McpStatus::Ready);
            }
        }
        if let Some(cb) = &on_status {
            cb(event);
        }
    };
    let parent: BTreeMap<String, String> = std::env::vars().collect();
    let mut command = tokio::process::Command::new(&config.command);
    command.args(&config.args).env_clear();
    for (k, v) in scrub_env(&parent, &config.env) {
        command.env(k, v);
    }
    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }
    let transport = match TokioChildProcess::new(command) {
        Ok(t) => t,
        Err(e) => {
            notify(McpStatusEvent::Failed(format!("MCP server 启动失败: {e}")));
            return;
        }
    };
    let handler = ServerHandler {
        resync: Arc::clone(&state.resync),
    };
    notify(McpStatusEvent::Connecting);
    // 握手/清单拉取均可被 cancel 打断:中断即 return,transport drop
    // 杀子进程(ChildWithCleanup Drop 防僵尸)
    let running: RunningService<RoleClient, ServerHandler> = tokio::select! {
        r = rmcp::service::serve_client(handler, transport) => match r {
            Ok(s) => s,
            Err(e) => {
                notify(McpStatusEvent::Failed(format!("MCP 连接失败: {e}")));
                return;
            }
        },
        _ = cancel.cancelled() => return,
    };

    // 首次全量同步(list_all_tools 内部排干分页)
    let tools = tokio::select! {
        r = running.list_all_tools() => match r {
            Ok(t) => t,
            Err(e) => {
                notify(McpStatusEvent::Failed(format!("MCP 工具清单拉取失败: {e}")));
                return;
            }
        },
        _ = cancel.cancelled() => return,
    };
    state.replace_generation(&config.server_name, tools);
    *state.service.lock().await = Some(running);
    notify(McpStatusEvent::Ready);

    // list_changed → 重同步,持续消费(失败保留上一代工具,照源;
    // cancel → 停机:发 shutdown 并等待连接清理)
    loop {
        tokio::select! {
            _ = state.resync.notified() => {
                let mut svc = state.service.lock().await;
                if let Some(service) = svc.as_mut()
                    && let Ok(tools) = service.list_all_tools().await
                {
                    state.replace_generation(&config.server_name, tools);
                }
            }
            _ = cancel.cancelled() => {
                if let Some(service) = state.service.lock().await.take() {
                    let _ = service.cancel().await;
                }
                return;
            }
        }
    }
}

// ── ToolPort 实现 ─────────────────────────────────────────────────────────

impl ToolPort for McpServerPort {
    fn specs(&self) -> Vec<Value> {
        self.state.specs()
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        // 等待就绪(Connecting → Ready/Failed;超时 = 调用超时上限)
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
                McpStatus::Connecting => {}
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

        let service = self.state.service.lock().await;
        let Some(service) = service.as_ref() else {
            return ToolOutput {
                output: "MCP server 未连接".into(),
                success: false,
                ..Default::default()
            };
        };
        let call_future = service.call_tool(
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

        // isError → 工具失败(内容仍投影,供模型诊断)
        let text = project_content(&result);
        let success = result.is_error != Some(true);
        ToolOutput {
            output: text,
            success,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// 内容投影矩阵:text 合并 / image 占位 / resource_link 文本 / 空 /
    /// isError 不改投影(错误语义在 ToolOutput.success)
    #[test]
    fn project_content_matrix() {
        use rmcp::model::ResourceContents;
        // 纯 text:换行合并
        let r =
            CallToolResult::success(vec![ContentBlock::text("行一"), ContentBlock::text("行二")]);
        assert_eq!(project_content(&r), "行一\n行二");
        // image → 占位;resource_link → 文本
        let r = CallToolResult::success(vec![
            ContentBlock::image("ZmFrZQ==", "image/png"),
            ContentBlock::resource_link(rmcp::model::Resource::new("file:///x", "x")),
        ]);
        let out = project_content(&r);
        assert!(out.contains("image: image/png"), "{out}");
        assert!(out.contains("Resource link: x (file:///x)"), "{out}");
        // 空 → 固定占位
        let r = CallToolResult::success(vec![]);
        assert_eq!(
            project_content(&r),
            "(tool returned no model-visible content)"
        );
        // audio → 未支持占位
        let r = CallToolResult::success(vec![ContentBlock::audio("ZmFrZQ==", "audio/wav")]);
        assert!(project_content(&r).contains("audio: audio/wav"));
        // embedded resource → uri
        let r = CallToolResult::success(vec![ContentBlock::resource(ResourceContents::text(
            "正文",
            "file:///r",
        ))]);
        assert!(project_content(&r).contains("file:///r"));
    }

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

        // 路由到声明者:b 的 y(service 未连 → 端口级错误,而非 unknown)
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
