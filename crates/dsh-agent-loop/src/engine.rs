//! turn 引擎:驱动一次 turn 的完整事件序列。
//!
//! 事件序列(无工具路径):
//! turn/start → user/message → step/start → (chunk* → assistant/message) → step/end → turn/end
//!
//! 记录优先:每个事件先 append 进 [`EventLog`] 并触发 `sink`(持久化),
//! 再进入后续处理——「模型可见 ⟺ 已记录」在 loop 侧的体现是
//! 出网请求的消息**只能**来自日志派生(derive_messages)。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::cancel::CancelToken;
use crate::runtime_context::{ContextSection, RuntimeContextProjection};
use dsh_session::audit::{BOUNDARY_LLM, BOUNDARY_TOOL, audit_call_event};
use dsh_session::events::{derive_visible_messages, message_from_event};
use dsh_session::{EventEnvelope, EventLog};
use serde_json::Value;
use thiserror::Error;

use crate::RequestHeader;
use crate::retry::RetryPolicy;
use crate::transport::{LlmEvent, LlmTransport, TransportError};

/// 循环相位(对等 WIT `dsh:loop/driver` phase;最小集)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 空闲
    Idle,
    /// turn 进行中
    Running,
    /// 已停止(取消或完成后的终态,直到下一次 send 复位)
    Stopped,
}

/// turn 结果
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    /// 助手最终消息内容
    pub assistant_message: String,
    /// 本 turn 消费的事件 seq 区间(含端点)
    pub seq_range: (u64, u64),
}

/// loop 错误
#[derive(Debug, Error, PartialEq)]
pub enum LoopError {
    /// 传输失败(分类见 [`TransportError`];重试耗尽/不可重试才到此处)
    #[error("transport: {0}")]
    Transport(TransportError),
    /// 日志层错误(seq 连续等)
    #[error("log: {0}")]
    Log(String),
    /// phase 违例(turn 进行中又 send)
    #[error("invalid phase transition: running 中不可再 send")]
    Busy,
    /// 取消令牌在安全点触发(软取消;turn/end 已记录 cancelled 归因)
    #[error("cancelled")]
    Cancelled,
}

/// 历史折叠预算:可见消息 JSON 字符数超此值触发折叠。
/// 上下文窗口按 1M tokens 计,窗口内不折叠——预算 = 1M token 的
/// 中文字符估算(4 字/token);超窗口才折叠兜底
pub const FOLD_BUDGET_CHARS: usize = 4_000_000;
/// 折叠保留的最近消息条数(近期工具往返不折)
pub const FOLD_KEEP_MESSAGES: usize = 6;

/// 流式消费累积态(回调侧零借用:事件经 channel,消费侧在主帧处理)
#[derive(Default)]
struct StreamAcc {
    assistant_text: String,
    final_message: Option<Value>,
    usage: Option<Value>,
    /// 流内失败帧(provider 以数据帧报错;引擎终态判定用)
    failure: Option<Value>,
}

impl StreamAcc {
    /// 本次尝试是否已向日志落过内容帧(chunk/物化消息)——重试前
    /// 据此决定是否需要 `assistant/stream-reset` 丢弃标记
    fn has_streamed_content(&self) -> bool {
        !self.assistant_text.is_empty() || self.final_message.is_some()
    }
}

/// 一条运行中 turn 的中途输入(steer;宿主在提交时分配持久 id,
/// 与队列行 / 认领 splice 共用同一 id —— UI 据此把消息渲染为 steering)
#[derive(Debug, Clone)]
pub struct SteerInput {
    /// 消息 id(持久;与 session/queue 帧行、agent/inbox/spliced 一致)
    pub id: String,
    /// 文本内容
    pub text: String,
    /// 图片附件(准入在宿主 prompt 侧完成,此处只携带持久引用)
    pub images: Vec<dsh_session::attachments::ImageAttachmentRef>,
    /// 来源染色(None = 真实用户 steer)。subagent 结算通知等宿主
    /// 内部注入携带 `source.kind`(如 "subagent-settled"),随 user/message
    /// 落档,客户端凭此分流渲染(以此字段为唯一分类权威)。
    pub source: Option<serde_json::Value>,
}

/// turn 引擎。状态 = 日志 + phase + inbox;跨 turn 保留日志,phase 回 Idle。
pub struct LoopEngine {
    log: Arc<Mutex<EventLog>>,
    phase: Phase,
    inbox: Vec<String>,
    header: RequestHeader,
    /// 上次审计落档完整快照的 header(深比对;None = 尚未落过)。
    /// 信封不变时不重复携带 systemPrompt/tools,防日志膨胀
    /// (headerEquals 去重)
    logged_header: Option<RequestHeader>,
    /// 软取消令牌(安全点检查:step 边界、工具执行前;默认永不取消)
    cancel: CancelToken,
    /// 运行中 turn 的中途输入缓冲(steer;None = 未装配 steer 支持)
    steer_buf: Option<Arc<Mutex<VecDeque<SteerInput>>>>,
    /// 下一 turn 真实用户消息的来源染色(None = 真实用户)。宿主认领
    /// 带 source 的条目(如 subagent 结算通知)时设置,run_turn 消费——
    /// 宿主每次认领必设(有则 Some、无则 None),不存在跨 turn 残留。
    pending_input_source: Option<serde_json::Value>,
    /// 历史折叠预算字符数(可调小测试;默认 [`FOLD_BUDGET_CHARS`])
    fold_budget_chars: usize,
    /// 会话级 runtime-context 投影(retained 快照去重;每步 project,折叠命中失效)。
    /// 引擎跨 turn 保留,替代 driver 的 `last_runtime_snapshot` 单文本比对。
    projection: RuntimeContextProjection,
    /// 每步渲染 runtime-context 快照的回调(宿主注入;返回渲染后的
    /// `(current 文本, sections)`;None = 无快照)。渲染权策略在宿主侧
    /// (dsh-core permission),引擎只管投影去重 + 落档。
    context_provider: Option<ContextProvider>,
    /// workspace 指令(AGENTS.md)每步重扫回调(宿主注入;入参 = 上一步
    /// 工具触碰的路径,返回完整注入 user/message 载荷或 None)。差分/版本
    /// 缓存全在宿主 InstructionRuntimeState,引擎只管按序落档。
    instructions_provider: Option<InstructionsProvider>,
    /// skill 目录每步回调(宿主注入;digest 幂等在宿主,变化才 Some)。
    /// 排在 runtime 快照之后、手势之前。
    skill_catalog_provider: Option<SkillCatalogProvider>,
    /// `/name` 手势注入回调(宿主注入;入参 = 本步用户面消息文本,
    /// 返回注入载荷列表,排在全部注入最后)。
    skill_gesture_provider: Option<SkillGestureProvider>,
    /// 本 turn 内待下探的触碰路径(file_read/file_edit 的 path 参数);
    /// 下一次组合指令时消费并清空,turn 结束自然丢弃
    pending_touches: Vec<String>,
    /// 请求失败重试策略(测试注入短延迟)
    retry_policy: RetryPolicy,
    /// 抖动随机源(∈ [0,1];默认取 uuid v7 随机位,测试注入固定样本)
    random_source: Box<dyn Fn() -> f64 + Send + Sync>,
}

/// 每步重扫 workspace 指令的回调类型(宿主注入;`&[String]` = 上步触碰路径)
pub type InstructionsProviderFn = dyn Fn(&[String]) -> Option<Value> + Send + Sync;
/// 每步重扫 workspace 指令的回调(宿主注入;`&[String]` = 上步触碰路径)。
pub type InstructionsProvider = Box<InstructionsProviderFn>;

/// 每步渲染 runtime-context 快照的回调(宿主注入;`None` = 无快照)。
pub type ContextProvider = Box<dyn Fn() -> Option<(String, Vec<ContextSection>)> + Send + Sync>;

/// 每步渲染 skill 目录的回调(宿主注入;`Some` = 完整 user/message 载荷
/// {content, source}。目录变化才 Some——digest 幂等在宿主 SkillCatalogState,
/// 引擎只管按序落档;None = 无变化不重发)。
pub type SkillCatalogProvider = Box<dyn Fn() -> Option<serde_json::Value> + Send + Sync>;

/// 本步用户面消息文本 → `/name` 手势注入载荷(源 skill-invocation;
/// 引擎在全部注入之后追加落档——源序:「背景在前,模型要执行的材料
/// 在后,最贴近它的回答」)。仅扫真实用户消息(外部文本不可伪造手势)。
pub type SkillGestureProvider = Box<dyn Fn(&[String]) -> Vec<serde_json::Value> + Send + Sync>;

/// 默认抖动随机源:uuid v7 的随机位(62 bit)折算 [0,1)。
/// 同一毫秒内连续调用各自独立,足以做退避抖动(非密码学场景)
fn uuid_random() -> f64 {
    (uuid::Uuid::now_v7().as_u128() >> 64) as f64 / (1u128 << 64) as f64
}

impl LoopEngine {
    /// 以初始 header 与共享日志构建(闸门与引擎共享同一日志视图)
    pub fn new(header: RequestHeader, log: Arc<Mutex<EventLog>>) -> Self {
        Self {
            log,
            phase: Phase::Idle,
            inbox: Vec::new(),
            header,
            logged_header: None,
            cancel: CancelToken::new(),
            steer_buf: None,
            pending_input_source: None,
            fold_budget_chars: FOLD_BUDGET_CHARS,
            projection: RuntimeContextProjection::new(),
            context_provider: None,
            instructions_provider: None,
            skill_catalog_provider: None,
            skill_gesture_provider: None,
            pending_touches: Vec::new(),
            retry_policy: RetryPolicy::default(),
            random_source: Box::new(uuid_random),
        }
    }

    /// 装配重试策略(测试注入短延迟/零次数)
    pub fn set_retry_policy(&mut self, policy: RetryPolicy) {
        self.retry_policy = policy;
    }

    /// 装配抖动随机源(测试注入固定样本;默认 uuid v7 随机位)
    pub fn set_random_source(&mut self, source: Box<dyn Fn() -> f64 + Send + Sync>) {
        self.random_source = source;
    }

    /// 装配运行中 turn 的中途输入缓冲(steer;由宿主 worker 创建并共享,
    /// 引擎在 step 边界认领。未设置 = 无 steer 支持,中途输入永不消费)
    pub fn set_steer_buf(&mut self, buf: Arc<Mutex<VecDeque<SteerInput>>>) {
        self.steer_buf = Some(buf);
    }

    /// 设置下一 turn 真实用户消息的来源染色(None = 真实用户)。
    /// 宿主认领带 source 的队列条目时调用;run_turn 落档真实用户消息时
    /// 消费(take)。未设置 = 真实用户,行为与既有路径完全一致。
    pub fn set_input_source(&mut self, source: Option<serde_json::Value>) {
        self.pending_input_source = source;
    }

    /// 装配会话级 runtime-context 渲染回调(宿主注入;每步调它拿当前
    /// 渲染的 `(current 文本, sections)`,交投影去重后落档为注入 user/message)。
    /// 未设置 = 本会话无 runtime-context 注入。
    pub fn set_context_provider(&mut self, provider: ContextProvider) {
        self.context_provider = Some(provider);
    }

    /// 装配 workspace 指令(AGENTS.md)每步重扫回调(宿主注入)。每步在
    /// runtime 快照之前调用;上一步 file_read/file_edit 触碰的路径作为入参
    /// 传入,供宿主下探后代目录拾取嵌套指令文件。
    pub fn set_instructions_provider(&mut self, provider: InstructionsProvider) {
        self.instructions_provider = Some(provider);
    }

    /// 装配 skill 目录每步回调(宿主注入;dsh-skill SkillCatalogState)。
    /// 未设置 = 本会话无 skill 目录注入(skill 工具不在场时宿主不装,照源:
    /// 目录只在工具视图解析到本注册的 skill 工具时发布)。
    pub fn set_skill_catalog_provider(&mut self, provider: SkillCatalogProvider) {
        self.skill_catalog_provider = Some(provider);
    }

    /// 装配 `/name` 手势注入回调(宿主注入;dsh-skill gesture_payloads)。
    /// 未设置 = 本会话无手势识别。
    pub fn set_skill_gesture_provider(&mut self, provider: SkillGestureProvider) {
        self.skill_gesture_provider = Some(provider);
    }

    /// 从既有日志恢复投影 retained(重开会话:runtime-context 快照同源恢复)。
    /// 在引擎持有共享日志后调用。
    pub fn restore_projection(&mut self) {
        let projection = &mut self.projection;
        if let Ok(l) = self.log.lock() {
            let snap = l.iter().cloned().collect::<Vec<_>>();
            projection.restore(&snap);
        }
    }

    /// 认领所有待处理的中途输入并落档(唯一写入者 = 引擎,单线程保证
    /// splice 与 user/message 的 seq 顺序)。认领时序:
    /// 先记录 inserted splice(pending 累积),再记录认领 splice(removed →
    /// UI claimed 集合),随后逐条 user/message —— 同一 id 贯穿三处,
    /// UI 据此把中途消息渲染为 steering 节点并收回瞬态队列行。
    /// 返回每条 claim 的 (user/message seq, 文本, 是否真实用户)——文本供
    /// `/name` 手势扫描;空 = 无 steer。
    fn claim_steered(
        &self,
        clock: &(dyn Fn() -> i64 + Send + Sync),
        sink: &mut (dyn FnMut(&EventEnvelope) + Send),
    ) -> Result<Vec<(u64, String, bool)>, LoopError> {
        let Some(buf) = &self.steer_buf else {
            return Ok(Vec::new());
        };
        let entries: Vec<SteerInput> = {
            let mut b = buf
                .lock()
                .map_err(|_| LoopError::Log("steer 缓冲锁中毒".into()))?;
            b.drain(..).collect()
        };
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        Self::commit(
            &self.log,
            EventEnvelope::new(
                "agent/inbox/spliced",
                clock(),
                serde_json::json!({
                    "target": "next-step",
                    "start": 0,
                    "removedCount": 0,
                    "inserted": entries
                        .iter()
                        .map(|e| {
                            dsh_session::attachments::splice_item(
                                e.id.clone(),
                                e.text.clone(),
                                &e.images,
                                e.source.as_ref(),
                            )
                        })
                        .collect::<Vec<_>>(),
                }),
            ),
            sink,
        )?;
        Self::commit(
            &self.log,
            EventEnvelope::new(
                "agent/inbox/spliced",
                clock(),
                serde_json::json!({
                    "target": "next-step",
                    "start": 0,
                    "removedCount": entries.len(),
                    "inserted": [],
                }),
            ),
            sink,
        )?;
        let mut claims = Vec::new();
        for e in &entries {
            let seq = Self::commit(
                &self.log,
                EventEnvelope::new("user/message", clock(), {
                    let mut payload = serde_json::json!({
                        "content": dsh_session::attachments::message_content(&e.text, &e.images),
                        "id": e.id,
                    });
                    // 来源染色:结算通知等宿主注入 steer 时随消息落档
                    if let Some(src) = &e.source {
                        payload["source"] = src.clone();
                    }
                    payload
                }),
                sink,
            )?;
            // plain_user = 真实用户 steer(source 缺省或 kind=user);
            // 宿主注入(结算通知等)不参与 `/name` 手势扫描
            let plain_user = e
                .source
                .as_ref()
                .and_then(|s| s["kind"].as_str())
                .map(|k| k == "user")
                .unwrap_or(true);
            claims.push((seq, e.text.clone(), plain_user));
        }
        Ok(claims)
    }

    /// 调整历史折叠预算(测试用;默认 [`FOLD_BUDGET_CHARS`])
    pub fn set_fold_budget(&mut self, budget: usize) {
        self.fold_budget_chars = budget;
    }

    /// 替换请求 header(plan mode 切换后由装配层重建 prompt 注入;
    /// 闸门比对的 header 侧取请求自带 header,切换即生效)
    pub fn set_header(&mut self, header: RequestHeader) {
        self.header = header;
    }

    /// 会话级事件追加(session/mode、plan/approved 等;单边界:
    /// 与 turn 内事件同一条 log+sink 写入路径)
    pub fn commit_session_event(
        &self,
        r#type: &str,
        data: Value,
        clock: &(dyn Fn() -> i64 + Send + Sync),
        sink: &mut (dyn FnMut(&EventEnvelope) + Send),
    ) -> Result<u64, LoopError> {
        Self::commit(&self.log, EventEnvelope::new(r#type, clock(), data), sink)
    }

    /// 设置/替换软取消令牌(会话级共享;REPL 每回合 reset)
    pub fn set_cancel(&mut self, token: CancelToken) {
        self.cancel = token;
    }

    /// 当前相位
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// 共享日志(宿主闸门派生用)
    pub fn log(&self) -> Arc<Mutex<EventLog>> {
        Arc::clone(&self.log)
    }

    /// 追加事件:先记录(log + sink)再返回 seq——记录优先
    fn commit(
        log: &Mutex<EventLog>,
        ev: EventEnvelope,
        sink: &mut dyn FnMut(&EventEnvelope),
    ) -> Result<u64, LoopError> {
        let mut log = log
            .lock()
            .map_err(|_| LoopError::Log("log 锁中毒".into()))?;
        let seq = log.append(ev).map_err(|e| LoopError::Log(e.to_string()))?;
        let committed = log
            .get(seq)
            .expect("刚 append 的事件必在日志内(宿主不变式)")
            .clone();
        drop(log);
        sink(&committed);
        Ok(seq)
    }

    /// 流式消费一个请求:transport 逐事件送 channel,主循环 select 逐条
    /// commit + sink(chunk 到达即落档广播 → 前端逐 token)。
    /// 回调侧只捕获 owned tx(零借用,规避 async fn 嵌套闭包生命周期限制
    /// rust#100013);消费侧在主函数帧内逐事件处理。
    ///
    /// 取消安全点在**每个到达事件**上:令牌置位即断流(丢弃 stream
    /// future = HTTP 请求随之中止)并温和收尾——此前只在 step 边界检查,
    /// 长 LLM 流期间取消按钮形同虚设(「点击长时间无响应」)。
    #[allow(clippy::too_many_arguments)]
    async fn consume_stream<T: LlmTransport + Send>(
        transport: &mut T,
        header: &RequestHeader,
        messages: &Value,
        log: &Mutex<EventLog>,
        clock: &(dyn Fn() -> i64 + Send + Sync),
        sink: &mut (dyn FnMut(&EventEnvelope) + Send),
        acc: &mut StreamAcc,
        cancel: &CancelToken,
    ) -> Result<(), LoopError> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LlmEvent>();
        let stream_fut = transport.stream_events(header, messages, tx);
        tokio::pin!(stream_fut);
        loop {
            tokio::select! {
                r = &mut stream_fut => {
                    // 先排空缓冲尾部事件(整批 transport 在流完成前已全量
                    // 入队;**错误路径同样先排空**——记录优先,断流前已
                    // 到达的 chunk/reasoning 都要落档,重试的丢弃语义据此
                    // 可见),再传播结果
                    let result = r.map_err(LoopError::Transport);
                    while let Ok(ev) = rx.try_recv() {
                        Self::handle_stream_event(ev, log, clock, sink, acc)?;
                    }
                    result?;
                    break;
                }
                ev = rx.recv() => {
                    let Some(event) = ev else { continue };
                    if cancel.is_cancelled() {
                        // 温和收尾(turn/end cancelled 已落档);
                        // stream_fut 随 return 丢弃 → 请求中止
                        return Self::stop_cancelled(log, clock, sink).map(|_| ());
                    }
                    Self::handle_stream_event(event, log, clock, sink, acc)?;
                }
                // 取消是无事件时也要响应的安全点:provider 停顿(没有 chunk
                // 在路上)时 rx.recv() 与 stream_fut 都不置就绪,若只在
                // 事件臂查 cancel,发出「卡住了?」的停顿里点中止会永久悬挂,
                // 引擎不返回 → driver 不推 running:false → UI 动画不停。
                // cancelled() 是 Notify 驱动的 async 臂,取消即唤醒。
                _ = cancel.cancelled() => {
                    // 温和收尾;stream_fut 随 drop 丢弃 → HTTP 请求中止
                    return Self::stop_cancelled(log, clock, sink).map(|_| ());
                }
            }
        }
        Ok(())
    }

    /// 单条流事件处理(chunk 落档 / assistant 物化 / usage 合并)
    fn handle_stream_event(
        event: LlmEvent,
        log: &Mutex<EventLog>,
        clock: &(dyn Fn() -> i64 + Send + Sync),
        sink: &mut (dyn FnMut(&EventEnvelope) + Send),
        acc: &mut StreamAcc,
    ) -> Result<(), LoopError> {
        match event {
            // 推理增量:逐条落档(ignorable)——Think 行随流生长。
            // 此前累积到首个内容帧才一次性 flush,推理期(数秒,
            // reasoning_effort=high)UI 全空 =「Think 等输出完才显示」
            LlmEvent::Reasoning(delta) => Self::commit(
                log,
                EventEnvelope::new_ignorable(
                    "assistant/reasoning",
                    clock(),
                    serde_json::json!({ "text": delta }),
                ),
                sink,
            )
            .map(|_| ()),
            LlmEvent::Chunk(delta) => {
                acc.assistant_text.push_str(&delta);
                Self::commit(
                    log,
                    EventEnvelope::new_ignorable(
                        "assistant/chunk",
                        clock(),
                        serde_json::json!({ "delta": delta }),
                    ),
                    sink,
                )
                .map(|_| ())
            }
            LlmEvent::AssistantMessage(m) => {
                acc.final_message = Some(m);
                Ok(())
            }
            // 用量:并入调用完成审计(多帧合并;含 transport 附带的 ttft)。
            // prev 非对象(如未守卫的 null 帧)而新值为对象时让位替换
            LlmEvent::Usage(v) => {
                let prev = acc.usage.take();
                acc.usage = Some(match prev {
                    Some(mut prev) => {
                        if let (Some(a), Some(b)) = (prev.as_object_mut(), v.as_object()) {
                            for (k, val) in b {
                                a.insert(k.clone(), val.clone());
                            }
                            prev
                        } else if v.is_object() {
                            v
                        } else {
                            prev
                        }
                    }
                    None => v,
                });
                Ok(())
            }
            // 流内失败帧:此前静默忽略(provider 以数据帧报错时回合假成功,
            // 落空 assistant/message)。记入 acc,流结束后参与重试分类
            LlmEvent::Failure(v) => {
                acc.failure = Some(v);
                Ok(())
            }
            LlmEvent::Done => Ok(()),
        }
    }

    /// 流正常结束后的失败判定:流内失败帧优先,其次空响应
    /// (零 chunk 且零物化消息 = provider 未产出任何可用内容;
    /// 带 tool_calls 的物化消息不算空——那是正常的工具步)
    fn post_stream_failure(acc: &StreamAcc) -> Option<TransportError> {
        if let Some(v) = &acc.failure {
            return Some(Self::classify_stream_failure(v));
        }
        if acc.final_message.is_none() && acc.assistant_text.is_empty() {
            return Some(TransportError::EmptyResponse);
        }
        None
    }

    /// 流内失败帧归类:按帧内 `code` 字段映射既有分类(流内错误
    /// 语义);未知/缺省 → Other(不重试)
    fn classify_stream_failure(v: &Value) -> TransportError {
        let message = v
            .get("message")
            .and_then(|m| m.as_str())
            .map(String::from)
            .unwrap_or_else(|| v.to_string());
        match v.get("code").and_then(|c| c.as_str()).unwrap_or_default() {
            "TRANSPORT" => TransportError::Transport(message),
            "TIMEOUT" => TransportError::Timeout(message),
            "RATE_LIMIT" => TransportError::RateLimit {
                retry_after_ms: v.get("retryAfterMs").and_then(|r| r.as_u64()),
                body: message,
            },
            "EMPTY_RESPONSE" => TransportError::EmptyResponse,
            "SERVER" => TransportError::Server {
                status: v.get("status").and_then(|s| s.as_u64()).unwrap_or(0) as u16,
                body: message,
            },
            // AUTH/INVALID_REQUEST/未分类:直通不重试
            _ => TransportError::Other(message),
        }
    }

    /// 驱动一个 turn:认领输入 → step 循环 → 空闲收尾。
    ///
    /// 一个 step = 一次模型请求加其触发的工具执行:
    /// assistant 携带 tool_calls → tool/call → 执行(ToolPort)→ tool/result
    /// → 下一 step;无 tool_calls → step/end → turn/end。
    /// 循环无步数上限(模型驱动终止);失控兜底 = 取消令牌
    /// (安全点:step 边界/出网返回后/工具执行前)+ 硬取消。
    ///
    /// `sink` 在每个事件落日志后同步触发(持久化/分发);`clock` 注入时间戳。
    /// 两者要求 Send:网关在独立任务上驱动 serve 循环。
    ///
    /// 错误退场保证:任何 Err(除 Busy)都会落档 `turn/error` 并复位
    /// 运行态(inbox/phase)——否则传输悬挂/失败后日志悬着未闭合的
    /// step/start,且 phase 停留 Running 使后续 turn 永久 Busy。
    /// Cancelled 例外:软取消路径已落档 turn/end(cancelled)。
    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn<T, TOOLS>(
        &mut self,
        input: &str,
        input_id: Option<&str>,
        images: &[dsh_session::attachments::ImageAttachmentRef],
        contexts: &[serde_json::Value],
        transport: &mut T,
        tools: &mut TOOLS,
        clock: &(dyn Fn() -> i64 + Send + Sync),
        sink: &mut (dyn FnMut(&EventEnvelope) + Send),
    ) -> Result<TurnOutcome, LoopError>
    where
        T: LlmTransport + crate::summarizer::Summarizer + Send,
        TOOLS: crate::tools::ToolPort,
    {
        if self.phase == Phase::Running {
            return Err(LoopError::Busy);
        }
        match self
            .run_turn_inner(
                input, input_id, images, contexts, transport, tools, clock, sink,
            )
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                if !matches!(e, LoopError::Busy | LoopError::Cancelled) {
                    // 错误码随 turn/error 落档(translate 透传给客方面;
                    // 非传输错误 = 引擎内部问题)
                    let code = match &e {
                        LoopError::Transport(te) => te.code(),
                        _ => "INTERNAL",
                    };
                    let _ = Self::commit(
                        &self.log,
                        EventEnvelope::new(
                            "turn/error",
                            clock(),
                            serde_json::json!({ "error": e.to_string(), "code": code }),
                        ),
                        sink,
                    );
                }
                // Busy = 别人的 turn 在跑,不动状态;其余复位
                // (Cancelled 此前不复位 → 取消后再发永久 Busy,一并修)
                if !matches!(e, LoopError::Busy) {
                    self.inbox.clear();
                    self.phase = Phase::Idle;
                }
                Err(e)
            }
        }
    }

    /// run_turn 主体(错误退场见 [`Self::run_turn`] 文档)
    #[allow(clippy::too_many_arguments)]
    async fn run_turn_inner<T, TOOLS>(
        &mut self,
        input: &str,
        input_id: Option<&str>,
        images: &[dsh_session::attachments::ImageAttachmentRef],
        contexts: &[serde_json::Value],
        transport: &mut T,
        tools: &mut TOOLS,
        clock: &(dyn Fn() -> i64 + Send + Sync),
        sink: &mut (dyn FnMut(&EventEnvelope) + Send),
    ) -> Result<TurnOutcome, LoopError>
    where
        T: LlmTransport + crate::summarizer::Summarizer + Send,
        TOOLS: crate::tools::ToolPort,
    {
        self.inbox.push(input.to_string());
        self.phase = Phase::Running;
        // 工具声明随 turn 注入 header(工具集来自宿主装配,不是会话日志派生)
        self.header.tools = tools.specs();

        // 消息 id 持久入档(重放稳定;steer/队列条目携带宿主预分配 id,
        // 普通输入在此生成 v7)。记录优先:与事件同一条 commit 路径。
        let input_id = match input_id {
            Some(id) => id.to_string(),
            None => uuid::Uuid::now_v7().to_string(),
        };
        let seq_start = Self::commit(
            &self.log,
            EventEnvelope::new("turn/start", clock(), serde_json::json!({})),
            sink,
        )?;

        // 首步消息组装输入:真实用户消息在 step/start 之后落档——真实
        // 用户消息与 steer 都是该步的 claimed,注入上下文随后,都在
        // step/start 之后。
        let mut first_user_committed = false;
        // 跨步累积的归因锚:首步 = 真实用户消息 seq;后续步若有 steer 认领则
        // 更新为该步首 steer seq,否则沿用(整个 turn 的请求都归因到触发它
        // 的 user/message——请求/折叠都归因到该 turn 的用户输入)。
        let mut turn_anchor = 0u64;

        // 唯一落到收尾的路径是 break(无 tool_calls),必已赋值
        let final_assistant;
        loop {
            // 安全点:step 边界(软取消;turn/end 记录 cancelled 归因)
            if self.cancel.is_cancelled() {
                return Self::stop_cancelled(&self.log, clock, sink);
            }
            Self::commit(
                &self.log,
                EventEnvelope::new("step/start", clock(), serde_json::json!({})),
                sink,
            )?;

            // step/start 之后:①认领 steer(claimed)→ ②真实用户消息(仅首步,
            // 1 条)→ ③注入上下文(每步经 projection,
            // 文本变才生成)。全部落档为 user/message。
            // 同一步内 steer claims 先、真实用户次(drain 顺序:
            // next-step 先,next-turn 后)。
            // 本步用户面文本(真实用户消息)随步收集,供 `/name` 手势
            // 扫描(step 末统一处理;宿主注入的染色消息不参与)。
            let mut step_user_texts: Vec<String> = Vec::new();
            let steer_claims = self.claim_steered(clock, sink)?;
            if let Some((first, _, _)) = steer_claims.first() {
                turn_anchor = *first;
            }
            for (_, text, plain) in &steer_claims {
                if *plain {
                    step_user_texts.push(text.clone());
                }
            }
            if !first_user_committed {
                // 真实用户消息(仅首步):作为本步 claimed 一员,step/start 后落档。
                // 来源染色:宿主认领带 source 的条目(结算通知)时随
                // 消息落档;缺省 = 真实用户,载荷不带 source(与既有日志同形)。
                let tinted = self.pending_input_source.take();
                let plain_user = tinted
                    .as_ref()
                    .and_then(|s| s["kind"].as_str())
                    .map(|k| k == "user")
                    .unwrap_or(true);
                let mut user_payload = serde_json::json!({
                    "content": dsh_session::attachments::message_content(input, images),
                    "id": input_id,
                });
                if let Some(src) = tinted {
                    user_payload["source"] = src;
                }
                let user_seq = Self::commit(
                    &self.log,
                    EventEnvelope::new("user/message", clock(), user_payload),
                    sink,
                )?;
                first_user_committed = true;
                if plain_user {
                    step_user_texts.push(input.to_string());
                }
                // 若本步无 steer,真实用户消息即本 turn 首条 claimed(锚点);
                // 有 steer 则保持 steer 为首锚。
                if turn_anchor == 0 {
                    turn_anchor = user_seq;
                }
            }
            // 注入上下文(每步判断;源 RuntimeContextProjection:文本变才生成)。
            // step 级投影在移植后每步调用;此处为当前 turn 级 contexts 透传。
            for ctx in contexts {
                let mut payload = ctx.clone();
                if payload.get("id").and_then(|v| v.as_str()).is_none() {
                    payload["id"] = serde_json::json!(uuid::Uuid::now_v7().to_string());
                }
                if payload.get("source").is_none() {
                    payload["source"] = serde_json::json!({ "kind": "context" });
                }
                Self::commit(
                    &self.log,
                    EventEnvelope::new("user/message", clock(), payload),
                    sink,
                )?;
            }

            // workspace 指令注入(每步重扫;紧跟用户输入、先于
            // runtime context)。差分/缓存/预算在
            // 宿主 InstructionRuntimeState;触碰路径随本次消费传入。
            if let Some(provider) = &self.instructions_provider {
                let touches = std::mem::take(&mut self.pending_touches);
                if let Some(mut payload) = provider(&touches)
                    && payload.get("id").and_then(|v| v.as_str()).is_none()
                {
                    payload["id"] = serde_json::json!(uuid::Uuid::now_v7().to_string());
                    Self::commit(
                        &self.log,
                        EventEnvelope::new("user/message", clock(), payload),
                        sink,
                    )?;
                }
            }

            // 投影快照注入(每步;源 RuntimeContextProjection:文本变才生成)。
            // runtime 快照(sandbox/approval 策略)经宿主渲染回调交投影去重;
            // 用户主动注入的 contexts 走上面的循环,两者独立。
            if let Some(provider) = &self.context_provider
                && let Some((current, sections)) = provider()
            {
                // 折叠遮蔽命中 retained 时先失效(读日志判定)
                if let Ok(l) = self.log.lock() {
                    let snap = l.iter().cloned().collect::<Vec<_>>();
                    self.projection.refresh_fold(&snap);
                }
                if let Some((mut payload, _text)) = self.projection.project(&current, &sections) {
                    if payload.get("id").and_then(|v| v.as_str()).is_none() {
                        payload["id"] = serde_json::json!(uuid::Uuid::now_v7().to_string());
                    }
                    let injected_seq = Self::commit(
                        &self.log,
                        EventEnvelope::new("user/message", clock(), payload),
                        sink,
                    )?;
                    // 观察注入事件,retained 更新为新快照(seq/text)
                    if let Ok(l) = self.log.lock()
                        && let Some(ev) = l.get(injected_seq)
                    {
                        self.projection.observe_event(ev);
                    }
                }
            }

            // skill 目录注入(每步;宿主 digest 幂等,变化才 Some)。排
            // runtime 快照之后、手势之前——源序:背景(workspace 规则、
            // runtime 策略、目录)在前。仅首个 pre-step 发布 + 变化整条
            // 替换;「模型只见一份」由 dsh-session 派生层保留最新一条达成。
            if let Some(provider) = &self.skill_catalog_provider
                && let Some(mut payload) = provider()
            {
                if payload.get("id").and_then(|v| v.as_str()).is_none() {
                    payload["id"] = serde_json::json!(uuid::Uuid::now_v7().to_string());
                }
                Self::commit(
                    &self.log,
                    EventEnvelope::new("user/message", clock(), payload),
                    sink,
                )?;
            }

            // skill 手势注入(仅本步有真实用户消息时;载荷排在全部注入
            // 最后——源序:模型要执行的材料最贴近它的回答)。
            if let Some(provider) = &self.skill_gesture_provider
                && !step_user_texts.is_empty()
            {
                for mut payload in provider(&step_user_texts) {
                    if payload.get("id").and_then(|v| v.as_str()).is_none() {
                        payload["id"] = serde_json::json!(uuid::Uuid::now_v7().to_string());
                    }
                    Self::commit(
                        &self.log,
                        EventEnvelope::new("user/message", clock(), payload),
                        sink,
                    )?;
                }
            }

            // 历史折叠:可见消息超预算 → 一次性摘要(记录优先:
            // audit → 调用 → compaction/summary 落档;重放读记录不重调)。
            // turn_anchor = 本 turn 首条 claimed 的归因锚。
            Self::maybe_fold(
                &self.log,
                self.fold_budget_chars,
                transport,
                &self.header,
                turn_anchor,
                clock,
                sink,
            )
            .await?;

            // 模型可见消息 = 日志投影 + 显式策略栈(裁剪/折叠;唯一来源,
            // 闸门期望侧共用同一函数——不变式比对的两侧同一实现)
            let messages = {
                let log = self
                    .log
                    .lock()
                    .map_err(|_| LoopError::Log("log 锁中毒".into()))?;
                derive_visible_messages(log.iter())
            };

            // E5 审计:跨边界调用(llm 出网)记录优先——先落日志再出网,
            // 归因指向触发本 turn 的 user/message。
            // 出网(失败自动重试):每次尝试一对
            // audit/call request/request-done。可重试失败(TRANSPORT/TIMEOUT/
            // SERVER/RATE_LIMIT/空响应/流内失败帧)→ llm/retry 落档 → 可取消
            // 退避 → llm/retry-started → 重发同一步完整请求;不可重试或耗尽
            // → LoopError 退场(run_turn 统一落 turn/error)。
            // 记录优先的偏差处置:失败尝试已落档的部分 chunk 由
            // assistant/stream-reset(ignorable)标记丢弃,投影与重放据此
            // 清空残段(日志只追加,重发前以丢弃标记达成同一可见语义)。
            let mut acc = StreamAcc::default();
            let mut attempt: u32 = 0;
            loop {
                attempt += 1;
                // 信封变更时附带完整快照(systemPrompt 全文 + tools 全量目录;
                // 不变则省略防膨胀):轨迹 SYSTEM 详情(System Prompt/Tools/
                // Diff)与工具 Schema 页的数据面(headerEquals 去重:
                // 重试的请求信封不变,自然去重)
                let envelope_changed = self.logged_header.as_ref() != Some(&self.header);
                let mut audit_detail = serde_json::json!({
                    "model": self.header.model,
                    "messages": messages.as_array().map(|a| a.len()).unwrap_or(0),
                    // 请求 Options 面板(轨迹详情)数据源:推理等级
                    "reasoningEffort": self.header.reasoning_effort,
                    // 上下文构成面板数据源(UI ContextMeter 分段条):
                    // system/tools 字符长度,重放可复算,不随实现丢失
                    "systemChars": self.header.system.chars().count(),
                    "toolsChars": serde_json::to_string(&self.header.tools)
                        .map(|s| s.chars().count())
                        .unwrap_or(0),
                });
                if envelope_changed {
                    audit_detail["systemPrompt"] = serde_json::json!(self.header.system);
                    audit_detail["tools"] = serde_json::json!(self.header.tools);
                    self.logged_header = Some(self.header.clone());
                }
                Self::commit(
                    &self.log,
                    audit_call_event(
                        clock(),
                        BOUNDARY_LLM,
                        "request",
                        audit_detail,
                        vec![turn_anchor],
                    ),
                    sink,
                )?;

                // 出网:宿主闸门在此做内容级校验(E1)。
                // 流式路径:transport 逐事件回调,每个 chunk 立即落档 + sink → 前端逐 token
                let llm_t0 = clock();
                let consumed = Self::consume_stream(
                    transport,
                    &self.header,
                    &messages,
                    &self.log,
                    clock,
                    sink,
                    &mut acc,
                    &self.cancel,
                )
                .await;
                let llm_duration_ms = clock() - llm_t0;
                // 安全点:出网返回后(取消在等待期间触发的常见窗口)
                if self.cancel.is_cancelled() {
                    return Self::stop_cancelled(&self.log, clock, sink);
                }

                // 终态失败归一:传输错误直取;流正常结束但带失败帧/零内容
                // → 分类判定(此前 Failure 静默忽略、空流落空 assistant/message)
                let failure = match consumed {
                    Ok(()) => Self::post_stream_failure(&acc),
                    Err(LoopError::Transport(f)) => Some(f),
                    // 取消/日志错误与重试无关,直通退场
                    Err(e) => return Err(e),
                };
                let Some(failure) = failure else {
                    // E5 审计:调用完成记录(时长 + 用量;与出网前意图记录成对)
                    let mut done_detail = serde_json::json!({
                        "model": self.header.model,
                        "durationMs": llm_duration_ms,
                    });
                    if let Some(u) = &acc.usage {
                        done_detail["usage"] = u.clone();
                    }
                    Self::commit(
                        &self.log,
                        audit_call_event(
                            clock(),
                            BOUNDARY_LLM,
                            "request-done",
                            done_detail,
                            vec![turn_anchor],
                        ),
                        sink,
                    )?;
                    break;
                };

                // 失败态 request-done:失败尝试留痕(错误码;无用量)
                Self::commit(
                    &self.log,
                    audit_call_event(
                        clock(),
                        BOUNDARY_LLM,
                        "request-done",
                        serde_json::json!({
                            "model": self.header.model,
                            "durationMs": llm_duration_ms,
                            "error": failure.code(),
                        }),
                        vec![turn_anchor],
                    ),
                    sink,
                )?;
                // 重试决策:第 attempt 次失败 → 第 attempt 次重试;
                // None = 不可重试分类或已耗尽 → 放行错误
                let retry_no = attempt;
                let random = (self.random_source)();
                let Some(delay) = self.retry_policy.decide(&failure, retry_no, random) else {
                    return Err(LoopError::Transport(failure));
                };
                // 残段丢弃标记:失败尝试已落档内容帧时通知投影/重放清空,
                // 随后累积态整体重置(成功尝试的文本不得叠加残段)
                if acc.has_streamed_content() {
                    Self::commit(
                        &self.log,
                        EventEnvelope::new_ignorable(
                            "assistant/stream-reset",
                            clock(),
                            serde_json::json!({}),
                        ),
                        sink,
                    )?;
                }
                acc = StreamAcc::default();
                let delay_ms = delay.as_millis() as u64;
                Self::commit(
                    &self.log,
                    EventEnvelope::new(
                        "llm/retry",
                        clock(),
                        serde_json::json!({
                            "retry": retry_no,
                            "maxRetries": self.retry_policy.max_retries,
                            "delayMs": delay_ms,
                            "code": failure.code(),
                            "message": failure.to_string(),
                        }),
                    ),
                    sink,
                )?;
                // 可取消退避:等待中点「停止」= 立即中止重试转 aborted
                let cancel = self.cancel.clone();
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel.cancelled() => {
                        return Self::stop_cancelled(&self.log, clock, sink);
                    }
                }
                Self::commit(
                    &self.log,
                    EventEnvelope::new(
                        "llm/retry-started",
                        clock(),
                        serde_json::json!({ "retry": retry_no }),
                    ),
                    sink,
                )?;
            }

            let message_content = acc
                .final_message
                .as_ref()
                .map(|m| m["content"].as_str().unwrap_or_default().to_string())
                .unwrap_or_else(|| acc.assistant_text.clone());
            let tool_calls: Vec<Value> = acc
                .final_message
                .as_ref()
                .and_then(|m| m["tool_calls"].as_array().cloned())
                .unwrap_or_default();

            // assistant/message:内容 + tool_calls(派生面透传,记录 ⟺ 可见)
            // assistant/message 落档携带持久消息 id(v7;消息反馈定位 key;
            // 不兼容旧日志,开发期只做最优实现)
            let mut message_data = serde_json::json!({
                "content": message_content,
                "id": uuid::Uuid::now_v7().to_string(),
            });
            if !tool_calls.is_empty() {
                message_data["tool_calls"] = serde_json::json!(tool_calls);
            }
            let assistant_seq = Self::commit(
                &self.log,
                EventEnvelope::new("assistant/message", clock(), message_data),
                sink,
            )?;

            if tool_calls.is_empty() {
                // 流式期间到达的 steer:turn 延续(nextStep 非空则
                // turn 不断,认领后模型再跑一轮);空认领才收尾
                if !self.claim_steered(clock, sink)?.is_empty() {
                    continue;
                }
                final_assistant = message_content;
                break; // 模型不再调用工具:turn 收尾(正常终止,无步数上限)
            }
            // 有工具调用则继续下一 step(循环无上限,取消令牌是失控兜底)

            // 工具执行在本 step 内:tool/call → 执行 → tool/result
            for call in &tool_calls {
                let request = crate::tools::ToolCallRequest {
                    name: call["name"].as_str().unwrap_or_default().to_string(),
                    arguments: call["arguments"].clone(),
                };
                // call 侧渲染意图(运行中意图,如 file_edit 的 old/new
                // diff);视图随事件持久化,回放与新会话恒等
                let mut call_data = serde_json::json!({
                    "name": request.name,
                    "arguments": request.arguments,
                });
                if let Some(view) = tools.present_call(&request)
                    && let Ok(v) = serde_json::to_value(&view)
                {
                    call_data["view"] = v;
                }
                let call_seq = Self::commit(
                    &self.log,
                    EventEnvelope::new("tool/call", clock(), call_data),
                    sink,
                )?;
                // 安全点:工具执行前(取消则本 turn 温和收尾)
                if self.cancel.is_cancelled() {
                    return Self::stop_cancelled(&self.log, clock, sink);
                }
                // E5 审计:工具执行前记录,归因指向携带 tool_calls 的 assistant/message
                Self::commit(
                    &self.log,
                    audit_call_event(
                        clock(),
                        BOUNDARY_TOOL,
                        &request.name,
                        serde_json::json!({ "call": call_seq }),
                        vec![assistant_seq],
                    ),
                    sink,
                )?;
                // 指令触碰记账:文件读/写的路径入队,下一步组合指令时下探
                // 后代目录(收集在 tools/result、统一投影在 step/end,时序等价)
                if matches!(
                    request.name.as_str(),
                    "file_read" | "file_edit" | "read" | "write" | "edit"
                ) && let Some(path) = request
                    .arguments
                    .get("path")
                    .or_else(|| request.arguments.get("file_path"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                {
                    self.pending_touches.push(path.to_string());
                }
                let tool_t0 = clock();
                let output = tools.execute(&request).await;
                let tool_duration_ms = clock() - tool_t0;
                let mut result_data = serde_json::json!({
                    "call": call_seq,
                    // provider 的调用标识(wire 方言 tool_call_id 适配用)
                    "id": call["id"].as_str().unwrap_or_default(),
                    "output": output.output,
                    "success": output.success,
                });
                // result 侧渲染意图(终端退出详情/读窗口/搜索分组/diff
                // 事实);在场才写,非视图工具零噪音
                if let Some(view) = &output.view
                    && let Ok(v) = serde_json::to_value(view)
                {
                    result_data["view"] = v;
                }
                // 结果图片(MCP 图片桥);非空才写(零噪音惯例)
                if !output.images.is_empty()
                    && let Ok(imgs) = serde_json::to_value(&output.images)
                {
                    result_data["images"] = imgs;
                }
                Self::commit(
                    &self.log,
                    EventEnvelope::new("tool/result", clock(), result_data),
                    sink,
                )?;
                // E5 审计:工具完成记录(时长;状态栏数据源)
                Self::commit(
                    &self.log,
                    audit_call_event(
                        clock(),
                        BOUNDARY_TOOL,
                        &request.name,
                        serde_json::json!({
                            "call": call_seq,
                            "durationMs": tool_duration_ms,
                        }),
                        vec![assistant_seq],
                    ),
                    sink,
                )?;
                // 工具的持久状态事件(todo/write 等):仍经唯一写入口追加
                for (state_type, state_data) in tools.take_state_events() {
                    Self::commit(
                        &self.log,
                        EventEnvelope::new(&state_type, clock(), state_data),
                        sink,
                    )?;
                }
            }
            Self::commit(
                &self.log,
                EventEnvelope::new("step/end", clock(), serde_json::json!({})),
                sink,
            )?;
        }
        Self::commit(
            &self.log,
            EventEnvelope::new("step/end", clock(), serde_json::json!({})),
            sink,
        )?;
        let seq_end = Self::commit(
            &self.log,
            EventEnvelope::new("turn/end", clock(), serde_json::json!({})),
            sink,
        )?;

        self.inbox.clear();
        self.phase = Phase::Idle;
        Ok(TurnOutcome {
            assistant_message: final_assistant,
            seq_range: (seq_start, seq_end),
        })
    }

    /// 折叠判定与执行:可见消息超预算且余量足够时,把前缀摘要为一条
    /// `compaction/summary` 事件。折叠前缀 = 除最近 [`FOLD_KEEP_MESSAGES`]
    /// 条外的全部可见消息;`through_seq` = 前缀最后一条消息对应的事件 seq。
    #[allow(clippy::too_many_arguments)]
    async fn maybe_fold<T>(
        log: &Arc<Mutex<EventLog>>,
        budget: usize,
        transport: &mut T,
        header: &RequestHeader,
        user_seq: u64,
        clock: &(dyn Fn() -> i64 + Send + Sync),
        sink: &mut (dyn FnMut(&EventEnvelope) + Send),
    ) -> Result<(), LoopError>
    where
        T: LlmTransport + crate::summarizer::Summarizer,
    {
        let visible = {
            let Ok(l) = log.lock() else {
                return Err(LoopError::Log("log 锁中毒".into()));
            };
            derive_visible_messages(l.iter())
        };
        let total = visible.to_string().chars().count();
        if total <= budget {
            return Ok(());
        }
        let Some(arr) = visible.as_array() else {
            return Ok(());
        };
        if arr.len() <= FOLD_KEEP_MESSAGES {
            return Ok(());
        }
        let fold_len = arr.len() - FOLD_KEEP_MESSAGES;

        // through_seq:与当前可见面一致地数事件产出(folding 后只数
        // 最近 summary 之后的事件),取第 fold_len 条(扣除摘要消息)条。
        // shadowed_range = 折叠遮蔽的可见面 seq 区间——
        // start = 折叠前缀第一条 message seq,end = through_seq。
        let (through_seq, shadowed_start) = {
            let Ok(l) = log.lock() else {
                return Err(LoopError::Log("log 锁中毒".into()));
            };
            let prev_through = l
                .iter()
                .rev()
                .find(|e| e.r#type == "compaction/summary")
                .and_then(|e| e.data["throughSeq"].as_u64())
                .unwrap_or(0);
            let summary_msgs = u64::from(prev_through > 0) as usize;
            let mut produced = 0usize;
            let mut through = 0u64;
            let mut start = 0u64;
            for ev in l.iter().filter(|e| e.seq > prev_through) {
                if message_from_event(&ev.r#type, &ev.data).is_some() {
                    if start == 0 {
                        start = ev.seq;
                    }
                    produced += 1;
                    if produced + summary_msgs == fold_len {
                        through = ev.seq;
                        break;
                    }
                }
            }
            (through, start)
        };
        if through_seq == 0 {
            return Ok(()); // 前缀映射不到事件(不应发生;不折叠保守处理)
        }

        // 记录优先:审计先落(一次性调用,归因当前 user/message)
        Self::commit(
            log,
            audit_call_event(
                clock(),
                BOUNDARY_LLM,
                "compaction",
                serde_json::json!({ "chars": total, "through": through_seq }),
                vec![user_seq],
            ),
            sink,
        )?;
        let fold_messages = Value::Array(arr[..fold_len].to_vec());
        let summary = match transport.summarize(header, &fold_messages).await {
            Ok(s) => s,
            Err(e) => {
                // 折叠失败(超时/拒绝):跳过折叠,历史保持完整,本次 turn 不受阻
                // (不落 compaction/summary → 后续可见面与日志一致,不变式保持)
                eprintln!("[dsh-agent-loop] 历史折叠失败,跳过: {e}");
                return Ok(());
            }
        };
        Self::commit(
            log,
            EventEnvelope::new(
                "compaction/summary",
                clock(),
                serde_json::json!({
                    "summary": summary,
                    "throughSeq": through_seq,
                    // 折叠遮蔽的可见面 seq 区间(投影据此判定
                    // retained 失效——端取 through_seq,启取折叠前缀首条 message seq,
                    // 退化时以 end 兜底)
                    "shadowedRange": {
                        "start": if shadowed_start == 0 { through_seq } else { shadowed_start },
                        "end": through_seq,
                    },
                }),
            ),
            sink,
        )?;
        Ok(())
    }

    /// 软取消收尾:turn/end 记录 cancelled 归因,phase 置 Stopped
    fn stop_cancelled(
        log: &Mutex<EventLog>,
        clock: &dyn Fn() -> i64,
        sink: &mut dyn FnMut(&EventEnvelope),
    ) -> Result<TurnOutcome, LoopError> {
        Self::commit(
            log,
            EventEnvelope::new(
                "turn/end",
                clock(),
                serde_json::json!({ "cancelled": "token" }),
            ),
            sink,
        )?;
        Err(LoopError::Cancelled)
    }

    /// 取消(cause 记录进日志;仅 turn 进行中生效)
    pub fn cancel(
        &mut self,
        cause: &str,
        clock: &(dyn Fn() -> i64 + Send + Sync),
        sink: &mut (dyn FnMut(&EventEnvelope) + Send),
    ) -> Result<(), LoopError> {
        if self.phase != Phase::Running {
            return Ok(());
        }
        Self::commit(
            &self.log,
            EventEnvelope::new(
                "turn/end",
                clock(),
                serde_json::json!({ "cancelled": cause }),
            ),
            sink,
        )?;
        self.phase = Phase::Stopped;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolCallRequest, ToolOutput};
    use crate::transport::LlmEvent;
    use std::pin::Pin;
    use std::sync::Arc;

    /// 恒错 transport(模拟传输悬挂/失败被超时打断)
    struct FailingTransport;

    impl LlmTransport for FailingTransport {
        async fn stream(
            &mut self,
            _header: &RequestHeader,
            _messages: &Value,
        ) -> Result<Vec<LlmEvent>, TransportError> {
            Err(TransportError::Other("transport stalled".into()))
        }
    }

    impl crate::summarizer::Summarizer for FailingTransport {
        fn summarize<'a>(
            &'a mut self,
            _header: &'a RequestHeader,
            _messages: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async { Err("no summarize".into()) })
        }
    }

    /// 极简成功 transport:一条内容后 Done(空响应已是可重试失败,
    /// 成功用例必须携带内容)
    struct EmptyTransport;

    impl LlmTransport for EmptyTransport {
        async fn stream(
            &mut self,
            _header: &RequestHeader,
            _messages: &Value,
        ) -> Result<Vec<LlmEvent>, TransportError> {
            Ok(vec![LlmEvent::Chunk("ok".into()), LlmEvent::Done])
        }
    }

    impl crate::summarizer::Summarizer for EmptyTransport {
        fn summarize<'a>(
            &'a mut self,
            _header: &'a RequestHeader,
            _messages: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    struct NoTools;

    impl crate::tools::ToolPort for NoTools {
        async fn execute(&mut self, _call: &ToolCallRequest) -> ToolOutput {
            ToolOutput {
                output: String::new(),
                success: true,
                ..Default::default()
            }
        }
    }

    fn header() -> RequestHeader {
        RequestHeader {
            model: "test-model".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: Vec::new(),
        }
    }

    /// 传输错误退场:turn/error 落档 + phase 复位(否则后续 turn
    /// 永久 Busy、日志悬着未闭合的 step/start)
    #[tokio::test]
    async fn transport_error_records_turn_error_and_recovers() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut engine = LoopEngine::new(header(), Arc::clone(&log));
        let mut transport = FailingTransport;
        let mut tools = NoTools;
        let clock = || 1i64;

        let r = engine
            .run_turn(
                "hi",
                None,
                &[],
                &[],
                &mut transport,
                &mut tools,
                &clock,
                &mut |_| {},
            )
            .await;
        assert!(matches!(r, Err(LoopError::Transport(_))));

        // 错误已持久化为 durable 事件
        {
            let l = log.lock().unwrap();
            let ty_err = l
                .iter()
                .find(|ev| ev.r#type == "turn/error")
                .expect("turn/error 应已落档");
            assert!(ty_err.data["error"].as_str().unwrap().contains("stalled"));
        }

        // phase 复位:换成功 transport 再来一轮,不再 Busy
        let mut ok_transport = EmptyTransport;
        let r2 = engine
            .run_turn(
                "again",
                None,
                &[],
                &[],
                &mut ok_transport,
                &mut tools,
                &clock,
                &mut |_| {},
            )
            .await;
        assert!(r2.is_ok(), "错误退场后应可再次驱动 turn: {r2:?}");
        let l = log.lock().unwrap();
        assert!(l.iter().any(|ev| ev.r#type == "turn/end"));
    }

    /// 审计快照去重:信封变更时 audit request 携带 systemPrompt/tools
    /// 全量,未变更时省略(防每请求重复膨胀;headerEquals 语义)
    #[tokio::test]
    async fn audit_request_carries_snapshot_only_on_change() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut engine = LoopEngine::new(header(), Arc::clone(&log));
        let mut transport = EmptyTransport;
        let mut tools = NoTools;
        let clock = || 1i64;

        engine
            .run_turn(
                "one",
                None,
                &[],
                &[],
                &mut transport,
                &mut tools,
                &clock,
                &mut |_| {},
            )
            .await
            .unwrap();
        engine
            .run_turn(
                "two",
                None,
                &[],
                &[],
                &mut transport,
                &mut tools,
                &clock,
                &mut |_| {},
            )
            .await
            .unwrap();

        let audits = |log: &Arc<Mutex<EventLog>>| {
            log.lock()
                .unwrap()
                .iter()
                .filter(|ev| {
                    ev.r#type == "audit/call"
                        && ev.data["boundary"] == "llm"
                        && ev.data["operation"] == "request"
                })
                .map(|ev| ev.data["detail"].clone())
                .collect::<Vec<_>>()
        };
        let two = audits(&log);
        assert_eq!(two.len(), 2, "两 turn 两条 request 审计");
        assert!(
            two[0]["systemPrompt"].is_string(),
            "首请求携带全量快照: {}",
            two[0]
        );
        assert!(
            two[1]["systemPrompt"].is_null() && two[1]["tools"].is_null(),
            "信封未变 → 省略快照: {}",
            two[1]
        );
        assert!(two[1]["systemChars"].is_u64(), "字符数恒在场");

        // header 变更(set_header)→ 再次携带
        let mut h2 = header();
        h2.system = "changed-prompt".into();
        engine.set_header(h2);
        engine
            .run_turn(
                "three",
                None,
                &[],
                &[],
                &mut transport,
                &mut tools,
                &clock,
                &mut |_| {},
            )
            .await
            .unwrap();
        let three = audits(&log);
        assert_eq!(three.len(), 3);
        assert_eq!(three[2]["systemPrompt"].as_str(), Some("changed-prompt"));
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use crate::cancel::CancelToken;
    use crate::tools::{ToolCallRequest, ToolOutput};
    use crate::transport::LlmEvent;
    use std::pin::Pin;
    use std::sync::Arc;

    /// 脚本化 transport(整批,默认 stream_events 转发;顺序保持)。
    /// 外层 Vec = 多批(每次 stream 调用弹出一批;工具步的回合需要
    /// 「工具调用批 + 收尾批」两批);批耗尽后返回带内容的收尾批——
    /// 空响应已是可重试失败,不能再用空脚本收尾
    struct ScriptedTransport(Vec<Vec<LlmEvent>>);

    impl LlmTransport for ScriptedTransport {
        async fn stream(
            &mut self,
            _header: &RequestHeader,
            _messages: &Value,
        ) -> Result<Vec<LlmEvent>, TransportError> {
            Ok(if self.0.is_empty() {
                vec![LlmEvent::Chunk("done".into()), LlmEvent::Done]
            } else {
                self.0.remove(0)
            })
        }
    }

    impl crate::summarizer::Summarizer for ScriptedTransport {
        fn summarize<'a>(
            &'a mut self,
            _header: &'a RequestHeader,
            _messages: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    /// 慢流 transport:100 条推理增量,每 30ms 一条(3s 总长)
    struct SlowReasoningStream;

    impl LlmTransport for SlowReasoningStream {
        async fn stream(
            &mut self,
            _header: &RequestHeader,
            _messages: &Value,
        ) -> Result<Vec<LlmEvent>, TransportError> {
            Err(TransportError::Other("测试不走批量路径".into()))
        }

        async fn stream_events(
            &mut self,
            _header: &RequestHeader,
            _messages: &Value,
            tx: tokio::sync::mpsc::UnboundedSender<LlmEvent>,
        ) -> Result<(), TransportError> {
            for i in 0..100 {
                if tx.send(LlmEvent::Reasoning(format!("段{i}"))).is_err() {
                    break; // 消费侧已断开(取消丢弃 future 的正常路径)
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
            Ok(())
        }
    }

    impl crate::summarizer::Summarizer for SlowReasoningStream {
        fn summarize<'a>(
            &'a mut self,
            _header: &'a RequestHeader,
            _messages: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    /// 静默流 transport:发出首帧后长时间无事件(模拟 provider/网络停顿,
    /// 「卡住了?」出现的那一刻——没有 chunk 在路上)。
    struct StallingStream;

    impl LlmTransport for StallingStream {
        async fn stream(
            &mut self,
            _header: &RequestHeader,
            _messages: &Value,
        ) -> Result<Vec<LlmEvent>, TransportError> {
            Err(TransportError::Other("测试不走批量路径".into()))
        }

        async fn stream_events(
            &mut self,
            _header: &RequestHeader,
            _messages: &Value,
            tx: tokio::sync::mpsc::UnboundedSender<LlmEvent>,
        ) -> Result<(), TransportError> {
            // 发一个 reasoning 帧(进入流态),随后无限期沉默
            let _ = tx.send(LlmEvent::Reasoning("开始".into()));
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    impl crate::summarizer::Summarizer for StallingStream {
        fn summarize<'a>(
            &'a mut self,
            _header: &'a RequestHeader,
            _messages: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    struct NoTools;

    impl crate::tools::ToolPort for NoTools {
        async fn execute(&mut self, _call: &ToolCallRequest) -> ToolOutput {
            ToolOutput {
                output: String::new(),
                success: true,
                ..Default::default()
            }
        }
    }

    fn header() -> RequestHeader {
        RequestHeader {
            model: "test-model".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: Vec::new(),
        }
    }

    /// 推理增量逐条落档:Think 随流生长(此前攒到首个内容帧才
    /// 一次性 flush,推理期 UI 全空 =「Think 等输出完才显示」)
    #[tokio::test]
    async fn reasoning_deltas_commit_incrementally() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut engine = LoopEngine::new(header(), Arc::clone(&log));
        let mut transport = ScriptedTransport(vec![vec![
            LlmEvent::Reasoning("思".into()),
            LlmEvent::Reasoning("考".into()),
            LlmEvent::Chunk("答".into()),
            LlmEvent::AssistantMessage(serde_json::json!({ "content": "答" })),
            LlmEvent::Done,
        ]]);
        let mut seen: Vec<String> = Vec::new();
        let clock = || 1i64;
        {
            let mut sink = |ev: &EventEnvelope| seen.push(ev.r#type.clone());
            engine
                .run_turn(
                    "q",
                    None,
                    &[],
                    &[],
                    &mut transport,
                    &mut NoTools,
                    &clock,
                    &mut sink,
                )
                .await
                .unwrap();
        }
        let reasoning_count = seen.iter().filter(|t| *t == "assistant/reasoning").count();
        assert!(reasoning_count >= 2, "推理应逐条落档: {seen:?}");
        let first_reasoning = seen
            .iter()
            .position(|t| t == "assistant/reasoning")
            .unwrap();
        let first_chunk = seen
            .iter()
            .position(|t| t == "assistant/chunk")
            .expect("chunk");
        assert!(first_reasoning < first_chunk, "推理先于内容: {seen:?}");
        let l = log.lock().unwrap();
        let texts: Vec<&str> = l
            .iter()
            .filter(|ev| ev.r#type == "assistant/reasoning")
            .map(|ev| ev.data["text"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(texts, vec!["思", "考"], "增量原样(不合并): {texts:?}");
    }

    /// usage 合并:null 毒化帧不再顶掉真实用量,transport 附带的 ttft
    /// 并入同一对象(request-done 携带完整用量;统计条输入/输出/首 token 的数据源)
    #[tokio::test]
    async fn usage_merge_survives_null_poison_frame() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut engine = LoopEngine::new(header(), Arc::clone(&log));
        let mut transport = ScriptedTransport(vec![vec![
            LlmEvent::Usage(serde_json::Value::Null),
            LlmEvent::Chunk("答".into()),
            LlmEvent::AssistantMessage(serde_json::json!({ "content": "答" })),
            LlmEvent::Usage(serde_json::json!({ "prompt_tokens": 10, "completion_tokens": 5 })),
            LlmEvent::Usage(serde_json::json!({ "ttftMs": 120 })),
            LlmEvent::Done,
        ]]);
        let clock = || 1i64;
        {
            let mut sink = |_: &EventEnvelope| {};
            engine
                .run_turn(
                    "q",
                    None,
                    &[],
                    &[],
                    &mut transport,
                    &mut NoTools,
                    &clock,
                    &mut sink,
                )
                .await
                .unwrap();
        }
        let l = log.lock().unwrap();
        let done = l
            .iter()
            .find(|ev| ev.r#type == "audit/call" && ev.data["operation"] == "request-done")
            .expect("request-done 落档");
        assert_eq!(
            done.data["detail"]["usage"],
            serde_json::json!({ "prompt_tokens": 10, "completion_tokens": 5, "ttftMs": 120 }),
            "null 帧让位,真实用量与 ttft 合并"
        );
    }

    /// 渲染意图双事件落档:call 侧(present_call)与 result 侧
    /// (ToolOutput.view)各自在场;无视图工具零噪音
    #[tokio::test]
    async fn tool_view_persists_on_call_and_result_events() {
        use crate::presentation::{FileDiff, ToolView};

        struct DiffTool;
        impl crate::tools::ToolPort for DiffTool {
            fn specs(&self) -> Vec<serde_json::Value> {
                serde_json::json!([{ "type": "function",
                    "function": { "name": "file_edit", "parameters": {} } }])
                .as_array()
                .cloned()
                .unwrap_or_default()
            }
            async fn execute(&mut self, _call: &ToolCallRequest) -> ToolOutput {
                ToolOutput {
                    output: "edited a.txt (1 replacement)".into(),
                    success: true,
                    view: Some(ToolView::Diff {
                        diffs: vec![FileDiff {
                            path: "a.txt".into(),
                            old_text: Some("old".into()),
                            new_text: "new".into(),
                        }],
                    }),
                    ..Default::default()
                }
            }
            fn present_call(&self, _call: &ToolCallRequest) -> Option<ToolView> {
                Some(ToolView::Diff {
                    diffs: vec![FileDiff {
                        path: "a.txt".into(),
                        old_text: Some("old".into()),
                        new_text: "new".into(),
                    }],
                })
            }
        }

        struct PlainTool;
        impl crate::tools::ToolPort for PlainTool {
            fn specs(&self) -> Vec<serde_json::Value> {
                serde_json::json!([{ "type": "function",
                    "function": { "name": "plain", "parameters": {} } }])
                .as_array()
                .cloned()
                .unwrap_or_default()
            }
            async fn execute(&mut self, _call: &ToolCallRequest) -> ToolOutput {
                ToolOutput {
                    output: "ok".into(),
                    success: true,
                    ..Default::default()
                }
            }
        }

        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut engine = LoopEngine::new(header(), Arc::clone(&log));
        let mut transport = ScriptedTransport(vec![
            vec![
                LlmEvent::AssistantMessage(serde_json::json!({
                    "content": "",
                    "tool_calls": [ { "id": "c1", "name": "file_edit",
                        "arguments": { "path": "a.txt", "old_text": "old", "new_text": "new" } } ],
                })),
                LlmEvent::Done,
            ],
            vec![LlmEvent::Chunk("edited".into()), LlmEvent::Done],
        ]);
        let mut plain_transport = ScriptedTransport(vec![
            vec![
                LlmEvent::AssistantMessage(serde_json::json!({
                    "content": "",
                    "tool_calls": [ { "id": "c2", "name": "plain", "arguments": {} } ],
                })),
                LlmEvent::Done,
            ],
            vec![LlmEvent::Chunk("done".into()), LlmEvent::Done],
        ]);
        let clock = || 1i64;
        engine
            .run_turn(
                "edit",
                None,
                &[],
                &[],
                &mut transport,
                &mut DiffTool,
                &clock,
                &mut |_| {},
            )
            .await
            .unwrap();
        engine
            .run_turn(
                "plain",
                None,
                &[],
                &[],
                &mut plain_transport,
                &mut PlainTool,
                &clock,
                &mut |_| {},
            )
            .await
            .unwrap();

        let l = log.lock().unwrap();
        let call = l.iter().find(|e| e.r#type == "tool/call").unwrap();
        assert_eq!(call.data["view"]["card"], "diff", "call 侧视图在场");
        assert_eq!(call.data["view"]["diffs"][0]["path"], "a.txt");
        let result = l.iter().find(|e| e.r#type == "tool/result").unwrap();
        assert_eq!(result.data["view"]["card"], "diff", "result 侧视图在场");

        // 无视图工具:两事件均无 view 键(零噪音)
        let plain_call = l.iter().filter(|e| e.r#type == "tool/call").nth(1).unwrap();
        assert!(plain_call.data.get("view").is_none());
        let plain_result = l
            .iter()
            .filter(|e| e.r#type == "tool/result")
            .nth(1)
            .unwrap();
        assert!(plain_result.data.get("view").is_none());
    }

    /// 流中取消:每个到达事件都是安全点——断流收尾要快(不等流完),
    /// turn/end(cancelled) 落档,phase 复位
    #[tokio::test]
    async fn cancel_mid_stream_stops_promptly() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut engine = LoopEngine::new(header(), Arc::clone(&log));
        let token = CancelToken::new();
        engine.set_cancel(token.clone());
        let mut transport = SlowReasoningStream;
        let mut tools = NoTools;
        let clock = || 1i64;

        let canceller = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            token.cancel();
        });
        let started = std::time::Instant::now();
        let r = engine
            .run_turn(
                "q",
                None,
                &[],
                &[],
                &mut transport,
                &mut tools,
                &clock,
                &mut |_| {},
            )
            .await;
        let elapsed = started.elapsed();
        canceller.await.unwrap();
        assert!(
            matches!(r, Err(LoopError::Cancelled)),
            "取消应温和返回: {r:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(1000),
            "断流应即时(实际 {elapsed:?};流总长 3s)"
        );
        let l = log.lock().unwrap();
        assert!(
            l.iter()
                .any(|ev| ev.r#type == "turn/end" && ev.data.get("cancelled").is_some())
        );
        assert_eq!(engine.phase(), Phase::Idle, "取消后相位复位");
    }

    /// 流中**停顿**(无事件到达)时取消:provider/网络卡住、「卡住了?」
    /// 出现的瞬间没有 chunk 在路上——旧实现对取消只在 `rx.recv()` 事件臂
    /// 检查,无事件则 select 永久悬挂 → 引擎不返回 → driver 不推
    /// running:false → UI 动画不停。回归:取消须**不依赖下一个事件**仍中断。
    #[tokio::test]
    async fn cancel_during_stream_stall_interrupts_without_next_event() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut engine = LoopEngine::new(header(), Arc::clone(&log));
        let token = CancelToken::new();
        engine.set_cancel(token.clone());
        let mut transport = StallingStream;
        let mut tools = NoTools;
        let clock = || 1i64;

        let canceller = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            token.cancel();
        });
        let started = std::time::Instant::now();
        let r = engine
            .run_turn(
                "q",
                None,
                &[],
                &[],
                &mut transport,
                &mut tools,
                &clock,
                &mut |_| {},
            )
            .await;
        let elapsed = started.elapsed();
        canceller.await.unwrap();
        assert!(
            matches!(r, Err(LoopError::Cancelled)),
            "停顿中取消应温和返回: {r:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(1000),
            "停顿断流应即时(实际 {elapsed:?};transport 永不返回)"
        );
        let l = log.lock().unwrap();
        assert!(
            l.iter()
                .any(|ev| ev.r#type == "turn/end" && ev.data.get("cancelled").is_some())
        );
        assert_eq!(engine.phase(), Phase::Idle, "取消后相位复位");
    }

    // ---- 失败自动重试(策略注入 1ms 级短延迟)----

    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn count_events(log: &Arc<Mutex<EventLog>>, ty: &str) -> usize {
        log.lock()
            .unwrap()
            .iter()
            .filter(|e| e.r#type == ty)
            .count()
    }

    fn count_audits(log: &Arc<Mutex<EventLog>>) -> usize {
        log.lock()
            .unwrap()
            .iter()
            .filter(|e| e.r#type == "audit/call" && e.data["boundary"] == "llm")
            .count()
    }

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            initial_delay_ms: 1,
            ..RetryPolicy::default()
        }
    }

    /// 依序脚本化传输结果(Ok 事件批 / Err 分类错误);记录尝试次数
    struct RetryScriptTransport {
        script: Vec<Result<Vec<LlmEvent>, TransportError>>,
        attempts: AtomicUsize,
    }

    impl LlmTransport for RetryScriptTransport {
        async fn stream(
            &mut self,
            _header: &RequestHeader,
            _messages: &Value,
        ) -> Result<Vec<LlmEvent>, TransportError> {
            self.attempts.fetch_add(1, AtomicOrdering::SeqCst);
            match self.script.is_empty() {
                true => Ok(vec![LlmEvent::Chunk("兜底".into()), LlmEvent::Done]),
                false => self.script.remove(0),
            }
        }
    }

    impl crate::summarizer::Summarizer for RetryScriptTransport {
        fn summarize<'a>(
            &'a mut self,
            _header: &'a RequestHeader,
            _messages: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    /// 逐事件流式传输:第 1 次尝试落「残段 chunk + reasoning」后断流,
    /// 第 2 次成功——验证 stream-reset 丢弃标记与残段不进最终消息
    struct MidStreamFailTransport {
        attempts: AtomicUsize,
    }

    impl LlmTransport for MidStreamFailTransport {
        async fn stream(
            &mut self,
            _header: &RequestHeader,
            _messages: &Value,
        ) -> Result<Vec<LlmEvent>, TransportError> {
            Err(TransportError::Other("测试不走批量路径".into()))
        }

        async fn stream_events(
            &mut self,
            _header: &RequestHeader,
            _messages: &Value,
            tx: tokio::sync::mpsc::UnboundedSender<LlmEvent>,
        ) -> Result<(), TransportError> {
            let n = self.attempts.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            if n == 1 {
                let _ = tx.send(LlmEvent::Chunk("残段".into()));
                let _ = tx.send(LlmEvent::Reasoning("想".into()));
                return Err(TransportError::Transport("断流".into()));
            }
            let _ = tx.send(LlmEvent::Chunk("ok".into()));
            let _ = tx.send(LlmEvent::Done);
            Ok(())
        }
    }

    impl crate::summarizer::Summarizer for MidStreamFailTransport {
        fn summarize<'a>(
            &'a mut self,
            _header: &'a RequestHeader,
            _messages: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    /// 瞬态失败后成功:重试事件成对(retry/retry-started),最终消息
    /// 只含成功尝试文本
    #[tokio::test]
    async fn retry_succeeds_after_transient_failures() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut engine = LoopEngine::new(header(), Arc::clone(&log));
        engine.set_retry_policy(fast_policy());
        let mut transport = RetryScriptTransport {
            script: vec![
                Err(TransportError::Transport("连接失败".into())),
                Err(TransportError::Server {
                    status: 502,
                    body: "bad gateway".into(),
                }),
                Ok(vec![LlmEvent::Chunk("答案".into()), LlmEvent::Done]),
            ],
            attempts: AtomicUsize::new(0),
        };
        let mut tools = NoTools;
        let clock = || 1i64;
        let outcome = engine
            .run_turn(
                "hi",
                None,
                &[],
                &[],
                &mut transport,
                &mut tools,
                &clock,
                &mut |_| {},
            )
            .await
            .expect("瞬态失败后应成功");
        assert_eq!(outcome.assistant_message, "答案", "最终消息 = 成功尝试文本");
        assert_eq!(transport.attempts.load(AtomicOrdering::SeqCst), 3);
        assert_eq!(count_events(&log, "llm/retry"), 2, "两次失败各一条重试排定");
        assert_eq!(count_events(&log, "llm/retry-started"), 2, "退避结束各一条");
        assert_eq!(count_events(&log, "turn/error"), 0);
        // audit:每次尝试一对 request/request-done(3 次尝试)
        assert_eq!(count_audits(&log), 6, "三次尝试各一对审计");
    }

    /// 401 直通:零重试,turn/error 携带 AUTH 码
    #[tokio::test]
    async fn auth_failure_passes_through_without_retry() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut engine = LoopEngine::new(header(), Arc::clone(&log));
        engine.set_retry_policy(fast_policy());
        let mut transport = RetryScriptTransport {
            script: vec![Err(TransportError::Auth {
                status: 401,
                body: "nope".into(),
            })],
            attempts: AtomicUsize::new(0),
        };
        let mut tools = NoTools;
        let clock = || 1i64;
        let r = engine
            .run_turn(
                "hi",
                None,
                &[],
                &[],
                &mut transport,
                &mut tools,
                &clock,
                &mut |_| {},
            )
            .await;
        assert!(matches!(
            r,
            Err(LoopError::Transport(TransportError::Auth { .. }))
        ));
        assert_eq!(
            transport.attempts.load(AtomicOrdering::SeqCst),
            1,
            "401 不重试"
        );
        assert_eq!(count_events(&log, "llm/retry"), 0);
        let code = log
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.r#type == "turn/error")
            .expect("turn/error 落档")
            .data["code"]
            .clone();
        assert_eq!(code, "AUTH");
    }

    /// 空响应:可重试分类;耗尽后 turn/error 携带 EMPTY_RESPONSE
    /// (桌面层重试行与「回合出错」通告并存)
    #[tokio::test]
    async fn empty_response_retries_then_exhausts() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut engine = LoopEngine::new(header(), Arc::clone(&log));
        engine.set_retry_policy(RetryPolicy {
            max_retries: 2,
            initial_delay_ms: 1,
            ..RetryPolicy::default()
        });
        let mut transport = RetryScriptTransport {
            // 恒空流(Done 哨兵、零内容)
            script: vec![Ok(vec![LlmEvent::Done]); 8],
            attempts: AtomicUsize::new(0),
        };
        let mut tools = NoTools;
        let clock = || 1i64;
        let r = engine
            .run_turn(
                "hi",
                None,
                &[],
                &[],
                &mut transport,
                &mut tools,
                &clock,
                &mut |_| {},
            )
            .await;
        assert!(matches!(
            r,
            Err(LoopError::Transport(TransportError::EmptyResponse))
        ));
        // 首请求 + 2 重试 = 3 次尝试;耗尽不再第 4 次
        assert_eq!(transport.attempts.load(AtomicOrdering::SeqCst), 3);
        assert_eq!(count_events(&log, "llm/retry"), 2);
        assert_eq!(count_events(&log, "llm/retry-started"), 2);
        let code = log
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.r#type == "turn/error")
            .expect("turn/error 落档")
            .data["code"]
            .clone();
        assert_eq!(code, "EMPTY_RESPONSE");
    }

    /// mid-stream 断流:已落档残段以 stream-reset 标记丢弃,最终
    /// assistant/message 只含成功尝试文本(记录优先的追加式丢弃)
    #[tokio::test]
    async fn mid_stream_failure_discards_partial_chunks() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut engine = LoopEngine::new(header(), Arc::clone(&log));
        engine.set_retry_policy(fast_policy());
        let mut transport = MidStreamFailTransport {
            attempts: AtomicUsize::new(0),
        };
        let mut tools = NoTools;
        let clock = || 1i64;
        let outcome = engine
            .run_turn(
                "hi",
                None,
                &[],
                &[],
                &mut transport,
                &mut tools,
                &clock,
                &mut |_| {},
            )
            .await
            .expect("断流后重试应成功");
        assert_eq!(outcome.assistant_message, "ok", "残段不得混入最终消息");
        assert_eq!(count_events(&log, "assistant/stream-reset"), 1);
        assert_eq!(count_events(&log, "llm/retry"), 1);
    }

    /// 退避等待中取消:立即中止重试转 aborted,不再发下一次请求
    #[tokio::test]
    async fn cancel_during_backoff_aborts_without_next_request() {
        let log = Arc::new(Mutex::new(EventLog::new()));
        let mut engine = LoopEngine::new(header(), Arc::clone(&log));
        engine.set_retry_policy(RetryPolicy {
            initial_delay_ms: 5_000,
            ..RetryPolicy::default()
        });
        let token = CancelToken::new();
        engine.set_cancel(token.clone());
        let mut transport = RetryScriptTransport {
            script: vec![Err(TransportError::Transport("boom".into()))],
            attempts: AtomicUsize::new(0),
        };
        let mut tools = NoTools;
        let clock = || 1i64;
        // 首次失败进入 5s 退避;50ms 后取消触发
        {
            let token = token.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                token.cancel();
            });
        }
        let r = engine
            .run_turn(
                "hi",
                None,
                &[],
                &[],
                &mut transport,
                &mut tools,
                &clock,
                &mut |_| {},
            )
            .await;
        assert!(
            matches!(r, Err(LoopError::Cancelled)),
            "退避中取消 → aborted: {r:?}"
        );
        assert_eq!(
            transport.attempts.load(AtomicOrdering::SeqCst),
            1,
            "不再发请求"
        );
        assert_eq!(count_events(&log, "llm/retry"), 1, "重试已排定");
        assert_eq!(count_events(&log, "llm/retry-started"), 0, "退避未结束");
        let l = log.lock().unwrap();
        assert!(
            l.iter()
                .any(|ev| ev.r#type == "turn/end" && ev.data.get("cancelled").is_some())
        );
    }
}
