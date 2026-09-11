//! 强类型事件载荷:判别联合,编译期收口。
//!
//! 登记最小事件集合:surface 三件(user/message、assistant/message、tool/result)+
//! turn/step 骨架 + assistant/chunk(ignorable,流式重建用)。
//! 新事件类型 = 在此处加变体 + 登记 [`KNOWN_EVENT_TYPES`];
//! 「模型可见 ⟺ 已记录」要求新模型可见输入必须配套新事件。

use serde::{Deserialize, Serialize};

/// user/message 载荷。所有「进对话流的东西」都是 user/message,
/// 靠 `source.kind` 区分——`kind === "user"` 是真实用户消息,其余
/// (plugin / agent-instructions / session-reference / skill-invocation 等)
/// 是注入上下文。`source` 是**旁车元数据**(仅客户端/轨迹渲染用,
/// 不进模型消息体——见 [`message_from_event`]),`id` 是宿主预分配 v7。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    /// 用户输入内容(注入时为模型可见的注入文本)
    pub content: String,
    /// 消息 id(宿主预分配 v7;旧日志缺失时回退合成)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 来源染色:{ kind: "user"|"plugin"|"agent-instructions"|..., form, ...producer 扩展 }
    #[serde(default = "default_user_source")]
    pub source: serde_json::Value,
}

/// 真实用户消息的默认 source(缺失时按用户消息处理)
fn default_user_source() -> serde_json::Value {
    serde_json::json!({ "kind": "user" })
}

/// assistant/message 载荷
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// 文本内容
    pub content: String,
}

/// tool/result 载荷
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// 对应的 tool/call 序号(引用链目标)
    pub call: u64,
    /// 工具输出
    pub output: String,
}

/// tool/call 载荷(模型发起的工具调用)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// 工具名
    pub name: String,
    /// 调用参数(JSON)
    pub arguments: serde_json::Value,
}

/// assistant/chunk 载荷(ignorable;seq 连续含原始 chunk,继承语义)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantChunk {
    /// 增量文本
    pub delta: String,
}

/// audit/call 载荷(E5 审计事件溯源化):宿主记录的跨边界调用。
///
/// 审计与 session 日志**一套机制**——审计记录本身入事件流,
/// 归因走与 surface 事件统一的 `sourceEventSeqs` 引用链。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditCall {
    /// 跨越的边界:`llm`(出网请求)/ `tool`(工具执行)/ `process`(子进程)
    pub boundary: String,
    /// 操作名(llm 为 "request",tool 为工具名,process 为程序名)
    pub operation: String,
    /// 边界特有细节(模型名/调用 seq 等;载荷不重复日志已有内容)
    pub detail: serde_json::Value,
}

/// 单条 todo 任务(todo_write 全量列表条目;{content, status}
/// 无 id —— 模型每次整表重写,身份即内容)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    /// 任务内容(非空短句)
    pub content: String,
    /// 状态:pending / in_progress / completed
    pub status: String,
}

/// todo/write 载荷:全量任务列表快照,整表替换语义(非 surface;
/// 模型经工具结果可见,崩溃恢复 = 读最近一条 todo/write)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoWrite {
    /// 全量任务(当前权威状态)
    pub todos: Vec<TodoItem>,
}

/// session/mode 载荷:会话模式切换(standard / plan)。
///
/// 模式影响模型可见 prompt → 必须入日志,否则重放不一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMode {
    /// 目标模式:standard / plan
    pub mode: String,
}

/// permission/preset 载荷:会话选中的权限预设名(带可选 origin)。
///
/// 预设把 sandbox mode + approval policy 捆绑,记录「用户当初选的哪个预设」,
/// 使两值相同时重放仍能还原用户意图。
/// 非 surface,不进模型 transcript;预设值经由 knob 事件(sandbox/mode、
/// approval/policy)控制执行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionPreset {
    /// 预设名(如 workspace-write / full-access;custom 为派生态非可切换项)
    pub preset: String,
    /// 选择来源:default / selection / inferred;旧日志缺失 = 运行时值
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// sandbox/mode 载荷:会话沙箱访问模式切换。
///
/// 非 surface,不进模型 transcript;执行侧(build_tools)与模型侧
/// (runtime-context 快照文本)都据此 fold 当前模式。`source:'delegation'`
/// 标记子代理委托时从父 seed 的 override。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxMode {
    /// 沙箱模式:read-only / workspace-write / full-access
    pub mode: String,
    /// 委托标记(子代理 seed;缺失 = 运行时切换)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// approval/policy 载荷:会话审批策略切换。
///
/// 非 surface,不进模型 transcript;模型经 runtime-context 快照文本得知
/// 策略(模型从快照 + 实时切换通知学习策略)。
/// `source:'delegation'` 标记子代理委托时钉为 never(子不得扩大权限)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalPolicy {
    /// 审批策略:ask / never
    pub policy: String,
    /// 委托标记(子代理 seed;缺失 = 运行时切换)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// approval/asked 载荷:工具请求沙箱升级,审批闸门已向用户发起问询。
///
/// 与 approval/decided 以 id 配对成审计对;log-only,不进模型 transcript;
/// 必须被 open turn 包住(闸门仅在工具执行中发起,闲时拒绝不落档)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalAsked {
    /// 审批请求 id(每次请求新生成,与 decided 配对)
    pub id: String,
    /// 发起工具名(bash / …)
    pub tool_name: String,
    /// 工具调用 id(缺失 = 无关联调用)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// 审批事由(自包含:`escalate sandbox to <mode>: <justification>`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// approval/decided 载荷:审批裁决(与 asked 以 id 配对收口)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalDecided {
    /// 与 asked 配对的请求 id
    pub id: String,
    /// 裁决:allowed-once / rejected / cancelled / unavailable
    pub outcome: String,
}

/// 会话分叉记录(非 surface)。分叉 = 复制父日志后追加本事件,
/// 血缘(source session_query)由此链重建
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionForked {
    /// 父会话 id(ws/stem)
    pub parent: String,
}

/// plan/submitted 载荷:模型经 exit_plan_mode 提交的计划
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanSubmitted {
    /// 计划正文(markdown)
    pub plan: String,
}

/// plan/approved 载荷:用户批准的计划(批准后进入 active plan)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanApproved {
    /// 计划正文(markdown)
    pub plan: String,
}

/// 单条目标
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalItem {
    /// 目标标识(单调递增分配)
    pub id: u64,
    /// 目标描述
    pub text: String,
    /// 是否达成
    pub done: bool,
    /// 是否暂停(宿主 RPC 面 goal.pause/resume;模型面工具
    /// 不产此态,缺省 false 兼容旧日志)
    #[serde(default)]
    pub paused: bool,
}

/// goal/state 载荷:全量目标快照(非 surface,恢复 = 读最近一条)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalState {
    /// 全量目标(当前权威状态)
    pub goals: Vec<GoalItem>,
}

/// compaction/summary 载荷:历史折叠摘要。
///
/// 折叠是一次性出网调用(非会话面,经 Summarizer 端口)的结果落档:
/// 派生 = 摘要消息 + through_seq 之后事件的正常派生。重放读此事件,
/// 不重调摘要(确定性重放的前提)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionSummary {
    /// 折叠摘要正文
    pub summary: String,
    /// 折叠覆盖到的事件 seq(不含;其后事件照常派生)
    pub through_seq: u64,
}

/// turn/error 载荷:turn 异常终止(传输失败/悬挂超时等)的落档。
/// 非 surface(不进派生面);UI 据此显示通告并闭合回合。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnError {
    /// 错误摘要(人类可读)
    pub error: String,
    /// 稳定错误码(TRANSPORT/TIMEOUT/SERVER/RATE_LIMIT/EMPTY_RESPONSE/
    /// AUTH/INVALID_REQUEST/OTHER/INTERNAL;缺席 = 旧日志,按 TRANSPORT 处理)
    #[serde(default)]
    pub code: Option<String>,
}

/// llm/retry 载荷:一次模型请求重试的排定(llm-retry 语义;
/// 非 surface,消息流折叠行的数据源)。每次失败尝试后落一条,
/// 随后的 llm/retry-started 标记退避结束、重发开始。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmRetry {
    /// 第几次重试(1 起)
    pub retry: u32,
    /// 重试上限(策略值)
    pub max_retries: u32,
    /// 本次退避时长(毫秒;倒计时数据源)
    pub delay_ms: u64,
    /// 失败分类码(TransportError::code)
    pub code: String,
    /// 失败摘要(人类可读)
    pub message: String,
}

/// llm/retry-started 载荷:退避结束、重发开始(非 surface)。
/// 消息流行据此把「等待重试」翻转为「已重试」终态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmRetryStarted {
    /// 对应 llm/retry 的次序
    pub retry: u32,
}

/// 事件载荷判别联合。
///
/// 未登记类型的载荷以 [`SessionEventData::Other`] 原样保留
/// (ignorable 前置已在 [`crate::envelope::decode_envelope`] 守卫)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum SessionEventData {
    /// 会话开启(元数据)
    #[serde(rename = "session/start", rename_all = "camelCase")]
    SessionStart,
    /// turn 开始
    #[serde(rename = "turn/start", rename_all = "camelCase")]
    TurnStart,
    /// step 开始
    #[serde(rename = "step/start", rename_all = "camelCase")]
    StepStart,
    /// 用户消息(surface 事件;source.kind 区分真实用户/注入上下文)
    #[serde(rename = "user/message", rename_all = "camelCase")]
    UserMessage(UserMessage),
    /// 助手消息(surface 事件)
    #[serde(rename = "assistant/message", rename_all = "camelCase")]
    AssistantMessage(AssistantMessage),
    /// 工具结果(surface 事件)
    #[serde(rename = "tool/result", rename_all = "camelCase")]
    ToolResult(ToolResult),
    /// step 结束
    #[serde(rename = "step/end", rename_all = "camelCase")]
    StepEnd,
    /// turn 结束
    #[serde(rename = "turn/end", rename_all = "camelCase")]
    TurnEnd,
    /// turn 异常终止(传输失败等;step/start 未闭合的兜底闭合)
    #[serde(rename = "turn/error", rename_all = "camelCase")]
    TurnError(TurnError),
    /// 工具调用(非 surface;其结果 tool/result 是 surface 事件)
    #[serde(rename = "tool/call", rename_all = "camelCase")]
    ToolCall(ToolCall),
    /// 助手流式 chunk(ignorable)
    #[serde(rename = "assistant/chunk", rename_all = "camelCase")]
    AssistantChunk(AssistantChunk),
    /// 跨边界调用审计(E5;归因引用链的目标读者)
    #[serde(rename = "audit/call", rename_all = "camelCase")]
    AuditCall(AuditCall),
    /// todo 任务列表全量快照(todo_write 整表替换;非 surface)
    #[serde(rename = "todo/write", rename_all = "camelCase")]
    TodoWrite(TodoWrite),
    /// 历史折叠摘要(非 surface,派生面特殊处理)
    #[serde(rename = "compaction/summary", rename_all = "camelCase")]
    CompactionSummary(CompactionSummary),
    /// 会话模式切换(非 surface,prompt 组装读取)
    #[serde(rename = "session/mode", rename_all = "camelCase")]
    SessionMode(SessionMode),
    /// 会话分叉记录(非 surface,血缘重建)
    #[serde(rename = "session/forked", rename_all = "camelCase")]
    SessionForked(SessionForked),
    /// 权限预设选择(非 surface;knob 事件前的意图记录)
    #[serde(rename = "permission/preset", rename_all = "camelCase")]
    PermissionPreset(PermissionPreset),
    /// 沙箱访问模式切换(非 surface;执行侧与快照文本 fold 读取)
    #[serde(rename = "sandbox/mode", rename_all = "camelCase")]
    SandboxMode(SandboxMode),
    /// 审批策略切换(非 surface;快照文本 fold 读取)
    #[serde(rename = "approval/policy", rename_all = "camelCase")]
    ApprovalPolicy(ApprovalPolicy),
    /// 审批发起(非 surface;闸门审计对,与 decided 以 id 配对)
    #[serde(rename = "approval/asked", rename_all = "camelCase")]
    ApprovalAsked(ApprovalAsked),
    /// 审批裁决(非 surface;审计对收口)
    #[serde(rename = "approval/decided", rename_all = "camelCase")]
    ApprovalDecided(ApprovalDecided),
    /// 模型提交的计划(非 surface)
    #[serde(rename = "plan/submitted", rename_all = "camelCase")]
    PlanSubmitted(PlanSubmitted),
    /// 用户批准的计划(非 surface,prompt 组装读取)
    #[serde(rename = "plan/approved", rename_all = "camelCase")]
    PlanApproved(PlanApproved),
    /// 用户取消/驳回计划(非 surface;计划归档状态用)
    #[serde(rename = "plan/cancelled", rename_all = "camelCase")]
    PlanCancelled(PlanSubmitted),
    /// 目标列表全量快照(非 surface)
    #[serde(rename = "goal/state", rename_all = "camelCase")]
    GoalState(GoalState),
    /// 模型请求重试排定(llm-retry;非 surface)
    #[serde(rename = "llm/retry", rename_all = "camelCase")]
    LlmRetry(LlmRetry),
    /// 重试退避结束、重发开始(非 surface)
    #[serde(rename = "llm/retry-started", rename_all = "camelCase")]
    LlmRetryStarted(LlmRetryStarted),
    /// 未登记类型(ignorable 守卫已过,原样保留)
    #[serde(untagged)]
    Other(serde_json::Value),
}

/// 已登记事件类型表(读取方守卫依据,known-event-types 语义)
pub const KNOWN_EVENT_TYPES: &[&str] = &[
    "session/start",
    "turn/start",
    "step/start",
    "user/message",
    "assistant/message",
    "tool/result",
    "tool/call",
    "step/end",
    "turn/end",
    // turn 异常终止(LLM 请求悬挂/失败此前无任何落档,
    // 日志悬着未闭合的 step/start 且 UI 零反馈)
    "turn/error",
    "assistant/chunk",
    "audit/call",
    "todo/write",
    "compaction/summary",
    "session/mode",
    "plan/submitted",
    "plan/approved",
    "plan/cancelled",
    "goal/state",
    // 队列/steer 认领(web 宿主;engine 在 step 边界落档,UI 据 removed
    // ids 分类 steering 节点)——漏登记会导致含 splice
    // 的会话日志整体拒读
    "agent/inbox/spliced",
    "session/forked",
    // 权限预设/沙箱模式/审批策略(非 surface,可重放的策略状态)
    "permission/preset",
    "sandbox/mode",
    "approval/policy",
    // 沙箱升级审批对(闸门审计;新增类型对旧日志安全)
    "approval/asked",
    "approval/decided",
    // hooks 桥事件对(hook 钩子调用与裁决;log-only、turn 封闭;
    // 新增类型对旧日志安全——旧日志无此类型,守卫只拒「未登记且非
    // ignorable」。漏登记会导致含 hook 对的会话日志整体拒读 +
    // turso FTS 索引同步失败,2026-09-12 现场实测)
    "hook/invoked",
    "hook/result",
    // LLM 请求重试(llm-retry 语义;新增类型对旧日志
    // 安全——旧日志无此类型,守卫只拒「未登记且非 ignorable」)
    "llm/retry",
    "llm/retry-started",
];

/// surface 事件类型(携带 surfaceOp 的类别:用户面三件 + 注入上下文)
pub const SURFACE_EVENT_TYPES: &[&str] = &["user/message", "assistant/message", "tool/result"];

/// 可归因事件类型:允许携带 `sourceEventSeqs` 引用链的集合。
///
/// E5 统一归因:surface 三件 + audit/call。审计事件不是模型可见面
/// (`message_from_event` 返回 None),但共享同一引用链机制。
pub const ATTRIBUTED_EVENT_TYPES: &[&str] = &[
    "user/message",
    "assistant/message",
    "tool/result",
    "audit/call",
];

impl SessionEventData {
    /// 事件类型名(判别键)
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::SessionStart => "session/start",
            Self::TurnStart => "turn/start",
            Self::StepStart => "step/start",
            Self::UserMessage(_) => "user/message",
            Self::AssistantMessage(_) => "assistant/message",
            Self::ToolResult(_) => "tool/result",
            Self::StepEnd => "step/end",
            Self::TurnEnd => "turn/end",
            Self::TurnError(_) => "turn/error",
            Self::ToolCall(_) => "tool/call",
            Self::AssistantChunk(_) => "assistant/chunk",
            Self::AuditCall(_) => "audit/call",
            Self::TodoWrite(_) => "todo/write",
            Self::CompactionSummary(_) => "compaction/summary",
            Self::SessionMode(_) => "session/mode",
            Self::SessionForked(_) => "session/forked",
            Self::PermissionPreset(_) => "permission/preset",
            Self::SandboxMode(_) => "sandbox/mode",
            Self::ApprovalPolicy(_) => "approval/policy",
            Self::ApprovalAsked(_) => "approval/asked",
            Self::ApprovalDecided(_) => "approval/decided",
            Self::PlanSubmitted(_) => "plan/submitted",
            Self::PlanApproved(_) => "plan/approved",
            Self::PlanCancelled(_) => "plan/cancelled",
            Self::GoalState(_) => "goal/state",
            Self::LlmRetry(_) => "llm/retry",
            Self::LlmRetryStarted(_) => "llm/retry-started",
            Self::Other(_) => "",
        }
    }

    /// 是否 surface 事件(surfaceOp 的合法性依据)
    pub fn is_surface(&self) -> bool {
        SURFACE_EVENT_TYPES.contains(&self.type_name())
    }

    /// 是否可归因事件(sourceEventSeqs 的合法性依据)
    pub fn is_attributed(&self) -> bool {
        ATTRIBUTED_EVENT_TYPES.contains(&self.type_name())
    }
}

/// 消息面折叠规则:单个事件 → 模型可见消息(非消息面事件返回 None)。
///
/// 这是 deriveMessages 的唯一权威实现:guest 投影、宿主派生、不变式比对
/// 三方共用。
pub fn message_from_event(type_name: &str, data: &serde_json::Value) -> Option<serde_json::Value> {
    match type_name {
        // 模型可见面 = role:user + content。source 是旁车元数据,不进模型
        // 消息体——真实用户(默认)与注入上下文(source.kind != "user")外观一致,
        // 客户端凭 source.kind 区分渲染。
        "user/message" => Some(serde_json::json!({
            "role": "user",
            "content": data["content"],
        })),
        "assistant/message" => {
            let mut m = serde_json::json!({
                "role": "assistant",
                "content": data["content"],
            });
            if let Some(tc) = data.get("tool_calls") {
                m["tool_calls"] = tc.clone();
            }
            Some(m)
        }
        "tool/result" => {
            let mut m = serde_json::json!({
                "role": "tool",
                "output": data["output"],
                "call": data["call"],
                "id": data["id"],
            });
            // 结果图片(MCP 图片桥;零字节持久引用)。请求期翻译由方言层
            // 处理(chat/anthropic 带图,responses 维持降级),缺席 = 无图
            if let Some(imgs) = data
                .get("images")
                .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
            {
                m["images"] = imgs.clone();
            }
            Some(m)
        }
        // audit/call 不进消息面:审计面向归因重放,模型不可见
        _ => None,
    }
}

/// 从事件序列派生模型可见消息(保序;跳过非消息面事件)。
///
/// `Session.deriveMessages` 语义:模型历史 = 日志投影。
pub fn derive_messages<'a>(
    events: impl Iterator<Item = &'a crate::EventEnvelope>,
) -> serde_json::Value {
    serde_json::Value::Array(
        events
            .filter_map(|ev| message_from_event(&ev.r#type, &ev.data))
            .collect(),
    )
}

/// tool/result 输出裁剪阈值(超阈值 head/tail 截断)。
/// 常量而非配置:派生投影必须跨重放稳定,常量零配置面。
pub const PRUNE_THRESHOLD_CHARS: usize = 8192;
/// 裁剪保留的头部字符数
pub const PRUNE_HEAD_CHARS: usize = 4096;
/// 裁剪保留的尾部字符数
pub const PRUNE_TAIL_CHARS: usize = 1024;

/// 超长工具输出裁剪(head/tail 保真,中段注明省略量)
pub fn prune_output(output: &str) -> String {
    let total = output.chars().count();
    if total <= PRUNE_THRESHOLD_CHARS {
        return output.to_string();
    }
    let head: String = output.chars().take(PRUNE_HEAD_CHARS).collect();
    let tail: String = output.chars().skip(total - PRUNE_TAIL_CHARS).collect();
    let omitted = total - PRUNE_HEAD_CHARS - PRUNE_TAIL_CHARS;
    format!("{head}\n…[pruned {omitted} chars]…\n{tail}")
}

/// 模型可见消息 = 日志投影 + 显式策略栈。
///
/// 策略栈:① tool/result 输出裁剪(常量,确定性);② 历史折叠——最近一条
/// compaction/summary 之前的事件折叠为单条摘要消息,其后照常派生;
/// ③ skill 目录替换——`source.kind=skill-catalog` 的 user/message 只保留
/// 最新一条(源在 pre-step 决策里物理移除旧目录,模型恒只见一份;日志
/// 只追加,这里以纯派生规则达成同一可见语义,重放稳定)。
/// engine 的请求构造与闸门的期望比对**共用本函数**(唯一实现);
/// derive_messages 保留为无策略的裸映射(审计/测试用)。
pub fn derive_visible_messages<'a>(
    events: impl Iterator<Item = &'a crate::EventEnvelope>,
) -> serde_json::Value {
    let events: Vec<&crate::EventEnvelope> = events.collect();
    let last_catalog_seq = events
        .iter()
        .rev()
        .find_map(|e| (e.r#type == "user/message" && is_skill_catalog(&e.data)).then_some(e.seq));
    let folded = events
        .iter()
        .rev()
        .find(|e| e.r#type == "compaction/summary")
        .map(|e| {
            (
                e.data["throughSeq"].as_u64().unwrap_or(0),
                e.data["summary"].as_str().unwrap_or_default().to_string(),
            )
        });

    let superseded_catalog = |e: &&crate::EventEnvelope| -> bool {
        Some(e.seq) < last_catalog_seq && is_skill_catalog(&e.data)
    };

    let mut msgs: Vec<serde_json::Value> = Vec::new();
    if let Some((through, summary)) = folded {
        if through > 0 {
            msgs.push(serde_json::json!({
                "role": "user",
                "content": format!("<session-summary>\n{summary}\n</session-summary>"),
            }));
        }
        for ev in events
            .iter()
            .filter(|e| e.seq > through && !superseded_catalog(e))
        {
            push_visible(&mut msgs, ev);
        }
    } else {
        for ev in events.into_iter().filter(|e| !superseded_catalog(e)) {
            push_visible(&mut msgs, ev);
        }
    }
    pair_dangling_tool_calls(&mut msgs);
    serde_json::Value::Array(msgs)
}

/// skill 目录事件判定(user/message + source.kind=skill-catalog)
fn is_skill_catalog(data: &serde_json::Value) -> bool {
    data["source"]["kind"].as_str() == Some("skill-catalog")
}

/// 悬空 tool_calls 的占位结果:turn 中断/异常退出时工具未执行完,
/// 日志里的 assistant{tool_calls} 缺对应 tool/result——provider 协议
/// 要求每个 tool_call_id 都有响应消息,否则 400(实测)。
/// 占位内容为常量(纯日志派生,重放稳定,不破坏「模型可见 ⟺ 已记录」)。
pub const DANGLING_TOOL_PLACEHOLDER: &str = "(tool call interrupted; no result recorded)";

/// 派生后处理:assistant{tool_calls} 后紧跟的 tool 消息(按 provider
/// id 匹配;id 空时按位置计数兜底)未覆盖的调用,补占位 tool 消息。
fn pair_dangling_tool_calls(msgs: &mut Vec<serde_json::Value>) {
    let mut i = 0;
    while i < msgs.len() {
        let Some(calls) = msgs[i].get("tool_calls").and_then(|c| c.as_array()) else {
            i += 1;
            continue;
        };
        if calls.is_empty() {
            i += 1;
            continue;
        }
        // 紧随的连续 tool 消息(下一非 tool 消息即边界)
        let mut j = i + 1;
        let mut answered: std::collections::HashSet<&str> = Default::default();
        while j < msgs.len() && msgs[j].get("role").and_then(|r| r.as_str()) == Some("tool") {
            if let Some(id) = msgs[j].get("id").and_then(|v| v.as_str())
                && !id.is_empty()
            {
                answered.insert(id);
            }
            j += 1;
        }
        let run_len = j - (i + 1);
        let mut slots = run_len; // 位置兜底额度(供 id 空的调用消耗)
        let mut synth: Vec<serde_json::Value> = Vec::new();
        for call in calls {
            let id = call["id"].as_str().unwrap_or_default().to_string();
            let is_answered = if id.is_empty() {
                slots > 0 && {
                    slots -= 1;
                    true
                }
            } else {
                answered.contains(id.as_str())
            };
            if !is_answered {
                synth.push(serde_json::json!({
                    "role": "tool",
                    "call": serde_json::Value::Null,
                    "id": id,
                    "output": DANGLING_TOOL_PLACEHOLDER,
                    "success": false,
                }));
            }
        }
        if !synth.is_empty() {
            for (k, s) in synth.into_iter().enumerate() {
                msgs.insert(j + k, s);
            }
        }
        i = j;
    }
}

/// 单事件 → 可见消息(含 tool/result 裁剪)
fn push_visible(msgs: &mut Vec<serde_json::Value>, ev: &crate::EventEnvelope) {
    let Some(mut m) = message_from_event(&ev.r#type, &ev.data) else {
        return;
    };
    if ev.r#type == "tool/result"
        && let serde_json::Value::String(output) = &mut m["output"]
    {
        *output = prune_output(output);
    }
    msgs.push(m);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tagged_roundtrip() {
        let payload = SessionEventData::UserMessage(UserMessage {
            content: "hi".into(),
            id: Some("m-1".into()),
            source: json!({ "kind": "user" }),
        });
        let v = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(v["type"], "user/message");
        assert_eq!(v["data"]["content"], "hi");
        assert_eq!(v["data"]["source"]["kind"], "user");
        let back: SessionEventData = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, payload);
    }

    #[test]
    fn every_variant_registered() {
        // 判别联合的每个具名变体都必须登记在 KNOWN_EVENT_TYPES(守卫完整性)
        for name in [
            "session/start",
            "turn/start",
            "step/start",
            "user/message",
            "assistant/message",
            "tool/result",
            "step/end",
            "turn/end",
            "assistant/chunk",
            "audit/call",
            "todo/write",
            "permission/preset",
            "sandbox/mode",
            "approval/policy",
        ] {
            assert!(KNOWN_EVENT_TYPES.contains(&name), "未登记:{name}");
        }
    }

    #[test]
    fn user_message_injection_roundtrip_and_registration() {
        // 注入上下文 = user/message + source.kind != "user"(无独立
        // context/message 事件,唯一分类权威是 source.kind)
        let payload = SessionEventData::UserMessage(UserMessage {
            content: "AGENTS.md 全文".into(),
            id: Some("ctx-1".into()),
            source: json!({ "kind": "agent-instructions", "form": "instructions" }),
        });
        let v = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(v["type"], "user/message");
        assert_eq!(v["data"]["content"], "AGENTS.md 全文");
        assert_eq!(v["data"]["source"]["kind"], "agent-instructions");
        let back: SessionEventData = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, payload);
        assert_eq!(payload.type_name(), "user/message");
        assert!(KNOWN_EVENT_TYPES.contains(&payload.type_name()));
    }

    /// 权限三事件(permission/preset + sandbox/mode + approval/policy):
    /// ①tagged roundtrip;②登记 KNOWN;③log-only(非 surface 非 attributed);
    /// ④非消息面(message_from_event = None);⑤delegation source 保留。
    #[test]
    fn permission_events_roundtrip_registration_and_log_only() {
        let preset = SessionEventData::PermissionPreset(PermissionPreset {
            preset: "workspace-write".into(),
            origin: Some("selection".into()),
        });
        let sandbox = SessionEventData::SandboxMode(SandboxMode {
            mode: "full-access".into(),
            source: Some("delegation".into()),
        });
        let approval = SessionEventData::ApprovalPolicy(ApprovalPolicy {
            policy: "never".into(),
            source: Some("delegation".into()),
        });

        for (data, ty) in [
            (&preset, "permission/preset"),
            (&sandbox, "sandbox/mode"),
            (&approval, "approval/policy"),
        ] {
            let v = serde_json::to_value(data).expect("serialize");
            assert_eq!(v["type"], ty);
            let back: SessionEventData = serde_json::from_value(v).expect("deserialize");
            assert_eq!(&back, data);
            assert_eq!(data.type_name(), ty);
            assert!(KNOWN_EVENT_TYPES.contains(&ty), "未登记:{ty}");
            // log-only:非 surface 非 attributed
            assert!(!data.is_surface(), "{ty} 不应是 surface");
            assert!(!data.is_attributed(), "{ty} 不应是可归因事件");
            // 非消息面
            assert!(
                message_from_event(ty, &serde_json::to_value(data).unwrap()).is_none(),
                "{ty} 不应派生出模型消息"
            );
        }

        // 缺省 origin/source 序列化省略,旧日志可读
        let preset_no_origin = SessionEventData::PermissionPreset(PermissionPreset {
            preset: "x".into(),
            origin: None,
        });
        let v = serde_json::to_value(&preset_no_origin).expect("serialize");
        assert!(v["data"].get("origin").is_none(), "缺省 origin 应省略");
        // 仅带 mode 无 source 的 sandbox 也可读
        let sandbox_bare = json!({ "type": "sandbox/mode", "data": { "mode": "read-only" } });
        let back: SessionEventData = serde_json::from_value(sandbox_bare).expect("bare sandbox");
        assert_eq!(back.type_name(), "sandbox/mode");
    }

    #[test]
    fn user_message_projects_to_user_role_without_source() {
        // 模型可见面 = role:user + content;source 是旁车,不进消息体
        // —— 真实用户与注入上下文外观一致,客户端凭 source.kind 区分渲染
        for kind in ["user", "plugin", "agent-instructions"] {
            let m = message_from_event(
                "user/message",
                &json!({ "content": "ctx-text", "id": "ctx-1", "source": { "kind": kind } }),
            )
            .expect("模型可见");
            assert_eq!(m["role"], "user");
            assert_eq!(m["content"], "ctx-text");
            assert!(m.get("source").is_none(), "source 不进模型消息体");
        }
    }

    #[test]
    fn surface_classification() {
        assert!(
            SessionEventData::UserMessage(UserMessage {
                content: String::new(),
                id: None,
                source: json!({ "kind": "user" }),
            })
            .is_surface()
        );
        assert!(!SessionEventData::TurnStart.is_surface());
    }

    fn envelope(t: &str, seq: u64, data: serde_json::Value) -> crate::EventEnvelope {
        let mut e = crate::EventEnvelope::new(t, 0, data);
        e.seq = seq;
        e
    }

    #[test]
    fn prune_keeps_head_and_tail_with_marker() {
        let short = "x".repeat(PRUNE_THRESHOLD_CHARS);
        assert_eq!(prune_output(&short), short, "不超阈值原样返回");
        let long = "a".repeat(PRUNE_HEAD_CHARS) + &"b".repeat(9000) + &"c".repeat(PRUNE_TAIL_CHARS);
        let pruned = prune_output(&long);
        assert!(pruned.starts_with(&"a".repeat(PRUNE_HEAD_CHARS)));
        assert!(pruned.ends_with(&"c".repeat(PRUNE_TAIL_CHARS)));
        assert!(pruned.contains("[pruned "));
        assert!(pruned.chars().count() < long.chars().count());
    }

    #[test]
    fn derive_visible_prunes_tool_outputs() {
        let long = "z".repeat(PRUNE_THRESHOLD_CHARS + 100);
        let evs = [
            envelope("user/message", 1, json!({ "content": "hi" })),
            envelope(
                "tool/result",
                2,
                json!({ "output": long, "call": 1, "id": "" }),
            ),
        ];
        let visible = derive_visible_messages(evs.iter());
        let arr = visible.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(arr[1]["output"].as_str().unwrap().contains("[pruned "));
        // 裸映射不裁剪(审计/测试对照面)
        let raw = derive_messages(evs.iter());
        assert!(
            raw.as_array().unwrap()[1]["output"].as_str().unwrap().len() > PRUNE_THRESHOLD_CHARS
        );
    }

    /// skill 目录替换:同 kind 的 user/message 派生时只保留最新一条
    /// (源在 pre-step 决策里移除旧目录;日志只追加,派生层同一语义)。
    /// 普通注入消息不受影响;保留的是最新一条而非「非空的」。
    #[test]
    fn derive_keeps_only_latest_skill_catalog() {
        let evs = [
            envelope("user/message", 1, json!({ "content": "hi" })),
            envelope(
                "user/message",
                2,
                json!({
                    "content": "old catalog",
                    "source": { "kind": "skill-catalog", "form": "catalog",
                        "entries": [ { "name": "a", "description": "d" } ] },
                }),
            ),
            envelope(
                "user/message",
                3,
                json!({
                    "content": "replacement catalog",
                    "source": { "kind": "skill-catalog", "form": "catalog", "update": true,
                        "entries": [ { "name": "b", "description": "d" } ] },
                }),
            ),
            envelope(
                "user/message",
                4,
                json!({
                    "content": "injected",
                    "source": { "kind": "agent-instructions", "form": "instructions" },
                }),
            ),
        ];
        let arr = derive_visible_messages(evs.iter())
            .as_array()
            .unwrap()
            .clone();
        let contents: Vec<&str> = arr.iter().filter_map(|m| m["content"].as_str()).collect();
        assert_eq!(
            contents,
            vec!["hi", "replacement catalog", "injected"],
            "旧目录被替换,普通注入不受影响"
        );

        // 全删墓碑(空 entries 的最新目录)同样替换旧目录
        let evs2 = [
            envelope(
                "user/message",
                1,
                json!({
                    "content": "first",
                    "source": { "kind": "skill-catalog", "form": "catalog",
                        "entries": [ { "name": "a", "description": "d" } ] },
                }),
            ),
            envelope(
                "user/message",
                2,
                json!({
                    "content": "tombstone",
                    "source": { "kind": "skill-catalog", "form": "catalog", "update": true,
                        "entries": [] },
                }),
            ),
        ];
        let arr2 = derive_visible_messages(evs2.iter())
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(arr2.len(), 1);
        assert_eq!(arr2[0]["content"], "tombstone");
    }

    /// 悬空 tool_calls(中断/异常退出的回合)派生时补占位 tool 消息:
    /// provider 协议要求每个 tool_call_id 都有响应,否则 400
    /// (实测,「回合出错」现场)
    #[test]
    fn derive_pairs_dangling_tool_calls() {
        let evs = [
            envelope("user/message", 1, json!({ "content": "go" })),
            envelope(
                "assistant/message",
                2,
                json!({
                    "content": "",
                    "tool_calls": [
                        { "id": "call_a", "name": "bash", "arguments": "{}" },
                        { "id": "call_b", "name": "file_read", "arguments": "{}" },
                    ],
                }),
            ),
            // 仅 call_a 有结果(call_b 被中断)
            envelope(
                "tool/result",
                3,
                json!({ "output": "ok", "call": 5, "id": "call_a" }),
            ),
            envelope("turn/error", 4, json!({ "error": "中断" })),
            envelope("user/message", 5, json!({ "content": "next" })),
        ];
        let visible = derive_visible_messages(evs.iter());
        let arr = visible.as_array().unwrap();
        // user → assistant → tool(call_a) → 占位(call_b) → user
        assert_eq!(arr.len(), 5, "缺一个响应即补一个占位");
        assert_eq!(arr[3]["role"], "tool");
        assert_eq!(arr[3]["id"], "call_b");
        assert_eq!(arr[3]["output"], DANGLING_TOOL_PLACEHOLDER);
        assert_eq!(arr[4]["role"], "user");
    }

    /// 全配对的回合不受后处理影响;末尾悬空(日志截断)同样补齐
    #[test]
    fn derive_pairing_leaves_complete_turns_alone() {
        let evs = [
            envelope("user/message", 1, json!({ "content": "go" })),
            envelope(
                "assistant/message",
                2,
                json!({
                    "content": "",
                    "tool_calls": [ { "id": "call_a", "name": "bash", "arguments": "{}" } ],
                }),
            ),
            envelope(
                "tool/result",
                3,
                json!({ "output": "ok", "call": 5, "id": "call_a" }),
            ),
            envelope("user/message", 4, json!({ "content": "next" })),
        ];
        let arr = derive_visible_messages(evs.iter())
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(arr.len(), 4);
        assert!(!arr.iter().any(|m| m["output"] == DANGLING_TOOL_PLACEHOLDER));

        // 末尾悬空(进程被杀,日志停在 tool_calls)
        let truncated = [
            envelope("user/message", 1, json!({ "content": "go" })),
            envelope(
                "assistant/message",
                2,
                json!({
                    "content": "",
                    "tool_calls": [ { "id": "call_x", "name": "bash", "arguments": "{}" } ],
                }),
            ),
        ];
        let arr2 = derive_visible_messages(truncated.iter())
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(arr2.len(), 3, "末尾悬空也要闭合");
        assert_eq!(arr2[2]["id"], "call_x");
    }

    #[test]
    fn derive_visible_folds_behind_summary() {
        let evs = [
            envelope("user/message", 1, json!({ "content": "old question" })),
            envelope("assistant/message", 2, json!({ "content": "old answer" })),
            envelope(
                "compaction/summary",
                3,
                json!({ "summary": "asked and answered", "throughSeq": 2 }),
            ),
            envelope("user/message", 4, json!({ "content": "new question" })),
        ];
        let visible = derive_visible_messages(evs.iter());
        let arr = visible.as_array().unwrap();
        assert_eq!(arr.len(), 2, "折叠后 = 摘要消息 + 其后事件");
        assert!(
            arr[0]["content"]
                .as_str()
                .unwrap()
                .contains("<session-summary>\nasked and answered")
        );
        assert_eq!(arr[1]["content"], "new question");

        // 多级折叠:取最近一条 summary
        let chained = [
            envelope("user/message", 1, json!({ "content": "oldest" })),
            envelope(
                "compaction/summary",
                2,
                json!({ "summary": "first", "throughSeq": 1 }),
            ),
            envelope("user/message", 3, json!({ "content": "later" })),
            envelope(
                "compaction/summary",
                4,
                json!({ "summary": "second", "throughSeq": 3 }),
            ),
            envelope("user/message", 5, json!({ "content": "current" })),
        ];
        let visible = derive_visible_messages(chained.iter());
        let arr = visible.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(arr[0]["content"].as_str().unwrap().contains("second"));
        assert_eq!(arr[1]["content"], "current");
    }
}
