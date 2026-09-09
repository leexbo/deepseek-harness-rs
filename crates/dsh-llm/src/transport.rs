//! LLM transport 宿主侧:不变式闸门与假 provider。
//!
//! 闸门实现 agent-loop 的 [`LlmTransport`] 端口,包裹真实传输:
//! 出网请求的 messages/header 在边界做内容级 derive-and-compare
//! (期望侧 = 共享日志的 [`derive_messages`] + engine header),
//! 不一致即拒绝——组件无法绕过(「策略变机制」)。
//!
//! 假 provider 是可编程测试装备(llm-replay 思路):脚本化事件序列 +
//! 记录收到的请求供断言;e2e 不依赖真实网络。
//!
//! 总线传输(BusTransport)留在 dsh-host(依赖事件总线,属核心机制)。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use dsh_agent_loop::{LlmEvent, LlmTransport, RequestHeader, TransportError};
use dsh_session::EventLog;
use dsh_session::events::derive_visible_messages;
use serde_json::Value;

use crate::invariant::{InvariantViolation, verify_request};

/// 不变式闸门:包裹内层传输,强制「模型可见 ⟺ 已记录」
pub struct InvariantGate<T> {
    inner: T,
    log: Arc<Mutex<EventLog>>,
}

/// 闸门拒绝(不变式违反)
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum GateError {
    /// 内容级不一致被拒
    #[error("invariant rejected: {0}")]
    Violation(#[from] InvariantViolation),
}

impl<T> InvariantGate<T> {
    /// 以共享日志视图包裹内层传输(与 LoopEngine 共享同一日志)
    pub fn new(inner: T, log: Arc<Mutex<EventLog>>) -> Self {
        Self { inner, log }
    }

    /// 共享日志视图(装配层与 engine 共用同一日志;不变式比对的期望侧)
    pub fn log(&self) -> Arc<Mutex<EventLog>> {
        Arc::clone(&self.log)
    }

    /// 内层传输引用(测试装备断言用)
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// 解包内层传输
    pub fn into_inner(self) -> T {
        self.inner
    }

    /// 内容级校验:实际请求 vs 日志派生
    pub fn verify(&self, header: &RequestHeader, messages: &Value) -> Result<(), GateError> {
        let log = self
            .log
            .lock()
            .map_err(|_| GateError::Violation(internal_error("log 锁中毒")))?;
        // 期望侧与 engine 共用同一投影(裁剪/折叠策略栈;唯一实现)
        let derived_messages = derive_visible_messages(log.iter());
        let derived_header = header.to_json();
        verify_request(
            &derived_messages,
            messages,
            &derived_header,
            &derived_header,
        )?;
        Ok(())
    }
}

fn internal_error(msg: &str) -> InvariantViolation {
    InvariantViolation::MessagesDiverge {
        derived: msg.to_string(),
        actual: String::new(),
    }
}

/// 闸门转发一次性摘要调用(非会话面:不经 derive-and-compare;
/// 持久化由 engine 的 audit/call + compaction/summary 承担)
impl<T: dsh_agent_loop::Summarizer> dsh_agent_loop::Summarizer for InvariantGate<T> {
    fn summarize<'a>(
        &'a mut self,
        header: &'a RequestHeader,
        messages: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        self.inner.summarize(header, messages)
    }
}

impl<T: LlmTransport + Send> LlmTransport for InvariantGate<T> {
    async fn stream(
        &mut self,
        header: &RequestHeader,
        messages: &Value,
    ) -> Result<Vec<LlmEvent>, TransportError> {
        // 闸门拒绝 = 我方不变式违反,归 Other(不重试,直通 turn/error)
        self.verify(header, messages)
            .map_err(|e| TransportError::Other(e.to_string()))?;
        self.inner.stream(header, messages).await
    }

    /// 流式路径必须覆写并**转发**给内层——此前漏覆写,落到 trait
    /// 默认实现(先 `stream()` 攒完全量再整批发送),所有真实流量在
    /// 闸门处被攒批,下游表现为「假流式」(生产探针定位:
    /// reqwest/HttpTransport 均渐进,唯经闸门的路径整批)。
    /// 校验语义不变:请求侧 derive-and-compare 在发起前完成。
    async fn stream_events(
        &mut self,
        header: &RequestHeader,
        messages: &Value,
        tx: tokio::sync::mpsc::UnboundedSender<LlmEvent>,
    ) -> Result<(), TransportError> {
        if std::env::var_os("DSH_PROBE").is_some() {
            eprintln!("[g1] gate.stream_events 进入");
        }
        self.verify(header, messages)
            .map_err(|e| TransportError::Other(e.to_string()))?;
        if std::env::var_os("DSH_PROBE").is_some() {
            eprintln!("[g2] gate.verify 通过");
        }
        let r = self.inner.stream_events(header, messages, tx).await;
        if std::env::var_os("DSH_PROBE").is_some() {
            eprintln!("[g3] inner 完成: {}", r.is_ok());
        }
        r
    }
}

/// 假 provider:脚本化事件 + 请求录制(测试装备)
#[derive(Default)]
pub struct FakeProvider {
    /// 脚本:每次 stream 调用依序弹出一组事件;空则返回空序列
    pub script: Vec<Vec<LlmEvent>>,
    /// 录制:收到的 (header, messages)
    pub received: Vec<(RequestHeader, Value)>,
    /// 摘要脚本:summarize 调用依序弹出;空则回固定串
    pub summaries: Vec<String>,
}

impl FakeProvider {
    /// 空脚本
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一组事件(下一次 stream 调用返回)
    pub fn then(&mut self, events: Vec<LlmEvent>) -> &mut Self {
        self.script.push(events);
        self
    }
}

impl dsh_agent_loop::Summarizer for FakeProvider {
    fn summarize<'a>(
        &'a mut self,
        _header: &'a RequestHeader,
        _messages: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            Ok(if self.summaries.is_empty() {
                "[fake summary]".into()
            } else {
                self.summaries.remove(0)
            })
        })
    }
}

impl LlmTransport for FakeProvider {
    async fn stream(
        &mut self,
        header: &RequestHeader,
        messages: &Value,
    ) -> Result<Vec<LlmEvent>, TransportError> {
        self.received.push((header.clone(), messages.clone()));
        Ok(if self.script.is_empty() {
            Vec::new()
        } else {
            self.script.remove(0)
        })
    }
}
