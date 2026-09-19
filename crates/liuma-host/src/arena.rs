//! 每会话组件竞技场。
//!
//! 一个会话 = 一个 [`Arena`] = 一个 wasmtime `Store` + 竞技场内全部组件实例。
//! 组件代码经 [`crate::engine::HostEngine`] 的 `InstancePre` 池共享;
//! 会话销毁即状态销毁(wasm 隔离的三收益之一,重写方案 §7)。
//!
//! session/agent-loop/prompt/tools 四个在树组件挂接于此;
//! 「竞技场级事务替换」(dispose → 日志重放 → 重建)也以本类型为单位。

use thiserror::Error;

use crate::engine::{EngineError, HostEngine};

/// 竞技场错误
#[derive(Debug, Error)]
pub enum ArenaError {
    /// 引擎层错误
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// 实例化失败
    #[error("instantiate failed: {0}")]
    Instantiate(String),
    /// 组件未挂载到本竞技场
    #[error("component not mounted in this arena: {0}")]
    NotMounted(String),
}

/// 宿主侧每 Store 状态:WASI ctx + 资源表(wasmtime-wasi 的 WasiView 契约)。
///
/// 后续在此扩展:会话句柄、cancel-token 表、审计计数等;
/// Store 数据即「宿主视角的会话上下文」,与 guest 内存相互独立。
pub struct HostState {
    /// WASI 上下文(时钟/随机/环境等能力;能力束 E3 的注入点)
    pub wasi: wasmtime_wasi::WasiCtx,
    /// WASI 资源表(句柄分配)
    pub table: wasmtime::component::ResourceTable,
}

impl wasmtime_wasi::WasiView for HostState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl Default for HostState {
    fn default() -> Self {
        Self::new()
    }
}

impl HostState {
    /// 空状态(无预开目录/网络;能力最小化起点,按需在 WasiCtxBuilder 扩展)
    pub fn new() -> Self {
        Self {
            wasi: wasmtime_wasi::WasiCtxBuilder::new().build(),
            table: wasmtime::component::ResourceTable::new(),
        }
    }
}

/// 每会话竞技场:持有 Store 与已实例化组件。
///
/// 状态隔离单位:两个 Arena 之间无共享可变状态(见 tests/engine.rs 的
/// 并发实例化互不渗透测试)。调用方需处于 tokio 上下文。
pub struct Arena {
    /// 会话级 Store,数据为 [`HostState`]
    store: wasmtime::Store<HostState>,
    /// 已实例化组件(名字 → 实例)
    instances: Vec<(String, wasmtime::component::Instance)>,
}

impl Arena {
    /// 为组件 `name` 建立竞技场并实例化(异步:实例化走组件模型 async 入口)。
    pub async fn new(engine: &HostEngine, name: &str) -> Result<Self, ArenaError> {
        let pre = engine.instance_pre(name)?;
        let mut store = wasmtime::Store::new(engine.engine(), HostState::new());
        // epoch_interruption 开启后 store 默认 deadline=0(立即到期):
        // 竞技场默认给大余量,需要硬停时显式 set_epoch_deadline
        store.set_epoch_deadline(u32::MAX as u64);
        let instance = pre
            .instantiate_async(&mut store)
            .await
            .map_err(|e| ArenaError::Instantiate(e.to_string()))?;
        Ok(Self {
            store,
            instances: vec![(name.to_string(), instance)],
        })
    }

    /// 取已挂载组件实例
    pub fn instance(&self, name: &str) -> Result<&wasmtime::component::Instance, ArenaError> {
        self.instances
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, i)| i)
            .ok_or_else(|| ArenaError::NotMounted(name.to_string()))
    }

    /// 会话 Store(可变借用;调用导出函数时需要)
    pub fn store_mut(&mut self) -> &mut wasmtime::Store<HostState> {
        &mut self.store
    }

    /// 设置 epoch deadline(n 个 epoch 后硬停;E4 兜底,死循环组件将被 trap)。
    ///
    /// 常规取消走 cancel-token 轮询(软);epoch 是最终手段(硬)。
    pub fn set_epoch_deadline(&mut self, epochs_ahead: u64) {
        self.store.set_epoch_deadline(epochs_ahead);
    }
}
