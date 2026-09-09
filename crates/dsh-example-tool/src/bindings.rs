//! guest 侧 bindgen 辅助:dsh:json/value 的本地映射。
//!
//! `wit/tools/deps/` 是依赖副本集(wit-bindgen 的 deps/ 解析约定),与唯一
//! 契约源的一致性由 dsh-wit 的 verify-wit 测试锁定。

/// dsh:json/value 的本地实现:无损 JSON 载荷 = 序列化字节背板
pub mod dsh_json {
    /// json 载荷(list<u8> 背板)
    pub type Json = Vec<u8>;
}
