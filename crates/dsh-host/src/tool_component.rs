//! wasm 工具组件装载器(WIT `dsh:tools` world → `ToolPort` 桥)。
//!
//! preset 的路径型 mount 行装载外部工具组件:文件内容寻址注册进
//! [`HostEngine`](进程级共享 InstancePre 池)→ 自持 Store 实例化 →
//! `config-schema()` 出 JSON Schema 经 [`crate::config::validate_config`]
//! 校验 config → `init(config)` → `describe()` 组装 OpenAI function 声明。
//! 组件是纯 json→json 计算(world 零 import,能力边界即 import 集);
//! 执行走 `spawn_blocking` 同步调用,WASI 0.2 产物无 async export
//! (0.3 切换时升 async func,契约版本 bump)。
//!
//! 取消/硬停:取消 = 放弃等待返回 `cancelled`(不推进全局 epoch——
//! 会误伤并发中的其他调用);死循环组件由 epoch 预算兜底(E4:ticker
//! 20ms/epoch,默认预算 30_000 ≈ 10 分钟,超时 trap)。

use std::sync::{Arc, Mutex};

use dsh_agent_loop::{CancelToken, ToolCallRequest, ToolOutput, ToolPort};
use serde_json::Value;
use sha2::Digest as _;

use crate::arena::HostState;
use crate::config::validate_config;
use crate::engine::{EngineError, global_engine};
use dsh_wit::tool::ToolComponent;

/// 默认 epoch 硬停预算(ticker 20ms/epoch ≈ 10 分钟;死循环组件的 E4 兜底)
pub const DEFAULT_EPOCH_BUDGET: u64 = 30_000;

/// wasm 工具组件装载错误
#[derive(Debug, thiserror::Error)]
pub enum WasmToolError {
    /// 读取组件文件失败
    #[error("读取组件失败 {path}: {source}")]
    Read {
        /// 组件路径
        path: String,
        /// IO 错误
        source: std::io::Error,
    },
    /// 引擎层错误(编译/注册)
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// 实例化失败
    #[error("实例化失败:{0}")]
    Instantiate(String),
    /// 组件声明的 config-schema 不是合法 JSON
    #[error("config-schema 非法:{0}")]
    Schema(String),
    /// config 未过组件声明的 schema(类型即校验,装配期拒绝)
    #[error("config 校验失败:{0}")]
    Config(String),
    /// lifecycle.init 拒绝
    #[error("组件初始化失败:{0}")]
    Init(String),
    /// describe 失败或产出非法声明
    #[error("describe 失败:{0}")]
    Describe(String),
}

/// 外部 wasm 工具组件的宿主桥(实现 [`ToolPort`],经 blanket 进 `ToolSet`)
pub struct WasmTool {
    /// 组件状态实例(独占;工具执行引擎侧本就串行,Mutex 承载跨线程)。
    /// Store 自持 Engine 存活(wasmtime Engine 内部引用计数),无需另行持有
    store: Arc<Mutex<wasmtime::Store<HostState>>>,
    /// 实例句柄(Copy;world wrap 每次调用时构造)
    instance: wasmtime::component::Instance,
    /// describe 缓存的工具声明(装载期一次)
    specs: Vec<Value>,
    /// 软取消令牌(与引擎安全点同源)
    cancel: CancelToken,
    /// epoch 硬停预算(测试可注入小值)
    epoch_budget: u64,
    /// 来源路径(错误信息)
    source: String,
}

impl WasmTool {
    /// 装载并初始化组件:内容寻址注册(同内容命中池)→ 实例化 →
    /// config-schema 校验 → init → describe。
    /// `config` 为 preset mount 行的 config(归一后对象)
    pub fn new(
        path: &std::path::Path,
        config: &Value,
        cancel: CancelToken,
    ) -> Result<Self, WasmToolError> {
        let engine = global_engine()?;
        let bytes = std::fs::read(path).map_err(|source| WasmToolError::Read {
            path: path.display().to_string(),
            source,
        })?;
        // 内容寻址注册键:同内容命中 InstancePre 池,内容变即新键
        let digest = hex::encode(sha2::Sha256::digest(&bytes));
        let key = format!("wasm-{}", &digest[..16]);
        engine.register(&key, &bytes)?;
        let pre = engine.instance_pre(&key)?;
        let mut store = wasmtime::Store::new(engine.engine(), HostState::new());
        // 竞技场同款:大余量起步,预算在每次 execute 前设置
        store.set_epoch_deadline(u32::MAX as u64);
        let instance = pre
            .instantiate(&mut store)
            .map_err(|e| WasmToolError::Instantiate(e.to_string()))?;
        let component = ToolComponent::new(&mut store, &instance)
            .map_err(|e| WasmToolError::Instantiate(e.to_string()))?;

        // lifecycle:init 前宿主按组件声明的 schema 校验 config
        let lifecycle = component.dsh_plugin_lifecycle();
        let schema_bytes = lifecycle
            .call_config_schema(&mut store)
            .map_err(|e| WasmToolError::Schema(e.to_string()))?
            .map_err(WasmToolError::Schema)?;
        let schema: Value =
            serde_json::from_slice(&schema_bytes).map_err(|e| WasmToolError::Schema(e.to_string()))?;
        validate_config(&schema, config).map_err(|e| WasmToolError::Config(e.to_string()))?;
        let config_bytes = serde_json::to_vec(config)
            .map_err(|e| WasmToolError::Config(e.to_string()))?;
        lifecycle
            .call_init(&mut store, &config_bytes)
            .map_err(|e| WasmToolError::Init(e.to_string()))?
            .map_err(WasmToolError::Init)?;

        // describe → OpenAI function 声明(装载期缓存)
        let specs = component
            .dsh_tools_tools()
            .call_describe(&mut store)
            .map_err(|e| WasmToolError::Describe(e.to_string()))?
            .into_iter()
            .map(|spec| {
                let input_schema: Value = serde_json::from_slice(&spec.input_schema)
                    .map_err(|e| WasmToolError::Describe(format!("input-schema 非法:{e}")))?;
                Ok(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": spec.name,
                        "description": spec.description,
                        "parameters": input_schema,
                    },
                }))
            })
            .collect::<Result<Vec<_>, WasmToolError>>()?;
        if specs.is_empty() {
            return Err(WasmToolError::Describe("组件未声明任何工具".into()));
        }

        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            instance,
            specs,
            cancel,
            epoch_budget: DEFAULT_EPOCH_BUDGET,
            source: path.display().to_string(),
        })
    }

    /// 覆写 epoch 硬停预算(测试注入小值验证硬停)
    pub fn with_epoch_budget(mut self, budget: u64) -> Self {
        self.epoch_budget = budget;
        self
    }

    /// 来源路径
    pub fn source(&self) -> &str {
        &self.source
    }
}

impl ToolPort for WasmTool {
    fn specs(&self) -> Vec<Value> {
        self.specs.clone()
    }

    /// 执行:spawn_blocking 内同步调用组件 export;取消 = 放弃等待
    /// (不推进全局 epoch,防误伤并发调用),死循环由 epoch 预算硬停兜底
    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        let name = call.name.clone();
        let input = serde_json::to_vec(&call.arguments).unwrap_or_default();
        let store = Arc::clone(&self.store);
        let instance = self.instance;
        let budget = self.epoch_budget;
        let handle = tokio::task::spawn_blocking(move || {
            let mut guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard.set_epoch_deadline(budget);
            let store: &mut wasmtime::Store<HostState> = &mut guard;
            let component = ToolComponent::new(&mut *store, &instance)?;
            let tools = component.dsh_tools_tools();
            tools.call_execute(store, &name, &input)
        });
        tokio::select! {
            res = handle => match res {
                Ok(Ok(Ok(bytes))) => {
                    let output = String::from_utf8_lossy(&bytes).to_string();
                    ToolOutput { output, success: true, ..Default::default() }
                }
                Ok(Ok(Err(err))) => {
                    // 工具级失败(result Err:组件自报,如参数不合法)
                    ToolOutput { output: err, success: false, ..Default::default() }
                }
                Ok(Err(trap)) => {
                    // trap(含 epoch 硬停):执行失败
                    ToolOutput { output: format!("组件执行失败:{trap}"), success: false, ..Default::default() }
                }
                Err(join) => ToolOutput { output: format!("组件执行任务失败:{join}"), success: false, ..Default::default() },
            },
            _ = self.cancel.cancelled() => {
                // 放弃等待;组件在 epoch 预算内自然结束或被硬停
                // (store 锁持有至组件返回,后续 execute 排队)
                ToolOutput { output: "cancelled".into(), success: false, ..Default::default() }
            }
        }
    }
}
