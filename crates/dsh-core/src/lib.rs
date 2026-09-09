//! 多会话应用核心库(HTTP/WS 载波已移除,进程内调用面)。
//!
//! [`registry`] 多会话注册表(应用核心):[`proto`] 线上协议类型
//! (纯数据)与 [`translate`] 事件翻译(我方 `dsh-session` 词汇 → 客方
//! SessionEvent,纯函数)先行定稿并逐行单测锁定,注册表在其上迭代;
//! [`trajectory`]/[`stats`]/[`context`] 为轨迹/统计/上下文投影;
//! [`settings`] 用户级设置存储、[`credentials`] 凭据 seam。
//! 由桌面客户端进程内消费(同步方法直调,异步经 tokio runtime)。

#![deny(missing_docs)]

pub use dsh_agent_loop::LlmEvent;
/// 装配层凭据解析类型窄重导出(桌面客户端经 core 间接;
/// 见 [`dsh_app`](https://docs.rs/dsh-app) 的 `Resolved`)。
pub use dsh_app::Resolved;

pub mod attachments;
pub mod context;
pub mod credentials;
pub mod permission;
pub mod proto;
pub mod registry;
pub mod settings;
pub mod stats;
pub mod title;
pub mod trajectory;
pub mod translate;
