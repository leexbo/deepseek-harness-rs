//! 文档预览功能切片(右栏预览标签页:text/code/markdown/image 渲染
//! 器 + 打开方式菜单 + 分页读 + 变更提示条;PDF 渲染器见 pdf.rs)。

pub(crate) mod face;
pub(crate) mod pdf;
pub(crate) mod store;
mod views;

pub(crate) use store::PreviewStore;
pub(crate) use views::{render, renderer_menu_card};
