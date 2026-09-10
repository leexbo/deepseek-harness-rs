//! preset 装配器:在树组件注册表 + mount 行装载。
//!
//! 「一切皆插件」的 RS 形态:preset manifest 的每行 mount 装载一个组件——
//! 在树组件(编译期注册的 Rust 工具,注册名即 source)或本地 wasm 工具
//! 组件(路径形态,经 `dsh-host` 的 WasmTool 桥;OCI 引用随分发面)。
//! config 先过组件声明的 JSON Schema 子集(类型即校验,装配期拒绝),
//! 再透传给组件。
//!
//! 工具参数(permission/pty/宿主 port/会话工厂)永远来自宿主面
//! [`MountContext`],preset 面只选「装哪些组件 + 组件 config」——分界与
//! preset 体系一致。组件间接线(jobs↔bash 共享后台任务注册表、subagent
//! 的工具/控制对共享 registry)由在场集合与共享件显式表达,不再藏在
//! if 链里。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use dsh_agent_loop::ToolPortObj;
use dsh_host::validate_config;
use dsh_host::PresetManifest;
use dsh_session::EventLog;
use dsh_agent_loop::CancelToken;
use serde_json::{Value, json};

use crate::Resolved;

/// 装配器级共享件(组件间接线:首个索取者构造,后续复用)
#[derive(Default)]
struct MountShared {
    /// jobs↔bash 共享的后台任务注册表(JobTool 与 bash.with_jobs 同源)
    jobs: Mutex<Option<dsh_tools::JobsRegistry>>,
}

/// 装配上下文:宿主面参数(工具参数一律来自宿主面,与 preset 分界)
pub struct MountContext<'a> {
    /// 装配参数(模型路由/工作区/日志路径)
    pub resolved: &'a Resolved,
    /// 子代理/工作流独立传输所需 api_key
    pub api_key: &'a str,
    /// 共享日志(todo/plan/goal 落事件)
    pub log: &'a Arc<Mutex<EventLog>>,
    /// 软取消令牌(bash/子代理执行中 select 同源)
    pub cancel: &'a CancelToken,
    /// bash pty 模式
    pub pty: bool,
    /// 访问模式:read-only / workspace-write / full-access
    /// (mode_source 缺席时的静态兜底)
    pub permission: &'a str,
    /// 会话权限模式动态源(有 = 工具执行时实时解析,权限切换落档即
    /// 生效;缺 = 装配期静态 permission)
    pub mode_source: Option<dsh_tools::ModeSource>,
    /// 宿主审批闸门(有 = bash 支持 sandbox_permissions 一次性升级;
    /// 缺 = CLI/测试装配,带参逐字拒绝)
    pub approval: Option<Arc<dyn dsh_tools::ApprovalPort>>,
    /// session_query 宿主 port(缺 = 该组件跳过)
    pub query_port: Option<Arc<dyn dsh_tools::session_query::SessionQueryPort>>,
    /// ask_user_question 宿主 port(缺 = 该组件跳过)
    pub ask_port: Option<Arc<dyn dsh_tools::AskQuestionPort>>,
    /// 子代理会话工厂(桌面 attach 形态)
    pub session_factory: Option<Arc<dyn dsh_tools::subagent::SessionFactory>>,
    /// 子代理结算通知 port(缺 = subagent 同步语义)
    pub notify_port: Option<Arc<dyn dsh_tools::subagent::SettlementNotificationPort>>,
    /// 归属会话槽位 id(复合形态;ask 问答卡会话门控用)
    pub current_session: Option<&'a str>,
    /// 子代理宿主桥(jobs 帧 + 子会话事件实时流;缺 = CLI)
    pub subagent_bridge:
        Option<Arc<dsh_tools::subagent::SubagentBridge>>,
    /// 在场组件集合(jobs↔bash 接线判别)
    present: HashSet<&'a str>,
    /// 装配器级共享件
    shared: MountShared,
}

impl<'a> MountContext<'a> {
    /// 指定组件是否在场(同一 manifest 的 mounts 集合)
    fn present(&self, source: &str) -> bool {
        self.present.contains(source)
    }

    /// jobs↔bash 共享的后台任务注册表(懒构造)
    fn jobs_registry(&self) -> dsh_tools::JobsRegistry {
        let mut slot = self.shared.jobs.lock().expect("装配共享件锁中毒(宿主 bug)");
        if slot.is_none() {
            *slot = Some(Arc::new(Mutex::new(Vec::new())));
        }
        Arc::clone(slot.as_ref().expect("已构造"))
    }
}

/// 在树组件装配函数产出(0..n 个工具实例;persona 等非工具组件产空)
type MountFn = fn(&MountContext, &Value) -> Result<Vec<Box<dyn ToolPortObj>>>;

/// 在树组件(注册名 → config schema + 装配函数 + prompt 节)
pub struct InTreeComponent {
    /// config schema(JSON Schema 子集;装配期校验 preset config)
    pub schema: Value,
    /// 装配:吃上下文与 config,产工具实例
    pub mount: MountFn,
    /// 工具使用指南节(tool:<name> section;空 = 无节)。文本与
    /// 工具插件一致;工具名/接口不同处按接口适配(挂节处注明)。
    /// 行为指导承诺必须与 RS 实际接口一致:无后台参数的 subagent、
    /// 无完成通知的 jobs 不给完成承诺句。
    pub prompt: &'static str,
}

/// 在树组件注册表(编译期注册;source 命中即装载)
pub fn in_tree_registry() -> &'static HashMap<&'static str, InTreeComponent> {
    static REG: std::sync::OnceLock<HashMap<&'static str, InTreeComponent>> =
        std::sync::OnceLock::new();
    REG.get_or_init(|| {
        let object_schema = || json!({ "type": "object" });
        let mut m: HashMap<&'static str, InTreeComponent> = HashMap::new();
        for (name, schema) in [
            ("persona", persona_schema()),
            ("bash", object_schema()),
            ("files", object_schema()),
            ("todo_write", object_schema()),
            ("plan", object_schema()),
            ("goal", object_schema()),
            ("subagent", object_schema()),
            ("jobs", object_schema()),
            ("workflow", object_schema()),
            ("session_query", object_schema()),
            ("ask_user_question", object_schema()),
        ] {
            let mount = match name {
                "persona" => mount_persona,
                "bash" => mount_bash,
                "files" => mount_files,
                "todo_write" => mount_todo_write,
                "plan" => mount_plan,
                "goal" => mount_goal,
                "subagent" => mount_subagent,
                "jobs" => mount_jobs,
                "workflow" => mount_workflow,
                "session_query" => mount_session_query,
                "ask_user_question" => mount_ask,
                _ => unreachable!("注册表名与装配函数一一对应"),
            };
            m.insert(
                name,
                InTreeComponent {
                    schema,
                    mount,
                    prompt: component_prompt(name),
                },
            );
        }
        m
    })
}

/// 组件的模型面使用指南(源 tool:<name> section;文本出处见各行)。
/// todo_write/plan/ask_user_question 源本就无节;subagent 源节承诺后台
/// 运行(run_in_background/完成通知),仅结算通知 port 在场时挂
/// (见 [`tool_prompt_sections_with`])。
fn component_prompt(name: &str) -> &'static str {
    match name {
        // 源 packages/shell/tool-bash/src/index.ts(逐字)
        "bash" => {
            "Check the [exit code: N] marker on every bash result; investigate failures before moving on."
        }
        // 源 packages/fs/tool-fs/src/{read,edit}.ts 与 tool-fs-search/{glob,grep}.ts;
        // 工具名/参数名按 RS 接口适配(read→file_read、edit→file_edit、
        // glob+grep→file_search;file_edit 无 replace_all 参数故去该句;
        // 源 glob 节的 hidden/mtime 行为细节与 file_search 语义相反故不抄)
        "files" => {
            "Use the file_read tool — not shell commands like cat — to inspect text files. Results include line numbers. Use offset and limit to continue reading large files.\n\nUse the file_edit tool for targeted changes to existing UTF-8 text files. It replaces literal old_text with new_text; by default old_text must appear exactly once. If old_text appears multiple times, provide a more specific old_text. Read the file first (the default fs-observation-policy requires it), unless you just created or edited it in this session.\n\nUse the file_search tool — not shell find or grep — to discover files by path pattern or to search file contents. Use file_read on a matched file when you need surrounding context."
        }
        // 源 packages/jobs/tool-jobs/src/index.ts;RS jobs 为单工具
        // (list/read/stop)且无完成通知,「notified in-session / do not
        // busy-poll」承诺不成立 → 按接口适配
        "jobs" => {
            "Track every background job id you start; keep working on independent steps and do not duplicate a running job's work. Before giving a final answer, read every still-relevant job with the jobs tool (action read), and stop jobs that stopped mattering."
        }
        // 源 packages/goal/tool-goal/src/index.ts guidance();RS goal 为单
        // 工具(add/complete/list),无 goal_id/revision/resume/blocked 语义
        // → 按接口适配保留骨架
        "goal" => {
            "Use the goal tool for one long-running completion objective in the current session. State it with action add and mark complete only when the objective is actually achieved; do not create a goal for routine single-turn work."
        }
        // 源 packages/workflow/tool-workflow/src/index.ts(逐字)
        "workflow" => {
            "Use the workflow tool ONLY when the user explicitly asks for a workflow or for large multi-agent orchestration: you write a JavaScript script (the tool description documents the exact format) that fans work out across many subagents with phases and structured results. For one or two delegations, prefer plain subagent calls."
        }
        // 源 packages/session-query/tool-session-query/src/index.ts(逐字;
        // 五个工具名两侧一致)
        "session_query" => {
            "Use session_search to find relevant work from prior sessions, or session_event_search to search earlier events in one session. Search results are cursor-free and workspace-scoped. Follow a useful hit with session_trace, session_event_trace, or session_event_read when you need lineage, relationships, or exact data."
        }
        _ => "",
    }
}

/// subagent 使用指南节(源 tool:subagent section;逐字)。仅在 subagent
/// 装配出后台形态(结算通知 port 在场)时挂——节承诺的 run_in_background /
/// 结算通知语义此时才真实成立(行为承诺必须与接口一致)。
pub const SUBAGENT_BACKGROUND_SECTION: &str = "Use subagent in the background by default. Start independent delegations together in one assistant message and continue useful work while they run. Set `run_in_background: false` only when your next action depends on that subagent's result. When a background run settles, the runtime sends you a notice containing its outcome and any final assistant message.";

/// 收集 preset 在场组件的 prompt 节(manifest 行序;装配期一次)。
/// 不含条件节(subagent 后台节)——等价 `tool_prompt_sections_with(preset, false)`。
pub fn tool_prompt_sections(preset: &PresetManifest) -> Vec<&'static str> {
    tool_prompt_sections_with(preset, false)
}

/// 同 [`tool_prompt_sections`],`subagent_background` = subagent 是否装配为
/// 后台形态(结算通知 port 在场):真则 manifest 含 subagent 行时追加源节
pub fn tool_prompt_sections_with(
    preset: &PresetManifest,
    subagent_background: bool,
) -> Vec<&'static str> {
    let reg = in_tree_registry();
    preset
        .spec
        .mounts
        .iter()
        .filter_map(|m| {
            let source = m.source.as_str();
            if source == "subagent" {
                // 条件节:后台形态才有(其余组件用注册表静态节)
                return subagent_background.then_some(SUBAGENT_BACKGROUND_SECTION);
            }
            reg.get(source).map(|c| c.prompt).filter(|p| !p.is_empty())
        })
        .collect()
}

/// persona config schema(prompt 组件:身份/追加段可覆盖)
fn persona_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "identity": { "type": "string" },
            "append": { "type": "string" },
        },
    })
}

// ---- 各组件装配函数(自旧 build_tools 的 if 链逐字迁移;接线显式化)----

/// persona:prompt 组件(config = identity/append;消费在 `prompt_parts`)
fn mount_persona(_ctx: &MountContext, _cfg: &Value) -> Result<Vec<Box<dyn ToolPortObj>>> {
    Ok(vec![])
}

    /// bash:持久 shell(策略执行时从动态源解析——read-only 走只读沙箱
    /// 策略,读命令可用、写被内核拦;jobs 在场时装备后台任务能力)
fn mount_bash(ctx: &MountContext, _cfg: &Value) -> Result<Vec<Box<dyn ToolPortObj>>> {
    let mut bash = dsh_tools::BashTool::new(&ctx.resolved.workspace).with_cancel(ctx.cancel.clone());
    match &ctx.mode_source {
        Some(src) => {
            bash = bash.with_mode_source(Arc::clone(src));
        }
        None => {
            // 静态装配(CLI/测试):full-access 全盘可写,read-only 无可写根
            // (仍要求可用 rung,fail-closed,不设「danger = 无沙箱」通道)
            if ctx.permission == "full-access" {
                bash = bash.with_policy(dsh_sandbox::SandboxPolicy::full_access());
            } else if ctx.permission == "read-only" {
                bash = bash.with_policy(dsh_sandbox::SandboxPolicy::read_only());
            }
        }
    }
    if let Some(port) = &ctx.approval {
        bash = bash.with_approval_port(Arc::clone(port));
    }
    if ctx.present("jobs") {
        bash = bash.with_jobs(ctx.jobs_registry());
    }
    if ctx.pty {
        bash = bash.with_pty();
    }
    Ok(vec![Box::new(bash)])
}

/// files:文件读写工具(read-only 权限时 file_edit 拒写;动态源优先)
fn mount_files(ctx: &MountContext, _cfg: &Value) -> Result<Vec<Box<dyn ToolPortObj>>> {
    let mut files = dsh_tools::FileTools::new(&ctx.resolved.workspace);
    files = match &ctx.mode_source {
        Some(src) => files.with_mode_source(Arc::clone(src)),
        None => {
            if ctx.permission == "read-only" {
                files.with_readonly()
            } else {
                files
            }
        }
    };
    Ok(vec![Box::new(files)])
}

/// todo_write:任务清单工具(共享日志)
fn mount_todo_write(ctx: &MountContext, _cfg: &Value) -> Result<Vec<Box<dyn ToolPortObj>>> {
    Ok(vec![Box::new(dsh_tools::TodoWriteTool::new(Arc::clone(
        ctx.log,
    )))])
}

/// plan:计划模式工具(共享日志)
fn mount_plan(ctx: &MountContext, _cfg: &Value) -> Result<Vec<Box<dyn ToolPortObj>>> {
    Ok(vec![Box::new(dsh_tools::PlanTool::new(Arc::clone(ctx.log)))])
}

/// goal:目标工具(共享日志)
fn mount_goal(ctx: &MountContext, _cfg: &Value) -> Result<Vec<Box<dyn ToolPortObj>>> {
    Ok(vec![Box::new(dsh_tools::GoalTool::new(Arc::clone(ctx.log)))])
}

/// 子代理独立传输工厂(subagent/workflow 各持一份)。工厂形态——
/// 每个子代理执行时独立构建出网传输,后台并发不被单一传输钉死。
fn make_subagent(
    ctx: &MountContext,
    with_notify: bool,
) -> Result<dsh_tools::SubagentTool<dsh_llm::HttpTransport>> {
    let resolved = ctx.resolved.clone();
    let api_key = ctx.api_key.to_string();
    let transport_factory: dsh_tools::subagent::TransportFactory<dsh_llm::HttpTransport> =
        Arc::new(move || {
            crate::build_raw_transport(&resolved, &api_key, None).map_err(|e| e.to_string())
        });
    let mut t = dsh_tools::SubagentTool::new(
        &ctx.resolved.workspace,
        transport_factory,
        ctx.resolved.model.clone(),
    )
    .with_cancel(ctx.cancel.clone());
    if let Some(f) = &ctx.session_factory {
        t = t
            .with_session_factory(f.clone())
            .with_parent_id(ctx.current_session.unwrap_or_default());
    }
    // 结算通知 port 仅顶层委派工具注入(workflow/ralph 是前台编排,子代理
    // 结果由编排器收集,不投通知)
    if with_notify && let Some(n) = &ctx.notify_port {
        t = t.with_notify(n.clone());
    }
    // 会话检索 port:子代理工具面扩装
    if let Some(port) = &ctx.query_port {
        t = t.with_query_port(port.clone());
    }
    Ok(t)
}

/// subagent:子代理工具 + 控制对(共享 registry;注册表统一,全可见)。
/// 装配即做重启恢复扫描:父会话下中断的驻留子代理重挂 + 投
/// 「已恢复」通知,已结算的重挂为可续话。
fn mount_subagent(ctx: &MountContext, _cfg: &Value) -> Result<Vec<Box<dyn ToolPortObj>>> {
    let mut subagent = make_subagent(ctx, true)?;
    subagent.resume_children();
    let registry = subagent.registry.clone();
    // 注册表接线宿主(jobs 帧 + 子会话事件实时流)
    if let (Some(bridge), Some(session_id)) = (&ctx.subagent_bridge, ctx.current_session) {
        (bridge.jobs)(session_id, registry.clone());
        subagent = subagent.with_event_sink(bridge.events.clone());
    }
    Ok(vec![
        Box::new(subagent),
        Box::new(dsh_tools::SubagentControlTool::new(registry)),
    ])
}

/// jobs:后台任务工具(与 bash 共享注册表)
fn mount_jobs(ctx: &MountContext, _cfg: &Value) -> Result<Vec<Box<dyn ToolPortObj>>> {
    Ok(vec![Box::new(dsh_tools::JobTool::new(ctx.jobs_registry()))])
}

/// workflow:工作流编排(workflow + ralph,各持独立子代理传输;前台编排
/// 不注入结算通知)
fn mount_workflow(ctx: &MountContext, _cfg: &Value) -> Result<Vec<Box<dyn ToolPortObj>>> {
    Ok(vec![
        Box::new(dsh_tools::WorkflowTool::new(make_subagent(ctx, false)?)),
        Box::new(dsh_tools::RalphTool::new(make_subagent(ctx, false)?)),
    ])
}

/// session_query:会话检索工具(需宿主 port,缺 = 跳过)
fn mount_session_query(ctx: &MountContext, _cfg: &Value) -> Result<Vec<Box<dyn ToolPortObj>>> {
    let Some(port) = ctx.query_port.clone() else {
        return Ok(vec![]);
    };
    // 归属会话 = 本日志(路径派生 id 由宿主面定;此处用会话目录名)
    let current = ctx
        .resolved
        .session
        .rsplit('/')
        .nth(1)
        .unwrap_or_default()
        .to_string();
    Ok(vec![Box::new(
        dsh_tools::session_query::SessionQueryTool::new(port, &current),
    )])
}

/// ask_user_question:问答工具(需宿主 port,缺 = 跳过)
fn mount_ask(ctx: &MountContext, _cfg: &Value) -> Result<Vec<Box<dyn ToolPortObj>>> {
    let Some(port) = ctx.ask_port.clone() else {
        return Ok(vec![]);
    };
    // 归属会话 = 调用方传入的**槽位 id**(非默认工作区为 "<ws>/<stem>" 复合
    // 形式;此前从文件路径反推裸 stem,与桌面当前会话复合 id 不相等 →
    // question/requested 帧被问答卡会话门控整批跳过 = 不弹窗)
    let current = ctx
        .current_session
        .map(String::from)
        .unwrap_or_else(|| {
            ctx.resolved
                .session
                .rsplit('/')
                .nth(1)
                .unwrap_or_default()
                .to_string()
        });
    Ok(vec![Box::new(dsh_tools::AskQuestionTool::new(port, &current))])
}

// ---- source 三态判别 ----

/// source 引用形态(在树注册表未命中时)
enum SourceKind {
    /// 本地 wasm 工具组件(相对 workspace 解析)
    WasmPath(std::path::PathBuf),
    /// OCI 引用(registry/name:tag@digest;拉取随分发面)
    Oci(String),
    /// 无法判别
    Unknown(String),
}

/// source 三态判别:在树注册名(调用方已查)→ 路径 → OCI → 未知。
/// 路径形态:`./`、`/` 前缀或 `.wasm` 后缀;OCI 形态:首段(host)含
/// `.`/`:`、或带 `:tag`/`@digest` 后缀
fn classify_source(workspace: &std::path::Path, source: &str) -> SourceKind {
    if source.starts_with("./") || source.starts_with("../") || source.starts_with('/') {
        return SourceKind::WasmPath(workspace.join(source));
    }
    if source.ends_with(".wasm") {
        return SourceKind::WasmPath(workspace.join(source));
    }
    let host = source.split('/').next().unwrap_or(source);
    let tail = source.rsplit('/').next().unwrap_or(source);
    if host.contains('.') || host.contains(':') || tail.contains(':') || source.contains('@') {
        return SourceKind::Oci(source.to_string());
    }
    SourceKind::Unknown(source.to_string())
}

/// preset 驱动的工具装配:遍历 mount 行 → 在树注册表(未命中走三态判别)
/// → config 过 schema → 装配。
/// `permission` 为访问模式:read-only(无 bash,文件只读)/
/// workspace-write(默认:可写根 = workspace)/ full-access(bash 全盘可写)
#[allow(clippy::too_many_arguments)]
pub fn assemble(
    resolved: &Resolved,
    api_key: &str,
    log: &Arc<Mutex<EventLog>>,
    cancel: &CancelToken,
    pty: bool,
    permission: &str,
    mode_source: Option<dsh_tools::ModeSource>,
    approval_port: Option<Arc<dyn dsh_tools::ApprovalPort>>,
    query_port: Option<Arc<dyn dsh_tools::session_query::SessionQueryPort>>,
    ask_port: Option<Arc<dyn dsh_tools::AskQuestionPort>>,
    session_factory: Option<Arc<dyn dsh_tools::subagent::SessionFactory>>,
    notify_port: Option<Arc<dyn dsh_tools::subagent::SettlementNotificationPort>>,
    current_session: Option<&str>,
    subagent_bridge: Option<Arc<dsh_tools::subagent::SubagentBridge>>,
) -> Result<Vec<Box<dyn ToolPortObj>>> {
    let present: HashSet<&str> = resolved
        .preset
        .spec
        .mounts
        .iter()
        .map(|m| m.source.as_str())
        .collect();
    let ctx = MountContext {
        resolved,
        api_key,
        log,
        cancel,
        pty,
        permission,
        mode_source,
        approval: approval_port,
        query_port,
        ask_port,
        session_factory,
        notify_port,
        current_session,
        subagent_bridge,
        present,
        shared: MountShared::default(),
    };
    let registry = in_tree_registry();
    let mut tools: Vec<Box<dyn ToolPortObj>> = Vec::new();
    for mount in &resolved.preset.spec.mounts {
        if let Some(component) = registry.get(mount.source.as_str()) {
            let cfg = mount.config_object();
            validate_config(&component.schema, &cfg)
                .map_err(|e| anyhow::anyhow!("组件 {} 配置校验失败:{e}", mount.source))?;
            tools.extend((component.mount)(&ctx, &cfg)?);
        } else {
            match classify_source(&resolved.workspace, &mount.source) {
                SourceKind::WasmPath(path) => {
                    // 本地 wasm 工具组件:内容寻址注册 + init(config-schema 校验)
                    // + describe(装载期 fail-fast,见 dsh-host tool_component)
                    tools.push(Box::new(dsh_host::WasmTool::new(
                        &path,
                        &mount.config_object(),
                        ctx.cancel.clone(),
                    )?));
                }
                SourceKind::Oci(reference) => {
                    bail!("OCI 镜像拉取随分发面启用:{reference}");
                }
                SourceKind::Unknown(source) => {
                    bail!("未知组件源 {source}(在树注册表未命中,也不是路径/OCI 引用形态)");
                }
            }
        }
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小 manifest 的 Resolved(装配器测试底座)
    fn resolved_with(mounts_yaml: &str) -> Resolved {
        let text = format!(
            "apiVersion: dsh/v1\nkind: Preset\nmetadata:\n  name: test\n  description: d\nspec:\n  mounts:\n{mounts_yaml}"
        );
        let preset = dsh_host::PresetManifest::parse(&text, "<test>").unwrap();
        Resolved {
            model: "m".into(),
            base_url: "https://example.invalid".into(),
            session: "dir/s.jsonl".into(),
            workspace: std::path::PathBuf::from("."),
            dialect: "openai-chat".into(),
            reasoning_effort: None,
            models: None,
            preset,
        }
    }

    fn sources(resolved: &Resolved, permission: &str) -> Vec<String> {
        let log = crate::fresh_log();
        let cancel = CancelToken::new();
        let tools = assemble(
            resolved,
            "key",
            &log,
            &cancel,
            false,
            permission,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        tools
            .iter()
            .flat_map(|t| t.specs())
            .map(|spec| spec["function"]["name"].as_str().unwrap_or("?").to_string())
            .collect()
    }

    #[test]
    fn standard_assembles_full_toolset() {
        let preset = dsh_host::PresetManifest::load(std::path::Path::new("/nonexistent"), "standard")
            .unwrap();
        let resolved = Resolved {
            preset,
            ..resolved_with("")
        };
        let names = sources(&resolved, "workspace-write");
        // 与旧 build_tools 等价:bash + files 三件 + todo/plan/goal + subagent 对
        // (subagent/list_agents/send_message/interrupt_agent)+ jobs + workflow 对
        // (workflow/ralph);session_query/ask_user_question 无宿主 port 跳过。
        // 控制工具三件:list_agents + send_message + interrupt_agent
        for expect in [
            "bash",
            "file_read",
            "file_edit",
            "file_search",
            "todo_write",
            "exit_plan_mode",
            "goal",
            "subagent",
            "list_agents",
            "send_message",
            "interrupt_agent",
            "jobs",
            "workflow",
            "ralph",
        ] {
            assert!(names.contains(&expect.to_string()), "{expect} 缺席:{names:?}");
        }
        assert_eq!(names.len(), 14, "standard 装载 14 工具声明:{names:?}");
        assert!(!names.iter().any(|n| n == "session_query"), "无 port 应跳过");
    }

    #[test]
    fn minimal_assembles_bash_files_only() {
        let preset = dsh_host::PresetManifest::load(std::path::Path::new("/nonexistent"), "minimal")
            .unwrap();
        let resolved = Resolved {
            preset,
            ..resolved_with("")
        };
        let names = sources(&resolved, "workspace-write");
        assert_eq!(
            names,
            vec![
                "bash".to_string(),
                "file_read".to_string(),
                "file_edit".to_string(),
                "file_search".to_string(),
            ],
            "minimal = bash + files 三件"
        );
    }

    #[test]
    fn readonly_mounts_bash_with_readonly_policy_files_readonly() {
        // read-only 不再压制 bash:恒装配、静态兜底走只读沙箱策略(读命令
        // 可用,写被内核拦带拒绝标记);files 声明不变(执行面只读)
        let resolved = resolved_with("    - source: bash\n    - source: files\n");
        let names = sources(&resolved, "read-only");
        assert!(names.contains(&"bash".to_string()), "read-only 仍装配 bash");
        assert!(names.contains(&"file_read".to_string()));
        assert!(names.contains(&"file_edit".to_string()));
    }

    #[test]
    fn jobs_presence_wires_bash_background() {
        // jobs 在场:装配出 jobs 工具;仅 bash:无
        let with_jobs = resolved_with("    - source: bash\n    - source: jobs\n");
        assert!(sources(&with_jobs, "workspace-write").contains(&"jobs".to_string()));
        let bare = resolved_with("    - source: bash\n");
        assert!(!sources(&bare, "workspace-write").contains(&"jobs".to_string()));
    }

    #[test]
    fn tool_prompt_sections_follow_manifest_order() {
        // standard 内置:bash/files/jobs/goal/workflow/session_query 六节
        // (todo_write/plan/ask_user_question/subagent 源本无节或语义未达)
        let preset =
            dsh_host::PresetManifest::load(std::path::Path::new("/nonexistent"), "standard")
                .unwrap();
        let secs = tool_prompt_sections(&preset);
        assert_eq!(secs.len(), 6, "standard 应有六节");
        assert!(secs[0].contains("[exit code: N]"), "bash 节按行序在首");
        assert!(secs[1].contains("file_read"), "files 节次之");
        assert!(secs.iter().any(|s| s.contains("Use session_search")));
        assert!(secs.iter().any(|s| s.contains("workflow tool ONLY")));
        // minimal = persona+bash+files → 两节(persona 无节)
        let minimal =
            dsh_host::PresetManifest::load(std::path::Path::new("/nonexistent"), "minimal")
                .unwrap();
        assert_eq!(tool_prompt_sections(&minimal).len(), 2);
    }

    #[test]
    fn subagent_mounts_without_prompt_section() {
        // 无结算通知 port = 同步语义 → 不挂后台节(行为承诺与接口一致)
        let resolved = resolved_with("    - source: subagent\n");
        assert!(tool_prompt_sections(&resolved.preset).is_empty());
        // port 在场 = 后台形态 → 挂源节(逐字,承诺 run_in_background/结算通知)
        let secs = tool_prompt_sections_with(&resolved.preset, true);
        assert_eq!(secs.len(), 1);
        assert!(secs[0].starts_with("Use subagent in the background by default."));
        assert!(secs[0].contains("run_in_background: false"));
    }

    #[test]
    fn unknown_source_rejected() {
        let resolved = resolved_with("    - source: no-such-thing\n");
        let log = crate::fresh_log();
        let Err(err) = assemble(
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
        ) else {
            panic!("未知组件源应拒绝");
        };
        assert!(err.to_string().contains("未知组件源"), "{err}");
    }

    #[test]
    fn oci_reference_reported_as_pending() {
        let resolved = resolved_with("    - source: registry.example.com/foo/bar:1.0.0\n");
        let log = crate::fresh_log();
        let Err(err) = assemble(
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
        ) else {
            panic!("OCI 引用应报分发面待启用");
        };
        assert!(err.to_string().contains("OCI"), "{err}");
    }

    #[test]
    fn classify_source_three_kinds() {
        let ws = std::path::Path::new("/ws");
        assert!(matches!(
            classify_source(ws, "./tools/x.wasm"),
            SourceKind::WasmPath(_)
        ));
        assert!(matches!(
            classify_source(ws, "tools/x.wasm"),
            SourceKind::WasmPath(_)
        ));
        assert!(matches!(
            classify_source(ws, "registry.io/foo/bar:1.0"),
            SourceKind::Oci(_)
        ));
        assert!(matches!(
            classify_source(ws, "localhost:5000/foo"),
            SourceKind::Oci(_)
        ));
        assert!(matches!(
            classify_source(ws, "foo/bar@sha256:abc"),
            SourceKind::Oci(_)
        ));
        assert!(matches!(
            classify_source(ws, "just-a-name"),
            SourceKind::Unknown(_)
        ));
    }
}
