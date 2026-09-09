//! 总线传输:loop 出网请求经总线 waterfall("llm/stream") 分发。
//!
//! 默认行为 = 内层传输;插件经 [`EventBus::subscribe_around`] 挂进管线——
//! 重试(调 next 多次)、回放替换(不调 next)、改写(p 传新载荷)皆可。
//! 不变式闸门应在更外层包裹(先校验后进总线)。
//!
//! 不变式闸门(InvariantGate)与假 provider(FakeProvider)已随 LLM 接入
//! 迁至 dsh-llm crate;本文件只留总线适配(BusTransport 依赖事件总线,
//! 属核心机制,不随出网实现拆分)。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use dsh_agent_loop::{LlmEvent, LlmTransport, RequestHeader, TransportError};
use serde_json::Value;

use crate::bus::EventBus;

/// TransportError → waterfall 错误通道(String):JSON 串化保结构
/// (重试分类跨总线不失真);串化失败回落 Display 文本
fn serialize_transport_error(e: TransportError) -> String {
    serde_json::to_string(&e).unwrap_or_else(|_| e.to_string())
}

/// waterfall 错误通道(String)→ TransportError:结构解码优先;
/// 非本枚举形态(插件自行报错)回落 Other
fn decode_transport_error(raw: String) -> TransportError {
    serde_json::from_str(&raw).unwrap_or(TransportError::Other(raw))
}

/// 总线传输:loop 出网请求经总线 waterfall("llm/stream") 分发。
///
/// 默认行为 = 内层传输;插件经 [`EventBus::subscribe_around`] 挂进管线——
/// 重试(调 next 多次)、回放替换(不调 next)、改写(p 传新载荷)皆可。
/// 不变式闸门应在更外层包裹(先校验后进总线)。
pub struct BusTransport<T> {
    bus: Arc<EventBus>,
    inner: Arc<tokio::sync::Mutex<T>>,
}

impl<T: 'static> BusTransport<T> {
    /// 以总线与内层传输构建
    pub fn new(bus: Arc<EventBus>, inner: T) -> Self {
        Self {
            bus,
            inner: Arc::new(tokio::sync::Mutex::new(inner)),
        }
    }
}

impl<T: LlmTransport + Send + 'static> LlmTransport for BusTransport<T> {
    async fn stream(
        &mut self,
        header: &RequestHeader,
        messages: &Value,
    ) -> Result<Vec<LlmEvent>, TransportError> {
        let payload = serde_json::json!({
            "header": header.to_json(),
            "messages": messages,
        });
        let inner = Arc::clone(&self.inner);
        let header = header.clone();
        let result = self
            .bus
            .waterfall("llm/stream", payload, move |p| {
                let inner = Arc::clone(&inner);
                let header = header.clone();
                Box::pin(async move {
                    let mut transport = inner.lock().await;
                    let messages = p["messages"].clone();
                    let events = transport
                        .stream(&header, &messages)
                        .await
                        .map_err(serialize_transport_error)?;
                    serde_json::to_value(&events).map_err(|e| e.to_string())
                })
            })
            .await
            .map_err(|stop| decode_transport_error(stop.to_string()))?;
        serde_json::from_value(result).map_err(|e| TransportError::Other(e.to_string()))
    }
}

/// 总线传输转发一次性摘要调用(与 stream 同经内层传输)
impl<T: dsh_agent_loop::Summarizer + Send> dsh_agent_loop::Summarizer for BusTransport<T> {
    fn summarize<'a>(
        &'a mut self,
        header: &'a RequestHeader,
        messages: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let mut guard = inner.lock().await;
            guard.summarize(header, messages).await
        })
    }
}
