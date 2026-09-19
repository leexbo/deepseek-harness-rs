//! 会话统计聚合:事件落档点增量维护 + 推送(session/stats 帧)。
//!
//! 此前统计只能客户端轮询 `session.stats`(全量日志重解析,2s 一拍,
//! 且思考/工具执行的静默期 timer 挂起 →「turn 结束才统一结算」)。
//! 统计本就是事件溯源的派生值:**`apply` 单事件应用逻辑为唯一实现**,
//! 全量解析(RPC 冷读)与直播增量(driver_loop on_event)fold 同一
//! 函数,两路恒等不漂移。
//!
//! 口径:
//! - TPS = Σ输出 tokens / Σ解码秒;解码 = 请求耗时 − 首 token 延迟
//!   (只计同时有计时与 tokens 的请求;无解码样本回落 总输出/总请求时长)
//! - 首 token 均值 = Σ首 token 延迟 / 有记录的请求数
//! - 缓存命中 = 缓存读 / prompt 侧总输入(未缓存+缓存读+缓存写)

use serde_json::{Value, json};

/// 上下文窗口缺省值(未配置 per-model 时;压缩阈值同源读
/// `liuma_compaction::DEFAULT_CONTEXT_WINDOW`)
pub const CONTEXT_WINDOW: u64 = liuma_compaction::DEFAULT_CONTEXT_WINDOW;

/// 单轮统计桶(turn/start 开桶,turn/end 收桶;事件序由日志保证,
/// 同一时刻至多一个开桶)
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TurnStats {
    /// 轮号(turn/start 计数,与 Translator 客户端计数同源)
    pub turn: u64,
    start_ms: i64,
    end_ms: i64,
    llm_ms: i64,
    tool_ms: i64,
    /// 轮内最早首 token 延迟(取最早步的 TTFT)
    ttft_min: Option<i64>,
    decode_ms: i64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
    /// 轮内出现过的模型(去重保序;routes 的模型半边)
    models: Vec<String>,
}

impl TurnStats {
    /// 轮桶序列化。`provider_id` = 会话生效提供方 id(routes 的提供方
    /// 半边;引擎审计只记模型名,提供方由宿主按当前生效值补——中途
    /// 换过提供方的历史轮可能标错提供方,模型名恒准确)
    pub fn to_json(&self, provider_id: &str) -> Value {
        let tps = tps_of(self.decode_ms, self.llm_ms, self.output_tokens);
        json!({
            "turn": self.turn,
            "runMs": (self.end_ms - self.start_ms).max(0),
            "llmMs": self.llm_ms,
            "toolMs": self.tool_ms,
            "ttftMs": self.ttft_min.unwrap_or(0),
            "tokensPerSecond": tps,
            "uncachedInputTokens": self.input_tokens
                .saturating_sub(self.cached_tokens + self.cache_write_tokens),
            "cacheReadTokens": self.cached_tokens,
            "cacheWriteTokens": self.cache_write_tokens,
            "outputTokens": self.output_tokens,
            "reasoningTokens": self.reasoning_tokens,
            "routes": self
                .models
                .iter()
                .map(|m| format!("{provider_id}/{m}"))
                .collect::<Vec<_>>(),
        })
    }

    fn apply_llm_request(&mut self, detail: &Value) {
        self.llm_ms += detail["durationMs"].as_i64().unwrap_or(0);
        if let Some(m) = detail["model"].as_str()
            && !self.models.iter().any(|x| x == m)
        {
            self.models.push(m.to_string());
        }
        let usage = &detail["usage"];
        let out = usage["output_tokens"].as_u64().unwrap_or(0);
        self.input_tokens += usage["input_tokens"].as_u64().unwrap_or(0);
        self.output_tokens += out;
        self.cached_tokens += usage["cached_tokens"].as_u64().unwrap_or(0);
        self.cache_write_tokens += usage["cache_write_tokens"].as_u64().unwrap_or(0);
        self.reasoning_tokens += usage["reasoning_tokens"].as_u64().unwrap_or(0);
        if let Some(t) = usage["ttftMs"].as_i64() {
            self.ttft_min = Some(self.ttft_min.map_or(t, |cur: i64| cur.min(t)));
            if out > 0 {
                let dur = detail["durationMs"].as_i64().unwrap_or(0);
                self.decode_ms += (dur - t).max(0);
            }
        }
    }
}

/// TPS 统一口径:解码秒优先(输出/解码),无解码样本回落总请求时长
fn tps_of(decode_ms: i64, llm_ms: i64, output_tokens: u64) -> u64 {
    let base = if decode_ms > 0 { decode_ms } else { llm_ms };
    if base > 0 && output_tokens > 0 {
        (output_tokens as f64 / (base as f64 / 1000.0)).round() as u64
    } else {
        0
    }
}

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
    /// 缓存写入 token 累计
    cache_write_tokens: u64,
    /// 推理 token 累计(输出侧子集)
    reasoning_tokens: u64,
    /// 解码耗时累计(请求耗时 − 首 token;TPS 分母)
    decode_ms: i64,
    /// 上下文占用(prompt 侧、**最新采样覆盖**——token-meter
    /// pressureTokens:每次 usage 覆写非峰值;折叠/换模型后环回落)
    context_used: u64,
    /// 当前开启的轮桶
    open_turn: Option<TurnStats>,
    /// 最近完成的轮桶(live 推送 lastTurn)
    last_turn: Option<TurnStats>,
    /// 全部完成轮桶(仅冷读全量形态留存;live 不留省内存)
    completed_turns: Vec<TurnStats>,
    /// 是否留存全部完成轮桶(冷读 true / live false)
    retain_turns: bool,
}

impl StatsAgg {
    /// 直播形态:只留最近完成轮(每会话常驻内存,不累积历史桶)
    pub fn new() -> Self {
        Self::default()
    }

    /// 冷读形态:留存全部完成轮桶(session_stats RPC 的 turnList)
    pub fn with_retained_turns() -> Self {
        Self {
            retain_turns: true,
            ..Self::default()
        }
    }

    /// 单事件应用(非统计相关事件为 no-op;与日志事件类型对齐)
    pub fn apply(&mut self, ty: &str, time_ms: i64, data: &Value) {
        match ty {
            "turn/start" => {
                self.turns += 1;
                self.open_turn = Some(TurnStats {
                    turn: self.turns,
                    start_ms: time_ms,
                    ..TurnStats::default()
                });
            }
            "step/start" => self.steps += 1,
            "turn/end" => {
                if let Some(mut t) = self.open_turn.take() {
                    t.end_ms = time_ms;
                    if self.retain_turns {
                        self.completed_turns.push(t.clone());
                    }
                    self.last_turn = Some(t);
                }
            }
            "audit/call" => {
                let boundary = data["boundary"].as_str().unwrap_or_default();
                let op = data["operation"].as_str().unwrap_or_default();
                let detail = &data["detail"];
                if boundary == "llm" && op == "request-done" {
                    // usage 为映射器归一后的规范形(见 liuma-llm::usage)
                    self.llm_ms += detail["durationMs"].as_i64().unwrap_or(0);
                    let usage = &detail["usage"];
                    let pt = usage["input_tokens"].as_u64().unwrap_or(0);
                    let out = usage["output_tokens"].as_u64().unwrap_or(0);
                    self.input_tokens += pt;
                    self.output_tokens += out;
                    self.cached_tokens += usage["cached_tokens"].as_u64().unwrap_or(0);
                    self.cache_write_tokens += usage["cache_write_tokens"].as_u64().unwrap_or(0);
                    self.reasoning_tokens += usage["reasoning_tokens"].as_u64().unwrap_or(0);
                    if pt > 0 {
                        self.context_used = pt;
                    }
                    if let Some(t) = usage["ttftMs"].as_i64() {
                        self.ttft_sum += t;
                        self.ttft_n += 1;
                        if out > 0 {
                            self.decode_ms +=
                                (detail["durationMs"].as_i64().unwrap_or(0) - t).max(0);
                        }
                    }
                    if let Some(t) = self.open_turn.as_mut() {
                        t.apply_llm_request(detail);
                    }
                } else if boundary == "tool" && detail["durationMs"].is_i64() {
                    let ms = detail["durationMs"].as_i64().unwrap_or(0);
                    self.tool_ms += ms;
                    if let Some(t) = self.open_turn.as_mut() {
                        t.tool_ms += ms;
                    }
                }
            }
            _ => {}
        }
    }

    /// 是否统计相关(决定是否推送;chunk 等高频非统计事件不推;
    /// turn/end 在列 = 轮桶收口即推,轮尾即时拿到本轮用量)
    pub fn is_stats_event(ty: &str) -> bool {
        matches!(ty, "turn/start" | "step/start" | "turn/end" | "audit/call")
    }

    /// 输出(session.stats RPC 与 session/stats 推送同形;
    /// 构成三段由 [crate::context] 启发式拆分,由调用方注入;
    /// `context_window` = 会话当前模型窗口,由宿主解析后传入——
    /// 与压缩阈值同源,不在此处硬编码)
    pub fn to_json(&self, breakdown: Breakdown, context_window: u64) -> Value {
        let cache_hit = if self.input_tokens > 0 {
            (self.cached_tokens as f64 / self.input_tokens as f64 * 100.0).round() as u64
        } else {
            0
        };
        json!({
            "turns": self.turns,
            "steps": self.steps,
            "llmMs": self.llm_ms,
            "toolMs": self.tool_ms,
            "firstTokenMs": if self.ttft_n > 0 { self.ttft_sum / self.ttft_n as i64 } else { 0 },
            "tokensPerSecond": tps_of(self.decode_ms, self.llm_ms, self.output_tokens),
            "cacheHitPercent": cache_hit,
            "inputTokens": self.input_tokens,
            "outputTokens": self.output_tokens,
            "uncachedInputTokens": self.input_tokens
                .saturating_sub(self.cached_tokens + self.cache_write_tokens),
            "cacheReadTokens": self.cached_tokens,
            "cacheWriteTokens": self.cache_write_tokens,
            "reasoningTokens": self.reasoning_tokens,
            "contextUsed": self.context_used,
            "contextWindow": context_window,
            "contextBreakdown": {
                "systemTokens": breakdown.system_tokens,
                "toolsTokens": breakdown.tools_tokens,
                "messageTokens": breakdown.message_tokens,
            },
        })
    }

    /// 最近完成轮桶(live 推送帧的 `stats.lastTurn`)
    pub fn last_turn_json(&self, provider_id: &str) -> Option<Value> {
        self.last_turn.as_ref().map(|t| t.to_json(provider_id))
    }

    /// 全量形态:基础输出 + `turnList`(全部完成轮桶;仅 session_stats
    /// RPC 用。键名避开已有的计数键 `turns`)
    pub fn to_json_full(
        &self,
        breakdown: Breakdown,
        context_window: u64,
        provider_id: &str,
    ) -> Value {
        let mut v = self.to_json(breakdown, context_window);
        v["turnList"] = Value::Array(
            self.completed_turns
                .iter()
                .map(|t| t.to_json(provider_id))
                .collect(),
        );
        v
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
                "model": "deepseek-chat",
                "durationMs": 1000,
                "usage": {
                    "input_tokens": prompt,
                    "output_tokens": completion,
                    "cached_tokens": cached,
                    "ttftMs": 200,
                },
            },
        })
    }

    #[test]
    fn agg_counts_and_derived_ratios() {
        let mut a = StatsAgg::new();
        a.apply("turn/start", 0, &json!({}));
        a.apply("step/start", 0, &json!({}));
        a.apply("audit/call", 0, &llm_done(10_000, 500, 5_000));
        a.apply(
            "audit/call",
            0,
            &json!({
                "boundary": "tool", "detail": { "durationMs": 300 }
            }),
        );
        let v = a.to_json(Breakdown::default(), CONTEXT_WINDOW);
        assert_eq!(v["turns"], 1);
        assert_eq!(v["steps"], 1);
        assert_eq!(v["llmMs"], 1000);
        assert_eq!(v["toolMs"], 300);
        assert_eq!(v["firstTokenMs"], 200);
        // 解码口径:500 tok / (1000-200)ms = 625 tok/s
        assert_eq!(v["tokensPerSecond"], 625);
        assert_eq!(v["cacheHitPercent"], 50);
        assert_eq!(v["contextUsed"], 10_000);
    }

    /// prompt 侧绝对值桶:未缓存输入 = 输入 − 缓存读 − 缓存写(saturating)
    #[test]
    fn token_buckets_expose_input_side() {
        let mut a = StatsAgg::new();
        a.apply(
            "audit/call",
            0,
            &json!({
                "boundary": "llm", "operation": "request-done",
                "detail": { "durationMs": 1000, "usage": {
                    "input_tokens": 10_000,
                    "output_tokens": 400,
                    "cached_tokens": 6_000,
                    "cache_write_tokens": 2_500,
                    "reasoning_tokens": 150,
                }},
            }),
        );
        let v = a.to_json(Breakdown::default(), CONTEXT_WINDOW);
        assert_eq!(v["cacheReadTokens"], 6_000);
        assert_eq!(v["cacheWriteTokens"], 2_500);
        assert_eq!(v["uncachedInputTokens"], 1_500);
        assert_eq!(v["reasoningTokens"], 150);
    }

    /// 无 TTFT 样本(非流式/异常)时 TPS 回落总输出/总请求时长
    #[test]
    fn decode_speed_fallback_without_ttft() {
        let mut a = StatsAgg::new();
        a.apply(
            "audit/call",
            0,
            &json!({
                "boundary": "llm", "operation": "request-done",
                "detail": { "durationMs": 2000, "usage": {
                    "input_tokens": 100, "output_tokens": 600,
                }},
            }),
        );
        let v = a.to_json(Breakdown::default(), CONTEXT_WINDOW);
        assert_eq!(v["tokensPerSecond"], 300);
    }

    /// 轮桶生命周期:turn/start 开桶 → 请求/工具累计 → turn/end 收桶;
    /// models 去重保序;TTFT 取轮内最小;下一轮重新开桶
    #[test]
    fn turn_bucket_lifecycle() {
        let mut a = StatsAgg::new();
        a.apply("turn/start", 1_000, &json!({}));
        a.apply(
            "audit/call",
            0,
            &json!({
                "boundary": "llm", "operation": "request-done",
                "detail": { "model": "m1", "durationMs": 1000, "usage": {
                    "input_tokens": 100, "output_tokens": 50, "ttftMs": 300,
                }},
            }),
        );
        a.apply(
            "audit/call",
            0,
            &json!({
                "boundary": "llm", "operation": "request-done",
                "detail": { "model": "m2", "durationMs": 2000, "usage": {
                    "input_tokens": 200, "output_tokens": 80, "ttftMs": 150,
                }},
            }),
        );
        a.apply(
            "audit/call",
            0,
            &json!({ "boundary": "tool", "detail": { "durationMs": 700 } }),
        );
        a.apply("turn/end", 9_000, &json!({}));
        let lt = a.last_turn_json("deepseek-official").expect("收桶");
        assert_eq!(lt["turn"], 1);
        assert_eq!(lt["runMs"], 8_000);
        assert_eq!(lt["llmMs"], 3_000);
        assert_eq!(lt["toolMs"], 700);
        assert_eq!(lt["ttftMs"], 150);
        assert_eq!(lt["outputTokens"], 130);
        assert_eq!(
            lt["routes"],
            json!(["deepseek-official/m1", "deepseek-official/m2"])
        );
        // 下一轮重新开桶(轮号递增),收桶后覆盖 lastTurn
        a.apply("turn/start", 10_000, &json!({}));
        a.apply("turn/end", 11_000, &json!({}));
        let lt2 = a.last_turn_json("p").expect("第二轮收桶");
        assert_eq!(lt2["turn"], 2);
        assert_eq!(lt2["outputTokens"], 0);
    }

    /// 无 LLM 请求的轮(如钩子拒绝即收)也收桶:零值, routes 为空
    #[test]
    fn empty_turn_bucket_still_closes() {
        let mut a = StatsAgg::new();
        a.apply("turn/start", 5_000, &json!({}));
        a.apply("turn/end", 5_400, &json!({}));
        let lt = a.last_turn_json("p").expect("空轮也收桶");
        assert_eq!(lt["runMs"], 400);
        assert_eq!(lt["outputTokens"], 0);
        assert_eq!(lt["routes"], json!([]));
    }

    /// 留存开关:live 只留最近轮;冷读 turnList 全留
    #[test]
    fn retain_switch_separates_live_and_cold() {
        let mut live = StatsAgg::new();
        let mut cold = StatsAgg::with_retained_turns();
        for _ in 0..3 {
            let ev = json!({});
            live.apply("turn/start", 0, &ev);
            live.apply("turn/end", 1_000, &ev);
            cold.apply("turn/start", 0, &ev);
            cold.apply("turn/end", 1_000, &ev);
        }
        let base = Breakdown::default();
        assert!(
            live.to_json_full(base, 0, "p")["turnList"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            cold.to_json_full(base, 0, "p")["turnList"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        // 计数键 turns 与轮桶数组不同名,互不覆盖
        assert_eq!(cold.to_json_full(base, 0, "p")["turns"], 3);
    }

    /// turn/end 无开桶(日志截尾等)安全忽略
    #[test]
    fn turn_end_without_open_bucket_ignored() {
        let mut a = StatsAgg::new();
        a.apply("turn/end", 1_000, &json!({}));
        assert_eq!(a.last_turn_json("p"), None);
        assert_eq!(a.turns, 0);
    }

    /// 上下文占用 = 最新采样覆盖(折叠/换模型后回落,非峰值)
    #[test]
    fn context_used_is_latest_sample() {
        let mut a = StatsAgg::new();
        a.apply("audit/call", 0, &llm_done(900_000, 1, 0));
        a.apply("audit/call", 0, &llm_done(12_000, 1, 0));
        assert_eq!(
            a.to_json(Breakdown::default(), CONTEXT_WINDOW)["contextUsed"],
            12_000
        );
        // prompt=0 的响应(异常/空)不覆盖
        a.apply("audit/call", 0, &llm_done(0, 1, 0));
        assert_eq!(
            a.to_json(Breakdown::default(), CONTEXT_WINDOW)["contextUsed"],
            12_000
        );
    }

    /// 窗口由宿主按会话模型注入(per-model),序列化原样带出、不硬编码
    #[test]
    fn context_window_is_parameterized() {
        let a = StatsAgg::new();
        assert_eq!(
            a.to_json(Breakdown::default(), 128_000)["contextWindow"],
            128_000
        );
        assert_eq!(
            a.to_json(Breakdown::default(), CONTEXT_WINDOW)["contextWindow"],
            1_000_000
        );
    }

    /// 归一形只认规范键;非规范键(如 raw wire 形)计零不误报
    #[test]
    fn nested_cached_tokens_counted() {
        let mut a = StatsAgg::new();
        a.apply(
            "audit/call",
            0,
            &json!({
                "boundary": "llm", "operation": "request-done",
                "detail": { "durationMs": 1000, "usage": {
                    "input_tokens": 10_000,
                    "output_tokens": 100,
                    "cached_tokens": 8_000,
                }},
            }),
        );
        let v = a.to_json(Breakdown::default(), CONTEXT_WINDOW);
        assert_eq!(v["cacheHitPercent"], 80);
    }

    /// 非统计事件 no-op + 推送判定(turn/end 在列 = 轮桶收口即推)
    #[test]
    fn non_stats_events_ignored() {
        let mut a = StatsAgg::new();
        a.apply("assistant/chunk", 0, &json!({ "chunk": {} }));
        a.apply("user/message", 0, &json!({ "content": "x" }));
        assert_eq!(a, StatsAgg::new());
        assert!(!StatsAgg::is_stats_event("assistant/chunk"));
        assert!(StatsAgg::is_stats_event("turn/start"));
        assert!(StatsAgg::is_stats_event("turn/end"));
        assert!(StatsAgg::is_stats_event("audit/call"));
    }
}
