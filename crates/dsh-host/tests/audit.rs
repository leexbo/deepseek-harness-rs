//! E5 审计事件溯源化测试(出口门:审计重放)。
//!
//! 断言链:
//! - 跨边界调用(llm 出网 / 工具执行)各留一条 `audit/call`,
//!   归因 sourceEventSeqs 指向正确的因果上游;
//! - **审计随会话归因重放**:JSONL 重载 → 归因链 bit-exact 相等;
//! - 篡改日志(不可归因事件携带引用链)被读取方守卫拒绝。

use std::sync::{Arc, Mutex};

use dsh_agent_loop::{LlmEvent, LoopEngine, RequestHeader, ToolCallRequest, ToolOutput, ToolPort};
use dsh_host::JsonlBackend;
use dsh_llm::{FakeProvider, InvariantGate};
use dsh_session::{Attribution, EventEnvelope, EventLog, attribution_chain};
use serde_json::{Value, json};

/// 固定输出工具(E5 只断言归因链,不关心真实 bash;自持实现使
/// host-core 测试不依赖 dsh-tools,消除 dev 依赖环)
struct EchoTool;

impl ToolPort for EchoTool {
    fn specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "fixed-output tool for audit tests",
                "parameters": { "type": "object" },
            },
        })]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        ToolOutput {
            output: format!("echo {}", call.name),
            success: true,
            ..Default::default()
        }
    }
}

fn header() -> RequestHeader {
    RequestHeader {
        model: "test-model".into(),
        system: String::new(),
        temperature: 0.0,
        reasoning_effort: None,
        tools: Vec::new(),
    }
}

#[tokio::test]
async fn audit_attributable_and_replayable() {
    let dir = std::env::temp_dir().join(format!("dsh-audit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jsonl = JsonlBackend::create(dir.join("audit.jsonl")).unwrap();

    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(header(), Arc::clone(&log));
    let mut provider = FakeProvider::new();
    // 第一步:模型请求 bash;第二步:最终答复
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "",
        "tool_calls": [
            { "name": "bash", "arguments": { "command": "echo audited" } }
        ],
    }))]);
    provider.then(vec![LlmEvent::AssistantMessage(
        json!({ "content": "fin" }),
    )]);
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let mut bash = EchoTool;
    let clock = || 7_i64;
    let mut sink = |ev: &EventEnvelope| {
        jsonl.append(ev).unwrap();
    };

    engine
        .run_turn(
            "run it",
            None,
            &[],
            &[],
            &mut gate,
            &mut bash,
            &clock,
            &mut sink,
        )
        .await
        .expect("turn");

    // 活体归因链:llm ×2(意图 + 完成)+ tool ×1(意图 + 完成)
    let live = attribution_chain(&log.lock().unwrap());
    assert_eq!(
        live.len(),
        6,
        "两 step 各一次出网(意图+完成)+ 一次工具执行(意图+完成)"
    );

    let user_seq = live[0].audit.source_seqs[0];
    assert_eq!(live[0].audit.boundary, "llm");
    assert_eq!(live[0].audit.operation, "request");
    assert_eq!(
        live[0].sources,
        vec![(user_seq, "user/message".to_string())],
        "llm 出网归因到触发 turn 的 user/message"
    );
    assert_eq!(live[1].audit.boundary, "llm");
    assert_eq!(live[1].audit.operation, "request-done");
    assert_eq!(
        live[1].sources,
        vec![(user_seq, "user/message".to_string())],
        "llm 完成审计与意图审计同归因"
    );

    assert_eq!(live[2].audit.boundary, "tool");
    assert_eq!(live[2].audit.operation, "bash");
    let assistant_seq = live[2].audit.source_seqs[0];
    assert_eq!(
        live[2].sources,
        vec![(assistant_seq, "assistant/message".to_string())],
        "工具执行归因到携带 tool_calls 的 assistant/message"
    );
    // 归因目标确实是携带 tool_calls 的那条
    {
        let l = log.lock().unwrap();
        let ev = l.get(assistant_seq).unwrap();
        assert!(
            ev.data["tool_calls"].is_array(),
            "归因目标须携带 tool_calls"
        );
    }
    assert_eq!(live[3].audit.boundary, "tool");
    assert_eq!(live[3].audit.operation, "bash");
    assert_eq!(
        live[3].audit.source_seqs[0], assistant_seq,
        "工具完成审计与意图审计同归因"
    );

    assert_eq!(live[4].audit.boundary, "llm", "第二 step 的出网审计");
    assert_eq!(live[5].audit.boundary, "llm", "第二 step 的完成审计");

    // ===== 审计重放:JSONL → 重建 → 归因链 bit-exact =====
    let persisted = jsonl.load().unwrap();
    let rebuilt: Vec<Attribution> = {
        let mut l = EventLog::new();
        for ev in &persisted {
            l.append(ev.clone()).unwrap();
        }
        attribution_chain(&l)
    };
    assert_eq!(live, rebuilt, "审计归因链必须随会话重放 bit-exact 重建");
}

#[test]
fn tampered_attribution_refused_on_rebuild() {
    // 篡改:不可归因事件(turn/start)被塞入引用链 → 读取方拒绝重建
    let mut raw = serde_json::to_value(EventEnvelope::new("turn/start", 0, json!({}))).unwrap();
    raw["seq"] = json!(1);
    raw["source_event_seqs"] = json!([42]);
    let err = dsh_session::decode_envelope(&raw).expect_err("必须拒绝");
    assert!(err.to_string().contains("not attributable"), "got: {err}");
}
