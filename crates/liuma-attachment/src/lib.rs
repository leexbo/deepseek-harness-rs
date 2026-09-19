//! 附件子系统:词汇(媒体类型/引用/限值)+ 内容寻址对象存储。
//!
//! 附件是**内容寻址不可变对象**:`sha256:<64hex>` 为 id,字节存宿主侧
//! 对象存储,会话日志只携带引用([`ImageAttachmentRef`] 等出现在
//! `user/message` 块数组里,零字节)。契约与本地落盘后端
//! 在这里合一——RS 仅本地单机一种后端,不立 contract/backend trait。
//!
//! 分层:类型词汇见 [`types`];落盘存储见 [`store`];请求期字节来源
//! 契约 [`AttachmentSource`] 由 liuma-llm 的方言翻译消费(随 [`store`]
//! 一并提供实现)。

#![deny(missing_docs)]

mod store;
mod types;

pub use store::{AttachmentStore, AttachmentStoreError, SaveFile, SaveImage};
pub use types::{
    FileAttachmentRef, FileKind, ImageAdmissionError, ImageAttachmentLimits, ImageAttachmentRef,
    ImageMediaType, classify_file_name, content_text, file_blocks, file_extension_label,
    image_blocks, message_content, splice_item,
};

/// 请求期附件来源契约(liuma-llm 方言翻译注入;实现 = [`AttachmentStore`])
pub trait AttachmentSource: Send + Sync {
    /// 按附件 id 读图片编码字节;缺席 = None(翻译侧降级占位)
    fn image_bytes(&self, id: &str) -> Option<Vec<u8>>;

    /// 文件引用的当前执行环境可读路径;
    /// 默认 None = 句柄文本走无路径分支
    fn file_path(&self, attachment_id: &str, name: &str) -> Option<String> {
        let _ = (attachment_id, name);
        None
    }
}
