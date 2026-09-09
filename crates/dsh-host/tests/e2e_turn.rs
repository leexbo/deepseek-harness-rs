//! 端到端 turn 测试:engine → 闸门 → 假 provider → JSONL 落盘 → 重放。
//!
//! 出口门核心断言:
//! - 事件序列与 seq 连续(记录优先:每事件先落盘再进后续处理);
//! - provider 收到的 messages ≡ 日志派生(不变式的正向);
//! - 篡改请求被闸门拒绝(不变式的反向,出口标准「伪造请求被宿主拒」);
//! - JSONL 重放后派生结果与出网请求 bit-exact 相等。

use std::sync::{Arc, Mutex};

use dsh_agent_loop::{LlmEvent, LlmTransport, LoopEngine, Phase, RequestHeader};
use dsh_host::JsonlBackend;
use dsh_llm::{FakeProvider, InvariantGate};
use dsh_session::events::derive_messages;
use dsh_session::{EventEnvelope, EventLog};
use serde_json::json;

fn header() -> RequestHeader {
    RequestHeader {
        model: "test-model".into(),
        system: "you are a test".into(),
        temperature: 0.0,
        reasoning_effort: None,
        tools: Vec::new(),
    }
}

#[tokio::test]
async fn turn_e2e_record_first_and_replayable() {
    let dir = std::env::temp_dir().join(format!("dsh-e2e-turn-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jsonl = JsonlBackend::create(dir.join("turn.jsonl")).unwrap();

    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(header(), Arc::clone(&log));
    let mut provider = FakeProvider::new();
    provider.then(vec![
        LlmEvent::Chunk("Hel".into()),
        LlmEvent::Chunk("lo".into()),
        LlmEvent::AssistantMessage(json!({ "content": "Hello" })),
        LlmEvent::Done,
    ]);
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));

    let mut sink = |ev: &EventEnvelope| {
        jsonl.append(ev).unwrap();
    };
    let clock = || 42_i64;

    let outcome = engine
        .run_turn(
            "hi",
            None,
            &[],
            &[],
            &mut gate,
            &mut dsh_agent_loop::NoTools,
            &clock,
            &mut sink,
        )
        .await
        .expect("turn");
    assert_eq!(outcome.assistant_message, "Hello");
    assert_eq!(engine.phase(), Phase::Idle);
    assert_eq!(outcome.seq_range, (1, 10)); // 10 事件:turn/start…turn/end(含 llm 意图+完成审计)

    // provider 收到的 = 日志派生(正向不变式)
    let (received_header, received_messages) = &gate.inner().received[0];
    assert_eq!(received_header.model, "test-model");
    assert_eq!(
        received_messages,
        &json!([{ "role": "user", "content": "hi" }])
    );

    // JSONL:完整序列 + seq 连续(记录优先的落盘证据)
    let persisted = jsonl.load().unwrap();
    let types: Vec<&str> = persisted.iter().map(|e| e.r#type.as_str()).collect();
    assert_eq!(
        types,
        [
            "turn/start",
            "step/start",
            "user/message",
            "audit/call",
            "assistant/chunk",
            "assistant/chunk",
            "audit/call", // llm request-done(时长/用量审计)
            "assistant/message",
            "step/end",
            "turn/end",
        ]
    );
    for (idx, ev) in persisted.iter().enumerate() {
        assert_eq!(ev.seq, idx as u64 + 1, "seq 必须连续");
    }

    // 重放:加载 → 请求时刻前缀(seq ≤ step/start)派生 ≡ 出网请求(bit-exact)。
    // 完整日志派生多出的 assistant/message 正是本 turn 产出——下一轮请求才会看到。
    let reloaded: EventLog = {
        let mut l = EventLog::new();
        for ev in &persisted {
            l.append(ev.clone()).unwrap();
        }
        l
    };
    let prefix: Vec<_> = persisted
        .iter()
        .take_while(|ev| ev.r#type != "assistant/chunk")
        .collect();
    let request_time = derive_messages(prefix.into_iter());
    assert_eq!(
        &request_time, received_messages,
        "请求时刻的日志前缀派生必须与出网请求一致"
    );
    let full = derive_messages(reloaded.iter());
    assert_eq!(
        full,
        json!([
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": "Hello" },
        ]),
        "完整日志派生包含本 turn 产出"
    );
}

#[tokio::test]
async fn gate_rejects_tampered_request() {
    // 出口标准:内容与日志派生不一致的伪造请求被宿主拒
    let log = Arc::new(Mutex::new(EventLog::new()));
    {
        let mut l = log.lock().unwrap();
        l.append(EventEnvelope::new(
            "user/message",
            0,
            json!({ "content": "hi" }),
        ))
        .unwrap();
    }
    let mut gate = InvariantGate::new(FakeProvider::new(), log);

    // 篡改内容
    let tampered = json!([{ "role": "user", "content": "TAMPERED" }]);
    let err = gate
        .stream(&header(), &tampered)
        .await
        .expect_err("篡改必须被拒");
    assert!(err.to_string().contains("diverge"), "got: {err}");

    // 注入未记录消息(模型可见但日志无此物)
    let injected = json!([
        { "role": "user", "content": "hi" },
        { "role": "assistant", "content": "hallucinated" },
    ]);
    assert!(gate.stream(&header(), &injected).await.is_err());

    // 未篡改的派生请求通过
    let legit = json!([{ "role": "user", "content": "hi" }]);
    gate.stream(&header(), &legit)
        .await
        .expect("派生一致的请求必须放行");
}

#[tokio::test]
async fn multi_turn_accumulates_history() {
    let dir = std::env::temp_dir().join(format!("dsh-e2e-multi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jsonl = JsonlBackend::create(dir.join("multi.jsonl")).unwrap();
    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(header(), Arc::clone(&log));
    let mut provider = FakeProvider::new();
    provider.then(vec![LlmEvent::Chunk("a".into()), LlmEvent::Done]);
    provider.then(vec![LlmEvent::Chunk("b".into()), LlmEvent::Done]);
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let clock = || 0_i64;
    let mut sink = |ev: &EventEnvelope| {
        jsonl.append(ev).unwrap();
    };

    engine
        .run_turn(
            "one",
            None,
            &[],
            &[],
            &mut gate,
            &mut dsh_agent_loop::NoTools,
            &clock,
            &mut sink,
        )
        .await
        .unwrap();
    engine
        .run_turn(
            "two",
            None,
            &[],
            &[],
            &mut gate,
            &mut dsh_agent_loop::NoTools,
            &clock,
            &mut sink,
        )
        .await
        .unwrap();

    // 第二轮出网请求包含完整历史(日志派生的累积性)
    let (_, second_messages) = &gate.inner().received[1];
    assert_eq!(
        second_messages,
        &json!([
            { "role": "user", "content": "one" },
            { "role": "assistant", "content": "a" },
            { "role": "user", "content": "two" },
        ]),
        "第二轮请求必须包含第一轮的完整历史(全部来自日志派生)"
    );
}
