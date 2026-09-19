//! 上下文压缩策略(纯函数层)。
//!
//! 照源 `packages/compaction` 包族语义:压力阈值与保留尾按上下文窗口的
//! token 预算计(threshold=0.8×窗口,retain=0.16×窗口);压缩范围 = 连续
//! 头部区间,切点回退到 tool 配对平衡处(永不拆散 assistant tool_calls
//! 与其 tool/result);摘要指令以最终 user 消息追加在逐字前缀之后。
//! 持久化词汇(`compaction/summary` 事件)与折叠包装(派生面)归
//! `liuma-session`;本 crate 只做决策与指令构造,不做 IO、不调模型。

#![deny(missing_docs)]

use liuma_session::EventEnvelope;

/// 上下文窗口缺省值(未配置 per-model 窗口时;照源 DEFAULT_CONTEXT_WINDOW)
pub const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;
/// 压力阈值占比:上下文 ≥ 窗口×此值时自动折叠(源 thresholdRatio)
pub const THRESHOLD_RATIO: f64 = 0.8;
/// 保留尾占比:最近窗口×此值的上下文逐字保留(源 retainRatio)
pub const RETAIN_RATIO: f64 = 0.16;
/// 无真实 usage 时的 token 估算启发式(中文文本 ≈ 4 字符/token)
pub const CHARS_PER_TOKEN: u64 = 4;

/// 压力阈值 token 数(自动折叠触发线;`window` = 当前模型上下文窗口)
pub fn threshold_tokens(window: u64) -> u64 {
    (window as f64 * THRESHOLD_RATIO) as u64
}

/// 保留尾预算 token 数(最近上下文逐字保留的下限;`window` 同上)
pub fn retain_tokens(window: u64) -> u64 {
    (window as f64 * RETAIN_RATIO) as u64
}

/// 当前上下文量测:优先最近一次 LLM 请求的真实 usage
/// (audit/call boundary=llm operation=request-done 的归一 input_tokens),
/// 无则退化为派生字符数÷4 启发式。
pub fn measure_tokens(events: &[EventEnvelope], derived_chars: u64) -> u64 {
    events
        .iter()
        .rev()
        .find(|e| {
            e.r#type == "audit/call"
                && e.data["boundary"] == "llm"
                && e.data["operation"] == "request-done"
        })
        .and_then(|e| e.data["detail"]["usage"]["input_tokens"].as_u64())
        .filter(|t| *t > 0)
        .unwrap_or(derived_chars / CHARS_PER_TOKEN)
}

/// 选出的压缩区间(模型可见面连续头部)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactRange {
    /// 折叠终点:此 seq(含)之前的消息面事件全部进摘要
    pub through_seq: u64,
    /// 折叠遮蔽区间的首条消息事件 seq(退化 = through_seq)
    pub shadowed_start: u64,
    /// 本次折叠的 live 消息条数(上次 checkpoint 之后的消息面事件)
    pub fold_len: usize,
    /// 待摘要前缀在派生面(可见消息数组)里的长度——engine 按它切片。
    /// `prev_through > 0` 时派生面头部多一条旧 checkpoint 占位,故
    /// `prefix_len = fold_len + 1`:前缀末条 == `through_seq` 指向的
    /// 消息;若切 `fold_len` 则丢尾条(旧实现在二次压缩时即此缺陷)。
    pub prefix_len: usize,
    /// 折叠前缀的估算 token(字符÷4;UI 通告与日志载荷用)
    pub estimated_tokens: u64,
}

/// 消息面事件的 tool 配对增量:assistant/message 带 tool_calls 计 +
/// N,tool/result 计 −1,其余 0(照源 tool-pairing eventDelta)。
fn pairing_delta(ty: &str, data: &serde_json::Value) -> i64 {
    match ty {
        "assistant/message" => data["tool_calls"].as_array().map_or(0, |a| a.len() as i64),
        "tool/result" => -1,
        _ => 0,
    }
}

/// 消息体 token 估算(字符÷4)
fn estimate_tokens(ty: &str, data: &serde_json::Value) -> u64 {
    liuma_session::message_from_event(ty, data)
        .map(|m| m.to_string().chars().count() as u64)
        .unwrap_or(0)
        / CHARS_PER_TOKEN
}

/// 选段:在「最近一次折叠之后」的消息面事件上,自尾倒序累计估算
/// token(字符÷4)到 `retain` 得保留尾,再把切点向回走到 tool 配对
/// 平衡处(切点前的调用全部已有结果)。保留尾不足以离开区间头、或
/// 回退后无平衡切点 → None(无可压缩)。
///
/// 返回的 [`CompactRange::prefix_len`] 是派生面切片的权威长度
/// (含旧 checkpoint 占位);[`CompactRange::fold_len`] 只计本次折叠
/// 的 live 消息数(UI 统计)。
pub fn select_range(events: &[EventEnvelope], retain: u64) -> Option<CompactRange> {
    let prev_through = events
        .iter()
        .rev()
        .find(|e| e.r#type == "compaction/summary")
        .and_then(|e| e.data["throughSeq"].as_u64())
        .unwrap_or(0);
    let live: Vec<&EventEnvelope> = events.iter().filter(|e| e.seq > prev_through).collect();

    // 消息面事件(与派生面同一判定谓词)+ 逐切点配对平衡
    let mut msg_idx: Vec<usize> = Vec::new();
    let mut msg_tokens: Vec<u64> = Vec::new();
    let mut balanced: Vec<bool> = Vec::new();
    let mut in_progress: i64 = 0;
    for ev in &live {
        if liuma_session::message_from_event(&ev.r#type, &ev.data).is_some() {
            msg_idx.push(balanced.len());
            msg_tokens.push(estimate_tokens(&ev.r#type, &ev.data));
        }
        in_progress += pairing_delta(&ev.r#type, &ev.data);
        balanced.push(in_progress == 0);
    }
    if msg_idx.is_empty() {
        return None;
    }

    // 自尾倒序累计消息 token 到保留预算 → 保留尾起点(消息序下标)
    let mut accumulated: u64 = 0;
    let mut keep_from = 0usize;
    for (k, &t) in msg_tokens.iter().enumerate().rev() {
        accumulated += t;
        keep_from = k;
        if accumulated >= retain {
            break;
        }
    }
    if keep_from == 0 {
        return None;
    }

    // 切点 = 保留尾起点前;回退到配对平衡处(该切点前无未回应用的调用)
    let mut cut = keep_from;
    while cut > 0 && !balanced[msg_idx[cut - 1]] {
        cut -= 1;
    }
    if cut == 0 {
        return None;
    }

    let through_seq = live[msg_idx[cut - 1]].seq;
    let shadowed_start = live[msg_idx[0]].seq;
    let estimated_tokens = msg_tokens[..cut].iter().sum();
    // 派生面头部占位:上次 checkpoint 以合成 user 消息插入头(见
    // liuma_session::derive_visible_messages),摘要前缀须含它——
    // 指令要求「已有 <compacted-summary> 是旧 checkpoint,合并而非丢弃」
    let prior_checkpoint = usize::from(prev_through > 0);
    Some(CompactRange {
        through_seq,
        shadowed_start,
        fold_len: cut,
        prefix_len: cut + prior_checkpoint,
        estimated_tokens,
    })
}

/// 摘要指令(照源 COMPACTION_INSTRUCTION 逐字):以最终 user 消息追加
/// 在逐字前缀之后——前缀复用上次路由请求的 system/tools/消息形态,
/// 命中 provider KV cache。
pub const COMPACTION_INSTRUCTION: &str = r#"You are now acting as a compaction engine for this AI coding assistant. Condense the conversation ABOVE into a structured checkpoint that lets another model resume the work with no loss of essential context.

Output EXACTLY the Markdown structure below: keep every section, in order. Use terse bullets, not prose paragraphs. Write "(none)" for an empty section — never drop a section.

## Primary Request and Intent
- [the user's original and evolving goals; quote verbatim where the exact wording matters]

## Key Technical Concepts
- [technologies, frameworks, patterns, and conventions in play]

## Files and Code
- [exact path: why it matters, key changes or snippets]

## Errors and Fixes
- [error: how it was resolved, plus any related user feedback]

## Pending Jobs
- [explicitly requested work not yet completed]

## Current Work
- [precisely what was in progress at this checkpoint]

## Next Step
- [the single next action, directly in line with the most recent request, or "(none)"]

## Critical Context
- [decisions and their rationale, constraints, user preferences, open questions, data needed to continue]

Rules:
- Write concise English engineering prose. Preserve exact file paths, commands, error strings, identifiers, numeric values, function signatures, and syntax fragments.
- Capture user feedback and explicit instructions faithfully, especially corrections.
- Do NOT mention this summarization request or that the context was compacted.
- Output only the checkpoint text: do not call any tool or take any other action.
- If the conversation already contains a <compacted-summary> block, it is a PRIOR checkpoint. Do not copy it forward verbatim: preserve still-true facts, drop stale ones, and merge newer information into a single consolidated summary under the same structure."#;

/// 构造摘要请求的完整输入:逐字前缀 + 追加含指令的最终 user 消息。
pub fn summarization_messages(fold_messages: &serde_json::Value) -> serde_json::Value {
    let mut msgs = fold_messages.as_array().cloned().unwrap_or_default();
    msgs.push(serde_json::json!({
        "role": "user",
        "content": COMPACTION_INSTRUCTION,
    }));
    serde_json::Value::Array(msgs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use liuma_session::{EventLog, derive_visible_messages, message_from_event};

    /// 造带 seq 的事件:经 EventLog append 分配连续 seq
    fn logged(events: &[(&str, serde_json::Value)]) -> Vec<EventEnvelope> {
        let mut log = EventLog::new();
        for (ty, data) in events {
            log.append(EventEnvelope::new(ty, 0, data.clone()))
                .expect("append");
        }
        log.iter().cloned().collect()
    }

    fn user_msg(text: &str) -> (&'static str, serde_json::Value) {
        ("user/message", serde_json::json!({ "content": text }))
    }

    fn assistant_msg(text: &str) -> (&'static str, serde_json::Value) {
        ("assistant/message", serde_json::json!({ "content": text }))
    }

    fn big_user(kchars: usize) -> (&'static str, serde_json::Value) {
        user_msg(&"x".repeat(kchars * 1000))
    }

    #[test]
    fn empty_log_selects_nothing() {
        let all = logged(&[]);
        assert_eq!(select_range(&all, 1), None);
    }

    #[test]
    fn below_retain_selects_nothing() {
        // 全部消息估算 token < retain(160K)→ 无可压缩(源同款:小会话 no-op)
        let all = logged(&[user_msg("hi"), assistant_msg("hello")]);
        assert_eq!(
            select_range(&all, retain_tokens(DEFAULT_CONTEXT_WINDOW)),
            None
        );
    }

    #[test]
    fn big_history_selects_head_prefix() {
        // 自尾累计到 retain(100K):最后一条大消息(≈125K)即越过 →
        // 保留尾 = 最后 2 条,前 4 条折叠
        let all = logged(&[
            big_user(500), // ≈125k tok
            assistant_msg("a"),
            big_user(500), // ≈125k tok
            assistant_msg("b"),
            big_user(500), // ≈125k tok
            assistant_msg("c"),
        ]);
        let r = select_range(&all, 100_000).expect("range");
        assert_eq!(r.fold_len, 4);
        assert_eq!(r.prefix_len, r.fold_len, "无旧 checkpoint → 切点即前缀长");
        assert_eq!(r.through_seq, 4, "折叠到第 4 条消息(assistant b)");
        assert_eq!(r.shadowed_start, 1);
        assert!(r.estimated_tokens > 0);
    }

    #[test]
    fn cut_walks_back_to_tool_pairing_balance() {
        // 保留尾起点落在 tool/result 上(其估算 token 越过 retain)→
        // 其前切点在 assistant 调用上不平衡 → 回退到配对平衡处
        let all = logged(&[
            big_user(500), // seq1 ≈125k tok
            (
                "assistant/message",
                serde_json::json!({ "content": "", "tool_calls": [ { "id": "t1" } ] }),
            ), // seq2
            (
                "tool/result",
                serde_json::json!({ "output": "ok" .repeat(500_000 / 2) }),
            ), // seq3 ≈125k tok
            big_user(500), // seq4 ≈125k tok
            assistant_msg("done"), // seq5
        ]);
        // 自尾累计:seq4(125K)<200K → seq3(125K)越过多 retain(200K)
        // → 保留尾起点 = seq3;切点回退:seq2 后余 +1 不平衡 → seq1 后平衡
        let r = select_range(&all, 200_000).expect("range");
        assert_eq!(r.fold_len, 1);
        assert_eq!(r.through_seq, 1);
        assert_eq!(r.shadowed_start, 1);
    }

    #[test]
    fn only_after_last_summary_is_considered() {
        // 已有 compaction/summary:只统计 throughSeq 之后的消息面
        let all = logged(&[
            big_user(500),
            (
                "compaction/summary",
                serde_json::json!({ "summary": "s", "throughSeq": 1 }),
            ),
            assistant_msg("after fold"),
        ]);
        // throughSeq 之后仅 1 条小消息 → 无可压缩
        assert_eq!(
            select_range(&all, retain_tokens(DEFAULT_CONTEXT_WINDOW)),
            None
        );
    }

    /// 回归锁:二次压缩的前缀长度须含派生面头部的旧 checkpoint 占位,
    /// 且前缀末条 == through_seq 指向的消息(engine 按 prefix_len 切片;
    /// 旧实现切 fold_len,二次压缩丢尾条——tool 配对可被拆散)
    #[test]
    fn prefix_len_accounts_for_prior_checkpoint() {
        let all = logged(&[
            big_user(500), // seq1
            (
                "compaction/summary",
                serde_json::json!({ "summary": "prior", "throughSeq": 1 }),
            ),
            big_user(500), // seq3
            assistant_msg("a"),
            big_user(500), // seq5
            assistant_msg("b"),
            big_user(500), // seq7
            assistant_msg("c"),
        ]);
        let r = select_range(&all, 100_000).expect("range");
        assert_eq!(r.prefix_len, r.fold_len + 1, "含头部旧 checkpoint 占位");

        let visible = derive_visible_messages(all.iter());
        let arr = visible.as_array().expect("array");
        let through = all
            .iter()
            .find(|e| e.seq == r.through_seq)
            .expect("through 事件");
        let expected = message_from_event(&through.r#type, &through.data).expect("消息面");
        assert_eq!(
            arr[r.prefix_len - 1],
            expected,
            "前缀末条 = through_seq 指向的消息(切片对齐)"
        );
        assert_ne!(
            arr[r.fold_len - 1],
            expected,
            "切 fold_len 会漏掉尾条(旧缺陷位)"
        );
    }

    #[test]
    fn measure_prefers_real_usage_then_chars_heuristic() {
        let plain = logged(&[user_msg("hi")]);
        assert_eq!(measure_tokens(&plain, 800), 200, "无 usage → 字符÷4");

        let with_usage = logged(&[
            user_msg("hi"),
            (
                "audit/call",
                serde_json::json!({ "boundary": "llm", "operation": "request-done",
                    "detail": { "usage": { "input_tokens": 12345 } } }),
            ),
        ]);
        assert_eq!(measure_tokens(&with_usage, 800), 12345);
    }

    #[test]
    fn summarization_appends_instruction_as_final_user_message() {
        let msgs = serde_json::json!([
            { "role": "user", "content": "q" },
            { "role": "assistant", "content": "a" },
        ]);
        let out = summarization_messages(&msgs);
        let arr = out.as_array().expect("array");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[2]["role"], "user");
        assert_eq!(arr[2]["content"], COMPACTION_INSTRUCTION);
        // 指令内含结构化八节标题(照源逐字的锚)
        for section in [
            "## Primary Request and Intent",
            "## Files and Code",
            "## Next Step",
            "## Critical Context",
        ] {
            assert!(COMPACTION_INSTRUCTION.contains(section), "{section} 缺席");
        }
    }

    /// 阈值随窗口线性缩放(默认 1M 与 128K 两档锚点)
    #[test]
    fn thresholds_follow_source_ratios() {
        assert_eq!(threshold_tokens(DEFAULT_CONTEXT_WINDOW), 800_000);
        assert_eq!(retain_tokens(DEFAULT_CONTEXT_WINDOW), 160_000);
        // 128K 窗口:阈值 102_400 / 保留尾 20_480(旧硬编码 1M 会晚触发 8 倍)
        assert_eq!(threshold_tokens(128_000), 102_400);
        assert_eq!(retain_tokens(128_000), 20_480);
    }

    #[test]
    fn message_predicate_matches_derive() {
        // 选段用的消息判定与派生面同一函数
        let all = logged(&[user_msg("u"), ("tool/call", serde_json::json!({}))]);
        assert!(message_from_event("user/message", &all[0].data).is_some());
        assert!(message_from_event("tool/call", &all[1].data).is_none());
    }
}
