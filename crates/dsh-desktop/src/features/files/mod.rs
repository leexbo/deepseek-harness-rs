//! 文件树功能切片(工作区目录树,右栏「文件」标签页;lazy 逐层装载,
//! 点文件开预览——预览见 `features::preview`)。

pub(crate) mod face;
pub(crate) mod store;
mod views;

pub(crate) use store::FilesStore;
pub(crate) use views::render;
