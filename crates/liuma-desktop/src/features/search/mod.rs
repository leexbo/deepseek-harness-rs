//! 全库检索切片:侧栏搜索框(Enter 触发)与命中面板(行 = 会话
//! 标题 + 命中片段,点击开会话切轨迹定位台账行)。跳转定位循「先登记
//! 待定位 seq、轨迹数据就绪后再定位」的延迟模式(与工具卡 Inspect 同)。

pub(crate) mod store;
mod views;

pub(crate) use store::SearchStore;
pub(crate) use views::{search_hits_panel, search_row};
