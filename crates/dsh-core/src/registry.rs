//! 多会话注册表:应用核心(AppHost)。
//!
//! 会话模型:sessionId = 会话目录名;日志文件位于
//! `<DSH_RS_HOME|~/.dshrs>/sessions/<projectKey(workspace)>/<id>/session.jsonl`
//! (沿用 ~/.dsh/sessions 布局,独立目录防冲突)。**冷会话**(仅文件)
//! 在首次 `history` / `prompt` 时懒装配——装配配方与
//! `dsh chat` 同源(`Resolved::resolve` → `InvariantGate` →
//! `build_tools` → [`dsh_app::Session`])。每个附着会话一个 worker
//! 任务:启动时若日志已有待审计划(重启/重连恢复)先发问,然后串行
//! 驱动 turn(队列模式);turn 结束若有待审计划则发 `question/requested`
//! 并等待 `respond` 应答。所有会话写入(turn/session_event)都
//! 发生在 worker 内——唯一写入者,无跨任务锁竞争。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use dsh_agent_loop::LlmEvent;
use dsh_agent_loop::{
    CancelToken, LlmTransport, NoTools, RequestHeader, SteerInput, ToolSet, TurnOutcome,
};
use dsh_app::{Resolved, Session};
use dsh_llm::{FakeProvider, HttpTransport, InvariantGate};
use dsh_session::attachments::ImageMediaType;
use dsh_session::{EventEnvelope, EventLog};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast, mpsc, oneshot};
use uuid::Uuid;

use crate::credentials::{
    CredentialStore, KEYCHAIN_SERVICE, KeychainPort, OsKeychain, resolve_credential,
};
use crate::proto::{
    HistoryEntry, HistoryValue, HostSessionAdded, HostSessionStatus, ProjectionFrame, Projections,
    Question, QuestionOption, QuestionRequestedFrame, QuestionResolvedFrame, RespondReceipt,
    RpcError, RpcResult, ServerRequest, SessionEventFrame, SessionSummary, SubscribedFrame,
};
use crate::settings::{
    BillingConfig, BillingKind, BillingSnapshot, ProviderEntry, SettingsStore, json_path,
    json_percent,
};
use crate::stats;
use crate::translate::{Page, ProviderInfo, Translator, paginate, translate_events};

/// 任意会话(真实 / fake 双臂;dsh-app Session 是泛型,非 trait 对象)
enum AnySession {
    /// 真实:闸门包 HTTP transport + preset 全量工具
    Real(Session<InvariantGate<HttpTransport>, ToolSet>),
    /// fake(自检/测试)
    Fake(Session<InvariantGate<FakeProvider>, NoTools>),
}

impl AnySession {
    async fn turn_with(
        &mut self,
        input: &str,
        input_id: Option<&str>,
        images: &[dsh_session::attachments::ImageAttachmentRef],
        contexts: &[serde_json::Value],
        on_event: &mut (dyn FnMut(&EventEnvelope) + Send),
    ) -> anyhow::Result<TurnOutcome> {
        match self {
            AnySession::Real(s) => {
                s.turn_with(input, input_id, images, contexts, on_event)
                    .await
            }
            AnySession::Fake(s) => {
                s.turn_with(input, input_id, images, contexts, on_event)
                    .await
            }
        }
    }

    fn session_event(&mut self, ty: &str, data: Value) -> anyhow::Result<u64> {
        match self {
            AnySession::Real(s) => s.session_event(ty, data),
            AnySession::Fake(s) => s.session_event(ty, data),
        }
    }

    fn pending_plan(&self) -> Option<String> {
        match self {
            AnySession::Real(s) => s.pending_plan(),
            AnySession::Fake(s) => s.pending_plan(),
        }
    }

    fn set_steer_buf(&mut self, buf: Arc<Mutex<VecDeque<SteerInput>>>) {
        match self {
            AnySession::Real(s) => s.set_steer_buf(buf),
            AnySession::Fake(s) => s.set_steer_buf(buf),
        }
    }

    /// 下一 turn 输入的来源染色(结算通知;转发引擎,消费见 engine)
    fn set_input_source(&mut self, source: Option<Value>) {
        match self {
            AnySession::Real(s) => s.set_input_source(source),
            AnySession::Fake(s) => s.set_input_source(source),
        }
    }

    fn set_context_provider(
        &mut self,
        provider: Box<
            dyn Fn() -> Option<(String, Vec<dsh_agent_loop::ContextSection>)> + Send + Sync,
        >,
    ) {
        match self {
            AnySession::Real(s) => s.set_context_provider(provider),
            AnySession::Fake(s) => s.set_context_provider(provider),
        }
    }

    fn restore_projection(&mut self) {
        match self {
            AnySession::Real(s) => s.restore_projection(),
            AnySession::Fake(s) => s.restore_projection(),
        }
    }

    fn set_instructions_provider(&mut self, provider: dsh_agent_loop::InstructionsProvider) {
        match self {
            AnySession::Real(s) => s.set_instructions_provider(provider),
            AnySession::Fake(s) => s.set_instructions_provider(provider),
        }
    }
}

/// 提交模式:queue = 追加为下一 turn;steer = 注入运行中 turn
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptMode {
    Queue,
    Steer,
}

/// 队列条目变更动作(源 QueueAction 语义:edit / remove / steer)
enum QueueAction {
    /// 替换待运行条目文本(仅 text;源 QUEUE_EDIT_NON_TEXT 限制)
    Edit(String),
    /// 移除待运行条目
    Remove,
    /// 移除并注入运行中 turn(仅 running 且条目在 next-turn 时可用)
    Steer,
}

/// 泵任务输入(worker 拆分后:泵 = 队列/模式调度,驱动 = turn 执行)
enum Job {
    /// 一条提交消息(队列或 steer;id 为宿主预分配的持久消息 id;
    /// images = 已准入的持久附件引用)
    Prompt {
        id: String,
        text: String,
        images: Vec<dsh_session::attachments::ImageAttachmentRef>,
        mode: PromptMode,
        /// 4a 注入上下文(transient,仅本 prompt;不入 durable splice)。
        /// 驱动并入注入数组交引擎,在用户消息**之后**落档为 user/message。
        contexts: Vec<serde_json::Value>,
    },
    /// 队列条目变更(需应答;错误码:queue-item-not-found /
    /// steer-unavailable / queue-edit-non-text)
    UpdateQueue {
        item_id: String,
        action: QueueAction,
        reply: oneshot::Sender<Result<Value, RpcError>>,
    },
    /// 子代理结算通知(宿主内部注入,不经 client RPC)。走 steer
    /// 通道:父空闲 → 驱动弹出作为 turn 输入(followup 唤醒);
    /// 父忙碌 → 引擎 step 边界认领(steer 注入)。source 染色随
    /// user/message 落档,桌面凭 kind=subagent-settled 渲染通知卡。
    Notice {
        id: String,
        text: String,
        source: serde_json::Value,
    },
    /// 模式切换(standard / plan;经驱动通道执行,与 turn 串行)
    SetMode(String),
    /// 沙箱访问模式切换(经驱动通道落档 sandbox/mode;与 turn 串行)
    SetPermission(String),
    /// 审批策略切换(经驱动通道落档 approval/policy;与 turn 串行)
    SetApproval(String),
}

/// 驱动任务控制命令(泵 → 驱动;与 turn 串行——驱动只在 turn 间隙处理)
#[allow(clippy::enum_variant_names)]
enum DriverCmd {
    /// 模式切换(session/mode 落档 + 广播)
    SetMode(String),
    /// 沙箱访问模式切换(sandbox/mode 落档 + 广播)
    SetPermission(String),
    /// 审批策略切换(approval/policy 落档 + 广播)
    SetApproval(String),
}

/// 队列态(进程内权威快照经 session/queue 帧下发)。
/// 持久化(durable inbox):全部变更以 `agent/inbox/spliced` 事件落档
/// (泵任务提交),冷附着时 [`replay_inbox`] 折叠日志重建未消费条目
struct QueueState {
    /// 待运行(next-turn)条目,按序
    pending: VecDeque<PendingItem>,
    /// 运行中 turn 的中途输入(next-step;驱动开 turn 前优先认领;
    /// 独立 Arc = 引擎 step 边界认领的共享入口)
    steer: Arc<Mutex<VecDeque<SteerInput>>>,
    /// 当前是否有 turn 在跑(steer-unavailable 检查)
    running: bool,
}

/// 一条待运行(next-turn)队列条目
struct PendingItem {
    /// 持久消息 id(队列帧 / 认领 splice / user/message 共用)
    id: String,
    /// 文本内容
    text: String,
    /// 图片附件(持久引用;空 = 纯文本条目,可编辑)
    images: Vec<dsh_session::attachments::ImageAttachmentRef>,
    /// 4a 注入上下文(transient;durable splice 重建时为空)——仅驱动认领后
    /// 在 turn/start 前 commit,不持久化。
    contexts: Vec<serde_json::Value>,
}

/// 附着态(OnceLock 一次性装配)
struct SlotInner {
    queue_tx: mpsc::UnboundedSender<Job>,
    driver_cmd: mpsc::UnboundedSender<DriverCmd>,
    /// 驱动唤醒(泵在提交后 notify;驱动在空闲时等待)
    wake: Arc<Notify>,
    /// 队列态(泵与驱动共享;驱动 turn 前 pop,泵变更 + 发帧)
    qs: Arc<Mutex<QueueState>>,
    cancel: CancelToken,
    log: Arc<Mutex<EventLog>>,
    /// 落盘句柄克隆(durable 队列:泵侧 splice 事件的持久化通道;
    /// 与驱动共享同一写者互斥,行完整性不破)
    backend: dsh_host::JsonlBackend,
}

/// 会话槽:冷(仅文件)→ 附着(worker 驱动)
struct SessionSlot {
    id: String,
    path: PathBuf,
    inner: std::sync::OnceLock<SlotInner>,
    running: std::sync::atomic::AtomicBool,
}

/// 未决问题(计划审批)
struct PendingQuestion {
    #[allow(dead_code)]
    session_id: String,
    /// 批准选项标签(应答按标签判别;plan 用)
    approve_label: String,
    /// plan 审批应答通道(QuestionAnswer 三元)
    tx: Option<oneshot::Sender<QuestionAnswer>>,
    /// ask_user_question 应答通道(工具结果 JSON 文本)
    answer_tx: Option<oneshot::Sender<Result<String, String>>>,
    /// 重放用的请求帧(mux 重连时同 rpcId 重发)
    frame: ServerRequest,
}

/// 计划审批应答
enum QuestionAnswer {
    /// 批准(plan/approved + 回 standard)
    Approve,
    /// 拒绝(仅回 standard;feedback = 选项②「否,并告诉它应该如何做
    /// 不同」的行内输入,宿主经引导轮直送模型;「跳过」= None)
    Decline { feedback: Option<String> },
    /// 取消(ok:false 应答)
    Cancel,
}

/// plan 审批终局(plan_question 返回;驱动侧据此注入引导轮)
enum PlanReviewOutcome {
    /// 批准(plan/approved 已落档,回 standard)
    Approved,
    /// 拒绝(已回 standard;feedback = 选项②行内输入)
    Declined { feedback: Option<String> },
    /// 取消(去聊天里说;plan/cancelled 已落档)
    Cancelled,
}

/// 一条消息反馈(按 assistant messageId 定位)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageFeedbackItem {
    /// 目标 assistant 消息的持久 id(engine 落档 v7)
    pub message_id: String,
    /// 赞/踩(positive / negative)
    pub rating: String,
    /// 备注(可选;trim 非空,≤ max_note_bytes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// 乐观锁 token(put/delete 的 ifVersion 比对;改一次换新)
    pub version: String,
    /// 创建/更新时间戳(ms)
    /// 创建时间戳(ms)
    pub created_at: i64,
    /// 更新时间戳(ms)
    pub updated_at: i64,
}

/// 消息反馈 sidecar(per-session
/// JSON 文件,与 session 日志分离,不发给模型)。put/delete 带 ifVersion 乐观锁。
#[derive(Debug, Clone)]
pub struct MessageFeedbackStore {
    root: std::path::PathBuf,
    /// 备注字节上限(8192)
    max_note_bytes: usize,
}

impl MessageFeedbackStore {
    /// 以反馈根构建(~/.dshrs/feedback)
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_note_bytes: 8192,
        }
    }

    fn path(&self, session_id: &str) -> std::path::PathBuf {
        self.root
            .join(format!("{}.json", session_id.replace('/', "-")))
    }

    fn load(&self, session_id: &str) -> Vec<MessageFeedbackItem> {
        std::fs::read_to_string(self.path(session_id))
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<MessageFeedbackItem>>(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, session_id: &str, items: &[MessageFeedbackItem]) {
        let _ = std::fs::create_dir_all(&self.root);
        let data = serde_json::to_string(items).unwrap_or_default();
        let _ = std::fs::write(self.path(session_id), data);
    }

    /// list:某 session 的全部反馈
    pub fn list(&self, session_id: &str) -> Vec<MessageFeedbackItem> {
        self.load(session_id)
    }

    /// put:新增/更新一条反馈。`if_version` = 期望当前值(Some=必须匹配旧值才
    /// 写;None=必须不存在才写,即新增——源 CAS)。返回写入后的条目;
    /// 冲突返回 Err("version-conflict")。
    pub fn put(
        &self,
        session_id: &str,
        message_id: &str,
        rating: &str,
        note: Option<&str>,
        if_version: Option<&str>,
    ) -> Result<MessageFeedbackItem, String> {
        if !matches!(rating, "positive" | "negative") {
            return Err("rating 必须为 positive/negative".into());
        }
        let note = note.map(|n| n.trim()).filter(|n| !n.is_empty());
        if let Some(n) = note
            && n.len() > self.max_note_bytes
        {
            return Err("note-too-large".into());
        }
        let mut items = self.load(session_id);
        let now = now_ms() as i64;
        let idx = items.iter().position(|i| i.message_id == message_id);
        match (idx, if_version) {
            (Some(i), Some(ver)) if items[i].version == ver => {
                // 更新
                let mut it = items[i].clone();
                it.rating = rating.to_string();
                it.note = note.map(String::from);
                it.version = Uuid::now_v7().to_string();
                it.updated_at = now;
                items[i] = it.clone();
                self.save(session_id, &items);
                Ok(it)
            }
            (Some(_), _) => Err("version-conflict".into()),
            (None, None) => {
                // 新增
                let it = MessageFeedbackItem {
                    message_id: message_id.into(),
                    rating: rating.into(),
                    note: note.map(String::from),
                    version: Uuid::now_v7().to_string(),
                    created_at: now,
                    updated_at: now,
                };
                items.push(it.clone());
                self.save(session_id, &items);
                Ok(it)
            }
            (None, Some(_)) => Err("version-conflict".into()),
        }
    }

    /// delete:删除某条反馈(if_version 须匹配当前值)。成功 = Ok(被删条目)。
    pub fn delete(
        &self,
        session_id: &str,
        message_id: &str,
        if_version: &str,
    ) -> Result<MessageFeedbackItem, String> {
        let mut items = self.load(session_id);
        let Some(i) = items.iter().position(|it| it.message_id == message_id) else {
            return Err("version-conflict".into());
        };
        if items[i].version != if_version {
            return Err("version-conflict".into());
        }
        let removed = items.remove(i);
        self.save(session_id, &items);
        Ok(removed)
    }
}

/// 一条 slash 命令描述(host 注册表形态)。
/// 命令名 lowercase 无前导斜杠;`input` 可带 hint(参数 ghost text)。
#[derive(Debug, Clone, PartialEq)]
pub struct CommandDescriptor {
    /// 命令名(lowercase,无 `/`;`/^[a-z][a-z0-9_-]*$/`)
    pub name: &'static str,
    /// 描述(菜单展示)
    pub description: &'static str,
    /// 参数 hint(Some = leadingInput,可 claim 补参数;None = 无参 execute)
    pub hint: Option<&'static str>,
}

/// 内置命令目录(host 注册表;静态声明 + AppHost 执行分派)。
/// 命令选中 → `AppHost::execute_command`(host 直接执行,非发模型)。
pub fn builtin_commands() -> Vec<CommandDescriptor> {
    vec![
        CommandDescriptor {
            name: "plan",
            description: "进入或退出计划模式",
            // 无 hint = 菜单点击立即执行(进计划模式,chip 随回声点亮);
            // off/首条任务走手打斜杠:/plan off、/plan <任务>
            hint: None,
        },
        CommandDescriptor {
            name: "export",
            description: "导出本会话日志为 ZIP 归档",
            hint: None,
        },
        CommandDescriptor {
            name: "goal",
            description: "查看或设置长期任务的目标",
            hint: Some("[object|clear|edit <object>|pause|resume]"),
        },
        CommandDescriptor {
            name: "model",
            description: "查看或切换当前模型",
            hint: Some("[model]"),
        },
    ]
}

/// 应用宿主:多会话注册表 + 下行总线 + 未决交互表
pub struct AppHost {
    /// 启动工作区(--workspace;仅引导期兜底,默认工作区权威 = 清单第 0 位)
    workspace: PathBuf,
    /// 工作区清单(默认 + 添加;名称 = basename)
    workspaces: std::sync::RwLock<Vec<PathBuf>>,
    /// 会话根(~/.dshrs/sessions;源 ~/.dsh/sessions 同构)
    sessions_root: PathBuf,
    fake: bool,
    api_key: String,
    /// 装配默认(model/dialect/prompt;workspace 级 dsh.toml 在 attach 时读)
    base: Resolved,
    sessions: std::sync::RwLock<HashMap<String, Arc<SessionSlot>>>,
    /// 驻留子代理认领集(防双挂:同一子会话同时至多一个驻留任务)
    live_children: std::sync::Mutex<std::collections::HashSet<String>>,
    /// 子代理注册表 jobs 源(父会话 id → 弱引用;registry 随工具释放)
    jobs_sources: std::sync::Mutex<HashMap<String, dsh_tools::subagent::WeakRegistry>>,
    /// 子会话事件翻译器(按子会话持计数器状态,translate→mux 实时流)
    subagent_translators: std::sync::Mutex<HashMap<String, crate::translate::Translator>>,
    /// 用户级设置(~/.dshrs/settings.json;setter 落盘与冷装配读取)
    settings: SettingsStore,
    /// 钥匙串端口(生产 OS 实现;测试经 [`AppHost::set_keychain`] 注入)
    keychain: std::sync::RwLock<Arc<dyn KeychainPort>>,
    /// 会话 → 模型覆盖(空 = 用默认;切换时 detach,下次 prompt 重装配)
    model_overrides: std::sync::RwLock<HashMap<String, String>>,
    /// 会话 → preset 覆盖(standard / minimal / 工作区自定义)
    preset_overrides: std::sync::RwLock<HashMap<String, String>>,
    /// 会话 → 推理等级覆盖(low / high / max;空 = 默认)
    effort_overrides: std::sync::RwLock<HashMap<String, String>>,
    /// 会话 → 重命名标题(持久化 .dsh-titles.json;优先于首条投影)。
    /// LLM 语义标题(4b)也写入此映射——手动 rename 随时覆盖,二者不冲突。
    titles: std::sync::RwLock<HashMap<String, String>>,
    /// LLM 标题生成中的会话集(去重:并发 turn 启动的重复生成只一个在跑;
    /// 以 in-flight 集合实现生成结果的覆盖/替换语义)
    title_gen_inflight: std::sync::Mutex<std::collections::HashSet<String>>,
    mux: broadcast::Sender<ServerRequest>,
    host: broadcast::Sender<ServerRequest>,
    pending: Mutex<HashMap<String, PendingQuestion>>,
    /// fake 模式脚本(每次 stream 调用消费一段;测试注入)
    fake_script: Mutex<Vec<Vec<LlmEvent>>>,
    /// fake 模式 LLM 标题输出(测试注入;None = fake 不生成标题,保持回退)。
    /// 真实模式走 `build_raw_transport` 的一次请求,不经此字段。
    fake_title: Mutex<Option<String>>,
    /// provider 模型清单探测缓存(provider id → 清单;键缺席 = 未探测,
    /// 空 Vec = 显式配置之外的探测失败已被刷新写入)
    models_cache: Mutex<HashMap<String, Vec<String>>>,
    /// 全局检索索引(~/.dshrs/search.db;懒建——首次检索时打开)
    search: tokio::sync::Mutex<Option<dsh_host::search::SearchIndex>>,
    /// 消息反馈 sidecar(~/.dshrs/feedback;per-session JSON,与日志分离)
    feedback: MessageFeedbackStore,
    /// 图片附件对象存储(~/.dshrs/attachments/v1;prompt 准入 /
    /// 历史图读取 / 请求期字节来源 / 导出 ZIP 同一权威)
    attachments: dsh_host::AttachmentStore,
}

/// fake 演示用的模型清单(演示数据;真实模式不落此分支)
fn demo_models() -> Vec<String> {
    vec!["deepseek-v4-flash".into(), "deepseek-v4-pro".into()]
}

/// 演示回声脚本(--fake 演示模式且未注入测试脚本时,按会话生成):
/// 固定回声 + 模拟用量(状态栏/上下文圆环在演示环境可见;真实模式由
/// provider 返回)。首段为 mermaid 渲染演示(流式分块,闭合围栏前显示
/// 源码、闭合瞬间转图),其余 19 段回声——多轮可用,不跨会话共享
fn demo_fake_segments() -> Vec<Vec<LlmEvent>> {
    let mut segments: Vec<Vec<LlmEvent>> = vec![mermaid_demo_segment()];
    segments.extend((0..19).map(|_| echo_segment()));
    segments
}

/// 常规回声段:固定回声 + 模拟用量(状态栏/上下文圆环在演示环境可见)
fn echo_segment() -> Vec<LlmEvent> {
    vec![
        LlmEvent::Chunk("(fake echo) ".into()),
        LlmEvent::AssistantMessage(json!({
            "content": "(fake echo) 已收到;真实模式请去掉 --fake。"
        })),
        LlmEvent::Usage(json!({
            "prompt_tokens": 1200,
            "completion_tokens": 300,
            "prompt_cache_hit_tokens": 900,
            "ttftMs": 150,
        })),
        LlmEvent::Done,
    ]
}

/// mermaid 演示段(首段):`just desktop-run --fake` 的手动验收载体。
/// 分块流式覆盖(计划 Step 4):①闭合围栏前显示源码、闭合瞬间转图
/// (fig1);②故意失败的图回退为代码块(fig2——模型输出半成品时不是
/// bug 而是 fallback);③超宽图横向滚动(fig3,14 节点链);中文标签。
fn mermaid_demo_segment() -> Vec<LlmEvent> {
    const CHUNKS: [&str; 6] = [
        "这是 mermaid 渲染演示:中文窄图 / 失败回退 / 超宽横向滚动。",
        "\n\n```mermaid\nflowchart LR\n    A[阅读设计文档] --> B{方案可行?}\n    B -->|是| C1[多会话装配层]\n    B -->|否| C2[回退测试]\n    C1 & C2 --> D[WASM 组件]\n",
        "```\n",
        "\n```mermaid\nclassDiagram\n    class\n```\n",
        "\n```mermaid\nflowchart LR\n    N0[0] --> N1[1] --> N2[2] --> N3[3] --> N4[4] --> N5[5] --> N6[6] --> N7[7] --> N8[8] --> N9[9] --> N10[10] --> N11[11] --> N12[12] --> N13[13]\n```\n",
        "\n演示结束;真实模式请去掉 --fake。\n",
    ];
    let full = CHUNKS.concat();
    let mut events: Vec<LlmEvent> = CHUNKS
        .iter()
        .map(|c| LlmEvent::Chunk((*c).into()))
        .collect();
    events.push(LlmEvent::AssistantMessage(json!({ "content": full })));
    events.push(LlmEvent::Usage(json!({
        "prompt_tokens": 1200,
        "completion_tokens": 300,
        "prompt_cache_hit_tokens": 900,
        "ttftMs": 150,
    })));
    events.push(LlmEvent::Done);
    events
}

/// 重命名标题持久化文件(workspace 顶层,JSON 对象)
const TITLES_FILE: &str = ".dsh-titles.json";

/// 添加工作区持久化文件(默认工作区顶层;路径数组)
const WORKSPACES_FILE: &str = ".dsh-workspaces.json";

/// dsh 会话根(env `DSH_RS_HOME` 覆盖;默认 `~/.dshrs`,与 `DSH_HOME`/
/// `~/.dsh` 约定同构,独立目录名防止两宿主会话混存)
fn default_dshrs_root() -> PathBuf {
    if let Some(h) = std::env::var_os("DSH_RS_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home).join(".dshrs")
}

/// 项目目录键:
/// 路径分隔符(`/` `\` `:`)→ `-`(连续折叠),危险码点 `~XXXX` 转义,
/// 去前导 `-`,包 `--…--`,截断 251 字符;空路径 → `root`。
/// 例:`/Volumes/DATA/projects/quant-forge` → `--Volumes-DATA-projects-quant-forge--`
fn project_key(cwd: &str) -> String {
    let mut readable = String::new();
    let mut separator_run = false;
    for ch in cwd.chars() {
        if ch == '/' || ch == '\\' || ch == ':' {
            if !separator_run {
                readable.push('-');
            }
            separator_run = true;
        } else if ch != '~' && (ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')) {
            readable.push(ch);
            separator_run = false;
        } else {
            readable.push_str(&format!("~{:04X}", ch as u32));
            separator_run = false;
        }
    }
    let slug = readable.trim_start_matches('-').to_string();
    let slug: String = if slug.is_empty() {
        "root".to_string()
    } else {
        slug.chars().take(251).collect()
    };
    format!("--{slug}--")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 从会话日志文件读全部事件(供权限 fold;无/损坏 = None)。
/// 冷重放与 live 同源:权限值一律读盘,不依赖附着态。
fn load_envelopes(path: &Path) -> Option<Vec<EventEnvelope>> {
    dsh_app::load_log(path.to_str()?)
        .ok()
        .map(|log| log.iter().cloned().collect())
}

/// 会话血缘 header 文件路径(会话目录下;存 parent/origin)
fn session_header_path(slot: &Path) -> PathBuf {
    slot.join("header.json")
}

/// 读会话 header 血缘(parent/origin;无/损坏 = None)
fn read_session_header(slot: &Path) -> (Option<String>, Option<String>) {
    std::fs::read_to_string(session_header_path(slot))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .map(|v| {
            (
                v["parentSessionId"].as_str().map(String::from),
                v["origin"].as_str().map(String::from),
            )
        })
        .unwrap_or((None, None))
}

/// 写会话 header 血缘(parent/origin),以会话目录为名(lazy 创建目录)
fn write_session_header(slot: &Path, parent: Option<&str>, origin: Option<&str>) {
    if std::fs::create_dir_all(slot).is_err() {
        return;
    }
    let mut m = serde_json::Map::new();
    if let Some(p) = parent {
        m.insert("parentSessionId".into(), Value::String(p.into()));
    }
    if let Some(o) = origin {
        m.insert("origin".into(), Value::String(o.into()));
    }
    let _ = std::fs::write(
        session_header_path(slot),
        serde_json::to_string(&Value::Object(m)).unwrap_or_default(),
    );
}

/// epoch 毫秒 → ISO-8601(WorkspaceView 时间字段;宿主侧墙钟)
fn iso8601(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    let rem = secs.rem_euclid(86400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

/// epoch 天数 → 公历日期(Howard Hinnant civil_from_days)
fn civil_from_days(z: i64) -> (i64, u64, u64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u64;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 下行帧铸造(uuid rpcId;交互帧由调用方构造同 rpcId 帧)
fn frame(method: &str, payload: Value) -> ServerRequest {
    ServerRequest {
        r#type: "server-request".into(),
        rpc_id: Uuid::new_v4().to_string(),
        method: method.into(),
        payload,
    }
}

/// 会话事件帧
fn event_frame(session_id: &str, event: crate::proto::SessionEvent) -> Option<ServerRequest> {
    let payload = serde_json::to_value(SessionEventFrame {
        session_id: session_id.into(),
        event,
        view: None,
    })
    .ok()?;
    Some(frame("session/event", payload))
}

/// 从日志翻译并广播指定事件(append 后的回声;`seq=None` 时取最后一条)。
/// 与 turn 事件同一翻译表,计数器经全量预热。**必须传调用方刚 append
/// 的那条 seq**:此前「只翻最后一条」在 append→读日志的间隙被并发
/// append 插队时(SetMode 落档后紧随的入队 splice),回声永久丢失、
/// 桌面收不到 plan/mode。
fn broadcast_event(
    provider: &ProviderInfo,
    log: &Mutex<EventLog>,
    session_id: &str,
    mux: &broadcast::Sender<ServerRequest>,
    seq: Option<u64>,
) {
    let Ok(l) = log.lock() else { return };
    let mut tr = Translator::new(provider.clone());
    for ev in l.iter() {
        tr.translate(ev);
    }
    let target = match seq {
        Some(s) => l.iter().find(|e| e.seq == s),
        None => l.iter().next_back(),
    };
    if let Some(ev) = target
        && let Some(event) = tr.translate(ev)
        && let Some(f) = event_frame(session_id, event)
    {
        let _ = mux.send(f);
    }
}

/// 迁移旧布局会话文件到 `~/.dshrs/sessions/<key>/<id>/session.jsonl`
/// (幂等:目标已存在跳过):
/// - 旧布局 A:工作区根 `s-*.jsonl` + 根 `.archive/`
/// - 旧布局 B:工作区 `.dshrs/s-*.jsonl` + `.dshrs/.archive/`(中间态)
fn migrate_legacy_layout(ws: &Path, sessions_root: &Path) {
    let proj = sessions_root.join(project_key(&ws.display().to_string()));
    for base in [ws.to_path_buf(), ws.join(".dshrs")] {
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|x| x != "jsonl") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let dst = proj.join(stem).join("session.jsonl");
                if !dst.exists() {
                    std::fs::create_dir_all(dst.parent().unwrap_or(&proj)).ok();
                    let _ = std::fs::rename(&path, &dst);
                }
            }
        }
    }
    for base in [ws.join(".archive"), ws.join(".dshrs").join(".archive")] {
        if !base.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|x| x != "jsonl") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let dst = proj.join(".archive").join(stem).join("session.jsonl");
                if !dst.exists() {
                    std::fs::create_dir_all(dst.parent().unwrap_or(&proj)).ok();
                    let _ = std::fs::rename(&path, &dst);
                }
            }
        }
    }
}

/// 会话标题派生:首条 user/message 内容取回退标题
/// (`fallbackSessionTitle`:5 词 / 40 字节)。无消息 → None
/// (空串会遮蔽客户端的清单摘录投影——标题仅在真实存在时下发)。
/// 与手动 `rename`(`.dsh-titles.json`)叠加:本函数仅作无重命名时的回退。
fn title_of(events: &[EventEnvelope]) -> Option<String> {
    events
        .iter()
        .find(|e| e.r#type == "user/message")
        .and_then(|e| e.data["content"].as_str())
        .map(|s| crate::title::fallback_session_title(s, 5, 40))
        .filter(|t| !t.is_empty())
}

/// 4b:LLM 语义标题的 system prompt
/// (目标 5 词 / 10 汉字,base 默认)。
fn title_system_prompt() -> String {
    "Create a concise title for an AI coding-assistant session from the supplied human messages.\n\
Return only the title on one line, **in plain text of natural language**, with no quotes, \
prefix, explanation, Markdown, XML, or terminal control codes. No code is allowed.\n\
Use the language of the messages.\n\
Aim for about 5 words in non-CJK languages or 10 CJK characters."
        .to_string()
}

/// 4b:一次性零工具请求 header(零历史;仅模型与 system,temp 0)。
fn title_request_header(model: &str) -> RequestHeader {
    RequestHeader {
        model: model.to_string(),
        system: title_system_prompt(),
        temperature: 0.0,
        reasoning_effort: None,
        tools: Vec::new(),
    }
}

/// 4b:帧定携带用户消息的 JSON 数组为一条 user 消息(源
/// `frameMessages`:`Generate the session title from this JSON array of human messages:\n...`)。
fn frame_title_messages(first_text: &str) -> Value {
    let array = serde_json::to_string(&json!([{ "text": first_text }])).unwrap_or_default();
    let framed =
        format!("Generate the session title from this JSON array of human messages:\n{array}");
    json!([{ "role": "user", "content": [{ "type": "text", "text": framed }] }])
}

/// 4b:从流式事件累积标题文本 → normalize(源 `generateSessionTitleWithLlm`
/// 的 BlockAssembler 文本面;maxTitleBytes=80,base 默认)。
fn title_from_events(events: &[LlmEvent]) -> String {
    let mut text = String::new();
    for ev in events {
        match ev {
            LlmEvent::Chunk(delta) => text.push_str(delta),
            LlmEvent::AssistantMessage(m) => {
                if let Some(c) = m["content"].as_str() {
                    text.push_str(c);
                }
            }
            _ => {}
        }
    }
    crate::title::normalize_session_title(&text, 80)
}

impl AppHost {
    /// 构建宿主(fake = 假 provider,自检/测试)。装配失败即拒(fail-fast)
    pub fn new(workspace: PathBuf, fake: bool, api_key: &str) -> anyhow::Result<Self> {
        Self::new_at(workspace, fake, api_key, default_dshrs_root())
    }

    /// 指定会话根构建(测试注入临时根,避免污染 ~/.dshrs)
    pub fn new_at(
        workspace: PathBuf,
        fake: bool,
        api_key: &str,
        sessions_root: PathBuf,
    ) -> anyhow::Result<Self> {
        let workspace_clone = workspace.clone();
        // workspace 目录缺失时创建(否则后续建会话/写日志全部静默失败)
        std::fs::create_dir_all(&workspace_clone).map_err(|e| {
            anyhow::anyhow!("workspace 目录不可用 {}: {e}", workspace_clone.display())
        })?;
        let base = Resolved::resolve(
            dsh_app::ResolveArgs {
                workspace: Some(workspace_clone.display().to_string()),
                ..Default::default()
            },
            &workspace_clone.join("dsh.toml"),
        )?;
        let (mux, _) = broadcast::channel(512);
        let (host, _) = broadcast::channel(128);
        let titles = std::fs::read_to_string(workspace_clone.join(TITLES_FILE))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // 设置先行:工作区注册表存于 settings.json
        let settings = SettingsStore::open(sessions_root.join("settings.json"));
        let reg_paths = settings.read().workspace_paths.clone();
        let workspaces: Vec<PathBuf> = if reg_paths.is_empty() {
            // 未初始化:一次性导入旧 .dsh-workspaces.json(旧格式)
            let added: Vec<PathBuf> =
                std::fs::read_to_string(workspace_clone.join(WORKSPACES_FILE))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())
                    .collect();
            let mut ws = vec![workspace_clone.clone()];
            ws.extend(added);
            ws
        } else {
            // 注册表顺序即显示顺序;缺席目录剔除,默认工作区保底在列
            let mut ws: Vec<PathBuf> = reg_paths
                .into_iter()
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .collect();
            if !ws.contains(&workspace_clone) {
                ws.insert(0, workspace_clone.clone());
            }
            ws
        };
        // 迁移固化:从旧文件装载的清单回写 settings(一次性;此后旧文件
        // 不再被读——删除亦不影响)
        if settings.read().workspace_paths.is_empty() && workspaces.len() > 1 {
            let paths: Vec<String> = workspaces.iter().map(|p| p.display().to_string()).collect();
            let _ = settings.update(|s| s.workspace_paths = paths);
        }
        // 附件存储根与会话根同域(内容寻址对象,目录懒建)
        let attachments = dsh_host::AttachmentStore::new(sessions_root.join("attachments/v1"));
        let feedback = MessageFeedbackStore::new(sessions_root.join("feedback"));
        // 迁移旧布局(工作区根/工作区 .dshrs)到 ~/.dshrs(幂等)
        std::fs::create_dir_all(&sessions_root).ok();
        for ws in &workspaces {
            migrate_legacy_layout(ws, &sessions_root);
        }
        Ok(Self {
            workspace: workspace_clone,
            workspaces: std::sync::RwLock::new(workspaces),
            sessions_root,
            fake,
            api_key: api_key.to_string(),
            base,
            settings,
            keychain: std::sync::RwLock::new(Arc::new(OsKeychain)),
            sessions: std::sync::RwLock::new(HashMap::new()),
            live_children: std::sync::Mutex::new(std::collections::HashSet::new()),
            jobs_sources: std::sync::Mutex::new(HashMap::new()),
            subagent_translators: std::sync::Mutex::new(HashMap::new()),
            model_overrides: std::sync::RwLock::new(HashMap::new()),
            preset_overrides: std::sync::RwLock::new(HashMap::new()),
            effort_overrides: std::sync::RwLock::new(HashMap::new()),
            titles: std::sync::RwLock::new(titles),
            title_gen_inflight: std::sync::Mutex::new(std::collections::HashSet::new()),
            fake_title: std::sync::Mutex::new(None),
            mux,
            host,
            pending: Mutex::new(HashMap::new()),
            fake_script: Mutex::new(Vec::new()),
            models_cache: Mutex::new(HashMap::new()),
            search: tokio::sync::Mutex::new(None),
            attachments,
            feedback,
        })
    }

    /// 探测 provider 模型清单:`GET {base}/models`。鉴权头按方言:
    /// anthropic = x-api-key + anthropic-version,openai 系 = Bearer。
    /// 失败(网络/鉴权/解析)→ 空 Vec——**不硬编码模型名**
    async fn fetch_provider_models(&self, provider: &ProviderEntry, ws: &Path) -> Vec<String> {
        let key = self.resolve_provider_key(provider, ws);
        Self::models_request(&provider.base_url, &provider.dialect, key).await
    }

    /// 裸模型发现(设置页「从端点获取」):显式 key 优先,缺席回退该
    /// provider 的四级凭据链;两者皆无 → 无鉴权头直试(部分网关免鉴权)
    pub async fn discover_models(
        &self,
        base_url: String,
        dialect: String,
        api_key: Option<String>,
        provider_id: Option<String>,
    ) -> Vec<String> {
        let key = match api_key {
            Some(k) => Some(k),
            None => provider_id.and_then(|pid| {
                let provider = self.settings.read().provider(Some(&pid));
                self.resolve_provider_key(&provider, &self.default_workspace())
            }),
        };
        Self::models_request(&base_url, &dialect, key).await
    }

    /// GET `{base}/models` 并解析 `data[].id`;鉴权头按方言
    async fn models_request(base_url: &str, dialect: &str, key: Option<String>) -> Vec<String> {
        let url = format!("{}/models", base_url.trim_end_matches('/'));
        let client = reqwest::Client::new();
        let mut req = client.get(&url);
        if let Some(key) = key {
            req = if dialect == "anthropic" {
                req.header("x-api-key", &key)
                    .header("anthropic-version", "2023-06-01")
            } else {
                req.header("Authorization", format!("Bearer {key}"))
            };
        }
        let res = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[dsh-core] 模型探测请求失败: {e}");
                return vec![];
            }
        };
        if !res.status().is_success() {
            eprintln!("[dsh-core] 模型探测 HTTP {}", res.status());
            return vec![];
        }
        let body = match res.json::<serde_json::Value>().await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[dsh-core] 模型探测解析失败: {e}");
                return vec![];
            }
        };
        body["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 确保模型清单就绪(describe 前调用):显式配置 > 探测缓存 > fake 演示 > 空。
    /// 真实模式探测失败保持缺席——UI 模型菜单为空,不做编造回退
    pub async fn ensure_models(&self) {
        if self.base.models.is_some() {
            return;
        }
        let provider = self.default_provider();
        if self
            .models_cache
            .lock()
            .expect("models_cache 锁中毒")
            .contains_key(&provider.id)
        {
            return;
        }
        if self.fake {
            self.models_cache
                .lock()
                .expect("models_cache 锁中毒")
                .insert(provider.id.clone(), demo_models());
            return;
        }
        let fetched = self.fetch_provider_models(&provider, &self.default_workspace()).await;
        if !fetched.is_empty() {
            self.models_cache
                .lock()
                .expect("models_cache 锁中毒")
                .insert(provider.id.clone(), fetched);
        }
        // 失败:不缓存(下次 describe/手动刷新重试)
    }

    /// 手动刷新 provider 模型清单(设置页刷新入口;强制重探,
    /// 结果——含空——写入缓存,与 ensure_models 的「失败不缓存」区分)
    pub async fn refresh_models(&self, provider_id: &str) -> Vec<String> {
        let provider = self.settings.read().provider(Some(provider_id));
        let fetched = if self.fake && provider_id == self.default_provider().id {
            demo_models()
        } else {
            self.fetch_provider_models(&provider, &self.default_workspace()).await
        };
        self.models_cache
            .lock()
            .expect("models_cache 锁中毒")
            .insert(provider_id.to_string(), fetched.clone());
        fetched
    }

    /// provider 模型清单:**用户圈定清单**(设置页 models 字段)优先,
    /// 否则探测缓存;缺省 = 空。聊天模型选择器(describe.models)经此接上
    pub fn models_for(&self, provider_id: &str) -> Vec<String> {
        let configured = self.settings.read().provider(Some(provider_id)).models;
        if !configured.is_empty() {
            return configured;
        }
        self.models_cache
            .lock()
            .expect("models_cache 锁中毒")
            .get(provider_id)
            .cloned()
            .unwrap_or_default()
    }

    /// 拉取 provider 计费快照:GET 配置 URL(鉴权头按方言)→ JSON 路径
    /// 求值 → BillingSnapshot 写回 settings.billing_cache(落盘持久化)。
    /// HTTP/解析/路径全部未命中 → Err(错误串,设置页通告用)
    pub async fn fetch_billing(&self, provider_id: &str, ws: &Path) -> Result<(), String> {
        let provider = self.settings.read().provider(Some(provider_id));
        let Some(billing) = provider.billing.clone() else {
            return Err("该 provider 未配置计费端点".into());
        };
        let Some(key) = self.resolve_provider_key(&provider, ws) else {
            return Err("凭据各级缺席,无法查询计费".into());
        };
        let snapshot = Self::billing_request(&billing, &provider.dialect, &key).await?;
        self.set_billing_cache(provider_id, snapshot)
            .map_err(|e| e.message)
    }

    /// 计费试查(编辑器「立即刷新」用**表单当前值**,不读已保存配置、
    /// 不落缓存;key 显式优先回退四级链)。只求值,返回快照
    pub async fn test_billing(
        &self,
        cfg: BillingConfig,
        dialect: String,
        api_key: Option<String>,
        provider_id: Option<String>,
    ) -> Result<BillingSnapshot, String> {
        let key = match api_key {
            Some(k) if !k.is_empty() => Some(k),
            _ => provider_id.and_then(|pid| {
                let provider = self.settings.read().provider(Some(&pid));
                self.resolve_provider_key(&provider, &self.default_workspace())
            }),
        }
        .ok_or("凭据各级缺席,无法查询计费")?;
        Self::billing_request(&cfg, &dialect, &key).await
    }

    /// 计费缓存落盘(试查通过后由桌面侧显式写入)
    pub fn set_billing_cache(
        &self,
        provider_id: &str,
        snapshot: BillingSnapshot,
    ) -> Result<(), RpcError> {
        self.settings
            .update(|s| {
                if let Some(p) = s.providers.iter_mut().find(|p| p.id == provider_id) {
                    p.billing_cache = Some(snapshot.clone());
                }
            })
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))
    }

    /// GET 计费 URL → 按路径集求值 → 快照(纯请求,无副作用)
    async fn billing_request(
        cfg: &BillingConfig,
        dialect: &str,
        key: &str,
    ) -> Result<BillingSnapshot, String> {
        let client = reqwest::Client::new();
        let mut req = client.get(&cfg.url);
        req = if dialect == "anthropic" {
            req.header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
        } else {
            req.header("Authorization", format!("Bearer {key}"))
        };
        let res = req.send().await.map_err(|e| format!("请求失败:{e}"))?;
        if !res.status().is_success() {
            return Err(format!("HTTP {}", res.status()));
        }
        // 先按文本读(错误信息可携带响应片段,路径未命中时用户能看到
        // 端点实际返回了什么),再解析 JSON
        let text = res.text().await.map_err(|e| format!("读取失败:{e}"))?;
        let body: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("响应解析失败({e}):{}", Self::body_snippet(&text)))?;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let snapshot = match cfg.kind {
            BillingKind::Balance => {
                let amount = cfg
                    .paths
                    .balance
                    .as_deref()
                    .and_then(|p| json_path(&body, p))
                    .and_then(|v| {
                        v.as_str()
                            .map(String::from)
                            .or_else(|| v.as_f64().map(|f| f.to_string()))
                    })
                    .ok_or("余额路径未命中")?;
                let currency = cfg
                    .paths
                    .currency
                    .as_deref()
                    .and_then(|p| json_path(&body, p))
                    .and_then(|v| v.as_str().map(String::from));
                BillingSnapshot::Balance {
                    fetched_at_ms: now_ms,
                    amount,
                    currency,
                }
            }
            BillingKind::Usage => {
                let pct = |p: &Option<String>| {
                    p.as_deref()
                        .and_then(|p| json_path(&body, p))
                        .and_then(json_percent)
                };
                BillingSnapshot::Usage {
                    fetched_at_ms: now_ms,
                    pct_5h: pct(&cfg.paths.usage_5h),
                    pct_7d: pct(&cfg.paths.usage_7d),
                    resets: cfg
                        .paths
                        .resets
                        .as_deref()
                        .and_then(|p| json_path(&body, p))
                        .and_then(|v| v.as_str().map(String::from)),
                }
            }
        };
        let has_data = match &snapshot {
            BillingSnapshot::Balance { amount, .. } => !amount.is_empty(),
            BillingSnapshot::Usage { pct_5h, pct_7d, .. } => pct_5h.is_some() || pct_7d.is_some(),
        };
        if !has_data {
            return Err(format!("路径未命中,响应:{}", Self::body_snippet(&text)));
        }
        Ok(snapshot)
    }

    /// 响应体诊断片段(去空白截 160 字;错误信息里展示端点实际返回)
    fn body_snippet(text: &str) -> String {
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let cut: String = flat.chars().take(160).collect();
        if flat.chars().count() > 160 {
            format!("{cut}…")
        } else {
            cut
        }
    }

    /// 钥匙串端口注入(测试 seam,同 set_fake_script 模式)
    pub fn set_keychain(&self, port: Arc<dyn KeychainPort>) {
        *self.keychain.write().expect("keychain 锁中毒(宿主 bug)") = port;
    }

    /// provider 凭据解析(四级链;显式注入 = 启动时传入的非空 key)
    fn resolve_provider_key(&self, provider: &ProviderEntry, ws: &Path) -> Option<String> {
        let explicit = if self.api_key.is_empty() {
            None
        } else {
            Some(self.api_key.as_str())
        };
        resolve_credential(
            explicit,
            provider,
            ws,
            &**self.keychain.read().expect("keychain 锁中毒(宿主 bug)"),
        )
    }

    /// 会话所属工作区的生效 provider(设置工作区默认 > 内置回落)
    fn provider_for(&self, ws: &Path) -> ProviderEntry {
        let s = self.settings.read();
        let key = project_key(&ws.display().to_string());
        s.provider(s.workspace(&key).provider.as_deref())
    }

    /// 默认工作区 provider(describe/新会话/启动探测)
    fn default_provider(&self) -> ProviderEntry {
        self.provider_for(&self.default_workspace())
    }

    /// 工作区名 → 路径(默认工作区名 = basename)
    fn workspace_of(&self, name: &str) -> Option<PathBuf> {
        self.workspaces
            .read()
            .expect("workspaces 锁中毒")
            .iter()
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == name)
            })
            .cloned()
    }

    /// 默认工作区 = 清单第 0 位(UI 侧 default_workspace 语义的唯一权威;
    /// 空表仅引导期出现,回落启动目录)
    pub fn default_workspace(&self) -> PathBuf {
        self.workspaces
            .read()
            .expect("workspaces 锁中毒")
            .first()
            .cloned()
            .unwrap_or_else(|| self.workspace.clone())
    }

    /// 默认工作区名(basename;list_sessions 默认区判定用)
    fn default_workspace_name(&self) -> String {
        self.default_workspace()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string()
    }

    /// 会话 id → (所在工作区, 文件 stem)。
    /// 非默认工作区会话 id = "<ws 名>/<stem>";默认工作区 = 裸 stem(兼容)
    fn resolve_session(&self, id: &str) -> (PathBuf, String) {
        if let Some((ws_name, stem)) = id.split_once('/')
            && let Some(ws) = self.workspace_of(ws_name)
        {
            return (ws, stem.to_string());
        }
        (self.default_workspace(), id.to_string())
    }

    /// 工作区名列表(默认在前)
    pub fn workspace_names(&self) -> Vec<String> {
        self.workspaces
            .read()
            .expect("workspaces 锁中毒")
            .iter()
            .filter_map(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(std::borrow::ToOwned::to_owned)
            })
            .collect()
    }

    /// 添加工作区:目录须存在、名称(= basename)不与现有冲突;持久化 + 广播
    pub fn add_workspace(&self, path: &str) -> Result<String, RpcError> {
        let p = PathBuf::from(path);
        if !p.is_dir() {
            return Err(RpcError::bad_request("目录不存在"));
        }
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| RpcError::bad_request("路径无名"))?
            .to_string();
        if self.workspace_of(&name).is_some() {
            return Err(RpcError::bad_request("同名工作区已存在"));
        }
        self.workspaces.write().expect("workspaces 锁中毒").push(p);
        self.persist_workspaces()?;
        let _ = self.host.send(frame("host/workspace-changed", json!({})));
        Ok(name)
    }

    /// 工作区注册表落盘(settings.workspace_paths 与内存顺序同步)
    fn persist_workspaces(&self) -> Result<(), RpcError> {
        let paths: Vec<String> = self
            .workspaces
            .read()
            .expect("workspaces 锁中毒")
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        self.settings
            .update(|s| s.workspace_paths = paths)
            .map_err(|e| RpcError::internal(format!("工作区清单持久化失败:{e}")))
    }

    /// 工作区显示名(标题覆盖;缺席 = basename)。仅显示层,
    /// 身份与路由恒用 basename
    pub fn workspace_title(&self, name: &str) -> String {
        self.settings
            .read()
            .workspace_titles
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    /// 重命名工作区(= 设置显示标题;空/同名 = 清除覆盖回 basename)
    pub fn rename_workspace(&self, name: &str, title: &str) -> Result<(), RpcError> {
        self.workspace_of(name)
            .ok_or_else(|| RpcError::bad_request("未知工作区"))?;
        let title = title.trim().chars().take(60).collect::<String>();
        self.settings
            .update(|s| {
                if title.is_empty() || title == name {
                    s.workspace_titles.remove(name);
                } else {
                    s.workspace_titles.insert(name.to_string(), title);
                }
            })
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))?;
        let _ = self.host.send(frame("host/workspace-changed", json!({})));
        Ok(())
    }

    /// 移除工作区(仅出清单;默认工作区不可移除)。该工作区的附着会话
    /// 随之卸载(槽移除,文件保留——重新添加即恢复);标题覆盖清理
    pub fn remove_workspace(&self, name: &str) -> Result<(), RpcError> {
        let ws = self
            .workspace_of(name)
            .ok_or_else(|| RpcError::bad_request("未知工作区"))?;
        if ws == self.default_workspace() {
            return Err(RpcError::bad_request("默认工作区不可移除"));
        }
        // 卸载该工作区的附着会话(锁序:sessions → workspaces,与既有写序一致)
        let victims: Vec<String> = self
            .sessions
            .read()
            .expect("sessions 锁中毒")
            .keys()
            .filter(|id| self.resolve_session(id).0 == ws)
            .cloned()
            .collect();
        if !victims.is_empty() {
            let mut slots = self.sessions.write().expect("sessions 锁中毒");
            for id in &victims {
                slots.remove(id);
            }
        }
        self.workspaces
            .write()
            .expect("workspaces 锁中毒")
            .retain(|p| *p != ws);
        self.persist_workspaces()?;
        self.settings
            .update(|s| {
                s.workspace_titles.remove(name);
            })
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))?;
        let _ = self.host.send(frame("host/workspace-changed", json!({})));
        Ok(())
    }

    /// 工作区排序(源 insertBefore 语义:移到 `before` 之前;
    /// before 缺席/未知 = 移到末尾)
    pub fn reorder_workspace(&self, name: &str, before: Option<&str>) -> Result<(), RpcError> {
        let ws = self
            .workspace_of(name)
            .ok_or_else(|| RpcError::bad_request("未知工作区"))?;
        let before_ws = match before {
            Some(b) => Some(
                self.workspace_of(b)
                    .ok_or_else(|| RpcError::bad_request("未知工作区(锚点)"))?,
            ),
            None => None,
        };
        let mut list = self.workspaces.write().expect("workspaces 锁中毒");
        list.retain(|p| *p != ws);
        let at = match &before_ws {
            Some(b) => list.iter().position(|p| p == b).unwrap_or(list.len()),
            None => list.len(),
        };
        list.insert(at, ws);
        drop(list);
        self.persist_workspaces()?;
        let _ = self.host.send(frame("host/workspace-changed", json!({})));
        Ok(())
    }

    /// 系统目录选择:macOS `osascript choose folder` 弹原生对话框,
    /// 返回 POSIX 绝对路径(模态阻塞,须在 spawn_blocking 中调用)。
    /// 取消/不可用 → bad_request。
    pub fn pick_workspace_directory(&self) -> Result<String, RpcError> {
        #[cfg(target_os = "macos")]
        {
            use std::process::Command;
            let out = Command::new("osascript")
                .args([
                    "-e",
                    "POSIX path of (choose folder with prompt \"选择工作区目录\")",
                ])
                .output()
                .map_err(|e| RpcError::internal(format!("无法启动系统对话框:{e}")))?;
            if !out.status.success() {
                return Err(RpcError::bad_request("用户取消或系统对话框不可用"));
            }
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if path.is_empty() {
                return Err(RpcError::bad_request("用户取消"));
            }
            Ok(path)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = self;
            Err(RpcError::internal("系统目录选择暂仅支持 macOS"))
        }
    }

    /// 会话日志快照(附着 = 活日志克隆;冷会话 = 文件重放)
    fn session_log(&self, id: &str) -> Result<Vec<EventEnvelope>, RpcError> {
        if let Some(slot) = self.get_slot(id)
            && let Some(inner) = slot.inner.get()
        {
            let l = inner
                .log
                .lock()
                .map_err(|_| RpcError::internal("log 锁中毒"))?;
            Ok(l.iter().cloned().collect())
        } else {
            let path = self.slot_path(id);
            if !path.exists() {
                return Err(RpcError::session_not_found(id));
            }
            Ok(dsh_app::load_log(&path.display().to_string())
                .map_err(|e| RpcError::internal(format!("日志重载失败:{e}")))?
                .iter()
                .cloned()
                .collect())
        }
    }

    /// session.trajectory:轨迹台账(记录尾窗 + 全量请求清单)。
    /// 分页按记录 index:before_index 给定时返回更早一窗(「加载更早」)。
    pub fn session_trajectory(
        &self,
        id: &str,
        max_records: usize,
        before_index: Option<u64>,
    ) -> Result<Value, RpcError> {
        let page = self.trajectory_page(id, max_records, before_index)?;
        Ok(serde_json::to_value(page).unwrap_or(Value::Null))
    }

    /// 轨迹台账 typed 出口(桌面进程内直调;RPC 层走 [`Self::session_trajectory`]
    /// 的 JSON 序列化)。max_records clamp 1..2000,before_index = 尾窗向前分页
    pub fn trajectory_page(
        &self,
        id: &str,
        max_records: usize,
        before_index: Option<u64>,
    ) -> Result<crate::trajectory::TrajectoryPage, RpcError> {
        let log = self.session_log(id)?;
        let mut data = crate::trajectory::fold_trajectory(&log);
        // provider 方言后填(fold 是纯函数,不接触装配态)
        for req in &mut data.requests {
            if req.provider.is_empty() {
                req.provider = self.base.dialect.clone();
            }
        }
        let total = data.records.len() as u64;
        let max_records = max_records.clamp(1, 2000);
        let mut records = data.records;
        if let Some(before) = before_index {
            records.retain(|r| r.index < before);
        }
        let has_older = records.len() > max_records;
        if has_older {
            let start = records.len() - max_records;
            records.drain(..start);
        }
        Ok(crate::trajectory::TrajectoryPage {
            records,
            requests: data.requests,
            has_older,
            total,
        })
    }

    /// session.stats:轮/步/LLM 与工具时长/首 token/速率/缓存/tokens/上下文占用。
    /// 全量 fold 与直播推送([`stats::StatsAgg`])共用同一 apply 逻辑;
    /// 冷读(打开会话/丢帧自愈)走此路径,直播增量见 driver_loop
    pub fn session_stats(&self, id: &str) -> Result<Value, RpcError> {
        let log = self.session_log(id)?;
        let mut agg = stats::StatsAgg::default();
        for ev in &log {
            agg.apply(&ev.r#type, &ev.data);
        }
        let breakdown = crate::context::context_breakdown(log.iter());
        Ok(agg.to_json(stats::Breakdown {
            system_tokens: breakdown.system_tokens,
            tools_tokens: breakdown.tools_tokens,
            message_tokens: breakdown.message_tokens,
        }))
    }

    /// 工作区 dsh.toml 显式配置(合并序中设置层的上位;读失败视为无)
    fn ws_config(&self, ws: &Path) -> dsh_host::DshConfig {
        dsh_host::DshConfig::load(&ws.join("dsh.toml")).unwrap_or_default()
    }

    /// 默认模型:settings 默认工作区默认 > provider 默认 > 装配默认;
    /// 当前 provider 探测清单在场且不含配置值时回落清单首项
    /// (避免使用不存在的陈旧默认模型名)
    pub fn default_model(&self) -> String {
        let s = self.settings.read();
        let key = project_key(&self.default_workspace().display().to_string());
        let ws = s.workspace(&key);
        let provider = s.provider(ws.provider.as_deref());
        let configured = ws
            .model
            .clone()
            .or_else(|| provider.default_model.clone())
            .unwrap_or_else(|| self.base.model.clone());
        let list = self.models_for(&provider.id);
        if !list.is_empty() && !list.iter().any(|m| m == &configured) {
            return list[0].clone();
        }
        configured
    }

    /// 会话当前模型(会话覆盖 > 工作区 dsh.toml > 设置工作区默认 > 默认模型)
    pub fn session_model(&self, id: &str) -> String {
        if let Some(m) = self
            .model_overrides
            .read()
            .expect("model_overrides 锁中毒")
            .get(id)
        {
            return m.clone();
        }
        let (ws_root, _) = self.resolve_session(id);
        if let Some(m) = self.ws_config(&ws_root).model {
            return m;
        }
        let key = project_key(&ws_root.display().to_string());
        if let Some(m) = self.settings.read().workspace(&key).model {
            return m;
        }
        self.default_model()
    }

    /// 会话当前访问模式(fold 会话日志最后一个 sandbox/mode;无则默认
    /// workspace-write)。可重放:从会话日志读,跨重启/子代理一致。
    pub fn session_permission(&self, id: &str) -> String {
        self.session_sandbox_mode(id).to_string()
    }

    /// 会话当前 sandbox 模式(fold 磁盘会话日志;无则默认 workspace-write)。
    fn session_sandbox_mode(&self, id: &str) -> &'static str {
        match load_envelopes(&self.slot_path(id)) {
            Some(events) => crate::permission::sandbox_mode_of(&events),
            None => crate::permission::DEFAULT_SANDBOX_MODE,
        }
    }

    /// 会话当前审批策略(fold 磁盘会话日志;无则默认 ask)。
    pub fn session_approval(&self, id: &str) -> String {
        match load_envelopes(&self.slot_path(id)) {
            Some(events) => crate::permission::approval_policy_of(&events).to_string(),
            None => crate::permission::DEFAULT_APPROVAL_POLICY.to_string(),
        }
    }

    /// 会话当前 preset(覆盖 > 工作区 dsh.toml > 设置工作区默认 > standard)
    pub fn session_preset(&self, id: &str) -> String {
        if let Some(p) = self
            .preset_overrides
            .read()
            .expect("preset_overrides 锁中毒")
            .get(id)
        {
            return p.clone();
        }
        let (ws_root, _) = self.resolve_session(id);
        if let Some(p) = self.ws_config(&ws_root).preset {
            return p;
        }
        let key = project_key(&ws_root.display().to_string());
        if let Some(p) = self.settings.read().workspace(&key).preset {
            return p;
        }
        "standard".into()
    }

    /// 可选模型清单(describe 模型菜单;当前 provider 视角)
    /// 模型清单:显式配置 > 探测缓存 > fake 演示 > 空(不硬编码)
    pub fn models(&self) -> Vec<String> {
        if let Some(cfg) = &self.base.models {
            return cfg.clone();
        }
        self.models_for(&self.default_provider().id)
    }

    /// 推理等级清单(DeepSeek V4 thinking.reasoning_effort 取值)
    pub fn efforts(&self) -> Vec<String> {
        vec!["low".into(), "high".into(), "max".into()]
    }

    /// 会话当前推理等级(覆盖 > 工作区 dsh.toml > 设置工作区默认 > 装配默认;
    /// 兜底 High:思考输出需 thinking 参数)
    pub fn session_effort(&self, id: &str) -> Option<String> {
        if let Some(e) = self
            .effort_overrides
            .read()
            .expect("effort_overrides 锁中毒")
            .get(id)
        {
            return Some(e.clone());
        }
        let (ws_root, _) = self.resolve_session(id);
        if let Some(e) = self.ws_config(&ws_root).reasoning_effort {
            return Some(e);
        }
        let key = project_key(&ws_root.display().to_string());
        if let Some(e) = self.settings.read().workspace(&key).effort {
            return Some(e);
        }
        self.base
            .reasoning_effort
            .clone()
            .or_else(|| Some("high".into()))
    }

    /// 切换推理等级(同 set-model 语义:空闲时重装配 + 工作区默认落盘)
    pub fn set_effort(&self, id: &str, effort: &str) -> Result<(), RpcError> {
        if !self.efforts().iter().any(|e| e == effort) {
            return Err(RpcError::bad_request("未知推理等级"));
        }
        self.detach_if_idle(id)?;
        self.effort_overrides
            .write()
            .expect("effort_overrides 锁中毒")
            .insert(id.into(), effort.into());
        self.persist_workspace_default(id, |d| d.effort = Some(effort.into()))
    }

    /// 权限预设清单(sandbox+approval 捆绑)。
    /// 每个预设的 sandbox 模式经 `sandbox/mode` 事件、approval 经
    /// `approval/policy` 事件落档,可重放。
    pub fn permissions(&self) -> Vec<String> {
        crate::permission::PRESETS
            .iter()
            .map(|p| p.name.to_string())
            .collect()
    }

    /// 权限预设选项(含名称描述,供前端 select)
    pub fn permission_options(&self) -> Vec<Value> {
        crate::permission::PRESETS
            .iter()
            .map(|p| {
                json!({
                    "value": p.name,
                    "name": p.name,
                    "sandbox": p.sandbox,
                    "approval": p.approval,
                })
            })
            .collect()
    }

    /// 默认权限预设名(新会话/缺 knob 时 pin;设置通用区权限行)。
    /// 读设置工作区默认 `default_permission_preset`;未设则按默认 knob 推导
    /// (workspace-write)。
    pub fn default_permission(&self) -> String {
        let key = project_key(&self.default_workspace().display().to_string());
        self.settings
            .read()
            .workspace(&key)
            .default_permission_preset
            .clone()
            .unwrap_or_else(|| {
                crate::permission::derive_preset(&crate::permission::KnobState::default())
            })
    }

    /// 设置默认权限预设(落盘为默认工作区默认;新会话 pin 时沿用)。
    /// defaultPreset——捆绑 sandbox+approval 的可选权限预设。
    pub fn set_default_permission_preset(&self, preset: &str) -> Result<(), RpcError> {
        if !crate::permission::PRESETS.iter().any(|p| p.name == preset) {
            return Err(RpcError::bad_request("未知权限预设"));
        }
        let key = project_key(&self.default_workspace().display().to_string());
        self.settings
            .update(|s| {
                s.workspaces
                    .entry(key)
                    .or_default()
                    .default_permission_preset = Some(preset.to_string())
            })
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))
    }

    /// preset 清单(内置 + workspace presets/ 目录;**与内置重名的工作区文件跳过**,
    /// 避免 standard/minimal 重复列出)。元数据真解析(单一来源 = manifest):
    /// 内置走同一加载路径;工作区坏文件以 broken 描述留在清单上
    /// (broken-preset 仍留 roster),不再恒显占位文案
    pub fn presets(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let ws = self.default_workspace();
        for id in ["standard", "minimal"] {
            let Ok(m) = dsh_host::PresetManifest::load(&ws, id) else {
                continue; // 内置解析失败 = 宿主 bug(编译期测试锁);不列死角
            };
            seen.insert(id.to_string());
            out.push(json!({
                "id": id,
                "name": m.metadata.display_name.as_deref().unwrap_or(id),
                "description": m.metadata.description,
            }));
        }
        if let Ok(entries) = std::fs::read_dir(ws.join("presets")) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().is_none_or(|x| x != "yaml") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !seen.insert(stem.to_string()) {
                    continue; // 与内置重名:跳过
                }
                match dsh_host::PresetManifest::load(&ws, stem) {
                    Ok(m) => out.push(json!({
                        "id": stem,
                        "name": m.metadata.display_name.as_deref().unwrap_or(stem),
                        "description": m.metadata.description,
                    })),
                    Err(err) => out.push(json!({
                        "id": stem,
                        "name": stem,
                        "description": format!("(不可用){err}"),
                    })),
                }
            }
        }
        out
    }

    /// 空闲时 detach(覆盖类切换的公共前置:下次 prompt 以新参数重装配)
    fn detach_if_idle(&self, id: &str) -> Result<(), RpcError> {
        let slots = self.sessions.read().expect("sessions 锁中毒");
        if let Some(slot) = slots.get(id) {
            if slot.running.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(RpcError::bad_request("会话运行中,请先停止"));
            }
            drop(slots);
            self.sessions.write().expect("sessions 锁中毒").remove(id);
        }
        Ok(())
    }

    /// 切换会话模型:空闲时 detach + 记覆盖,下次 prompt 以新模型装配;
    /// 运行中拒绝(竞态下旧 turn 语义不明)。同时落盘工作区默认
    /// (重启后冷会话沿用)。校验按会话所属工作区的 provider 模型清单
    pub fn set_model(&self, id: &str, model: &str) -> Result<(), RpcError> {
        let (ws_root, _) = self.resolve_session(id);
        let provider = self.provider_for(&ws_root);
        if !self.models_for(&provider.id).iter().any(|m| m == model) {
            return Err(RpcError::bad_request("未知模型"));
        }
        self.detach_if_idle(id)?;
        self.model_overrides
            .write()
            .expect("model_overrides 锁中毒")
            .insert(id.into(), model.into());
        self.persist_workspace_default(id, |d| d.model = Some(model.into()))
    }

    /// 切换访问模式(经驱动通道落档 sandbox/mode;工具执行时动态 fold
    /// 同一日志——落档即对下一次执行生效,空闲与运行中一致)。
    /// setSandboxMode——写日志事件而非 override+settings,可重放。
    pub async fn set_permission(
        self: &Arc<Self>,
        id: &str,
        permission: &str,
    ) -> Result<(), RpcError> {
        if !crate::permission::SANDBOX_MODES.contains(&permission) {
            return Err(RpcError::bad_request("未知访问模式"));
        }
        let slot = self.attach(id)?;
        let inner = slot.inner.get().expect("attach 后必有 inner");
        inner
            .queue_tx
            .send(Job::SetPermission(permission.into()))
            .map_err(|_| RpcError::internal("session worker 已退出"))?;
        Ok(())
    }

    /// 切换审批策略(经驱动通道落档 approval/policy;setApprovalPolicy 语义;
    /// 同 set_permission:落档即生效,运行中不拒)
    pub async fn set_approval(self: &Arc<Self>, id: &str, policy: &str) -> Result<(), RpcError> {
        if !crate::permission::APPROVAL_POLICIES.contains(&policy) {
            return Err(RpcError::bad_request("未知审批策略"));
        }
        let slot = self.attach(id)?;
        let inner = slot.inner.get().expect("attach 后必有 inner");
        inner
            .queue_tx
            .send(Job::SetApproval(policy.into()))
            .map_err(|_| RpcError::internal("session worker 已退出"))?;
        Ok(())
    }

    /// 切换 preset(同 set-model 语义:空闲时重装配 + 工作区默认落盘)
    pub fn set_preset(&self, id: &str, preset: &str) -> Result<(), RpcError> {
        let known = self.presets().iter().any(|p| p["id"] == preset);
        if !known {
            return Err(RpcError::bad_request("未知 preset"));
        }
        self.detach_if_idle(id)?;
        self.preset_overrides
            .write()
            .expect("preset_overrides 锁中毒")
            .insert(id.into(), preset.into());
        self.persist_workspace_default(id, |d| d.preset = Some(preset.into()))
    }

    /// 会话所属工作区默认落盘(projectKey 键控,与冷装配读取同键)
    fn persist_workspace_default(
        &self,
        id: &str,
        f: impl FnOnce(&mut crate::settings::WorkspaceDefaults),
    ) -> Result<(), RpcError> {
        let (ws_root, _) = self.resolve_session(id);
        let key = project_key(&ws_root.display().to_string());
        self.settings
            .update(|s| f(s.workspaces.entry(key).or_default()))
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))
    }

    /// 设置快照(设置页数据源):onboarding/provider 注册表/工作区
    /// 默认 + 各 provider 凭据可解析态(**不含明文**)
    pub fn settings_view(&self) -> Value {
        let file = self.settings.read();
        let providers: Vec<Value> = file
            .providers
            .iter()
            .map(|p| {
                let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
                v["credentialReady"] =
                Value::Bool(self.resolve_provider_key(p, &self.default_workspace()).is_some());
                v["modelsCached"] = Value::Bool(!self.models_for(&p.id).is_empty());
                v
            })
            .collect();
        json!({
            "onboarded": file.onboarded,
            "providers": providers,
            "workspaces": serde_json::to_value(&file.workspaces).unwrap_or(Value::Null),
            "defaultProvider": self.default_provider().id,
            "busyEnter": file.busy_enter,
            "language": file.language,
            "appearance": file.appearance,
            "sessionsRoot": self.sessions_root.display().to_string(),
            // 通用区偏好行数据(源 AgentPresetRow / PermissionRow 对应物)
            "presetOptions": self.presets(),
            "permissionOptions": self.permissions(),
            "defaultPreset": self.default_preset(),
            "defaultPermission": self.default_permission(),
        })
    }

    /// 界面语言偏好
    pub fn language(&self) -> String {
        self.settings.read().language.clone()
    }

    /// 设置界面语言偏好(落盘;RS 现仅 zh 实装)
    pub fn set_language(&self, id: &str) -> Result<(), RpcError> {
        if id != "zh" {
            return Err(RpcError::bad_request("界面语言暂仅支持中文"));
        }
        self.settings
            .update(|s| s.language = id.to_string())
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))
    }

    /// 外观偏好(light / dark / system)
    pub fn appearance(&self) -> String {
        self.settings.read().appearance.clone()
    }

    /// 设置外观偏好(落盘)
    pub fn set_appearance(&self, id: &str) -> Result<(), RpcError> {
        if !["light", "dark", "system"].contains(&id) {
            return Err(RpcError::bad_request("未知外观"));
        }
        self.settings
            .update(|s| s.appearance = id.to_string())
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))
    }

    /// 默认工作区的默认 preset(设置通用区 Agent 预设行)
    pub fn default_preset(&self) -> String {
        let key = project_key(&self.default_workspace().display().to_string());
        self.settings
            .read()
            .workspace(&key)
            .preset
            .unwrap_or_else(|| "standard".into())
    }

    /// 设置默认 preset(落盘为默认工作区默认;新会话/冷装配沿用)
    pub fn set_default_preset(&self, preset: &str) -> Result<(), RpcError> {
        if !self.presets().iter().any(|p| p["id"] == preset) {
            return Err(RpcError::bad_request("未知 preset"));
        }
        let key = project_key(&self.default_workspace().display().to_string());
        self.settings
            .update(|s| s.workspaces.entry(key).or_default().preset = Some(preset.to_string()))
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))
    }

    /// 运行中 Enter 行为偏好(queue / steer)
    pub fn busy_enter(&self) -> String {
        self.settings.read().busy_enter.clone()
    }

    /// 设置运行中 Enter 行为偏好(落盘)
    pub fn set_busy_enter(&self, behavior: &str) -> Result<(), RpcError> {
        if !["queue", "steer"].contains(&behavior) {
            return Err(RpcError::bad_request("未知 Enter 行为"));
        }
        self.settings
            .update(|s| s.busy_enter = behavior.to_string())
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))
    }

    /// onboarding 完成态落盘
    pub fn set_onboarded(&self) -> Result<(), RpcError> {
        self.settings
            .update(|s| s.onboarded = true)
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))
    }

    /// provider 注册表(设置页 Models 区)
    pub fn providers(&self) -> Vec<ProviderEntry> {
        self.settings.read().providers
    }

    /// 新增/更新 provider(校验 id 形态、base_url 协议、方言、引用可解析;
    /// 变更后清该 provider 模型缓存,下次刷新重探)
    pub fn upsert_provider(&self, entry: ProviderEntry) -> Result<(), RpcError> {
        if entry.id.is_empty()
            || !entry
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            || entry.id.starts_with('-')
            || entry.id.ends_with('-')
        {
            return Err(RpcError::bad_request(
                "provider id 仅限小写字母/数字/连字符",
            ));
        }
        if !entry.base_url.starts_with("http://") && !entry.base_url.starts_with("https://") {
            return Err(RpcError::bad_request("base_url 须以 http(s):// 开头"));
        }
        if !["openai-chat", "anthropic", "openai-responses"].contains(&entry.dialect.as_str()) {
            return Err(RpcError::bad_request("未知方言"));
        }
        if let Some(r) = &entry.credential_ref
            && crate::credentials::parse_credential_ref(r).is_none()
        {
            return Err(RpcError::bad_request(
                "凭据引用须为 env:NAME / dotenv:NAME / keychain:SERVICE/ACCOUNT",
            ));
        }
        self.settings
            .update(
                |s| match s.providers.iter_mut().find(|p| p.id == entry.id) {
                    Some(slot) => *slot = entry.clone(),
                    None => s.providers.push(entry.clone()),
                },
            )
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))?;
        self.models_cache
            .lock()
            .expect("models_cache 锁中毒")
            .remove(&entry.id);
        Ok(())
    }

    /// 删除 provider(悬空的工作区引用由 provider() 内置回落兜底;
    /// 凭据记录不随之删除——钥匙串/`.env` 是用户资产)
    pub fn remove_provider(&self, id: &str) -> Result<(), RpcError> {
        self.settings
            .update(|s| s.providers.retain(|p| p.id != id))
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))?;
        self.models_cache
            .lock()
            .expect("models_cache 锁中毒")
            .remove(id);
        Ok(())
    }

    /// 工作区默认 provider 落盘(按工作区名定位;不存在 = 未知工作区)
    pub fn set_workspace_provider(&self, ws_name: &str, provider_id: &str) -> Result<(), RpcError> {
        let ws = self
            .workspace_of(ws_name)
            .ok_or_else(|| RpcError::bad_request("未知工作区"))?;
        let known = self
            .settings
            .read()
            .providers
            .iter()
            .any(|p| p.id == provider_id);
        if !known {
            return Err(RpcError::bad_request("未知 provider"));
        }
        let key = project_key(&ws.display().to_string());
        self.settings
            .update(|s| s.workspaces.entry(key).or_default().provider = Some(provider_id.into()))
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))
    }

    /// 凭据可解析态(设置页状态行;不回明文)
    pub fn credential_status(&self, provider_id: &str) -> bool {
        let provider = self.settings.read().provider(Some(provider_id));
        provider.id == provider_id
            && self
                .resolve_provider_key(&provider, &self.default_workspace())
                .is_some()
    }

    /// 录入凭据(写入钥匙串默认槽或工作区 `.env`,provider 引用同步
    /// 指向新记录;明文不落 settings.json)
    pub fn store_credential(
        &self,
        provider_id: &str,
        secret: &str,
        target: CredentialStore,
    ) -> Result<(), RpcError> {
        if secret.is_empty() {
            return Err(RpcError::bad_request("凭据不可为空"));
        }
        let provider = self.settings.read().provider(Some(provider_id));
        if provider.id != provider_id {
            return Err(RpcError::bad_request("未知 provider"));
        }
        let (write, refstr) = match target {
            CredentialStore::Keychain => {
                let kc = self.keychain.read().expect("keychain 锁中毒(宿主 bug)");
                let write = kc.set(KEYCHAIN_SERVICE, provider_id, secret);
                (write, format!("keychain:{KEYCHAIN_SERVICE}/{provider_id}"))
            }
            CredentialStore::Dotenv => {
                let name = crate::credentials::default_env_name(provider_id);
                let write = dsh_app::write_dotenv_key(&self.default_workspace(), &name, secret);
                (write, format!("dotenv:{name}"))
            }
        };
        write.map_err(|e| RpcError::internal(format!("凭据写入失败:{e}")))?;
        self.settings
            .update(|s| {
                if let Some(p) = s.providers.iter_mut().find(|p| p.id == provider_id) {
                    p.credential_ref = Some(refstr);
                }
            })
            .map_err(|e| RpcError::internal(format!("设置落盘失败:{e}")))
    }

    /// 重命名(标题覆盖;持久化 + 清单/历史投影可见)
    pub fn rename(&self, id: &str, title: &str) -> Result<(), RpcError> {
        let title = title.trim().chars().take(120).collect::<String>();
        if title.is_empty() {
            return Err(RpcError::bad_request("标题不可为空"));
        }
        let mut titles = self.titles.write().expect("titles 锁中毒");
        titles.insert(id.into(), title.clone());
        let text = serde_json::to_string_pretty(&*titles).unwrap_or_default();
        std::fs::write(self.default_workspace().join(TITLES_FILE), text)
            .map_err(|e| RpcError::internal(format!("标题持久化失败:{e}")))?;
        Ok(())
    }

    /// 会话标题(重命名 > 首条 user 投影)
    pub fn session_title(&self, id: &str) -> Option<String> {
        self.titles.read().expect("titles 锁中毒").get(id).cloned()
    }

    /// 4b:LLM 语义标题生成(源 `session-title-first-prompt-llm`)。
    ///
    /// 非会话面一次性调用(与 summary 同构,不经 invariant gate 的
    /// derive-and-compare):以首条 user/message 文本构造零工具请求 →
    /// 流式累积 → normalize → 写入 `titles` 映射(与手动 rename 同路径,
    /// rename 随时覆盖)。成功后以 `session/projection` 帧广播 title。
    ///
    /// 触发点:driver_loop 首 turn 的 user/message 落档后。in-flight 集合
    /// 去重(并发 turn 不重复生成;源 session-title 的 revision/supersede
    /// 以「只在首条消息且尚无标题时生成」等效简化)。
    ///
    /// registry 无独立 tokio runtime;调用方(driver_loop)在 spawn 的
    /// tokio 任务里 await。fake 模式用注入的 [`self.fake_title`](测试演示),
    /// 真实模式用 `build_raw_transport` 的独立一次请求。
    async fn generate_llm_title(
        self: &Arc<Self>,
        id: &str,
        first_text: &str,
    ) -> Result<(), String> {
        // in-flight 去重:同会话已在生成则跳过
        {
            let mut in_flight = self
                .title_gen_inflight
                .lock()
                .map_err(|_| "title_gen_inflight 锁中毒".to_string())?;
            if !in_flight.insert(id.to_string()) {
                return Ok(()); // 已在生成;首个请求负责产出
            }
        }
        // 生成为异步任务;失败/完成都清 in-flight
        let host = self.clone();
        let id_owned = id.to_string();
        let first_text = first_text.to_string();
        let result = async move {
            // 已有人工/手动标题则不覆盖(源:rename 钉住后不再自动生成)
            if host.session_title(&id_owned).is_some() {
                return Ok(());
            }
            let (ws_root, _) = host.resolve_session(&id_owned);
            let provider = host.provider_for(&ws_root);
            let cfg = host.ws_config(&ws_root);
            let resolved = Resolved::resolve(
                dsh_app::ResolveArgs {
                    workspace: Some(ws_root.display().to_string()),
                    session: Some(host.slot_path(&id_owned).display().to_string()),
                    model: Some(host.session_model(&id_owned)),
                    preset: Some(host.session_preset(&id_owned)),
                    reasoning_effort: host.session_effort(&id_owned),
                    base_url: cfg.base_url.clone().or(Some(provider.base_url.clone())),
                    dialect: cfg.dialect.clone().or(Some(provider.dialect.clone())),
                    ..Default::default()
                },
                &ws_root.join("dsh.toml"),
            )
            .map_err(|e| format!("title 装配失败:{e}"))?;

            let header = title_request_header(&host.session_model(&id_owned));
            let messages = frame_title_messages(&first_text);

            if host.fake {
                // fake 模式标题来自注入的 fake_title(测试演示);未注入 =
                // 不生成,保持确定性回退标题(fake 不建真实 HTTP)。
                let fake = host
                    .fake_title
                    .lock()
                    .map_err(|_| "fake_title 锁中毒".to_string())?
                    .clone();
                let title = fake.unwrap_or_default();
                if title.is_empty() {
                    return Ok(()); // 未注入:跳过,回退标题生效
                }
                host.apply_generated_title(&id_owned, &title)?;
                return Ok(());
            }
            let key = host
                .resolve_provider_key(&provider, &ws_root)
                .ok_or_else(|| "provider 凭据缺席".to_string())?;
            let mut transport = dsh_app::build_raw_transport(
                &resolved,
                &key,
                Some(Arc::new(host.attachments.clone())),
            )
            .map_err(|e| format!("title transport 构建失败:{e}"))?;
            let events = tokio::time::timeout(
                Duration::from_secs(60),
                transport.stream(&header, &messages),
            )
            .await
            .map_err(|_| "title 超时 60s".to_string())?
            .map_err(|e| e.to_string())?;
            let title = title_from_events(&events);

            if title.is_empty() {
                return Err("title 模型未产出文本".to_string());
            }
            host.apply_generated_title(&id_owned, &title)?;
            Ok(())
        }
        .await;

        // 清 in-flight
        if let Ok(mut set) = self.title_gen_inflight.lock() {
            set.remove(&id.to_string());
        }
        if let Err(e) = &result {
            eprintln!("[dsh-core] LLM 标题生成失败 {id}: {e}");
        }
        result
    }

    /// 4b:接受一处生成的标题(写入 titles 映射 + 持久化 + `session/projection` 广播)。
    fn apply_generated_title(&self, id: &str, title: &str) -> Result<(), String> {
        {
            let mut titles = self
                .titles
                .write()
                .map_err(|_| "titles 锁中毒".to_string())?;
            // 写入前再次确认无人工覆盖(并发 rename 竞态)
            titles
                .entry(id.to_string())
                .or_insert_with(|| title.to_string());
            let text = serde_json::to_string_pretty(&*titles).map_err(|e| e.to_string())?;
            std::fs::write(self.default_workspace().join(TITLES_FILE), text)
                .map_err(|e| format!("标题持久化失败:{e}"))?;
        }
        // 广播 title 投影(客户端 state.titles 增量更新;history/list 全量兜底)
        let _ = self.mux.send(frame(
            "session/projection",
            serde_json::to_value(ProjectionFrame {
                session_id: id.to_string(),
                key: "title".into(),
                value: serde_json::json!(title),
                seq: 0,
            })
            .unwrap_or(Value::Null),
        ));
        Ok(())
    }

    /// fake 模式注入脚本(每段对应一次 stream 调用;测试用)
    pub fn set_fake_script(&self, script: Vec<Vec<LlmEvent>>) {
        *self.fake_script.lock().expect("fake_script 锁中毒") = script;
    }

    /// fake 模式注入 LLM 标题输出(测试/演示用;None 默认 = fake 不生成)
    pub fn set_fake_title(&self, title: Option<String>) {
        *self.fake_title.lock().expect("fake_title 锁中毒") = title;
    }

    /// 模型源信息(翻译器 assistant source 块)
    pub fn provider_info(&self) -> ProviderInfo {
        ProviderInfo {
            provider: self.base.dialect.clone(),
            model: self.default_model(),
        }
    }

    /// mux 总线订阅端(桌面桥接层扇出)
    pub fn mux_subscribe(&self) -> broadcast::Receiver<ServerRequest> {
        self.mux.subscribe()
    }

    /// host 总线订阅端
    pub fn host_subscribe(&self) -> broadcast::Receiver<ServerRequest> {
        self.host.subscribe()
    }

    /// 默认工作区目录(清单第 0 位;describe cwd / 凭据 / 计费等工作区锚点)
    pub fn workspace(&self) -> std::path::PathBuf {
        self.default_workspace()
    }

    /// 会话文件目录:<root>/sessions/<projectKey(ws)>/<id>/session.jsonl
    fn slot_path(&self, id: &str) -> PathBuf {
        let (ws, stem) = self.resolve_session(id);
        self.sessions_root
            .join(project_key(&ws.display().to_string()))
            .join(&stem)
            .join("session.jsonl")
    }

    /// 会话日志路径(公开;测试/外部按 id 定位)
    pub fn session_log_path(&self, id: &str) -> PathBuf {
        self.slot_path(id)
    }

    fn get_slot(&self, id: &str) -> Option<Arc<SessionSlot>> {
        self.sessions
            .read()
            .expect("sessions 锁中毒")
            .get(id)
            .cloned()
    }

    fn register_slot(&self, id: &str, path: PathBuf) -> Arc<SessionSlot> {
        self.sessions
            .write()
            .expect("sessions 锁中毒")
            .entry(id.to_string())
            .or_insert_with(|| {
                Arc::new(SessionSlot {
                    id: id.to_string(),
                    path,
                    inner: std::sync::OnceLock::new(),
                    running: std::sync::atomic::AtomicBool::new(false),
                })
            })
            .clone()
    }

    // ── 会话数据面 ─────────────────────────────────────────────────

    /// session.list:各工作区顶层 *.jsonl 扫描 + 运行态。
    /// 非默认工作区会话 id = "<ws 名>/<stem>"
    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        let mut items: Vec<SessionSummary> = Vec::new();
        let workspaces = self.workspaces.read().expect("workspaces 锁中毒").clone();
        let default_name = self.default_workspace_name();
        for ws in &workspaces {
            let ws_name = ws
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let prefix = if ws_name == default_name {
                String::new()
            } else {
                format!("{ws_name}/")
            };
            let entries = match std::fs::read_dir(
                self.sessions_root
                    .join(project_key(&ws.display().to_string())),
            ) {
                Ok(e) => e,
                // 该项目目录不存在 = 无会话
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                // 会话 = 项目目录下的会话子目录(含 session.jsonl);.archive 除外
                let path = entry.path();
                if !path.is_dir() || path.file_name().is_none_or(|n| n == ".archive") {
                    continue;
                }
                let Some(stem) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let log = path.join("session.jsonl");
                if !log.exists() {
                    continue;
                }
                let id = format!("{prefix}{stem}");
                let (parent_id, origin) = read_session_header(&path);
                let running = self
                    .get_slot(&id)
                    .map(|s| s.running.load(std::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(false);
                let updated_at = log
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                let blank = !text.contains("\"turn/start\"");
                // title 投影:重命名 > 首条 user/message 内容(60 字)
                let title = self.session_title(&id).or_else(|| {
                    text.lines()
                        .find(|l| l.contains("\"user/message\""))
                        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                        .and_then(|v| v["data"]["content"].as_str().map(str::to_owned))
                });
                let projections = title.map(|t| Projections {
                    // 清单场景不统计 seq(0 = 未统计,客户端只读 values.title)
                    as_of_seq: 0,
                    values: serde_json::json!({ "title": t }),
                });
                items.push(SessionSummary {
                    session_id: id,
                    updated_at,
                    running,
                    blank,
                    projections,
                    parent_session_id: parent_id,
                    origin,
                    cwd: Some(ws.display().to_string()),
                    agent_preset: None,
                });
            }
        }
        items.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        items
    }

    /// session.create:新建空会话文件并广播 host 帧。
    /// workspace 给定时在该工作区建(id 带 "<ws 名>/" 前缀);preset 记覆盖
    pub fn create_session(
        &self,
        session_id: Option<String>,
        preset: Option<String>,
        workspace: Option<String>,
    ) -> String {
        let prefix = match workspace.as_deref() {
            Some(w) => match self.workspace_of(w) {
                Some(_) => format!("{w}/"),
                None => {
                    eprintln!("[dsh-core] session.create:未知工作区 {w:?},会话落默认工作区");
                    String::new()
                }
            },
            None => String::new(),
        };
        let id = session_id
            .map(|s| format!("{prefix}{s}"))
            .unwrap_or_else(|| format!("{prefix}s-{}", Uuid::new_v4().simple()));
        let path = self.slot_path(&id);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, "");
        }
        if let Some(p) = preset.as_deref()
            && self.presets().iter().any(|x| x["id"] == p)
        {
            self.preset_overrides
                .write()
                .expect("preset_overrides 锁中毒")
                .insert(id.clone(), p.to_owned());
        }
        self.register_slot(&id, path);
        let (ws_root, _) = self.resolve_session(&id);
        let _ = self.host.send(frame(
            "host/session-added",
            serde_json::to_value(HostSessionAdded {
                session_id: id.clone(),
                blank: true,
                parent_session_id: None,
                origin: None,
                cwd: Some(ws_root.display().to_string()),
                agent_preset: preset,
            })
            .unwrap_or(Value::Null),
        ));
        id
    }

    /// 建 subagent 子会话(带血缘 header:parent + origin:'subagent')。
    /// 与普通会话同 slot 布局,list_sessions 据此暴露血缘。
    pub fn create_subagent_session(&self, parent_id: &str) -> String {
        let prefix = match parent_id.split_once('/') {
            Some((ws, _)) if self.workspace_of(ws).is_some() => format!("{ws}/"),
            _ => String::new(),
        };
        let new_id = format!("{prefix}s-a{}", Uuid::new_v4().simple());
        let log = self.slot_path(&new_id);
        let slot = log
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| log.clone());
        let _ = std::fs::create_dir_all(&slot);
        if !log.exists() {
            let _ = std::fs::write(&log, "");
        }
        write_session_header(&slot, Some(parent_id), Some("subagent"));
        // 子代理权限 seed(delegation):继承父显式
        // sandbox override,审批钉 'never' 且都带 source:'delegation'。写成
        // 日志事件(与全库权限模型一致,子代理冷重放 fold 同源)。
        {
            let parent_sandbox = self.session_sandbox_mode(parent_id).to_string();
            if let Ok(backend) = dsh_host::JsonlBackend::open(&log) {
                let mut sandbox = EventEnvelope::new(
                    "sandbox/mode",
                    now_ms() as i64,
                    json!({
                        "mode": parent_sandbox,
                        "source": "delegation",
                    }),
                );
                sandbox.seq = 1;
                let mut approval = EventEnvelope::new(
                    "approval/policy",
                    now_ms() as i64,
                    json!({
                        "policy": "never",
                        "source": "delegation",
                    }),
                );
                approval.seq = 2;
                let _ = backend.append(&sandbox);
                let _ = backend.append(&approval);
            }
        }
        self.register_slot(&new_id, log.clone());
        let (ws_root, _) = self.resolve_session(&new_id);
        let _ = self.host.send(frame(
            "host/session-added",
            serde_json::to_value(HostSessionAdded {
                session_id: new_id.clone(),
                blank: true,
                parent_session_id: Some(parent_id.into()),
                origin: Some("subagent".into()),
                cwd: Some(ws_root.display().to_string()),
                agent_preset: None,
            })
            .unwrap_or(Value::Null),
        ));
        new_id
    }

    /// 分叉:复制整份日志为新会话(同 preset/模型/权限沿用)
    pub fn fork_session(&self, id: &str) -> Result<String, RpcError> {
        let src = self.slot_path(id);
        if !src.exists() {
            return Err(RpcError::session_not_found(id));
        }
        // 分叉留在同一工作区(沿用 id 前缀)
        let prefix = match id.split_once('/') {
            Some((ws, _)) if self.workspace_of(ws).is_some() => format!("{ws}/"),
            _ => String::new(),
        };
        let new_id = format!("{prefix}s-{}", Uuid::new_v4().simple());
        let dst = self.slot_path(&new_id);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| RpcError::internal(format!("分叉目录创建失败:{e}")))?;
        }
        std::fs::copy(&src, &dst).map_err(|e| RpcError::internal(format!("分叉复制失败:{e}")))?;
        // 血缘落档:日志尾追加 session/forked,seq = 父日志
        // 末 seq + 1(load_log/检索按行 decode,连续性由 append 侧保证)
        let parent_last_seq = dsh_app::load_log(&src.display().to_string())
            .ok()
            .and_then(|l| l.iter().last().map(|ev| ev.seq))
            .unwrap_or(0);
        let mut forked = dsh_session::EventEnvelope::new(
            "session/forked",
            now_ms() as i64,
            json!({ "parent": id }),
        );
        forked.seq = parent_last_seq + 1;
        use std::io::Write as _;
        let line = serde_json::to_string(&forked).unwrap_or_default();
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&dst) {
            let _ = writeln!(f, "{line}");
        }
        // 沿用模型/preset/推理等级覆盖(权限随 fork 复制的日志事件,
        // 无需再记 override——事件已在子会话日志里)
        for (overrides, cur) in [
            (&self.model_overrides, self.session_model(id)),
            (&self.preset_overrides, self.session_preset(id)),
            (
                &self.effort_overrides,
                self.session_effort(id).unwrap_or_default(),
            ),
        ] {
            overrides
                .write()
                .expect("覆盖表锁中毒")
                .insert(new_id.clone(), cur);
        }
        let (ws_root, _) = self.resolve_session(&new_id);
        let _ = self.host.send(frame(
            "host/session-added",
            serde_json::to_value(HostSessionAdded {
                session_id: new_id.clone(),
                blank: false,
                parent_session_id: Some(id.into()),
                origin: Some("fork".into()),
                cwd: Some(ws_root.display().to_string()),
                agent_preset: None,
            })
            .unwrap_or(Value::Null),
        ));
        Ok(new_id)
    }

    /// 归档:移入 .archive/(清单即不可见;恢复 = 移回)
    pub fn archive_session(&self, id: &str) -> Result<(), RpcError> {
        let slots = self.sessions.read().expect("sessions 锁中毒");
        if let Some(slot) = slots.get(id) {
            if slot.running.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(RpcError::bad_request("会话运行中,请先停止"));
            }
            drop(slots);
            self.sessions.write().expect("sessions 锁中毒").remove(id);
        }
        let src = self.slot_path(id);
        if !src.exists() {
            return Err(RpcError::session_not_found(id));
        }
        let (ws_root, stem) = self.resolve_session(id);
        let dir = self
            .sessions_root
            .join(project_key(&ws_root.display().to_string()))
            .join(".archive")
            .join(&stem);
        std::fs::create_dir_all(&dir)
            .map_err(|e| RpcError::internal(format!("归档目录创建失败:{e}")))?;
        std::fs::rename(&src, dir.join(src.file_name().unwrap_or_default()))
            .map_err(|e| RpcError::internal(format!("归档移动失败:{e}")))?;
        let _ = self
            .host
            .send(frame("host/session-removed", json!({ "sessionId": id })));
        Ok(())
    }

    /// 删除会话(永久移除日志文件;运行中拒绝。归档是移动到 .archive,
    /// 删除是不可恢复的清理)
    pub fn delete_session(&self, id: &str) -> Result<(), RpcError> {
        let slots = self.sessions.read().expect("sessions 锁中毒");
        if let Some(slot) = slots.get(id) {
            if slot.running.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(RpcError::bad_request("会话运行中,请先停止"));
            }
            drop(slots);
            self.sessions.write().expect("sessions 锁中毒").remove(id);
        }
        let src = self.slot_path(id);
        if !src.exists() {
            return Err(RpcError::session_not_found(id));
        }
        std::fs::remove_file(&src).map_err(|e| RpcError::internal(format!("会话删除失败:{e}")))?;
        // 会话目录随日志一并移除(目录内仅日志;附件/任务输出均外置)。
        // remove_dir 对非空目录无害失败,意外残留可见不静默吞
        if let Some(dir) = src.parent() {
            let _ = std::fs::remove_dir(dir);
        }
        let _ = self
            .host
            .send(frame("host/session-removed", json!({ "sessionId": id })));
        Ok(())
    }

    /// 附着(懒装配 + worker)。幂等:已附着直接返回。
    fn attach(self: &Arc<Self>, id: &str) -> Result<Arc<SessionSlot>, RpcError> {
        let path = self.slot_path(id);
        if !path.exists() {
            return Err(RpcError::session_not_found(id));
        }
        let slot = self.register_slot(id, path);
        if slot.inner.get().is_some() {
            return Ok(slot);
        }

        // 装配(同步;与 dsh chat 同配方;工作区/模型/preset/推理等级覆盖优先)
        let (ws_root, _) = self.resolve_session(id);
        let provider = self.provider_for(&ws_root);
        eprintln!(
            "[dsh-core] attach {} provider={} model={} effort={:?}",
            id,
            provider.id,
            self.session_model(id),
            self.session_effort(id)
        );
        let self_arc = Arc::clone(self);
        // base_url/dialect 合并:dsh.toml 显式 > provider 设置层
        let cfg = self.ws_config(&ws_root);
        let resolved = Resolved::resolve(
            dsh_app::ResolveArgs {
                workspace: Some(ws_root.display().to_string()),
                session: Some(slot.path.display().to_string()),
                model: Some(self.session_model(id)),
                preset: Some(self.session_preset(id)),
                reasoning_effort: self.session_effort(id),
                base_url: cfg.base_url.clone().or(Some(provider.base_url.clone())),
                dialect: cfg.dialect.clone().or(Some(provider.dialect.clone())),
                ..Default::default()
            },
            &ws_root.join("dsh.toml"),
        )
        .map_err(|e| RpcError::internal(format!("session 装配失败:{e}")))?;
        let parts = dsh_app::prompt_parts(&resolved, true);
        // AGENTS.md 基线不在此注入:per-step 指令重扫的 compose 首步发现
        // 日志无基线时自动整段载入,时序天然排在用户消息之后(pre-step
        // 语义);attach 时注入会插到用户消息之前,导致注入行
        // 被折叠组吞掉。
        let backend = dsh_app::open_backend(&resolved.session)
            .map_err(|e| RpcError::internal(format!("打开会话日志失败:{e}")))?;
        // 泵侧落盘句柄克隆(见 SlotInner.backend)
        let backend_shared = backend.clone();
        let log = dsh_app::load_log(&resolved.session)
            .map_err(|e| RpcError::internal(format!("会话日志重载失败:{e}")))?;
        let log = Arc::new(Mutex::new(log));
        // 冷加载修夏:此刻 pump/driver 未 spawn,单写者;OnceLock 保证每
        // 会话每进程只跑一次。悬挂 tool/call(重启遗留)在此收口成
        // isError result + turn/end,桌面投影/轨迹/模型三层自然一致
        repair_dangling_calls(&log, &backend);
        let cancel = CancelToken::new();
        let provider_info = self.provider_info();

        let (mut session, session_log) = if self.fake {
            let mut provider = FakeProvider::new();
            let drained: Vec<Vec<LlmEvent>> = self
                .fake_script
                .lock()
                .expect("fake_script 锁中毒")
                .drain(..)
                .collect();
            // 测试脚本未注入(fake_script 空)时按会话生成演示回声脚本——
            // 此前演示脚本一次性被首个附着会话整段消费,后续会话拿到空
            // 脚本 → 空响应 → UI 空助手行(实测)
            let script = if drained.is_empty() {
                demo_fake_segments()
            } else {
                drained
            };
            for segment in script {
                provider.then(segment);
            }
            let gate = InvariantGate::new(provider, log);
            let l = gate.log();
            (
                AnySession::Fake(Session::new(
                    parts,
                    gate,
                    l.clone(),
                    NoTools,
                    backend,
                    resolved.session.clone(),
                    cancel.clone(),
                )),
                l,
            )
        } else {
            // 真实模式凭据:四级链(显式注入 > 引用 > env/.env > 钥匙串);
            // 缺席即拒绝——provider key 只在装配点解析,不缓存(设置页
            // 录入后下次装配即生效)
            let key = self
                .resolve_provider_key(&provider, &ws_root)
                .ok_or_else(|| {
                    RpcError::internal(format!(
                        "provider {} 凭据缺席:请在设置中配置 API key",
                        provider.id
                    ))
                })?;
            let transport = dsh_app::build_raw_transport(
                &resolved,
                &key,
                Some(Arc::new(self.attachments.clone())),
            )
            .map_err(|e| RpcError::internal(format!("transport 构建失败:{e}")))?;
            let gate = InvariantGate::new(transport, log);
            let l = gate.log();
            // 动态权限源:工具每次执行 fold 共享日志取最后 sandbox/mode——
            // set_permission 落档即对下一次执行生效,无需重装配(源语义)
            let mode_log = Arc::clone(&l);
            let mode_source: dsh_tools::ModeSource = Arc::new(move || {
                crate::permission::sandbox_mode_of_name(
                    crate::permission::sandbox_mode_of(
                        &mode_log
                            .lock()
                            .expect("log 锁中毒")
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>(),
                    ),
                )
            });
            let tools = dsh_app::build_tools(
                &resolved,
                &key,
                &l,
                &cancel,
                false,
                crate::permission::sandbox_mode_of(
                    &l.lock()
                        .expect("log 锁中毒")
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
                Some(mode_source),
                Some(Arc::new(SessionQueryPortImpl(self_arc.clone()))),
                Some(Arc::new(AskQuestionPortImpl(self_arc.clone()))),
                Some(Arc::new(SessionFactoryImpl(self_arc.clone()))),
                Some(Arc::new(SettlementNoticeImpl(self_arc.clone()))),
                Some(id),
                Some(Arc::new(dsh_tools::subagent::SubagentBridge {
                    jobs: {
                        let host = self_arc.clone() as Arc<AppHost>;
                        Arc::new(move |pid: &str, reg: dsh_tools::subagent::SubagentRegistry| {
                            AppHost::bind_jobs(&host, pid, reg);
                        })
                    },
                    events: {
                        let host = self_arc.clone() as Arc<AppHost>;
                        Arc::new(move |sid: &str, ev: &dsh_session::EventEnvelope| {
                            host.relay_subagent_event(sid, ev);
                        })
                    },
                })),
            )
            .map_err(|e| RpcError::internal(format!("工具组装失败:{e}")))?;
            (
                AnySession::Real(Session::new(
                    parts,
                    gate,
                    l.clone(),
                    tools,
                    backend,
                    resolved.session.clone(),
                    cancel.clone(),
                )),
                l,
            )
        };

        // 队列重放 + 驱动通道 + 唤醒(泵/驱动双任务拆分)。
        // durable 队列:折叠 splice 事件重建未消费条目——重启不丢排队消息
        let (pending, steer_items) = {
            let l = session_log
                .lock()
                .map_err(|_| RpcError::internal("log 锁中毒"))?;
            replay_inbox(&l)
        };
        let steer_buf = Arc::new(Mutex::new(steer_items));
        session.set_steer_buf(Arc::clone(&steer_buf));
        let qs = Arc::new(Mutex::new(QueueState {
            pending,
            steer: steer_buf,
            running: false,
        }));
        let wake = Arc::new(Notify::new());
        let (queue_tx, queue_rx) = mpsc::unbounded_channel::<Job>();
        let (driver_cmd, driver_rx) = mpsc::unbounded_channel::<DriverCmd>();
        let inner = SlotInner {
            queue_tx,
            driver_cmd,
            wake,
            qs,
            cancel: cancel.clone(),
            log: session_log,
            backend: backend_shared,
        };
        if slot.inner.set(inner).is_err() {
            return Ok(slot); // 并发装配:另一线程赢了,worker 已由它起
        }

        // 附着广播:subscribed(此后事件实时下发)
        let last_seq = slot
            .inner
            .get()
            .map(|i| i.log.lock().map(|l| l.high_water()).unwrap_or(0))
            .unwrap_or(0);
        let _ = self.mux.send(frame(
            "session/subscribed",
            serde_json::to_value(SubscribedFrame {
                session_id: id.into(),
                last_seq,
            })
            .unwrap_or(Value::Null),
        ));
        // 队列基线(subscribed 之后同一流;空队列不发,客户端 reset 清旧代)
        let inner0 = slot.inner.get().expect("attach 后必有 inner");
        if !queue_items(inner0).is_empty() {
            let _ = self.mux.send(queue_frame(id, inner0));
        }

        tokio::spawn(pump_loop(Arc::downgrade(self), slot.clone(), queue_rx));
        tokio::spawn(driver_loop(
            Arc::downgrade(self),
            slot.clone(),
            session,
            driver_rx,
            provider_info,
        ));
        Ok(slot)
    }

    /// 全库检索:懒建索引 + 各会话增量同步(冷/热同一条重放
    /// 路径)→ 全文命中(按会话/seq 升序)。结果含定位锚(台账跳转用)。
    /// 多词 = OR(turso MATCH 语义);单字/前缀语法不命中(ngram 限制,
    /// 见 dsh-host search 模块头)
    pub async fn search_sessions(
        &self,
        query: &str,
        limit: usize,
        session: Option<&str>,
    ) -> Result<Value, RpcError> {
        use dsh_host::search::SearchIndex;
        let mut guard = self.search.lock().await;
        if guard.is_none() {
            let path = self.sessions_root.join("search.db");
            *guard = Some(
                SearchIndex::open(path.to_str().unwrap_or_default())
                    .await
                    .map_err(|e| RpcError::internal(format!("检索索引打开失败:{e}")))?,
            );
        }
        let index = guard.as_ref().expect("刚建");
        // 全量会话增量同步(list_sessions 已按工作区序产出)
        for summary in self.list_sessions() {
            let path = self.slot_path(&summary.session_id);
            index
                .sync_session(&summary.session_id, &path)
                .await
                .map_err(|e| RpcError::internal(format!("索引同步失败:{e}")))?;
        }
        let mut hits = index
            .search(query, limit)
            .await
            .map_err(|e| RpcError::internal(format!("检索失败:{e}")))?;
        if let Some(sid) = session {
            hits.retain(|h| h.session == sid);
        }
        let items: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({
                    "sessionId": h.session,
                    "seq": h.seq,
                    "kind": h.kind,
                    "content": h.content,
                })
            })
            .collect();
        Ok(json!({ "query": query, "hits": items }))
    }

    /// 最近 goal/state 快照(缺席 = 空表)
    fn goal_current(&self, id: &str) -> Vec<dsh_session::GoalItem> {
        self.session_log(id)
            .ok()
            .and_then(|events| {
                events
                    .iter()
                    .rev()
                    .find(|ev| ev.r#type == "goal/state")
                    .and_then(|ev| {
                        ev.data["goals"].as_array().map(|items| {
                            items
                                .iter()
                                .filter_map(|g| {
                                    Some(dsh_session::GoalItem {
                                        id: g["id"].as_u64()?,
                                        text: g["text"].as_str()?.to_string(),
                                        done: g["done"].as_bool().unwrap_or(false),
                                        paused: g["paused"].as_bool().unwrap_or(false),
                                    })
                                })
                                .collect::<Vec<_>>()
                        })
                    })
            })
            .unwrap_or_default()
    }

    /// 落一份新 goal/state 快照(附着 = 活日志+backend;冷 = 文件追加;
    /// 与 splice 同为日志第二写入路径,append 在日志互斥下安全)
    fn goal_commit(&self, id: &str, goals: &[dsh_session::GoalItem]) -> Result<(), RpcError> {
        let items: Vec<Value> = goals
            .iter()
            .map(|g| json!({ "id": g.id, "text": g.text, "done": g.done, "paused": g.paused }))
            .collect();
        let ev = dsh_session::EventEnvelope::new(
            "goal/state",
            now_ms() as i64,
            json!({ "goals": items }),
        );
        if let Some(slot) = self.get_slot(id)
            && let Some(inner) = slot.inner.get()
        {
            let committed = inner.log.lock().ok().and_then(|mut l| {
                let seq = l.append(ev).ok()?;
                l.get(seq).cloned()
            });
            if let Some(ev) = committed
                && let Err(e) = inner.backend.append(&ev)
            {
                return Err(RpcError::internal(format!("goal 落盘失败:{e}")));
            }
            // UI 经 goal_state 拉取(goal/state 不在 translate 客方词汇内,
            // 不做事件帧直播)
            return Ok(());
        }
        // 冷会话:文件追加(JsonlBackend append 模式)
        let path = self.slot_path(id);
        let backend = dsh_app::open_backend(&path.display().to_string())
            .map_err(|e| RpcError::internal(format!("goal 落盘失败:{e}")))?;
        // seq = 文件尾 seq + 1(load 全量只为尾序——冷路径频次低,可接受)
        let last_seq = dsh_app::load_log(&path.display().to_string())
            .map_err(|e| RpcError::internal(format!("goal 落盘失败:{e}")))?
            .iter()
            .last()
            .map(|e| e.seq)
            .unwrap_or(0);
        let mut ev = ev;
        ev.seq = last_seq + 1;
        backend
            .append(&ev)
            .map_err(|e| RpcError::internal(format!("goal 落盘失败:{e}")))?;
        Ok(())
    }

    /// goal 只读面(源 goal.active/revision 投影对应物)
    pub fn goal_state(&self, id: &str) -> Result<Value, RpcError> {
        if !self.slot_path(id).exists() {
            return Err(RpcError::session_not_found(id));
        }
        let goals = self.goal_current(id);
        Ok(json!({ "sessionId": id, "goals": goals }))
    }

    /// goal.create(六 RPC 之一)
    pub fn goal_create(&self, id: &str, text: &str) -> Result<Value, RpcError> {
        let mut goals = self.goal_current(id);
        if goals.is_empty() && !self.slot_path(id).exists() {
            return Err(RpcError::session_not_found(id));
        }
        let next_id = goals.iter().map(|g| g.id).max().unwrap_or(0) + 1;
        goals.push(dsh_session::GoalItem {
            id: next_id,
            text: text.to_string(),
            done: false,
            paused: false,
        });
        self.goal_commit(id, &goals)?;
        Ok(json!({ "id": next_id }))
    }

    /// goal.edit
    pub fn goal_edit(&self, id: &str, goal_id: u64, text: &str) -> Result<(), RpcError> {
        let mut goals = self.goal_current(id);
        let Some(g) = goals.iter_mut().find(|g| g.id == goal_id) else {
            return Err(RpcError::bad_request("目标不存在"));
        };
        g.text = text.to_string();
        self.goal_commit(id, &goals)
    }

    /// goal.complete
    pub fn goal_complete(&self, id: &str, goal_id: u64) -> Result<(), RpcError> {
        let mut goals = self.goal_current(id);
        let Some(g) = goals.iter_mut().find(|g| g.id == goal_id) else {
            return Err(RpcError::bad_request("目标不存在"));
        };
        g.done = true;
        self.goal_commit(id, &goals)
    }

    /// goal.clear(清空目标表)
    pub fn goal_clear(&self, id: &str) -> Result<(), RpcError> {
        self.goal_commit(id, &[])
    }

    /// goal.pause / goal.resume
    pub fn goal_set_paused(&self, id: &str, goal_id: u64, paused: bool) -> Result<(), RpcError> {
        let mut goals = self.goal_current(id);
        let Some(g) = goals.iter_mut().find(|g| g.id == goal_id) else {
            return Err(RpcError::bad_request("目标不存在"));
        };
        g.paused = paused;
        self.goal_commit(id, &goals)
    }

    /// 会话血缘(session_trace):父链(沿各日志 session/forked)
    /// + 直接后代(扫全库 forked)。返回 id 与父 id(标题由调用层补)
    pub fn session_trace(&self, id: &str) -> Result<Value, RpcError> {
        if !self.slot_path(id).exists() {
            return Err(RpcError::session_not_found(id));
        }
        let fork_parent = |sid: &str| -> Option<String> {
            let log = dsh_app::load_log(&self.slot_path(sid).display().to_string()).ok()?;
            log.iter()
                .rev()
                .find(|ev| ev.r#type == "session/forked")
                .and_then(|ev| ev.data["parent"].as_str().map(String::from))
        };
        // 祖先链(沿 forked 上溯)
        let mut ancestors = vec![];
        let mut cur = id.to_string();
        while let Some(parent) = fork_parent(&cur) {
            if ancestors.contains(&parent) || parent == id {
                break; // 环防御(理论不可达)
            }
            ancestors.push(parent.clone());
            cur = parent;
        }
        // 直接后代:扫全库各会话的 forked parent
        let children: Vec<String> = self
            .list_sessions()
            .into_iter()
            .filter(|s| s.session_id != id)
            .filter(|s| fork_parent(&s.session_id).as_deref() == Some(id))
            .map(|s| s.session_id)
            .collect();
        Ok(json!({ "session": id, "ancestors": ancestors, "children": children }))
    }

    /// 单事件全文 + 邻居摘要(session_event_read;冷/热同一
    /// session_log——附着态取活日志快照,冷会话文件重放)
    pub fn event_read(
        &self,
        id: &str,
        seq: u64,
        before: usize,
        after: usize,
    ) -> Result<Value, RpcError> {
        let events = self.session_log(id)?;
        let Some(ix) = events.iter().position(|ev| ev.seq == seq) else {
            return Err(RpcError::bad_request("事件不存在(seq 越界)"));
        };
        let ev = &events[ix];
        let summarize = |e: &dsh_session::EventEnvelope| json!({ "seq": e.seq, "type": e.r#type });
        let before_events: Vec<Value> = events[..ix]
            .iter()
            .rev()
            .take(before)
            .rev()
            .map(summarize)
            .collect();
        let after_events: Vec<Value> = events[ix + 1..].iter().take(after).map(summarize).collect();
        Ok(json!({
            "session": id,
            "event": { "seq": ev.seq, "type": ev.r#type, "data": ev.data },
            "before": before_events,
            "after": after_events,
        }))
    }

    /// 单事件溯源(session_event_trace):归因链(sourceEventSeqs)
    /// + 事件基础面
    pub fn event_trace(&self, id: &str, seq: u64) -> Result<Value, RpcError> {
        let events = self.session_log(id)?;
        let Some(ev) = events.iter().find(|ev| ev.seq == seq) else {
            return Err(RpcError::bad_request("事件不存在(seq 越界)"));
        };
        let sources: Vec<Value> = ev
            .source_event_seqs
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter_map(|sseq| {
                events
                    .iter()
                    .find(|e| e.seq == *sseq)
                    .map(|e| json!({ "seq": e.seq, "type": e.r#type }))
            })
            .collect();
        Ok(json!({
            "session": id,
            "seq": ev.seq,
            "type": ev.r#type,
            "sourceEventSeqs": ev.source_event_seqs.clone().unwrap_or_default(),
            "sources": sources,
        }))
    }

    /// session.history:附着(懒)→ 活日志快照 → 翻译 + 分页 + 投影
    pub async fn history(
        self: &Arc<Self>,
        session_id: &str,
        before_seq: Option<u64>,
        max_messages: usize,
    ) -> Result<HistoryValue, RpcError> {
        let slot = self.attach(session_id)?;
        let inner = slot.inner.get().expect("attach 后必有 inner");
        let log_snapshot: Vec<EventEnvelope> = {
            let l = inner
                .log
                .lock()
                .map_err(|_| RpcError::internal("log 锁中毒"))?;
            l.iter().cloned().collect()
        };
        let translated = translate_events(
            &ProviderInfo {
                provider: self.base.dialect.clone(),
                model: self.session_model(session_id),
            },
            &log_snapshot,
        );
        let Page {
            events, has_more, ..
        } = paginate(&translated, before_seq, max_messages.max(1));
        let high_water = log_snapshot.len() as u64;
        let title = self
            .session_title(session_id)
            .or_else(|| title_of(&log_snapshot));
        Ok(HistoryValue {
            events: events
                .into_iter()
                .map(|event| HistoryEntry { event, view: None })
                .collect(),
            has_more,
            // title 仅真实存在时下发(None = 无标题,客户端回落占位)
            projections: title.map(|t| Projections {
                as_of_seq: if high_water == 0 {
                    -1
                } else {
                    high_water as i64
                },
                values: json!({ "title": t }),
            }),
        })
    }

    /// session.export:导出原始会话日志(JSONL 文本;
    /// 事实流原样,不翻译不投影)
    pub fn export_session_log(&self, session_id: &str) -> Result<String, RpcError> {
        let path = self.slot_path(session_id);
        if !path.exists() {
            return Err(RpcError::session_not_found(session_id));
        }
        std::fs::read_to_string(&path)
            .map_err(|e| RpcError::internal(format!("会话日志读取失败:{e}")))
    }

    /// /export:会话 ZIP —— 根 session.jsonl + fork 后代按血缘序
    /// (`descendants/<安全id>/session.jsonl`)。返回 ZIP 字节,落盘路径由调用层定
    pub fn export_session_zip(
        &self,
        session_id: &str,
        include_descendants: bool,
    ) -> Result<Vec<u8>, RpcError> {
        use std::io::Write as _;
        let root_path = self.slot_path(session_id);
        if !root_path.exists() {
            return Err(RpcError::session_not_found(session_id));
        }
        let root = std::fs::read_to_string(&root_path)
            .map_err(|e| RpcError::internal(format!("会话日志读取失败:{e}")))?;
        let safe = |id: &str| {
            id.replace(
                |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_',
                "_",
            )
        };

        let mut artifacts: Vec<String> = vec![root.clone()];
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("session.jsonl", options)
            .map_err(|e| RpcError::internal(format!("zip 写入失败:{e}")))?;
        zip.write_all(root.as_bytes())
            .map_err(|e| RpcError::internal(format!("zip 写入失败:{e}")))?;

        if include_descendants {
            // fork 后代:递归扫全库 forked 血缘(与 session_trace 同源)
            let fork_parent = |sid: &str| -> Option<String> {
                let log = dsh_app::load_log(&self.slot_path(sid).display().to_string()).ok()?;
                log.iter()
                    .rev()
                    .find(|ev| ev.r#type == "session/forked")
                    .and_then(|ev| ev.data["parent"].as_str().map(String::from))
            };
            fn descendants_of(
                host: &AppHost,
                fork_parent: &dyn Fn(&str) -> Option<String>,
                id: &str,
                out: &mut Vec<String>,
            ) {
                for s in host.list_sessions() {
                    if fork_parent(&s.session_id).as_deref() == Some(id)
                        && !out.contains(&s.session_id)
                    {
                        out.push(s.session_id.clone());
                        descendants_of(host, fork_parent, &s.session_id, out);
                    }
                }
            }
            let mut queue = vec![];
            descendants_of(self, &fork_parent, session_id, &mut queue);
            for sid in queue {
                let text = std::fs::read_to_string(self.slot_path(&sid))
                    .map_err(|e| RpcError::internal(format!("后代日志读取失败:{e}")))?;
                zip.start_file(format!("descendants/{}/session.jsonl", safe(&sid)), options)
                    .map_err(|e| RpcError::internal(format!("zip 写入失败:{e}")))?;
                zip.write_all(text.as_bytes())
                    .map_err(|e| RpcError::internal(format!("zip 写入失败:{e}")))?;
                artifacts.push(text);
            }
        }

        // 媒体条目:内容寻址去重,路径 =
        // media/<attachmentId>.<ext>;对象缺席跳过(不阻断文本导出)
        let mut refs: Vec<dsh_session::attachments::ImageAttachmentRef> = Vec::new();
        for text in &artifacts {
            collect_image_refs(text, &mut refs);
        }
        for r in refs {
            let Ok(bytes) = self.attachments.read_image(&r.attachment_id) else {
                continue;
            };
            let path = format!("media/{}.{}", r.attachment_id, r.media_type.extension());
            zip.start_file(path, options)
                .map_err(|e| RpcError::internal(format!("zip 写入失败:{e}")))?;
            zip.write_all(&bytes)
                .map_err(|e| RpcError::internal(format!("zip 写入失败:{e}")))?;
        }

        let bytes = zip
            .finish()
            .map_err(|e| RpcError::internal(format!("zip 收尾失败:{e}")))?
            .into_inner();
        Ok(bytes)
    }

    /// 图片准入限制(客户端前置检查与错误文案共用)
    pub fn image_limits(&self) -> &dsh_session::attachments::ImageAttachmentLimits {
        self.attachments.limits()
    }

    /// 消息反馈 list(某 session 全部)
    pub fn message_feedback_list(&self, session_id: &str) -> Vec<MessageFeedbackItem> {
        self.feedback.list(session_id)
    }

    /// 消息反馈 put(新增/更新;if_version CAS)
    pub fn message_feedback_put(
        &self,
        session_id: &str,
        message_id: &str,
        rating: &str,
        note: Option<&str>,
        if_version: Option<&str>,
    ) -> Result<MessageFeedbackItem, RpcError> {
        self.feedback
            .put(session_id, message_id, rating, note, if_version)
            .map_err(|e| RpcError {
                code: "version-conflict".into(),
                message: e,
                details: Value::Null,
            })
    }

    /// 消息反馈 delete(if_version CAS)
    pub fn message_feedback_delete(
        &self,
        session_id: &str,
        message_id: &str,
        if_version: &str,
    ) -> Result<MessageFeedbackItem, RpcError> {
        self.feedback
            .delete(session_id, message_id, if_version)
            .map_err(|e| RpcError {
                code: "version-conflict".into(),
                message: e,
                details: Value::Null,
            })
    }

    /// 命令目录(host 注册表;桌面命令菜单拉取渲染)
    pub fn command_list(&self) -> Vec<CommandDescriptor> {
        builtin_commands()
    }

    /// ask_user_question 阻塞提问——落 pending(question/requested 帧),
    /// await 用户应答;resolve = 工具结果 JSON 文本(`{"answers":[...]}`),
    /// 取消/中断 → Err。模型在同一 tool-call 拿到结果。
    pub async fn ask_questions(
        self: &Arc<Self>,
        session_id: &str,
        questions: &[dsh_tools::QuestionItem],
    ) -> Result<String, String> {
        let rpc_id = Uuid::now_v7().to_string();
        let frame_questions: Vec<crate::proto::Question> = questions
            .iter()
            .map(|q| crate::proto::Question {
                id: q.id.clone(),
                question: q.question.clone(),
                header: q.header.clone(),
                detail: None,
                options: Some(
                    q.options
                        .iter()
                        .map(|o| crate::proto::QuestionOption {
                            label: o.label.clone(),
                            description: o.description.clone(),
                        })
                        .collect(),
                ),
                multi_select: Some(q.multi_select),
                intent: None,
            })
            .collect();
        let request = crate::proto::QuestionRequestedFrame {
            session_id: session_id.into(),
            questions: frame_questions,
        };
        let frame = ServerRequest {
            r#type: "server-request".into(),
            rpc_id: rpc_id.clone(),
            method: "question/requested".into(),
            payload: serde_json::to_value(&request).unwrap_or(Value::Null),
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending 锁中毒").insert(
            rpc_id.clone(),
            PendingQuestion {
                session_id: session_id.into(),
                approve_label: String::new(),
                tx: None,
                answer_tx: Some(tx),
                frame: frame.clone(),
            },
        );
        let _ = self.mux.send(frame);
        rx.await
            .map_err(|_| "ask_user_question 未被应答".to_string())?
    }

    /// 从 JSON 数组发起问答(帧形状与工具面一致;桌面端到端测试入口)。
    /// 数组元素 = {id, question, header?, options?[{label, description?}], multi_select?}
    pub async fn ask_questions_json(
        self: &Arc<Self>,
        session_id: &str,
        questions: Vec<serde_json::Value>,
    ) -> Result<String, String> {
        let items: Vec<dsh_tools::QuestionItem> = questions
            .into_iter()
            .map(|q| {
                Ok(dsh_tools::QuestionItem {
                    id: q["id"].as_str().ok_or("questions[].id 必填")?.to_string(),
                    question: q["question"]
                        .as_str()
                        .ok_or("questions[].question 必填")?
                        .to_string(),
                    header: q["header"].as_str().map(String::from),
                    options: q["options"]
                        .as_array()
                        .map(|os| {
                            os.iter()
                                .map(|o| {
                                    Ok(dsh_tools::QuestionOption {
                                        label: o["label"]
                                            .as_str()
                                            .ok_or("options[].label 必填")?
                                            .to_string(),
                                        description: o["description"].as_str().map(String::from),
                                    })
                                })
                                .collect::<Result<Vec<_>, String>>()
                        })
                        .transpose()?
                        .unwrap_or_default(),
                    multi_select: q["multi_select"].as_bool().unwrap_or(false),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        self.ask_questions(session_id, &items).await
    }

    /// 执行 slash 命令(host 直接执行,非发模型——
    /// `command.execute` 语义)。`line` 含 `/name args`;返回结果文本
    /// (菜单/flow 节点呈现)。未知命令或参数错 → Err。
    pub async fn execute_command(
        self: &Arc<Self>,
        session_id: &str,
        line: &str,
    ) -> Result<Value, RpcError> {
        let line = line.trim_start();
        let cmd = line.split_whitespace().next().unwrap_or_default();
        let name = cmd.trim_start_matches('/');
        let args = line[cmd.len()..].trim();
        match name {
            "plan" => {
                let mode = match args {
                    "on" => "plan",
                    "" => "plan",
                    "off" => "standard",
                    other => {
                        return Err(RpcError::bad_request(format!(
                            "/plan 参数必须为空/on/off(收到 {other})"
                        )));
                    }
                };
                let side = session_id.to_string();
                let mode_str = mode.to_string();
                self.set_mode(&side, &mode_str).await?;
                Ok(json!({ "accepted": true, "mode": mode_str }))
            }
            "export" => {
                use base64::Engine as _;
                let id = session_id.to_string();
                let bytes = self.export_session_zip(&id, false)?;
                let base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                Ok(json!({ "kind": "export", "data": base64 }))
            }
            "goal" => {
                if args.is_empty() {
                    return self.goal_state(session_id);
                }
                let (verb, _rest) = match args.split_once(char::is_whitespace) {
                    Some((v, r)) => (v, r),
                    None => (args, ""),
                };
                match verb {
                    "clear" => {
                        self.goal_clear(session_id)?;
                        Ok(json!({ "accepted": true }))
                    }
                    _ => self.goal_create(session_id, args),
                }
            }
            "model" => {
                if args.is_empty() {
                    return Ok(json!({ "kind": "model", "model": self.session_model(session_id) }));
                }
                self.set_model(session_id, args)?;
                Ok(json!({ "accepted": true, "model": args }))
            }
            other => Err(RpcError::bad_request(format!("未知命令 /{other}"))),
        }
    }

    /// session.attachment:读一张本会话日志引用的图片(授权 = 日志引用,
    /// 源 attachment RPC 语义)。返回 {attachment: ref, data: base64}。
    pub fn read_attachment(
        self: &Arc<Self>,
        session_id: &str,
        attachment_id: &str,
    ) -> Result<Value, RpcError> {
        let slot = self.attach(session_id)?;
        let inner = slot.inner.get().expect("attach 后必有 inner");
        let referenced = {
            let Ok(log) = inner.log.lock() else {
                return Err(RpcError::internal("log 锁中毒"));
            };
            log.iter().find_map(|ev| match ev.r#type.as_ref() {
                "user/message" => dsh_session::attachments::image_blocks(&ev.data["content"])
                    .iter()
                    .filter_map(dsh_session::attachments::ImageAttachmentRef::from_block)
                    .find(|r| r.attachment_id == attachment_id),
                "agent/inbox/spliced" => ev.data["inserted"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m["images"].as_array())
                    .flatten()
                    .filter_map(|r| {
                        serde_json::from_value::<dsh_session::attachments::ImageAttachmentRef>(
                            r.clone(),
                        )
                        .ok()
                    })
                    .find(|r| r.attachment_id == attachment_id),
                _ => None,
            })
        };
        let Some(r#ref) = referenced else {
            return Err(RpcError {
                code: "attachment-error".into(),
                message: "attachment is not referenced by this session".into(),
                details: json!({ "reason": "ATTACHMENT_NOT_REFERENCED" }),
            });
        };
        let bytes = self
            .attachments
            .read_image(attachment_id)
            .map_err(|e| RpcError {
                code: "attachment-error".into(),
                message: format!("附件读取失败:{e}"),
                details: json!({ "reason": e.reason() }),
            })?;
        use base64::Engine as _;
        Ok(json!({
            "attachment": r#ref,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        }))
    }

    /// session.prompt:文本块拼接入队或中途注入;
    /// mode = queue(追加为下一 turn)| steer(注入运行中 turn)。
    /// `/plan on|off` 斜杠快路直切模式(image → attachment-error)。
    pub async fn prompt(
        self: &Arc<Self>,
        session_id: &str,
        content: &[Value],
        mode: &str,
    ) -> Result<Value, RpcError> {
        self.prompt_inner(session_id, content, mode, Vec::new())
            .await
    }

    /// session.prompt + 注入上下文(4a 完整溯源模型):`contexts` 为待 commit 的
    /// user/message 注入载荷(含 source 染色,kind≠user);驱动并入注入数组交
    /// 引擎,在用户消息**之后**依序落档,作为独立注入行进入模型可见流
    /// (preStep 注入语义)。
    pub async fn prompt_with_contexts(
        self: &Arc<Self>,
        session_id: &str,
        content: &[Value],
        mode: &str,
        contexts: Vec<Value>,
    ) -> Result<Value, RpcError> {
        self.prompt_inner(session_id, content, mode, contexts).await
    }

    /// prompt 实现(两公开入口共用;contexts 为空时即纯 prompt)。
    async fn prompt_inner(
        self: &Arc<Self>,
        session_id: &str,
        content: &[Value],
        mode: &str,
        contexts: Vec<Value>,
    ) -> Result<Value, RpcError> {
        let prompt_mode = match mode {
            "queue" => PromptMode::Queue,
            "steer" => PromptMode::Steer,
            other => {
                return Err(RpcError::bad_request(format!(
                    "mode 取值必须为 queue 或 steer(收到 {other})"
                )));
            }
        };
        // attach 前确保模型清单就绪:探测在 describe 之外也会发生
        // (直发 prompt / 重启后首条消息,装配不能落到编造的默认模型名)
        self.ensure_models().await;
        let mut text = String::new();
        let mut images = Vec::new();
        for part in content {
            match part["type"].as_str() {
                Some("text") => text.push_str(part["text"].as_str().unwrap_or_default()),
                // 图片准入(源 durablePromptContent 序):canonical base64 解码
                // → 白名单/数量/大小/解码校验 → 内容寻址落盘 → 持久引用入队
                Some("image") => {
                    use base64::Engine as _;
                    let media_type = part["mediaType"].as_str().and_then(ImageMediaType::parse);
                    let Some(media_type) = media_type else {
                        return Err(RpcError {
                            code: "attachment-error".into(),
                            message: "仅支持 PNG、JPG、WebP、GIF 格式的图片".into(),
                            details: json!({ "reason": "UNSUPPORTED_IMAGE_TYPE" }),
                        });
                    };
                    let data = part["data"].as_str().and_then(|d| {
                        base64::engine::general_purpose::STANDARD
                            .decode(d)
                            .ok()
                            .filter(|bytes| !bytes.is_empty())
                    });
                    let Some(data) = data else {
                        return Err(RpcError {
                            code: "attachment-error".into(),
                            message: "图片编码无效".into(),
                            details: json!({ "reason": "INVALID_IMAGE_BASE64" }),
                        });
                    };
                    images.push(dsh_host::SaveImage {
                        data,
                        media_type,
                        name: part["name"].as_str().map(String::from),
                    });
                }
                _ => {}
            }
        }
        let refs = if images.is_empty() {
            Vec::new()
        } else {
            self.attachments
                .save_images(&images, 0, 0)
                .map_err(|e| RpcError {
                    code: "attachment-error".into(),
                    message: format!("图片附件被拒:{e}"),
                    details: json!({ "reason": e.reason() }),
                })?
        };
        let trimmed = text.trim();
        // 命令统一短路(command.execute 语义——命令不走模型面)。
        // 命令名 plan/goal/model/export;图片带命令 → 全批拒
        // (command.imagesUnsupported)。
        let cmd_name = trimmed
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_start_matches('/');
        if matches!(cmd_name, "plan" | "goal" | "model" | "export") {
            if !refs.is_empty() {
                return Err(RpcError {
                    code: "attachment-error".into(),
                    message: format!("/{} 不接受图片附件,请先移除图片", cmd_name),
                    details: json!({ "reason": "COMMAND_IMAGES_UNSUPPORTED" }),
                });
            }
            return self.execute_command(session_id, trimmed).await;
        }

        let slot = self.attach(session_id)?;
        let inner = slot.inner.get().expect("attach 后必有 inner");
        // 消息 id 宿主预分配(v7):队列帧 / 认领 splice / user/message 共用
        let id = Uuid::now_v7().to_string();
        inner
            .queue_tx
            .send(Job::Prompt {
                id,
                text,
                images: refs,
                mode: prompt_mode,
                contexts,
            })
            .map_err(|_| RpcError::internal("session worker 已退出"))?;
        Ok(json!({ "accepted": true }))
    }

    /// session.updateQueue:队列条目变更(edit / remove / steer)。
    /// 错误码:queue-item-not-found / steer-unavailable /
    /// queue-edit-non-text。会话未附着 = 无队列存在 → queue-item-not-found。
    pub async fn update_queue(
        self: &Arc<Self>,
        session_id: &str,
        item_id: &str,
        action: &Value,
    ) -> Result<Value, RpcError> {
        let action = parse_queue_action(action)?;
        let slot = self.get_slot(session_id).ok_or(RpcError {
            code: "queue-item-not-found".into(),
            message: "queued item is no longer pending".into(),
            details: Value::Null,
        })?;
        let inner = slot.inner.get().ok_or(RpcError {
            code: "queue-item-not-found".into(),
            message: "queued item is no longer pending".into(),
            details: Value::Null,
        })?;
        let (tx, rx) = oneshot::channel();
        inner
            .queue_tx
            .send(Job::UpdateQueue {
                item_id: item_id.into(),
                action,
                reply: tx,
            })
            .map_err(|_| RpcError::internal("session worker 已退出"))?;
        tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .map_err(|_| RpcError::internal("session worker 响应超时"))?
            .map_err(|_| RpcError::internal("session worker 已退出"))?
    }

    /// 模式切换(worker 执行;落档后广播 plan/mode 回声)
    pub async fn set_mode(
        self: &Arc<Self>,
        session_id: &str,
        mode: &str,
    ) -> Result<Value, RpcError> {
        if !matches!(mode, "standard" | "plan") {
            return Err(RpcError::bad_request("mode 取值必须为 standard 或 plan"));
        }
        let slot = self.attach(session_id)?;
        let inner = slot.inner.get().expect("attach 后必有 inner");
        inner
            .queue_tx
            .send(Job::SetMode(mode.into()))
            .map_err(|_| RpcError::internal("session worker 已退出"))?;
        Ok(json!({ "accepted": true }))
    }

    /// session.cancel:软取消令牌(turn 执行中即刻生效,不经队列)
    pub fn cancel_session(&self, session_id: &str) -> bool {
        match self.get_slot(session_id) {
            Some(slot) => {
                if let Some(inner) = slot.inner.get() {
                    inner.cancel.cancel();
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    /// 子代理结算通知入队:投 Job::Notice 到父会话泵通道(泵走
    /// steer 落档 + 唤醒——父空闲=驱动弹出作 turn 输入,忙碌=引擎 step
    /// 边界认领,followup/steer 双语义)。父会话未附着/已终结 →
    /// 静默丢弃(父不再存活不是错误,子会话自身即持久记录)。
    pub fn notify_subagent_settled(&self, parent_session: &str, text: String, source: Value) {
        let Some(slot) = self.get_slot(parent_session) else {
            return;
        };
        let Some(inner) = slot.inner.get() else {
            return;
        };
        let id = Uuid::now_v7().to_string();
        let _ = inner.queue_tx.send(Job::Notice { id, text, source });
    }

    /// 认领驻留子代理(防双挂):同 id 只认领一次
    fn claim_child(&self, session_id: &str) -> bool {
        self.live_children
            .lock()
            .expect("live_children 锁中毒")
            .insert(session_id.to_string())
    }

    /// 释放驻留认领(驻留退出时)
    fn release_child(&self, session_id: &str) {
        self.live_children
            .lock()
            .expect("live_children 锁中毒")
            .remove(session_id);
    }

    /// 接线子代理注册表为某父会话的 jobs 源:登记弱引用并挂观察
    /// 回调——注册表任何状态变化即向 mux 重广播该父的 jobs 快照。
    pub fn bind_jobs(self: &Arc<Self>, parent_id: &str, registry: dsh_tools::subagent::SubagentRegistry) {
        registry.set_on_change(Some(Arc::new({
            let host = Arc::downgrade(self);
            let parent = parent_id.to_string();
            move || {
                if let Some(host) = host.upgrade() {
                    host.broadcast_jobs(&parent);
                }
            }
        })));
        self.jobs_sources
            .lock()
            .expect("jobs_sources 锁中毒")
            .insert(parent_id.to_string(), registry.downgrade());
    }

    /// 宿主侧打断子代理(任务面板行直呼;与 interrupt_agent 工具
    /// 同一信号——stop 唤醒 → 当前 turn 令牌取消,LLM 流/bash 即时中断,
    /// 子代理保持可续话)。找到且在跑 = true;已结束/idle = false。
    pub fn interrupt_subagent(&self, child_session_id: &str) -> bool {
        let sources = self.jobs_sources.lock().expect("jobs_sources 锁中毒");
        for weak in sources.values() {
            let Some(registry) = weak.upgrade() else {
                continue;
            };
            let stop = {
                let records = registry.lock().expect("records 锁中毒");
                records
                    .iter()
                    .find(|rec| {
                        rec.session_id == child_session_id && rec.status == "running"
                    })
                    .and_then(|rec| rec.stop.clone())
            };
            // notify_one 许可语义:与等待注册的竞态窗口不丢信号
            if let Some(stop) = stop {
                stop.notify_one();
                return true;
            }
        }
        false
    }

    /// 子会话事件实时流转发:驻留子代理的每个引擎事件经翻译
    /// 后以 session/event 帧广播(与主会话同帧形态,桌面投影无差别)。
    /// 翻译器按子会话缓存(turn/step 计数器跨事件连续)。
    pub fn relay_subagent_event(&self, session_id: &str, ev: &dsh_session::EventEnvelope) {
        let event = {
            let mut guards = self
                .subagent_translators
                .lock()
                .expect("subagent_translators 锁中毒");
            let translator = guards
                .entry(session_id.to_string())
                .or_insert_with(|| crate::translate::Translator::new(self.provider_info()));
            translator.translate(ev)
        };
        if let Some(event) = event
            && let Some(f) = event_frame(session_id, event)
        {
            let _ = self.mux.send(f);
        }
    }

    /// 向 mux 广播某父会话的后台子代理清单(session/jobs 帧;
    /// onJobsChanged 语义)。registry 已释放 = 无任务可报,静默返回。
    pub fn broadcast_jobs(&self, parent_id: &str) {
        let registry = {
            let mut sources = self.jobs_sources.lock().expect("jobs_sources 锁中毒");
            match sources.get(parent_id).and_then(|w| w.upgrade()) {
                Some(r) => r,
                None => {
                    sources.remove(parent_id);
                    return;
                }
            }
        };
        let jobs: Vec<Value> = registry
            .background_records()
            .iter()
            .map(|r| {
                // 状态映射 SessionJob:running/completed/killed/failed;
                // RS 驻留 idle = 该轮完成仍可续话 → completed + detail 可继续
                let (status, detail) = match r.status.as_str() {
                    "running" => ("running", None),
                    "idle" => ("completed", Some("可继续")),
                    "cancelled" => ("killed", None),
                    "failed" => ("failed", None),
                    _ => ("completed", None),
                };
                let mut job = json!({
                    "id": r.session_id,
                    "kind": "subagent",
                    "label": r.task,
                    "status": status,
                    "startedAt": r.started_at,
                });
                if let Some(detail) = detail {
                    job["detail"] = json!(detail);
                }
                if let Some(ended) = r.ended_at {
                    job["finishedAt"] = json!(ended);
                }
                if !r.prompt.is_empty() {
                    job["prompt"] = json!(r.prompt);
                }
                job
            })
            .collect();
        let _ = self.mux.send(frame(
            "session/jobs",
            json!({ "sessionId": parent_id, "jobs": jobs }),
        ));
        // 子会话运行态同步(host 总线;驱动侧栏点/标题计时,与主会话同帧)
        for job in &jobs {
            self.host
                .send(frame(
                    "host/session-status",
                    serde_json::to_value(HostSessionStatus {
                        session_id: job["id"].as_str().unwrap_or_default().to_string(),
                        running: job["status"] == "running",
                    })
                    .unwrap_or(Value::Null),
                ))
                .ok();
        }
    }

    /// mux 流开基线:附着会话 subscribed + 队列快照 + 未决问题重放(同 rpcId)。
    /// 队列基线紧跟 subscribed(客户端在 subscribed 时清旧代,基线随后替换)
    pub fn mux_baseline(&self) -> Vec<ServerRequest> {
        let mut out = Vec::new();
        let mut ids: Vec<(String, u64)> = {
            let slots = self.sessions.read().expect("sessions 锁中毒");
            slots
                .iter()
                .filter_map(|(id, s)| {
                    let inner = s.inner.get()?;
                    let last_seq = inner.log.lock().ok()?.high_water();
                    Some((id.clone(), last_seq))
                })
                .collect()
        };
        ids.sort();
        for (id, last_seq) in ids {
            out.push(frame(
                "session/subscribed",
                serde_json::to_value(SubscribedFrame {
                    session_id: id.clone(),
                    last_seq,
                })
                .unwrap_or(Value::Null),
            ));
            // 队列基线:非空才发(空队列由 subscribed 清旧代表达)
            let slots = self.sessions.read().expect("sessions 锁中毒");
            if let Some(inner) = slots.get(&id).and_then(|s| s.inner.get())
                && !queue_items(inner).is_empty()
            {
                out.push(queue_frame(&id, inner));
            }
        }
        for pending in self.pending.lock().expect("pending 锁中毒").values() {
            out.push(pending.frame.clone());
        }
        out
    }

    /// 附着会话数(host.describe)
    pub fn attached_count(&self) -> usize {
        self.sessions
            .read()
            .expect("sessions 锁中毒")
            .values()
            .filter(|s| s.inner.get().is_some())
            .count()
    }

    /// workspace.list 响应值(默认 + 添加的工作区)
    pub fn workspace_view(&self) -> Value {
        let now = iso8601(now_ms());
        let attached: Vec<String> = self
            .sessions
            .read()
            .expect("sessions 锁中毒")
            .keys()
            .cloned()
            .collect();
        let items: Vec<Value> = self
            .workspace_names()
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                let ws = self.workspace_of(&name).expect("workspace_names 与表一致");
                let ids: Vec<String> = attached
                    .iter()
                    .filter(|id| match id.split_once('/') {
                        Some((prefix, _)) => prefix == name,
                        None => i == 0,
                    })
                    .cloned()
                    .collect();
                json!({
                    "workspaceId": name,
                    "path": ws.display().to_string(),
                    "title": self.workspace_title(&name),
                    "sessionIds": ids,
                    "createdAt": now,
                    "updatedAt": now,
                })
            })
            .collect();
        json!({
            "items": items,
            "archivedSessionIds": [],
        })
    }

    /// respond:按 rpcId 路由未决问题应答
    pub fn respond(&self, rpc_id: &str, result: &RpcResult) -> RespondReceipt {
        let mut pending = self.pending.lock().expect("pending 锁中毒");
        let Some(p) = pending.remove(rpc_id) else {
            return RespondReceipt {
                accepted: false,
                reason: Some("not-pending".into()),
            };
        };
        // ask_user_question 应答(answer_tx = 工具结果 JSON 文本)
        if let Some(ask_tx) = p.answer_tx {
            let text = match result {
                RpcResult::Ok(value) => {
                    // {sessionId, answer:{answers:[{id, selected[], custom?}]}}
                    match encode_answers(value) {
                        Ok(t) => t,
                        Err(e) => {
                            // 形状不符 = 拒答,不 resolve(保留 pending?已 remove。
                            // 这里超纲——直接以错误回给模型,不重复挂起)
                            return RespondReceipt {
                                accepted: false,
                                reason: Some(e),
                            };
                        }
                    }
                }
                RpcResult::Err(_) => {
                    return RespondReceipt {
                        accepted: false,
                        reason: Some("cancelled".into()),
                    };
                }
            };
            if ask_tx.send(Ok(text)).is_err() {
                return RespondReceipt {
                    accepted: false,
                    reason: Some("bad-response".into()),
                };
            }
            return RespondReceipt {
                accepted: true,
                reason: None,
            };
        }
        // plan 审批应答(tx = QuestionAnswer 三元)
        let Some(tx) = p.tx else {
            return RespondReceipt {
                accepted: false,
                reason: Some("no-sender".into()),
            };
        };
        let answer = match result {
            RpcResult::Ok(value) => {
                // {sessionId, answer:{answers:[{id, selected[], custom?}]}}
                let answers = value["answer"]["answers"].as_array();
                let approve = answers.is_some_and(|answers| {
                    answers.iter().any(|a| {
                        a["selected"].as_array().is_some_and(|labels| {
                            labels
                                .iter()
                                .any(|l| l.as_str() == Some(p.approve_label.as_str()))
                        })
                    })
                });
                if approve {
                    QuestionAnswer::Approve
                } else {
                    // 反馈取应答项 custom(「否,并告诉它应该如何做不同」
                    // 的行内输入;「跳过」不带 custom,空串视同无)
                    let feedback = answers.and_then(|answers| {
                        answers
                            .iter()
                            .find_map(|a| a["custom"].as_str().map(str::to_owned))
                    });
                    QuestionAnswer::Decline {
                        feedback: feedback.filter(|t| !t.trim().is_empty()),
                    }
                }
            }
            RpcResult::Err(_) => QuestionAnswer::Cancel,
        };
        if tx.send(answer).is_err() {
            return RespondReceipt {
                accepted: false,
                reason: Some("bad-response".into()),
            };
        }
        RespondReceipt {
            accepted: true,
            reason: None,
        }
    }
}

/// 计划审批问题生命周期:发问 → 等待应答 → 批准/拒绝 → resolved。
/// session 由 worker 持有并借用——所有写入同任务。
async fn plan_question(
    host: &Arc<AppHost>,
    session: &mut AnySession,
    session_id: &str,
    plan: String,
) -> PlanReviewOutcome {
    const APPROVE: &str = "批准";
    let rpc_id = Uuid::new_v4().to_string();
    let request = QuestionRequestedFrame {
        session_id: session_id.into(),
        questions: vec![Question {
            id: "plan".into(),
            question: "批准该计划?".into(),
            header: Some("计划待批准".into()),
            detail: Some(plan.clone()),
            options: Some(vec![
                QuestionOption {
                    label: APPROVE.into(),
                    description: None,
                },
                QuestionOption {
                    label: "拒绝".into(),
                    description: None,
                },
            ]),
            multi_select: Some(false),
            intent: Some(json!({ "kind": "plan-review", "approve": APPROVE })),
        }],
    };
    let request_frame = ServerRequest {
        r#type: "server-request".into(),
        rpc_id: rpc_id.clone(),
        method: "question/requested".into(),
        payload: serde_json::to_value(&request).unwrap_or(Value::Null),
    };
    let (tx, rx) = oneshot::channel();
    host.pending.lock().expect("pending 锁中毒").insert(
        rpc_id.clone(),
        PendingQuestion {
            session_id: session_id.into(),
            approve_label: APPROVE.into(),
            tx: Some(tx),
            answer_tx: None,
            frame: request_frame.clone(),
        },
    );
    let _ = host.mux.send(request_frame);

    let (outcome, frame_outcome): (PlanReviewOutcome, &str) = match rx.await {
        Ok(QuestionAnswer::Approve) => {
            let _ = session
                .session_event("plan/approved", json!({ "plan": plan }))
                .and_then(|_| session.session_event("session/mode", json!({ "mode": "standard" })));
            (PlanReviewOutcome::Approved, "approved")
        }
        Ok(QuestionAnswer::Decline { feedback }) => {
            let _ = session.session_event("session/mode", json!({ "mode": "standard" }));
            (PlanReviewOutcome::Declined { feedback }, "declined")
        }
        Ok(QuestionAnswer::Cancel) | Err(_) => {
            // 落取消事件:pending_plan() 判定依赖「submitted 后无
            // approved」——不落事件则取消后仍视为待批,驱动会重复发问
            let _ = session.session_event("plan/cancelled", json!({ "plan": plan }));
            (PlanReviewOutcome::Cancelled, "cancelled")
        }
    };
    host.pending.lock().expect("pending 锁中毒").remove(&rpc_id);
    let _ = host.mux.send(frame(
        "question/resolved",
        serde_json::to_value(QuestionResolvedFrame {
            session_id: session_id.into(),
            question_rpc_id: rpc_id,
            outcome: frame_outcome.into(),
        })
        .unwrap_or(Value::Null),
    ));
    outcome
}

/// 「去聊天里说」引导轮文案(取消审批;宿主注入,非用户原文)
const PLAN_CANCEL_GUIDE: &str = "用户取消了计划审批,想在聊天里继续讨论。请简短确认已停止执行该计划,并邀请用户直接说明要调整的方向;不要重复输出计划全文。";

/// 批准引导轮文案(修复「批准后没有后续动作」:批准此前
/// 只切回 standard 模式不启动任何 turn;现经引导轮驱动模型即刻开工)
const PLAN_APPROVED_GUIDE: &str = "用户已批准该计划。请开始执行:按计划逐步实施,边做边简要汇报进展。";

/// 拒绝带反馈的引导轮文案:修改意见直送模型(审批是 turn 后流程,
/// 模型无工具结果可读——队列注入是唯一送达通道)
fn plan_decline_guide(feedback: &str) -> String {
    format!(
        "用户拒绝了该计划,并说明了希望的不同做法:{feedback}\n请根据该反馈调整方案继续;不要重复输出计划全文。"
    )
}

/// 审批终局引导轮注入(队列 pending 追加,泵 splice 落档 + 唤醒驱动):
/// 取消与「拒绝+反馈」共用同一机制
fn inject_plan_guide_turn(session_id: &str, inner: &SlotInner, host: &Arc<AppHost>, guide: &str) {
    let gid = format!("s-guide-{}", Uuid::now_v7().simple());
    let mut q = inner.qs.lock().expect("qs 锁中毒");
    let at = q.pending.len();
    q.pending.push_back(PendingItem {
        id: gid.clone(),
        text: guide.into(),
        images: vec![],
        contexts: vec![],
    });
    drop(q);
    let rec = SpliceRecord {
        target: "next-turn",
        start: at,
        removed: 0,
        inserted: vec![SpliceItem {
            id: gid,
            text: guide.into(),
            images: vec![],
            source: None,
        }],
    };
    commit_splice(&inner.log, &inner.backend, &rec);
    inner.wake.notify_one();
    let _ = host.mux.send(queue_frame(session_id, inner));
}

/// 队列帧 items:待运行(queued)+ 中途输入(steering)条目。
/// 权威瞬态快照(session/queue 帧;消息形状 = 客方 Message)
fn queue_items(inner: &SlotInner) -> Vec<Value> {
    let qs = inner.qs.lock().expect("qs 锁中毒");
    let mut items = Vec::new();
    // 客方 Message 形状恒为块数组(图前文后;纯文本包 text 块)
    let frame_content = |text: &str, images: &[dsh_session::attachments::ImageAttachmentRef]| {
        if images.is_empty() {
            json!([ { "type": "text", "text": text } ])
        } else {
            dsh_session::attachments::message_content(text, images)
        }
    };
    for p in &qs.pending {
        items.push(json!({
            "id": p.id,
            "placement": "queued",
            "message": {
                "id": p.id,
                "role": "user",
                "content": frame_content(&p.text, &p.images),
                "source": { "kind": "user" },
            },
        }));
    }
    for s in qs.steer.lock().expect("steer 锁中毒").iter() {
        // 通知条目不下发队列帧:结算通知在认领前短暂驻留 steer,
        // 队列条/steer 伪行不闪现通知,呈现只剩消息落档后的通知卡一路
        if s.source.as_ref().and_then(|v| v["kind"].as_str()) == Some("subagent-settled") {
            continue;
        }
        items.push(json!({
            "id": s.id,
            "placement": "steering",
            "message": {
                "id": s.id,
                "role": "user",
                "content": frame_content(&s.text, &s.images),
                "source": { "kind": "user" },
            },
        }));
    }
    items
}

/// session/queue 帧(每次变更广播;基线在 subscribed 之后同一流下发)
fn queue_frame(session_id: &str, inner: &SlotInner) -> ServerRequest {
    frame(
        "session/queue",
        json!({ "sessionId": session_id, "items": queue_items(inner) }),
    )
}

/// splice 插入条目(id + 文本 + 可选图片引用)
#[derive(Debug, Clone)]
struct SpliceItem {
    /// 持久消息 id
    id: String,
    /// 文本内容
    text: String,
    /// 图片附件(持久引用)
    images: Vec<dsh_session::attachments::ImageAttachmentRef>,
    /// 来源染色(None = 用户输入。结算通知等宿主注入经 splice 持久化,
    /// 冷重放须恢复,否则重开后的通知失去染色)
    source: Option<Value>,
}

/// 队列 splice 落档记录(泵侧变更 → `agent/inbox/spliced` 载荷)
#[derive(Debug)]
struct SpliceRecord {
    /// 目标清单(next-turn / next-step)
    target: &'static str,
    /// 插入位置(重放语义:先删 removed 条,再于 start 起逐条插入)
    start: usize,
    /// 删除条数
    removed: usize,
    /// 插入条目
    inserted: Vec<SpliceItem>,
}

impl SpliceRecord {
    /// 事件载荷(inserted 条目形状 {id, content[, images]} 与
    /// translate / replay_inbox 的读取面一致)
    fn payload(&self) -> Value {
        json!({
            "target": self.target,
            "start": self.start,
            "removedCount": self.removed,
            "inserted": self
                .inserted
                .iter()
                .map(|i| {
                    dsh_session::attachments::splice_item(
                        i.id.clone(),
                        i.text.clone(),
                        &i.images,
                        i.source.as_ref(),
                    )
                })
                .collect::<Vec<_>>(),
        })
    }
}

/// 冷加载修夏:扫描「有 tool/call 无配对
/// result」的悬挂调用,补写 isError 的合成 tool/result;turn 仍开着则
/// 再补 turn/end(cancelled 收口)。应用重启会杀掉挂起问答/工具所在的
/// turn 任务,日志只剩裸 tool/call——不修夏则桌面投影永挂 Running、
/// 轨迹台账无结果列、问题卡丢失。
/// 合成 result 显式 `success:false`(translate 缺省当 true)、`call` 填
/// tool/call 的 seq(桌面配对只认 seq)、`id` 按前一 assistant/message
/// tool_calls 的 name+顺序取(容忍空串,engine 同规则)。占位文案复用
/// 派生层常量,模型视角与旧派生占位一致。
/// 幂等:修夏后调用均有 result、turn 均闭合,再跑零追加。
/// 只在 attach 冷路径调用(此刻 pump/driver 未 spawn,单写者)。
fn repair_dangling_calls(log: &Arc<Mutex<EventLog>>, backend: &dsh_host::JsonlBackend) {
    let committed = log.lock().ok().and_then(|mut l| {
        // 单临界区内「扫描 + 追加」:与引擎 commit / splice / goal_commit 互斥
        let mut turn_open = false;
        // 最近 assistant/message 的 (已用, provider id, name)
        let mut last_tools: Vec<(bool, String, String)> = Vec::new();
        let mut open_calls: Vec<(u64, String)> = Vec::new(); // (tool/call seq, provider id)
        for ev in l.iter() {
            match ev.r#type.as_str() {
                "turn/start" => turn_open = true,
                "turn/end" | "turn/error" => turn_open = false,
                "assistant/message" => {
                    last_tools = ev
                        .data
                        .get("tool_calls")
                        .and_then(|v| v.as_array())
                        .map(|calls| {
                            calls
                                .iter()
                                .map(|c| {
                                    (
                                        false,
                                        c.get("id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        c.get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                }
                "tool/call" => {
                    let name =
                        ev.data.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let id = last_tools
                        .iter_mut()
                        .find(|(used, _, n)| !*used && n == name)
                        .map(|(used, id, _)| {
                            *used = true;
                            id.clone()
                        })
                        .unwrap_or_default();
                    open_calls.push((ev.seq, id));
                }
                "tool/result" => {
                    if let Some(call) = ev.data.get("call").and_then(|v| v.as_u64()) {
                        open_calls.retain(|(seq, _)| *seq != call);
                    }
                }
                _ => {}
            }
        }
        if open_calls.is_empty() && !turn_open {
            return None;
        }
        let mut out = Vec::new();
        for (seq, id) in open_calls {
            let ev = EventEnvelope::new(
                "tool/result",
                now_ms() as i64,
                serde_json::json!({
                    "call": seq,
                    "id": id,
                    "output": dsh_session::events::DANGLING_TOOL_PLACEHOLDER,
                    "success": false,
                }),
            );
            if let Ok(s) = l.append(ev)
                && let Some(ev) = l.get(s).cloned()
            {
                out.push(ev);
            }
        }
        if turn_open {
            let ev = EventEnvelope::new(
                "turn/end",
                now_ms() as i64,
                serde_json::json!({ "cancelled": "session-restart" }),
            );
            if let Ok(s) = l.append(ev)
                && let Some(ev) = l.get(s).cloned()
            {
                out.push(ev);
            }
        }
        Some(out)
    });
    if let Some(committed) = committed {
        for ev in &committed {
            if let Err(e) = backend.append(ev) {
                eprintln!("[dsh-core] 修夏落盘失败: {e}");
            }
        }
    }
}

/// 提交 splice 事件:日志(锁内定 seq)+ 落盘。持久化失败仅 stderr——
/// 内存队列态仍是权威,失败降级为瞬态队列(旧行为),不阻断提交
fn commit_splice(log: &Arc<Mutex<EventLog>>, backend: &dsh_host::JsonlBackend, rec: &SpliceRecord) {
    let ev = EventEnvelope::new("agent/inbox/spliced", now_ms() as i64, rec.payload());
    let committed = log.lock().ok().and_then(|mut l| {
        let seq = l.append(ev).ok()?;
        l.get(seq).cloned()
    });
    if let Some(ev) = committed
        && let Err(e) = backend.append(&ev)
    {
        eprintln!("[dsh-core] splice 落盘失败: {e}");
    }
}

/// 冷重放:折叠 `agent/inbox/spliced` 重建未消费队列(入队/编辑携带
/// inserted,认领/移除只携带 removedCount)。start/removedCount 越界
/// 防御性截断——日志损坏不留 panic 面
fn replay_inbox(log: &EventLog) -> (VecDeque<PendingItem>, VecDeque<SteerInput>) {
    let mut next_turn: Vec<SpliceItem> = Vec::new();
    let mut next_step: Vec<SpliceItem> = Vec::new();
    for ev in log.iter() {
        if ev.r#type != "agent/inbox/spliced" {
            continue;
        }
        let list = match ev.data["target"].as_str() {
            Some("next-step") => &mut next_step,
            Some("next-turn") => &mut next_turn,
            _ => continue,
        };
        let start = ev.data["start"].as_u64().unwrap_or(0) as usize;
        let removed = ev.data["removedCount"].as_u64().unwrap_or(0) as usize;
        let start = start.min(list.len());
        let end = (start + removed).min(list.len());
        list.drain(start..end);
        for (i, m) in ev.data["inserted"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let item = splice_item_from_event(m);
            let at = (start + i).min(list.len());
            list.insert(at, item);
        }
    }
    (
        next_turn
            .into_iter()
            .map(|i| PendingItem {
                id: i.id,
                text: i.text,
                images: i.images,
                contexts: Vec::new(),
            })
            .collect(),
        next_step
            .into_iter()
            .map(|i| SteerInput {
                id: i.id,
                text: i.text,
                images: i.images,
                source: i.source,
            })
            .collect(),
    )
}

/// 从一段会话日志文本收集图片引用(user/message 块数组 + splice
/// inserted.images;按 attachment_id 去重——导出 media 条目共用)
fn collect_image_refs(text: &str, out: &mut Vec<dsh_session::attachments::ImageAttachmentRef>) {
    let mut seen_ids: Vec<String> = out.iter().map(|r| r.attachment_id.clone()).collect();
    for line in text.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let mut candidates: Vec<Value> =
            dsh_session::attachments::image_blocks(&ev["data"]["content"]);
        if let Some(inserted) = ev["data"]["inserted"].as_array() {
            for m in inserted {
                if let Some(images) = m["images"].as_array() {
                    candidates.extend(images.iter().cloned());
                }
            }
        }
        // splice images 是裸 ref(非 image 块);双形状兼容
        for c in candidates {
            let block = if c["type"].as_str() == Some("image") {
                c
            } else {
                json!({ "type": "image", "attachment": c })
            };
            if let Some(r) = dsh_session::attachments::ImageAttachmentRef::from_block(&block)
                && !seen_ids.contains(&r.attachment_id)
            {
                seen_ids.push(r.attachment_id.clone());
                out.push(r);
            }
        }
    }
}

/// splice inserted 条目解析({id, content[, images]};旧日志无 images 键 =
/// 纯文本条目,形状不判错)
fn splice_item_from_event(m: &Value) -> SpliceItem {
    SpliceItem {
        id: m["id"].as_str().unwrap_or_default().to_string(),
        text: m["content"].as_str().unwrap_or_default().to_string(),
        images: m["images"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| serde_json::from_value(r.clone()).ok())
                    .collect()
            })
            .unwrap_or_default(),
        source: m.get("source").filter(|s| s.is_object()).cloned(),
    }
}

/// 队列条目变更(QueueAction:edit / remove / steer)。
/// 错误码:queue-item-not-found / steer-unavailable。
/// 返回变更应答 + splice 落档记录(调用方提交)
fn apply_queue_action(
    qs: &Mutex<QueueState>,
    wake: &Notify,
    item_id: &str,
    action: QueueAction,
) -> Result<(Value, Vec<SpliceRecord>), RpcError> {
    let not_found = || RpcError {
        code: "queue-item-not-found".into(),
        message: "queued item is no longer pending".into(),
        details: Value::Null,
    };
    let mut q = qs.lock().expect("qs 锁中毒");
    match action {
        QueueAction::Edit(text) => {
            let Some(item) = q.pending.iter_mut().find(|i| i.id == item_id) else {
                return Err(not_found());
            };
            // QUEUE_EDIT_NON_TEXT:含图条目不接受编辑(文本替换会丢引用)
            if !item.images.is_empty() {
                return Err(RpcError {
                    code: "attachment-error".into(),
                    message: "queue edits accept text content only".into(),
                    details: json!({ "reason": "QUEUE_EDIT_NON_TEXT" }),
                });
            }
            item.text = text.clone();
            let pos = q.pending.iter().position(|i| i.id == item_id).unwrap_or(0);
            Ok((
                json!({ "accepted": true }),
                vec![SpliceRecord {
                    target: "next-turn",
                    start: pos,
                    removed: 1,
                    inserted: vec![SpliceItem {
                        id: item_id.to_string(),
                        text,
                        images: Vec::new(),
                        source: None,
                    }],
                }],
            ))
        }
        QueueAction::Remove => {
            let Some(pos) = q.pending.iter().position(|i| i.id == item_id) else {
                return Err(not_found());
            };
            q.pending.remove(pos);
            Ok((
                json!({ "accepted": true }),
                vec![SpliceRecord {
                    target: "next-turn",
                    start: pos,
                    removed: 1,
                    inserted: vec![],
                }],
            ))
        }
        QueueAction::Steer => {
            let Some(pos) = q.pending.iter().position(|i| i.id == item_id) else {
                return Err(not_found());
            };
            if !q.running {
                // steer 仅对 next-turn 且 agent running 可用
                return Err(RpcError {
                    code: "steer-unavailable".into(),
                    message: "current turn no longer accepts steering".into(),
                    details: Value::Null,
                });
            }
            let item = q.pending.remove(pos).expect("刚定位的条目必在");
            let at = {
                let mut steer = q.steer.lock().expect("steer 锁中毒");
                let at = steer.len();
                steer.push_back(SteerInput {
                    id: item.id.clone(),
                    text: item.text.clone(),
                    images: item.images.clone(),
                    source: None,
                });
                at
            };
            wake.notify_one();
            Ok((
                json!({ "accepted": true }),
                vec![
                    // 转移 = 两条 splice:next-turn 移除 + next-step 追加
                    SpliceRecord {
                        target: "next-turn",
                        start: pos,
                        removed: 1,
                        inserted: vec![],
                    },
                    SpliceRecord {
                        target: "next-step",
                        start: at,
                        removed: 0,
                        inserted: vec![SpliceItem {
                            id: item.id,
                            text: item.text,
                            images: item.images,
                            source: None,
                        }],
                    },
                ],
            ))
        }
    }
}

/// 解析队列变更动作 payload(QueueAction 形状:
/// { kind: "edit", content: [text blocks] } | { kind: "remove" | "steer" }。
/// edit 仅接受 text 块,否则 queue-edit-non-text)
fn parse_queue_action(action: &Value) -> Result<QueueAction, RpcError> {
    match action["kind"].as_str() {
        Some("edit") => {
            let mut text = String::new();
            for block in action["content"].as_array().into_iter().flatten() {
                match block["type"].as_str() {
                    Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
                    _ => {
                        return Err(RpcError {
                            code: "queue-edit-non-text".into(),
                            message: "queue edits accept text content only".into(),
                            details: json!({ "reason": "QUEUE_EDIT_NON_TEXT" }),
                        });
                    }
                }
            }
            Ok(QueueAction::Edit(text))
        }
        Some("remove") => Ok(QueueAction::Remove),
        Some("steer") => Ok(QueueAction::Steer),
        _ => Err(RpcError::bad_request(
            "action.kind 必须为 edit / remove / steer",
        )),
    }
}

/// 检索/血缘端口实现(SessionQueryTool 的宿主面)
struct SessionQueryPortImpl(Arc<AppHost>);

impl dsh_tools::session_query::SessionQueryPort for SessionQueryPortImpl {
    fn search(
        &self,
        query: &str,
        limit: usize,
        session: Option<&str>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>> {
        let host = Arc::clone(&self.0);
        let query = query.to_string();
        let session = session.map(String::from);
        Box::pin(async move {
            host.search_sessions(&query, limit, session.as_deref())
                .await
                .map_err(|e| e.message)
        })
    }

    fn trace_session(
        &self,
        session: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>> {
        let host = Arc::clone(&self.0);
        let session = session.to_string();
        Box::pin(async move { host.session_trace(&session).map_err(|e| e.message) })
    }

    fn trace_event(
        &self,
        session: &str,
        seq: u64,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>> {
        let host = Arc::clone(&self.0);
        let session = session.to_string();
        Box::pin(async move { host.event_trace(&session, seq).map_err(|e| e.message) })
    }

    fn read_event(
        &self,
        session: &str,
        seq: u64,
        before: usize,
        after: usize,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>> {
        let host = Arc::clone(&self.0);
        let session = session.to_string();
        Box::pin(async move {
            host.event_read(&session, seq, before, after)
                .map_err(|e| e.message)
        })
    }
}

/// 问答端口实现(AskQuestionTool 的宿主面——阻塞 await 用户应答)
/// 子会话工厂实现(SubagentTool 的宿主面——创建带血缘的独立会话)
struct SessionFactoryImpl(Arc<AppHost>);

impl dsh_tools::subagent::SessionFactory for SessionFactoryImpl {
    fn create_subagent(&self, parent: &str) -> dsh_tools::subagent::SubagentSessionHandle {
        let id = self.0.create_subagent_session(parent);
        dsh_tools::subagent::SubagentSessionHandle {
            session_path: self.0.slot_path(&id),
            session_id: id,
        }
    }

    /// 重启恢复扫描:父会话下带 descriptor 标记的驻留子代理。
    /// 末事件非 settled = 中断(先冷修夏悬挂调用再重挂);已 settled =
    /// 静默重挂(ready 可续话态)。认领失败的(已有驻留)不返回。
    fn resumable_children(
        &self,
        parent: &str,
    ) -> Vec<(dsh_tools::subagent::SubagentSessionHandle, bool, String, String)> {
        let mut out = Vec::new();
        for s in self.0.list_sessions() {
            if s.origin.as_deref() != Some("subagent")
                || s.parent_session_id.as_deref() != Some(parent)
            {
                continue;
            }
            let path = self.0.slot_path(&s.session_id);
            let events = dsh_host::persistence::jsonl::load_jsonl(&path).unwrap_or_default();
            let Some(desc) = events.iter().find(|e| e.r#type == "subagent/descriptor") else {
                continue;
            };
            let label = desc.data["label"].as_str().unwrap_or("").to_string();
            let prompt = desc.data["prompt"].as_str().unwrap_or("").to_string();
            let interrupted = events
                .last()
                .map(|e| e.r#type.as_str() != "subagent/settled")
                .unwrap_or(true);
            if interrupted {
                // 中断者先冷修夏(与主会话同规则:悬挂 tool/call 补合成 result、
                // 开 turn 补收口)并持久化,重挂引擎才见一致历史
                let log = Arc::new(Mutex::new(EventLog::new()));
                {
                    let mut l = log.lock().expect("log 锁中毒");
                    for ev in &events {
                        let _ = l.append(ev.clone());
                    }
                }
                // open(追加)而非 create(截断):修夏是补事件,不得清史
                let Ok(backend) = dsh_host::JsonlBackend::open(&path) else {
                    continue;
                };
                repair_dangling_calls(&log, &backend);
            }
            if !self.0.claim_child(&s.session_id) {
                continue;
            }
            out.push((
                dsh_tools::subagent::SubagentSessionHandle {
                    session_id: s.session_id.clone(),
                    session_path: path,
                },
                interrupted,
                label,
                prompt,
            ));
        }
        out
    }

    fn release_child(&self, session_id: &str) {
        self.0.release_child(session_id);
    }
}

/// 子代理结算通知 port 真身:把通知作为 Job::Notice 投入父会话
/// 泵通道。父会话不存在/已终结 → 静默丢弃(父不再存活不是错误,
/// 子会话自身即持久记录)。
struct SettlementNoticeImpl(Arc<AppHost>);

impl dsh_tools::subagent::SettlementNotificationPort for SettlementNoticeImpl {
    fn notify(
        &self,
        parent_session: &str,
        text: String,
        source: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let host = Arc::clone(&self.0);
        let parent = parent_session.to_string();
        Box::pin(async move {
            host.notify_subagent_settled(&parent, text, source);
        })
    }
}

struct AskQuestionPortImpl(Arc<AppHost>);
impl dsh_tools::AskQuestionPort for AskQuestionPortImpl {
    fn ask(
        &self,
        session_id: &str,
        questions: &[dsh_tools::QuestionItem],
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>> {
        let host = Arc::clone(&self.0);
        let session_id = session_id.to_string();
        let questions = questions.to_vec();
        Box::pin(async move { host.ask_questions(&session_id, &questions).await })
    }
}

/// 问题应答编码(questionResponsePayloadSchema 校验 + 工具结果形状)。
/// 校验:sessionId 匹配非空、answers 为数组、每项 id/selected 在场、单选 selected≤1、
/// selected 与 custom 互斥;返回工具结果文本 JSON `{"answers":[{id,selected[],custom?}]}`。
fn encode_answers(value: &Value) -> Result<String, String> {
    let answers = value["answer"]["answers"]
        .as_array()
        .ok_or_else(|| "缺少 answer.answers 数组".to_string())?;
    let mut items = Vec::new();
    for a in answers {
        let id = a["id"]
            .as_str()
            .ok_or_else(|| "answer 需要 id".to_string())?;
        let selected = a["selected"]
            .as_array()
            .ok_or_else(|| "answer 需要 selected 数组".to_string())?;
        if selected.len() > 1 {
            return Err("单选 selected 不能超过 1 项".to_string());
        }
        let selected: Vec<&str> = selected.iter().filter_map(|s| s.as_str()).collect();
        let custom = a.get("custom").and_then(|c| c.as_str());
        if selected.iter().any(|s| s.is_empty()) && custom.is_some() {
            return Err("selected 与 custom 互斥".to_string());
        }
        let mut item = serde_json::Map::new();
        item.insert("id".into(), Value::String(id.to_string()));
        item.insert(
            "selected".into(),
            Value::Array(
                selected
                    .iter()
                    .map(|s| Value::String(s.to_string()))
                    .collect(),
            ),
        );
        if let Some(c) = custom.filter(|c| !c.is_empty()) {
            item.insert("custom".into(), Value::String(c.to_string()));
        }
        items.push(Value::Object(item));
    }
    Ok(json!({ "answers": items }).to_string())
}

/// 泵任务:消费 job 流(提交 / 队列变更 / 模式切换转发)。
/// 队列态变更 + 帧广播 + splice 落档(durable inbox);turn 事件的
/// 唯一写入者仍是驱动任务,泵只写 inbox splice(两条写入路径经
/// 日志互斥与 backend 写者锁串行,seq 序 = 真实顺序)
async fn pump_loop(
    host: Weak<AppHost>,
    slot: Arc<SessionSlot>,
    mut queue_rx: mpsc::UnboundedReceiver<Job>,
) {
    let Some(host) = host.upgrade() else { return };
    let Some(inner) = slot.inner.get() else {
        return;
    };
    let session_id = slot.id.clone();
    let qs = Arc::clone(&inner.qs);
    let wake = Arc::clone(&inner.wake);
    let driver_cmd = inner.driver_cmd.clone();
    let mux = host.mux.clone();
    while let Some(job) = queue_rx.recv().await {
        match job {
            Job::Prompt {
                id,
                text,
                images,
                mode,
                contexts,
            } => {
                let rec = {
                    let mut q = qs.lock().expect("qs 锁中毒");
                    match mode {
                        PromptMode::Queue => {
                            let at = q.pending.len();
                            q.pending.push_back(PendingItem {
                                id: id.clone(),
                                text: text.clone(),
                                images: images.clone(),
                                contexts: contexts.clone(),
                            });
                            SpliceRecord {
                                target: "next-turn",
                                start: at,
                                removed: 0,
                                inserted: vec![SpliceItem {
                                id,
                                text,
                                images,
                                source: None,
                            }],
                            }
                        }
                        PromptMode::Steer => {
                            let mut steer = q.steer.lock().expect("steer 锁中毒");
                            let at = steer.len();
                            steer.push_back(SteerInput {
                                id: id.clone(),
                                text: text.clone(),
                                images: images.clone(),
                                source: None,
                            });
                            drop(steer);
                            SpliceRecord {
                                target: "next-step",
                                start: at,
                                removed: 0,
                                inserted: vec![SpliceItem {
                                id,
                                text,
                                images,
                                source: None,
                            }],
                            }
                        }
                    }
                };
                commit_splice(&inner.log, &inner.backend, &rec);
                wake.notify_one();
                let _ = mux.send(queue_frame(&session_id, inner));
            }
            Job::UpdateQueue {
                item_id,
                action,
                reply,
            } => {
                let result = apply_queue_action(&qs, &wake, &item_id, action);
                if let Ok((_, recs)) = &result {
                    for rec in recs {
                        commit_splice(&inner.log, &inner.backend, rec);
                    }
                }
                let _ = mux.send(queue_frame(&session_id, inner));
                let _ = reply.send(result.map(|(v, _)| v));
            }
            Job::Notice { id, text, source } => {
                // 通知走 steer 通道(父空闲=驱动弹出作 turn 输入;忙碌=引擎
                // step 边界认领)。队列条不闪现通知:queue_frame 按 source 过滤。
                let rec = {
                    let q = qs.lock().expect("qs 锁中毒");
                    let mut steer = q.steer.lock().expect("steer 锁中毒");
                    let at = steer.len();
                    steer.push_back(SteerInput {
                        id: id.clone(),
                        text: text.clone(),
                        images: Vec::new(),
                        source: Some(source.clone()),
                    });
                    SpliceRecord {
                        target: "next-step",
                        start: at,
                        removed: 0,
                        inserted: vec![SpliceItem {
                            id,
                            text,
                            images: Vec::new(),
                            source: Some(source),
                        }],
                    }
                };
                commit_splice(&inner.log, &inner.backend, &rec);
                wake.notify_one();
                let _ = mux.send(queue_frame(&session_id, inner));
            }
            Job::SetMode(mode) => {
                let _ = driver_cmd.send(DriverCmd::SetMode(mode));
            }
            Job::SetPermission(mode) => {
                let _ = driver_cmd.send(DriverCmd::SetPermission(mode));
            }
            Job::SetApproval(policy) => {
                let _ = driver_cmd.send(DriverCmd::SetApproval(policy));
            }
        }
    }
}

/// 驱动命令处理(与 turn 串行;session/mode 落档 + 广播尾事件)
fn handle_driver_cmd(
    session: &mut AnySession,
    provider_info: &ProviderInfo,
    inner: &SlotInner,
    session_id: &str,
    mux: &broadcast::Sender<ServerRequest>,
    cmd: DriverCmd,
) {
    match cmd {
        DriverCmd::SetMode(mode) => {
            if let Ok(seq) = session.session_event("session/mode", json!({ "mode": mode })) {
                broadcast_event(provider_info, &inner.log, session_id, mux, Some(seq));
            }
        }
        DriverCmd::SetPermission(mode) => {
            if let Ok(seq) = session.session_event("sandbox/mode", json!({ "mode": mode })) {
                broadcast_event(provider_info, &inner.log, session_id, mux, Some(seq));
            }
        }
        DriverCmd::SetApproval(policy) => {
            if let Ok(seq) = session.session_event("approval/policy", json!({ "policy": policy }))
            {
                broadcast_event(provider_info, &inner.log, session_id, mux, Some(seq));
            }
        }
    }
}

/// 驱动任务:拥有会话,串行驱动 turn。启动时检查待审计划(重启恢复);
/// turn 间隙处理驱动命令(SetMode)与待审计划发问(队列暂停 = 审批优先)。
/// 认领顺序:next-step(steer)先于 next-turn(队列)。
async fn driver_loop(
    host: Weak<AppHost>,
    slot: Arc<SessionSlot>,
    mut session: AnySession,
    mut driver_rx: mpsc::UnboundedReceiver<DriverCmd>,
    provider_info: ProviderInfo,
) {
    let session_id = slot.id.clone();
    let Some(host0) = host.upgrade() else { return };
    let Some(inner) = slot.inner.get() else {
        return;
    };
    let qs = Arc::clone(&inner.qs);
    let wake = Arc::clone(&inner.wake);

    // 重启/重连恢复:日志已有待审计划 → 先发问
    if let Some(plan) = session.pending_plan() {
        let outcome = plan_question(&host0, &mut session, &session_id, plan).await;
        broadcast_event(&provider_info, &inner.log, &session_id, &host0.mux, None);
        match outcome {
            PlanReviewOutcome::Cancelled => {
                // 与 turn 后一致:取消后注入引导轮(见下「待审计划」注释)
                inject_plan_guide_turn(&session_id, inner, &host0, PLAN_CANCEL_GUIDE);
            }
            PlanReviewOutcome::Declined {
                feedback: Some(fb),
            } => {
                inject_plan_guide_turn(&session_id, inner, &host0, &plan_decline_guide(&fb));
            }
            PlanReviewOutcome::Approved => {
                // 冷恢复同样补「开工」引导轮(与 turn 后批准路径一致)
                inject_plan_guide_turn(&session_id, inner, &host0, PLAN_APPROVED_GUIDE);
            }
            _ => {}
        }
    }

    // workspace 指令(AGENTS.md)逐步重扫(agent-instructions pre-step
    // compose):宿主持有 InstructionRuntimeState(版本缓存/排除集),每步读
    // 日志重建可见表 → stat 探测基线链+触碰后代目录 → 差分渲染增量载荷。
    // 触碰路径由引擎从 file_read/file_edit 参数收集并传入。
    {
        let log = Arc::clone(&inner.log);
        let ws_root = host0.resolve_session(&session_id).0;
        let home = dsh_host::default_dshrs_root();
        let state = std::sync::Mutex::new(dsh_host::InstructionRuntimeState::new());
        if let Ok(mut st) = state.lock() {
            let snapshot = log
                .lock()
                .ok()
                .map(|l| l.iter().cloned().collect::<Vec<_>>());
            if let Some(snap) = snapshot {
                st.restore_from_log(&snap);
            }
        }
        session.set_instructions_provider(Box::new(move |touches| {
            let Ok(mut st) = state.lock() else {
                return None;
            };
            let events = log.lock().ok()?.iter().cloned().collect::<Vec<_>>();
            st.compose(&events, &home, &ws_root, touches)
        }));
    }

    // runtime-context 快照注入改由引擎内 RuntimeContextProjection 每步判断生成
    // (镜像源 preStep → project):宿主只注入渲染回调(读日志 fold 权限策略 +
    // workspace_root),投影在引擎侧跨步去重、折叠遮蔽时失效。冷附着重开会话恢复 retained。
    {
        let log = Arc::clone(&inner.log);
        let ws_root = host0.resolve_session(&session_id).0;
        session.set_context_provider(Box::new(move || {
            let events = log.lock().ok()?.iter().cloned().collect::<Vec<_>>();
            let sections =
                crate::permission::snapshot_sections(&events, &ws_root.display().to_string());
            if sections.is_empty() {
                return None;
            }
            let current = crate::permission::join_snapshot_text(&sections);
            let ctx_sections = sections
                .into_iter()
                .map(|s| dsh_agent_loop::ContextSection {
                    name: s.name,
                    text: s.text,
                })
                .collect();
            Some((current, ctx_sections))
        }));
        session.restore_projection();
    }

    loop {
        // 命令间隙处理(与 turn 串行;turn 内到达的命令延后到下一轮)
        while let Ok(cmd) = driver_rx.try_recv() {
            handle_driver_cmd(
                &mut session,
                &provider_info,
                inner,
                &session_id,
                &host0.mux,
                cmd,
            );
        }
        // 认领:steer 优先(锁序:qs → steer 各自获取,不交叉持有)
        let claimed = {
            let steer = qs.lock().expect("qs 锁中毒").steer.clone();
            let mut steer_guard = steer.lock().expect("steer 锁中毒");
            if let Some(s) = steer_guard.pop_front() {
                Some((s.id, s.text, s.images, "next-step", Vec::new(), s.source))
            } else {
                drop(steer_guard);
                let mut q = qs.lock().expect("qs 锁中毒");
                q.pending
                    .pop_front()
                    .map(|p| (p.id, p.text, p.images, "next-turn", p.contexts, None))
            }
        };
        let Some((id, text, images, target, contexts, input_source)) = claimed else {
            // 无活干:等待唤醒(提交必 notify;唤醒后重查,覆盖 notify 合流)
            tokio::select! {
                _ = wake.notified() => {}
                cmd = driver_rx.recv() => {
                    let Some(cmd) = cmd else { return }; // 泵已死(槽被移除)
                    handle_driver_cmd(
                        &mut session, &provider_info, inner, &session_id, &host0.mux, cmd,
                    );
                }
            }
            continue;
        };
        // running 标记 + 状态广播(认领 splice 前——steer-unavailable 判断窗口)
        slot.running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        qs.lock().expect("qs 锁中毒").running = true;
        let _ = host0.host.send(frame(
            "host/session-status",
            serde_json::to_value(HostSessionStatus {
                session_id: session_id.clone(),
                running: true,
            })
            .unwrap_or(Value::Null),
        ));
        // 认领 splice(出队记录;与 turn 事件同 seq 序,先于 user/message)
        if let Ok(seq) = session.session_event(
            "agent/inbox/spliced",
            json!({ "target": target, "start": 0, "removedCount": 1, "inserted": [] }),
        ) {
            broadcast_event(&provider_info, &inner.log, &session_id, &host0.mux, Some(seq));
        }
        // 队列帧(认领后待运行条目已出队)
        let _ = host0.mux.send(queue_frame(&session_id, inner));
        // 翻译器计数预热(扫描既有日志)
        let mut translator = Translator::new(provider_info.clone());
        // 统计聚合预热(同一 fold 逻辑;turn 内增量 apply)
        let mut stats_agg = stats::StatsAgg::default();
        if let Ok(l) = inner.log.lock() {
            for ev in l.iter() {
                translator.translate(ev);
                stats_agg.apply(&ev.r#type, &ev.data);
            }
        }
        let mux = host0.mux.clone();
        let sid = session_id.clone();
        // 4a 完整溯源模型:本 turn 的注入上下文队列(contexts,transient,不
        // 入 durable 队列)+ runtime-context 快照(变更才产出)。二者合并为
        // 单一注入载荷数组,作为 turn_with 的 contexts 入参交给引擎——由
        // 引擎在真实用户消息**之后**依序落档为 user/message + source.kind
        // (preStep `[...claimed, context]` 事件序)。
        let mut injection: Vec<Value> = Vec::new();
        for ctx in &contexts {
            // 消息 id 宿主预分配(v7;载荷可已带 id,缺省补上,与 user/message 同规)
            let mut ctx = ctx.clone();
            if ctx.get("id").and_then(|v| v.as_str()).is_none() {
                ctx["id"] = json!(Uuid::now_v7().to_string());
            }
            injection.push(ctx);
        }
        // 新会话 pin 默认权限预设(pinInitialPermission):日志尚无
        // sandbox/mode + approval/policy + permission/preset 事件时,按默认
        // 权限预设补全三事实,使 default_permission_preset 生效且 fold/回放
        // 确定性。子代理/已 pin 会话沿用继承事件,不再补。此属于权限事件
        // (非注入),不作为 contexts 交给引擎。
        {
            let has_permission_events = inner
                .log
                .lock()
                .ok()
                .map(|l| {
                    l.iter()
                        .any(|e| e.r#type == "sandbox/mode" || e.r#type == "permission/preset")
                })
                .unwrap_or(false);
            if !has_permission_events {
                let preset = host0.default_permission();
                if let Some(spec) = crate::permission::PRESETS.iter().find(|p| p.name == preset) {
                    let preset_name = spec.name.to_string();
                    for (ty, val) in [
                        ("permission/preset", json!({ "preset": preset_name })),
                        ("sandbox/mode", json!({ "mode": spec.sandbox })),
                        ("approval/policy", json!({ "policy": spec.approval })),
                    ] {
                        let _ = session.session_event(ty, val);
                    }
                    broadcast_event(&provider_info, &inner.log, &session_id, &host0.mux, None);
                }
            }
        }
        // 来源染色:通知等带 source 的认领条目交引擎,随真实用户
        // 消息落档;普通条目显式清 None(每次认领必设,无跨 turn 残留)
        session.set_input_source(input_source);
        // runtime-context 快照注入已移入引擎内 RuntimeContextProjection(每步判断生成,
        // 见 driver_loop 开头 set_context_provider)。此处 injection 仅承载用户主动注入
        // (如 @session 引用),不再预组装 runtime 快照。
        let outcome = session
            .turn_with(
                &text,
                Some(&id),
                &images,
                &injection,
                &mut |ev: &EventEnvelope| {
                    if let Some(event) = translator.translate(ev)
                        && let Some(f) = event_frame(&sid, event)
                    {
                        let _ = mux.send(f);
                    }
                    // 统计相关事件落档即推 session/stats(事件驱动,替代
                    // 客户端轮询;chunk 等高频非统计事件不推)。构成三段
                    // 为字符启发式线性扫,随推随算保鲜
                    if stats::StatsAgg::is_stats_event(&ev.r#type) {
                        stats_agg.apply(&ev.r#type, &ev.data);
                        let breakdown = inner
                            .log
                            .lock()
                            .ok()
                            .map(|l| {
                                let b = crate::context::context_breakdown(l.iter());
                                stats::Breakdown {
                                    system_tokens: b.system_tokens,
                                    tools_tokens: b.tools_tokens,
                                    message_tokens: b.message_tokens,
                                }
                            })
                            .unwrap_or_default();
                        let _ = mux.send(frame(
                            "session/stats",
                            json!({ "sessionId": sid, "stats": stats_agg.to_json(breakdown) }),
                        ));
                    }
                },
            )
            .await;
        slot.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        qs.lock().expect("qs 锁中毒").running = false;
        let _ = host0.host.send(frame(
            "host/session-status",
            serde_json::to_value(HostSessionStatus {
                session_id: session_id.clone(),
                running: false,
            })
            .unwrap_or(Value::Null),
        ));
        // turn/error 已由引擎落档并经 sink 直播;此处进程日志留痕
        // (此前 `let _` 丢弃——turn 失败完全静默)
        if let Err(e) = &outcome {
            eprintln!("[dsh-core] turn 失败 {session_id}: {e}");
        }

        // 4b:LLM 语义标题生成(源 session-title-first-prompt-llm)。
        // 仅对顶层会话(无 parent)且尚无标题时,在首 turn 的 user/message
        // 落档后触发一次;离线异步,不阻塞 turn(源 fire-and-forget +
        // 60s 超时)。重复 turn 不再生成(in-flight + titles 已存双保险),
        // 手动 rename 后 session_title() 已 Some → 跳过。
        {
            // slot.path 是 session.jsonl 文件;血缘 header 在会话**目录**下,
            // 故从路径的父目录读(与 list_sessions/log 的 read_session_header(&path) 一致)
            let session_dir = slot.path.parent().map(|p| p.to_path_buf());
            let (parent, _) = session_dir
                .as_deref()
                .map(read_session_header)
                .unwrap_or((None, None));
            let already_titled = host0.session_title(&session_id).is_some();
            if parent.is_none() && !already_titled {
                let first_text = inner.log.lock().ok().and_then(|l| {
                    l.iter()
                        .find(|e| e.r#type == "user/message")
                        .and_then(|e| e.data["content"].as_str().map(str::to_owned))
                });
                // 只对「首条 user 消息内容」触发生成(源 automatic=first-prompt;
                // 且消息文本非空)
                if let Some(first_text) = first_text.filter(|t| !t.trim().is_empty()) {
                    let host_task = host0.clone();
                    let sid = session_id.clone();
                    // 源对该调用的生成在后台跑,不阻塞 turn;此处 spawn 到
                    // registry 所在 tokio 任务池(attach 时已 spawn 的任务即在此全局池)
                    tokio::spawn(async move {
                        let _ = host_task.generate_llm_title(&sid, &first_text).await;
                    });
                }
            }
        }

        // 待审计划:发问并等待(队列暂停 = 审批优先)
        if let Some(plan) = session.pending_plan() {
            let outcome = plan_question(&host0, &mut session, &session_id, plan).await;
            broadcast_event(&provider_info, &inner.log, &session_id, &host0.mux, None);
            // 「去聊天里说」:取消后注入引导轮,让模型接管并邀请用户在
            // 聊天中给出修改方向(用户点「去聊天里说」后期待 AI 引导);
            // 「否,并告诉它应该如何做不同」:反馈经同一注入直送模型
            match outcome {
                PlanReviewOutcome::Cancelled => {
                    inject_plan_guide_turn(&session_id, inner, &host0, PLAN_CANCEL_GUIDE);
                }
                PlanReviewOutcome::Declined {
                    feedback: Some(fb),
                } => {
                    inject_plan_guide_turn(&session_id, inner, &host0, &plan_decline_guide(&fb));
                }
                // 批准 = 模式已切回 standard + 注入「开工」引导轮:
                // 模型即刻按计划开始实施(否则批准后无任何后续动作)
                PlanReviewOutcome::Approved => {
                    inject_plan_guide_turn(&session_id, inner, &host0, PLAN_APPROVED_GUIDE);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 冷加载修夏:悬挂 tool/call 补 isError result(success=false、
    /// call=调用 seq、id 取 assistant tool_calls)+ turn/end(cancelled)
    /// 收口;已闭合 turn 的悬挂调用只补 result;再跑幂等零追加。
    #[test]
    fn repair_dangling_calls_closes_dangling_tool_call_and_turn() {
        let dir =
            std::env::temp_dir().join(format!("dsh-core-repair-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let backend = dsh_host::JsonlBackend::open(dir.join("s.jsonl")).unwrap();

        // 场景 A:turn 开着 + ask_user_question 无 result(重启遗留现场)
        let log = Arc::new(Mutex::new(EventLog::new()));
        {
            let mut l = log.lock().unwrap();
            for (ty, data) in [
                ("turn/start", serde_json::json!({})),
                (
                    "assistant/message",
                    serde_json::json!({
                        "content": "",
                        "tool_calls": [
                            { "id": "callu_1", "name": "ask_user_question", "arguments": {} }
                        ]
                    }),
                ),
                (
                    "tool/call",
                    serde_json::json!({ "name": "ask_user_question", "arguments": {} }),
                ),
            ] {
                l.append(EventEnvelope::new(ty, 0, data)).unwrap();
            }
        }
        repair_dangling_calls(&log, &backend);
        {
            let l = log.lock().unwrap();
            let evs: Vec<&EventEnvelope> = l.iter().collect();
            assert_eq!(evs.len(), 5, "应追加合成 result + turn/end");
            let r = evs[3];
            assert_eq!(r.r#type, "tool/result");
            assert_eq!(r.data["call"], serde_json::json!(3), "配对键 = call seq");
            assert_eq!(r.data["id"], serde_json::json!("callu_1"));
            assert_eq!(r.data["success"], serde_json::json!(false), "必须显式 false");
            assert_eq!(
                r.data["output"],
                serde_json::json!(dsh_session::events::DANGLING_TOOL_PLACEHOLDER)
            );
            let t = evs[4];
            assert_eq!(t.r#type, "turn/end");
            assert!(t.data.get("cancelled").is_some(), "中止收口");
        }
        // 幂等:再跑零追加
        repair_dangling_calls(&log, &backend);
        assert_eq!(log.lock().unwrap().iter().count(), 5, "再跑应零追加");

        // 场景 B:turn 已闭合(取消路径遗留)→ 只补 result,不动 turn
        let log2 = Arc::new(Mutex::new(EventLog::new()));
        {
            let mut l = log2.lock().unwrap();
            for (ty, data) in [
                ("turn/start", serde_json::json!({})),
                ("tool/call", serde_json::json!({ "name": "bash", "arguments": {} })),
                ("turn/end", serde_json::json!({ "cancelled": "token" })),
            ] {
                l.append(EventEnvelope::new(ty, 0, data)).unwrap();
            }
        }
        repair_dangling_calls(&log2, &backend);
        {
            let l = log2.lock().unwrap();
            let evs: Vec<&EventEnvelope> = l.iter().collect();
            assert_eq!(evs.len(), 4, "只追加合成 result");
            assert_eq!(evs[3].r#type, "tool/result");
            assert_eq!(evs[3].data["call"], serde_json::json!(2));
            assert_eq!(evs[3].data["id"], serde_json::json!(""), "无 assistant 时容忍空 id");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 非默认工作区:会话 id 复合 "<ws>/<stem>",ask 发帧必须用同一
    /// 槽位 id(此前 ask 工具从文件路径反推裸 stem → 帧会话与桌面当前
    /// 会话不相等 → 问答卡整批跳过 = 不弹窗)
    #[tokio::test]
    async fn ask_questions_frame_uses_slot_id_for_non_default_workspace() {
        let host = temp_host("ask-ws");
        let ws2 =
            std::env::temp_dir().join(format!("dsh-core-ask-ws2-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&ws2).unwrap();
        let name = host
            .add_workspace(ws2.display().to_string().as_str())
            .unwrap();
        let sid = host.create_session(None, None, Some(name.clone()));
        assert!(sid.starts_with(&format!("{name}/")), "复合 id 形态");

        let mut mux = host.mux_subscribe();
        let ask_sid = sid.clone();
        let ask_task = tokio::spawn(async move {
            let _ = host
                .ask_questions_json(
                    &ask_sid,
                    vec![serde_json::json!({
                        "id": "t1",
                        "question": "优先级?",
                        "options": [ { "label": "A" } ],
                        "multi_select": false
                    })],
                )
                .await;
        });
        let f = recv_until(&mut mux, |f| f.method == "question/requested")
            .await
            .expect("question/requested 帧");
        assert_eq!(
            f.payload["sessionId"].as_str(),
            Some(sid.as_str()),
            "帧 sessionId 必须为槽位复合 id"
        );
        ask_task.abort();
    }

    /// 余额计费端到端:fetch_billing 走真实 HTTP(本地 mock 端点,
    /// DeepSeek 余额响应形状)→ 路径求值 → billing_cache 落盘
    #[tokio::test]
    async fn fetch_billing_balance_writes_cache() {
        let host = temp_host("billing");
        let ws =
            std::env::temp_dir().join(format!("dsh-core-billing-ws-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&ws).unwrap();

        // 本地 mock:收一个连接,回 DeepSeek 余额响应形状
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let body = r#"{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"9.52"}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            std::io::Write::write_all(&mut stream, resp.as_bytes()).unwrap();
        });

        let mut p = crate::settings::builtin_provider();
        p.base_url = format!("http://127.0.0.1:{port}/v1");
        p.billing = Some(BillingConfig {
            kind: BillingKind::Balance,
            url: format!("http://127.0.0.1:{port}/user/balance"),
            paths: crate::settings::BillingPaths {
                balance: Some("balance_infos.0.total_balance".into()),
                currency: Some("balance_infos.0.currency".into()),
                ..Default::default()
            },
        });
        host.upsert_provider(p).unwrap();

        host.fetch_billing("deepseek", &ws).await.expect("查询成功");

        let view = host.settings_view();
        let me = view["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "deepseek")
            .unwrap();
        assert_eq!(me["billing_cache"]["kind"], "balance");
        assert_eq!(me["billing_cache"]["amount"], "9.52");
        assert_eq!(me["billing_cache"]["currency"], "CNY");
        assert!(me["billing_cache"]["fetched_at_ms"].as_u64().unwrap() > 0);
        server.join().unwrap();
    }

    fn temp_host(tag: &str) -> Arc<AppHost> {
        let dir = std::env::temp_dir().join(format!("dsh-core-{tag}-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        // 会话根注入临时目录(不污染 ~/.dshrs)
        let sroot =
            std::env::temp_dir().join(format!("dsh-core-{tag}-sroot-{}", Uuid::new_v4().simple()));
        Arc::new(AppHost::new_at(dir, true, "test-key", sroot).unwrap())
    }

    /// 会话根下项目目录(测试 fixture 定位)
    fn proj_dir(host: &AppHost, ws: &std::path::Path) -> PathBuf {
        host.sessions_root
            .join(project_key(&ws.display().to_string()))
    }

    fn script(msgs: &[&str]) -> Vec<Vec<LlmEvent>> {
        msgs.iter()
            .map(|m| {
                vec![
                    LlmEvent::Chunk((*m).into()),
                    LlmEvent::AssistantMessage(json!({ "content": m })),
                    LlmEvent::Done,
                ]
            })
            .collect()
    }

    /// 收帧直到谓词命中或超时
    async fn recv_until(
        mux: &mut broadcast::Receiver<ServerRequest>,
        pred: impl Fn(&ServerRequest) -> bool,
    ) -> Option<ServerRequest> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match mux.try_recv() {
                Ok(f) if pred(&f) => return Some(f),
                Ok(_) => continue,
                Err(broadcast::error::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await
                }
                Err(_) => return None,
            }
        }
        None
    }

    /// 轮询磁盘日志 fold 出 sandbox 模式,直到等于期望或超时。
    /// set_permission 走 worker 异步落盘,测试据此等待事件持久化。
    async fn wait_log_sandbox(host: &AppHost, id: &str, want: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mode = session_sandbox_of(host, id);
            if mode == want {
                return mode;
            }
            if std::time::Instant::now() >= deadline {
                return mode;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// 单次从磁盘 fold 当前 sandbox 模式(测试辅助;读文件而非运行态)
    fn session_sandbox_of(host: &AppHost, id: &str) -> String {
        let path = host.session_log_path(id);
        match load_envelopes(&path) {
            Some(events) => crate::permission::sandbox_mode_of(&events).to_string(),
            None => crate::permission::DEFAULT_SANDBOX_MODE.to_string(),
        }
    }

    /// 提交一轮 prompt 并等到 turn/end(确保 driver_loop 注入已落盘)。
    async fn run_turn(
        host: &Arc<AppHost>,
        mux: &mut broadcast::Receiver<ServerRequest>,
        id: &str,
        txt: &str,
    ) {
        host.prompt(id, &[json!({ "type": "text", "text": txt })], "queue")
            .await
            .unwrap();
        loop {
            let f = recv_until(mux, |f| {
                f.method == "session/event"
                    && f.payload["event"]["type"].as_str() == Some("turn/end")
            })
            .await
            .expect("turn/end 帧");
            if f.payload["event"]["type"].as_str() == Some("turn/end") {
                break;
            }
        }
    }

    /// 子代理结算通知 → 驱动认领作 turn 输入,user/message 携
    /// source.kind=subagent-settled 落档(模型可见,桌面凭此渲染通知卡);
    /// 队列帧全程不闪现通知文本(queue_frame 过滤)
    #[tokio::test]
    async fn subagent_settlement_notice_becomes_tinted_turn_input() {
        let host = temp_host("d47-notice");
        host.set_fake_script(script(&["hello", "notice processed"]));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        // 先跑一个普通 turn 完成附着
        host.prompt(&id, &[json!({ "type": "text", "text": "hi" })], "queue")
            .await
            .unwrap();
        recv_until(&mut mux, |f| {
            f.method == "session/event"
                && f.payload["event"]["type"] == "turn/end"
        })
        .await
        .expect("首 turn 结束");

        // 结算通知入队(settlementSummary + closing message 拼装)
        let source = json!({
            "kind": "subagent-settled",
            "form": "notice",
            "summary": "Background subagent s-x finished and will do no further work unless you send it more.",
            "senderSessionId": "s-x",
        });
        host.notify_subagent_settled(
            &id,
            "Background subagent s-x finished and will do no further work unless you send it \
             more.\n\nIts closing message:\n\nthe report"
                .into(),
            source,
        );
        // 驱动认领通知 → 新 turn(模型回复来自脚本第 2 条);沿途收集
        // 队列帧——全程不得闪现通知文本(queue_frame 按 source 过滤)
        let mut queue_payloads: Vec<Value> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if std::time::Instant::now() >= deadline {
                panic!("通知 turn 未在预算内结束");
            }
            match mux.try_recv() {
                Ok(f) => {
                    if f.method == "session/queue" {
                        queue_payloads.push(f.payload.clone());
                    }
                    if f.method == "session/event" && f.payload["event"]["type"] == "turn/end" {
                        break;
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => panic!("mux 关闭"),
            }
        }
        assert!(
            !queue_payloads
                .iter()
                .any(|p| p.to_string().contains("Background subagent")),
            "队列帧不得闪现通知文本:{queue_payloads:?}"
        );

        // 日志:通知以 user/message + source.kind=subagent-settled 落档
        let events = dsh_host::persistence::jsonl::load_jsonl(&host.session_log_path(&id))
            .unwrap_or_default();
        let notice = events
            .iter()
            .find(|e| {
                e.r#type == "user/message"
                    && e.data["source"]["kind"] == "subagent-settled"
            })
            .expect("通知必须以染色 user/message 落档");
        assert_eq!(notice.data["source"]["form"], "notice");
        assert_eq!(notice.data["source"]["senderSessionId"], "s-x");
        let notice_text = notice.data["content"].as_str().unwrap_or_default();
        assert!(notice_text.contains("Background subagent s-x finished"), "{notice_text}");
        assert!(notice_text.contains("Its closing message:"), "{notice_text}");
        // 通知后的 assistant 回复 = 模型确实看到了通知并推进 turn
        let notice_seq = notice.seq;
        assert!(events.iter().any(|e| {
            e.r#type == "assistant/message"
                && e.seq > notice_seq
                && e.data["content"] == "notice processed"
        }));
    }

    /// prompt → 有序翻译事件
    #[tokio::test]
    async fn prompt_streams_translated_events() {
        let host = temp_host("prompt");
        host.set_fake_script(script(&["hello"]));
        let mut mux = host.mux_subscribe();

        let id = host.create_session(None, None, None);
        host.prompt(&id, &[json!({ "type": "text", "text": "hi" })], "queue")
            .await
            .unwrap();

        // 收帧直到 turn/end,沿途积累事件顺序
        let mut types = Vec::new();
        loop {
            let f = recv_until(&mut mux, |f| f.method == "session/event")
                .await
                .expect("事件帧");
            let ty = f.payload["event"]["type"].as_str().unwrap().to_string();
            let is_end = ty == "turn/end";
            types.push(ty);
            if is_end {
                break;
            }
        }
        assert_eq!(
            types,
            vec![
                // 驱动认领 splice(队列条目出队)先于 turn 事件
                "agent/inbox/spliced",
                // step 级事件序——真实用户(snapshot 也作 user/message)在 step/start 之后
                "turn/start",
                "step/start",
                // 真实用户消息(snapshot 也在此落档为 user/message + source.kind=plugin,
                // 由引擎内 RuntimeContextProjection 每步判断生成)
                "user/message",
                "user/message",
                "assistant/chunk",
                "assistant/message",
                "step/end",
                "turn/end"
            ]
        );
    }

    /// AGENTS.md 基线不再 attach 时注入:per-step 指令重扫的 compose 在
    /// 首个 turn 的首步发现日志无基线 → 整段落档,时序天然排在用户消息
    /// 之后(pre-step 语义);后续 turn 认可既存基线不再注。
    #[tokio::test]
    async fn agents_md_baseline_injected_on_first_step_after_user_message() {
        let host = temp_host("agents-first-step");
        // 工作区写 AGENTS.md(find_agents_instructions 自工作区向上找)
        let ws = host.workspace().to_path_buf();
        std::fs::write(ws.join("AGENTS.md"), "# 项目规范\n\n用 Rust。").unwrap();
        let id = host.create_session(None, None, None);
        let baseline_count = || {
            std::fs::read_to_string(host.session_log_path(&id))
                .unwrap()
                .lines()
                .filter(|l| l.contains("\"user/message\"") && l.contains("agent-instructions"))
                .count()
        };
        // attach(history)不注入基线
        host.history(&id, None, 100).await.unwrap();
        assert_eq!(baseline_count(), 0, "attach 不注入基线");

        // 首个 turn:首步 compose 落档基线,且排在用户消息之后
        host.set_fake_script(script(&["好"]));
        let mut mux = host.mux_subscribe();
        run_turn(&host, &mut mux, &id, "hi").await;
        assert_eq!(baseline_count(), 1, "首步注入一次基线");
        let text = std::fs::read_to_string(host.session_log_path(&id)).unwrap();
        assert!(text.contains("项目规范"), "基线内容为 AGENTS.md 文本");
        let (mut user_pos, mut base_pos) = (None, None);
        for (i, l) in text.lines().enumerate() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else {
                continue;
            };
            if v["type"] != "user/message" {
                continue;
            }
            if v["data"]["source"]["kind"] == "agent-instructions" {
                if base_pos.is_none() {
                    base_pos = Some(i);
                }
            } else if v["data"]["source"].is_null() && user_pos.is_none() {
                // 真实用户消息无 source(kind 只在注入载荷上)
                user_pos = Some(i);
            }
        }
        assert!(
            user_pos.is_some() && base_pos.is_some() && user_pos < base_pos,
            "基线须排在用户消息之后(user_pos={user_pos:?} base_pos={base_pos:?})"
        );

        // 第二个 turn:基线已在日志且身份匹配 → 不再注入
        host.set_fake_script(script(&["again"]));
        run_turn(&host, &mut mux, &id, "second").await;
        assert_eq!(baseline_count(), 1, "既存基线被认可,不重复注入");
    }

    /// runtime-context 快照注入去重——首轮注入源 plugin 快照;
    /// 同策略再 turn 不重发;切换权限后文本变 → 重发。
    #[tokio::test]
    async fn runtime_snapshot_injected_and_deduped() {
        let host = temp_host("rt-snapshot");
        // 三轮各一段:脚本耗尽后的空响应是可重试失败,
        // 不再「静默空回复收尾」
        host.set_fake_script(script(&["hi", "again", "third"]));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        let plugin_ctx = || {
            let path = host.session_log_path(&id);
            std::fs::read_to_string(&path)
                .unwrap_or_default()
                .lines()
                .filter(|l| l.contains("\"user/message\"") && l.contains("\"plugin\""))
                .count()
        };
        // 首轮:注入默认权限快照(workspace-write + ask → 恒非空,必注入)
        run_turn(&host, &mut mux, &id, "hi").await;
        assert_eq!(plugin_ctx(), 1, "首轮注入一次 plugin 快照");
        let text = std::fs::read_to_string(host.session_log_path(&id)).unwrap();
        assert!(
            text.contains("@deepseek-ai/dsh-system-prompt"),
            "快照 source.plugin 对齐源插件名"
        );

        // 二轮:策略未变 → 去重,不新增
        run_turn(&host, &mut mux, &id, "again").await;
        assert_eq!(plugin_ctx(), 1, "策略未变不重发快照");

        // 切 full-access → 文本变 → 重发
        host.set_permission(&id, "full-access")
            .await
            .unwrap();
        wait_log_sandbox(&host, &id, "full-access").await;
        run_turn(&host, &mut mux, &id, "third").await;
        assert_eq!(plugin_ctx(), 2, "策略变更重发快照");
        let text = std::fs::read_to_string(host.session_log_path(&id)).unwrap();
        assert!(
            text.contains("full-access"),
            "变更后快照含新 sandbox 文本"
        );
    }

    /// 新会话 pin 默认权限预设(pinInitialPermission)——首轮 turn 前
    /// 按 default_permission 捆绑事件补全(sandbox + approval + preset),使
    /// 用户设的默认权限预设生效且 fold/回放确定性。
    #[tokio::test]
    async fn new_session_pins_default_permission_preset() {
        let host = temp_host("pin-perm");
        host.set_default_permission_preset("full-access")
            .unwrap();
        host.set_fake_script(script(&["hi"]));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        run_turn(&host, &mut mux, &id, "hi").await;

        // pin 后 fold:sandbox=full-access,approval=never(捆绑)
        assert_eq!(
            wait_log_sandbox(&host, &id, "full-access").await,
            "full-access"
        );
        assert_eq!(host.session_approval(&id), "never", "捆绑预设钉 never");
        let path = host.session_log_path(&id);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("permission/preset"), "首轮 pin 权限预设事件");
    }

    /// 4a:prompt_with_contexts → 引擎在 turn/start + user/message **之后**落档注入
    /// (user/message + source.kind≠user),preStep `[...claimed, context]`
    /// 事件序:用户注入的 session-reference(ctx-a)先、driver 的 runtime-snapshot 后。
    #[tokio::test]
    async fn prompt_with_contexts_commits_after_user_message() {
        let host = temp_host("ctx-prompt");
        host.set_fake_script(script(&["hello"]));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        host.prompt_with_contexts(
            &id,
            &[json!({ "type": "text", "text": "hi" })],
            "queue",
            vec![json!({
                "content": "@other 会话快照",
                "id": "ctx-a",
                "source": { "kind": "session-reference", "form": "recall" },
            })],
        )
        .await
        .unwrap();

        // 收帧:注入(user/message)应出现在 turn/start 与真实 user/message 之后
        let mut saw_turn_start = false;
        let mut seen_injections = Vec::new();
        loop {
            let f = recv_until(&mut mux, |f| f.method == "session/event")
                .await
                .expect("事件帧");
            let ty = f.payload["event"]["type"].as_str().unwrap().to_string();
            if ty == "user/message" {
                let kind = f.payload["event"]["data"]["source"]["kind"]
                    .as_str()
                    .unwrap_or("user");
                if kind != "user" {
                    assert!(
                        saw_turn_start,
                        "注入(user/message+source≠user)应在 turn/start 之后"
                    );
                    seen_injections.push(kind.to_string());
                }
            }
            if ty == "turn/start" {
                saw_turn_start = true;
            }
            if ty == "turn/end" {
                break;
            }
        }
        // 首个注入应为用户提供的 session-reference(ctx-a),其次 driver 快照(plugin)
        assert_eq!(
            seen_injections.first().map(String::as_str),
            Some("session-reference"),
            "首个注入应为用户提供 context"
        );
        assert!(
            !seen_injections.is_empty(),
            "应有注入落档(user/session-reference + plugin 快照)"
        );
    }

    /// 4b:首条 user/message 落档后触发 LLM 语义标题生成(fake 注入输出),
    /// 写入 titles 映射并以 `session/projection` 帧广播。
    #[tokio::test]
    async fn first_prompt_generates_llm_title_and_broadcasts() {
        let host = temp_host("title-llm");
        host.set_fake_script(script(&["hello"]));
        host.set_fake_title(Some("Justfile".into()));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        host.prompt(
            &id,
            &[json!({ "type": "text", "text": "@justfile" })],
            "queue",
        )
        .await
        .unwrap();

        // 收帧直到 turn/end(标题生成在 turn 后异步触发)
        loop {
            let f = recv_until(&mut mux, |f| f.method == "session/event")
                .await
                .expect("事件帧");
            if f.payload["event"]["type"].as_str() == Some("turn/end") {
                break;
            }
        }

        // 生成是 spawn 的异步任务;轮询等 title 投影帧
        let title_frame = recv_until(&mut mux, |f| {
            f.method == "session/projection" && f.payload["key"] == "title"
        })
        .await
        .expect("title 投影帧");
        assert_eq!(title_frame.payload["sessionId"], id);
        assert_eq!(title_frame.payload["value"], "Justfile");

        // titles 映射落地(session_title 反映 LLM 标题)
        assert_eq!(host.session_title(&id).as_deref(), Some("Justfile"));
        // 持久化 .dsh-titles.json 含该会话
        let file = std::fs::read_to_string(host.workspace().join(".dsh-titles.json")).unwrap();
        assert!(file.contains("Justfile"), "标题应持久化:{file}");
    }

    /// 1x1 PNG(合法最小文件,附件准入全解码可过)
    const TEST_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x90,
        0xEF, 0xE6, 0x00, 0x00, 0x01, 0x7F, 0x00, 0xB3, 0xC4, 0x4B, 0x81, 0x05, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn image_part() -> Value {
        use base64::Engine as _;
        json!({
            "type": "image",
            "mediaType": "image/png",
            "data": base64::engine::general_purpose::STANDARD.encode(TEST_PNG),
            "name": "dot.png",
        })
    }

    /// 图片准入 → 落档块数组(图前文后)→ read_attachment 授权读取
    #[tokio::test]
    async fn prompt_image_roundtrip_and_read_attachment() {
        let host = temp_host("img-rt");
        host.set_fake_script(script(&["seen"]));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        host.prompt(
            &id,
            &[image_part(), json!({ "type": "text", "text": "看图" })],
            "queue",
        )
        .await
        .unwrap();

        // 收到 user/message:content = 块数组,图前文后
        let f = recv_until(&mut mux, |f| {
            f.method == "session/event" && f.payload["event"]["type"] == "user/message"
        })
        .await
        .expect("user/message 帧");
        let content = &f.payload["event"]["data"]["content"];
        let blocks = content.as_array().unwrap();
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["attachment"]["mediaType"], "image/png");
        assert_eq!(blocks[0]["attachment"]["name"], "dot.png");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "看图");
        let attachment_id = blocks[0]["attachment"]["attachmentId"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(attachment_id.starts_with("sha256:"));

        // 授权读取:本会话引用 → base64 原字节
        let read = host.read_attachment(&id, &attachment_id).unwrap();
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(read["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, TEST_PNG);
        assert_eq!(read["attachment"]["attachmentId"], attachment_id);

        // 未引用 id → 拒
        let absent = format!("sha256:{}", "b".repeat(64));
        let err = host.read_attachment(&id, &absent).unwrap_err();
        assert_eq!(err.code, "attachment-error");
        assert_eq!(err.details["reason"], "ATTACHMENT_NOT_REFERENCED");
    }

    /// 准入拒绝面(白名单外类型 / 非法 base64)错误码
    #[tokio::test]
    async fn prompt_image_admission_rejections() {
        let host = temp_host("img-rej");
        let id = host.create_session(None, None, None);
        let mut bmp = image_part();
        bmp["mediaType"] = json!("image/bmp");
        let err = host.prompt(&id, &[bmp], "queue").await.unwrap_err();
        assert_eq!(err.code, "attachment-error");
        assert_eq!(err.details["reason"], "UNSUPPORTED_IMAGE_TYPE");

        let mut bad = image_part();
        bad["data"] = json!("!!!not-base64!!!");
        let err = host.prompt(&id, &[bad], "queue").await.unwrap_err();
        assert_eq!(err.details["reason"], "INVALID_IMAGE_BASE64");

        // 命令 + 图 → 全批拒绝(command.imagesUnsupported)
        let err = host
            .prompt(
                &id,
                &[json!({ "type": "text", "text": "/plan on" }), image_part()],
                "queue",
            )
            .await
            .unwrap_err();
        assert_eq!(err.details["reason"], "COMMAND_IMAGES_UNSUPPORTED");
        assert_eq!(err.message, "/plan 不接受图片附件,请先移除图片");
    }

    /// 含图队列条目重启重建(images 键回读)+ 编辑拒绝(QUEUE_EDIT_NON_TEXT)
    #[tokio::test]
    async fn queue_image_entry_replay_and_edit_rejection() {
        // 直接落一条带图的入队 splice(模拟上一进程排队后退出)
        let host = temp_host("img-q");
        let id = host.create_session(None, None, None);
        let path = host.session_log_path(&id);
        let attachment = json!({
            "attachmentId": format!("sha256:{}", "a".repeat(64)),
            "mediaType": "image/png",
            "bytes": 69,
            "width": 1,
            "height": 1,
        });
        std::fs::write(
            &path,
            json!({
                "type": "agent/inbox/spliced", "seq": 1, "time": 1,
                "data": {
                    "target": "next-turn", "start": 0, "removedCount": 0,
                    "inserted": [ { "id": "q1", "content": "", "images": [attachment] } ],
                },
            })
            .to_string(),
        )
        .unwrap();
        let mut mux = host.mux_subscribe();
        host.history(&id, None, 50).await.unwrap();
        let f = recv_until(&mut mux, |f| f.method == "session/queue")
            .await
            .expect("冷附着队列基线帧");
        let item = &f.payload["items"][0];
        assert_eq!(item["message"]["content"][0]["type"], "image");

        // 编辑含图条目 → QUEUE_EDIT_NON_TEXT
        let err = host
            .update_queue(
                &id,
                "q1",
                &json!({
                    "kind": "edit",
                    "content": [ { "type": "text", "text": "改文本" } ],
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, "attachment-error");
        assert_eq!(err.details["reason"], "QUEUE_EDIT_NON_TEXT");
    }

    /// /export ZIP 含 media/ 条目(内容寻址路径;缺席对象跳过不阻断)
    #[tokio::test]
    async fn export_zip_includes_media_entries() {
        let host = temp_host("img-zip");
        host.set_fake_script(script(&["ok"]));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        host.prompt(
            &id,
            &[image_part(), json!({ "type": "text", "text": "留档" })],
            "queue",
        )
        .await
        .unwrap();
        let f = recv_until(&mut mux, |f| {
            f.method == "session/event" && f.payload["event"]["type"] == "user/message"
        })
        .await
        .expect("user/message 帧");
        let attachment_id = f.payload["event"]["data"]["content"][0]["attachment"]["attachmentId"]
            .as_str()
            .unwrap()
            .to_string();
        let zip_bytes = host.export_session_zip(&id, false).unwrap();
        let cursor = std::io::Cursor::new(&zip_bytes);
        let archive = zip::ZipArchive::new(cursor).unwrap();
        let names: Vec<String> = archive.file_names().map(String::from).collect();
        assert!(names.contains(&"session.jsonl".to_string()));
        assert!(
            names.contains(&format!("media/{attachment_id}.png")),
            "media 条目应在列:{names:?}"
        );
    }

    /// 轨迹分页:尾窗 / beforeIndex 向前翻页 / hasOlder / total 语义
    #[tokio::test]
    async fn trajectory_page_windows_before_index() {
        let host = temp_host("traj-page");
        host.set_fake_script(script(&["a", "b", "c"]));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        for text in ["one", "two", "three"] {
            host.prompt(&id, &[json!({ "type": "text", "text": text })], "queue")
                .await
                .unwrap();
            let done = recv_until(&mut mux, |f| {
                f.method == "session/event"
                    && f.payload["event"]["type"] == "turn/end"
                    && f.payload["sessionId"].as_str() == Some(id.as_str())
            })
            .await;
            assert!(done.is_some(), "turn/end 帧");
        }

        let full = host.trajectory_page(&id, 2000, None).expect("全量页");
        assert!(
            full.records.len() >= 6,
            "3 轮至少 user+message 各 3 条,实际 {}",
            full.records.len()
        );
        assert!(!full.has_older);
        assert_eq!(full.total as usize, full.records.len());
        assert_eq!(full.requests.len(), 3, "三轮各一次请求");

        // 尾窗:更小窗口 → hasOlder + 尾部对齐 + total 不变
        let win = host.trajectory_page(&id, 2, None).expect("尾窗");
        assert!(win.has_older);
        assert_eq!(win.records.len(), 2);
        assert_eq!(
            win.records[1].index,
            full.records.last().unwrap().index,
            "尾窗贴齐最新记录"
        );
        assert_eq!(win.total, full.total);

        // beforeIndex 向前翻页:全部早于窗口首条,满页
        let first = win.records[0].index;
        let older = host.trajectory_page(&id, 2, Some(first)).expect("更早窗");
        assert!(older.has_older);
        assert_eq!(older.records.len(), 2);
        assert!(older.records.iter().all(|r| r.index < first));

        // 翻到头:before=1 → 仅 index 0,无更早
        let head = host.trajectory_page(&id, 2000, Some(1)).expect("首页");
        assert!(!head.has_older);
        assert!(head.records.iter().all(|r| r.index < 1));
    }

    /// 重开会话:日志含 agent/inbox/spliced(steer 认领)时新宿主必须能
    /// 加载——splice 漏登记 KNOWN_EVENT_TYPES 曾导致整份日志拒读
    /// (「会话没有加载」)
    #[tokio::test]
    async fn reopen_session_with_splice_events() {
        let ws = std::env::temp_dir().join(format!("dsh-core-reopen-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&ws).unwrap();
        let sroot =
            std::env::temp_dir().join(format!("dsh-core-reopen-sroot-{}", Uuid::new_v4().simple()));
        let host1 = Arc::new(AppHost::new_at(ws.clone(), true, "test-key", sroot.clone()).unwrap());
        host1.set_fake_script(script(&["hi"]));
        let id = host1.create_session(None, None, None);
        host1
            .prompt(&id, &[json!({ "type": "text", "text": "hi" })], "queue")
            .await
            .unwrap();
        // 等 turn 落档(含 claim splice)
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(host1);

        // 模拟重启:同一 workspace + 会话根的新宿主
        let host2 = Arc::new(AppHost::new_at(ws, true, "test-key", sroot).unwrap());
        let page = host2.history(&id, None, 50).await.expect("重开加载成功");
        assert!(
            page.events
                .iter()
                .any(|e| e.event.ty == "agent/inbox/spliced"),
            "重放保留 splice 事件"
        );
    }

    /// 统计事件驱动推送:turn 内 turn/start/step/start/audit/call 落档
    /// 点推 session/stats(替代客户端轮询);推送值与 session_stats
    /// RPC 全量 fold 恒等(同一 apply 逻辑)
    #[tokio::test]
    async fn stats_push_event_driven_and_consistent() {
        let host = temp_host("stats-push");
        host.set_fake_script(script(&["hello"]));
        let mut mux = host.mux_subscribe();

        let id = host.create_session(None, None, None);
        host.prompt(&id, &[json!({ "type": "text", "text": "hi" })], "queue")
            .await
            .unwrap();

        // 收齐直到 turn/end,累计 stats 推送
        let mut pushes = Vec::new();
        loop {
            let f = recv_until(&mut mux, |f| {
                f.method == "session/event" || f.method == "session/stats"
            })
            .await
            .expect("帧");
            if f.method == "session/event" && f.payload["event"]["type"] == "turn/end" {
                break;
            }
            if f.method == "session/stats" {
                assert_eq!(f.payload["sessionId"], json!(id), "推送携带会话 id");
                pushes.push(f.payload["stats"].clone());
            }
        }
        // turn/start、step/start、(fake 无工具)audit/call → 至少两推
        assert!(pushes.len() >= 2, "统计推送过少(事件驱动断裂): {pushes:?}");
        // 首推即见 turns=1(turn 开始落档点,不等回合结束)
        assert_eq!(pushes[0]["turns"], 1, "首推应含 turn 计数");
        // 末推与 RPC 全量 fold 恒等
        let rpc = host.session_stats(&id).expect("RPC 统计");
        assert_eq!(
            pushes.last().unwrap()["turns"],
            rpc["turns"],
            "推送与 RPC 恒等(turns)"
        );
        assert_eq!(
            pushes.last().unwrap()["contextUsed"],
            rpc["contextUsed"],
            "推送与 RPC 恒等(contextUsed)"
        );
    }

    /// 队列串行:两个 prompt 依次各跑一个 turn
    #[tokio::test]
    async fn queue_serializes_turns() {
        let host = temp_host("queue");
        host.set_fake_script(script(&["a", "b"]));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        host.prompt(&id, &[json!({ "type": "text", "text": "q1" })], "queue")
            .await
            .unwrap();
        host.prompt(&id, &[json!({ "type": "text", "text": "q2" })], "queue")
            .await
            .unwrap();

        let mut turn_ends = 0;
        while turn_ends < 2 {
            recv_until(&mut mux, |f| {
                f.method == "session/event" && f.payload["event"]["type"] == "turn/end"
            })
            .await
            .expect("第二个 turn/end 应到达");
            turn_ends += 1;
        }
    }

    /// steer 而会话空闲:驱动优先认领为下一 turn(next-step 先于
    /// next-turn),认领 splice + user/message 落档,单 turn 内运行
    #[tokio::test]
    async fn steer_while_idle_starts_turn() {
        let host = temp_host("steer");
        host.set_fake_script(script(&["steered"]));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        host.prompt(
            &id,
            &[json!({ "type": "text", "text": "steer-now" })],
            "steer",
        )
        .await
        .unwrap();

        // turn 事件流:认领 splice → turn/start → user/message(steer) → turn/end
        let mut saw_splice = false;
        let mut saw_steer = false;
        loop {
            let f = recv_until(&mut mux, |f| f.method == "session/event")
                .await
                .expect("事件帧");
            let ty = f.payload["event"]["type"].as_str().unwrap().to_string();
            if ty == "agent/inbox/spliced" {
                saw_splice = true;
            }
            // 仅真实用户消息(source.kind=user)断言为 steer 文本;注入行
            // (source.kind≠user,如 runtime 快照)不作数
            if ty == "user/message" {
                let kind = f.payload["event"]["data"]["source"]["kind"]
                    .as_str()
                    .unwrap_or("user");
                if kind == "user" {
                    assert_eq!(
                        f.payload["event"]["data"]["content"][0]["text"],
                        "steer-now"
                    );
                    saw_steer = true;
                }
            }
            if ty == "turn/end" {
                break;
            }
        }
        assert!(saw_splice, "steer 认领 splice 已落档");
        assert!(saw_steer, "steer 文本已作为 user/message 落档");
    }

    /// updateQueue 错误码:未附着/未决条目不存在 → queue-item-not-found;
    /// 空闲会话 steer → steer-unavailable;未知 kind → bad_request
    #[tokio::test]
    async fn update_queue_errors_match_source() {
        let host = temp_host("queue-err");
        host.set_fake_script(script(&["a"]));
        let id = host.create_session(None, None, None);

        // 未附着(无队列存在)
        let err = host
            .update_queue(&id, "nope", &json!({ "kind": "remove" }))
            .await
            .unwrap_err();
        assert_eq!(err.code, "queue-item-not-found");

        // history 附着(空闲,无 turn)
        let _ = host.history(&id, None, 50).await.unwrap();
        let err = host
            .update_queue(&id, "nope", &json!({ "kind": "remove" }))
            .await
            .unwrap_err();
        assert_eq!(err.code, "queue-item-not-found");
        // 条目不存在时 not-found 优先(steer-unavailable 见
        // apply_queue_action 单测——条目存在但空闲的窗口)
        let err = host
            .update_queue(&id, "nope", &json!({ "kind": "steer" }))
            .await
            .unwrap_err();
        assert_eq!(err.code, "queue-item-not-found");
        let err = host
            .update_queue(&id, "nope", &json!({ "kind": "bogus" }))
            .await
            .unwrap_err();
        assert_eq!(err.code, "bad-request");
    }

    /// apply_queue_action 直测:条目存在但空闲 → steer-unavailable;
    /// edit / remove / steer(running)各路径
    #[test]
    fn apply_queue_action_semantics() {
        let steer = Arc::new(Mutex::new(VecDeque::new()));
        let qs = Mutex::new(QueueState {
            pending: VecDeque::from([
                PendingItem {
                    id: "p1".into(),
                    text: "one".into(),
                    images: Vec::new(),
                    contexts: Vec::new(),
                },
                PendingItem {
                    id: "p2".into(),
                    text: "two".into(),
                    images: Vec::new(),
                    contexts: Vec::new(),
                },
            ]),
            steer: Arc::clone(&steer),
            running: false,
        });
        let wake = Notify::new();

        // 空闲 + 条目存在 → steer-unavailable
        let err = apply_queue_action(&qs, &wake, "p1", QueueAction::Steer).unwrap_err();
        assert_eq!(err.code, "steer-unavailable");

        // edit:文本替换
        apply_queue_action(&qs, &wake, "p2", QueueAction::Edit("edited".into())).unwrap();
        assert_eq!(qs.lock().unwrap().pending[1].text, "edited");

        // running 后 steer:条目出队入 steer 缓冲
        qs.lock().unwrap().running = true;
        apply_queue_action(&qs, &wake, "p1", QueueAction::Steer).unwrap();
        assert_eq!(qs.lock().unwrap().pending.len(), 1);
        let s = steer.lock().unwrap().pop_front().expect("steer 已入缓冲");
        assert_eq!(s.id, "p1");

        // remove
        apply_queue_action(&qs, &wake, "p2", QueueAction::Remove).unwrap();
        assert!(qs.lock().unwrap().pending.is_empty());
    }

    /// updateQueue edit 拒绝非 text 内容块(源 QUEUE_EDIT_NON_TEXT)
    #[tokio::test]
    async fn update_queue_rejects_non_text_edit() {
        let host = temp_host("queue-edit");
        let id = host.create_session(None, None, None);
        let err = host
            .update_queue(
                &id,
                "any",
                &json!({ "kind": "edit", "content": [ { "type": "image", "url": "x" } ] }),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, "queue-edit-non-text");
    }

    /// prompt 提交模式校验:非 queue/steer 拒绝
    #[tokio::test]
    async fn prompt_rejects_bad_mode() {
        let host = temp_host("mode");
        let id = host.create_session(None, None, None);
        let err = host
            .prompt(&id, &[json!({ "type": "text", "text": "hi" })], "kill")
            .await
            .unwrap_err();
        assert_eq!(err.code, "bad-request");
    }

    /// 计划问题全流程:预置待审计划文件 → attach 恢复发问 → 批准 →
    /// 日志含 plan/approved + 回 standard → resolved 帧
    #[tokio::test]
    async fn plan_question_approve_flow() {
        let host = temp_host("plan");
        let path = proj_dir(&host, &host.workspace)
            .join("s-plan")
            .join("session.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session/mode\",\"seq\":1,\"time\":0,\"data\":{\"mode\":\"plan\"},\"ignorable\":false}\n",
                "{\"type\":\"plan/submitted\",\"seq\":2,\"time\":0,\"data\":{\"plan\":\"# 计划\"},\"ignorable\":false}\n",
            ),
        )
        .unwrap();
        host.register_slot("s-plan", path);

        let mut mux = host.mux_subscribe();
        // history 触发 attach → worker 启动检查 → 发问
        let _ = host.history("s-plan", None, 50).await.unwrap();

        let question = recv_until(&mut mux, |f| f.method == "question/requested")
            .await
            .expect("应收到 question/requested");
        let rpc_id = question.rpc_id.clone();
        assert_eq!(
            question.payload["questions"][0]["intent"]["kind"],
            "plan-review"
        );
        // mux 基线重放同 rpcId
        let baseline = host.mux_baseline();
        assert!(
            baseline
                .iter()
                .any(|f| f.method == "question/requested" && f.rpc_id == rpc_id)
        );

        // 批准
        let receipt = host.respond(
            &rpc_id,
            &RpcResult::Ok(json!({
                "sessionId": "s-plan",
                "answer": { "answers": [ { "id": "plan", "selected": ["批准"] } ] }
            })),
        );
        assert!(receipt.accepted);

        let resolved = recv_until(&mut mux, |f| f.method == "question/resolved")
            .await
            .expect("resolved");
        assert_eq!(resolved.payload["outcome"], "approved");

        // 日志落档:plan/approved + 回 standard
        let text = std::fs::read_to_string(
            proj_dir(&host, &host.workspace)
                .join("s-plan")
                .join("session.jsonl"),
        )
        .unwrap();
        assert!(text.contains("plan/approved"));
        assert!(text.contains("\"standard\""));
    }

    /// 拒绝路径:不产生 plan/approved,仅回 standard
    #[tokio::test]
    async fn plan_question_decline_flow() {
        let host = temp_host("decline");
        let path = proj_dir(&host, &host.workspace)
            .join("s-dec")
            .join("session.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"type\":\"plan/submitted\",\"seq\":1,\"time\":0,\"data\":{\"plan\":\"# p\"},\"ignorable\":false}\n",
        )
        .unwrap();
        host.register_slot("s-dec", path);

        let mut mux = host.mux_subscribe();
        let _ = host.history("s-dec", None, 50).await.unwrap();
        let question = recv_until(&mut mux, |f| f.method == "question/requested")
            .await
            .expect("question");
        let receipt = host.respond(
            &question.rpc_id,
            &RpcResult::Ok(json!({
                "answer": { "answers": [ { "id": "plan", "selected": ["拒绝"] } ] }
            })),
        );
        assert!(receipt.accepted);
        let resolved = recv_until(&mut mux, |f| f.method == "question/resolved")
            .await
            .expect("resolved");
        assert_eq!(resolved.payload["outcome"], "declined");
        let text = std::fs::read_to_string(
            proj_dir(&host, &host.workspace)
                .join("s-dec")
                .join("session.jsonl"),
        )
        .unwrap();
        assert!(!text.contains("plan/approved"));
        assert!(text.contains("\"standard\""));
    }

    /// 「否,并告诉它应该如何做不同」(选项②是卡内
    /// 直接输入):应答 custom → 引导轮注入队列(splice 落档),修改意见
    /// 直送模型;跳过(无 custom)不注入
    #[tokio::test]
    async fn plan_decline_feedback_reaches_model_via_guide_turn() {
        let host = temp_host("decfb");
        let path = proj_dir(&host, &host.workspace)
            .join("s-decfb")
            .join("session.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"type\":\"plan/submitted\",\"seq\":1,\"time\":0,\"data\":{\"plan\":\"# p\"},\"ignorable\":false}\n",
        )
        .unwrap();
        host.register_slot("s-decfb", path);

        let mut mux = host.mux_subscribe();
        let _ = host.history("s-decfb", None, 50).await.unwrap();
        let question = recv_until(&mut mux, |f| f.method == "question/requested")
            .await
            .expect("question");

        // 拒绝 + 反馈(选项②行内输入提交)
        let receipt = host.respond(
            &question.rpc_id,
            &RpcResult::Ok(json!({
                "sessionId": "s-decfb",
                "answer": { "answers": [ { "id": "plan", "selected": ["拒绝"], "custom": "把登录改成 OAuth,不要自研" } ] }
            })),
        );
        assert!(receipt.accepted);
        let resolved = recv_until(&mut mux, |f| f.method == "question/resolved")
            .await
            .expect("resolved");
        assert_eq!(resolved.payload["outcome"], "declined");

        // 引导轮落档:splice 事件携带反馈原文
        let text = std::fs::read_to_string(
            proj_dir(&host, &host.workspace)
                .join("s-decfb")
                .join("session.jsonl"),
        )
        .unwrap();
        assert!(
            text.contains("agent/inbox/spliced") && text.contains("把登录改成 OAuth,不要自研"),
            "反馈应经引导轮落档送达模型"
        );
    }

    /// 冷会话 history:attach + 翻译 + 分页 + 投影(title)
    #[tokio::test]
    async fn cold_history_translates() {
        let host = temp_host("history");
        let path = proj_dir(&host, &host.workspace)
            .join("s-cold")
            .join("session.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // 用 fake session 预跑一个 turn 落档
        let backend = dsh_host::JsonlBackend::create(path.to_str().unwrap()).unwrap();
        let resolved = Resolved::resolve(
            dsh_app::ResolveArgs {
                workspace: Some(host.workspace.display().to_string()),
                session: Some(path.display().to_string()),
                ..Default::default()
            },
            &host.workspace.join("dsh.toml"),
        )
        .unwrap();
        let parts = dsh_app::prompt_parts(&resolved, true);
        let mut fake = FakeProvider::new();
        fake.then(script(&["第一答"])[0].clone());
        let gate = InvariantGate::new(fake, dsh_app::fresh_log());
        let log = gate.log();
        let mut session = Session::new(
            parts,
            gate,
            log,
            NoTools,
            backend,
            path.display().to_string(),
            CancelToken::new(),
        );
        session.turn("第一问").await.unwrap();

        let page = host.history("s-cold", None, 50).await.unwrap();
        let types: Vec<&str> = page.events.iter().map(|e| e.event.ty.as_str()).collect();
        assert!(types.contains(&"user/message"));
    }

    /// respond 路由:approve 判别 + not-pending
    #[tokio::test]
    async fn respond_routes_pending() {
        let host = temp_host("respond");
        let (tx, rx) = oneshot::channel();
        host.pending.lock().unwrap().insert(
            "q-rpc".into(),
            PendingQuestion {
                session_id: "s".into(),
                approve_label: "批准".into(),
                tx: Some(tx),
                answer_tx: None,
                frame: frame("question/requested", json!({})),
            },
        );
        let ok = RpcResult::Ok(json!({
            "answer": { "answers": [ { "id": "plan", "selected": ["批准"] } ] }
        }));
        let receipt = host.respond("q-rpc", &ok);
        assert!(receipt.accepted);
        assert!(matches!(rx.await, Ok(QuestionAnswer::Approve)));

        let unknown = host.respond("nope", &ok);
        assert!(!unknown.accepted);
        assert_eq!(unknown.reason.as_deref(), Some("not-pending"));
    }

    /// session.list:扫描会话根项目目录 + blank 判定 + 旧布局迁移
    #[tokio::test]
    async fn list_sessions_scans_files() {
        let host = temp_host("list");
        let proj = proj_dir(&host, &host.workspace);
        std::fs::create_dir_all(proj.join("a")).unwrap();
        std::fs::write(proj.join("a").join("session.jsonl"), "").unwrap();
        std::fs::create_dir_all(proj.join("b")).unwrap();
        std::fs::write(
            proj.join("b").join("session.jsonl"),
            "{\"type\":\"turn/start\",\"seq\":1,\"time\":0,\"data\":{},\"ignorable\":false}\n",
        )
        .unwrap();
        std::fs::write(host.workspace.join("ignored.txt"), "").unwrap();

        let items = host.list_sessions();
        let mut ids: Vec<&str> = items.iter().map(|s| s.session_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
        let blank_of = |id: &str| items.iter().find(|s| s.session_id == id).unwrap().blank;
        assert!(blank_of("a"));
        assert!(!blank_of("b"));

        // 旧布局迁移:工作区根遗留 s-*.jsonl 在 AppHost 构建时迁入会话根
        let ws2 =
            std::env::temp_dir().join(format!("dsh-core-list-migrate-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&ws2).unwrap();
        std::fs::write(ws2.join("s-legacy.jsonl"), "").unwrap();
        let sroot = std::env::temp_dir().join(format!(
            "dsh-core-list-migrate-sroot-{}",
            Uuid::new_v4().simple()
        ));
        let host2 =
            Arc::new(AppHost::new_at(ws2.clone(), true, "test-key", sroot.clone()).unwrap());
        let items2 = host2.list_sessions();
        assert!(items2.iter().any(|s| s.session_id == "s-legacy"));
        let migrated = sroot
            .join(project_key(&ws2.display().to_string()))
            .join("s-legacy")
            .join("session.jsonl");
        assert!(migrated.exists());
        assert!(!ws2.join("s-legacy.jsonl").exists());
    }

    /// setter 落盘工作区默认,重启(重建宿主)后冷会话沿用。
    /// 权限不再走工作区默认(已迁日志 fold),此处只验 model/preset/effort。
    #[tokio::test]
    async fn setter_persists_workspace_default_across_reload() {
        let host = temp_host("setpersist");
        host.ensure_models().await; // fake → demo 清单进缓存(set_model 校验依赖)
        host.set_model("s1", "deepseek-v4-pro").unwrap();
        host.set_preset("s1", "minimal").unwrap();
        host.set_effort("s1", "low").unwrap();
        // 内存覆盖即时生效
        assert_eq!(host.session_model("s1"), "deepseek-v4-pro");
        assert_eq!(host.session_preset("s1"), "minimal");
        assert_eq!(host.session_effort("s1").as_deref(), Some("low"));
        assert!(host.sessions_root.join("settings.json").exists());

        // 模拟重启:同会话根重建宿主——内存覆盖清零,设置层接管
        let host2 = AppHost::new_at(
            host.workspace.clone(),
            true,
            "test-key",
            host.sessions_root.clone(),
        )
        .unwrap();
        assert_eq!(host2.session_model("s1"), "deepseek-v4-pro");
        assert_eq!(host2.session_preset("s1"), "minimal");
        assert_eq!(host2.session_effort("s1").as_deref(), Some("low"));
    }

    /// 合并序:工作区 dsh.toml 显式值 > 设置层;会话内存覆盖仍最高
    #[tokio::test]
    async fn dsh_toml_beats_settings_layer() {
        let host = temp_host("tomlprec");
        host.ensure_models().await;
        host.set_model("s1", "deepseek-v4-pro").unwrap();
        std::fs::write(host.workspace.join("dsh.toml"), "model = \"toml-model\"\n").unwrap();
        assert_eq!(
            host.session_model("s1"),
            "deepseek-v4-pro",
            "会话内存覆盖最高"
        );
        let host2 = AppHost::new_at(
            host.workspace.clone(),
            true,
            "test-key",
            host.sessions_root.clone(),
        )
        .unwrap();
        assert_eq!(host2.session_model("s1"), "toml-model", "dsh.toml > 设置层");
    }

    /// provider 注册表 CRUD + 凭据录入(钥匙串/.env)+ 引用回写
    #[tokio::test]
    async fn provider_registry_and_credential_surface() {
        // 空 key 宿主(凭据链不走显式注入,四级链行为可见)
        let dir =
            std::env::temp_dir().join(format!("dsh-core-provreg-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let sroot = std::env::temp_dir().join(format!(
            "dsh-core-provreg-sroot-{}",
            Uuid::new_v4().simple()
        ));
        let host = Arc::new(AppHost::new_at(dir, true, "", sroot).unwrap());
        host.set_keychain(Arc::new(crate::credentials::InMemoryKeychain::new()));

        // 初始:内置 deepseek,各级缺席
        assert!(!host.credential_status("deepseek"));
        assert_eq!(host.default_model(), "deepseek-chat");

        // upsert 校验:非法方言 / 非法 id / 非法 base_url
        let mut bad = crate::settings::builtin_provider();
        bad.dialect = "nope".into();
        assert!(host.upsert_provider(bad).is_err());
        let mut bad = crate::settings::builtin_provider();
        bad.id = "Acme!".into();
        assert!(host.upsert_provider(bad).is_err());
        let mut bad = crate::settings::builtin_provider();
        bad.base_url = "ftp://x".into();
        assert!(host.upsert_provider(bad).is_err());

        // 新 provider + 钥匙串录入:引用回写、状态翻转
        let acme = ProviderEntry {
            id: "acme".into(),
            base_url: "https://acme.example/v1".into(),
            dialect: "openai-chat".into(),
            credential_ref: None,
            default_model: Some("acme-1".into()),
            ..crate::settings::builtin_provider()
        };
        host.upsert_provider(acme).unwrap();
        host.store_credential("acme", "sk-acme", CredentialStore::Keychain)
            .unwrap();
        assert!(host.credential_status("acme"));
        let stored = host
            .providers()
            .into_iter()
            .find(|p| p.id == "acme")
            .unwrap();
        assert_eq!(
            stored.credential_ref.as_deref(),
            Some("keychain:dsh/acme"),
            "录入后引用指向钥匙串默认槽"
        );

        // 工作区默认 provider 切到 acme:模型走 provider 默认、清单为其视角
        let ws_name = host.workspace_names()[0].clone();
        host.set_workspace_provider(&ws_name, "acme").unwrap();
        assert_eq!(host.default_model(), "acme-1");
        assert!(host.models().is_empty(), "acme 未探测,清单为空");

        // settings_view:状态可见、明文不可见
        let view = host.settings_view().to_string();
        assert!(view.contains("acme"));
        assert!(!view.contains("sk-acme"), "设置视图不得携带凭据明文");

        // 删除 provider:工作区引用悬空 → 内置回落
        host.remove_provider("acme").unwrap();
        assert_eq!(host.default_model(), "deepseek-chat");
        assert!(host.set_workspace_provider(&ws_name, "ghost").is_err());
    }

    /// 通用区偏好:默认 preset 落盘 + 校验 + 重启保留;默认权限为
    /// 常量(权限已迁日志 fold,不再有工作区默认权限设置)。
    #[test]
    fn default_preset_permission_roundtrip() {
        let host = temp_host("defpref");
        assert_eq!(host.default_preset(), "standard");
        assert_eq!(host.default_permission(), "workspace-write");
        host.set_default_preset("minimal").unwrap();
        assert!(host.set_default_preset("ghost").is_err());
        let host2 = AppHost::new_at(
            host.workspace.clone(),
            true,
            "test-key",
            host.sessions_root.clone(),
        )
        .unwrap();
        assert_eq!(host2.default_preset(), "minimal", "重启保留 preset");
        assert_eq!(host2.default_permission(), "workspace-write");
        assert_eq!(host2.settings_view()["defaultPreset"], "minimal");
        assert_eq!(
            host2.settings_view()["defaultPermission"],
            "workspace-write"
        );
    }

    /// 权限经日志 fold 跨重载持久:set_permission 写 sandbox/mode 事件落盘,
    /// 重建宿主后 session_permission 从磁盘 fold 读回(而非内存/设置覆盖)。
    #[tokio::test]
    async fn permission_log_fold_persists_across_reload() {
        let host = temp_host("permlog");
        let id = host.create_session(None, None, None);
        // 默认:无事件 fold 得 workspace-write
        assert_eq!(host.session_permission(&id), "workspace-write");
        host.set_permission(&id, "read-only").await.unwrap();
        // worker 异步落盘:等 sandbox/mode 事件上盘
        assert_eq!(wait_log_sandbox(&host, &id, "read-only").await, "read-only");
        assert_eq!(host.session_permission(&id), "read-only");

        // 模拟重启:同会话根重建宿主——权限值从磁盘日志 fold 恢复
        let host2 = AppHost::new_at(
            host.workspace.clone(),
            true,
            "test-key",
            host.sessions_root.clone(),
        )
        .unwrap();
        assert_eq!(
            host2.session_permission(&id),
            "read-only",
            "重启后 fold 日志还原"
        );
        assert_eq!(
            host2.session_approval(&id),
            "ask",
            "无 approval 事件默认 ask"
        );
    }

    /// 运行中切权限不再被拒:工具执行时动态 fold 日志,无重装配依赖,
    /// 事件落档即生效(旧实现在此拒绝「会话运行中,请先停止」)
    #[tokio::test]
    async fn set_permission_allowed_while_running() {
        let host = temp_host("perm-run");
        let id = host.create_session(None, None, None);
        host.get_slot(&id)
            .unwrap()
            .running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        host.set_permission(&id, "full-access").await.unwrap();
        assert_eq!(
            wait_log_sandbox(&host, &id, "full-access").await,
            "full-access"
        );
    }

    /// ZIP 导出(根 + fork 后代血缘序;解包校验条目集)
    #[tokio::test]
    async fn export_zip_contains_root_and_descendants() {
        let host = temp_host("zipexp");
        host.set_fake_script(script(&["内容"]));
        let mut mux = host.mux_subscribe();
        let parent = host.create_session(None, None, None);
        host.prompt(
            &parent,
            &[json!({ "type": "text", "text": "根会话" })],
            "queue",
        )
        .await
        .unwrap();
        recv_until(&mut mux, |f| {
            f.method == "session/event" && f.payload["event"]["type"] == "turn/end"
        })
        .await
        .expect("turn/end");
        let child = host.fork_session(&parent).unwrap();
        let grand = host.fork_session(&child).unwrap();

        // 仅根
        let bytes = host.export_session_zip(&parent, false).unwrap();
        let zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
        let names: Vec<String> = zip.file_names().map(String::from).collect();
        assert_eq!(names, vec!["session.jsonl"]);

        // 全血缘
        let bytes = host.export_session_zip(&parent, true).unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
        let mut names: Vec<String> = zip.file_names().map(String::from).collect();
        names.sort();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(names.contains(&format!("descendants/{child}/session.jsonl")));
        assert!(names.contains(&format!("descendants/{grand}/session.jsonl")));
        // 根条目内容 = 原日志
        let mut root = zip.by_name("session.jsonl").unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut root, &mut text).unwrap();
        assert!(text.contains("根会话"));
    }

    /// goal 六 RPC 全程(create/edit/complete/pause/resume/clear +
    /// 只读面),冷热两路落档
    #[tokio::test]
    async fn goal_rpc_roundtrip() {
        let host = temp_host("goalrpc");
        let id = host.create_session(None, None, None);

        // create(冷会话路径:未附着)
        let g1 = host.goal_create(&id, "发布 M2").unwrap();
        let g2 = host.goal_create(&id, "检索导出").unwrap();
        let state = host.goal_state(&id).unwrap();
        assert_eq!(state["goals"].as_array().unwrap().len(), 2);

        // edit + complete + pause/resume
        host.goal_edit(&id, g1["id"].as_u64().unwrap(), "发布 M2.1")
            .unwrap();
        host.goal_complete(&id, g1["id"].as_u64().unwrap()).unwrap();
        host.goal_set_paused(&id, g2["id"].as_u64().unwrap(), true)
            .unwrap();
        let state = host.goal_state(&id).unwrap();
        let goals = state["goals"].as_array().unwrap();
        assert_eq!(goals[0]["text"], "发布 M2.1");
        assert_eq!(goals[0]["done"], true);
        assert_eq!(goals[1]["paused"], true);

        // 冷重读(文件重建)
        host.goal_set_paused(&id, g2["id"].as_u64().unwrap(), false)
            .unwrap();
        let state = host.goal_state(&id).unwrap();
        assert_eq!(state["goals"].as_array().unwrap()[1]["paused"], false);

        // clear
        host.goal_clear(&id).unwrap();
        let state = host.goal_state(&id).unwrap();
        assert!(state["goals"].as_array().unwrap().is_empty());

        // 非法目标拒绝
        assert!(host.goal_edit(&id, 99, "x").is_err());
    }

    /// 命令目录(host 注册表)+ execute 分派(plan/model/goal/未知)
    #[tokio::test]
    async fn command_registry_and_execute() {
        let host = temp_host("cmds");
        let id = host.create_session(None, None, None);

        // 目录拉取(含名字/描述/hint)
        let cmds = host.command_list();
        let names: Vec<&str> = cmds.iter().map(|c| c.name).collect();
        assert!(names.contains(&"plan"));
        assert!(names.contains(&"export"));
        assert!(names.contains(&"goal"));
        assert!(names.contains(&"model"));

        // /model 空参 → 当前模型
        let v = host.execute_command(&id, "/model").await.unwrap();
        assert_eq!(v["kind"], "model");
        assert!(v["model"].as_str().is_some());

        // /plan on → set_mode(plan)
        let v = host.execute_command(&id, "/plan on").await.unwrap();
        assert_eq!(v["mode"], "plan");
        // /plan off → standard
        host.execute_command(&id, "/plan off").await.unwrap();

        // /goal 带参 → create;空参 → goal_state(goals)
        host.execute_command(&id, "/goal 发布 M3").await.unwrap();
        let state = host.goal_state(&id).unwrap();
        assert_eq!(state["goals"].as_array().unwrap().len(), 1);
        let v = host.execute_command(&id, "/goal").await.unwrap();
        assert!(!v["goals"].as_array().unwrap().is_empty());
        // /goal clear
        let v = host.execute_command(&id, "/goal clear").await.unwrap();
        assert_eq!(v["accepted"], true);
        let state = host.goal_state(&id).unwrap();
        assert!(state["goals"].as_array().unwrap().is_empty());

        // 未知命令 → 错
        let err = host.execute_command(&id, "/nope").await.unwrap_err();
        assert_eq!(err.code, "bad-request");
    }

    /// ask_user_question 阻塞链路(ask → question/requested → respond → resolve)
    #[tokio::test]
    async fn ask_questions_roundtrip() {
        let host = temp_host("ask");
        let id = host.create_session(None, None, None);
        let mut mux = host.mux_subscribe();
        let questions = vec![dsh_tools::QuestionItem {
            id: "q1".into(),
            question: "继续?".into(),
            header: Some("Confirm".into()),
            options: vec![dsh_tools::QuestionOption {
                label: "是".into(),
                description: Some("开始".into()),
            }],
            multi_select: false,
        }];
        // 后台发起 ask(阻塞 await 应答)
        let host2 = host.clone();
        let sid = id.clone();
        let ask_task = tokio::spawn(async move { host2.ask_questions(&sid, &questions).await });
        // 收 question/requested 帧
        let f = recv_until(&mut mux, |f| f.method == "question/requested")
            .await
            .expect("应收到 question/requested");
        assert_eq!(f.payload["questions"][0]["id"], "q1");
        let rpc_id = f.rpc_id.clone();
        // 应答
        let receipt = host.respond(
            &rpc_id,
            &RpcResult::Ok(json!({
                "sessionId": id,
                "answer": { "answers": [ { "id": "q1", "selected": ["是"] } ] },
            })),
        );
        assert!(receipt.accepted);
        // resolve = 工具结果 JSON
        let text = ask_task.await.unwrap().unwrap();
        assert!(text.contains("\"answers\""));
        assert!(text.contains("q1"));
        assert!(text.contains("是"));
    }

    /// encode_answers 校验(单选≤1/缺 id/非法形状)
    #[test]
    fn encode_answers_validates() {
        let ok = encode_answers(&json!({
            "answer": { "answers": [ { "id": "q1", "selected": ["是"] } ] },
        }))
        .unwrap();
        assert!(ok.contains("\"q1\""));
        // 单选超 1 → 拒
        assert!(
            encode_answers(&json!({
                "answer": { "answers": [ { "id": "q1", "selected": ["a", "b"] } ] },
            }))
            .is_err()
        );
        // 缺 id → 拒
        assert!(
            encode_answers(&json!({
                "answer": { "answers": [ { "selected": [] } ] },
            }))
            .is_err()
        );
    }

    /// 消息反馈 sidecar(put/delete CAS + 列表)
    #[test]
    fn message_feedback_store_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dsh-fb-{}", Uuid::new_v4().simple()));
        let store = MessageFeedbackStore::new(&dir);
        // 新增
        let it = store
            .put("s1", "m1", "positive", Some(" 很好 "), None)
            .unwrap();
        assert_eq!(it.note.as_deref(), Some("很好"));
        // CAS 更新(带正确 version)
        let upd = store
            .put("s1", "m1", "negative", Some("有问题"), Some(&it.version))
            .unwrap();
        assert_eq!(upd.rating, "negative");
        // 误用旧 version → conflict
        assert!(
            store
                .put("s1", "m1", "positive", None, Some(&it.version))
                .is_err()
        );
        // list 应有 1 条
        assert_eq!(store.list("s1").len(), 1);
        // delete(正确 version)
        let del = store.delete("s1", "m1", &upd.version).unwrap();
        assert_eq!(del.message_id, "m1");
        assert!(store.list("s1").is_empty());
        // 非法 rating
        assert!(store.put("s1", "m2", "meh", None, None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// subagent 会话化血缘(create_subagent_session → list_sessions 暴露 parent/origin)
    #[test]
    fn subagent_lineage_sessions() {
        let host = temp_host("sublin");
        let parent = host.create_session(None, None, None);
        let child = host.create_subagent_session(&parent);
        let list = host.list_sessions();
        let c = list
            .iter()
            .find(|s| s.session_id == child)
            .expect("子会话在列");
        assert_eq!(c.parent_session_id.as_deref(), Some(parent.as_str()));
        assert_eq!(c.origin.as_deref(), Some("subagent"));
        let p = list.iter().find(|s| s.session_id == parent).unwrap();
        assert!(p.parent_session_id.is_none());
    }

    /// 子代理权限 seed(delegation)——继承父显式 sandbox override,
    /// approval 钉 'never',均带 source:'delegation' 标记。
    #[tokio::test]
    async fn subagent_delegation_permission_seed() {
        let host = temp_host("subperm");
        let parent = host.create_session(None, None, None);
        host.set_permission(&parent, "read-only").await.unwrap();
        wait_log_sandbox(&host, &parent, "read-only").await;

        let child = host.create_subagent_session(&parent);
        // 继承父 sandbox(mode=read-only) + 审批钉 never
        assert_eq!(
            wait_log_sandbox(&host, &child, "read-only").await,
            "read-only"
        );
        assert_eq!(host.session_approval(&child), "never", "子代理审批钉 never");
        let path = host.session_log_path(&child);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"source\":\"delegation\""),
            "子代理权限事件带 delegation 标记"
        );
    }

    /// fork 血缘落档 + session_trace 祖先/后代
    #[tokio::test]
    async fn fork_lineage_trace() {
        let host = temp_host("lineage");
        host.set_fake_script(script(&["回复"]));
        let mut mux = host.mux_subscribe();
        let parent = host.create_session(None, None, None);
        host.prompt(
            &parent,
            &[json!({ "type": "text", "text": "父会话内容" })],
            "queue",
        )
        .await
        .unwrap();
        recv_until(&mut mux, |f| {
            f.method == "session/event" && f.payload["event"]["type"] == "turn/end"
        })
        .await
        .expect("turn/end");
        let child = host.fork_session(&parent).unwrap();

        let trace = host.session_trace(&child).unwrap();
        assert_eq!(trace["ancestors"].as_array().unwrap().len(), 1);
        assert_eq!(trace["ancestors"][0], parent);
        assert!(trace["children"].as_array().unwrap().is_empty());

        let trace = host.session_trace(&parent).unwrap();
        assert!(trace["ancestors"].as_array().unwrap().is_empty());
        assert_eq!(trace["children"].as_array().unwrap().len(), 1);
        assert_eq!(trace["children"][0], child);

        // 子日志可整读(新事件类型已登记,不拒读)
        let events = host.session_log(&child).unwrap();
        assert!(events.iter().any(|ev| ev.r#type == "session/forked"));
    }

    /// event_read 全文 + 邻居;event_trace 归因链
    #[tokio::test]
    async fn event_read_and_trace() {
        let host = temp_host("evread");
        host.set_fake_script(script(&["答案"]));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        host.prompt(&id, &[json!({ "type": "text", "text": "问题" })], "queue")
            .await
            .unwrap();
        recv_until(&mut mux, |f| {
            f.method == "session/event" && f.payload["event"]["type"] == "turn/end"
        })
        .await
        .expect("turn/end");

        // 注入事件(权限 pin 三件 + 快照 user/message+source.kind=plugin)
        // 使 seq 随内容前移——动态定位 user/message 而非硬编码
        let log = host.session_log(&id).unwrap();
        let useq = log
            .iter()
            .find(|ev| ev.r#type == "user/message")
            .map(|ev| ev.seq)
            .expect("应含 user/message");
        let out = host.event_read(&id, useq, 1, 2).unwrap();
        assert_eq!(out["event"]["type"], "user/message");
        assert_eq!(out["event"]["data"]["content"], "问题");
        assert_eq!(out["before"].as_array().unwrap().len(), 1);
        assert_eq!(out["after"].as_array().unwrap().len(), 2);

        // 归因:assistant/message 派生自其请求(源链非空或为空都合法——
        // 断言形状)
        let out = host.event_trace(&id, useq).unwrap();
        assert_eq!(out["seq"], useq);
        assert!(out["sourceEventSeqs"].is_array());

        assert!(host.event_read(&id, 999, 0, 0).is_err(), "越界 seq 拒绝");
    }

    /// 全库检索:fake turn 落档后命中(中文子串/工具调用),冷会话文件同样可检
    #[tokio::test]
    async fn search_sessions_hits_after_turn_and_cold_log() {
        let host = temp_host("search");
        host.set_fake_script(script(&["队列持久化已修复"]));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        host.prompt(
            &id,
            &[json!({ "type": "text", "text": "怎么修复队列持久化的 bug" })],
            "queue",
        )
        .await
        .unwrap();
        recv_until(&mut mux, |f| {
            f.method == "session/event" && f.payload["event"]["type"] == "turn/end"
        })
        .await
        .expect("turn/end");

        // 命中 user 文本(中文子串)
        let out = host.search_sessions("持久化", 10, None).await.unwrap();
        let hits = out["hits"].as_array().unwrap();
        assert!(
            hits.iter()
                .any(|h| h["kind"] == "user" && h["sessionId"] == id),
            "user 文本应命中: {out}"
        );

        // 命中 assistant 文本
        let out = host.search_sessions("修复", 10, None).await.unwrap();
        assert!(
            out["hits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["kind"] == "assistant")
        );

        // 空查询 → 空结果
        let out = host.search_sessions("  ", 10, None).await.unwrap();
        assert!(out["hits"].as_array().unwrap().is_empty());
    }

    /// 通用区偏好:busy_enter 落盘 + 校验 + 重启保留 + view 携带
    #[test]
    fn busy_enter_preference_roundtrip() {
        let host = temp_host("busyenter");
        assert_eq!(host.busy_enter(), "queue", "缺省排队");
        host.set_busy_enter("steer").unwrap();
        assert_eq!(host.busy_enter(), "steer");
        assert!(host.set_busy_enter("nope").is_err(), "非法值拒绝");
        let host2 = AppHost::new_at(
            host.workspace.clone(),
            true,
            "test-key",
            host.sessions_root.clone(),
        )
        .unwrap();
        assert_eq!(host2.busy_enter(), "steer", "重启保留");
        assert_eq!(host2.settings_view()["busyEnter"], "steer");
    }

    /// dotenv 录入目标(默认工作区 .env + 引用回写 + 链上可解析)
    #[test]
    fn credential_dotenv_target() {
        let host = temp_host("credenv");
        host.set_keychain(Arc::new(crate::credentials::InMemoryKeychain::new()));
        host.store_credential("deepseek", "sk-env", CredentialStore::Dotenv)
            .unwrap();
        assert!(host.workspace.join(".env").exists());
        let stored = host
            .providers()
            .into_iter()
            .find(|p| p.id == "deepseek")
            .unwrap();
        assert_eq!(
            stored.credential_ref.as_deref(),
            Some("dotenv:DEEPSEEK_API_KEY")
        );
        // temp_host 显式 key="test-key" 优先——空 key 宿主上验证链解析
        let ws = host.workspace.clone();
        let sroot = host.sessions_root.clone();
        let bare = AppHost::new_at(ws, true, "", sroot).unwrap();
        bare.set_keychain(Arc::new(crate::credentials::InMemoryKeychain::new()));
        assert!(bare.credential_status("deepseek"));
    }

    /// durable 队列:replay_inbox 折叠语义(入队/编辑/认领/转移)
    #[test]
    fn replay_inbox_folds_splices() {
        let splice = |target: &str, start: u64, removed: u64, inserted: &[(&str, &str)]| {
            EventEnvelope::new(
                "agent/inbox/spliced",
                0,
                json!({
                    "target": target, "start": start, "removedCount": removed,
                    "inserted": inserted
                        .iter()
                        .map(|(id, text)| json!({ "id": id, "content": text }))
                        .collect::<Vec<_>>(),
                }),
            )
        };
        let mut log = EventLog::new();
        log.append(splice("next-turn", 0, 0, &[("a", "1")]))
            .unwrap();
        log.append(splice("next-turn", 1, 0, &[("b", "2")]))
            .unwrap();
        log.append(splice("next-turn", 2, 0, &[("c", "3")]))
            .unwrap();
        // 编辑 b(原地替换)
        log.append(splice("next-turn", 1, 1, &[("b", "2-改")]))
            .unwrap();
        // 认领 a(驱动:removedCount 1,inserted 空)
        log.append(splice("next-turn", 0, 1, &[])).unwrap();
        // steer 转移 c:next-turn 移除 + next-step 追加
        log.append(splice("next-turn", 1, 1, &[])).unwrap();
        log.append(splice("next-step", 0, 0, &[("c", "3")]))
            .unwrap();
        let (pending, steer) = replay_inbox(&log);
        assert_eq!(pending.len(), 1, "仅剩编辑后的 b");
        assert_eq!(
            (pending[0].id.as_str(), pending[0].text.as_str()),
            ("b", "2-改")
        );
        assert_eq!(steer.len(), 1);
        assert_eq!((steer[0].id.as_str(), steer[0].text.as_str()), ("c", "3"));
        // 越界防御:start/removedCount 超界不 panic,截断处理
        let mut bad = EventLog::new();
        bad.append(splice("next-turn", 9, 5, &[("x", "x")]))
            .unwrap();
        let (p, s) = replay_inbox(&bad);
        assert_eq!(p.len(), 1);
        assert!(s.is_empty());
    }

    /// durable 队列:入队 splice 落盘;消费完毕后重启重建队列为空
    #[tokio::test]
    async fn durable_queue_roundtrip_consumed() {
        let host = temp_host("durable-rt");
        host.set_fake_script(script(&["ok"]));
        let mut mux = host.mux_subscribe();
        let id = host.create_session(None, None, None);
        host.prompt(
            &id,
            &[json!({ "type": "text", "text": "排队即消费" })],
            "queue",
        )
        .await
        .unwrap();
        recv_until(&mut mux, |f| {
            f.method == "session/event" && f.payload["event"]["type"] == "turn/end"
        })
        .await
        .expect("turn/end 应到达");
        // 入队 splice 已落盘且携带 inserted 内容(结构化断言——键序随
        // serde_json feature 合流变化,不依赖序列化顺序)
        let file = std::fs::read_to_string(host.session_log_path(&id)).unwrap();
        let first: serde_json::Value =
            serde_json::from_str(file.lines().next().unwrap_or("{}")).expect("首行应为合法 JSON");
        assert_eq!(first["type"], "agent/inbox/spliced", "首事件为入队 splice");
        assert_eq!(first["data"]["inserted"][0]["content"], "排队即消费");
        // 重启(重建宿主):消费完毕的队列重建后为空——不发 session/queue 帧
        let host2 = Arc::new(
            AppHost::new_at(
                host.workspace.clone(),
                true,
                "test-key",
                host.sessions_root.clone(),
            )
            .unwrap(),
        );
        let mut mux2 = host2.mux_subscribe();
        host2.history(&id, None, 50).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut saw_queue = false;
        while let Ok(f) = mux2.try_recv() {
            if f.method == "session/queue" {
                saw_queue = true;
            }
        }
        assert!(!saw_queue, "已消费队列重启后应重建为空(无队列帧)");
    }

    /// durable 队列:重启前未消费的排队条目冷附着重建,队列基线帧下发
    #[tokio::test]
    async fn durable_queue_rebuilds_pending_across_restart() {
        let host = temp_host("durable-pend");
        let id = host.create_session(None, None, None);
        // 会话文件直接落两条入队 splice(模拟「上一进程排队后退出」)
        let path = host.session_log_path(&id);
        let line = |seq: u64, mid: &str, text: &str| {
            json!({
                "type": "agent/inbox/spliced", "seq": seq, "time": 1,
                "data": { "target": "next-turn", "start": seq - 1, "removedCount": 0,
                          "inserted": [ { "id": mid, "content": text } ] },
            })
            .to_string()
        };
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                line(1, "q1", "重启保留 1"),
                line(2, "q2", "重启保留 2")
            ),
        )
        .unwrap();
        let mut mux = host.mux_subscribe();
        host.history(&id, None, 50).await.unwrap();
        let f = recv_until(&mut mux, |f| f.method == "session/queue")
            .await
            .expect("冷附着应下发队列基线帧");
        let items = f.payload["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "两条排队消息重建");
        assert_eq!(items[0]["message"]["content"][0]["text"], "重启保留 1");
        assert_eq!(items[1]["message"]["content"][0]["text"], "重启保留 2");
    }

    /// 工作区整理(排序/重命名/移除)落盘 + 重启保序 + 默认保护
    #[test]
    fn workspace_management_roundtrip() {
        let base = |p: &Path| p.file_name().unwrap().to_str().unwrap().to_string();
        let host = temp_host("wsmgmt");
        let ws2 = std::env::temp_dir().join(format!("dsh-core-ws2-{}", Uuid::new_v4().simple()));
        let ws3 = std::env::temp_dir().join(format!("dsh-core-ws3-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&ws2).unwrap();
        std::fs::create_dir_all(&ws3).unwrap();
        host.add_workspace(ws2.display().to_string().as_str())
            .unwrap();
        host.add_workspace(ws3.display().to_string().as_str())
            .unwrap();

        // 排序:ws3 移到 ws2 之前 → [默认, ws3, ws2]
        host.reorder_workspace(&base(&ws3), Some(&base(&ws2)))
            .unwrap();
        let names = host.workspace_names();
        assert_eq!(
            (names[1].as_str(), names[2].as_str()),
            (base(&ws3).as_str(), base(&ws2).as_str()),
            "reorder 生效"
        );
        // 锚点 None → 移到末尾
        host.reorder_workspace(&base(&ws3), None).unwrap();
        // 重命名(标题覆盖;身份不变)
        host.rename_workspace(&base(&ws3), "第三个").unwrap();
        assert_eq!(host.workspace_title(&base(&ws3)), "第三个");
        // 默认工作区不可移除
        let def = host.workspace_names()[0].clone();
        assert!(host.remove_workspace(&def).is_err());
        // 移除 ws2(标题清理 + 出清单)
        host.remove_workspace(&base(&ws2)).unwrap();

        // 重启:顺序 [默认, ws3] + 标题保留
        let host2 = AppHost::new_at(
            host.workspace.clone(),
            true,
            "test-key",
            host.sessions_root.clone(),
        )
        .unwrap();
        let names = host2.workspace_names();
        assert_eq!(names.len(), 2);
        assert_eq!(names[1], base(&ws3));
        assert_eq!(host2.workspace_title(&names[1]), "第三个");
        let _ = std::fs::remove_dir_all(&ws2);
        let _ = std::fs::remove_dir_all(&ws3);
    }

    /// 默认工作区权威 = 清单第 0 位(而非启动目录):裸 id 会话落第 0 位,
    /// 默认保护护第 0 位。锁「排序把启动目录挪离首位后新建归属仍正确」
    #[test]
    fn default_workspace_is_list_head() {
        let base = |p: &Path| p.file_name().unwrap().to_str().unwrap().to_string();
        let host = temp_host("ws-head");
        let launch = host.workspace().to_path_buf();
        let other = std::env::temp_dir().join(format!("dsh-core-wshead-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&other).unwrap();
        host.add_workspace(other.display().to_string().as_str())
            .unwrap();
        // other 移到启动目录之前 → [other, 启动目录]
        host.reorder_workspace(&base(&other), Some(&base(&launch)))
            .unwrap();
        assert_eq!(host.workspace_names()[0], base(&other), "other 已在首位");

        // 裸 id 会话落第 0 位工作区(而非启动目录)
        let id = host.create_session(None, None, None);
        assert!(!id.contains('/'), "默认区会话 id 为裸 stem");
        let log_dir = host
            .session_log_path(&id)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        assert_eq!(log_dir, proj_dir(&host, &other), "裸 id 落清单首位工作区");
        assert_ne!(
            log_dir,
            proj_dir(&host, &launch),
            "不再落启动目录(修复前即错在此)"
        );

        // 指定启动目录名创建 → 带前缀 id,物理落启动目录
        let pid = host.create_session(None, None, Some(base(&launch)));
        assert!(pid.starts_with(&format!("{}/", base(&launch))));
        assert_eq!(
            host.session_log_path(&pid)
                .parent()
                .unwrap()
                .parent()
                .unwrap(),
            proj_dir(&host, &launch)
        );

        // 默认保护护第 0 位;启动目录挪到非首位后可移除
        assert!(host.remove_workspace(&base(&other)).is_err());
        let _ = std::fs::remove_dir_all(&other);
    }

    /// 删除会话:日志与空会话目录一并移除(不留空壳目录;
    /// 空壳曾让各工作区目录堆积幽灵会话夹)
    #[test]
    fn delete_session_removes_empty_dir() {
        let host = temp_host("ws-del-dir");
        let id = host.create_session(None, None, None);
        let path = host.session_log_path(&id);
        assert!(path.exists());
        host.delete_session(&id).unwrap();
        assert!(!path.exists(), "日志应已删除");
        assert!(!path.parent().unwrap().exists(), "空会话目录残留");
    }

    /// 旧 `.dsh-workspaces.json` 一次性导入 settings(导入后旧文件
    /// 删除不影响后续重启)
    #[test]
    fn workspace_legacy_import() {
        let dir = std::env::temp_dir().join(format!("dsh-core-wsleg-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let other =
            std::env::temp_dir().join(format!("dsh-core-wsleg-b-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            dir.join(WORKSPACES_FILE),
            format!("[\"{}\"]", other.display()),
        )
        .unwrap();
        let sroot =
            std::env::temp_dir().join(format!("dsh-core-wsleg-sroot-{}", Uuid::new_v4().simple()));
        let host = AppHost::new_at(dir.clone(), true, "test-key", sroot.clone()).unwrap();
        let names = host.workspace_names();
        assert_eq!(names.len(), 2, "旧格式导入");
        // 导入已固化到 settings:旧文件删除后重启仍双工作区
        std::fs::remove_file(dir.join(WORKSPACES_FILE)).unwrap();
        let host2 = AppHost::new_at(dir, true, "test-key", sroot).unwrap();
        assert_eq!(host2.workspace_names().len(), 2, "settings 注册表接管");
        let _ = std::fs::remove_dir_all(&other);
    }

    /// 手工给子会话落档(测试造恢复现场):EventLog 赋 seq 后取回信封持久化
    fn seed_child_log(host: &AppHost, id: &str, evs: &[(&str, Value)]) {
        let path = host.slot_path(id);
        let backend = dsh_host::JsonlBackend::create(&path).unwrap();
        let mut log = EventLog::new();
        for (ty, data) in evs {
            let ev = EventEnvelope::new_ignorable(ty, 0, data.clone());
            if let Ok(seq) = log.append(ev)
                && let Some(envelope) = log.get(seq)
            {
                let _ = backend.append(envelope);
            }
        }
    }

    /// 录制型结算通知 port(直接断言文本与染色,不经父会话泵——
    /// 通知→泵→染色 turn 链路已由 subagent_settlement_notice 测试覆盖)
    #[derive(Default, Clone)]
    struct ResumeRecordingNotify {
        calls: Arc<Mutex<Vec<(String, String, Value)>>>,
    }
    impl dsh_tools::subagent::SettlementNotificationPort for ResumeRecordingNotify {
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

    /// 重启恢复三态:中断子会话(有 descriptor 无 settled)= 重挂 +
    /// 「已恢复」通知 + 可续话;已结算子会话 = 静默重挂;认领幂等,
    /// release 后可再恢复。
    #[tokio::test]
    async fn subagent_children_resume_after_restart() {
        let host = temp_host("subresume");
        let factory = SessionFactoryImpl(Arc::clone(&host));
        let parent = host.create_session(None, None, None);
        let interrupted_child = host.create_subagent_session(&parent);
        let settled_child = host.create_subagent_session(&parent);

        // 中断现场:descriptor + 开着 turn(无 settled)
        seed_child_log(
            &host,
            &interrupted_child,
            &[
                (
                    "subagent/descriptor",
                    serde_json::json!({
                        "label": "Interrupted task",
                        "mode": "continuable",
                        "parentSessionId": parent,
                        "prompt": "do it",
                    }),
                ),
                ("turn/start", serde_json::json!({})),
                ("user/message", serde_json::json!({ "content": "do it" })),
            ],
        );
        // 已结算现场:descriptor + 完整 turn + settled
        seed_child_log(
            &host,
            &settled_child,
            &[
                (
                    "subagent/descriptor",
                    serde_json::json!({
                        "label": "Done task",
                        "mode": "continuable",
                        "parentSessionId": parent,
                        "prompt": "did it",
                    }),
                ),
                ("turn/start", serde_json::json!({})),
                ("turn/end", serde_json::json!({})),
                (
                    "subagent/settled",
                    serde_json::json!({ "stopReason": "completed" }),
                ),
            ],
        );

        // 扫描:两子会话都返回;中断者 interrupted=true 且带 label
        let children =
            dsh_tools::subagent::SessionFactory::resumable_children(&factory, &parent);
        assert_eq!(children.len(), 2);
        let intr = children
            .iter()
            .find(|(h, ..)| h.session_id == interrupted_child)
            .expect("中断子会话在列");
        assert!(intr.1, "末事件非 settled = 中断");
        assert_eq!(intr.2, "Interrupted task");
        let st = children
            .iter()
            .find(|(h, ..)| h.session_id == settled_child)
            .expect("已结算子会话在列");
        assert!(!st.1, "已 settled = 静默重挂");

        // 认领幂等:扫描即认领,重复扫描不再返回(防双挂)
        assert!(
            dsh_tools::subagent::SessionFactory::resumable_children(&factory, &parent).is_empty(),
            "已认领的子会话不得重复返回"
        );

        // release 后可再恢复(驻留退出的生命周期)
        dsh_tools::subagent::SessionFactory::release_child(&factory, &interrupted_child);
        let again = dsh_tools::subagent::SessionFactory::resumable_children(&factory, &parent);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].0.session_id, interrupted_child);
        assert!(again[0].1);
        // 此前各次扫描已把两子会话都认领;模拟上一驻留(进程)退出再放行,
        // 恢复链才有可领项
        dsh_tools::subagent::SessionFactory::release_child(&factory, &interrupted_child);
        dsh_tools::subagent::SessionFactory::release_child(&factory, &settled_child);

        // 全链重挂:SubagentTool + resume_children → 注册表即刻有两个驻留,
        // 中断者向父投「已恢复」通知(已结算者静默)
        let notify = ResumeRecordingNotify::default();
        let transport: dsh_tools::subagent::TransportFactory<dsh_llm::FakeProvider> = {
            // 两个驻留各消费一次工厂调用(重挂即建传输);组内容相同,
            // 无论启动顺序如何,续话 turn 的回复一致
            let script = vec![
                vec![LlmEvent::AssistantMessage(serde_json::json!({
                    "content": "resumed reply",
                }))];
                2
            ];
            let slot = Arc::new(Mutex::new(script));
            Arc::new(move || {
                // 每次工厂调用发一组脚本(= 一个子代理的传输)
                let group = slot.lock().unwrap().pop();
                let mut p = dsh_llm::FakeProvider::new();
                if let Some(response) = group {
                    p.then(response);
                }
                Ok(p)
            })
        };
        let mut tool = dsh_tools::subagent::SubagentTool::new(
            host.sessions_root.clone(),
            transport,
            "m".into(),
        )
        .with_session_factory(Arc::new(factory))
        .with_parent_id(&parent)
        .with_notify(Arc::new(notify.clone()));
        tool.resume_children();

        // 两子会话都已重挂为可续话驻留
        wait_registry_idle(&tool, 2).await;

        // 「已恢复」通知恰一条:发给父、含 interrupted 摘要、kind 染色
        for _ in 0..1000 {
            if notify.calls.lock().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        {
            let calls = notify.calls.lock().unwrap();
            assert_eq!(calls.len(), 1, "已结算者静默,只有中断者通知");
            let (p, text, source) = &calls[0];
            assert_eq!(p, &parent);
            assert!(text.contains("was interrupted by a host restart"), "{text}");
            assert_eq!(source["kind"], "subagent-settled");
            assert_eq!(
                source["senderSessionId"].as_str(),
                Some(interrupted_child.as_str())
            );
        }

        // 续话:send_message 到重挂的中断者 → 新 turn(脚本回复)→ 结算通知
        let mut control = dsh_tools::subagent::SubagentControlTool::new(tool.registry.clone());
        let sent = dsh_agent_loop::ToolPort::execute(
            &mut control,
            &dsh_agent_loop::ToolCallRequest {
                name: "send_message".into(),
                arguments: serde_json::json!({
                    "agent_id": interrupted_child,
                    "message": "wrap up"
                }),
            },
        )
        .await;
        assert!(sent.success, "{}", sent.output);
        for _ in 0..1000 {
            if notify.calls.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        {
            let calls = notify.calls.lock().unwrap();
            let (_, text, _) = &calls[1];
            assert!(text.contains("resumed reply"), "{text}");
            assert!(text.contains("finished and will do no further work"), "{text}");
        }

        // 子日志:续话 turn 落档 + settled 标记追加(下次重启=已结算态)
        let events = dsh_host::persistence::jsonl::load_jsonl(&host.slot_path(&interrupted_child))
            .expect("子日志可整份解码");
        assert_eq!(
            events
                .iter()
                .filter(|e| e.r#type == "turn/start")
                .count(),
            2,
            "重挂前 1 个 + 续话 1 个 turn"
        );
        assert_eq!(
            events.last().map(|e| e.r#type.as_str()),
            Some("subagent/settled"),
            "结算标记落档"
        );
        
    }

    /// 轮询等注册表有 n 个 idle 驻留(重挂异步)
    async fn wait_registry_idle(
        tool: &dsh_tools::subagent::SubagentTool<dsh_llm::FakeProvider>,
        n: usize,
    ) {
        for _ in 0..1000 {
            let ready = {
                let r = tool.registry.lock().unwrap();
                r.len() == n && r.iter().all(|x| x.status == "idle")
            };
            if ready {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("驻留未在预算内就绪");
    }

    /// jobs 帧:接线后后台委派 → 注册表状态变化即广播 session/jobs
    /// (委派=running,结算=completed);未接线的注册表不广播。
    #[tokio::test]
    async fn subagent_jobs_frame_broadcasts_on_status_change() {
        let host = temp_host("subjobs");
        let parent = host.create_session(None, None, None);
        let transport: dsh_tools::subagent::TransportFactory<dsh_llm::FakeProvider> = {
            let script = vec![vec![LlmEvent::AssistantMessage(serde_json::json!({
                "content": "bg done",
            }))]];
            let slot = Arc::new(Mutex::new(script));
            Arc::new(move || {
                let group = slot.lock().unwrap().pop();
                let mut p = dsh_llm::FakeProvider::new();
                if let Some(response) = group {
                    p.then(response);
                }
                Ok(p)
            })
        };
        let notify = ResumeRecordingNotify::default();
        let mut tool = dsh_tools::subagent::SubagentTool::new(
            host.sessions_root.clone(),
            transport,
            "m".into(),
        )
        .with_parent_id(&parent)
        .with_notify(Arc::new(notify));
        AppHost::bind_jobs(&host, &parent, tool.registry.clone());
        let mut mux = host.mux_subscribe();

        // 后台委派:委派帧(running)即时到达
        let out = dsh_agent_loop::ToolPort::execute(
            &mut tool,
            &dsh_agent_loop::ToolCallRequest {
                name: "subagent".into(),
                arguments: serde_json::json!({ "description": "Bg task", "prompt": "work" }),
            },
        )
        .await;
        assert!(out.success, "{}", out.output);
        let mut seen_running = false;
        for _ in 0..1000 {
            match mux.try_recv() {
                Ok(f) if f.method == "session/jobs" => {
                    assert_eq!(f.payload["sessionId"], serde_json::json!(parent));
                    let jobs = f.payload["jobs"].as_array().expect("jobs 数组");
                    assert_eq!(jobs.len(), 1);
                    if jobs[0]["status"] == "running" {
                        assert_eq!(jobs[0]["label"], "Bg task");
                        assert_eq!(jobs[0]["kind"], "subagent");
                        assert!(jobs[0]["startedAt"].is_i64(), "startedAt 在场");
                        seen_running = true;
                        break;
                    }
                }
                Ok(_) => continue,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
            }
        }
        assert!(seen_running, "委派后未收到 running jobs 帧");

        // 结算帧(completed + 可继续 detail + 计时终点)
        let mut seen_completed = None;
        for _ in 0..1000 {
            match mux.try_recv() {
                Ok(f) if f.method == "session/jobs" => {
                    let jobs = f.payload["jobs"].as_array().unwrap();
                    if jobs[0]["status"] == "completed" {
                        seen_completed = Some(jobs[0].clone());
                        break;
                    }
                }
                Ok(_) => continue,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
            }
        }
        let job = seen_completed.expect("结算后未收到 completed jobs 帧");
        assert_eq!(job["detail"], "可继续", "驻留 idle 映射 completed+可继续");
        assert!(job["finishedAt"].is_i64());
        assert_eq!(job["prompt"], "work");
    }

    /// 子会话实时流:驻留子代理的引擎事件经 relay 翻译后,以
    /// 子会话 id 的 session/event 帧广播(桌面切换视图即见流式)
    #[tokio::test]
    async fn subagent_child_events_stream_to_mux() {
        let host = temp_host("substream");
        let parent = host.create_session(None, None, None);
        let transport: dsh_tools::subagent::TransportFactory<dsh_llm::FakeProvider> = {
            let script = vec![vec![LlmEvent::AssistantMessage(serde_json::json!({
                "content": "streamed child reply",
            }))]];
            let slot = Arc::new(Mutex::new(script));
            Arc::new(move || {
                let group = slot.lock().unwrap().pop();
                let mut p = dsh_llm::FakeProvider::new();
                if let Some(response) = group {
                    p.then(response);
                }
                Ok(p)
            })
        };
        let notify = ResumeRecordingNotify::default();
        let host_for_sink = Arc::clone(&host);
        let mut tool = dsh_tools::subagent::SubagentTool::new(
            host.sessions_root.clone(),
            transport,
            "m".into(),
        )
        .with_parent_id(&parent)
        .with_notify(Arc::new(notify))
        .with_event_sink(Arc::new(move |sid: &str, ev: &dsh_session::EventEnvelope| {
            host_for_sink.relay_subagent_event(sid, ev);
        }));
        let mut mux = host.mux_subscribe();

        let out = dsh_agent_loop::ToolPort::execute(&mut tool, &{
            dsh_agent_loop::ToolCallRequest {
                name: "subagent".into(),
                arguments: serde_json::json!({ "description": "Stream task", "prompt": "work" }),
            }
        })
        .await;
        assert!(out.success, "{}", out.output);
        let child = out
            .output
            .strip_prefix("started subagent ")
            .unwrap()
            .to_string();

        // 子会话 id 的 turn/start 与 assistant/message 实时帧到达
        let mut seen_turn = false;
        let mut seen_reply = false;
        for _ in 0..2000 {
            match mux.try_recv() {
                Ok(f) if f.method == "session/event"
                    && f.payload["sessionId"] == serde_json::json!(child) =>
                {
                    let ty = f.payload["event"]["type"].as_str().unwrap_or_default();
                    if ty == "turn/start" {
                        seen_turn = true;
                    }
                    // 客方形状:content 在 message.content 块数组(translate 包裹)
                    if ty == "assistant/message"
                        && f.payload["event"]["data"]["message"]["content"]
                            .as_array()
                            .is_some_and(|blocks| {
                                blocks.iter().any(|b| {
                                    b["text"].as_str().is_some_and(|t| t.contains("streamed child reply"))
                                })
                            })
                    {
                        seen_reply = true;
                    }
                    if seen_turn && seen_reply {
                        break;
                    }
                }
                Ok(_) => continue,
                Err(broadcast::error::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await
                }
                Err(_) => continue,
            }
        }
        assert!(seen_turn, "子会话 turn/start 实时帧未达");
        assert!(seen_reply, "子会话 assistant/message 实时帧未达");
    }

    /// 宿主打断:任务面板行直呼 interrupt_subagent → 运行中子代理
    /// 当前 turn 中断(结算 aborted 通知)+ 保持可续话;重复打断 = false
    #[tokio::test]
    async fn host_interrupt_subagent_stops_running_child() {
        let host = temp_host("substop");
        let parent = host.create_session(None, None, None);
        // 门控 bash:打断即时生效,不等命令结束
        let go = std::env::temp_dir().join(format!("dsh-stop-{}.go", Uuid::new_v4().simple()));
        let transport: dsh_tools::subagent::TransportFactory<dsh_llm::FakeProvider> = {
            let group = vec![vec![LlmEvent::AssistantMessage(serde_json::json!({
                "content": "",
                "tool_calls": [ {
                    "name": "bash",
                    "arguments": {
                        "command": format!(
                            "until [ -f \"{}\" ]; do sleep 0.05; done",
                            go.display()
                        ),
                        "description": "Gate",
                    },
                } ],
            }))]];
            let slot = Arc::new(Mutex::new(group));
            Arc::new(move || {
                let response = slot.lock().unwrap().pop();
                let mut p = dsh_llm::FakeProvider::new();
                if let Some(events) = response {
                    p.then(events);
                }
                Ok(p)
            })
        };
        let notify = ResumeRecordingNotify::default();
        let mut tool = dsh_tools::subagent::SubagentTool::new(
            host.sessions_root.clone(),
            transport,
            "m".into(),
        )
        .with_parent_id(&parent)
        .with_notify(Arc::new(notify.clone()));
        AppHost::bind_jobs(&host, &parent, tool.registry.clone());
        let out = dsh_agent_loop::ToolPort::execute(&mut tool, &{
            dsh_agent_loop::ToolCallRequest {
                name: "subagent".into(),
                arguments: serde_json::json!({ "description": "Stop task", "prompt": "work" }),
            }
        })
        .await;
        let child = out
            .output
            .strip_prefix("started subagent ")
            .unwrap()
            .to_string();
        // 等 bash 执行中(注册表 running)
        for _ in 0..1000 {
            let running = tool
                .registry
                .background_records()
                .iter()
                .any(|r| r.session_id == child && r.status == "running");
            if running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // 宿主打断:即时生效 → aborted 结算通知 + 保持可续话(idle)
        assert!(host.interrupt_subagent(&child), "打断应命中运行中子代理");
        for _ in 0..1000 {
            if notify.calls.lock().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        {
            let calls = notify.calls.lock().unwrap();
            assert_eq!(calls.len(), 1, "打断应恰一次结算通知");
            let (_, text, _) = &calls[0];
            assert!(text.contains("was stopped before it finished"), "{text}");
        }
        for _ in 0..1000 {
            let idle = tool
                .registry
                .background_records()
                .iter()
                .any(|r| r.session_id == child && r.status == "idle");
            if idle {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // 已 idle:重复打断不命中(无事可停,不得残留信号误伤下一轮)
        assert!(!host.interrupt_subagent(&child), "idle 子代理无可打断");
        let _ = std::fs::remove_file(&go);
    }
}
