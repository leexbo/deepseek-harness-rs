//! wasmtime 引擎与组件预实例化池。
//!
//! 职责(重写方案 §1 双层结构):
//! - 全局唯一 [`Engine`],`Config` 开启 async(0.3 形状接口的前提);
//! - 组件二进制 → [`InstancePre`] 的缓存:代码经 InstancePre 跨会话共享,
//!   每会话一个 Store(竞技场)持有状态,并发会话无共享可变状态。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;

use crate::arena::HostState;

/// 宿主引擎错误
#[derive(Debug, Error)]
pub enum EngineError {
    /// 组件编译失败
    #[error("component compile failed: {0}")]
    Compile(String),
    /// 组件名未注册
    #[error("component not registered: {0}")]
    NotRegistered(String),
}

/// 全局引擎 + `InstancePre` 池。
///
/// `Engine` 与编译产物(`Component`/`InstancePre`)跨会话共享且线程安全;
/// 会话状态只存在于 [`crate::arena::Arena`] 的 Store 中。
pub struct HostEngine {
    /// wasmtime 引擎(线程安全,跨会话共享)
    engine: wasmtime::Engine,
    /// 名字 → 预实例化组件池(代码段共享的载体)
    pre: Mutex<HashMap<String, Arc<wasmtime::component::InstancePre<HostState>>>>,
    /// epoch 计数线程停止标志
    ticker_stop: Arc<AtomicBool>,
    /// epoch 计数线程句柄(Drop 时停止)
    ticker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for HostEngine {
    fn drop(&mut self) {
        self.ticker_stop.store(true, Ordering::Relaxed);
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
    }
}

impl HostEngine {
    /// 创建引擎。
    ///
    /// - wasmtime 47 起组件模型 async 恒开(`async_support` 已废弃);
    /// - epoch interruption 开启 + 后台计数线程:竞技场设 deadline 后,
    ///   无视取消标志的死循环组件会被硬停(epoch 硬停,比 exec.signal 更强的最终手段)。
    pub fn new() -> Result<Self, EngineError> {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        let engine =
            wasmtime::Engine::new(&config).map_err(|e| EngineError::Compile(e.to_string()))?;
        let ticker_engine = engine.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        // 守护线程:每 20ms 推进全局 epoch;Drop 置停
        let ticker = std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                ticker_engine.increment_epoch();
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Ok(Self {
            engine,
            pre: Mutex::new(HashMap::new()),
            ticker_stop: stop,
            ticker: Some(ticker),
        })
    }

    /// 底层 wasmtime 引擎
    pub fn engine(&self) -> &wasmtime::Engine {
        &self.engine
    }

    /// 注册组件(二进制或 wat 文本),编译并缓存其 `InstancePre`。
    ///
    /// 同名重复注册即覆盖(热替换;竞技场级事务替换的物料来源)。
    pub fn register(&self, name: &str, binary_or_wat: &[u8]) -> Result<(), EngineError> {
        let component = wasmtime::component::Component::new(&self.engine, binary_or_wat)
            .map_err(|e| EngineError::Compile(format!("{name}: {e}")))?;
        // WASI(wasi:io 等)是 wasip2 组件的标准依赖,统一挂接;
        // dsh:host/* 宿主函数挂接后经此同一路径,后续在此扩展
        let mut linker = wasmtime::component::Linker::<HostState>::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| EngineError::Compile(format!("{name}: {e}")))?;
        let pre = linker
            .instantiate_pre(&component)
            .map_err(|e| EngineError::Compile(format!("{name}: {e}")))?;
        self.pre
            .lock()
            .expect("InstancePre 池锁中毒(宿主 bug)")
            .insert(name.to_string(), Arc::new(pre));
        Ok(())
    }

    /// 取已注册组件的 `InstancePre`(克隆 Arc,跨会话共享代码)
    pub fn instance_pre(
        &self,
        name: &str,
    ) -> Result<Arc<wasmtime::component::InstancePre<HostState>>, EngineError> {
        self.pre
            .lock()
            .expect("InstancePre 池锁中毒(宿主 bug)")
            .get(name)
            .cloned()
            .ok_or_else(|| EngineError::NotRegistered(name.to_string()))
    }
}

/// 进程级共享宿主引擎(懒初始化;InstancePre 池与 epoch ticker 进程唯一)。
///
/// 装配层(如 wasm 工具组件装载)没有引擎接线的既有位置,经此取全局
/// 实例——与「全局唯一 Engine」的设计定位一致;首次调用时初始化,
/// 失败每次如实返回(不缓存错误)。
pub fn global_engine() -> Result<Arc<HostEngine>, EngineError> {
    static GLOBAL: Mutex<Option<Arc<HostEngine>>> = Mutex::new(None);
    let mut slot = GLOBAL.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(engine) = &*slot {
        return Ok(Arc::clone(engine));
    }
    let engine = Arc::new(HostEngine::new()?);
    *slot = Some(Arc::clone(&engine));
    Ok(engine)
}

// 说明:两处 `.expect` 是锁中毒兜底——Mutex 正常解锁路径下不可达,
// 属启动期不变式而非可恢复错误,符合项目规范「unwrap/expect 不入非测试代码」的例外条款。
