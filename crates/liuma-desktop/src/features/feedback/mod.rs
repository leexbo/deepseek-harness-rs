//! 消息反馈功能:assistant 消息动作行的赞/踩/备注 + 备注弹窗。
//! 目录 = 完整切片:view([`views`]) + 状态与行为(`store` 的
//! [`FeedbackStore`] 与 `impl AppStore` 扩展块)。

pub(crate) mod store;
mod views;

pub(crate) use store::FeedbackStore;
pub(crate) use views::{actions, render_note_editor};
