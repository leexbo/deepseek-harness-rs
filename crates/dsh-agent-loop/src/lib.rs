//! agent-loop 组件:turn/step 状态机与驱动循环(rlib 部分)。
//!
//! 一个 step = 一次模型请求加其触发的工具执行;一个 turn = 零或多个 step。
//! 无工具路径:一个 turn = 一个 step,assistant 完成
//! 且无工具调用即 turn/end。
//!
//! 端口设计:LLM 传输是 [`LlmTransport`] trait(对等 WIT `dsh:host/llm-transport`
//! 的 import);宿主实现该端口,不变式闸门(E1)在宿主侧包裹——
//! 组件可见的传输即已强制的传输。
//!
//! wasm 组件化(guest 导出 driver 四函数)随 stream<T> 工具链验证接入;
//! 本模块的全部语义先以 rlib 落定并被测试锁定。

pub mod cancel;
pub mod engine;
pub mod hooks;
pub mod presentation;
pub mod retry;
pub mod runtime_context;
pub mod summarizer;
pub mod tools;

pub use cancel::CancelToken;
pub use engine::{
    FOLD_BUDGET_CHARS, FOLD_KEEP_MESSAGES, InstructionsProvider, InstructionsProviderFn,
    LoopEngine, LoopError, Phase, SkillCatalogProvider, SkillGestureProvider, SteerInput,
    TurnOutcome,
};
pub use hooks::{
    HookPort, HookPortObj, PostToolVerdict, PreStepVerdict, PreToolVerdict, StopVerdict,
};
pub use presentation::{FileDiff, FileMatches, ToolView, ViewLine};
pub use retry::RetryPolicy;
pub use runtime_context::{
    CLEARED_CONTEXT, CONTEXT_SOURCE_PLUGIN, ContextSection, RuntimeContextProjection,
};
pub use summarizer::Summarizer;
pub use tools::{
    DuplicateToolNameError, NoTools, ToolCallRequest, ToolOutput, ToolPort, ToolPortObj, ToolSet,
};
pub use transport::{LlmEvent, LlmTransport, TransportError};

mod transport;

use serde_json::Value;

/// 请求折叠 header(model/system/temperature/maxTokens/stop/tools)
///
/// foldRequestHeader 的比对面:不变式双比对的一半。
#[derive(Debug, Clone, PartialEq)]
pub struct RequestHeader {
    /// 模型标识
    pub model: String,
    /// system prompt(经 prompt 组件组装)
    pub system: String,
    /// 采样温度
    pub temperature: f64,
    /// 推理等级(low / high / max;None = provider 默认)。
    /// 影响「模型可见」的请求参数 → 属于比对面
    pub reasoning_effort: Option<String>,
    /// 工具声明(OpenAI function 形状;engine 每次 turn 从
    /// [`ToolPort::specs`] 注入。工具集来自宿主装配而非会话日志,
    /// 不属于「模型可见 ⟺ 已记录」的比对面)
    pub tools: Vec<Value>,
}

impl RequestHeader {
    /// 折叠为可比对 JSON(键序无关;不变式比对用)
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::json!({
            "model": self.model,
            "system": self.system,
            "temperature": self.temperature,
            "tools": self.tools,
        });
        if let Some(effort) = &self.reasoning_effort {
            v["reasoningEffort"] = serde_json::json!(effort);
        }
        v
    }
}
