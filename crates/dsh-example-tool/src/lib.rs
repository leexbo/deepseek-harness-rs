//! 示例 wasm 工具组件:`dsh:tools` world 的参考实现。
//!
//! 两个工具:
//! - `echo_config`:回显 init 时的 config 与调用入参(json→json 往返);
//! - `spin`:纯死循环(epoch 硬停测试的物料,E4 兜底验证)。
//!
//! config 契约:宿主经 `config-schema()` 拿 JSON Schema 校验 preset mount 行
//! 的 config(类型即校验),再 `init(config)`;本组件要求 `label`(必填)。
//!
//! 双产物:rlib(native 空桩)+ wasm32-wasip2 组件(guest 实现 WIT 导出)。

#[cfg_attr(not(target_family = "wasm"), allow(missing_docs))]
pub mod bindings;
#[cfg_attr(not(target_family = "wasm"), allow(missing_docs))]
pub mod guest;

use guest::ExampleToolComponent;

wit_bindgen::generate!({
    path: "../../wit/tools",
    world: "tool-component",
    with: { "dsh:json/value@0.1.0": bindings::dsh_json },
});

export!(ExampleToolComponent);

// wit-bindgen 的接口入口与 cabi_post 清理钩子只在 wasm 目标生成实体;native 侧
// cdylib 链接时导出表仍引用这些符号(rlib 测试路径需要 crate-type 双轨),补空桩。
#[cfg(not(target_family = "wasm"))]
mod native_cabi_stubs {
    #[unsafe(export_name = "dsh:plugin/lifecycle@0.1.0")]
    extern "C" fn lifecycle_entry() {}
    #[unsafe(export_name = "dsh:tools/tools@0.1.0")]
    extern "C" fn tools_entry() {}
    #[unsafe(export_name = "cabi_post_dsh:plugin/lifecycle@0.1.0")]
    extern "C" fn lifecycle_post() {}
    #[unsafe(export_name = "cabi_post_dsh:tools/tools@0.1.0")]
    extern "C" fn tools_post() {}
}
