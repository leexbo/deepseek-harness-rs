//! 工具闭环 e2e:模型 tool_calls → 沙箱 bash → tool/result → 下一轮可见。
//!
//! 断言链:
//! - 事件序列含 tool/call 与 tool/result(引用链:tool/result.call = tool/call 的 seq);
//! - bash 命令真实经沙箱执行(echo 输出进入 tool/result.output);
//! - 第二轮出网请求包含 assistant(tool_calls) 与 tool 消息——全部来自日志派生;
//! - 「记录 ⟺ 可见」:出网消息恰为日志前缀派生(不变式闸门强制)。

use std::sync::{Arc, Mutex};

use dsh_agent_loop::{LlmEvent, LoopEngine, RequestHeader};
use dsh_host::JsonlBackend;
use dsh_llm::{FakeProvider, InvariantGate};
use dsh_session::{EventEnvelope, EventLog};
use dsh_tools::BashTool;
use serde_json::json;

#[tokio::test]
async fn tool_round_trip_through_sandbox() {
    let dir = std::env::temp_dir().join(format!("dsh-tool-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jsonl = JsonlBackend::create(dir.join("tool.jsonl")).unwrap();

    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(
        RequestHeader {
            model: "test".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: Vec::new(),
        },
        Arc::clone(&log),
    );

    // 第一步:模型请求 bash 工具;第二步:给出最终答复
    let mut provider = FakeProvider::new();
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "",
        "tool_calls": [
            { "name": "bash", "arguments": { "command": "echo tool-ran-ok", "description": "Echo confirmation marker" } }
        ],
    }))]);
    provider.then(vec![
        LlmEvent::Chunk("done".into()),
        LlmEvent::AssistantMessage(json!({ "content": "command finished" })),
        LlmEvent::Done,
    ]);
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let mut bash = BashTool::new(&dir);
    let clock = || 0_i64;
    let mut sink = |ev: &EventEnvelope| {
        jsonl.append(ev).unwrap();
    };

    let outcome = engine
        .run_turn(
            "run the tool",
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
    assert_eq!(outcome.assistant_message, "command finished");

    // 事件序列与引用链
    let persisted = jsonl.load().unwrap();
    let types: Vec<&str> = persisted.iter().map(|e| e.r#type.as_str()).collect();
    assert_eq!(
        types,
        [
            "turn/start",
            "step/start",
            "user/message",
            "audit/call", // llm 出网审计(E5,归因 user/message)
            "audit/call", // llm 完成审计(时长/用量)
            "assistant/message",
            "tool/call",
            "audit/call", // 工具执行审计(E5,归因 assistant/message)
            "tool/result",
            "audit/call", // 工具完成审计(时长)
            "step/end",
            "step/start",
            "audit/call",
            "assistant/chunk",
            "audit/call", // llm 完成审计
            "assistant/message",
            "step/end",
            "turn/end",
        ]
    );
    let tool_call = persisted
        .iter()
        .find(|e| e.r#type == "tool/call")
        .expect("tool/call");
    let tool_result = persisted
        .iter()
        .find(|e| e.r#type == "tool/result")
        .expect("tool/result");
    assert_eq!(
        tool_result.data["call"], tool_call.seq,
        "引用链:result.call = call.seq"
    );
    // bash 真实经沙箱执行
    assert_eq!(tool_result.data["output"], "tool-ran-ok");
    assert_eq!(tool_result.data["success"], true);
    // 终端详情经渲染意图视图携带(bash settle → Terminal 视图 → 事件 view)
    assert_eq!(tool_result.data["view"]["card"], "terminal");
    assert_eq!(tool_result.data["view"]["exitCode"], 0);
    assert!(
        tool_result.data["view"]["cwd"]
            .as_str()
            .is_some_and(|w| !w.is_empty()),
        "cwd 应在视图内"
    );

    // 第二轮出网请求:含 assistant(tool_calls) 与 tool 结果(全部来自派生)
    let (_, second) = &gate.inner().received[1];
    assert_eq!(
        second,
        &json!([
            { "role": "user", "content": "run the tool" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "name": "bash", "arguments": { "command": "echo tool-ran-ok", "description": "Echo confirmation marker" } }
            ]},
            { "role": "tool", "output": "tool-ran-ok", "call": tool_call.seq, "id": "" },
        ]),
        "第二轮请求必须包含完整工具往返(记录 ⟺ 可见)"
    );
}

#[tokio::test]
async fn sandboxed_tool_denies_out_of_root_write() {
    // 工具经沙箱执行:可写根外写被拒 → tool/result.success = false,loop 不中断
    let dir = std::env::temp_dir().join(format!("dsh-tool-deny-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // 根外 = 可写根(workspace + 平台临时区)之外:系统根目录
    // (macOS 只读系统卷 / bwrap ro-bind /,跨平台统一)
    let outside = std::path::PathBuf::from(format!("/dsh-tool-outside-{}", std::process::id()));
    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(
        RequestHeader {
            model: "t".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: Vec::new(),
        },
        Arc::clone(&log),
    );
    let mut provider = FakeProvider::new();
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "",
        "tool_calls": [ { "name": "bash",
            "arguments": { "command": format!("touch {}", outside.display()), "description": "Touch file outside workspace" } } ],
    }))]);
    provider.then(vec![LlmEvent::AssistantMessage(
        json!({ "content": "after" }),
    )]);
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let mut bash = BashTool::new(&dir);
    let clock = || 0_i64;
    let mut sink = |_ev: &EventEnvelope| {};

    engine
        .run_turn(
            "go",
            None,
            &[],
            &[],
            &mut gate,
            &mut bash,
            &clock,
            &mut sink,
        )
        .await
        .expect("turn 不因工具失败而中断");

    let log = log.lock().unwrap();
    let result = log
        .iter()
        .find(|e| e.r#type == "tool/result")
        .expect("tool/result");
    // 2026-08 语义:非零退出是结果数据(success=true + 视图 exitCode);
    // 沙箱拒绝以「非干净成功」断言(视图 exitCode 在场且非 0)
    assert!(
        result.data["success"] == serde_json::json!(false)
            || result.data["view"]["exitCode"]
                .as_i64()
                .is_some_and(|c| c != 0),
        "根外写必须失败:success={}",
        result.data["success"]
    );
    drop(log);
    assert!(!outside.exists(), "根外文件不应存在");
}

#[tokio::test]
async fn cancel_token_aborts_running_tool_and_turn() {
    // 出口门:取消 token 中止工具(软取消 select → kill_with_grace)
    use dsh_agent_loop::{CancelToken, LoopError};

    let dir = std::env::temp_dir().join(format!("dsh-tool-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let token = CancelToken::new();
    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(
        RequestHeader {
            model: "t".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: Vec::new(),
        },
        Arc::clone(&log),
    );
    engine.set_cancel(token.clone());

    let mut provider = FakeProvider::new();
    // 第一步请求长命令;取消后不再有下一步
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "",
        "tool_calls": [ { "name": "bash",
            "arguments": { "command": "sleep 30 && echo never", "description": "Sleep past cancel window" } } ],
    }))]);
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let mut bash = BashTool::new(&dir).with_cancel(token.clone());
    let mut sink = |_ev: &EventEnvelope| {};

    // 200ms 后触发软取消
    let delayed = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        delayed.cancel();
    });
    let started = std::time::Instant::now();
    let result = engine
        .run_turn(
            "go",
            None,
            &[],
            &[],
            &mut gate,
            &mut bash,
            &|| 0_i64,
            &mut sink,
        )
        .await;
    let elapsed = started.elapsed();
    assert!(
        matches!(result, Err(LoopError::Cancelled)),
        "软取消必须中止 turn"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "取消应及时(实际 {elapsed:?})"
    );

    // 日志:turn/end 带 cancelled 归因;无 "never" 输出
    let l = log.lock().unwrap();
    let end = l.iter().find(|e| e.r#type == "turn/end").expect("turn/end");
    assert_eq!(end.data["cancelled"], "token");
    let tool_result = l
        .iter()
        .find(|e| e.r#type == "tool/result")
        .expect("tool/result");
    assert_eq!(tool_result.data["output"], "cancelled");
}

#[tokio::test]
async fn pty_tool_runs_and_reports_tty() {
    // PTY 工具路径:test -t 1 在 PTY 下为真(沙箱经 argv 包装)
    let dir = std::env::temp_dir().join(format!("dsh-tool-pty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(
        RequestHeader {
            model: "t".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: Vec::new(),
        },
        Arc::clone(&log),
    );
    let mut provider = FakeProvider::new();
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "",
        "tool_calls": [ { "name": "bash",
            "arguments": { "command": "test -t 1 && echo is-tty", "description": "Check stdout is a TTY" } } ],
    }))]);
    provider.then(vec![LlmEvent::AssistantMessage(
        json!({ "content": "done" }),
    )]);
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let mut bash = BashTool::new(&dir).with_pty();
    let mut sink = |_ev: &EventEnvelope| {};

    engine
        .run_turn(
            "go",
            None,
            &[],
            &[],
            &mut gate,
            &mut bash,
            &|| 0_i64,
            &mut sink,
        )
        .await
        .expect("turn");
    let l = log.lock().unwrap();
    let tool_result = l
        .iter()
        .find(|e| e.r#type == "tool/result")
        .expect("tool/result");
    // landlock-only 机器:spawn 拒绝(fail-closed)也是合法结果
    let output = tool_result.data["output"].as_str().unwrap_or_default();
    assert!(
        output.contains("is-tty")
            || output.contains("fail-closed")
            || output.contains("pty spawn failed"),
        "got: {output}"
    );
}

#[tokio::test]
async fn file_tools_round_trip_through_toolset() {
    // ToolSet 按名分发文件三件套;file_edit → file_read 闭环,
    // 读写往返全部经 tool/result 事件进入下一轮派生(记录 ⟺ 可见)
    use dsh_agent_loop::ToolSet;
    use dsh_tools::FileTools;

    let dir = std::env::temp_dir().join(format!("dsh-file-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jsonl = JsonlBackend::create(dir.join("file.jsonl")).unwrap();

    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(
        RequestHeader {
            model: "t".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: Vec::new(),
        },
        Arc::clone(&log),
    );

    // 第一步:edit 写入;第二步:read 验证;第三步:收尾
    let mut provider = FakeProvider::new();
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "",
        "tool_calls": [ { "name": "file_edit", "arguments": {
            "path": "note.md", "old_text": "draft", "new_text": "final" } } ],
    }))]);
    // edit 前置内容由夹具准备;第二步读取同一文件
    std::fs::write(dir.join("note.md"), "this is a draft\n").unwrap();
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "",
        "tool_calls": [ { "name": "file_read", "arguments": { "path": "note.md" } } ],
    }))]);
    provider.then(vec![LlmEvent::AssistantMessage(
        json!({ "content": "verified" }),
    )]);

    let mut tools = ToolSet::new(vec![Box::new(FileTools::new(&dir))]).unwrap();
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let mut sink = |ev: &EventEnvelope| {
        jsonl.append(ev).unwrap();
    };

    let outcome = engine
        .run_turn(
            "edit then verify",
            None,
            &[],
            &[],
            &mut gate,
            &mut tools,
            &|| 0_i64,
            &mut sink,
        )
        .await
        .expect("turn");
    assert_eq!(outcome.assistant_message, "verified");

    // 文件真实被改写
    assert_eq!(
        std::fs::read_to_string(dir.join("note.md")).unwrap(),
        "this is a final\n"
    );

    // 两个工具结果都落日志且成功;read 输出含替换后的文本
    let persisted = jsonl.load().unwrap();
    let results: Vec<&EventEnvelope> = persisted
        .iter()
        .filter(|e| e.r#type == "tool/result")
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.data["success"] == true));
    assert!(
        results[1].data["output"]
            .as_str()
            .expect("output")
            .contains("this is a final")
    );

    // 日志可重放:重建后派生面一致(第三轮请求含两个工具往返)
    let (_, last) = &gate.inner().received[2];
    assert!(
        serde_json::to_string(last)
            .unwrap()
            .contains("this is a final")
    );
}

#[tokio::test]
async fn todo_write_events_flow_through_engine_and_restore() {
    // todo_write 整表替换 → engine 在 tool/result 后追加 todo/write(单边界);
    // 新实例共享日志懒恢复 = 崩溃恢复语义;todo/write 不进消息面(闸门仍过)
    use dsh_agent_loop::ToolSet;
    use dsh_tools::TodoWriteTool;

    let dir = std::env::temp_dir().join(format!("dsh-todo-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jsonl = JsonlBackend::create(dir.join("todo.jsonl")).unwrap();

    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(
        RequestHeader {
            model: "t".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: Vec::new(),
        },
        Arc::clone(&log),
    );

    let mut provider = FakeProvider::new();
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "",
        "tool_calls": [ { "name": "todo_write", "arguments": {
            "todos": [ { "content": "write todo tests", "status": "pending" } ] } } ],
    }))]);
    provider.then(vec![LlmEvent::AssistantMessage(
        json!({ "content": "tracked" }),
    )]);

    let mut tools = ToolSet::new(vec![Box::new(TodoWriteTool::new(Arc::clone(&log)))]).unwrap();
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    let mut sink = |ev: &EventEnvelope| {
        jsonl.append(ev).unwrap();
    };

    engine
        .run_turn(
            "track my work",
            None,
            &[],
            &[],
            &mut gate,
            &mut tools,
            &|| 0_i64,
            &mut sink,
        )
        .await
        .expect("turn");

    // 事件顺序:tool/result 紧随其后是 todo/write;type 已登记
    let persisted = jsonl.load().unwrap();
    let idx = |t: &str| {
        persisted
            .iter()
            .position(|e| e.r#type == t)
            .unwrap_or(usize::MAX)
    };
    assert!(idx("tool/call") < idx("tool/result"));
    assert!(
        idx("tool/result") < idx("todo/write"),
        "todo/write 必须在 tool/result 之后追加"
    );
    let state = persisted.iter().find(|e| e.r#type == "todo/write").unwrap();
    assert_eq!(state.data["todos"][0]["content"], "write todo tests");
    // 条目无 id({content, status})
    assert!(state.data["todos"][0].get("id").is_none());

    // 跨实例恢复:新 TodoWriteTool 共享同一日志,整表覆写即见恢复态(崩溃恢复语义)
    let mut revived = TodoWriteTool::new(Arc::clone(&log));
    use dsh_agent_loop::{ToolCallRequest, ToolPort};
    let out = ToolPort::execute(
        &mut revived,
        &ToolCallRequest {
            name: "todo_write".into(),
            arguments: json!({ "todos": [ { "content": "write todo tests", "status": "completed" } ] }),
        },
    )
    .await;
    assert_eq!(
        out.output,
        "Updated todo list: 0 pending, 0 in progress, 1 completed."
    );
    let events = ToolPort::take_state_events(&mut revived);
    assert_eq!(events.last().unwrap().1["todos"][0]["status"], "completed");

    // 消息面不含 todo/write(非 surface;闸门比对全程通过即证明)
    let l = log.lock().unwrap();
    let msgs = dsh_session::derive_messages(l.iter());
    let s = serde_json::to_string(&msgs).unwrap();
    assert!(!s.contains("todo/write"));
    assert!(s.contains("write todo tests"), "工具结果本身仍可见");
}

#[tokio::test]
async fn plan_mode_submit_and_approval_flow() {
    // 计划模式状态机。plan 态 exit_plan_mode 成功并落 plan/submitted;
    // 批准(plan/approved + 回标准态)后再提交被拒;全程事件入日志可重放
    use dsh_agent_loop::ToolSet;
    use dsh_tools::PlanTool;

    let dir = std::env::temp_dir().join(format!("dsh-plan-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jsonl = JsonlBackend::create(dir.join("plan.jsonl")).unwrap();

    let log = Arc::new(Mutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(
        RequestHeader {
            model: "t".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: Vec::new(),
        },
        Arc::clone(&log),
    );
    let mut sink = |ev: &EventEnvelope| {
        jsonl.append(ev).unwrap();
    };
    let clock = || 0_i64;

    // 标准态:exit_plan_mode 被拒
    let mut provider = FakeProvider::new();
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "", "tool_calls": [
            { "name": "exit_plan_mode", "arguments": { "plan": "# p" } } ],
    }))]);
    provider.then(vec![LlmEvent::AssistantMessage(
        json!({ "content": "done0" }),
    )]);
    let mut tools = ToolSet::new(vec![Box::new(PlanTool::new(Arc::clone(&log)))]).unwrap();
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    engine
        .run_turn(
            "go",
            None,
            &[],
            &[],
            &mut gate,
            &mut tools,
            &clock,
            &mut sink,
        )
        .await
        .expect("turn");
    {
        let l = log.lock().unwrap();
        let result = l
            .iter()
            .find(|e| e.r#type == "tool/result")
            .expect("tool/result");
        assert_eq!(result.data["success"], false, "标准态不得提交计划");
    }

    // 进入 plan 态:提交成功,plan/submitted 在 tool/result 后
    engine
        .commit_session_event("session/mode", json!({ "mode": "plan" }), &clock, &mut sink)
        .unwrap();
    let mut provider = FakeProvider::new();
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "", "tool_calls": [
            { "name": "exit_plan_mode", "arguments": { "plan": "# fix\n1. step" } } ],
    }))]);
    provider.then(vec![LlmEvent::AssistantMessage(
        json!({ "content": "submitted" }),
    )]);
    let mut tools = ToolSet::new(vec![Box::new(PlanTool::new(Arc::clone(&log)))]).unwrap();
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    engine
        .run_turn(
            "plan it",
            None,
            &[],
            &[],
            &mut gate,
            &mut tools,
            &clock,
            &mut sink,
        )
        .await
        .expect("turn");
    {
        let l = log.lock().unwrap();
        let result = l
            .iter()
            .rev()
            .find(|e| e.r#type == "tool/result")
            .expect("tool/result");
        assert_eq!(result.data["success"], true);
        let submitted = l
            .iter()
            .find(|e| e.r#type == "plan/submitted")
            .expect("plan/submitted");
        assert!(submitted.data["plan"].as_str().unwrap().contains("# fix"));
    }

    // 批准:plan/approved + 回标准态;此后再提交被拒(读日志态)
    engine
        .commit_session_event(
            "plan/approved",
            json!({ "plan": "# fix\n1. step" }),
            &clock,
            &mut sink,
        )
        .unwrap();
    engine
        .commit_session_event(
            "session/mode",
            json!({ "mode": "standard" }),
            &clock,
            &mut sink,
        )
        .unwrap();
    let mut provider = FakeProvider::new();
    provider.then(vec![LlmEvent::AssistantMessage(json!({
        "content": "", "tool_calls": [
            { "name": "exit_plan_mode", "arguments": { "plan": "# again" } } ],
    }))]);
    provider.then(vec![LlmEvent::AssistantMessage(
        json!({ "content": "done2" }),
    )]);
    let mut tools = ToolSet::new(vec![Box::new(PlanTool::new(Arc::clone(&log)))]).unwrap();
    let mut gate = InvariantGate::new(provider, Arc::clone(&log));
    engine
        .run_turn(
            "implement",
            None,
            &[],
            &[],
            &mut gate,
            &mut tools,
            &clock,
            &mut sink,
        )
        .await
        .expect("turn");
    let l = log.lock().unwrap();
    let result = l
        .iter()
        .rev()
        .find(|e| e.r#type == "tool/result")
        .expect("tool/result");
    assert_eq!(result.data["success"], false, "批准回标准态后不得再提交");
    // 重放:JSONL 重建后 mode/plan 状态可恢复(decode 全部通过即证)
    drop(l);
    let persisted = jsonl.load().unwrap();
    assert!(persisted.iter().any(|e| e.r#type == "session/mode"));
    assert!(persisted.iter().any(|e| e.r#type == "plan/approved"));
}

#[tokio::test]
async fn subagent_runs_own_session_and_reports_back() {
    // 子代理独立日志 + 能力束窄化 + 报告回主日志。
    // 直接驱动工具面(不经主 engine;主面往返已有其余测试覆盖)
    use dsh_agent_loop::{ToolCallRequest, ToolPort};
    use dsh_tools::{SubagentControlTool, SubagentTool};

    let dir = std::env::temp_dir().join(format!("dsh-sub-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // 子代理传输:一次 bash echo + 一次收尾(子代理内部也有工具循环)。
    // 传输为工厂形态——每次调用产一个携全脚本的 provider(FakeProvider
    // 不 Clone;单子代理场景每次 factory 调用即一次完整脚本重放)
    let responses = vec![
        vec![LlmEvent::AssistantMessage(json!({
            "content": "", "tool_calls": [
                { "name": "bash", "arguments": { "command": "echo child-at-work", "description": "Echo child marker" } } ],
        }))],
        vec![LlmEvent::AssistantMessage(json!({
            "content": "child finished: all done"
        }))],
    ];
    let factory: dsh_tools::subagent::TransportFactory<FakeProvider> = Arc::new(move || {
        let mut p = FakeProvider::new();
        for r in &responses {
            p.then(r.clone());
        }
        Ok(p)
    });

    let mut tool = SubagentTool::new(&dir, factory, "test-model".into());
    let registry = tool.registry.clone();
    let out = ToolPort::execute(
        &mut tool,
        &ToolCallRequest {
            name: "subagent".into(),
            arguments: json!({ "task": "echo something" }),
        },
    )
    .await;
    assert!(out.success, "{}", out.output);
    assert!(out.output.contains("child finished: all done"));
    assert!(out.output.contains("session"));

    // 子会话日志真实存在且可重放
    let rec = registry.lock().unwrap()[0].clone();
    assert_eq!(rec.status, "done");
    let child_events =
        dsh_host::persistence::jsonl::load_jsonl(std::path::Path::new(&rec.session_path))
            .unwrap_or_default();
    assert!(!child_events.is_empty(), "子会话事件必须落盘");
    assert!(child_events.iter().any(|e| e.r#type == "turn/start"));
    assert!(
        child_events
            .iter()
            .any(|e| e.r#type == "tool/result" && e.data["output"] == "child-at-work"),
        "子代理的 bash 真实执行"
    );
    // 能力束窄化:子可写根 = .dsh/subagents/1(+ 平台临时区);
    // 根外(系统根目录,macOS 只读系统卷 / bwrap ro-bind /)写被拒
    // (非干净成功 = 失败或非零退出,退出详情经 Terminal 视图携带)
    let child_root = dir.join(".dsh/subagents/1");
    let escape = std::path::PathBuf::from(format!("/dsh-tool-escape-{}", std::process::id()));
    let mut bash = BashTool::new(&child_root);
    let denied = ToolPort::execute(
        &mut bash,
        &ToolCallRequest {
            name: "bash".into(),
            arguments: json!({ "command": format!("touch {}", escape.display()), "description": "Touch escaped path" }),
        },
    )
    .await;
    let denied_exit = match denied.view {
        Some(dsh_agent_loop::ToolView::Terminal { exit_code, .. }) => exit_code,
        _ => None,
    };
    assert!(
        !denied.success || denied_exit.is_some_and(|c| c != 0),
        "子可写根外必须拒绝"
    );
    assert!(!escape.exists());

    // list 工具可见注册表(list_agents;前台一次性子代理
    // 不列入清单——不可续话,模型永不需要选它)
    let mut control = SubagentControlTool::new(registry.clone());
    let list = ToolPort::execute(
        &mut control,
        &ToolCallRequest {
            name: "list_agents".into(),
            arguments: json!({}),
        },
    )
    .await;
    assert!(list.output.contains("(no background subagents)"));
}

#[tokio::test]
async fn subagent_cancel_propagates_from_parent() {
    // 父令牌取消 → 子令牌 → 子引擎安全点收尾(cancelled)
    use dsh_agent_loop::{CancelToken, ToolCallRequest, ToolPort};
    use dsh_tools::SubagentTool;

    let dir = std::env::temp_dir().join(format!("dsh-sub-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let parent = CancelToken::new();

    // 子代理传输:子代理跑一个长 sleep bash,父令牌中途取消(工厂形态)
    let responses = vec![
        vec![LlmEvent::AssistantMessage(json!({
            "content": "", "tool_calls": [
                { "name": "bash", "arguments": { "command": "sleep 30 && echo never", "description": "Sleep past cancel window" } } ],
        }))],
        vec![LlmEvent::AssistantMessage(
            json!({ "content": "unreached" }),
        )],
    ];
    let factory: dsh_tools::subagent::TransportFactory<FakeProvider> = Arc::new(move || {
        let mut p = FakeProvider::new();
        for r in &responses {
            p.then(r.clone());
        }
        Ok(p)
    });
    let mut tool = SubagentTool::new(&dir, factory, "m".into()).with_cancel(parent.clone());
    let delayed = parent.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        delayed.cancel();
    });
    let started = std::time::Instant::now();
    let out = ToolPort::execute(
        &mut tool,
        &ToolCallRequest {
            name: "subagent".into(),
            arguments: json!({ "task": "long task" }),
        },
    )
    .await;
    assert!(!out.success);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(!out.output.contains("unreached"));
    let rec = &tool.registry.lock().unwrap()[0];
    assert_eq!(rec.status, "cancelled");
}

#[tokio::test]
async fn background_job_runs_reads_and_stops() {
    // run_in_background 立即返回 job id;输出落盘;跨调用可读;
    // stop 杀长任务;jobs 工具全程可见
    use dsh_agent_loop::{ToolCallRequest, ToolPort};
    use dsh_tools::{JobTool, JobsRegistry};

    let dir = std::env::temp_dir().join(format!("dsh-jobs-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let registry: JobsRegistry = Arc::new(Mutex::new(Vec::new()));
    let mut bash = BashTool::new(&dir).with_jobs(Arc::clone(&registry));
    let mut jobs = JobTool::new(Arc::clone(&registry));

    fn bash_call(args: serde_json::Value) -> ToolCallRequest {
        ToolCallRequest {
            name: "bash".into(),
            arguments: args,
        }
    }

    // 后台启动:立即返回
    let out = ToolPort::execute(
        &mut bash,
        &bash_call(json!({ "command": "echo bg-output && sleep 1", "description": "Echo background output", "run_in_background": true })),
    )
    .await;
    assert!(out.success, "{}", out.output);
    assert!(out.output.contains("started job 1"));

    // 长任务(用于 stop)
    let out = ToolPort::execute(
        &mut bash,
        &bash_call(json!({ "command": "sleep 30", "description": "Sleep for stop test", "run_in_background": true })),
    )
    .await;
    assert!(out.output.contains("started job 2"));

    // list:两个 running
    let list = ToolPort::execute(
        &mut jobs,
        &ToolCallRequest {
            name: "jobs".into(),
            arguments: json!({ "action": "list" }),
        },
    )
    .await;
    assert!(list.output.contains("1. [running]"), "{}", list.output);
    assert!(list.output.contains("2. [running]"));

    // 等 job 1 完成(watcher 落盘 + 收尾)
    let mut job1_done = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let done = registry
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == 1)
            .is_some_and(|j| j.status == "done");
        if done {
            job1_done = true;
            break;
        }
    }
    assert!(job1_done, "job 1 应在 5s 内完成");

    // read:输出已落盘
    let read = ToolPort::execute(
        &mut jobs,
        &ToolCallRequest {
            name: "jobs".into(),
            arguments: json!({ "action": "read", "id": 1 }),
        },
    )
    .await;
    assert!(read.output.contains("bg-output"), "{}", read.output);
    // 输出文件即持久事实
    assert!(dir.join(".dsh/jobs/1.log").exists());

    // stop job 2:快速终止
    let started = std::time::Instant::now();
    let stop = ToolPort::execute(
        &mut jobs,
        &ToolCallRequest {
            name: "jobs".into(),
            arguments: json!({ "action": "stop", "id": 2 }),
        },
    )
    .await;
    assert!(stop.success, "{}", stop.output);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let status2 = registry
        .lock()
        .unwrap()
        .iter()
        .find(|j| j.id == 2)
        .unwrap()
        .status
        .clone();
    assert!(
        status2 == "stopped",
        "job 2 状态应为 stopped,实际 {status2}"
    );

    // 未启用后台的装配:run_in_background 被拒
    let mut plain = BashTool::new(&dir);
    let denied = ToolPort::execute(
        &mut plain,
        &bash_call(json!({ "command": "echo x", "run_in_background": true })),
    )
    .await;
    assert!(!denied.success);
}
