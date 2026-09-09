//! 「模型可见 ⟺ 已记录」不变式:内容级 derive-and-compare 校验器。
//!
//! 双比对结构:
//! 请求的 messages 必须与从 session 日志派生的消息**整体等价**,
//! 折叠 header(model/system/temperature/maxTokens/stop/tools)同理。
//! 校验器挂在 `llm-transport` 调用边界(宿主特权强制,组件无法绕过);
//! 以纯函数 + 测试形态存在(不变式必测)。
//!
//! 注意:seq 高水位检查只是快速前置(廉价拒绝),不承担不变式本体——
//! 引用合法 seq 不等于内容与派生一致。

use serde_json::Value;

/// 不变式违反
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum InvariantViolation {
    /// 请求消息与日志派生不一致(内容级失败)
    #[error("request messages diverge from the durable derivation (log-reconstruction desync)")]
    MessagesDiverge {
        /// 派生期望(JSON 规范化文本)
        derived: String,
        /// 实际请求(JSON 规范化文本)
        actual: String,
    },
    /// 折叠 header 与派生不一致
    #[error("request header diverge from the durable derivation")]
    HeaderDiverge {
        /// 派生期望
        derived: String,
        /// 实际请求
        actual: String,
    },
}

/// 规范化比较:serde_json::Value 的对象键序不定,比较用规范化文本
/// (序列化前对对象键排序;数组保序——消息顺序是语义)。
pub fn canonical(v: &Value) -> String {
    let mut sorted = v.clone();
    sort_object_keys(&mut sorted);
    sorted.to_string()
}

fn sort_object_keys(v: &mut Value) {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<_> = map
                .iter()
                .map(|(k, val)| (k.clone(), val.clone()))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut new_map = serde_json::Map::new();
            for (k, mut val) in entries {
                sort_object_keys(&mut val);
                new_map.insert(k, val);
            }
            *map = new_map;
        }
        Value::Array(items) => {
            for item in items {
                sort_object_keys(item);
            }
        }
        _ => {}
    }
}

/// 比对一对「期望派生 vs 实际请求」载荷。
///
/// `derived` 由宿主调 session 组件 derive 产生;
/// `actual` 是组件发起的出网请求载荷。语义等价(规范化后相等)即通过。
pub fn verify_payload(derived: &Value, actual: &Value) -> Result<(), InvariantViolation> {
    let (d, a) = (canonical(derived), canonical(actual));
    if d == a {
        Ok(())
    } else {
        Err(InvariantViolation::MessagesDiverge {
            derived: d,
            actual: a,
        })
    }
}

/// 双比对入口:messages + 折叠 header(invariant.ts 对应结构)。
pub fn verify_request(
    derived_messages: &Value,
    actual_messages: &Value,
    derived_header: &Value,
    actual_header: &Value,
) -> Result<(), InvariantViolation> {
    verify_payload(derived_messages, actual_messages).map_err(|e| match e {
        InvariantViolation::MessagesDiverge { .. } => e,
        InvariantViolation::HeaderDiverge { .. } => e,
    })?;
    verify_payload(derived_header, actual_header).map_err(|e| match e {
        InvariantViolation::MessagesDiverge { derived, actual } => {
            InvariantViolation::HeaderDiverge { derived, actual }
        }
        InvariantViolation::HeaderDiverge { .. } => e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identical_payload_passes() {
        let m = json!([{"role": "user", "content": "hi"}]);
        assert_eq!(verify_payload(&m, &m), Ok(()));
    }

    #[test]
    fn key_order_is_not_a_divergence() {
        // 对象键序不定:语义等价的载荷必须通过(规范化比较的意义)
        let derived = json!([{"role": "user", "content": "hi"}]);
        let actual = json!([{"content": "hi", "role": "user"}]);
        assert_eq!(verify_payload(&derived, &actual), Ok(()));
    }

    #[test]
    fn content_rewrite_is_rejected() {
        // 核心场景:引用合法 seq 但改写内容 → 拒绝
        let derived = json!([{"role": "user", "content": "hi"}]);
        let actual = json!([{"role": "user", "content": "TAMPERED"}]);
        assert!(matches!(
            verify_payload(&derived, &actual),
            Err(InvariantViolation::MessagesDiverge { .. })
        ));
    }

    #[test]
    fn message_order_is_semantic() {
        // 数组保序:消息顺序不同即发散(与对象键序的语义区别)
        let derived = json!([{"i": 1}, {"i": 2}]);
        let actual = json!([{"i": 2}, {"i": 1}]);
        assert!(verify_payload(&derived, &actual).is_err());
    }

    #[test]
    fn header_divergence_distinguished() {
        let msgs = json!([]);
        assert!(matches!(
            verify_request(
                &msgs,
                &msgs,
                &json!({"model": "a", "temperature": 0.7}),
                &json!({"model": "a", "temperature": 1.0}),
            ),
            Err(InvariantViolation::HeaderDiverge { .. })
        ));
    }
}
