//! steer 中途注入语义(引擎级):运行中 turn 在 step 边界认领中途输入,
//! 认领 splice + user/message 同序落档;模型可见 ⟺ 已记录。
//! 闸门传输(gate)使首次模型调用阻塞——测试在阻塞窗口注入 steer,确定性验证。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};

use dsh_agent_loop::{
    LlmEvent, LlmTransport, LoopEngine, NoTools, RequestHeader, SteerInput, Summarizer,
    TransportError, TurnOutcome,
};
use dsh_session::{EventEnvelope, EventLog};
use serde_json::Value;
use tokio::sync::oneshot;

fn header() -> RequestHeader {
    RequestHeader {
        model: "m".into(),
        system: String::new(),
        temperature: 0.0,
        reasoning_effort: None,
        tools: Vec::new(),
    }
}

/// 闸门传输:首次调用阻塞(测试放行后返回),记录每次模型可见消息。
struct Gated {
    /// 首次调用开始时的通知(测试等待此信号)
    started: StdMutex<Option<oneshot::Sender<()>>>,
    /// 首次调用的放行闸(测试在注入 steer 后 send)
    release: StdMutex<Option<oneshot::Receiver<()>>>,
    calls: StdMutex<usize>,
    /// 每次调用的模型可见消息(断言 steer 已记录即已可见;Arc 供测试读取)
    seen: Arc<StdMutex<Vec<Value>>>,
}

impl Summarizer for Gated {
    fn summarize<'a>(
        &'a mut self,
        _header: &'a RequestHeader,
        _messages: &'a Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        Box::pin(async { Ok(String::new()) })
    }
}

impl LlmTransport for Gated {
    async fn stream(
        &mut self,
        _header: &RequestHeader,
        messages: &Value,
    ) -> Result<Vec<LlmEvent>, TransportError> {
        let n = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        self.seen.lock().unwrap().push(messages.clone());
        if n == 1 {
            let started = self.started.lock().unwrap().take().expect("首次调用有通知");
            let _ = started.send(());
            let rx = self.release.lock().unwrap().take().expect("首次调用有闸门");
            let _ = rx.await; // 测试放行前阻塞
        }
        Ok(vec![LlmEvent::Chunk(format!("r{n}")), LlmEvent::Done])
    }
}

/// 日志事件类型序列(过滤 ignorable)
fn types_of(log: &EventLog) -> Vec<(String, u64)> {
    log.iter().map(|e| (e.r#type.clone(), e.seq)).collect()
}

/// run_turn 未来(turn 完成后借出 transport/log 的读取句柄)
type TurnFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<TurnOutcome, dsh_agent_loop::LoopError>> + Send>,
>;

fn run_engine(
    transport: Gated,
    steer_buf: Arc<StdMutex<VecDeque<SteerInput>>>,
) -> (Arc<StdMutex<EventLog>>, TurnFuture) {
    let log = Arc::new(StdMutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(header(), Arc::clone(&log));
    engine.set_steer_buf(steer_buf);
    let fut = async move {
        let clock = || 0_i64;
        let mut sink = |_ev: &EventEnvelope| {};
        let mut transport = transport;
        engine
            .run_turn(
                "hello",
                None,
                &[],
                &[],
                &mut transport,
                &mut NoTools,
                &clock,
                &mut sink,
            )
            .await
    };
    (log, Box::pin(fut))
}

/// 流式期间的 steer:turn 不终结,认领 splice + user/message 同序落档,
/// 模型下一轮可见(单 turn 内两 step)。
#[tokio::test]
async fn steer_during_stream_continues_turn() {
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let gate = Gated {
        started: StdMutex::new(Some(started_tx)),
        release: StdMutex::new(Some(release_rx)),
        calls: StdMutex::new(0),
        seen: Arc::new(StdMutex::new(Vec::new())),
    };
    let steer_buf = Arc::new(StdMutex::new(VecDeque::new()));
    let seen = Arc::clone(&gate.seen);
    let (log, run) = run_engine(gate, Arc::clone(&steer_buf));

    let handle = tokio::spawn(run);
    started_rx.await.expect("首次模型调用已开始");
    // 阻塞窗口:注入 steer(宿主预分配 id)
    steer_buf.lock().unwrap().push_back(SteerInput {
        id: "s-1".into(),
        text: "steer-now".into(),
        images: Vec::new(),
        source: None,
    });
    release_tx.send(()).unwrap();
    let outcome = handle.await.unwrap().expect("turn ok");

    // 模型最终答复来自第二轮调用(steer 让 turn 延续)
    assert_eq!(outcome.assistant_message, "r2");

    // 事件序:inserted splice → claim splice → user/message(steer) 都在
    // 同一 turn 内(最后一个 turn/end 之前),且 turn/start 只有一个
    let events = log.lock().unwrap();
    let seqs = types_of(&events);
    let turns: Vec<&(String, u64)> = seqs.iter().filter(|(t, _)| t == "turn/start").collect();
    assert_eq!(turns.len(), 1, "steer 不得新开 turn");
    let steer_user = seqs
        .iter()
        .filter(|(t, _)| t == "user/message")
        .map(|(_, s)| *s)
        .collect::<Vec<_>>();
    assert_eq!(steer_user.len(), 2, "初始输入 + steer 两条 user/message");
    let splices = seqs
        .iter()
        .filter(|(t, _)| t == "agent/inbox/spliced")
        .collect::<Vec<_>>();
    assert_eq!(splices.len(), 2, "inserted + claim 两条 splice");
    // splice 与 user/message(steer)的顺序:splice 先于其 user/message
    let steer_seq = events
        .iter()
        .find(|e| e.r#type == "user/message" && e.data["id"] == "s-1")
        .map(|e| e.seq)
        .expect("steer user/message 已落档");
    let (inserted, claim) = (
        events
            .iter()
            .find(|e| {
                e.r#type == "agent/inbox/spliced"
                    && e.data["inserted"].as_array().is_some_and(|a| !a.is_empty())
            })
            .expect("inserted splice"),
        events
            .iter()
            .find(|e| e.r#type == "agent/inbox/spliced" && e.data["removedCount"] == 1)
            .expect("claim splice"),
    );
    assert!(inserted.seq < steer_seq && claim.seq < steer_seq);
    assert_eq!(
        inserted.data["inserted"][0]["id"], "s-1",
        "inserted splice 携带 steer id(UI claimed 匹配用)"
    );
    // 模型可见:第二轮调用的消息含 steer 文本(记录 ⟺ 可见)
    let seen = seen
        .lock()
        .unwrap()
        .get(1)
        .cloned()
        .expect("第二轮调用已发生");
    assert!(
        seen.to_string().contains("steer-now"),
        "模型第二轮可见 steer"
    );
}

/// turn 开始前已排队的 steer:首个 step 即认领(loop 顶部 drain),
/// 模型第一轮调用即见,仍为单 turn。
#[tokio::test]
async fn steer_queued_before_turn_drained_at_first_step() {
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let gate = Gated {
        started: StdMutex::new(Some(started_tx)),
        release: StdMutex::new(Some(release_rx)),
        calls: StdMutex::new(0),
        seen: Arc::new(StdMutex::new(Vec::new())),
    };
    let steer_buf = Arc::new(StdMutex::new(VecDeque::new()));
    steer_buf.lock().unwrap().push_back(SteerInput {
        id: "s-0".into(),
        text: "early".into(),
        images: Vec::new(),
        source: None,
    });
    let seen = Arc::clone(&gate.seen);
    let (log, run) = run_engine(gate, Arc::clone(&steer_buf));

    let handle = tokio::spawn(run);
    started_rx.await.expect("首次模型调用已开始");
    release_tx.send(()).unwrap();
    let outcome = handle.await.unwrap().expect("turn ok");
    assert_eq!(
        outcome.assistant_message, "r1",
        "首个 step 已含 steer,一轮即收尾"
    );

    let events = log.lock().unwrap();
    let seqs = types_of(&events);
    let turns: Vec<&(String, u64)> = seqs.iter().filter(|(t, _)| t == "turn/start").collect();
    assert_eq!(turns.len(), 1);
    // 模型第一轮调用已见 steer 文本
    let seen0 = seen.lock().unwrap().first().cloned().expect("首轮调用");
    assert!(seen0.to_string().contains("early"));
    assert!(seen0.to_string().contains("hello"), "初始输入也在");
}

/// 无 steer:不产生任何 splice(词汇不污染普通 turn)
#[tokio::test]
async fn no_steer_no_splice_events() {
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let gate = Gated {
        started: StdMutex::new(Some(started_tx)),
        release: StdMutex::new(Some(release_rx)),
        calls: StdMutex::new(0),
        seen: Arc::new(StdMutex::new(Vec::new())),
    };
    let steer_buf = Arc::new(StdMutex::new(VecDeque::new()));
    let (log, run) = run_engine(gate, steer_buf);
    let handle = tokio::spawn(run);
    started_rx.await.expect("首次模型调用已开始");
    release_tx.send(()).unwrap();
    handle.await.unwrap().expect("turn ok");

    let seqs = types_of(&log.lock().unwrap());
    assert!(
        !seqs.iter().any(|(t, _)| t == "agent/inbox/spliced"),
        "无 steer 不得出现 splice"
    );
}
