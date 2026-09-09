//! 图片附件功能:草稿缩略条/粘贴与文件选择输入/历史消息图渲染/
//! Lightbox/拒收 toast。目录 = 完整切片:view([`views`]) + 状态与行为
//! (`store` 的 [`AttachmentsStore`] 与 `impl AppStore` 扩展块)。
//!
//! 跨功能仅暴露窄接口:shell 的发送路径读草稿并构造拒收 toast
//! ([`AttachmentToast`]/`attachment_error_text`),根层渲染
//! lightbox/toast 经由 [`AttachmentsStore`] 字段与下述视图函数。

pub(crate) mod store;
mod views;

pub(crate) use store::{AttachmentToast, AttachmentsStore};
pub(crate) use views::{draft_rail, lightbox, message_images};
