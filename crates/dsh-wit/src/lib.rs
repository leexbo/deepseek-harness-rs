//! WIT 契约层的 host 侧入口。
//!
//! 职责:
//! - `tests/verify_wit.rs`:`wit/` 目录全部包的解析与解析级校验
//!   (verify-wit;契约先行的强制点,重写方案硬性原则 1)
//! - `bindings`:wasmtime host 侧 bindgen 产物(session world 起,逐组件追加)
//!
//! 唯一契约源是仓库根的 `wit/` 目录,本 crate 不定义接口,只消费它。

#![deny(missing_docs)]

pub mod bindings;

pub use bindings::{session, tool};

/// 契约层根目录(仓库 `wit/`)
pub const WIT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../wit");
