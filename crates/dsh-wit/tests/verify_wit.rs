//! verify-wit:`wit/` 契约层的解析级校验。
//!
//! 任何语法错误、跨包 `use` 解析失败、world 组合不完整都会在此失败——
//! 这是「WIT 契约先行」的强制点:契约不过,实现不得动工。
//!
//! 注:wit-parser 的 `push_dir` 只读取目录顶层的 `*.wit`(子目录被忽略),
//! 因此按依赖序逐包推入;顺序变化导致的解析失败本身就是契约层依赖断裂的信号。

use std::collections::BTreeSet;
use wit_parser::{Resolve, WorldKey};

/// 期望存在的包(name@version)
const EXPECTED_PACKAGES: &[&str] = &[
    "dsh:json@0.1.0",
    "dsh:session@0.1.0",
    "dsh:loop@0.1.0",
    "dsh:events@0.1.0",
    "dsh:host@0.1.0",
    "dsh:plugin@0.1.0",
    "dsh:tools@0.1.0",
];

/// 包目录,按依赖序。dsh:json 不单独 push:经 wit/session/deps/ 进入
/// (bindgen 的依赖解析约定,副本一致性由 session_deps_json_in_sync 锁定),
/// 其余包对其的 use 依「已注册即解析」满足。
/// dsh:tools 走 push_source:其 deps/ 副本集是 bindgen 的依赖解析约定,
/// push_dir 会连带重注册依赖包(re-add 崩),只推顶层契约文本。
fn load() -> Resolve {
    let mut resolve = Resolve::new();
    for dir in ["session", "loop", "events", "host", "plugin"] {
        let path = std::path::Path::new(dsh_wit::WIT_DIR).join(dir);
        resolve
            .push_dir(&path)
            .unwrap_or_else(|e| panic!("wit/{dir} 解析失败:{e:#}"));
    }
    let tools_path = std::path::Path::new(dsh_wit::WIT_DIR).join("tools/tools.wit");
    let tools = std::fs::read_to_string(&tools_path).expect("read wit/tools/tools.wit");
    resolve
        .push_source("wit/tools/tools.wit", &tools)
        .unwrap_or_else(|e| panic!("wit/tools 解析失败:{e:#}"));
    resolve
}

#[test]
fn wit_packages_parse_and_resolve() {
    let resolve = load();

    let names: BTreeSet<String> = resolve
        .packages
        .iter()
        .map(|(_, pkg)| pkg.name.to_string())
        .collect();

    for expected in EXPECTED_PACKAGES {
        assert!(
            names.contains(*expected),
            "缺少包 {expected},实际解析到:{names:?}"
        );
    }
    assert_eq!(
        names.len(),
        EXPECTED_PACKAGES.len(),
        "出现未登记的包:{names:?}(请更新 EXPECTED_PACKAGES)"
    );
}

#[test]
fn plugin_world_shape() {
    let resolve = load();

    let world = resolve
        .worlds
        .iter()
        .find(|(_, w)| w.name == "plugin")
        .map(|(_, w)| w)
        .expect("world `plugin` 未定义");

    let named = |resolve: &Resolve, map: &wit_parser::IndexMap<WorldKey, wit_parser::WorldItem>| {
        map.keys()
            .filter_map(|k| match k {
                WorldKey::Name(n) => Some(n.clone()),
                WorldKey::Interface(id) => {
                    let iface = &resolve.interfaces[*id];
                    let pkg = iface.package.map(|p| &resolve.packages[p])?;
                    Some(format!(
                        "{}/{}",
                        pkg.name,
                        iface.name.as_deref().unwrap_or("")
                    ))
                }
            })
            .collect::<BTreeSet<_>>()
    };
    let imports = named(&resolve, &world.imports);
    let exports = named(&resolve, &world.exports);

    for required in [
        "dsh:events@0.1.0/bus",
        "dsh:host@0.1.0/process",
        "dsh:host@0.1.0/cancel",
        "dsh:host@0.1.0/registry",
        "dsh:host@0.1.0/llm-transport",
        // 传递依赖一并并入(json 载荷类型、events 公共类型)
        "dsh:json@0.1.0/value",
        "dsh:events@0.1.0/types",
    ] {
        assert!(
            imports.contains(required),
            "plugin world 缺少 import {required}(实际:{imports:?})"
        );
    }
    for required in ["dsh:events@0.1.0/consumer", "dsh:plugin@0.1.0/lifecycle"] {
        assert!(
            exports.contains(required),
            "plugin world 缺少 export {required}(实际:{exports:?})"
        );
    }
}

#[test]
fn tool_world_shape() {
    // 工具组件 world:零 import(能力边界 = import 集:纯 json→json,
    // wasi/cancel/进程/LLM 面不可达);export 生命周期 + 工具面
    let resolve = load();
    let world = resolve
        .worlds
        .iter()
        .find(|(_, w)| w.name == "tool-component")
        .map(|(_, w)| w)
        .expect("world `tool-component` 未定义");

    let named = |resolve: &Resolve, map: &wit_parser::IndexMap<WorldKey, wit_parser::WorldItem>| {
        map.keys()
            .filter_map(|k| match k {
                WorldKey::Name(n) => Some(n.clone()),
                WorldKey::Interface(id) => {
                    let iface = &resolve.interfaces[*id];
                    let pkg = iface.package.map(|p| &resolve.packages[p])?;
                    Some(format!(
                        "{}/{}",
                        pkg.name,
                        iface.name.as_deref().unwrap_or("")
                    ))
                }
            })
            .collect::<BTreeSet<_>>()
    };
    let imports = named(&resolve, &world.imports);
    let exports = named(&resolve, &world.exports);
    // 能力边界:唯一允许的 import 是 dsh:json/value(纯类型包,export 的
    // tools interface 对它的 use 被 world elaboration 提升为类型依赖,
    // 无宿主函数);进程/取消/registry/LLM/wasi 面一律不可达
    assert_eq!(
        imports,
        BTreeSet::from(["dsh:json@0.1.0/value".to_string()]),
        "tool world 的 import 只允许 json 类型依赖:{imports:?}"
    );
    for required in ["dsh:plugin@0.1.0/lifecycle", "dsh:tools@0.1.0/tools"] {
        assert!(
            exports.contains(required),
            "tool world 缺少 export {required}(实际:{exports:?})"
        );
    }
}

#[test]
fn session_deps_json_in_sync() {
    // wit/session/deps/json.wit 是 dsh:json 的副本(bindgen 的依赖解析约定),
    // 与唯一契约源 wit/json/json.wit 必须逐字节一致——漂移即契约分叉。
    let wit = std::path::Path::new(dsh_wit::WIT_DIR);
    let source = std::fs::read_to_string(wit.join("json/json.wit")).expect("read wit/json");
    let deps = std::fs::read_to_string(wit.join("session/deps/json.wit")).expect("read deps copy");
    assert_eq!(
        source, deps,
        "wit/session/deps/json.wit 与 wit/json/json.wit 漂移,请同步副本"
    );
}

#[test]
fn tools_deps_in_sync() {
    // wit/tools/deps/ 是 bindgen 依赖解析约定的副本集(tool world 引用
    // dsh:plugin/lifecycle 与 dsh:json;plugin.wit 连带要求 events/host),
    // 与唯一契约源必须逐字节一致——漂移即契约分叉
    let wit = std::path::Path::new(dsh_wit::WIT_DIR);
    for name in ["json", "plugin", "events", "host"] {
        let source = std::fs::read_to_string(wit.join(format!("{name}/{name}.wit")))
            .unwrap_or_else(|e| panic!("read wit/{name}:{e}"));
        let deps = std::fs::read_to_string(wit.join(format!("tools/deps/{name}.wit")))
            .unwrap_or_else(|e| panic!("read tools/deps/{name}:{e}"));
        assert_eq!(
            source, deps,
            "wit/tools/deps/{name}.wit 与 wit/{name}/{name}.wit 漂移,请同步副本"
        );
    }
}
