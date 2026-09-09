//! 聊天消息流功能切片(消息流列/composer/@ 补全/toolcard/terminal/
//! todo_dock/context_meter;纯投影见 projection.rs,引用纯算法见 reference.rs)。

pub(crate) mod chat_pane;
pub(crate) mod composer;
mod context_meter;
pub(crate) mod mermaid_viewer;
pub(crate) mod projection;
mod reference;
pub(crate) mod store;
mod terminal;
pub(crate) mod todo_dock;
mod toolcard;

pub(crate) mod queue_dock;
pub(crate) use projection::{
    ChatNode, ChatState, PlanStatus, QueueEntry, QueuePlacement, parse_queue_items,
};
// RowSlot 仅测试(layout_tests 行槽形状断言)直接引用
#[cfg(test)]
pub(crate) use projection::RowSlot;
pub(crate) use store::{ChatStore, ComposerMenu, ComposerSubmenu};
