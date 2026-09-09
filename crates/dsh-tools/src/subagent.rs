//! 子代理工具:
//! 默认后台委派 + 结算通知回投 + 续话控制 + 重启恢复。
//!
//! 结构:主会话的 tool/call 触发 → 独立子会话(自己的 EventLog 与
//! JSONL 文件,会话边界 = 日志边界)→ 嵌套 LoopEngine 跑任务。
//! 两种委派形态:
//! - **后台(默认)**:立即返回子代理 id,子代理在驻留任务里跑;结算时经
//!   [`SettlementNotificationPort`] 把一条 `subagent-settled` 通知投回父
//!   会话并唤醒(父空闲=下一 turn,忙碌=step 边界认领);子代理驻留
//!   idle,`send_message` 可续话(运行中=steer 中途插话,idle=开新 turn);
//!   每次结算都再通知。子会话首事件写
//!   `subagent/descriptor`、每次结算写 `subagent/settled`——重启后父会话
//!   attach 扫描无 settled 尾的子会话即可 cold-resume(持久驻留)。
//! - **前台(run_in_background=false)**:同步等待,最终报告作为 tool/result
//!   返回(workflow/ralph 编排器走同一前台入口)。
//!
//! 能力束窄化:子代理可写根 = 主 workspace 下的 `.dsh/subagents/<id>/`
//! (读全盘、写限子根)。子工具面 = 全量 minus 递归:bash(+jobs)/文件三件/
//! todo_write/goal/session_query(port 在场)/send_message(回发父,仅驻留);
//! 排除 subagent/workflow(递归)、ask_user_question(子代理不能向人提问)、
//! plan(计划模式是与人的审批契约)、wasm 组件(实例不可克隆)。
//! 取消传播:父令牌触发 → 子静默退出(不通知,避免唤醒已取消会话);
//! `interrupt_agent` 只停当前 turn(每 turn 新令牌),子代理保持可续话。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use dsh_agent_loop::{
    CancelToken, LlmEvent, LlmTransport, LoopEngine, LoopError, RequestHeader, SteerInput,
    Summarizer, ToolCallRequest, ToolOutput, ToolPort, ToolPortObj,
};
use dsh_host::JsonlBackend;
use dsh_llm::InvariantGate;
use dsh_session::{EventEnvelope, EventLog};
use serde_json::{Value, json};

/// 子代理会话句柄(会话化血缘的产出:槽位 id 供通知卡跳转/续话寻址,
/// 路径供独立重放)
#[derive(Debug, Clone)]
pub struct SubagentSessionHandle {
    /// 子会话槽位 id(非默认工作区为 "<ws>/<stem>" 复合形态)
    pub session_id: String,
    /// 子会话 session.jsonl 路径
    pub session_path: std::path::PathBuf,
}

/// 子会话工厂(会话化 + 句柄化 + 重启恢复):让 subagent
/// 子任务注册为带血缘(parentSessionId + origin:'subagent')的独立会话。
/// 实现方:dsh-core AppHost。None = 未注入,回落自建 `.dsh/subagents/<id>/`。
pub trait SessionFactory: Send + Sync {
    /// 创建带血缘的子会话,返回其槽位 id 与 session.jsonl 路径
    fn create_subagent(&self, parent: &str) -> SubagentSessionHandle;

    /// 扫描可重挂的驻留子代理(有 descriptor 标记者):返回
    /// (句柄, 是否中断[末事件非 settled], label, prompt)。实现方负责认领
    /// 防双挂(已认领的不返回);中断者由调用方先冷修夏(实现方完成)再重挂。
    fn resumable_children(
        &self,
        parent: &str,
    ) -> Vec<(SubagentSessionHandle, bool, String, String)> {
        let _ = parent;
        Vec::new()
    }

    /// 驻留退出释放认领(默认 no-op)
    fn release_child(&self, session_id: &str) {
        let _ = session_id;
    }
}

/// 通知 port:子代理 → 父会话的单向通知通道(结算通知 kind=
/// "subagent-settled";子代理回发消息 kind="subagent-message")。宿主实现:
/// 经 Job::Notice 入父会话 steer 通道——父空闲=唤醒下一 turn,忙碌=引擎
/// step 边界认领(followup/steer 双语义)。父不存在时静默丢弃
/// (父不再存活不是错误,子会话自身即持久记录)。
pub trait SettlementNotificationPort: Send + Sync {
    /// 投递一条通知(text = 模型可见文本;source = 染色载荷)。
    fn notify(
        &self,
        parent_session: &str,
        text: String,
        source: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

/// 结算通知拼装(结算摘要 + 结算通知的消息形态):
/// 返回 (模型可见文本, source 染色载荷)。
pub fn settlement_notice(
    child_id: &str,
    stop_reason: &str,
    closing: Option<&str>,
) -> (String, Value) {
    let subject = format!("Background subagent {child_id}");
    let summary = match stop_reason {
        "completed" => {
            format!("{subject} finished and will do no further work unless you send it more.")
        }
        "aborted" => format!("{subject} was stopped before it finished."),
        "error" => format!("{subject} failed before it finished."),
        other => format!("{subject} ended abnormally ({other}) before it finished."),
    };
    let mut text = summary.clone();
    match closing {
        Some(closing) if !closing.trim().is_empty() => {
            text.push_str("\n\nIts closing message:\n\n");
            text.push_str(closing);
        }
        _ => text.push_str("\n\nIt left no closing message."),
    }
    let source = json!({
        "kind": "subagent-settled",
        "form": "notice",
        "summary": summary,
        "senderSessionId": child_id,
    });
    (text, source)
}

/// 重启恢复通知(中断重挂时投父)
fn resumed_notice(child_id: &str) -> (String, Value) {
    let summary = format!("Background subagent {child_id} was interrupted by a host restart and has been resumed.");
    let text = format!(
        "{summary} Its last turn did not finish; send it a message with `send_message` to continue."
    );
    let source = json!({
        "kind": "subagent-settled",
        "form": "notice",
        "summary": summary,
        "senderSessionId": child_id,
    });
    (text, source)
}

/// 子代理回发父消息拼装(kind=subagent-message,桌面独立卡形态)
fn child_message_notice(self_id: &str, message: &str) -> (String, Value) {
    let text = format!("Message from subagent {self_id}:\n\n{message}");
    let summary: String = message
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(80)
        .collect();
    let source = json!({
        "kind": "subagent-message",
        "form": "notice",
        "summary": summary,
        "senderSessionId": self_id,
    });
    (text, source)
}

/// 子代理独立传输工厂:每个子代理构建自己的出网传输——后台并发
/// 不再被单一传输钉死。实现方:mount 面(每次 build_raw_transport)与
/// 测试(脚本组队列逐个弹出)。
pub type TransportFactory<T> = Arc<dyn Fn() -> Result<T, String> + Send + Sync>;

/// 驻留子代理的续话消息(send_message 投递;逐条一个 turn)
#[derive(Debug)]
pub enum ChildMsg {
    /// 续话 prompt(作为子代理的下一 turn 输入)
    Prompt(String),
}

/// 子代理注册项(进程内记录;子会话日志文件才是持久事实)。
/// 前台(一次性)子代理只用到前半;后台(驻留)子代理额外携带续话
/// 通道、中途插话缓冲与打断句柄。
#[derive(Clone)]
pub struct SubagentRecord {
    /// 子代理标识(单调递增;进程内簿记,模型面用 session_id 寻址)
    pub id: u64,
    /// 任务描述(subagent 的 description 参数;list_agents 的 label)
    pub task: String,
    /// 状态:running / idle / done / failed / cancelled / stopped
    /// (后台驻留:running↔idle 循环;前台一次性:终态)
    pub status: String,
    /// 子会话槽位 id(血缘跳转 / send_message 寻址 / 通知卡跳转)
    pub session_id: String,
    /// 子会话日志路径(独立重放入口)
    pub session_path: String,
    /// 是否后台驻留子代理(list_agents 只列后台;前台一次性不列——
    /// "one-shot children cannot be continued, so the model never selects them")
    pub background: bool,
    /// 续话通道(后台驻留;None = 前台一次性)
    pub tx: Option<tokio::sync::mpsc::UnboundedSender<ChildMsg>>,
    /// 中途插话缓冲(运行中 send_message 的落点,引擎 step 边界认领)
    pub steer: Option<Arc<Mutex<VecDeque<SteerInput>>>>,
    /// 当前 turn 打断句柄(interrupt_agent;只停正在跑的 turn)
    pub stop: Option<Arc<tokio::sync::Notify>>,
    /// 委派 prompt(副行展示;重挂取自 descriptor)
    pub prompt: String,
    /// 委派/重挂时刻(ms)
    pub started_at: i64,
    /// 终态时刻(ms;None = 仍在驻留)
    pub ended_at: Option<i64>,
}

/// 注册表状态变化观察回调(宿主据此广播 jobs 帧;回调内禁止再锁 records)
pub type RegistryChangeHook = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Default)]
pub struct SubagentRegistry {
    inner: Arc<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    records: Mutex<Vec<SubagentRecord>>,
    on_change: Mutex<Option<RegistryChangeHook>>,
}

impl SubagentRegistry {
    /// 挂状态变化观察回调(覆盖式;装配时由宿主接线)
    pub fn set_on_change(&self, hook: Option<RegistryChangeHook>) {
        *self.inner.on_change.lock().expect("on_change 锁中毒") = hook;
    }

    /// 状态变化通知(guard 释放后调用,防回调重入死锁)
    fn changed(&self) {
        let hook = self.inner.on_change.lock().expect("on_change 锁中毒").clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// 弱引用(宿主 jobs 源登记;不阻止注册表随工具释放)
    pub fn downgrade(&self) -> WeakRegistry {
        WeakRegistry(Arc::downgrade(&self.inner))
    }

    /// 快照(background 驻留子代理;前台一次性不进 jobs 呈现)
    pub fn background_records(&self) -> Vec<SubagentRecord> {
        self.inner
            .records
            .lock()
            .expect("records 锁中毒")
            .iter()
            .filter(|r| r.background)
            .cloned()
            .collect()
    }
}

impl std::ops::Deref for SubagentRegistry {
    type Target = Mutex<Vec<SubagentRecord>>;

    fn deref(&self) -> &Self::Target {
        &self.inner.records
    }
}

/// 子会话事件出口(宿主据此向客户端广播子会话实时流;参数 =
/// 子会话槽位 id、引擎事件)。None = 只落盘不广播(CLI/单测)。
pub type SubagentEventSink = Arc<dyn Fn(&str, &EventEnvelope) + Send + Sync>;

/// jobs 呈现接线(宿主登记注册表为父会话的 jobs 源)
pub type JobsBinding = Arc<dyn Fn(&str, SubagentRegistry) + Send + Sync>;

/// 子代理宿主桥:jobs 呈现接线 + 子会话事件实时流转发,
/// 装配时由宿主(dsh-core)构造、经 MountContext 注入
pub struct SubagentBridge {
    /// 登记注册表为某父会话的 jobs 源(状态变化 → session/jobs 帧)
    pub jobs: JobsBinding,
    /// 子会话事件出口(引擎事件 → session/event 实时流)
    pub events: SubagentEventSink,
}

/// 注册表弱引用(宿主持有;upgrade 失败 = 工具已随会话释放)
#[derive(Clone, Default)]
pub struct WeakRegistry(std::sync::Weak<RegistryInner>);

impl WeakRegistry {
    /// 升级为强引用(失败 = 已释放)
    pub fn upgrade(&self) -> Option<SubagentRegistry> {
        self.0.upgrade().map(|inner| SubagentRegistry { inner })
    }
}

/// 子代理共享装配产出(前台/驻留两条执行路径共用)
struct ChildParts {
    /// 子可写根(bash 能力束窄化 + FileTools 根)
    child_root: std::path::PathBuf,
    header: RequestHeader,
    log: Arc<Mutex<EventLog>>,
    backend: JsonlBackend,
    /// 子代理自己的后台任务注册表(bash 后台 + jobs 工具共享)
    jobs: crate::JobsRegistry,
}

/// 子代理 system prompt(公共骨架;驻留形态追加回发父指引)
fn child_system_prompt(child_root: &std::path::Path, parent_link: Option<&ChildParentLink>) -> String {
    let mut system = format!(
        "You are a dsh subagent executing one task in an isolated workspace ({}). \
Finish the task and reply with the result only — you cannot ask questions.",
        child_root.display()
    );
    if let Some(link) = parent_link {
        // 可续话子代理指引:告知父 id 与 send_message 通路
        system.push_str(&format!(
            "\n\nYour parent agent id is {}. Send it messages with `send_message` when it must \
know something before you finish; your final reply is also delivered to it automatically.",
            link.parent_id
        ));
    }
    system
}

/// 子代理共享装配:子目录、子 header、日志与持久化后端、后台任务注册表。
/// 工具集按 turn 构建([`build_child_tools`])——取消令牌每 turn 一枚,
/// 运行中的 bash 进程靠它中断。
async fn prepare_child_parts(
    handle: &SubagentSessionHandle,
    model: &str,
    parent_link: Option<&ChildParentLink>,
) -> Result<ChildParts, String> {
    let child_root = handle
        .session_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    tokio::fs::create_dir_all(&child_root)
        .await
        .map_err(|e| format!("subagent workspace create failed: {e}"))?;
    let header = RequestHeader {
        model: model.to_string(),
        system: child_system_prompt(&child_root, parent_link),
        temperature: 0.0,
        reasoning_effort: None,
        tools: Vec::new(),
    };
    let log = Arc::new(Mutex::new(EventLog::new()));
    // open(追加,缺则建)而非 create(截断):重挂形态必须保全既有历史
    let backend = JsonlBackend::open(&handle.session_path)
        .map_err(|e| format!("subagent session create failed: {e}"))?;
    Ok(ChildParts {
        child_root,
        header,
        log,
        backend,
        jobs: Arc::new(Mutex::new(Vec::new())),
    })
}

/// 子代理回发父的链路(仅驻留子代理装配 send_message 工具时持有)
#[derive(Clone)]
pub(crate) struct ChildParentLink {
    pub(crate) parent_id: String,
    pub(crate) self_id: String,
    pub(crate) notify: Arc<dyn SettlementNotificationPort>,
}

/// 子代理工具面构建上下文(每 turn 重建工具集时的全部权属)
struct ChildToolContext<'a> {
    turn_token: &'a CancelToken,
    parts: &'a ChildParts,
    query_port: Option<Arc<dyn crate::session_query::SessionQueryPort>>,
    session_id: &'a str,
    parent_link: Option<ChildParentLink>,
}

/// 按 turn 构建子工具集(全量 minus 递归;bash 携带本 turn 取消令牌,
/// 运行中进程随令牌中断):
/// bash(+jobs 后台)/ 文件三件 / todo_write / goal / jobs /
/// session_query(port 在场)/ send_message 回发父(驻留子代理)。
/// 排除:subagent/workflow(递归)、ask_user_question(不能向人提问)、
/// plan(与人的审批契约)、wasm 组件(实例不可克隆)。
fn build_child_tools(ctx: ChildToolContext) -> Result<dsh_agent_loop::ToolSet, String> {
    let child_bash =
        crate::BashTool::new(&ctx.parts.child_root).with_cancel(ctx.turn_token.clone());
    let child_bash = child_bash.with_jobs(ctx.parts.jobs.clone());
    let mut tools: Vec<Box<dyn ToolPortObj>> = vec![
        Box::new(child_bash),
        Box::new(crate::FileTools::new(&ctx.parts.child_root)),
        Box::new(crate::TodoWriteTool::new(Arc::clone(&ctx.parts.log))),
        Box::new(crate::GoalTool::new(Arc::clone(&ctx.parts.log))),
        Box::new(crate::JobTool::new(ctx.parts.jobs.clone())),
    ];
    if let Some(port) = ctx.query_port {
        tools.push(Box::new(crate::session_query::SessionQueryTool::new(
            port,
            ctx.session_id,
        )));
    }
    if let Some(link) = ctx.parent_link {
        tools.push(Box::new(ChildParentMessageTool::new(link)));
    }
    dsh_agent_loop::ToolSet::new(tools).map_err(|e| format!("child toolset assembly failed: {e}"))
}

/// 子代理回发父消息工具(仅驻留子代理装配):`send_message` 唯一
/// 可寻址对象 = 直接父(驻留可续话子代理语义)。
pub struct ChildParentMessageTool {
    link: ChildParentLink,
}

impl ChildParentMessageTool {
    /// 以父链路构建
    pub(crate) fn new(link: ChildParentLink) -> Self {
        Self { link }
    }
}

impl ToolPort for ChildParentMessageTool {
    fn specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "send_message",
                "description": "Send a message to your direct parent agent. If the parent is still working, the message steers its nearest step; if it is idle, the message starts a turn. This call returns no answer from the parent — only confirmation that the message was delivered. A failure means the message was NOT delivered.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "agent_id": { "type": "string", "description": "Your direct parent agent id." },
                        "message": { "type": "string", "description": "The message to deliver to the agent." }
                    },
                    "required": ["agent_id", "message"]
                },
            },
        })]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        if call.name != "send_message" {
            return ToolOutput {
                output: format!("unknown tool: {}", call.name),
                success: false,
                ..Default::default()
            };
        }
        let fail = |msg: String| ToolOutput {
            output: msg,
            success: false,
            ..Default::default()
        };
        let arguments: Value = if let Some(s) = call.arguments.as_str() {
            serde_json::from_str(s).unwrap_or(json!({}))
        } else {
            call.arguments.clone()
        };
        let (Some(agent_id), Some(message)) = (
            arguments["agent_id"].as_str(),
            arguments["message"].as_str(),
        ) else {
            return fail(
                "send_message requires arguments.agent_id and arguments.message (strings)".into(),
            );
        };
        if agent_id != self.link.parent_id {
            return fail(format!(
                "only your direct parent ({}) is addressable",
                self.link.parent_id
            ));
        }
        let (text, source) = child_message_notice(&self.link.self_id, message);
        self.link.notify.notify(agent_id, text, source).await;
        ToolOutput {
            output: format!("message delivered to agent {agent_id}"),
            success: true,
            ..Default::default()
        }
    }
}

/// 跑一个子 turn:select 软取消(父令牌级联或 interrupt future 触发 →
/// 令牌取消 → 子引擎安全点收尾)。事件逐条落子日志(backend 持久化);
/// 时钟取系统毫秒。
#[allow(clippy::too_many_arguments)]
async fn run_child_turn<G>(
    engine: &mut LoopEngine,
    gate: &mut G,
    tools: &mut dsh_agent_loop::ToolSet,
    prompt: &str,
    turn_token: &CancelToken,
    parent_cancel: &CancelToken,
    interrupt: impl std::future::Future<Output = ()> + Send,
    backend: &JsonlBackend,
    session_id: &str,
    event_sink: Option<&SubagentEventSink>,
) -> Result<dsh_agent_loop::TurnOutcome, LoopError>
where
    G: dsh_agent_loop::LlmTransport + Summarizer + Send,
{
    // 双取消源:父令牌级联 / interrupt 句柄,任一触发即取消本 turn 令牌
    let canceller = {
        let turn_token = turn_token.clone();
        let parent_cancel = parent_cancel.clone();
        async move {
            tokio::select! {
                _ = parent_cancel.cancelled() => {}
                _ = interrupt => {}
            }
            turn_token.cancel();
        }
    };
    let mut sink = |ev: &EventEnvelope| {
        if let Err(e) = backend.append(ev) {
            eprintln!("子会话持久化失败:{e}");
        }
        // 实时流出口(桌面广播)
        if let Some(relay) = event_sink {
            relay(session_id, ev);
        }
    };
    let run = engine.run_turn(
        prompt,
        None,
        &[],
        &[],
        gate,
        tools,
        &|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        },
        &mut sink,
    );
    tokio::pin!(run);
    tokio::pin!(canceller);
    tokio::select! {
        out = &mut run => out,
        // 取消在子执行中触发:等待子引擎在安全点收尾(取消后 run 分支
        // 很快以 Cancelled 返回;此臂只负责不再等取消源)
        _ = &mut canceller => (&mut run).await,
    }
}

/// 注册表状态更新(找不到 = 会话已终结,忽略)。终态记 ended_at;
/// 回到运行/空闲态清空(驻留循环 running↔idle 反复)。变化后通知观察方。
fn set_status(registry: &SubagentRegistry, session_id: &str, status: &str) {
    let changed = if let Ok(mut r) = registry.lock() {
        match r.iter_mut().find(|s| s.session_id == session_id) {
            Some(rec) => {
                rec.status = status.into();
                // idle = 驻留最近一轮结算时刻(续话再跑时被 running 清空)
                rec.ended_at = matches!(
                    status,
                    "idle" | "done" | "failed" | "cancelled" | "stopped"
                )
                .then(now_ms);
                true
            }
            None => false,
        }
    } else {
        false
    };
    if changed {
        registry.changed();
    }
}

/// 子会话标记事件落档(descriptor/settled;重启恢复的判据)
fn commit_child_marker(log: &Arc<Mutex<EventLog>>, backend: &JsonlBackend, ty: &str, data: Value) {
    if let Ok(mut l) = log.lock() {
        // 归因事件照守卫约定标 ignorable:未登记类型不拒绝旧读取方重建日志
        let ev = EventEnvelope::new_ignorable(ty, now_ms(), data);
        // 照 engine commit 模式:append 赋 seq 后取回信封落盘(直接落原始
        // clone 会把 seq=0 写进日志)
        if let Ok(seq) = l.append(ev)
            && let Some(envelope) = l.get(seq)
        {
            let _ = backend.append(envelope);
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// subagent 工具:委派任务 → 嵌套引擎执行(前台等结果 / 后台即返回)
pub struct SubagentTool<T> {
    /// 父 workspace(子根 = `.dsh/subagents/<id>/`,能力束窄化)
    pub root: std::path::PathBuf,
    /// 子代理独立传输工厂(每个子代理一份传输;后台并发的前提)
    transport_factory: TransportFactory<T>,
    /// 模型标识(子 header)
    pub model: String,
    /// 父会话软取消令牌(级联:父取消 → 子静默退出)
    pub cancel: CancelToken,
    /// 注册表
    pub registry: SubagentRegistry,
    /// 子会话工厂(Some = 子任务注册为带血缘会话;None = 回落后建)
    pub factory: Option<Arc<dyn SessionFactory>>,
    /// 父会话 id(血缘 parent;factory 注入时传给 create_subagent)
    pub parent_id: String,
    /// 通知 port(None = 同步语义:模型面无后台参数、执行强制前台)
    pub notify: Option<Arc<dyn SettlementNotificationPort>>,
    /// 子会话事件出口(None = 只落盘)
    pub event_sink: Option<SubagentEventSink>,
    /// 会话检索 port(子代理工具面扩装,缺 = 子代理无检索)
    query_port: Option<Arc<dyn crate::session_query::SessionQueryPort>>,
}

impl<T> SubagentTool<T>
where
    T: LlmTransport + Summarizer + Send + 'static,
{
    /// 以父根、传输工厂与模型构建(与父 BashTool 同一 workspace;
    /// 子可写根在每次执行时按 id 窄化)
    pub fn new(
        root: impl Into<std::path::PathBuf>,
        transport_factory: TransportFactory<T>,
        model: String,
    ) -> Self {
        Self {
            root: root.into(),
            transport_factory,
            model,
            cancel: CancelToken::new(),
            registry: SubagentRegistry::default(),
            factory: None,
            parent_id: String::new(),
            notify: None,
            event_sink: None,
            query_port: None,
        }
    }

    /// 注入子会话事件出口(桌面广播实时流)
    pub fn with_event_sink(mut self, sink: SubagentEventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// 注入子会话工厂(None/未注入 = 回落后建)
    pub fn with_session_factory(mut self, factory: Arc<dyn SessionFactory>) -> Self {
        self.factory = Some(factory);
        self
    }

    /// 设置父会话 id(血缘 parent;factory 注入时传给 create_subagent)
    pub fn with_parent_id(mut self, parent: &str) -> Self {
        self.parent_id = parent.to_string();
        self
    }

    /// 设置父取消令牌(与 engine/REPL Ctrl-C 同源,级联到子)
    pub fn with_cancel(mut self, token: CancelToken) -> Self {
        self.cancel = token;
        self
    }

    /// 共享注册表(workflow/ralph 编排器与顶层 subagent 工具共用清单)
    pub fn with_registry(mut self, registry: SubagentRegistry) -> Self {
        self.registry = registry;
        self
    }

    /// 注入通知 port(注入后模型面切后台默认形态)
    pub fn with_notify(mut self, notify: Arc<dyn SettlementNotificationPort>) -> Self {
        self.notify = Some(notify);
        self
    }

    /// 注入会话检索 port(子代理工具面扩装)
    pub fn with_query_port(
        mut self,
        port: Arc<dyn crate::session_query::SessionQueryPort>,
    ) -> Self {
        self.query_port = Some(port);
        self
    }

    /// 建子会话:factory 注入 → 带血缘会话(返回槽位 id);未注入 → 回落
    /// 自建 `.dsh/subagents/<id>/`(id 为本地簿记形态)
    fn make_session(&self) -> (u64, SubagentSessionHandle) {
        let id = self.next_id();
        match &self.factory {
            Some(f) => (id, f.create_subagent(&self.parent_id)),
            None => {
                let root = self.root.join(format!(".dsh/subagents/{id}"));
                let path = root.join("session.jsonl");
                (
                    id,
                    SubagentSessionHandle {
                        session_id: format!("subagent-{id}"),
                        session_path: path,
                    },
                )
            }
        }
    }

    fn next_id(&self) -> u64 {
        self.registry
            .lock()
            .map(|r| r.iter().map(|s| s.id).max().unwrap_or(0) + 1)
            .unwrap_or(1)
    }

    /// 前台执行一次子代理任务:嵌套引擎 + 独立日志 + 窄化工具集,等结果。
    /// workflow/ralph 编排器与 run_in_background=false 同走此入口。
    pub(crate) async fn run_foreground(&mut self, prompt: &str) -> ToolOutput {
        let (id, handle) = self.make_session();
        let path_str = handle.session_path.display().to_string();
        let parts = match prepare_child_parts(&handle, &self.model, None).await {
            Ok(parts) => parts,
            Err(msg) => {
                return ToolOutput {
                    output: msg,
                    success: false,
                    ..Default::default()
                };
            }
        };
        let started_at = now_ms();
        self.registry.lock().expect("records 锁中毒").push(SubagentRecord {
            id,
            task: prompt.to_string(),
            status: "running".into(),
            session_id: handle.session_id.clone(),
            session_path: path_str.clone(),
            background: false,
            tx: None,
            steer: None,
            stop: None,
            prompt: prompt.to_string(),
            started_at,
            ended_at: None,
        });
        self.registry.changed();

        let transport = match (self.transport_factory)() {
            Ok(t) => t,
            Err(e) => {
                return ToolOutput {
                    output: format!("subagent transport unavailable: {e}"),
                    success: false,
                    ..Default::default()
                };
            }
        };
        let mut gate = InvariantGate::new(transport, Arc::clone(&parts.log));
        let mut engine = LoopEngine::new(parts.header.clone(), Arc::clone(&parts.log));
        // 每 turn 一枚令牌:前台只此一 turn,父取消传播直达;
        // 工具集随令牌构建(运行中 bash 可中断)
        let turn_token = CancelToken::new();
        engine.set_cancel(turn_token.clone());
        let mut tools = match build_child_tools(ChildToolContext {
            turn_token: &turn_token,
            parts: &parts,
            query_port: self.query_port.clone(),
            session_id: &handle.session_id,
            parent_link: None,
        }) {
            Ok(t) => t,
            Err(msg) => {
                return ToolOutput {
                    output: msg,
                    success: false,
                    ..Default::default()
                };
            }
        };

        let parent_cancel = self.cancel.clone();
        let outcome = run_child_turn(
            &mut engine,
            &mut gate,
            &mut tools,
            prompt,
            &turn_token,
            &parent_cancel,
            // 前台无 interrupt 句柄:永不就绪的 future 占位
            std::future::pending::<()>(),
            &parts.backend,
            &handle.session_id,
            self.event_sink.as_ref(),
        )
        .await;

        let status = match &outcome {
            Ok(_) => "done",
            Err(LoopError::Cancelled) => "cancelled",
            Err(_) => "failed",
        };
        if let Ok(mut r) = self.registry.lock()
            && let Some(rec) = r.iter_mut().find(|s| s.id == id)
        {
            rec.status = status.into();
        }

        match outcome {
            Ok(o) => ToolOutput {
                output: format!(
                    "{}\n\n(subagent {} · session {} · events {}..{})",
                    o.assistant_message, handle.session_id, path_str, o.seq_range.0, o.seq_range.1
                ),
                success: true,
                ..Default::default()
            },
            Err(e) => ToolOutput {
                output: format!(
                    "subagent {} failed: {e} (session {path_str})",
                    handle.session_id
                ),
                success: false,
                ..Default::default()
            },
        }
    }

    /// 后台委派:注册驻留记录 → spawn 驻留任务 → 立即返回子代理 id。
    /// 驻留任务跑初始 turn → 结算通知 → 转 idle 等 send_message。
    fn start_background(&mut self, label: &str, prompt: &str) -> ToolOutput {
        let Some(notify) = self.notify.clone() else {
            // 无通知 port = 接口无后台语义(不可达:specs 不声明后台参数;
            // 兜底拒绝,行为承诺与接口一致)
            return ToolOutput {
                output: "subagent background requires the notification port".into(),
                success: false,
                ..Default::default()
            };
        };
        let (id, handle) = self.make_session();
        let session_id = handle.session_id.clone();
        let path_str = handle.session_path.display().to_string();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ChildMsg>();
        let stop = Arc::new(tokio::sync::Notify::new());
        let steer = Arc::new(Mutex::new(VecDeque::new()));
        self.registry.lock().expect("records 锁中毒").push(SubagentRecord {
            id,
            task: label.to_string(),
            status: "running".into(),
            session_id: session_id.clone(),
            session_path: path_str,
            background: true,
            tx: Some(tx),
            steer: Some(Arc::clone(&steer)),
            stop: Some(Arc::clone(&stop)),
            prompt: prompt.to_string(),
            started_at: now_ms(),
            ended_at: None,
        });
        self.registry.changed();
        tokio::spawn(run_resident_child(ResidentChild {
            handle,
            label: label.to_string(),
            event_sink: self.event_sink.clone(),
            initial_prompt: prompt.to_string(),
            model: self.model.clone(),
            transport_factory: Arc::clone(&self.transport_factory),
            registry: self.registry.clone(),
            parent_id: self.parent_id.clone(),
            parent_cancel: self.cancel.clone(),
            notify,
            stop,
            steer,
            rx,
            query_port: self.query_port.clone(),
            release: Arc::new(|| ()),
            first: Some(ChildMsg::Prompt(prompt.to_string())),
            resumed_interrupted: false,
        }));
        ToolOutput {
            output: format!("started subagent {}", session_id),
            success: true,
            ..Default::default()
        }
    }

    /// 重启恢复:扫描父会话下有 descriptor 标记的子会话并重挂为
    /// 驻留(实现方已认领防双挂;中断者先冷修夏并投「已恢复」通知)。
    /// 无 factory/notify 或无 tokio runtime(纯装配单测)→ no-op。
    pub fn resume_children(&mut self) {
        let Some(factory) = self.factory.clone() else {
            return;
        };
        let Some(notify) = self.notify.clone() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        for (handle, interrupted, label, prompt) in factory.resumable_children(&self.parent_id) {
            let session_id = handle.session_id.clone();
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ChildMsg>();
            let stop = Arc::new(tokio::sync::Notify::new());
            let steer = Arc::new(Mutex::new(VecDeque::new()));
            let id = self.next_id();
            self.registry.lock().expect("records 锁中毒").push(SubagentRecord {
                id,
                task: label.clone(),
                status: "idle".into(),
                session_id: session_id.clone(),
                session_path: handle.session_path.display().to_string(),
                background: true,
                tx: Some(tx),
                steer: Some(Arc::clone(&steer)),
                stop: Some(Arc::clone(&stop)),
                prompt: prompt.clone(),
                started_at: now_ms(),
                ended_at: None,
            });
            self.registry.changed();
            let release_factory = Arc::clone(&factory);
            let release_id = session_id.clone();
            tokio::spawn(run_resident_child(ResidentChild {
                handle,
                label,
                event_sink: self.event_sink.clone(),
                initial_prompt: String::new(),
                model: self.model.clone(),
                transport_factory: Arc::clone(&self.transport_factory),
                registry: self.registry.clone(),
                parent_id: self.parent_id.clone(),
                parent_cancel: self.cancel.clone(),
                notify: Arc::clone(&notify),
                stop,
                steer,
                rx,
                query_port: self.query_port.clone(),
                release: Arc::new(move || release_factory.release_child(&release_id)),
                first: None,
                resumed_interrupted: interrupted,
            }));
        }
    }
}

/// 驻留子代理任务的全部权属(spawn 一次性移交)
struct ResidentChild<T> {
    handle: SubagentSessionHandle,
    label: String,
    /// 子会话事件出口(新建与重挂都从工具注入)
    event_sink: Option<SubagentEventSink>,
    /// 初始 prompt(descriptor 落档用;重挂为空)
    initial_prompt: String,
    model: String,
    transport_factory: TransportFactory<T>,
    registry: SubagentRegistry,
    parent_id: String,
    parent_cancel: CancelToken,
    notify: Arc<dyn SettlementNotificationPort>,
    stop: Arc<tokio::sync::Notify>,
    steer: Arc<Mutex<VecDeque<SteerInput>>>,
    rx: tokio::sync::mpsc::UnboundedReceiver<ChildMsg>,
    query_port: Option<Arc<dyn crate::session_query::SessionQueryPort>>,
    /// 驻留退出释放认领(重挂形态必持;新建为 no-op)
    release: Arc<dyn Fn() + Send + Sync>,
    /// 初始 prompt(委派即带;与续话统一走消息循环)
    first: Option<ChildMsg>,
    /// 重挂且上次被中断(投「已恢复」通知)
    resumed_interrupted: bool,
}

/// 驻留子代理主循环入口:退出统一释放认领(防双挂)
async fn run_resident_child<T>(child: ResidentChild<T>)
where
    T: LlmTransport + Summarizer + Send + 'static,
{
    let release = Arc::clone(&child.release);
    run_resident_child_inner(child).await;
    release();
}

/// 驻留子代理主循环:逐条消息一个 turn;每次结算通知父会话并落 settled
/// 标记;运行中 send_message 经 steer_buf 中途插话,turn 间排干续跑;
/// 父取消静默退出;interrupt 只停当前 turn(子代理保持可续话)。
async fn run_resident_child_inner<T>(child: ResidentChild<T>)
where
    T: LlmTransport + Summarizer + Send + 'static,
{
    // 先解构为局部变量:消息循环的 select 双臂各自持有独立权属,避免
    // 借权冲突(next 借 &mut rx / 取消臂借 token 副本)
    let ResidentChild {
        handle,
        label,
        event_sink,
        initial_prompt,
        model,
        transport_factory,
        registry,
        parent_id,
        parent_cancel,
        notify,
        stop,
        steer,
        mut rx,
        query_port,
        release: _,
        mut first,
        resumed_interrupted,
    } = child;
    let parent_link = ChildParentLink {
        parent_id: parent_id.clone(),
        self_id: handle.session_id.clone(),
        notify: Arc::clone(&notify),
    };

    let parts = match prepare_child_parts(&handle, &model, Some(&parent_link)).await {
        Ok(parts) => parts,
        Err(_) => {
            // 装配失败:记录 failed 并以失败通知收口(父会话必须知道委派没成)
            set_status(&registry, &handle.session_id, "failed");
            let (text, source) = settlement_notice(&handle.session_id, "error", None);
            notify.notify(&parent_id, text, source).await;
            return;
        }
    };
    if !initial_prompt.is_empty() {
        // descriptor 标记(重启恢复的判据;重挂形态不重写)
        commit_child_marker(
            &parts.log,
            &parts.backend,
            "subagent/descriptor",
            json!({
                "parentSessionId": parent_id,
                "label": label,
                "prompt": initial_prompt,
                "mode": "continuable",
            }),
        );
    } else {
        // 重挂形态:重载子日志(扫描侧已冷修夏)重建引擎历史
        let events = match dsh_host::persistence::jsonl::load_jsonl(&handle.session_path) {
            Ok(events) => events,
            Err(_) => {
                set_status(&registry, &handle.session_id, "failed");
                let (text, source) = settlement_notice(&handle.session_id, "error", None);
                notify.notify(&parent_id, text, source).await;
                return;
            }
        };
        if let Ok(mut l) = parts.log.lock() {
            for ev in events {
                let _ = l.append(ev);
            }
        }
    }
    let transport = match (transport_factory)() {
        Ok(t) => t,
        Err(_) => {
            set_status(&registry, &handle.session_id, "failed");
            let (text, source) = settlement_notice(&handle.session_id, "error", None);
            notify.notify(&parent_id, text, source).await;
            return;
        }
    };
    let mut gate = InvariantGate::new(transport, Arc::clone(&parts.log));
    let mut engine = LoopEngine::new(parts.header.clone(), Arc::clone(&parts.log));
    // 中途插话:引擎 step 边界认领(steer 语义)
    engine.set_steer_buf(Arc::clone(&steer));

    // 重挂且中断:投「已恢复」通知 + settled 标记(下次扫描视为已结)
    if resumed_interrupted {
        let (text, source) = resumed_notice(&handle.session_id);
        notify.notify(&parent_id, text, source).await;
        commit_child_marker(
            &parts.log,
            &parts.backend,
            "subagent/settled",
            json!({ "stopReason": "resumed" }),
        );
    }

    'resident: loop {
        // 空闲等待:取下一条消息。父取消 → 静默退出(不通知——唤醒已取消
        // 的父会话只会凭空起 turn;子会话日志即持久记录)
        let prompt_text = {
            let get_next = async {
                match first.take() {
                    Some(ChildMsg::Prompt(text)) => Some(text),
                    None => rx.recv().await.map(|m| match m {
                        ChildMsg::Prompt(text) => text,
                    }),
                }
            };
            tokio::select! {
                msg = get_next => match msg {
                    Some(text) => text,
                    None => break 'resident, // 注册表已 drop(会话终结)→ 退出
                },
                _ = parent_cancel.cancelled() => {
                    set_status(&registry, &handle.session_id, "cancelled");
                    commit_child_marker(
                        &parts.log,
                        &parts.backend,
                        "subagent/settled",
                        json!({ "stopReason": "aborted" }),
                    );
                    break 'resident;
                }
            }
        };
        // 内层:连续执行队列(本条 + 期间经 steer/续话到达的)
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(prompt_text);
        while let Some(prompt_text) = queue.pop_front() {
            if parent_cancel.is_cancelled() {
                set_status(&registry, &handle.session_id, "cancelled");
                commit_child_marker(
                    &parts.log,
                    &parts.backend,
                    "subagent/settled",
                    json!({ "stopReason": "aborted" }),
                );
                break 'resident;
            }
            set_status(&registry, &handle.session_id, "running");

            // 每 turn 一枚令牌 + 随令牌重建工具集:interrupt 只停当前 turn,
            // 运行中 bash 随令牌中断,子代理保持驻留
            let turn_token = CancelToken::new();
            engine.set_cancel(turn_token.clone());
            let mut tools =
                match build_child_tools(ChildToolContext {
                    turn_token: &turn_token,
                    parts: &parts,
                    query_port: query_port.clone(),
                    session_id: &handle.session_id,
                    parent_link: Some(parent_link.clone()),
                }) {
                    Ok(t) => t,
                    Err(_) => {
                        set_status(&registry, &handle.session_id, "failed");
                        let (text, source) =
                            settlement_notice(&handle.session_id, "error", None);
                        notify.notify(&parent_id, text, source).await;
                        return;
                    }
                };
            let interrupt = {
                let stop = Arc::clone(&stop);
                async move {
                    stop.notified().await;
                }
            };
            let outcome = run_child_turn(
                &mut engine,
                &mut gate,
                &mut tools,
                &prompt_text,
                &turn_token,
                &parent_cancel,
                interrupt,
                &parts.backend,
                &handle.session_id,
                event_sink.as_ref(),
            )
            .await;

            // 父取消级联 → 静默退出(不投通知)
            if parent_cancel.is_cancelled() {
                set_status(&registry, &handle.session_id, "cancelled");
                commit_child_marker(
                    &parts.log,
                    &parts.backend,
                    "subagent/settled",
                    json!({ "stopReason": "aborted" }),
                );
                break 'resident;
            }

            // 结算:状态 + 通知(成功/idle 可续话;cancelled=aborted 亦可续话;
            // 失败=error。全部通知变体,父会话决定下一步)
            let (status, stop_reason, closing) = match &outcome {
                Ok(o) => ("idle", "completed", Some(o.assistant_message.clone())),
                Err(LoopError::Cancelled) => ("idle", "aborted", None),
                Err(_) => ("failed", "error", None),
            };
            set_status(&registry, &handle.session_id, status);
            let (text, source) =
                settlement_notice(&handle.session_id, stop_reason, closing.as_deref());
            notify.notify(&parent_id, text, source).await;
            commit_child_marker(
                &parts.log,
                &parts.backend,
                "subagent/settled",
                json!({ "stopReason": stop_reason }),
            );

            // 排干运行中未及消费的插话(parked messages 依次续跑),
            // 再收续话通道;两者皆空 → 回 idle 等待
            {
                let mut b = steer.lock().expect("steer 锁中毒");
                while let Some(s) = b.pop_front() {
                    queue.push_back(s.text);
                }
            }
            if let Ok(ChildMsg::Prompt(text)) = rx.try_recv() {
                queue.push_back(text);
            }
        }
    }
}

impl<T> ToolPort for SubagentTool<T>
where
    T: LlmTransport + Summarizer + Send + 'static,
{
    /// 模型面按接口分形态(行为承诺与接口一致):
    /// - 通知 port 在场 → 后台默认形态(description/prompt/
    ///   run_in_background;send_message 语义句已真实成立)
    /// - 缺席(CLI/单测)→ 同步形态(维持既有 task 参数与描述)
    fn specs(&self) -> Vec<Value> {
        if self.notify.is_some() {
            vec![json!({
                "type": "function",
                "function": {
                    "name": "subagent",
                    "description": "Delegate a self-contained task to a subagent (a separate agent that works in its own context) to offload focused, independent work — research, a scoped implementation, an analysis — so it does not consume this conversation's context. The subagent returns its result, not its intermediate steps. Give it a complete, standalone prompt: it does not see this conversation. This tool runs in the background by default, immediately returns a durable subagent id, and keeps the child conversation available for later turns. When that run settles, the runtime sends the parent a notice containing its outcome and any final assistant message; `send_message` steers the child's nearest step while it is running and starts a turn while it is idle. Set `run_in_background: false` only when your next action depends on receiving the result.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "description": { "type": "string", "description": "A short (3-5 word) description of the delegated task, for display." },
                            "prompt": { "type": "string", "description": "The complete, self-contained task for the subagent. It does not share this conversation's context, so include everything it needs." },
                            "run_in_background": { "type": "boolean", "description": "Whether to run in the background and return a durable subagent id immediately. Defaults to true. Set false to wait for the result when your next action depends on it." }
                        },
                        "required": ["description", "prompt"]
                    },
                },
            })]
        } else {
            vec![json!({
                "type": "function",
                "function": {
                    "name": "subagent",
                    "description": "Run a task in an isolated subagent: its own turn loop, its own sandboxed workspace (a subdirectory of the current workspace), its own session log. Returns the subagent's final report. Use for self-contained subtasks.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "task": { "type": "string", "description": "Complete task description for the subagent" }
                        },
                        "required": ["task"],
                    },
                },
            })]
        }
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        if call.name != "subagent" {
            return ToolOutput {
                output: format!("unknown tool: {}", call.name),
                success: false,
                ..Default::default()
            };
        }
        let arguments: Value = if let Some(s) = call.arguments.as_str() {
            serde_json::from_str(s).unwrap_or(json!({}))
        } else {
            call.arguments.clone()
        };
        // 后台形态(通知 port 在场):description/prompt/run_in_background,
        // 默认后台
        if self.notify.is_some() {
            let Some(prompt) = arguments["prompt"].as_str() else {
                return ToolOutput {
                    output: "subagent requires arguments.prompt (string)".into(),
                    success: false,
                    ..Default::default()
                };
            };
            let label = arguments["description"].as_str().unwrap_or(prompt);
            let background = arguments["run_in_background"].as_bool().unwrap_or(true);
            if background {
                return self.start_background(label, prompt);
            }
            return self.run_foreground(prompt).await;
        }
        // 同步形态(无通知 port):task 参数,前台等结果
        let Some(task) = arguments["task"].as_str() else {
            return ToolOutput {
                output: "subagent requires arguments.task (string)".into(),
                success: false,
                ..Default::default()
            };
        };
        self.run_foreground(task).await
    }
}

/// 子代理控制工具:
/// `send_message`(续话/中途插话)/ `interrupt_agent`(打断当前 turn)/
/// `list_agents`(列驻留子代理)。
pub struct SubagentControlTool {
    /// 注册表(与执行工具共享)
    pub registry: SubagentRegistry,
}

impl SubagentControlTool {
    /// 以共享注册表构建
    pub fn new(registry: SubagentRegistry) -> Self {
        Self { registry }
    }
}

impl ToolPort for SubagentControlTool {
    fn specs(&self) -> Vec<Value> {
        vec![
            json!({
                "type": "function",
                "function": {
                    "name": "send_message",
                    "description": "Send a message to a direct continuable child by its agent id. If you are a resident continuable child, you may also target your direct parent. If the target is still working, the message steers its nearest step; if it is idle, the message starts a turn. This call returns no answer from the agent — only confirmation that the message was delivered. A failure means the message was NOT delivered.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "agent_id": { "type": "string", "description": "The agent id of your direct continuable child, or your direct parent when you are a resident continuable child." },
                            "message": { "type": "string", "description": "The message to deliver to the agent." }
                        },
                        "required": ["agent_id", "message"]
                    },
                },
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "interrupt_agent",
                    "description": "Request cancellation of a background agent's current turn by its agent id. The target is one of your direct children. Only the current turn stops: messages already queued for the agent stay parked until a later send_message, and the agent itself stays available for follow-ups. This call returns as soon as the stop request is accepted, so the target may keep running briefly; interrupting an agent that already finished is an accepted no-op.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "agent_id": { "type": "string", "description": "The agent id of the running agent to interrupt." }
                        },
                        "required": ["agent_id"]
                    },
                },
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "list_agents",
                    "description": "List your continuable background subagents by durable id and label. Use it to recall which ones you started, not to poll for completion — you are told when one finishes. Status comes from the live registry: running means the agent is working right now, idle means it is loaded but between turns, and stopped/failed are endings already reported by a settlement notice. The snapshot is not a delivery promise — `send_message` performs the authoritative check and may still fail.",
                    "parameters": { "type": "object", "properties": {} },
                },
            }),
        ]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        let fail = |msg: String| ToolOutput {
            output: msg,
            success: false,
            ..Default::default()
        };
        let arguments: Value = if let Some(s) = call.arguments.as_str() {
            serde_json::from_str(s).unwrap_or(json!({}))
        } else {
            call.arguments.clone()
        };
        match call.name.as_str() {
            "send_message" => {
                let (Some(agent_id), Some(message)) = (
                    arguments["agent_id"].as_str(),
                    arguments["message"].as_str(),
                ) else {
                    return fail(
                        "send_message requires arguments.agent_id and arguments.message (strings)"
                            .into(),
                    );
                };
                let Ok(r) = self.registry.lock() else {
                    return fail("registry unavailable".into());
                };
                let Some(rec) = r.iter().find(|s| s.session_id == agent_id) else {
                    return fail(format!("agent {agent_id} not found"));
                };
                // 路由:运行中 → steer 其最近 step(中途插话);
                // idle → 开新 turn。turn 刚结束的竞态窗口消息落在 steer 缓冲,
                // 由驻留循环在回 idle 前排干,不丢。
                if rec.status == "running"
                    && let Some(buf) = &rec.steer
                {
                    let mut b = buf.lock().expect("steer 锁中毒");
                    b.push_back(SteerInput {
                        id: format!("steer-{}", uuid::Uuid::now_v7()),
                        text: message.to_string(),
                        images: Vec::new(),
                        source: None,
                    });
                    drop(b);
                    return ToolOutput {
                        output: format!("message delivered to agent {agent_id}"),
                        success: true,
                        ..Default::default()
                    };
                }
                match &rec.tx {
                    Some(tx) if tx.send(ChildMsg::Prompt(message.to_string())).is_ok() => {
                        ToolOutput {
                            output: format!("message delivered to agent {agent_id}"),
                            success: true,
                            ..Default::default()
                        }
                    }
                    _ => fail(format!("agent {agent_id} is no longer accepting messages")),
                }
            }
            "interrupt_agent" => {
                let Some(agent_id) = arguments["agent_id"].as_str() else {
                    return fail("interrupt_agent requires arguments.agent_id (string)".into());
                };
                let Ok(r) = self.registry.lock() else {
                    return fail("registry unavailable".into());
                };
                // 打断已结束的子代理 = accepted no-op;只对正在跑的
                // turn 打断(Notify 仅唤醒在途等待者,idle 时本就无事可停)
                if let Some(rec) = r.iter().find(|s| s.session_id == agent_id)
                    && rec.status == "running"
                    && let Some(stop) = &rec.stop
                {
                    // notify_one 带许可语义:信号先于 interrupt future
                    // 注册的竞态窗口也不丢(notify_waiters 会丢)
                    stop.notify_one();
                }
                ToolOutput {
                    output: format!("interrupt requested for agent {agent_id}"),
                    success: true,
                    ..Default::default()
                }
            }
            "list_agents" => {
                let Ok(r) = self.registry.lock() else {
                    return fail("registry unavailable".into());
                };
                // 一次性(前台)子代理不出现在清单——不可续话,模型
                // 永不需要选它;清单只列后台驻留子代理
                let rows: Vec<String> = r
                    .iter()
                    .filter(|s| s.background)
                    .map(|s| format!("{} [{}] {}", s.session_id, s.status, s.task))
                    .collect();
                if rows.is_empty() {
                    ToolOutput {
                        output: "(no background subagents)".into(),
                        success: true,
                        ..Default::default()
                    }
                } else {
                    ToolOutput {
                        output: rows.join("\n"),
                        success: true,
                        ..Default::default()
                    }
                }
            }
            _ => fail(format!("unknown tool: {}", call.name)),
        }
    }
}

/// 仅供测试:子代理脚本事件的便捷构造(FakeProvider 脚本)
pub fn echo_events(text: &str) -> Vec<LlmEvent> {
    vec![LlmEvent::AssistantMessage(json!({ "content": text }))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_llm::FakeProvider;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 录制型通知 port:记录 (parent, text, source) 三元组
    #[derive(Default, Clone)]
    struct RecordingNotify {
        calls: Arc<Mutex<Vec<(String, String, Value)>>>,
    }

    impl RecordingNotify {
        fn texts(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(_, t, _)| t.clone())
                .collect()
        }

        fn kinds(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(_, _, s)| s["kind"].as_str().unwrap_or_default().to_string())
                .collect()
        }
    }

    impl SettlementNotificationPort for RecordingNotify {
        fn notify(
            &self,
            parent_session: &str,
            text: String,
            source: Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            self.calls
                .lock()
                .unwrap()
                .push((parent_session.to_string(), text, source));
            Box::pin(std::future::ready(()))
        }
    }

    /// 脚本组工厂:第 n 次调用产一个携第 n 组脚本的 provider(一个子代理
    /// 一次调用;组内多条脚本 = 该子代理的多个 turn 依次消费)
    fn scripted_factory(
        groups: &[&[&str]],
    ) -> (TransportFactory<FakeProvider>, Arc<AtomicUsize>) {
        let queue: Vec<Vec<Vec<LlmEvent>>> = groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .map(|text| vec![LlmEvent::AssistantMessage(json!({ "content": text }))])
                    .collect()
            })
            .collect();
        let built = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&built);
        let queue = Arc::new(Mutex::new(queue));
        (
            Arc::new(move || {
                let mut q = queue.lock().unwrap();
                let group = if q.is_empty() { Vec::new() } else { q.remove(0) };
                drop(q);
                counter.fetch_add(1, Ordering::SeqCst);
                let mut p = FakeProvider::new();
                for response in group {
                    p.then(response);
                }
                Ok(p)
            }),
            built,
        )
    }

    /// 工具调用起手脚本(子代理首个回复即发工具调用)
    fn tool_call_group(name: &str, arguments: Value) -> Vec<Vec<LlmEvent>> {
        vec![vec![LlmEvent::AssistantMessage(json!({
            "content": "", "tool_calls": [
                { "name": name, "arguments": arguments } ],
        }))]]
    }

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dsh-sub47-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 轮询直到谓词命中(驻留任务异步结算;5s 预算——并行测试竞争下不 flaky)
    async fn wait_for(mut pred: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if pred() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("condition not met within budget");
    }

    fn subagent_call(description: &str, prompt: &str) -> ToolCallRequest {
        ToolCallRequest {
            name: "subagent".into(),
            arguments: json!({ "description": description, "prompt": prompt }),
        }
    }

    fn child_events(session_path: &str) -> Vec<EventEnvelope> {
        dsh_host::persistence::jsonl::load_jsonl(std::path::Path::new(session_path))
            .expect("子会话日志必须可整份解码")
    }

    #[tokio::test]
    async fn background_delegation_returns_id_and_settles_with_notice() {
        let notify = RecordingNotify::default();
        let (factory, _built) = scripted_factory(&[&["hello from child"]]);
        let mut tool =
            SubagentTool::new(dir("bg"), factory, "m".into()).with_notify(Arc::new(notify.clone()));
        let out = ToolPort::execute(&mut tool, &subagent_call("Echo task", "say hi")).await;
        assert!(out.success, "{}", out.output);
        let session_id = out
            .output
            .strip_prefix("started subagent ")
            .expect("后台委派立即返回子代理 id")
            .to_string();
        assert!(!session_id.is_empty());

        // 结算通知:父会话、完成文案、closing message、染色载荷
        wait_for(|| notify.calls.lock().unwrap().len() == 1).await;
        let (parent, text, source) = notify.calls.lock().unwrap()[0].clone();
        assert_eq!(parent, "", "父会话 id 原样透传(未注入 parent_id 时为空)");
        assert!(text.contains("finished and will do no further work"), "{text}");
        assert!(text.contains("Its closing message:"), "{text}");
        assert!(text.contains("hello from child"), "{text}");
        assert_eq!(source["kind"], "subagent-settled");
        assert_eq!(source["form"], "notice");
        assert_eq!(source["senderSessionId"], session_id);

        // 驻留:idle 可续话
        wait_for(|| {
            tool.registry
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.session_id == session_id && r.status == "idle")
        })
        .await;
    }

    #[tokio::test]
    async fn send_message_drives_continuation_turn() {
        let notify = RecordingNotify::default();
        // 同一子代理两个 turn:初始报告 + 续话回复(工厂一次调用,组内两条)
        let (factory, _built) =
            scripted_factory(&[&["first report", "second report after nudge"]]);
        let mut tool =
            SubagentTool::new(dir("cont"), factory, "m".into()).with_notify(Arc::new(notify.clone()));
        let out = ToolPort::execute(&mut tool, &subagent_call("Two turns", "start")).await;
        let session_id = out
            .output
            .strip_prefix("started subagent ")
            .unwrap()
            .to_string();
        wait_for(|| notify.calls.lock().unwrap().len() == 1).await;

        // send_message 立即投递(不阻塞等子代理回复)
        let mut control = SubagentControlTool::new(tool.registry.clone());
        let sent = ToolPort::execute(
            &mut control,
            &ToolCallRequest {
                name: "send_message".into(),
                arguments: json!({ "agent_id": session_id, "message": "continue please" }),
            },
        )
        .await;
        assert!(sent.success, "{}", sent.output);
        assert_eq!(sent.output, format!("message delivered to agent {session_id}"));

        // 续话 turn 结算 → 第二次通知
        wait_for(|| notify.calls.lock().unwrap().len() == 2).await;
        let (_, text, _) = notify.calls.lock().unwrap()[1].clone();
        assert!(text.contains("second report after nudge"), "{text}");

        // 子会话日志确实跑了两个 turn
        let path = tool.registry.lock().unwrap()[0].session_path.clone();
        assert_eq!(
            child_events(&path)
                .iter()
                .filter(|e| e.r#type == "turn/start")
                .count(),
            2,
            "续话 = 子会话里的第二个 turn"
        );
    }

    #[tokio::test]
    async fn send_message_steers_running_child_mid_turn() {
        let notify = RecordingNotify::default();
        // 子代理卡在门控 bash(测试发完插话再放行,消费顺序确定);
        // 插话在其后的 step 边界被消费(同一 turn 内)
        let go_file = dir("steer-go").join("go");
        let group: Vec<Vec<LlmEvent>> = vec![
            tool_call_group(
                "bash",
                json!({
                    "command": format!(
                        "until [ -f \"{}\" ]; do sleep 0.05; done; echo released",
                        go_file.display()
                    ),
                    "description": "Gate on test signal",
                }),
            )
            .remove(0),
            vec![LlmEvent::AssistantMessage(json!({ "content": "final after steer" }))],
        ];
        let factory: TransportFactory<FakeProvider> = {
            let slot = Arc::new(Mutex::new(Some(group)));
            Arc::new(move || {
                let g = slot.lock().unwrap().take().unwrap_or_default();
                let mut p = FakeProvider::new();
                for response in g {
                    p.then(response);
                }
                Ok(p)
            })
        };
        let mut tool =
            SubagentTool::new(dir("steer"), factory, "m".into()).with_notify(Arc::new(notify.clone()));
        let out = ToolPort::execute(
            &mut tool,
            &ToolCallRequest {
                name: "subagent".into(),
                arguments: json!({ "description": "Steer me", "prompt": "run long" }),
            },
        )
        .await;
        let session_id = out
            .output
            .strip_prefix("started subagent ")
            .unwrap()
            .to_string();
        wait_for(|| {
            tool.registry
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.session_id == session_id && r.status == "running")
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // 运行中插话 → steer 路由(立即投递确认)
        let mut control = SubagentControlTool::new(tool.registry.clone());
        let sent = ToolPort::execute(
            &mut control,
            &ToolCallRequest {
                name: "send_message".into(),
                arguments: json!({ "agent_id": session_id, "message": "change course now" }),
            },
        )
        .await;
        assert!(sent.success, "{}", sent.output);
        std::fs::write(&go_file, b"").unwrap();

        // 同一 turn 内消费:bash 放行后 step2 认领插话,最终回复出自脚本第 2 条
        wait_for(|| notify.calls.lock().unwrap().len() == 1).await;
        let (_, text, _) = notify.calls.lock().unwrap()[0].clone();
        assert!(text.contains("final after steer"), "{text}");
        let path = tool.registry.lock().unwrap()[0].session_path.clone();
        let events = child_events(&path);
        assert_eq!(
            events.iter().filter(|e| e.r#type == "turn/start").count(),
            1,
            "插话必须被同一 turn 消费(不另起 turn)"
        );
        // steer 的 user/message 落在 tool/result 之后(step 边界认领)
        let tool_result_seq = events
            .iter()
            .find(|e| e.r#type == "tool/result")
            .map(|e| e.seq)
            .unwrap_or_else(|| {
                panic!(
                    "bash 必有 tool/result,实际事件:{:?}",
                    events.iter().map(|e| e.r#type.as_str()).collect::<Vec<_>>()
                )
            });
        let steer_seq = events
            .iter()
            .find(|e| {
                e.r#type == "user/message"
                    && e.data["content"]
                        .as_str()
                        .map(|c| c.contains("change course"))
                        .unwrap_or(false)
            })
            .map(|e| e.seq)
            .expect("插话必须落为 user/message");
        assert!(steer_seq > tool_result_seq, "插话在下一 step 边界认领");
    }

    #[tokio::test]
    async fn child_sends_message_to_parent_mid_task() {
        let notify = RecordingNotify::default();
        // 子代理在任务中途主动回发父(脚本:先 send_message 再收尾)
        let mut group = tool_call_group(
            "send_message",
            json!({ "agent_id": "parent-1", "message": "interim finding" }),
        );
        group.push(vec![LlmEvent::AssistantMessage(
            json!({ "content": "final report" }),
        )]);
        let factory: TransportFactory<FakeProvider> = {
            let slot = Arc::new(Mutex::new(Some(group)));
            Arc::new(move || {
                let g = slot.lock().unwrap().take().unwrap_or_default();
                let mut p = FakeProvider::new();
                for response in g {
                    p.then(response);
                }
                Ok(p)
            })
        };
        let mut tool = SubagentTool::new(dir("childmsg"), factory, "m".into())
            .with_notify(Arc::new(notify.clone()))
            .with_parent_id("parent-1");
        let out = ToolPort::execute(&mut tool, &subagent_call("Report home", "work")).await;
        let session_id = out
            .output
            .strip_prefix("started subagent ")
            .unwrap()
            .to_string();

        // 通知序:先中途消息(subagent-message)后结算(settled)
        wait_for(|| notify.calls.lock().unwrap().len() == 2).await;
        assert_eq!(
            notify.kinds(),
            vec!["subagent-message".to_string(), "subagent-settled".to_string()],
            "中途消息先于结算通知"
        );
        let (parent, text, source) = notify.calls.lock().unwrap()[0].clone();
        assert_eq!(parent, "parent-1");
        assert!(text.contains("Message from subagent"), "{text}");
        assert!(text.contains("interim finding"), "{text}");
        assert_eq!(source["kind"], "subagent-message");
        assert_eq!(source["senderSessionId"], session_id);
        let (_, text, _) = notify.calls.lock().unwrap()[1].clone();
        assert!(text.contains("final report"), "{text}");
    }

    #[tokio::test]
    async fn child_toolset_includes_todo_write() {
        // 工具面扩装:子代理可调 todo_write(全量 minus 递归的代表项)
        let notify = RecordingNotify::default();
        let mut group = tool_call_group(
            "todo_write",
            json!({ "todos": [ { "content": "step", "status": "completed" } ] }),
        );
        group.push(vec![LlmEvent::AssistantMessage(
            json!({ "content": "todo done" }),
        )]);
        let factory: TransportFactory<FakeProvider> = {
            let slot = Arc::new(Mutex::new(Some(group)));
            Arc::new(move || {
                let g = slot.lock().unwrap().take().unwrap_or_default();
                let mut p = FakeProvider::new();
                for response in g {
                    p.then(response);
                }
                Ok(p)
            })
        };
        let mut tool = SubagentTool::new(dir("tools"), factory, "m".into())
            .with_notify(Arc::new(notify.clone()))
            .with_parent_id("parent-1");
        let out = ToolPort::execute(&mut tool, &subagent_call("With tools", "work")).await;
        assert!(out.success, "{}", out.output);
        wait_for(|| notify.calls.lock().unwrap().len() == 1).await;
        let path = tool.registry.lock().unwrap()[0].session_path.clone();
        let events = child_events(&path);
        assert!(
            events
                .iter()
                .any(|e| e.r#type == "tool/call" && e.data["name"] == "todo_write"),
            "子代理必须能调 todo_write"
        );
        assert!(
            events
                .iter()
                .any(|e| e.r#type == "tool/result" && e.data["success"] == true),
            "todo_write 必须真实执行成功"
        );
    }

    #[tokio::test]
    async fn interrupt_agent_stops_current_turn_child_stays_contidable() {
        let notify = RecordingNotify::default();
        // 初始 turn 卡在长 bash;打断后子代理仍可续话(只停当前 turn)
        let mut group = tool_call_group(
            "bash",
            json!({ "command": "sleep 30 && echo never", "description": "Long bash" }),
        );
        group.push(vec![LlmEvent::AssistantMessage(
            json!({ "content": "after interrupt" }),
        )]);
        let factory: TransportFactory<FakeProvider> = {
            let slot = Arc::new(Mutex::new(Some(group)));
            Arc::new(move || {
                let g = slot.lock().unwrap().take().unwrap_or_default();
                let mut p = FakeProvider::new();
                for response in g {
                    p.then(response);
                }
                Ok(p)
            })
        };
        let mut tool =
            SubagentTool::new(dir("intr"), factory, "m".into()).with_notify(Arc::new(notify.clone()));
        let out = ToolPort::execute(
            &mut tool,
            &ToolCallRequest {
                name: "subagent".into(),
                arguments: json!({ "description": "Long task", "prompt": "run long bash" }),
            },
        )
        .await;
        let session_id = out
            .output
            .strip_prefix("started subagent ")
            .unwrap()
            .to_string();

        // 等 turn 进入长 bash 执行中
        wait_for(|| {
            tool.registry
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.session_id == session_id && r.status == "running")
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let mut control = SubagentControlTool::new(tool.registry.clone());
        let ack = ToolPort::execute(
            &mut control,
            &ToolCallRequest {
                name: "interrupt_agent".into(),
                arguments: json!({ "agent_id": session_id }),
            },
        )
        .await;
        assert!(ack.success, "{}", ack.output);
        assert_eq!(
            ack.output,
            format!("interrupt requested for agent {session_id}")
        );

        // 打断结算:aborted 变体 + 子代理转 idle(保持可续话)
        wait_for(|| notify.calls.lock().unwrap().len() == 1).await;
        let (_, text, _) = notify.calls.lock().unwrap()[0].clone();
        assert!(text.contains("was stopped before it finished"), "{text}");
        wait_for(|| {
            tool.registry
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.session_id == session_id && r.status == "idle")
        })
        .await;

        // 打断后续话仍可用(新 turn,新令牌)
        let sent = ToolPort::execute(
            &mut control,
            &ToolCallRequest {
                name: "send_message".into(),
                arguments: json!({ "agent_id": session_id, "message": "wrap up" }),
            },
        )
        .await;
        assert!(sent.success, "{}", sent.output);
        wait_for(|| notify.calls.lock().unwrap().len() == 2).await;
        let (_, text, _) = notify.calls.lock().unwrap()[1].clone();
        assert!(text.contains("after interrupt"), "{text}");
    }

    #[tokio::test]
    async fn parent_cancel_silently_stops_child_without_notice() {
        let notify = RecordingNotify::default();
        let group = tool_call_group(
            "bash",
            json!({ "command": "sleep 30 && echo never", "description": "Long bash" }),
        );
        let factory: TransportFactory<FakeProvider> = {
            let slot = Arc::new(Mutex::new(Some(group)));
            Arc::new(move || {
                let g = slot.lock().unwrap().take().unwrap_or_default();
                let mut p = FakeProvider::new();
                for response in g {
                    p.then(response);
                }
                Ok(p)
            })
        };
        let cancel = CancelToken::new();
        let mut tool = SubagentTool::new(dir("pcancel"), factory, "m".into())
            .with_notify(Arc::new(notify.clone()))
            .with_cancel(cancel.clone());
        let out = ToolPort::execute(&mut tool, &subagent_call("Long", "run long")).await;
        let session_id = out
            .output
            .strip_prefix("started subagent ")
            .unwrap()
            .to_string();
        wait_for(|| {
            tool.registry
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.session_id == session_id && r.status == "running")
        })
        .await;
        // 父令牌触发 → 子静默退出(cancelled),不发结算通知
        cancel.cancel();
        wait_for(|| {
            tool.registry
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.session_id == session_id && r.status == "cancelled")
        })
        .await;
        assert!(
            notify.texts().is_empty(),
            "父取消不投通知:{:?}",
            notify.texts()
        );
    }

    #[tokio::test]
    async fn list_agents_lists_background_only() {
        let notify = RecordingNotify::default();
        let (factory, built) = scripted_factory(&[&["bg result"], &["fg result"]]);
        let mut tool =
            SubagentTool::new(dir("list"), factory, "m".into()).with_notify(Arc::new(notify.clone()));
        let bg = ToolPort::execute(&mut tool, &subagent_call("Bg task", "run bg")).await;
        let bg_id = bg
            .output
            .strip_prefix("started subagent ")
            .unwrap()
            .to_string();
        // 等驻留任务取走第 1 组传输,前台再取第 2 组(脚本组与传输一一对应)
        wait_for(|| built.load(Ordering::SeqCst) >= 1).await;
        // 前台一次性(run_in_background=false)
        let fg = ToolPort::execute(
            &mut tool,
            &ToolCallRequest {
                name: "subagent".into(),
                arguments: json!({ "description": "Fg task", "prompt": "run fg", "run_in_background": false }),
            },
        )
        .await;
        assert!(fg.success, "{}", fg.output);
        assert!(fg.output.contains("fg result"), "前台等结果");

        wait_for(|| notify.calls.lock().unwrap().len() == 1).await;
        let mut control = SubagentControlTool::new(tool.registry.clone());
        let list = ToolPort::execute(
            &mut control,
            &ToolCallRequest {
                name: "list_agents".into(),
                arguments: json!({}),
            },
        )
        .await;
        assert!(list.output.contains(&bg_id), "{}", list.output);
        assert!(list.output.contains("[idle] Bg task"), "{}", list.output);
        assert!(
            !list.output.contains("Fg task"),
            "前台一次性不列入清单(照源):{}",
            list.output
        );
    }

    #[test]
    fn specs_adapt_to_notification_port() {
        let (factory, _built) = scripted_factory(&[]);
        // 无 port:同步形态(task 参数;无 run_in_background)
        let sync_tool = SubagentTool::<FakeProvider>::new(dir("s1"), factory.clone(), "m".into());
        let specs = dsh_agent_loop::ToolPort::specs(&sync_tool);
        assert_eq!(specs.len(), 1);
        assert!(specs[0]["function"]["parameters"]["properties"]
            .get("task")
            .is_some());
        assert!(specs[0]["function"]["parameters"]["properties"]
            .get("run_in_background")
            .is_none());

        // 有 port:后台形态(description/prompt/run_in_background;默认后台)
        let notify = RecordingNotify::default();
        let bg_tool =
            SubagentTool::<FakeProvider>::new(dir("s2"), factory, "m".into())
                .with_notify(Arc::new(notify));
        let specs = dsh_agent_loop::ToolPort::specs(&bg_tool);
        assert_eq!(specs.len(), 1);
        let props = specs[0]["function"]["parameters"]["properties"].clone();
        assert!(props.get("description").is_some());
        assert!(props.get("prompt").is_some());
        assert!(props.get("task").is_none());
        assert!(props.get("run_in_background").is_some());
        assert!(specs[0]["function"]["description"]
            .as_str()
            .unwrap()
            .contains("runs in the background by default"));
        // send_message 描述须含中途 steer 与父寻址句
        let control = SubagentControlTool::new(SubagentRegistry::default());
        let cspec = dsh_agent_loop::ToolPort::specs(&control)
            .into_iter()
            .into_iter()
            .find(|s| s["function"]["name"] == "send_message")
            .unwrap();
        let desc = cspec["function"]["description"].as_str().unwrap();
        assert!(desc.contains("steers its nearest step"), "{desc}");
        assert!(desc.contains("your direct parent"), "{desc}");
    }

    #[test]
    fn settlement_notice_variants() {
        let (text, source) = settlement_notice("s-1", "completed", Some("done report"));
        assert!(text.contains("Background subagent s-1 finished"));
        assert!(text.contains("Its closing message:\n\ndone report"));
        assert_eq!(source["kind"], "subagent-settled");
        assert_eq!(source["senderSessionId"], "s-1");

        let (text, _) = settlement_notice("s-2", "aborted", None);
        assert!(text.contains("was stopped before it finished"));
        assert!(text.contains("It left no closing message."));

        let (text, _) = settlement_notice("s-3", "error", Some("  "));
        assert!(text.contains("failed before it finished"));
        assert!(text.contains("It left no closing message."), "空白 closing 视同无");

        let (text, source) = resumed_notice("s-4");
        assert!(text.contains("was interrupted by a host restart"), "{text}");
        assert_eq!(source["kind"], "subagent-settled");
    }
}
