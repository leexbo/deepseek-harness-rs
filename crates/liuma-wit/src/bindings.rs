//! host 侧 bindgen:session world 的类型化调用接口。
//!
//! wasmtime `bindgen!` 从 `wit/session/` 生成宿主调用桩(含 deps/ 依赖解析);
//! 后续组件 world(loop/prompt/tools/plugin)在此逐一追加,
//! 生成类型与 WIT 契约的对应由 verify-wit + 组件契约测试共同锁定。

// bindgen 生成项不带 rustdoc,模块级豁免 missing_docs(crate 级 deny 不放松)
#[allow(missing_docs)]
pub mod session {
    wasmtime::component::bindgen!({
        path: "../../wit/session",
        world: "session",
    });
}

/// 工具组件 world 的宿主调用桩(WasmTool 桥用)
#[allow(missing_docs)]
pub mod tool {
    wasmtime::component::bindgen!({
        path: "../../wit/tools",
        world: "tool-component",
    });
}
