//! dsh-hooks:Claude Code / Codex shell hooks 桥(源 packages/hooks/
//! hook-protocol + 两桥逐字对齐移植)。
//!
//! 分层:matcher/codec/merge/events 是纯函数内核;config 两方言解析;
//! payloads 两方言 stdin 载荷;runner 经 dsh_sandbox spawn(沙箱链
//! fail-closed,拍板 2)执行;service 逐点运行 + 最严格合并,并实现
//! dsh-agent-loop 的 HookPort(引擎拦截点)。桥的定位与源一致:
//! **兼容适配器**——只跑既有 hooks.json 的 command-hook 子集,每一步
//! 降级绝不抛(钩子崩不掉调用 turn)。

pub mod codec;
pub mod config;
pub mod events;
pub mod matcher;
pub mod merge;
pub mod payloads;
pub mod runner;
pub mod service;

pub use codec::{Decision, HookOutput};
pub use config::{BridgeDialect, CommandHook, HookConfig, MatcherGroup, ParsedConfig, SkippedHook};
pub use events::{DEFAULT_STDERR_SUMMARY_MAX_CHARS, HookDialect, HookInvocation};
pub use merge::{MergedDecision, MergedHookOutcome};
pub use runner::{DEFAULT_HOOK_TIMEOUT_MS, RunOutcome};
pub use service::{HookPortImpl, HookService, HookSink};
