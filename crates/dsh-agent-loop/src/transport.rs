//! LLM 传输端口:agent-loop 拥有的 trait,宿主实现(WIT `dsh:host/llm-transport` 对应物)。
//!
//! native 形态:事件以 `Vec` 整批返回(组件化时升级为 `stream<llm-event>`);
//! 语义不变——chunk 先记录后消费(记录优先),由调用方(engine)保证。

use serde_json::Value;

/// 传输错误分类(LlmFailure 代码表;重试策略据此决策,
/// turn/error 面携带稳定 code)。
///
/// 经总线的 transport(BusTransport)以 JSON 形态串化本枚举过
/// waterfall 错误通道(String),对端按结构解码;解码失败回落 [`TransportError::Other`]。
#[derive(Debug, Clone, PartialEq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum TransportError {
    /// 连接失败/DNS/TLS/流中断
    #[error("{0}")]
    #[serde(rename = "TRANSPORT")]
    Transport(String),
    /// 超时(连接超时/读超时)
    #[error("读超时: {0}")]
    #[serde(rename = "TIMEOUT")]
    Timeout(String),
    /// 服务端 5xx
    #[error("provider {status}: {body}")]
    #[serde(rename = "SERVER", rename_all = "camelCase")]
    Server {
        /// HTTP 状态码(流内错误帧可能缺省为 0)
        status: u16,
        /// 响应体片段
        body: String,
    },
    /// 429(可携带服务端 Retry-After)
    #[error("provider 429: {body}")]
    #[serde(rename = "RATE_LIMIT", rename_all = "camelCase")]
    RateLimit {
        /// Retry-After 换算的毫秒数(仅整秒/缺失:None)
        retry_after_ms: Option<u64>,
        /// 响应体片段
        body: String,
    },
    /// 鉴权失败(401/403;不重试。诊断体保留在 `body` 字段,轨迹
    /// 详情可见;用户面文案固定语义,不透传原始响应)
    #[error("认证失败:API 密钥无效或已过期(provider {status})")]
    #[serde(rename = "AUTH", rename_all = "camelCase")]
    Auth {
        /// HTTP 状态码
        status: u16,
        /// 响应体片段
        body: String,
    },
    /// 请求无效(400/413;不重试)
    #[error("provider {status}: {body}")]
    #[serde(rename = "INVALID_REQUEST", rename_all = "camelCase")]
    InvalidRequest {
        /// HTTP 状态码
        status: u16,
        /// 响应体片段
        body: String,
    },
    /// 空响应(流正常结束但零内容;engine 侧判定,transport 不产此态)
    #[error("空响应")]
    #[serde(rename = "EMPTY_RESPONSE")]
    EmptyResponse,
    /// 其余未分类(不重试)
    #[error("{0}")]
    #[serde(rename = "OTHER")]
    Other(String),
}

impl TransportError {
    /// 可重试分类(DEFAULT_RETRYABLE_CODES:EMPTY_RESPONSE /
    /// RATE_LIMIT / SERVER / TIMEOUT / TRANSPORT;AUTH、INVALID_REQUEST
    /// 及其余直通不重试)
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport(_)
                | Self::Timeout(_)
                | Self::Server { .. }
                | Self::RateLimit { .. }
                | Self::EmptyResponse
        )
    }

    /// 稳定错误码(turn/error / llm/retry 面与轨迹用)
    pub fn code(&self) -> &'static str {
        match self {
            Self::Transport(_) => "TRANSPORT",
            Self::Timeout(_) => "TIMEOUT",
            Self::Server { .. } => "SERVER",
            Self::RateLimit { .. } => "RATE_LIMIT",
            Self::Auth { .. } => "AUTH",
            Self::InvalidRequest { .. } => "INVALID_REQUEST",
            Self::EmptyResponse => "EMPTY_RESPONSE",
            Self::Other(_) => "OTHER",
        }
    }

    /// 服务端 Retry-After(仅 RATE_LIMIT 携带)
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimit { retry_after_ms, .. } => *retry_after_ms,
            _ => None,
        }
    }
}

impl From<String> for TransportError {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}

/// LLM 流事件(对等 WIT `llm-event` variant;经总线分发需可序列化)
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LlmEvent {
    /// 增量文本
    Chunk(String),
    /// 推理增量(reasoning_content;思考内容)
    Reasoning(String),
    /// 完整助手消息(流结束时的物化)
    AssistantMessage(Value),
    /// 流终止哨兵
    Done,
    /// 用量统计
    Usage(Value),
    /// 失败(可恢复性由策略层判断)
    Failure(Value),
}

/// LLM 传输端口。
///
/// 实现方:宿主(真实 transport + 不变式闸门)与测试装备(假 provider)。
/// `header`/`messages` 即不变式比对对象(宿主在边界做内容级校验,E1)。
pub trait LlmTransport {
    /// 发起一次流式请求,返回完整事件序列
    fn stream(
        &mut self,
        header: &crate::RequestHeader,
        messages: &Value,
    ) -> impl std::future::Future<Output = Result<Vec<LlmEvent>, TransportError>> + Send;

    /// 流式路径:事件逐条送入 channel(记录优先,chunk 到达即落档+广播 → UI 逐 token)。
    /// 默认实现 = 收集后整批发送(非流式 transport 保持可用);http transport
    /// 覆盖为逐帧发送。channel 为 owned 值,回调零借用(规避 async fn 嵌套闭包
    /// 的生命周期限制 rust#100013)。
    fn stream_events<'a>(
        &'a mut self,
        header: &'a crate::RequestHeader,
        messages: &'a Value,
        tx: tokio::sync::mpsc::UnboundedSender<LlmEvent>,
    ) -> impl std::future::Future<Output = Result<(), TransportError>> + Send + 'a
    where
        Self: Sized + Send,
    {
        async move {
            let events = self.stream(header, messages).await?;
            for ev in events {
                let _ = tx.send(ev);
            }
            Ok(())
        }
    }
}
