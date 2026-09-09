//! compaction e2e:超预算触发历史折叠。
//!
//! 断言链:
//! - audit/call(operation=compaction)先落、compaction/summary 后落(记录优先);
//! - 折叠后出网请求 = 摘要消息 + 保留尾部(闸门全程通过——期望侧同一投影);
//! - 重放不重调:新 engine 共享日志再跑 turn,无新 summarize 调用、
//!   摘要消息仍可见(确定性重放);
//! - tool/result 裁剪:超阈值输出在请求中被截断而日志保留全文。

use std::sync::{Arc, Mutex};

use dsh_agent_loop::{LlmEvent, LoopEngine, NoTools, RequestHeader};
use dsh_llm::{FakeProvider, InvariantGate};
use dsh_session::{EventEnvelope, EventLog};
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
    // 预算压到 4000 字符:首 step 必触发折叠(历史 ~6600+)
    engine.set_fold_budget(4000);

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
    assert!(s.contains("<session-summary>"), "请求首条为摘要消息");
    assert!(s.contains("condensed history"));
    assert!(s.chars().count() < 3000, "折叠后请求应远小于全历史");

    // 重放不重调:新 engine(同日志,预算放大避免新折叠),摘要消息
    // 必须来自日志记录而非重新 summarize
    let mut engine2 = LoopEngine::new(header(), Arc::clone(&log));
    engine2.set_fold_budget(1_000_000);
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
