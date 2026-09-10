//! LLM 接入(dsh-llm):provider 方言适配器、HTTP/SSE transport 与不变式闸门。
//!
//! llm 缝 + provider 实现分离:端口
//! [`dsh_agent_loop::LlmTransport`] 定义在组件侧(dsh-agent-loop,对应
//! WIT `dsh:host/llm-transport` import),本 crate 是宿主的实现——替换
//! provider 方言/实现 = 换 crate 依赖,不触碰宿主核心
//! (「一切皆插件」在 crate 层的落地)。
//!
//! - [`streaming`] — SSE/裸流双模帧提取 + 方言映射器(纯逻辑,重放确定性);
//! - [`adapters`] — 方言差异收口(deepseek-responses / openai-responses /
//!   deepseek-chat / openai-chat / anthropic);
//! - [`chat`] / [`responses`] / [`anthropic`] — 通用引擎(各 wire 形态的
//!   兼容 provider 共有部分写一次);
//! - [`ext`] — 方言差异点声明(`ChatExt`/`ResponsesExt`/`AnthropicExt`:
//!   常量 + wire 钩子);
//! - [`attachments`] — 图片附件请求期处理(字节来源 trait + offload);
//! - [`http`] — reqwest 连接 + 流式消费([`LlmTransport`] 实现);
//! - [`invariant`] — 「模型可见 ⟺ 已记录」derive-and-compare 校验器(E1);
//! - [`transport`] — 不变式闸门([`InvariantGate`])与假 provider(测试装备)。

#![deny(missing_docs)]

pub mod adapters;
pub mod anthropic;
pub mod attachments;
pub mod chat;
pub mod ext;
pub mod http;
pub mod invariant;
pub mod responses;
pub mod streaming;
pub mod transport;

pub use adapters::{ProviderAdapter, adapter_by_name};
pub use anthropic::{AnthropicMapper, GenericAnthropicAdapter};
pub use attachments::{
    AttachmentSource, MAX_REQUEST_IMAGE_BYTES, NoAttachments, OFFLOADED_IMAGE_TEXT,
    offload_request_images, strip_images_for_summary,
};
pub use chat::GenericChatAdapter;
pub use ext::{
    AnthropicExt, ChatExt, DeepSeekChatExt, DeepSeekResponsesExt, OpenAiChatExt,
    OpenAiResponsesExt, ResponsesExt, StandardAnthropicExt,
};
pub use http::{HttpTransport, ProviderConfig};
pub use responses::{GenericResponsesAdapter, ResponsesMapper};
pub use transport::{FakeProvider, GateError, InvariantGate};
