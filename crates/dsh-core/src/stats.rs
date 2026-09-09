//! 会话统计聚合:事件落档点增量维护 + 推送(session/stats 帧)。
//!
//! 此前统计只能客户端轮询 `session.stats`(全量日志重解析,2s 一拍,
//! 且思考/工具执行的静默期 timer 挂起 →「turn 结束才统一结算」)。
//! 统计本就是事件溯源的派生值:**`apply` 单事件应用逻辑为唯一实现**,
//! 全量解析(RPC 冷读)与直播增量(driver_loop on_event)fold 同一
//! 函数,两路恒等不漂移。

use serde_json::{Value, json};

/// 上下文窗口(DEFAULT_CONTEXT_WINDOW = 1M)
pub const CONTEXT_WINDOW: u64 = 1_000_000;

/// 会话统计聚合(可增量 fold)
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StatsAgg {
    /// 回合数(turn/start 计数)
    turns: u64,
    /// 步数(step/start 计数)
    steps: u64,
    /// LLM 请求耗时累计(audit/call llm/request-done)
    llm_ms: i64,
    /// 工具执行耗时累计
    tool_ms: i64,
    /// 首 token 延迟累计/样本数(均值用)
    ttft_sum: i64,
    ttft_n: u64,
    /// prompt 侧 token 累计
    input_tokens: u64,
    /// completion 侧 token 累计
    output_tokens: u64,
    /// 缓存命中 token 累计
    cached_tokens: u64,
    /// 上下文占用(prompt 侧、**最新采样覆盖**——token-meter
    /// pressureTokens:每次 usage 覆写非峰值;折叠/换模型后环回落)
    context_used: u64,
}

impl StatsAgg {
    /// 单事件应用(非统计相关事件为 no-op;与日志事件类型对齐)
    pub fn apply(&mut self, ty: &str, data: &Value) {
        match ty {
            "turn/start" => self.turns += 1,
            "step/start" => self.steps += 1,
            "audit/call" => {
                let boundary = data["boundary"].as_str().unwrap_or_default();
                let op = data["operation"].as_str().unwrap_or_default();
                let detail = &data["detail"];
                if boundary == "llm" && op == "request-done" {
                    self.llm_ms += detail["durationMs"].as_i64().unwrap_or(0);
                    let usage = &detail["usage"];
                    let pt = usage["prompt_tokens"].as_u64().unwrap_or(0);
                    self.input_tokens += pt;
                    self.output_tokens += usage["completion_tokens"].as_u64().unwrap_or(0);
                    // DeepSeek 顶层 prompt_cache_hit_tokens;OpenAI 兼容端点
                    // 走嵌套 prompt_tokens_details.cached_tokens
                    self.cached_tokens += usage["prompt_cache_hit_tokens"]
                        .as_u64()
                        .or(usage["cached_tokens"].as_u64())
                        .or(usage["prompt_tokens_details"]["cached_tokens"].as_u64())
                        .unwrap_or(0);
                    if pt > 0 {
                        self.context_used = pt;
                    }
                    if let Some(t) = usage["ttftMs"].as_i64() {
                        self.ttft_sum += t;
                        self.ttft_n += 1;
                    }
                } else if boundary == "tool" && detail["durationMs"].is_i64() {
                    self.tool_ms += detail["durationMs"].as_i64().unwrap_or(0);
                }
            }
            _ => {}
        }
    }

    /// 是否统计相关(决定是否推送;chunk 等高频非统计事件不推)
    pub fn is_stats_event(ty: &str) -> bool {
        matches!(ty, "turn/start" | "step/start" | "audit/call")
    }

    /// 输出(session.stats RPC 与 session/stats 推送同形;
    /// 构成三段由 [crate::context] 启发式拆分,由调用方注入)
    pub fn to_json(&self, breakdown: Breakdown) -> Value {
        let cache_hit = if self.input_tokens > 0 {
            (self.cached_tokens as f64 / self.input_tokens as f64 * 100.0).round() as u64
        } else {
            0
        };
        let tok_per_s = if self.llm_ms > 0 && self.output_tokens > 0 {
            (self.output_tokens as f64 / (self.llm_ms as f64 / 1000.0)).round() as u64
        } else {
            0
        };
        json!({
            "turns": self.turns,
            "steps": self.steps,
            "llmMs": self.llm_ms,
            "toolMs": self.tool_ms,
            "firstTokenMs": if self.ttft_n > 0 { self.ttft_sum / self.ttft_n as i64 } else { 0 },
            "tokensPerSecond": tok_per_s,
            "cacheHitPercent": cache_hit,
            "inputTokens": self.input_tokens,
            "outputTokens": self.output_tokens,
            "contextUsed": self.context_used,
            "contextWindow": CONTEXT_WINDOW,
            "contextBreakdown": {
                "systemTokens": breakdown.system_tokens,
                "toolsTokens": breakdown.tools_tokens,
                "messageTokens": breakdown.message_tokens,
            },
        })
    }
}

/// 构成三段(ContextMeter 面板分段条/图例;重放与直播同源)
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Breakdown {
    /// 系统提示段
    pub system_tokens: u64,
    /// 工具定义段
    pub tools_tokens: u64,
    /// 会话消息段
    pub message_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn llm_done(prompt: u64, completion: u64, cached: u64) -> Value {
        json!({
            "boundary": "llm", "operation": "request-done",
            "detail": {
                "durationMs": 1000,
                "usage": {
                    "prompt_tokens": prompt,
                    "completion_tokens": completion,
                    "prompt_cache_hit_tokens": cached,
                    "ttftMs": 200,
                },
            },
        })
    }

    #[test]
    fn agg_counts_and_derived_ratios() {
        let mut a = StatsAgg::default();
        a.apply("turn/start", &json!({}));
        a.apply("step/start", &json!({}));
        a.apply("audit/call", &llm_done(10_000, 500, 5_000));
        a.apply(
            "audit/call",
            &json!({
                "boundary": "tool", "detail": { "durationMs": 300 }
            }),
        );
        let v = a.to_json(Breakdown::default());
        assert_eq!(v["turns"], 1);
        assert_eq!(v["steps"], 1);
        assert_eq!(v["llmMs"], 1000);
        assert_eq!(v["toolMs"], 300);
        assert_eq!(v["firstTokenMs"], 200);
        // 500 tok / 1s
        assert_eq!(v["tokensPerSecond"], 500);
        assert_eq!(v["cacheHitPercent"], 50);
        assert_eq!(v["contextUsed"], 10_000);
    }

    /// 上下文占用 = 最新采样覆盖(折叠/换模型后回落,非峰值)
    #[test]
    fn context_used_is_latest_sample() {
        let mut a = StatsAgg::default();
        a.apply("audit/call", &llm_done(900_000, 1, 0));
        a.apply("audit/call", &llm_done(12_000, 1, 0));
        assert_eq!(a.to_json(Breakdown::default())["contextUsed"], 12_000);
        // prompt=0 的响应(异常/空)不覆盖
        a.apply("audit/call", &llm_done(0, 1, 0));
        assert_eq!(a.to_json(Breakdown::default())["contextUsed"], 12_000);
    }

    /// OpenAI 兼容端点的嵌套口径:prompt_tokens_details.cached_tokens
    /// 也计入命中(DeepSeek 顶层字段优先)
    #[test]
    fn nested_cached_tokens_counted() {
        let mut a = StatsAgg::default();
        a.apply(
            "audit/call",
            &json!({
                "boundary": "llm", "operation": "request-done",
                "detail": { "durationMs": 1000, "usage": {
                    "prompt_tokens": 10_000,
                    "completion_tokens": 100,
                    "prompt_tokens_details": { "cached_tokens": 8_000 },
                }},
            }),
        );
        let v = a.to_json(Breakdown::default());
        assert_eq!(v["cacheHitPercent"], 80);
    }

    /// 非统计事件 no-op + 推送判定
    #[test]
    fn non_stats_events_ignored() {
        let mut a = StatsAgg::default();
        a.apply("assistant/chunk", &json!({ "chunk": {} }));
        a.apply("user/message", &json!({ "content": "x" }));
        assert_eq!(a, StatsAgg::default());
        assert!(!StatsAgg::is_stats_event("assistant/chunk"));
        assert!(StatsAgg::is_stats_event("turn/start"));
        assert!(StatsAgg::is_stats_event("audit/call"));
    }
}
