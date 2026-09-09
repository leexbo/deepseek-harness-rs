//! 附件类型窄重导出(dsh-desktop 不直接依赖 dsh-session,
//! 附件限额/媒体类型经 dsh-core 间接;新功能仍回归 dsh-session 实现)。

pub use dsh_session::attachments::{ImageAttachmentLimits, ImageMediaType};
