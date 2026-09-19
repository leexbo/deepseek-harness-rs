//! plan 族事件载荷形状校验(写入 chokepoint 调用;镜像源 plan-mode-invariant
//! 插件——校验 plan/mode 载荷形状,坏形状在写入前拦截)。

use serde_json::Value;

/// 校验 plan 族事件载荷(非 plan 族类型恒 Ok,不归本模块管)。
///
/// - `session/mode`:mode ∈ {standard, plan}
/// - `plan/submitted` | `plan/approved` | `plan/cancelled`:plan 为非空字符串
/// - `plan/declined`:plan 为非空字符串;feedback 缺省或非空字符串
pub fn validate_payload(ty: &str, data: &Value) -> Result<(), String> {
    match ty {
        "session/mode" => {
            let mode = data["mode"]
                .as_str()
                .ok_or_else(|| "session/mode 载荷需要字符串字段 mode".to_string())?;
            if matches!(mode, "standard" | "plan") {
                Ok(())
            } else {
                Err(format!(
                    "session/mode 取值必须为 standard 或 plan(收到 {mode})"
                ))
            }
        }
        "plan/submitted" | "plan/approved" | "plan/cancelled" | "plan/declined" => {
            let plan = data["plan"]
                .as_str()
                .filter(|p| !p.trim().is_empty())
                .ok_or_else(|| format!("{ty} 载荷需要非空字符串字段 plan"))?;
            let _ = plan;
            if ty == "plan/declined"
                && let Some(fb) = data["feedback"].as_str()
                && fb.trim().is_empty()
            {
                return Err("plan/declined 的 feedback 须为非空字符串或缺省".into());
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mode_payload_shape() {
        assert!(validate_payload("session/mode", &json!({ "mode": "plan" })).is_ok());
        assert!(validate_payload("session/mode", &json!({ "mode": "standard" })).is_ok());
        assert!(validate_payload("session/mode", &json!({ "mode": "PLAN" })).is_err());
        assert!(validate_payload("session/mode", &json!({})).is_err());
        assert!(validate_payload("session/mode", &json!({ "mode": 1 })).is_err());
    }

    #[test]
    fn plan_family_payload_shape() {
        for ty in [
            "plan/submitted",
            "plan/approved",
            "plan/cancelled",
            "plan/declined",
        ] {
            assert!(validate_payload(ty, &json!({ "plan": "# p" })).is_ok());
            assert!(
                validate_payload(ty, &json!({ "plan": "  " })).is_err(),
                "{ty} 空 plan 应拒"
            );
            assert!(validate_payload(ty, &json!({})).is_err());
        }
        // declined 的 feedback:缺省/非空 ok,空白拒
        assert!(validate_payload("plan/declined", &json!({ "plan": "# p" })).is_ok());
        assert!(
            validate_payload(
                "plan/declined",
                &json!({ "plan": "# p", "feedback": "use OAuth" })
            )
            .is_ok()
        );
        assert!(
            validate_payload("plan/declined", &json!({ "plan": "# p", "feedback": " " })).is_err()
        );
    }

    #[test]
    fn foreign_types_pass_through() {
        assert!(validate_payload("todo/write", &json!({})).is_ok());
        assert!(validate_payload("user/message", &json!("任意形状")).is_ok());
    }
}
