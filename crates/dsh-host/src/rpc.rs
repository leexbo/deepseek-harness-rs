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
    /// header 重建器(每 turn 前按日志态重建 prompt;None = 固定 header)
    header_rebuilder: Option<HeaderRebuilder>,
    /// 软取消令牌(turn 执行中可被 `cancel` 方法/外部触发打断)
    cancel: CancelToken,
    /// 实时下行通道(serve 层注入;直连 handle 调用时为 None,
    /// 通知以返回值聚合)
    downlink: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
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
            header_rebuilder: None,
        }
    }

    /// 共享日志视图(测试/宿主侧派生用)
    pub fn log(&self) -> Arc<Mutex<EventLog>> {
        Arc::clone(&self.log)
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
            "approve" => {
                // 批准最近一次提交的计划:plan/approved + 回标准态
                let plan = {
                    let log = self
                        .log
                        .lock()
                        .map_err(|_| JsonRpcError::internal("log 锁中毒"))?;
                    let submitted = log
                        .iter()
                        .rev()
                        .find(|e| e.r#type == "plan/submitted")
                        .ok_or_else(|| JsonRpcError::invalid_params("没有待批准的计划"))?;
                    let approved_after = log
                        .iter()
                        .any(|e| e.r#type == "plan/approved" && e.seq > submitted.seq);
                    if approved_after {
                        return Err(JsonRpcError::invalid_params("没有待批准的计划"));
                    }
                    submitted.data["plan"].clone()
                };
                self.append_session_event("plan/approved", plan.clone())?;
                let seq =
                    self.append_session_event("session/mode", json!({ "mode": "standard" }))?;
                Ok((json!({ "approved": true, "seq": seq }), Vec::new()))
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

    /// 注入 header 重建器:每 turn 前按日志态(plan 模式/活跃计划)
    /// 重建 prompt——装配层持有 prompt 组装,网关不感知具体策略
    pub fn set_header_rebuilder(&mut self, rebuild: HeaderRebuilder) {
        self.header_rebuilder = Some(rebuild);
    }

    async fn do_turn(&mut self, params: &Value) -> Result<(Value, Vec<Value>), JsonRpcError> {
        let input = params["input"]
            .as_str()
            .ok_or_else(|| JsonRpcError::invalid_params("turn 需要 string 参数 input"))?;
        // 每 turn 复位令牌(上一回合的取消不泄漏到本回合)
        self.cancel.reset();
        // header 重建器注入时按日志态(plan 模式/活跃计划)重建 prompt
        if let Some(rebuild) = &self.header_rebuilder {
            let header = {
                let log = self
                    .log
                    .lock()
                    .map_err(|_| JsonRpcError::internal("log 锁中毒"))?;
                rebuild(&log)
            };
            self.engine.set_header(header);
        }

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
    gateway.lock().await.set_downlink(down_tx);

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
