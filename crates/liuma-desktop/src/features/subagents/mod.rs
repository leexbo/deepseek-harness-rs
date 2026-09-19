//! 子代理血缘切片:页头「{count} 个子代理」目录——直接 subagent
//! 后代列表 + 子代理会话入口(点击 open_session)。subagent 会话在侧栏
//! 隐藏(侧栏渲染时过滤,见 ui::sidebar),仅经此目录进出。

pub(crate) mod store;
mod views;

pub(crate) use store::SubagentsStore;
pub(crate) use views::task_bar;
