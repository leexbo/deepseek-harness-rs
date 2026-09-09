//! 宿主核心库:被 `dsh` 二进制与测试引用的宿主能力集合。
//!
//! 覆盖:wasmtime 组件运行(Engine/Store/`InstancePre` 池 + 每会话竞技场)、
//! JSONL/turso 持久化、不变式校验(derive-and-compare;LLM transport 与
//! 不变式闸门在 dsh-llm crate)、spawn/pty 与 OS 沙箱链(在 dsh-sandbox
//! crate)、事件总线、插件生命周期与配置子系统。
//!
//! 本 crate 对外 API 必须带文档(`#![deny(missing_docs)]`),
//! 与 `dsh:host` WIT 契约的对应关系在各项文档中注明。

#![deny(missing_docs)]

pub mod arena;
pub mod attachments;
pub mod bus;
pub mod config;
pub mod engine;
pub mod hello;
pub mod instructions;
pub mod persistence;
pub mod plugins;
pub mod presets;
pub mod rpc;
pub mod search;
pub mod telemetry;
pub mod tool_component;
pub mod transport;

pub use arena::{Arena, ArenaError, HostState};
pub use attachments::{AttachmentStore, AttachmentStoreError, SaveImage};
pub use bus::{EventBus, ListenerResult, Next, Stop};
pub use config::{ConfigError, DshConfig, validate_config};
pub use dsh_agent_loop::CancelToken;
pub use engine::{EngineError, HostEngine, global_engine};
pub use instructions::{
    InstructionChange, InstructionRuntimeState, LoadedFile, MAX_SOURCE_BYTES, MAX_TOTAL_BYTES,
    baseline_identity, content_digest, default_dshrs_root, discover_baseline_files,
    find_project_root, load_file, render_baseline,
};
pub use persistence::{JsonlBackend, LogBackend, PersistenceError, TursoBackend};
pub use plugins::{PluginError, PluginRegistry, RegistryState};
pub use presets::{
    API_VERSION, KIND, MountSpec, PresetError, PresetManifest, PresetMetadata, PresetSpec,
};
pub use rpc::{Gateway, JsonRpcError, serve_stdio};
pub use telemetry::{SpanRecord, TelemetryError, export_spans, write_otlp_jsonl};
pub use tool_component::{DEFAULT_EPOCH_BUDGET, WasmTool, WasmToolError};
pub use transport::BusTransport;
