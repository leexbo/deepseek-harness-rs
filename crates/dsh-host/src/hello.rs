//! hello 组件回路:`dsh --version` 经一次真实组件调用产生输出。
//!
//! 组件以 wat 文本内嵌(canon lift 把核心函数导出为组件函数);
//! Rust→wasm32-wasip2 的真实组件构建回路随第一个组件 world(session)接入,
//! 本回路验证的是宿主侧 Engine/InstancePre/Arena/调用链全部环节。

// bindgen 生成项不带 rustdoc,模块级豁免 missing_docs(crate 级 deny 不放松)
#[allow(missing_docs)]
mod bindings {
    wasmtime::component::bindgen!({
        inline: r#"
            package dsh:hello@0.1.0;

            world hello {
                /// 出口验证:一次真实组件调用
                export version: func() -> u32;
            }
        "#
    });
}
pub use bindings::Hello;

/// 内嵌 hello 组件(wat)。
///
/// 组件函数 `version` 返回 u32 编码的 semver(major<<16 | minor<<8 | patch),
/// canon lift 无需线性内存;当前编码 0.1.0(= 0<<16 | 1<<8 | 0 = 256)。
pub const HELLO_WAT: &str = r#"
(component
  (core module $m
    (func (export "version") (result i32) i32.const 256)
  )
  (core instance $i (instantiate $m))
  (func (export "version") (result u32)
    (canon lift (core func $i "version"))
  )
)
"#;

/// u32 semver 编码(major<<16 | minor<<8 | patch)
pub fn encode_semver(major: u16, minor: u16, patch: u16) -> u32 {
    (major as u32) << 16 | (minor as u32) << 8 | patch as u32
}

/// u32 semver 解码为 `major.minor.patch` 字符串
pub fn decode_semver(v: u32) -> String {
    format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_roundtrip() {
        assert_eq!(encode_semver(0, 1, 0), 256);
        assert_eq!(decode_semver(encode_semver(1, 2, 3)), "1.2.3");
    }
}
