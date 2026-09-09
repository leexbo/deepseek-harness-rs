//! workspace 指令(AGENTS.md)逐步重扫 e2e:引擎 + 宿主 InstructionRuntimeState
//! 闭环验证(载具复用 tool_loop 假 provider 架构)。
//!
//! 断言链:
//! - 首回合注入基线(system-reminder 包裹,baseline+baselineIdentity+set changes);
//! - 无变化次回合零新注入;
//! - 根文件内容修改 → 「Updated instructions from」replace(不回放旧全文);
//! - 删除 → 「Instructions removed」remove;
//! - file_read 触碰路径在下一 step 拾取后代目录新建文件(同 turn 内可见;
//!   工作区之下的嵌套指令文件不在基线链内,唯一入口是触碰)。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dsh_agent_loop::{LlmEvent, LoopEngine, RequestHeader, ToolCallRequest, ToolOutput, ToolPort};
use dsh_host::InstructionRuntimeState;
use dsh_llm::{FakeProvider, InvariantGate};
use dsh_session::{EventEnvelope, EventLog};
use serde_json::{Value, json};

/// 可编程假工具:执行前弹出副作用(创建指令文件的场景)
struct ScriptTool {
    /// (要写的目标路径, 内容)
    side_effects: Mutex<Vec<(String, String)>>,
}

impl ToolPort for ScriptTool {
    fn specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "file_read",
                "parameters": { "type": "object", "properties": {
                    "path": { "type": "string" } }, "required": ["path"] }
            }
        })]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        let path = call.arguments["path"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        for (target, content) in self.side_effects.lock().unwrap().drain(..) {
            if let Some(parent) = PathBuf::from(&target).parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&target, content).unwrap();
        }
        ToolOutput {
            output: format!("read {path}"),
            success: true,
            ..Default::default()
        }
    }
}

struct Harness {
    /// 根目录(Drop 时整体清场;含 ws/home)
    root: PathBuf,
    log: Arc<Mutex<EventLog>>,
    state: Arc<Mutex<InstructionRuntimeState>>,
    home: PathBuf,
    ws: PathBuf,
    tool_fx: Arc<Mutex<Vec<(String, String)>>>,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("dsh-instr-e2e-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        Self {
            root: dir.clone(),
            log: Arc::new(Mutex::new(EventLog::new())),
            state: Arc::new(Mutex::new(InstructionRuntimeState::new())),
            home: dir.join("home"),
            ws: dir.join("ws"),
            tool_fx: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn run_turn(&self, input: &str, steps: Vec<Value>) {
        let mut provider = FakeProvider::new();
        for s in steps {
            provider.then(vec![LlmEvent::AssistantMessage(s), LlmEvent::Done]);
        }
        let mut gate = InvariantGate::new(provider, Arc::clone(&self.log));

        let mut engine = LoopEngine::new(
            RequestHeader {
                model: "t".into(),
                system: String::new(),
                temperature: 0.0,
                reasoning_effort: None,
                tools: Vec::new(),
            },
            Arc::clone(&self.log),
        );
        let (state, log, home, ws) = (
            Arc::clone(&self.state),
            Arc::clone(&self.log),
            self.home.clone(),
            self.ws.clone(),
        );
        engine.set_instructions_provider(Box::new(move |touches| {
            let mut st = state.lock().unwrap();
            let events = log.lock().unwrap().iter().cloned().collect::<Vec<_>>();
            st.compose(&events, &home, &ws, touches)
        }));
        let mut tool = ScriptTool {
            side_effects: Mutex::new(std::mem::take(&mut *self.tool_fx.lock().unwrap())),
        };
        let clock = || 0_i64;
        let mut sink = |_: &EventEnvelope| {};
        engine
            .run_turn(
                input,
                None,
                &[],
                &[],
                &mut gate,
                &mut tool,
                &clock,
                &mut sink,
            )
            .await
            .expect("turn");
    }

    fn instruction_events(&self) -> Vec<Value> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.r#type == "user/message")
            .filter(|e| e.data["source"]["kind"] == Value::String("agent-instructions".into()))
            .map(|e| e.data.clone())
            .collect()
    }

    fn text_of(ev: &Value) -> String {
        ev["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn actions(ev: &Value) -> Vec<String> {
        ev["source"]["changes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| c["action"].as_str().unwrap_or("").into())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn baseline_once_then_replace_and_remove() {
    let h = Harness::new("main");

    // 首回合:根 AGENTS.md 在场 → 注入基线(baseline 标记 + set changes)
    std::fs::write(h.ws.join("AGENTS.md"), "# 项目规范\n\n用 Rust。").unwrap();
    h.run_turn("hi", vec![json!({ "content": "ok" })]).await;
    let events = h.instruction_events();
    assert_eq!(events.len(), 1, "首回合恰好一条基线注入");
    assert!(Harness::text_of(&events[0]).contains("<system-reminder>"));
    assert!(Harness::text_of(&events[0]).contains("# 项目规范"));
    assert_eq!(events[0]["source"]["baseline"], Value::Bool(true));
    assert!(events[0]["source"]["baselineIdentity"].is_string());
    assert_eq!(Harness::actions(&events[0]), vec!["set"]);

    // 次回合无变化:零新注入
    h.run_turn("again", vec![json!({ "content": "still ok" })])
        .await;
    let events = h.instruction_events();
    if events.len() != 1 {
        panic!(
            "无变化不得重复注入;第二条={:?}",
            events.get(1).map(Harness::text_of)
        );
    }

    // 修改根内容 → replace(增量只含更新段落,不携带旧全文)
    std::fs::write(
        h.ws.join("AGENTS.md"),
        "# 项目规范 v2\n\n用 Rust + clippy。",
    )
    .unwrap();
    h.run_turn("third", vec![json!({ "content": "ok" })]).await;
    let events = h.instruction_events();
    assert_eq!(events.len(), 2, "内容变更产生且仅产生一条增量");
    let text = Harness::text_of(events.last().unwrap());
    assert!(
        text.contains("Updated instructions from: AGENTS.md"),
        "{text}"
    );
    assert!(text.contains("clippy"));
    assert!(
        !text.contains("用 Rust。\n</system-reminder>"),
        "replace 不回放旧版正文:{text}"
    );
    assert_eq!(events[1]["source"]["baseline"], Value::Null);
    assert_eq!(Harness::actions(events.last().unwrap()), vec!["replace"]);

    // 删除根文件 → remove 通告
    std::fs::remove_file(h.ws.join("AGENTS.md")).unwrap();
    h.run_turn("fourth", vec![json!({ "content": "ok" })]).await;
    let events = h.instruction_events();
    let last = events.last().unwrap();
    assert!(
        Harness::text_of(last).contains("Instructions removed: AGENTS.md"),
        "删除应出移除通告:{:?}",
        Harness::text_of(last)
    );
    assert_eq!(Harness::actions(last), vec!["remove"]);
}

#[tokio::test]
async fn touch_descendant_dirs_pick_up_nested_file_mid_turn() {
    let h = Harness::new("touch");
    std::fs::write(h.ws.join("AGENTS.md"), "root").unwrap();

    // 工具读 deep/inner.txt 时顺手写下 deep/AGENTS.md(模拟工作流中途落档)
    *h.tool_fx.lock().unwrap() = vec![(
        h.ws.join("deep/AGENTS.md").to_string_lossy().to_string(),
        "触碰后才发现的深层约定。".into(),
    )];

    // 单 turn 两步:step1 发起 file_read(deep/inner.txt);step2 收尾。
    // 触碰路径的后代目录 [deep] 在 step2 组合时下探 → 增量拾取新文件。
    let read_path = h.ws.join("deep/inner.txt").display().to_string();
    h.run_turn(
        "read nested",
        vec![
            json!({ "content": "", "tool_calls": [
                { "name": "file_read", "arguments": { "path": read_path } }
            ] }),
            json!({ "content": "done" }),
        ],
    )
    .await;

    let events = h.instruction_events();
    assert_eq!(
        events.len(),
        2,
        "同 turn 内应含基线 + 触碰拾取的增量;实际 {}",
        events.len()
    );
    let inc = Harness::text_of(events.last().unwrap());
    assert!(
        inc.contains("Additional instructions from: deep/AGENTS.md"),
        "增量应命中深层作用域:{inc}"
    );
}
