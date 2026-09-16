//! JSON-RPC 网关:stdio 行分帧的服务形态。
//!
//! 协议:JSON-RPC 2.0,一行一消息(请求/响应/通知)。
//! 下行(通知)在 turn 执行期间产生:每个落日志的事件以 `event` 通知
//! 下发——这是 web WS 下行通道的同构前身(stdio 只是把 WS 帧换成行)。
//!
//! 方法面:
//! - `turn` {input}:驱动一个 turn → {assistantMessage, seqRange};
//! - `log`:当前事件日志快照(重放材料,JSONL 等价物);
//! - `attribution`:E5 归因链(审计消费面);
//! - `status`:phase 与高水位;
//! - `shutdown`:响应后退出 serve 循环。
//!
//! 错误码:-32700 解析失败、-32601 方法不存在、-32602 参数无效、
//! -32603 内部错误。

use std::sync::{Arc, Mutex};

use dsh_agent_loop::{
    CancelToken, LlmTransport, LoopEngine, NoTools, RequestHeader, Summarizer, ToolPort,
};
use dsh_session::{EventEnvelope, EventLog, attribution_chain};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::persistence::JsonlBackend;

/// JSON-RPC 错误(dispatch 层的强类型形态)
#[derive(Debug, thiserror::Error, PartialEq)]
#[error("jsonrpc {code}: {message}")]
pub struct JsonRpcError {
    /// JSON-RPC 错误码
    pub code: i64,
    /// 错误消息
    pub message: String,
}

impl JsonRpcError {
    /// 方法不存在
    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
        }
    }

    /// 参数无效
    fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
        }
    }

    /// 内部错误
    fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: msg.into(),
        }
    }

    fn to_json(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

/// header 重建器:每 turn 前按日志态(plan 模式/活跃计划)重建 prompt
pub type HeaderRebuilder = Box<dyn Fn(&EventLog) -> RequestHeader + Send>;

/// Gateway 计划评审通道:exit_plan_mode 的 turn 内阻塞评审(port 侧)
/// + `approve`/`decline` RPC 直答。
///
/// turn RPC 后台执行期间持有网关锁,经锁应答会死锁(评审等应答、应答等
/// 锁)——通道自持状态(std 锁 + oneshot),serve 层绕网关锁直答(同
/// `cancel` 的令牌直通形态)。plan 族事件由通道落档共享日志(信封构造在
/// dsh-plan,与 dsh-core/CLI 同一语义源),状态变化经下行通道通知。
#[derive(Clone)]
pub struct PlanReviewChannel {
    inner: Arc<ReviewInner>,
}

struct ReviewInner {
    log: Arc<Mutex<EventLog>>,
    backend: JsonlBackend,
    /// 在审评审的应答通道(评审打开期间 Some)
    tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<dsh_plan::PlanReviewDecision>>>,
    /// 下行通知通道(serve 层注入;None = 不通知)
    downlink: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<Value>>>,
    /// 软取消令牌(serve 层注入;评审等待与取消竞速)
    cancel: std::sync::Mutex<Option<CancelToken>>,
}

impl PlanReviewChannel {
    /// 以共享日志与持久化后端构建(log/backend 须与 engine 同一视图)
    pub fn new(log: Arc<Mutex<EventLog>>, backend: JsonlBackend) -> Self {
        Self {
            inner: Arc::new(ReviewInner {
                log,
                backend,
                tx: std::sync::Mutex::new(None),
                downlink: std::sync::Mutex::new(None),
                cancel: std::sync::Mutex::new(None),
            }),
        }
    }

    /// serve 层注入下行通知通道
    pub fn set_downlink(&self, tx: tokio::sync::mpsc::UnboundedSender<Value>) {
        *self
            .inner
            .downlink
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(tx);
    }

    /// serve 层注入软取消令牌(评审等待与取消竞速)
    pub fn set_cancel(&self, cancel: CancelToken) {
        *self.inner.cancel.lock().unwrap_or_else(|p| p.into_inner()) = Some(cancel);
    }

    /// 下行 JSON-RPC 通知
    fn notify(&self, method: &str, params: Value) {
        let guard = self
            .inner
            .downlink
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
        }
    }

    /// log-only 事件落档(锁内定 seq + 落盘;失败记日志不阻断评审)
    fn append(&self, ev: EventEnvelope) {
        let committed = self
            .inner
            .log
            .lock()
            .ok()
            .and_then(|mut l| l.append(ev).ok().and_then(|seq| l.get(seq).cloned()));
        if let Some(ev) = committed
            && let Err(e) = self.inner.backend.append(&ev)
        {
            eprintln!("[dsh-host] plan 事件落盘失败: {e}");
        }
    }

    /// 直答一次决定(Approve/Decline);false = 无在审评审
    fn answer(&self, decision: dsh_plan::PlanReviewDecision) -> bool {
        let mut guard = self.inner.tx.lock().unwrap_or_else(|p| p.into_inner());
        match guard.take() {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// `approve` RPC:批准在审评审
    pub fn approve(&self) -> bool {
        self.answer(dsh_plan::PlanReviewDecision::Approve)
    }

    /// `decline` RPC:拒绝在审评审(带可选反馈;留在 plan 模式)
    pub fn decline(&self, feedback: Option<String>) -> bool {
        self.answer(dsh_plan::PlanReviewDecision::Decline { feedback })
    }

    /// 评审是否在审(serve 层/测试探针)
    pub fn is_open(&self) -> bool {
        self.inner
            .tx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// 阻塞评审主体:submitted 落档 + 下行通知 → 等应答(与取消竞速)→
    /// 终局事件 + 通知
    async fn run(&self, plan: &str) -> Result<dsh_plan::PlanReviewDecision, String> {
        let now = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        };
        self.append(dsh_plan::plan_envelope("plan/submitted", plan, None, now()));
        self.notify("plan/review", json!({ "state": "submitted", "plan": plan }));
        let (tx, rx) = tokio::sync::oneshot::channel();
        *self.inner.tx.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
        let cancel = self
            .inner
            .cancel
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let answer = match cancel {
            Some(cancel) => {
                tokio::select! {
                    res = rx => res.map_err(Some),
                    _ = cancel.cancelled() => Err(None),
                }
            }
            None => rx.await.map_err(Some),
        };
        self.inner
            .tx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        match answer {
            Ok(decision) => {
                let outcome = match &decision {
                    dsh_plan::PlanReviewDecision::Approve => {
                        self.append(dsh_plan::plan_envelope("plan/approved", plan, None, now()));
                        self.append(dsh_plan::mode_envelope("standard", now()));
                        "approved"
                    }
                    dsh_plan::PlanReviewDecision::Decline { feedback } => {
                        self.append(dsh_plan::plan_envelope(
                            "plan/declined",
                            plan,
                            feedback.as_deref(),
                            now(),
                        ));
                        "declined"
                    }
                };
                self.notify("plan/review", json!({ "state": outcome }));
                Ok(decision)
            }
            // 通道死(None)/取消:统一关闭评审(等待用户说话)
            Err(_) => {
                self.append(dsh_plan::plan_envelope("plan/cancelled", plan, None, now()));
                self.notify("plan/review", json!({ "state": "cancelled" }));
                Err(dsh_plan::DISMISSED_REVIEW_ERROR.to_string())
            }
        }
    }
}

impl dsh_plan::PlanReviewPort for PlanReviewChannel {
    fn review(
        &self,
        _session_id: &str,
        plan: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<dsh_plan::PlanReviewDecision, String>> + Send>,
    > {
        let channel = self.clone();
        let plan = plan.to_string();
        Box::pin(async move { channel.run(&plan).await })
    }
}

/// JSON-RPC 网关:engine + transport + 持久化 + 工具的装配单元。
///
/// 泛型:`T` 出网传输(经不变式闸门包裹),`TOOLS` 工具集
/// (默认 [`NoTools`];CLI 装配可传真实注册表)。
pub struct Gateway<T, TOOLS = NoTools> {
    engine: LoopEngine,
    transport: T,
    tools: TOOLS,
    backend: JsonlBackend,
    log: Arc<Mutex<EventLog>>,
    /// 软取消令牌(turn 执行中可被 `cancel` 方法/外部触发打断)
    cancel: CancelToken,
    /// 实时下行通道(serve 层注入;直连 handle 调用时为 None,
    /// 通知以返回值聚合)
    downlink: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
    /// 计划评审通道(None = 工具面无 plan;serve 层绕锁直答用)
    plan_review: Option<PlanReviewChannel>,
}

impl<T, TOOLS> Gateway<T, TOOLS> {
    /// 装配:header(模型/system prompt)、传输、工具集、持久化后端。
    pub fn new(header: RequestHeader, transport: T, tools: TOOLS, backend: JsonlBackend) -> Self {
        Self::with_log(
            header,
            transport,
            tools,
            backend,
            Arc::new(Mutex::new(EventLog::new())),
        )
    }

    /// 以外部共享日志构建(工具集含日志依赖项时使用:todo/plan/goal
    /// 的状态恢复需与 engine/闸门同一日志视图)
    pub fn with_log(
        header: RequestHeader,
        transport: T,
        tools: TOOLS,
        backend: JsonlBackend,
        log: Arc<Mutex<EventLog>>,
    ) -> Self {
        let cancel = CancelToken::new();
        let mut engine = LoopEngine::new(header, Arc::clone(&log));
        engine.set_cancel(cancel.clone());
        Self {
            engine,
            transport,
            tools,
            backend,
            log,
            cancel,
            downlink: None,
            plan_review: None,
        }
    }

    /// 共享日志视图(测试/宿主侧派生用)
    pub fn log(&self) -> Arc<Mutex<EventLog>> {
        Arc::clone(&self.log)
    }

    /// 装配当前模型上下文窗口(压缩阈值/保留尾按窗口占比重算;
    /// 宿主从 dsh.toml `context_window` 解析后注入)
    pub fn set_context_window(&mut self, window: u64) {
        self.engine.set_context_window(window);
    }

    /// 软取消令牌(serve 层/外部在 turn 执行中触发取消)
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// 替换软取消令牌(外部令牌接入:transport/工具挂在同一令牌上)
    pub fn set_cancel_token(&mut self, token: CancelToken) {
        self.cancel = token.clone();
        self.engine.set_cancel(token);
    }

    /// 注入实时下行通道(serve 层;通知在事件发生时即时流出,
    /// 而非随响应批量返回)
    pub fn set_downlink(&mut self, tx: tokio::sync::mpsc::UnboundedSender<Value>) {
        self.downlink = Some(tx);
    }

    /// 下行通道句柄(None = 未注入,直连 handle 形态)
    pub fn downlink_tx(&self) -> Option<tokio::sync::mpsc::UnboundedSender<Value>> {
        self.downlink.clone()
    }

    /// 装配计划评审通道(工具面含 plan 时;serve 层绕锁直答与
    /// exit_plan_mode 的 turn 内评审共用)
    pub fn set_plan_review(&mut self, channel: PlanReviewChannel) {
        self.plan_review = Some(channel);
    }

    /// 评审通道句柄(serve 层绕网关锁直答 approve/decline;None = 无 plan)
    pub fn plan_review_channel(&self) -> Option<PlanReviewChannel> {
        self.plan_review.clone()
    }
}

impl<T: LlmTransport + Summarizer + Send, TOOLS: ToolPort + Send> Gateway<T, TOOLS> {
    /// 分发一次调用。
    ///
    /// 返回 (result, notifications):通知在调用期间产生,由 serve 层
    /// 先于响应写出(下行时序 = 事件发生时序)。
    pub async fn handle(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Result<(Value, Vec<Value>), JsonRpcError> {
        match method {
            "turn" => self.do_turn(params).await,
            "log" => {
                let log = self
                    .log
                    .lock()
                    .map_err(|_| JsonRpcError::internal("log 锁中毒"))?;
                let events: Vec<&EventEnvelope> = log.iter().collect();
                Ok((
                    json!({ "events": events, "highWater": log.high_water() }),
                    Vec::new(),
                ))
            }
            "attribution" => {
                let log = self
                    .log
                    .lock()
                    .map_err(|_| JsonRpcError::internal("log 锁中毒"))?;
                Ok((json!({ "chain": attribution_chain(&log) }), Vec::new()))
            }
            "status" => {
                let high_water = self
                    .log
                    .lock()
                    .map_err(|_| JsonRpcError::internal("log 锁中毒"))?
                    .high_water();
                Ok((
                    json!({ "phase": format!("{:?}", self.engine.phase()), "highWater": high_water }),
                    Vec::new(),
                ))
            }
            "cancel" => {
                // 软取消:安全点生效(engine step 边界/工具执行前后)
                self.cancel.cancel();
                Ok((json!({ "cancelled": true }), Vec::new()))
            }
            "mode" => {
                // 会话模式切换:session/mode 事件入日志;
                // 下次 turn 前 header 重建器(若注入)按日志态生效
                let mode = params["mode"]
                    .as_str()
                    .ok_or_else(|| JsonRpcError::invalid_params("mode 需要 string 参数 mode"))?;
                if !matches!(mode, "standard" | "plan") {
                    return Err(JsonRpcError::invalid_params(
                        "mode 取值必须为 standard 或 plan",
                    ));
                }
                let seq = self.append_session_event("session/mode", json!({ "mode": mode }))?;
                Ok((json!({ "mode": mode, "seq": seq }), Vec::new()))
            }
            "approve" | "decline" => {
                // 应答在审评审(turn 内阻塞评审;经评审通道直答——serve 层
                // 绕网关锁调用,直连 handle 形态也能走通)。事件落档在通道
                // 内(批准切 standard;拒绝留 plan 模式)。
                let Some(channel) = self.plan_review.clone() else {
                    return Err(JsonRpcError::invalid_params(
                        "没有计划评审通道(工具面无 plan 组件)",
                    ));
                };
                let answered = if method == "approve" {
                    channel.approve()
                } else {
                    let feedback = params["feedback"]
                        .as_str()
                        .map(str::to_string)
                        .filter(|t| !t.trim().is_empty());
                    channel.decline(feedback)
                };
                if !answered {
                    return Err(JsonRpcError::invalid_params("没有在审的计划"));
                }
                Ok((
                    json!({ "answered": true, "decision": if method == "approve" { "approved" } else { "declined" } }),
                    Vec::new(),
                ))
            }
            "shutdown" => Ok((json!({ "stopping": true }), Vec::new())),
            other => Err(JsonRpcError::method_not_found(other)),
        }
    }

    /// 会话级事件追加(mode/approve 等):engine 唯一写入口 + 下行通知
    fn append_session_event(&mut self, r#type: &str, data: Value) -> Result<u64, JsonRpcError> {
        let clock = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        };
        let downlink = self.downlink.clone();
        let backend = &self.backend;
        let mut sink = |ev: &EventEnvelope| {
            let notification = json!({
                "jsonrpc": "2.0",
                "method": "event",
                "params": { "seq": ev.seq, "type": ev.r#type },
            });
            if let Some(tx) = &downlink {
                let _ = tx.send(notification);
            }
            if let Err(e) = backend.append(ev) {
                eprintln!("[gateway] 持久化失败:{e}");
            }
        };
        self.engine
            .commit_session_event(r#type, data, &clock, &mut sink)
            .map_err(|e| {
                JsonRpcError::internal(format!("session event {rtype}: {e}", rtype = r#type))
            })
    }

    /// 注入 header 重建器:每 step 按日志态(plan 模式/活跃计划)重建
    /// prompt(引擎内逐 step 生效——装配层持有 prompt 组装,网关不感知
    /// 具体策略;turn 中途落档的状态变化立即反映到下一步)
    pub fn set_header_rebuilder(&mut self, rebuild: HeaderRebuilder) {
        self.engine.set_header_rebuilder(rebuild);
    }

    async fn do_turn(&mut self, params: &Value) -> Result<(Value, Vec<Value>), JsonRpcError> {
        let input = params["input"]
            .as_str()
            .ok_or_else(|| JsonRpcError::invalid_params("turn 需要 string 参数 input"))?;
        // 每 turn 复位令牌(上一回合的取消不泄漏到本回合)
        self.cancel.reset();
        // header 重建器已注入引擎(每 step 重建:turn 中途落档的模式/计划
        // 态——如评审批准切 standard——立即生效于下一步 prompt)

        let clock = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        };
        let mut notifications = Vec::new();
        let downlink = self.downlink.clone();
        let mut sink = |ev: &EventEnvelope| {
            // 下行通知 + 持久化(记录优先:通知/落盘都发生在处理之前)。
            // serve 层注入了通道则即时流出;直连调用聚合进返回值
            let notification = json!({
                "jsonrpc": "2.0",
                "method": "event",
                "params": { "seq": ev.seq, "type": ev.r#type },
            });
            match &downlink {
                Some(tx) => {
                    let _ = tx.send(notification);
                }
                None => notifications.push(notification),
            }
            if let Err(e) = self.backend.append(ev) {
                eprintln!("[gateway] 持久化失败:{e}");
            }
        };
        let outcome = self
            .engine
            .run_turn(
                input,
                None,
                &[],
                &[],
                &[],
                &mut self.transport,
                &mut self.tools,
                &clock,
                &mut sink,
            )
            .await
            .map_err(|e| JsonRpcError::internal(format!("turn: {e}")))?;
        Ok((
            json!({
                "assistantMessage": outcome.assistant_message,
                "seqRange": [outcome.seq_range.0, outcome.seq_range.1],
            }),
            notifications,
        ))
    }
}

/// stdio serve 循环:一行一消息;EOF 或 `shutdown` 即退出。
///
/// 并发模型:turn 请求在后台任务执行(持有网关锁),事件通知经下行通道
/// **实时**流出;读端不被长 turn 阻塞——`cancel` 请求可在 turn 执行中
/// 到达并触发软取消(web WS 下行的同构形态)。非 turn 方法内联处理。
///
/// 无 `id` 的入站消息按 JSON-RPC 规范视为通知(不回应);
/// 下行通知先于响应写出(通道先进先出保序)。
pub async fn serve_stdio<R, W, T, TOOLS>(
    gateway: Gateway<T, TOOLS>,
    reader: R,
    mut writer: W,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    T: LlmTransport + Summarizer + Send + 'static,
    TOOLS: ToolPort + Send + 'static,
{
    let gateway = std::sync::Arc::new(tokio::sync::Mutex::new(gateway));
    let cancel = gateway.lock().await.cancel_token();
    let (down_tx, mut down_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    gateway.lock().await.set_downlink(down_tx.clone());
    // 评审通道接线(下行通知 + 取消竞速);approve/decline 经它绕网关锁
    // 直答(turn 后台任务持有网关锁,经锁应答会与阻塞评审互等)
    let plan_review = gateway.lock().await.plan_review_channel();
    if let Some(ch) = &plan_review {
        ch.set_downlink(down_tx);
        ch.set_cancel(cancel.clone());
    }

    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        tokio::select! {
            biased;
            maybe = down_rx.recv() => {
                let Some(message) = maybe else { continue };
                write_line(&mut writer, message).await?;
                writer.flush().await?;
            }
            read = reader.read_line(&mut line) => {
                let n = read?;
                if n == 0 {
                    return Ok(()); // EOF:客户端断开
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    line.clear();
                    continue;
                }
                let request: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        write_line(
                            &mut writer,
                            json!({
                                "jsonrpc": "2.0",
                                "id": Value::Null,
                                "error": { "code": -32700, "message": format!("parse error: {e}") },
                            }),
                        )
                        .await?;
                        line.clear();
                        continue;
                    }
                };
                line.clear();
                let id = request.get("id").cloned();
                let method = request["method"].as_str().unwrap_or_default().to_string();
                let params = request.get("params").cloned().unwrap_or(Value::Null);

                match method.as_str() {
                    "turn" => {
                        // 后台执行:通知/响应都经下行通道写出(通道保序:
                        // 通知先于响应);读端继续服务 cancel 等并发请求
                        let gateway = std::sync::Arc::clone(&gateway);
                        let down_tx = gateway
                            .lock()
                            .await
                            .downlink_tx()
                            .expect("serve 已注入下行通道");
                        let id = id.clone();
                        tokio::spawn(async move {
                            let mut gateway = gateway.lock().await;
                            let message = match gateway.handle("turn", &params).await {
                                Ok((result, _)) => {
                                    json!({ "jsonrpc": "2.0", "id": id, "result": result })
                                }
                                Err(e) => {
                                    json!({ "jsonrpc": "2.0", "id": id, "error": e.to_json() })
                                }
                            };
                            let _ = down_tx.send(message);
                        });
                    }
                    "cancel" => {
                        // 不经网关锁:令牌共享,turn 执行中即可打断
                        cancel.cancel();
                        if let Some(id) = id {
                            write_line(
                                &mut writer,
                                json!({ "jsonrpc": "2.0", "id": id, "result": { "cancelled": true } }),
                            )
                            .await?;
                            writer.flush().await?;
                        }
                    }
                    "approve" | "decline" => {
                        // 不经网关锁:评审通道直答(在审的 turn 内评审正持有
                        // 网关锁等待应答,经锁应答 = 死锁)
                        let message = match (&plan_review, method.as_str()) {
                            (Some(ch), "approve") if ch.approve() => {
                                json!({ "jsonrpc": "2.0", "id": id, "result": { "answered": true, "decision": "approved" } })
                            }
                            (Some(ch), "decline") => {
                                let feedback = params["feedback"]
                                    .as_str()
                                    .map(str::to_string)
                                    .filter(|t| !t.trim().is_empty());
                                if ch.decline(feedback) {
                                    json!({ "jsonrpc": "2.0", "id": id, "result": { "answered": true, "decision": "declined" } })
                                } else {
                                    json!({ "jsonrpc": "2.0", "id": id, "error": JsonRpcError::invalid_params("没有在审的计划").to_json() })
                                }
                            }
                            (Some(_), _) => {
                                json!({ "jsonrpc": "2.0", "id": id, "error": JsonRpcError::invalid_params("没有在审的计划").to_json() })
                            }
                            (None, _) => {
                                json!({ "jsonrpc": "2.0", "id": id, "error": JsonRpcError::invalid_params("没有计划评审通道(工具面无 plan 组件)").to_json() })
                            }
                        };
                        write_line(&mut writer, message).await?;
                        writer.flush().await?;
                    }
                    _ => {
                        let outcome = gateway.lock().await.handle(&method, &params).await;
                        if let Some(id) = id {
                            let message = match outcome {
                                Ok((result, notifications)) => {
                                    // 直连聚合的通知(serve 模式下 turn 之外
                                    // 通常为空)先写出,保序语义一致
                                    for notification in &notifications {
                                        write_line(&mut writer, notification.clone()).await?;
                                    }
                                    json!({ "jsonrpc": "2.0", "id": id, "result": result })
                                }
                                Err(e) => {
                                    json!({ "jsonrpc": "2.0", "id": id, "error": e.to_json() })
                                }
                            };
                            write_line(&mut writer, message).await?;
                            writer.flush().await?;
                        }
                    }
                }
                if method == "shutdown" {
                    return Ok(());
                }
            }
        }
    }
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, value: Value) -> std::io::Result<()> {
    let mut text = serde_json::to_string(&value).map_err(std::io::Error::other)?;
    text.push('\n');
    writer.write_all(text.as_bytes()).await
}
