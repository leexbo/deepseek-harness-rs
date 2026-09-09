//! 轨迹功能切片(时间线/台账/检查器 + 检索 Inspect 定位;json/diff/spans
//! 纯算法随视图同迁于 views.rs)。

pub(crate) mod store;
mod views;

pub(crate) use store::{InspectTarget, TrajectoryStore, TrajectoryView};
pub(crate) use views::render;
