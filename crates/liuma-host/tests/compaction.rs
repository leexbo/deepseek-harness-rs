//! compaction e2e:超预算触发历史折叠。
//!
//! 断言链:
//! - audit/call(operation=compaction)先落、compaction/summary 后落(记录优先);
//! - 折叠后出网请求 = 摘要消息 + 保留尾部(闸门全程通过——期望侧同一投影);
//! - 重放不重调:新 engine 共享日志再跑 turn,无新 summarize 调用、
//!   摘要消息仍可见(确定性重放);
//! - tool/result 裁剪:超阈值输出在请求中被截断而日志保留全文。

use std::sync::{Arc, Mutex};

use liuma_agent_loop::{LlmEvent, LoopEngine, NoTools, RequestHeader};
use liuma_llm::{FakeProvider, InvariantGate};
use liuma_session::{EventEnvelope, EventLog};
use serde_json::json;

fn header() -> RequestHeader {
    RequestHeader {
        model: "t".into(),
        system: String::new(),
        temperature: 0.0,
        reasoning_effort: None,
        tools: Vec::new(),
    }
}

/// 预置一个大历史会话日志(10 组超长工具往返)
fn big_log() -> Arc<Mutex<EventLog>> {
    let log = Arc::new(Mutex::new(EventLog::new()));
    {
        let mut l = log.lock().unwrap();
        for i in 0..10 {
            l.append(EventEnvelope::new(
                "user/message",
                0,
                json!({ "content": format!("question {i}: {}", "q".repeat(600)) }),
            ))
            .unwrap();
            l.append(EventEnvelope::new(
                "assistant/message",
                0,
                json!({ "content": format!("answer {i}") }),
            ))
            .unwrap();
        }
    }
    log
}

#[tokio::test]
async fn fold_triggers_records_and_replays_without_recall() {
    let log = big_log();
    let mut engine = LoopEngine::new(header(), Arc::clone(&log));
    // 阈值压到 1 token:首 step 必触发折叠(保留尾也压到 1)
    engine.set_fold_thresholds(1, 1);

    let mut provider = FakeProvider::new();
    provider.summaries.push("condensed history".into());
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "ok"
    }))]);
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let mut sink = |_ev: &EventEnvelope| {};

    engine
        .run_turn(
            "continue",
            None,
            &[],
            &[],
            &[],
            &mut gate,
            &mut NoTools,
            &|| 0_i64,
            &mut sink,
        )
        .await
        .expect("turn");

    // 记录优先:摘要审计先于 compaction/summary 落档
    let (audit_seq, summary_seq, summary_text, through) = {
        let l = log.lock().unwrap();
        let audit = l
            .iter()
            .find(|e| e.r#type == "audit/call" && e.data["operation"] == "compaction")
            .expect("compaction 审计");
        let summary = l
            .iter()
            .find(|e| e.r#type == "compaction/summary")
            .expect("compaction/summary");
        (
            audit.seq,
            summary.seq,
            summary.data["summary"].clone(),
            summary.data["throughSeq"].as_u64().unwrap_or(0),
        )
    };
    assert!(audit_seq < summary_seq, "审计先于折叠事件落档");
    assert_eq!(summary_text, "condensed history");
    assert!(through > 0);

    // 出网请求:含摘要消息,且长度显著小于全历史(闸门比对已通过)
    let (_, messages) = &gate.inner().received[0];
    let s = messages.to_string();
    assert!(s.contains("<compacted-summary>"), "请求首条为摘要消息");
    assert!(s.contains("condensed history"));
    assert!(s.chars().count() < 3000, "折叠后请求应远小于全历史");

    // 重放不重调:新 engine(同日志,预算放大避免新折叠),摘要消息
    // 必须来自日志记录而非重新 summarize
    let mut engine2 = LoopEngine::new(header(), Arc::clone(&log));
    engine2.set_fold_thresholds(u64::MAX, u64::MAX);
    let mut provider2 = FakeProvider::new();
    provider2.summaries.push("SHOULD-NOT-BE-CALLED".into());
    provider2.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "ok2"
    }))]);
    let mut gate2 = InvariantGate::new(provider2, Arc::clone(&log));
    let mut sink2 = |_ev: &EventEnvelope| {};
    engine2
        .run_turn(
            "again",
            None,
            &[],
            &[],
            &[],
            &mut gate2,
            &mut NoTools,
            &|| 0_i64,
            &mut sink2,
        )
        .await
        .expect("replay turn");
    assert_eq!(
        gate2.inner().summaries.len(),
        1,
        "已折叠历史不得重调摘要(读记录;脚本应原封未动)"
    );
    assert_eq!(gate2.inner().summaries[0], "SHOULD-NOT-BE-CALLED");
    let (_, m2) = &gate2.inner().received[0];
    assert!(m2.to_string().contains("condensed history"));
}

#[tokio::test]
async fn oversized_tool_output_pruned_in_request_kept_in_log() {
    let log = Arc::new(Mutex::new(EventLog::new()));
    {
        let mut l = log.lock().unwrap();
        l.append(EventEnvelope::new(
            "user/message",
            0,
            json!({ "content": "run it" }),
        ))
        .unwrap();
        l.append(EventEnvelope::new(
            "assistant/message",
            0,
            json!({ "content": "", "tool_calls": [ { "name": "bash", "arguments": {} } ] }),
        ))
        .unwrap();
        l.append(EventEnvelope::new(
            "tool/result",
            0,
            json!({ "call": 3, "id": "", "output": "o".repeat(20_000), "success": true }),
        ))
        .unwrap();
    }

    let mut provider = FakeProvider::new();
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "done"
    }))]);
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let mut engine = LoopEngine::new(header(), Arc::clone(&log));
    let mut sink = |_ev: &EventEnvelope| {};
    engine
        .run_turn(
            "next",
            None,
            &[],
            &[],
            &[],
            &mut gate,
            &mut NoTools,
            &|| 0_i64,
            &mut sink,
        )
        .await
        .expect("turn");

    // 请求侧:裁剪(闸门期望侧同一投影,比对通过即证明一致)
    let (_, messages) = &gate.inner().received[0];
    let s = messages.to_string();
    assert!(s.contains("[pruned "));
    assert!(s.chars().count() < 12_000);
    // 日志侧:全文保留(审计保真)
    let l = log.lock().unwrap();
    let tr = l
        .iter()
        .find(|e| e.r#type == "tool/result")
        .expect("tool/result");
    assert_eq!(tr.data["output"].as_str().unwrap().len(), 20_000);
}

/// 手动压缩(/compact 引擎面):无压力阈值门槛即压;summary 落档带
/// 统计载荷;二调无可压缩且不重调 summarize(重放确定性)。
#[tokio::test]
async fn manual_compact_forces_summary_and_replays_without_recall() {
    use liuma_agent_loop::FoldOutcome;

    let log = big_log();
    let mut engine = LoopEngine::new(header(), Arc::clone(&log));
    // 手动路径无压力阈值门槛(compact_now 不看 threshold),但保留尾
    // 预算取引擎 retain——压到 1 使小会话也能压出前缀
    engine.set_fold_thresholds(0, 1);

    let mut provider = FakeProvider::new();
    provider.summaries.push("manual condensed".into());
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let mut sink = |_ev: &EventEnvelope| {};

    let outcome = engine
        .compact_now(&mut gate, &|| 0_i64, &mut sink)
        .await
        .expect("compact");
    let FoldOutcome::Folded { seq, items, tokens } = outcome else {
        panic!("大会话手动压缩应折叠落档");
    };
    assert!(seq > 0);
    assert!(items > 0);
    assert!(tokens > 0);

    // 落档载荷:summary + 统计(UI 标记行素材)
    {
        let l = log.lock().unwrap();
        let summary = l
            .iter()
            .find(|e| e.r#type == "compaction/summary")
            .expect("compaction/summary");
        assert_eq!(summary.data["summary"], "manual condensed");
        assert_eq!(summary.data["items"], items);
        assert_eq!(summary.data["shadowedTokens"], tokens);
        // 派生面:摘要以 checkpoint 包装进请求前缀
        let visible = liuma_session::derive_visible_messages(l.iter());
        let first = &visible.as_array().unwrap()[0];
        assert!(
            first["content"]
                .as_str()
                .unwrap()
                .contains("This is an automatically generated checkpoint")
                && first["content"]
                    .as_str()
                    .unwrap()
                    .contains("<compacted-summary>\nmanual condensed"),
            "首条消息 = checkpoint 包装的摘要"
        );
    }

    // 二调:保留尾不足 retain → Skipped,且 summaries 已空(未重调;
    // 若重调,FakeProvider 会回退 "[fake summary]" 亦无从进入此分支)
    let outcome2 = engine
        .compact_now(&mut gate, &|| 0_i64, &mut sink)
        .await
        .expect("compact 2");
    assert_eq!(outcome2, FoldOutcome::Skipped);
    let l = log.lock().unwrap();
    assert_eq!(
        l.iter()
            .filter(|e| e.r#type == "compaction/summary")
            .count(),
        1,
        "二调不追加 summary(重放确定性)"
    );
}

/// 回归锁:同一会话第二次压缩——摘要前缀必须覆盖 `throughSeq` 指向的
/// 尾条消息,且 tool 往返完整。旧实现切派生面 `arr[..fold_len]`,而
/// 派生面头部多一条旧 checkpoint 占位,于是前缀漏掉尾条:本例尾条是
/// tool/result,漏掉后摘要请求以 assistant tool_calls 悬空收尾(provider
/// 直接拒)。锁定 `liuma_compaction::CompactRange::prefix_len` 的切片语义。
#[tokio::test]
async fn second_compaction_prefix_covers_through_seq_message() {
    use liuma_agent_loop::FoldOutcome;

    let log = Arc::new(Mutex::new(EventLog::new()));
    {
        let mut l = log.lock().unwrap();
        let mut push = |ty: &str, data: serde_json::Value| {
            l.append(EventEnvelope::new(ty, 0, data)).unwrap();
        };
        push(
            "user/message",
            json!({ "content": format!("q1: {}", "x".repeat(4000)) }),
        );
        push("assistant/message", json!({ "content": "a1" }));
        push(
            "user/message",
            json!({ "content": format!("q2: {}", "x".repeat(4000)) }),
        );
        push("assistant/message", json!({ "content": "a2" }));
    }

    let mut engine = LoopEngine::new(header(), Arc::clone(&log));
    // 手动路径无压力门槛;保留尾压到 1 token → 每次折叠只留最后一条
    engine.set_fold_thresholds(0, 1);
    let mut provider = FakeProvider::new();
    provider.summaries.push("first".into());
    provider.summaries.push("second".into());
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let mut sink = |_ev: &EventEnvelope| {};

    let first = engine
        .compact_now(&mut gate, &|| 0_i64, &mut sink)
        .await
        .expect("first compact");
    assert!(matches!(first, FoldOutcome::Folded { .. }));

    // 追加一轮工具往返 + 尾部 user(q4 为保留尾)
    {
        let mut l = log.lock().unwrap();
        let mut push = |ty: &str, data: serde_json::Value| {
            l.append(EventEnvelope::new(ty, 0, data)).unwrap();
        };
        push(
            "user/message",
            json!({ "content": format!("q3: {}", "x".repeat(4000)) }),
        );
        push(
            "assistant/message",
            json!({ "content": "", "tool_calls": [ { "id": "t1", "name": "bash", "arguments": {} } ] }),
        );
        push(
            "tool/result",
            json!({ "call": 6, "id": "t1", "output": "r", "success": true }),
        );
        push(
            "user/message",
            json!({ "content": format!("q4: {}", "x".repeat(4000)) }),
        );
    }

    let second = engine
        .compact_now(&mut gate, &|| 0_i64, &mut sink)
        .await
        .expect("second compact");
    let FoldOutcome::Folded { items, .. } = second else {
        panic!("二次压缩应折叠(仍有可压前缀)");
    };
    assert_eq!(items, 4, "live 前缀 = a2/q3/assistant(tool_calls)/tool");

    let through_seq = {
        let l = log.lock().unwrap();
        l.iter()
            .rfind(|e| e.r#type == "compaction/summary")
            .expect("第二次 compaction/summary")
            .data["throughSeq"]
            .as_u64()
            .unwrap_or(0)
    };

    // 第二次摘要输入(逐字前缀)的末条 == throughSeq 指向的消息
    let inputs = &gate.inner().summary_inputs;
    assert_eq!(inputs.len(), 2, "两次压缩各一次 summarize");
    let prefix = inputs[1].1.as_array().expect("前缀数组");
    let expected = {
        let l = log.lock().unwrap();
        let ev = l
            .iter()
            .find(|e| e.seq == through_seq)
            .expect("throughSeq 事件");
        liuma_session::message_from_event(&ev.r#type, &ev.data).expect("消息面")
    };
    assert_eq!(
        prefix.last(),
        Some(&expected),
        "前缀末条 = throughSeq 指向的消息(旧实现切 fold_len 时缺此条)"
    );
    // 尾条是 tool/result → 其前的 assistant tool_calls 已配对,无悬空
    assert_eq!(prefix.last().unwrap()["role"], "tool");
    assert_eq!(prefix.last().unwrap()["id"], "t1");
    assert_eq!(prefix[prefix.len() - 2]["tool_calls"][0]["id"], "t1");
    // 头部占位 = 上一次 checkpoint(prefix_len 已把该偏移算入)
    assert!(
        prefix[0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("<compacted-summary>\nfirst"),
        "前缀头部 = 上次 checkpoint(合并语义)"
    );
}
