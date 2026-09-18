//! 窗口文本选择的 order 分区常量(gpui-base SelectableText 的
//! document_order 契约:跨参与者圈选按 [min..=max] 区间,未分区会把
//! 无关域整段卷入)。聊天正文已迁 TextView(自带全局自增 order,
//! 从小值起不撞手动段);本常量服务剩余手动装配参与者:用户气泡
//! 分段、预览行、两个域尾哨兵(拖选落空终点钳制)。

/// 聊天域基址(用户气泡分段)
pub(crate) const CHAT_ORDER_BASE: u64 = 1 << 20;
/// 右栏域基址(域尾哨兵)
pub(crate) const PANEL_ORDER_BASE: u64 = 1 << 40;
/// 单文档 order 步长(块序上限;溢出仅回退为同档)
pub(crate) const ORDER_STRIDE: u64 = 4096;
/// 聊天域尾哨兵(栈底,盖整列)
pub(crate) const CHAT_TAIL_ORDER: u64 = CHAT_ORDER_BASE + u32::MAX as u64;
/// 右栏域尾哨兵(面板列根)
pub(crate) const PANEL_TAIL_ORDER: u64 = PANEL_ORDER_BASE + u32::MAX as u64;
