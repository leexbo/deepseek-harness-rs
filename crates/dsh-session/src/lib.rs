//! session 组件:追加式事件日志。
//!
//! 核心语义:
//! - seq 连续(运行时强制,见 [`log::EventLog::append`])
//! - `ignorable` 未知事件守卫(读取方拒绝重建,见 [`envelope::decode_envelope`])
//! - surfaceOp/sourceEventSeqs 引用链(surface 事件专属)
//! - 投影三件套([`projection`] 经 guest 导出)
//! - single boundary rule:持久可重放事实在此,活体信号走总线
//!
//! 双产物:rlib(宿主侧测试/复用)+ wasm32-wasip2 组件([`guest`] 实现 WIT 导出)。
//! 组件内禁止直接读时钟/随机——经显式 WASI import(重放确定性前提)。

pub mod attachments;
pub mod audit;
pub mod envelope;
pub mod events;
pub mod log;

#[cfg_attr(not(target_family = "wasm"), allow(missing_docs))]
pub mod bindings;
#[cfg_attr(not(target_family = "wasm"), allow(missing_docs))]
pub mod guest;

pub use attachments::{
    ImageAdmissionError, ImageAttachmentLimits, ImageAttachmentRef, ImageMediaType, content_text,
    image_blocks, message_content, splice_item,
};
pub use audit::{Attribution, AuditRecord, attribution_chain, audit_call_event, audit_records};
pub use envelope::{EnvelopeError, EventEnvelope, SESSION_FORMAT_VERSION, decode_envelope};
pub use events::{
    ATTRIBUTED_EVENT_TYPES, AssistantChunk, AssistantMessage, CompactionSummary, GoalItem,
    GoalState, KNOWN_EVENT_TYPES, LlmRetry, LlmRetryStarted, PRUNE_HEAD_CHARS,
    PRUNE_TAIL_CHARS, PRUNE_THRESHOLD_CHARS, PlanApproved, PlanSubmitted, SURFACE_EVENT_TYPES,
    SessionEventData, SessionMode, TodoItem, TodoWrite, ToolResult, UserMessage,
    derive_messages, derive_visible_messages, message_from_event, prune_output,
};
pub use log::{EventLog, LogError};

// 组件导出注册(cdylib 产物的导出入口)
use guest::SessionComponent;

// 根级 generate(官方模板形态):export! 宏与 exports 模块生成于 crate 根
wit_bindgen::generate!({
    path: "../../wit/session",
    world: "session",
    with: { "dsh:json/value@0.1.0": bindings::dsh_json },
});

export!(SessionComponent);

// wit-bindgen 的接口入口与 cabi_post 清理钩子只在 wasm 目标生成实体;native 侧
// cdylib 链接时导出表仍引用这些符号(rlib 测试路径需要 crate-type 双轨),补空桩。
#[cfg(not(target_family = "wasm"))]
mod native_cabi_stubs {
    #[unsafe(export_name = "dsh:session/event-log@0.1.0")]
    extern "C" fn event_log_entry() {}
    #[unsafe(export_name = "dsh:session/projection@0.1.0")]
    extern "C" fn projection_entry() {}
    #[unsafe(export_name = "cabi_post_dsh:session/event-log@0.1.0")]
    extern "C" fn event_log_post() {}
    #[unsafe(export_name = "cabi_post_dsh:session/projection@0.1.0")]
    extern "C" fn projection_post() {}
}
