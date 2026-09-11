//! dsh-app:会话装配层。
//!
//! 把「一个会话如何被组装」从 `dsh` 二进制下沉为库(为未来的替代
//! 前端——web 网关等——预留库形态):配置合并
//! (CLI > dsh.toml > 默认)、prompt 组装(AGENTS.md/preset/plan 态)、
//! transport 构建、preset 驱动的工具组装、`Session`(turn 驱动 +
//! 会话级事件)。错误用 anyhow:本 crate 属装配层(代码规范:
//! 二进制/装配层用 anyhow)。
//!
//! 架构不变量在此延续:闸门与 engine 共享同一日志(不变式比对的
//! 期望侧);prompt 的 plan 态每 turn 自日志重建(重放一致);
//! 事件先落日志再触发渲染回调(记录 ⟺ 显示)。

#![deny(missing_docs)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use dsh_agent_loop::{
    CancelToken, ContextSection, LoopEngine, RequestHeader, SteerInput, ToolPort, ToolSet,
    TurnOutcome,
};
use dsh_host::{DshConfig, JsonlBackend, MountSpec, PresetManifest};
use dsh_llm::streaming::StreamMode;
use dsh_llm::{HttpTransport, ProviderConfig};
use dsh_session::{EventEnvelope, EventLog};
use serde_json::Value;

pub mod mount;
use mount::assemble;

/// 配置合并的调用方原始输入(全部可缺省;与 clap 解耦,嵌入方同样可构造)
#[derive(Debug, Clone, Default)]
pub struct ResolveArgs {
    /// 模型标识
    pub model: Option<String>,
    /// provider base URL
    pub base_url: Option<String>,
    /// 会话日志路径
    pub session: Option<String>,
    /// 工作目录(可写根/工具 cwd)
    pub workspace: Option<String>,
    /// provider 方言
    pub dialect: Option<String>,
    /// 能力 preset 标识
    pub preset: Option<String>,
    /// 推理等级(low / high / max;缺省 = provider 默认)
    pub reasoning_effort: Option<String>,
    /// 可选模型清单(模型选择菜单;缺省 = 内置默认)
    pub models: Option<Vec<String>>,
}

/// 合并后的装配参数(CLI > dsh.toml > 默认)
#[derive(Debug, Clone)]
pub struct Resolved {
    /// 模型标识
    pub model: String,
    /// provider base URL
    pub base_url: String,
    /// 会话日志路径(追加式 JSONL)
    pub session: String,
    /// 工作目录
    pub workspace: std::path::PathBuf,
    /// provider 方言
    pub dialect: String,
    /// 推理等级(low / high / max;None = provider 默认)
    pub reasoning_effort: Option<String>,
    /// 可选模型清单(配置覆盖;None = 内置默认)
    pub models: Option<Vec<String>>,
    /// 能力 preset(k8s 形态 YAML manifest:组件装配清单)
    pub preset: PresetManifest,
}

impl Resolved {
    /// 配置合并:CLI 参数 > 配置文件 > 内置默认;preset 加载失败即拒绝
    pub fn resolve(args: ResolveArgs, config_path: &Path) -> Result<Self> {
        let cfg = DshConfig::load(config_path)?;
        let workspace = args
            .workspace
            .clone()
            .map(std::path::PathBuf::from)
            .or_else(|| cfg.workspace.clone().map(std::path::PathBuf::from))
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });
        let preset_id = args
            .preset
            .clone()
            .or(cfg.preset)
            .unwrap_or_else(|| "standard".into());
        let preset =
            PresetManifest::load(&workspace, &preset_id).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Self {
            model: args
                .model
                .clone()
                .or(cfg.model)
                .unwrap_or_else(|| "deepseek-chat".into()),
            base_url: args
                .base_url
                .clone()
                .or(cfg.base_url)
                .unwrap_or_else(|| "https://api.deepseek.com/v1".into()),
            session: args
                .session
                .clone()
                .or(cfg.session)
                .unwrap_or_else(|| "dsh-session.jsonl".into()),
            dialect: args
                .dialect
                .clone()
                .or(cfg.dialect)
                // 未配置 dialect 时 DeepSeek 走 Responses 形态(事件流
                // 语义/usage 天然齐全);chat 形态经显式配置选择
                .unwrap_or_else(|| "deepseek-responses".into()),
            reasoning_effort: args.reasoning_effort.clone().or(cfg.reasoning_effort),
            models: cfg.models.clone(),
            workspace,
            preset,
        })
    }

    /// API key:CLI 参数 > DEEPSEEK_API_KEY 环境变量 > cwd `.env` 文件。
    ///
    /// `.env` 回退覆盖「source 未 export 的 shell 变量子进程不可见」
    /// 场景。只读取 DEEPSEEK_API_KEY 一项,不回显。
    pub fn resolve_api_key(cli_key: Option<String>) -> Result<String> {
        cli_key
            .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok())
            .or_else(|| dotenv_key("DEEPSEEK_API_KEY"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "需要 --api-key、环境变量 DEEPSEEK_API_KEY 或 .env 中的 DEEPSEEK_API_KEY"
                )
            })
    }
}

/// 从 cwd 的 `.env` 读单个键(`KEY=VALUE` 行,容忍引号与空白;
/// 文件缺失/键不存在返回 None)
fn dotenv_key(key: &str) -> Option<String> {
    dotenv_key_at(std::path::Path::new("."), key)
}

/// 从指定目录的 `.env` 读单个键(凭据 seam 的工作区级回退;
/// 解析行为与 cwd 版一致,由 [`dotenv_value`] 单测锁定)
pub fn dotenv_key_at(dir: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(".env")).ok()?;
    dotenv_value(&text, key)
}

/// 把键值写入目录 `.env`(已有该键则整行替换,否则追加;
/// 文件不存在则创建)。值含换行时拒绝(行式格式不容纳)。
pub fn write_dotenv_key(dir: &Path, key: &str, value: &str) -> Result<()> {
    if value.contains('\n') || value.contains('\r') {
        anyhow::bail!("凭据值含换行,无法写入 .env 行式格式");
    }
    let path = dir.join(".env");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let prefix = format!("{key}=");
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut newline = format!("{prefix}{value}");
    match lines.iter_mut().find(|l| l.trim().starts_with(&prefix)) {
        Some(line) => *line = std::mem::take(&mut newline),
        None => {
            if !lines.is_empty() && !text.ends_with('\n') {
                lines.push(String::new()); // 原末行无换行时先补空行再追加
            }
            lines.push(newline);
        }
    }
    let out = format!("{}\n", lines.join("\n"));
    std::fs::write(&path, out).with_context(|| format!("写入 {} 失败", path.display()))
}

/// `.env` 文本中取单个键的值(纯函数,解析行为由单测锁定)
fn dotenv_value(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&format!("{key}=")) {
            let value = rest.trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// prompt 组装的静态部分(会话期不变;模式/计划态每 turn 从日志读)
#[derive(Debug, Clone)]
pub struct PromptParts {
    /// 身份段(preset 可覆盖)
    pub identity: String,
    /// 环境段
    pub env_info: String,
    /// preset 追加段
    pub append: Option<String>,
    /// @file 引用提示(@ 前缀文件用 read 工具读)
    pub file_reference_hint: Option<String>,
    /// 在场工具的使用指南节(tool:<name> section 语义;装配期收集)
    pub tool_sections: Vec<String>,
    /// 模型标识
    pub model: String,
    /// 推理等级(low / high / max;None = provider 默认)
    pub reasoning_effort: Option<String>,
}

/// 组装 prompt 静态部分(AGENTS.md 查找在此发生——宿主侧 IO)。
/// 身份两句式(harness:identity + preset persona 两节):harness 句
/// 恒在;persona 行 config 的 identity 经 {{model}}/{{cwd}} 插值后追加
/// (persona 文本即插值模板;无 persona 行 = 只有 harness 句)。
/// 工具指南节由装配注册表收集(在场组件的 prompt 节,tool:<name> 语义);
/// `subagent_background` = subagent 装配为后台形态(结算通知 port 在场)时
/// 追加源 tool:subagent 节(见 mount::tool_prompt_sections_with)。
pub fn prompt_parts(resolved: &Resolved, subagent_background: bool) -> PromptParts {
    let persona = resolved
        .preset
        .mount("persona")
        .map(MountSpec::config_object);
    let interpolate = |text: &str| {
        text.replace("{{model}}", &resolved.model)
            .replace("{{cwd}}", &resolved.workspace.display().to_string())
    };
    let mut identity = "You are an AI agent powered by dsh.".to_string();
    if let Some(text) = persona
        .as_ref()
        .and_then(|c| c["identity"].as_str())
        .filter(|s| !s.is_empty())
    {
        identity.push_str("\n\n");
        identity.push_str(&interpolate(text));
    }
    PromptParts {
        identity,
        env_info: format!(
            "cwd={}\nsandbox: workspace is the only writable root\n\
             git: 本目录是 git 仓库时,用 bash 工具执行 git 命令查看历史/分支/状态 \
             (git log --oneline -n 20 / git branch -a / git status --short),\
             不要直接读取 .git 目录下的文件",
            resolved.workspace.display()
        ),
        append: persona
            .as_ref()
            .and_then(|c| c["append"].as_str())
            .filter(|s| !s.is_empty())
            .map(interpolate),
        // @file 引用提示(恒注入;standard preset 有 read 工具,源 context:file-reference)
        file_reference_hint: Some(
            "Paths prefixed with @ are files explicitly referenced by the user. \
Use the read tool when their contents are needed; do not claim to have inspected a file before reading it."
                .into(),
        ),
        tool_sections: mount::tool_prompt_sections_with(&resolved.preset, subagent_background)
            .into_iter()
            .map(String::from)
            .collect(),
        model: resolved.model.clone(),
        reasoning_effort: resolved.reasoning_effort.clone(),
    }
}

/// 从日志读 plan 态(最近一条 session/mode 与 plan/approved)。
/// 模式与活跃计划影响模型可见 prompt → 必须来自日志(重放一致)
pub fn plan_state(log: &EventLog) -> (bool, Option<String>) {
    let mode = log
        .iter()
        .rev()
        .find(|e| e.r#type == "session/mode")
        .and_then(|e| e.data["mode"].as_str())
        .unwrap_or("standard");
    let active = log
        .iter()
        .rev()
        .find(|e| e.r#type == "plan/approved")
        .and_then(|e| e.data["plan"].as_str().map(String::from));
    (mode == "plan", active)
}

/// 组装请求 header(每 turn 重建:plan 态来自共享日志)
pub fn build_header(parts: &PromptParts, log: &EventLog) -> RequestHeader {
    let (plan_mode, active_plan) = plan_state(log);
    // AGENTS.md 不进 system prompt:4a 完整溯源模型下,它作为 user/message
    // + source.kind=agent-instructions 注入模型可见消息流(attach 基线 +
    // driver_loop 每步重扫,见 registry)。
    let ctx = dsh_prompt::AssembleContext {
        identity: parts.identity.clone(),
        env_info: parts.env_info.clone(),
        plan_mode,
        plan: None,
        active_plan,
        append: parts.append.clone(),
        file_reference: parts.file_reference_hint.clone(),
        tool_sections: parts.tool_sections.clone(),
    };
    RequestHeader {
        model: parts.model.clone(),
        system: dsh_prompt::assemble(&ctx),
        temperature: 0.0,
        reasoning_effort: parts.reasoning_effort.clone(),
        tools: Vec::new(), // engine 在每个 turn 从 ToolPort::specs 注入
    }
}

/// 空共享日志(闸门/engine 的共享视图起点)
pub fn fresh_log() -> Arc<Mutex<EventLog>> {
    Arc::new(Mutex::new(EventLog::new()))
}

/// 墙钟(ms;组件内禁止直读时钟,时钟在此层注入)
pub fn wall_clock() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 构建 HTTP 传输(未包闸门;子代理/工作流各持一份)。
/// `attachments` = 图片附件字节来源(请求期 data URL 组装;None = 无来源)
pub fn build_raw_transport(
    resolved: &Resolved,
    api_key: &str,
    attachments: Option<std::sync::Arc<dyn dsh_llm::AttachmentSource>>,
) -> Result<HttpTransport> {
    let adapter = dsh_llm::adapter_by_name(&resolved.dialect)
        .ok_or_else(|| anyhow::anyhow!("未知 provider 方言:{}", resolved.dialect))?;
    let mut transport = HttpTransport::with_adapter(
        ProviderConfig {
            base_url: resolved.base_url.clone(),
            api_key: api_key.to_string(),
            stream_mode: StreamMode::Sse,
        },
        adapter,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(source) = attachments {
        transport = transport.with_attachments(source);
    }
    Ok(transport)
}

/// preset 驱动的工具组装(薄壳:装配逻辑在 [`mount`] 的在树组件注册表)。
/// 每行 mount 装载一个组件——在树注册名或 wasm 工具组件路径;
/// `permission` 为访问模式:read-only(只读沙箱/文件拒写)/
/// workspace-write(默认:可写根 = workspace)/ full-access(bash 全盘可写)。
/// `mode_source` 在场时工具执行时实时解析权限(会话内动态切换),静态
/// `permission` 仅作缺省兜底。需要为子代理/工作流构建独立传输,故要求 api_key
#[allow(clippy::too_many_arguments)]
pub fn build_tools(
    resolved: &Resolved,
    api_key: &str,
    log: &Arc<Mutex<EventLog>>,
    cancel: &CancelToken,
    pty: bool,
    permission: &str,
    mode_source: Option<dsh_tools::ModeSource>,
    approval_port: Option<std::sync::Arc<dyn dsh_tools::ApprovalPort>>,
    query_port: Option<std::sync::Arc<dyn dsh_tools::session_query::SessionQueryPort>>,
    ask_port: Option<std::sync::Arc<dyn dsh_tools::AskQuestionPort>>,
    session_factory: Option<std::sync::Arc<dyn dsh_tools::subagent::SessionFactory>>,
    notify_port: Option<std::sync::Arc<dyn dsh_tools::subagent::SettlementNotificationPort>>,
    current_session: Option<&str>,
    subagent_bridge: Option<std::sync::Arc<dsh_tools::subagent::SubagentBridge>>,
    // 宿主侧追加工具(MCP server 桥等;与 preset 装配的工具同池,重名 fail-fast)
    mut extra_tools: Vec<Box<dyn dsh_agent_loop::tools::ToolPortObj>>,
) -> Result<ToolSet> {
    let mut tools = assemble(
        resolved,
        api_key,
        log,
        cancel,
        pty,
        permission,
        mode_source,
        approval_port,
        query_port,
        ask_port,
        session_factory,
        notify_port,
        current_session,
        subagent_bridge,
    )?;
    tools.append(&mut extra_tools);
    ToolSet::new(tools).map_err(|e| anyhow::anyhow!("{e}"))
}

/// 会话装配:engine + 闸门 + 工具 + 持久化,跨 turn 保留(REPL/GUI 的
/// 多轮历史全部来自日志派生——engine 的唯一状态就是日志 + phase)
pub struct Session<T, TOOLS> {
    engine: LoopEngine,
    gate: T,
    tools: TOOLS,
    backend: JsonlBackend,
    session_path: String,
    parts: PromptParts,
    cancel: CancelToken,
}

impl<T: Send, TOOLS> Session<T, TOOLS> {
    /// 以共享日志视图构建(闸门与 engine 必须共享同一日志——不变式比对
    /// 的期望侧;调用方经 `gate.log()` 取得)
    pub fn new(
        parts: PromptParts,
        gate: T,
        log: Arc<Mutex<EventLog>>,
        tools: TOOLS,
        backend: JsonlBackend,
        session_path: String,
        cancel: CancelToken,
    ) -> Self {
        let header = match log.lock() {
            Ok(l) => build_header(&parts, &l),
            // 装配期无并发,锁中毒以默认态兜底;首 turn 的 refresh 仍重建
            Err(_) => build_header(&parts, &EventLog::new()),
        };
        Self {
            engine: {
                let mut e = LoopEngine::new(header, log);
                e.set_cancel(cancel.clone());
                e
            },
            parts,
            // 令牌与工具共享:engine 安全点与工具执行中的 select 同源
            gate,
            tools,
            backend,
            session_path,
            cancel,
        }
    }

    /// 会话级软取消令牌(REPL Ctrl-C / GUI 取消按钮同源;每 turn 复位)
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// 装配运行中 turn 的中途输入缓冲(steer;web 宿主 worker 创建并共享,
    /// 引擎在 step 边界认领——队列瞬态的唯一活体入口)
    pub fn set_steer_buf(&mut self, buf: Arc<Mutex<VecDeque<SteerInput>>>) {
        self.engine.set_steer_buf(buf);
    }

    /// 设置下一 turn 输入的来源染色(结算通知;转发引擎,认领时由
    /// 驱动设置、落档真实用户消息时消费)
    pub fn set_input_source(&mut self, source: Option<Value>) {
        self.engine.set_input_source(source);
    }

    /// 装配会话级 runtime-context 渲染回调(由上层(如 dsh-core driver)注入;
    /// 每步调它拿当前渲染的 `(current 文本, sections)`,交引擎投影去重后落档为
    /// 注入 user/message)。未设置 = 本会话无 runtime-context 注入。
    pub fn set_context_provider(
        &mut self,
        provider: Box<dyn Fn() -> Option<(String, Vec<ContextSection>)> + Send + Sync>,
    ) {
        self.engine.set_context_provider(provider);
    }

    /// 装配 workspace 指令(AGENTS.md)每步重扫回调(上层 driver 注入)。
    pub fn set_instructions_provider(&mut self, provider: dsh_agent_loop::InstructionsProvider) {
        self.engine.set_instructions_provider(provider);
    }

    /// 装配 skill 目录每步回调(dsh-skill SkillCatalogState;变化才 Some)。
    pub fn set_skill_catalog_provider(&mut self, provider: dsh_agent_loop::SkillCatalogProvider) {
        self.engine.set_skill_catalog_provider(provider);
    }

    /// 装配 `/name` 手势注入回调(dsh-skill gesture_payloads)。
    pub fn set_skill_gesture_provider(&mut self, provider: dsh_agent_loop::SkillGestureProvider) {
        self.engine.set_skill_gesture_provider(provider);
    }

    /// 挂 hooks 拦截点(宿主装配;dsh-hooks HookPortImpl;M4.2)。
    pub fn set_hook_port(&mut self, port: std::sync::Arc<dyn dsh_agent_loop::hooks::HookPortObj>) {
        self.engine.set_hook_port(port);
    }

    /// 卸载 hooks 拦截点(热卸载)。
    pub fn clear_hook_port(&mut self) {
        self.engine.clear_hook_port();
    }

    /// 从既有日志恢复投影 retained(runtime 快照同源恢复;冷附着重开会话用)。
    pub fn restore_projection(&mut self) {
        self.engine.restore_projection();
    }

    /// 会话日志路径
    pub fn session_path(&self) -> &str {
        &self.session_path
    }

    /// 待批准计划:最近一条 plan/submitted 且其后无 plan/approved
    pub fn pending_plan(&self) -> Option<String> {
        let log = self.engine.log();
        let l = log.lock().ok()?;
        let submitted = l.iter().rev().find(|e| e.r#type == "plan/submitted")?;
        let plan = submitted.data["plan"].as_str()?.to_string();
        let resolved_after = l.iter().any(|e| {
            (e.r#type == "plan/approved" || e.r#type == "plan/cancelled") && e.seq > submitted.seq
        });
        if resolved_after { None } else { Some(plan) }
    }

    /// 追加会话级事件(mode 切换/计划批准;经 engine 唯一写入口)。
    /// 返回落档 seq——回声广播按 seq 定向,避免「只翻最后一条」被并发
    /// append 插队丢帧(见 registry broadcast_event 注释)
    pub fn session_event(&mut self, ty: &str, data: Value) -> Result<u64> {
        let Session {
            engine, backend, ..
        } = self;
        let mut sink = |ev: &EventEnvelope| {
            if let Err(e) = backend.append(ev) {
                eprintln!("持久化失败:{e}");
            }
        };
        let clock = wall_clock;
        engine
            .commit_session_event(ty, data, &clock, &mut sink)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

impl<T, TOOLS> Session<T, TOOLS>
where
    T: dsh_agent_loop::LlmTransport + dsh_agent_loop::Summarizer + Send,
    TOOLS: ToolPort + Send,
{
    /// 以日志中的 plan 态重建 header(模式切换/计划批准后生效)
    pub fn refresh_header(&mut self) {
        let header = {
            let log = self.engine.log();
            let Ok(l) = log.lock() else {
                return;
            };
            build_header(&self.parts, &l)
        };
        self.engine.set_header(header);
    }

    /// 驱动一个 turn:`on_event` 在每个事件落日志后同步触发
    /// (渲染源:记录优先——落日志 ⟺ 显示)。
    /// `input_id`:输入消息的持久 id(队列/steer 条目携带宿主预分配 id;
    /// None = 引擎生成 v7)。`images`:图片附件持久引用(准入在宿主
    /// prompt 侧完成)。
    pub async fn turn_with(
        &mut self,
        input: &str,
        input_id: Option<&str>,
        images: &[dsh_session::attachments::ImageAttachmentRef],
        contexts: &[serde_json::Value],
        on_event: &mut (dyn FnMut(&EventEnvelope) + Send),
    ) -> Result<TurnOutcome> {
        self.refresh_header(); // 模式/计划态自日志生效
        self.cancel.reset(); // 上一回合的取消不泄漏
        let Session {
            engine,
            gate,
            tools,
            backend,
            ..
        } = self;
        let mut sink = |ev: &EventEnvelope| {
            on_event(ev);
            if let Err(e) = backend.append(ev) {
                eprintln!("持久化失败:{e}");
            }
        };
        let clock = wall_clock;
        let outcome = engine
            .run_turn(
                input, input_id, images, contexts, gate, tools, &clock, &mut sink,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(outcome)
    }

    /// 驱动一个 turn(无渲染回调;无附件;无注入上下文)
    pub async fn turn(&mut self, input: &str) -> Result<TurnOutcome> {
        self.turn_with(input, None, &[], &[], &mut |_| {}).await
    }
}

/// 网关 header 重建器:每 turn 前按日志态(plan 模式/活跃计划)重建 prompt
pub fn header_rebuilder(parts: PromptParts) -> Box<dyn Fn(&EventLog) -> RequestHeader + Send> {
    Box::new(move |log| build_header(&parts, log))
}

/// 兼容入口:打开(或创建)会话日志后端——追加模式,不截断既有
/// 事实流(重开会话:load_log 恢复历史后继续 append)
pub fn open_backend(path: &str) -> Result<JsonlBackend> {
    JsonlBackend::open(path).with_context(|| format!("打开会话日志失败:{path}"))
}

/// 从 JSONL 会话文件重建事件日志(重开会话:模型可见历史与投影
/// 同源恢复)。文件缺失 = 空日志(新会话);每行经 [`dsh_session::
/// decode_envelope`] 读取方守卫,未知未标事件即拒(fail-closed)。
pub fn load_log(path: &str) -> Result<EventLog> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(EventLog::new()),
        Err(e) => return Err(anyhow::anyhow!("读取会话日志失败 {path}:{e}")),
    };
    let mut log = EventLog::new();
    for (n, line) in text.lines().enumerate() {
        let lineno = n + 1;
        if line.trim().is_empty() {
            continue;
        }
        let raw: Value =
            serde_json::from_str(line).with_context(|| format!("{path}:{lineno} 行解析失败"))?;
        let ev = dsh_session::decode_envelope(&raw)
            .map_err(|e| anyhow::anyhow!("{path}:{lineno} {e}"))?;
        log.append(ev)
            .map_err(|e| anyhow::anyhow!("{path}:{lineno} {e}"))?;
    }
    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::{dotenv_value, load_log, prompt_parts};

    #[test]
    fn prompt_parts_two_sentence_identity_with_interpolation() {
        // 身份两句式(harness:identity + preset persona):harness 句
        // 恒在,persona identity 经 {{model}}/{{cwd}} 插值追加
        let preset =
            dsh_host::PresetManifest::load(std::path::Path::new("/nonexistent"), "standard")
                .unwrap();
        let resolved = crate::Resolved {
            model: "test-model".into(),
            base_url: String::new(),
            session: String::new(),
            workspace: std::path::PathBuf::from("/tmp/ws"),
            dialect: String::new(),
            reasoning_effort: None,
            models: None,
            preset,
        };
        let parts = prompt_parts(&resolved, false);
        assert!(
            parts
                .identity
                .starts_with("You are an AI agent powered by dsh.\n\n"),
            "harness 句恒在且居首"
        );
        assert!(parts.identity.contains("powered by the test-model model"));
        assert!(parts.identity.contains("working directory is /tmp/ws"));
        assert!(!parts.identity.contains("{{"), "插值必须落值");
        // 工具指南节随装配收集,system 组装为无标题段落
        assert!(
            parts
                .tool_sections
                .iter()
                .any(|s| s.contains("[exit code: N]"))
        );
        let log = crate::fresh_log();
        let header = crate::build_header(&parts, &log.lock().expect("测试日志锁"));
        assert!(header.system.contains("Check the [exit code: N] marker"));
        assert!(!header.system.contains("# Check the"), "工具节无标题");
    }

    #[test]
    fn dotenv_parsing() {
        let text = "\
# comment
DEEPSEEK_API_KEY=sk-abc
OTHER=\"quoted value\"
EMPTY=
";
        assert_eq!(
            dotenv_value(text, "DEEPSEEK_API_KEY").as_deref(),
            Some("sk-abc")
        );
        assert_eq!(dotenv_value(text, "OTHER").as_deref(), Some("quoted value"));
        // 空值/缺失键/前缀撞名都取不到
        assert_eq!(dotenv_value(text, "EMPTY"), None);
        assert_eq!(dotenv_value(text, "MISSING"), None);
        assert_eq!(dotenv_value(text, "DEEPSEEK"), None);
    }

    #[test]
    fn load_log_roundtrip_and_guards() {
        let dir = std::env::temp_dir().join(format!("dsh-loadlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");

        // 事件 → JSONL 行(与 JsonlBackend 同形态:envelope 序列化)
        let mut log = dsh_session::EventLog::new();
        log.append(dsh_session::EventEnvelope::new(
            "user/message",
            0,
            serde_json::json!({ "content": "hi" }),
        ))
        .unwrap();
        log.append(dsh_session::EventEnvelope::new(
            "assistant/message",
            1,
            serde_json::json!({ "content": "yo" }),
        ))
        .unwrap();
        let text = log
            .iter()
            .map(|ev| serde_json::to_string(ev).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, text).unwrap();

        let re = load_log(path.to_str().unwrap()).unwrap();
        assert_eq!(re.snapshot(), log.snapshot(), "重载日志与原日志同构");
        assert_eq!(re.high_water(), 2);

        // 未知未标事件:读取方守卫拒绝(fail-closed)
        std::fs::write(
            &path,
            "{\"type\":\"evil/x\",\"seq\":1,\"time\":0,\"data\":{}}\n",
        )
        .unwrap();
        assert!(load_log(path.to_str().unwrap()).is_err());

        // 文件缺失 = 空日志(新会话)
        let missing = dir.join("none.jsonl");
        assert_eq!(load_log(missing.to_str().unwrap()).unwrap().high_water(), 0);
    }
}
