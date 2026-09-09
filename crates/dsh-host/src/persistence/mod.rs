//! 会话持久化:JSONL 主格式 + turso 派生索引后端。
//!
//! 职责划分:
//! - **JSONL 是事实流主格式**:追加式、流式写、人类可读、崩溃后按行恢复;
//! - **turso(SQLite 纯 Rust 重写)是可由日志全量重建的派生索引**——
//!   引擎级问题不损失数据,必要时可回退/重建。
//!
//! 读取方守卫在两个后端的 load 路径上一致生效:
//! 未知且未标 ignorable 的事件 → 拒绝整份日志(继承语义,防静默损坏)。

pub mod jsonl;
pub mod turso_backend;

use dsh_session::envelope::EnvelopeError;
use thiserror::Error;

pub use jsonl::JsonlBackend;
pub use turso_backend::TursoBackend;

/// 持久化错误
#[derive(Debug, Error)]
pub enum PersistenceError {
    /// IO 错误
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// turso 后端错误
    #[error("turso: {0}")]
    Turso(String),
    /// 序列化错误
    #[error("serde: {0}")]
    Encode(#[from] serde_json::Error),
    /// 信封解码失败(含未知事件守卫拒绝)
    #[error("envelope: {0}")]
    Envelope(#[from] EnvelopeError),
}

/// 落盘后端统一入口(enum 分发:native async fn in trait 非 dyn-safe)。
///
/// 后端语义:append 追加单事件;load 全量读取(守卫生效);
/// [`TursoBackend::replace_all`] 是索引重建路径(从 JSONL 主格式重建 turso)。
pub enum LogBackend {
    /// JSONL 主格式(事实流)
    Jsonl(JsonlBackend),
    /// turso 派生索引
    Turso(TursoBackend),
}

impl LogBackend {
    /// 追加单事件
    pub async fn append(&self, ev: &dsh_session::EventEnvelope) -> Result<(), PersistenceError> {
        match self {
            Self::Jsonl(b) => b.append(ev),
            Self::Turso(b) => b.append(ev).await,
        }
    }

    /// 全量读取(读取方守卫生效)
    pub async fn load(&self) -> Result<Vec<dsh_session::EventEnvelope>, PersistenceError> {
        match self {
            Self::Jsonl(b) => b.load(),
            Self::Turso(b) => b.load().await,
        }
    }
}
