//! 跨功能 UI 支持集(非功能、无状态):素材(主题/图标/脚手架)与纯算法,
//! 供各功能切片与 shell 底座共享;模态 chrome 见 menus.rs/`modals`。

pub(crate) mod cache;
pub(crate) mod highlight;
pub(crate) mod icons;
pub(crate) mod markdown;
pub(crate) mod mermaid;
pub(crate) mod modals;
pub(crate) mod state_dot;
pub(crate) mod theme;
