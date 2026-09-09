//! 会话与工作区树:侧栏(render/rail 壳 + 行/组/工作区网格)、会话行
//! CRUD(重命名/删除/fork/archive/导出)、行/组/工作区菜单与工作区
//! CRUD。store 域(行菜单/重命名/删除/工作区路径/分支/折叠组)见
//! features::sessions::store。reducer 的会话镜像仍聚合在 StoreState
//! (多功能共读 hub),待 chat/shell 收尾时随域分拆。

pub(crate) mod store;
mod views;

pub(crate) use store::SessionsStore;
pub(crate) use views::{drag_strip, render, row_menu_card, ws_menu_card};
