//! 插件生命周期注册表(native 形态)。
//!
//! 继承 Cordis 语义(重写方案 §3):
//! - 状态机 `running → disposing → destroyed`;disposing 期拒绝新注册
//!   (对应 UNLOADING 期拒绝 effect 注册);
//! - 销毁按注册**逆序**(fiber 树逆序回滚的等价物);
//! - 销毁错误 **per-plugin 包含**:单个插件销毁失败不阻断其余插件的回收
//!   (native 阶段粒度即插件;listener 级包含在总线层,见 bus.rs);
//! - 订阅随插件销毁自动退订。
//!
//! wasm 组件插件的对应物:dispose() export 逆序 await + 订阅是数据
//! (WIT `dsh:plugin/lifecycle`)。

use std::sync::Arc;

use thiserror::Error;

use crate::bus::EventBus;

/// 注册表状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryState {
    /// 接受注册
    Running,
    /// 销毁中:拒绝新注册
    Disposing,
    /// 终态
    Destroyed,
}

/// 插件错误
#[derive(Debug, Error, PartialEq)]
pub enum PluginError {
    /// disposing/destroyed 期拒绝注册(继承 UNLOADING 拒绝语义)
    #[error("registry is disposing/destroyed; registration refused")]
    Refused,
    /// 某插件销毁失败(包含:其余插件已按序销毁)
    #[error("plugin `{0}` dispose failed: {1}")]
    DisposeFailed(String, String),
}

/// 单个插件的登记项
struct PluginEntry {
    name: String,
    /// 该插件持有的订阅(注册时申报;销毁时自动退订)
    subscriptions: Vec<(String, u64)>,
    /// 销毁回调(逆序调用;失败包含)
    dispose: Option<Box<dyn FnOnce() -> Result<(), String> + Send>>,
}

/// 插件生命周期注册表
pub struct PluginRegistry {
    bus: Arc<EventBus>,
    entries: Vec<PluginEntry>,
    state: RegistryState,
}

impl PluginRegistry {
    /// 以总线构建(销毁时经它退订)
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self {
            bus,
            entries: Vec::new(),
            state: RegistryState::Running,
        }
    }

    /// 当前状态
    pub fn state(&self) -> RegistryState {
        self.state
    }

    /// 已注册插件名(注册序)
    pub fn plugins(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// 注册插件:申报订阅 + 可选销毁回调。
    ///
    /// 真实初始化(init/config)由调用方在注册前完成——native 阶段
    /// 插件即宿主内结构体;wasm 组件形态下对应 lifecycle.init(config)。
    pub fn register(
        &mut self,
        name: &str,
        subscriptions: Vec<(String, u64)>,
        dispose: Option<Box<dyn FnOnce() -> Result<(), String> + Send>>,
    ) -> Result<(), PluginError> {
        if self.state != RegistryState::Running {
            return Err(PluginError::Refused);
        }
        self.entries.push(PluginEntry {
            name: name.to_string(),
            subscriptions,
            dispose,
        });
        Ok(())
    }

    /// 全部销毁:注册逆序;先退订再调 dispose;
    /// 单插件失败被包含(收集返回,不阻断其余)。
    pub fn dispose_all(&mut self) -> Vec<PluginError> {
        self.state = RegistryState::Disposing;
        let mut errors = Vec::new();
        while let Some(entry) = self.entries.pop() {
            for (event, id) in &entry.subscriptions {
                self.bus.unsubscribe(event, *id);
            }
            if let Some(dispose) = entry.dispose
                && let Err(e) = dispose()
            {
                errors.push(PluginError::DisposeFailed(entry.name, e));
            }
        }
        self.state = RegistryState::Destroyed;
        errors
    }
}
