//! 用量归一:三家官方 usage 键 → 内部规范五键。
//!
//! [`crate::streaming::StreamEvent::Usage`] 载荷契约 = 规范形:
//! `input_tokens` / `output_tokens` / `cached_tokens`(缓存读)/
//! `cache_write_tokens`(缓存写)/ `reasoning_tokens`。各方言映射器在
//! 边界归一,引擎与统计只认规范形。缺席指标不产键(不产 null 帧)。
//!
//! 官方字段依据(2026-09 逐字段核对):
//! - OpenAI Responses(官方 SDK 类型,OpenAPI 规范生成):`input_tokens`、
//!   `input_tokens_details{cached_tokens, cache_write_tokens}`、
//!   `output_tokens`、`output_tokens_details{reasoning_tokens}`
//! - OpenAI chat / DeepSeek(DeepSeek 官方 API 参考):`prompt_tokens`、
//!   `completion_tokens`、`prompt_cache_hit_tokens`(必填)、
//!   `prompt_tokens_details.cached_tokens`、
//!   `completion_tokens_details.reasoning_tokens`
//! - Anthropic Messages(官方 API 参考):`input_tokens`、`output_tokens`、
//!   `cache_read_input_tokens`、`cache_creation_input_tokens`

use serde_json::{Value, json};

/// 规范形构造
fn canonical(
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
    cache_write: Option<u64>,
    reasoning: Option<u64>,
) -> Value {
    let mut o = serde_json::Map::new();
    for (k, v) in [
        ("input_tokens", input),
        ("output_tokens", output),
        ("cached_tokens", cached),
        ("cache_write_tokens", cache_write),
        ("reasoning_tokens", reasoning),
    ] {
        if let Some(v) = v {
            o.insert(k.into(), json!(v));
        }
    }
    Value::Object(o)
}

/// OpenAI Responses 形(DeepSeek /responses 同为官方 Responses 键名;
/// 兜底回退 chat 键——OpenAI 兼容生态存在以 chat 键报 usage 的
/// /responses 实现,回退键均在上列官方出处内)
pub fn normalize_responses(u: &Value) -> Value {
    canonical(
        u["input_tokens"]
            .as_u64()
            .or_else(|| u["prompt_tokens"].as_u64()),
        u["output_tokens"]
            .as_u64()
            .or_else(|| u["completion_tokens"].as_u64()),
        u["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .or_else(|| u["prompt_tokens_details"]["cached_tokens"].as_u64())
            .or_else(|| u["prompt_cache_hit_tokens"].as_u64()),
        u["input_tokens_details"]["cache_write_tokens"].as_u64(),
        u["output_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .or_else(|| u["completion_tokens_details"]["reasoning_tokens"].as_u64()),
    )
}

/// OpenAI chat 形(DeepSeek chat 同构,缓存命中是顶层必填键)
pub fn normalize_chat(u: &Value) -> Value {
    canonical(
        u["prompt_tokens"].as_u64(),
        u["completion_tokens"].as_u64(),
        u["prompt_cache_hit_tokens"]
            .as_u64()
            .or_else(|| u["prompt_tokens_details"]["cached_tokens"].as_u64()),
        None,
        u["completion_tokens_details"]["reasoning_tokens"].as_u64(),
    )
}

/// Anthropic Messages 形(message_start 带输入侧,message_delta 带累计
/// 输出——引擎按键合并两帧互补)
pub fn normalize_anthropic(u: &Value) -> Value {
    canonical(
        u["input_tokens"].as_u64(),
        u["output_tokens"].as_u64(),
        u["cache_read_input_tokens"].as_u64(),
        u["cache_creation_input_tokens"].as_u64(),
        u["output_tokens_details"]["thinking_tokens"].as_u64(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// OpenAI Responses 规范键(GLM 真机同形)
    #[test]
    fn responses_spec_keys() {
        let n = normalize_responses(&json!({
            "input_tokens": 2573,
            "input_tokens_details": { "cached_tokens": 2400, "cache_write_tokens": 10 },
            "output_tokens": 210,
            "output_tokens_details": { "reasoning_tokens": 64 },
            "total_tokens": 2783,
        }));
        assert_eq!(
            n,
            json!({
                "input_tokens": 2573,
                "output_tokens": 210,
                "cached_tokens": 2400,
                "cache_write_tokens": 10,
                "reasoning_tokens": 64,
            })
        );
    }

    /// /responses 端点回退 chat 键(偏差端点兜底)
    #[test]
    fn responses_falls_back_to_chat_keys() {
        let n = normalize_responses(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_cache_hit_tokens": 80,
        }));
        assert_eq!(
            n,
            json!({ "input_tokens": 100, "output_tokens": 20, "cached_tokens": 80 })
        );
    }

    /// chat 形:DeepSeek 顶层必填命中键;OpenAI 嵌套 cached 同归
    #[test]
    fn chat_both_cache_spellings() {
        let deepseek = normalize_chat(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20,
            "completion_tokens_details": { "reasoning_tokens": 5 },
        }));
        assert_eq!(
            deepseek,
            json!({
                "input_tokens": 100,
                "output_tokens": 20,
                "cached_tokens": 80,
                "reasoning_tokens": 5,
            })
        );
        let openai = normalize_chat(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": { "cached_tokens": 60 },
        }));
        assert_eq!(openai["cached_tokens"], json!(60));
    }

    /// anthropic 形:缓存读/写分列
    #[test]
    fn anthropic_cache_read_write() {
        let n = normalize_anthropic(&json!({
            "input_tokens": 2095,
            "output_tokens": 503,
            "cache_creation_input_tokens": 209,
            "cache_read_input_tokens": 1046,
        }));
        assert_eq!(
            n,
            json!({
                "input_tokens": 2095,
                "output_tokens": 503,
                "cached_tokens": 1046,
                "cache_write_tokens": 209,
            })
        );
    }

    /// 全缺席 → 空对象(不产 null)
    #[test]
    fn absent_keys_yield_empty_object() {
        assert_eq!(
            normalize_chat(&json!({ "service_tier": "default" })),
            json!({})
        );
    }
}
