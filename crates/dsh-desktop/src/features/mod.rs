//! 功能切片目录:功能 = 目录 = 完整切片(状态 + 行为 + 视图 + 测试)。
//! 每个功能自持演化,只经 shell 底座/宿主桥与其它功能互作。

pub(crate) mod ask;
pub(crate) mod attachments;
pub(crate) mod chat;
pub(crate) mod feedback;
pub(crate) mod search;
pub(crate) mod sessions;
pub(crate) mod settings;
pub(crate) mod subagents;
pub(crate) mod trajectory;
