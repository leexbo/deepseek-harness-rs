//! 总线语义测试。
//!
//! - 5 模式语义(emit 不等/parallel join/serial 短路/waterfall 传载)
//! - around 续体三形态:transform(调 next 传新载荷)/ 替换(不调 next,
//!   「回放插件替换整个流」验收用例)/ 包裹(调 next 后后处理)
//! - veto 传播;优先级排序;listener 级错误包含

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dsh_host::bus::{EventBus, ListenerResult, Stop};
use serde_json::json;

fn listener<F>(f: F) -> dsh_host::bus::Listener
where
    F: Fn(serde_json::Value) -> ListenerResult + Send + Sync + 'static,
{
    Arc::new(move |payload| {
        let result = f(payload);
        Box::pin(async move { Ok(result) })
    })
}

#[tokio::test]
async fn emit_serial_bail_semantics() {
    let bus = EventBus::new();
    let hits = Arc::new(AtomicUsize::new(0));

    bus.subscribe("e", 0, {
        let hits = Arc::clone(&hits);
        listener(move |_| {
            hits.fetch_add(1, Ordering::SeqCst);
            ListenerResult::Continue
        })
    });
    bus.emit("e", json!({})).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // serial:首个 Value 短路(后序 listener 不执行)
    bus.subscribe(
        "s",
        10, // 高优先级先执行
        listener(|_| ListenerResult::Value(json!({"from": "high"}))),
    );
    bus.subscribe(
        "s",
        0,
        listener(|_| ListenerResult::Value(json!({"from": "low"}))),
    );
    assert_eq!(
        bus.serial("s", json!({})).await,
        Some(json!({"from": "high"}))
    );
}

#[tokio::test]
async fn parallel_joins_all_with_errors_contained() {
    let bus = EventBus::new();
    bus.subscribe("p", 0, listener(|_| ListenerResult::Continue));
    let failing: dsh_host::bus::Listener = Arc::new(|_| Box::pin(async { Err("boom".into()) }));
    bus.subscribe("p", 0, failing);
    let results = bus.parallel("p", json!({})).await;
    assert_eq!(results.len(), 2);
    assert!(results[0].is_ok());
    assert!(results[1].is_err(), "错误包含:单个失败不中断同伴");
}

#[tokio::test]
async fn waterfall_transform_chain() {
    let bus = EventBus::new();
    bus.subscribe_around(
        "w",
        10,
        Arc::new(|payload, next| {
            Box::pin(async move {
                let mut p = payload;
                p["x"] = json!(p["x"].as_i64().unwrap_or(0) + 1);
                next.invoke(p).await
            })
        }),
    );
    bus.subscribe_around(
        "w",
        0,
        Arc::new(|payload, next| {
            Box::pin(async move {
                let mut p = payload;
                p["x"] = json!(p["x"].as_i64().unwrap_or(0) * 10);
                next.invoke(p).await
            })
        }),
    );
    let result = bus
        .waterfall("w", json!({ "x": 1 }), |p| Box::pin(async { Ok(p) }))
        .await
        .expect("chain");
    // 高优先级(+1)先,低优先级(×10)后,默认行为透传链上载荷
    assert_eq!(result, json!({ "x": 20 }));
}

#[tokio::test]
async fn around_replace_skips_rest_and_default() {
    // 验收用例:回放插件不调 next,替换整个流(默认行为 = 真实网络调用,必须被短路)
    let bus = EventBus::new();
    let default_called = Arc::new(AtomicUsize::new(0));
    let dc = Arc::clone(&default_called);
    bus.subscribe_around(
        "llm/stream",
        5,
        Arc::new(move |_payload, _next| {
            let dc = Arc::clone(&dc);
            // 不调 next:默认行为(真实网络)被替换为回放
            Box::pin(async move {
                let _ = dc; // 默认行为的调用计数在闭包外检查:此处仅确保不调 next
                Ok(json!({ "source": "replay" }))
            })
        }),
    );
    let called = Arc::clone(&default_called);
    let result = bus
        .waterfall("llm/stream", json!({}), move |_p| {
            let called = Arc::clone(&called);
            Box::pin(async move {
                called.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "source": "network" }))
            })
        })
        .await
        .expect("replaced");
    assert_eq!(result["source"], "replay");
    assert_eq!(
        default_called.load(Ordering::SeqCst),
        0,
        "默认行为(真实网络)必须被短路——原语义 llm-replay 的硬需求"
    );
}

#[tokio::test]
async fn around_wrapper_calls_next_then_postprocesses() {
    // 包裹形态:调 next,对结果后处理(超时/重试/指标的 around 语义)
    let bus = EventBus::new();
    bus.subscribe_around(
        "w",
        0,
        Arc::new(|payload, next| {
            Box::pin(async move {
                let mut result = next.invoke(payload).await?;
                result["wrapped"] = json!(true);
                Ok(result)
            })
        }),
    );
    let result = bus
        .waterfall("w", json!({ "v": 1 }), |p| Box::pin(async { Ok(p) }))
        .await
        .unwrap();
    assert_eq!(result, json!({ "v": 1, "wrapped": true }));
}

#[tokio::test]
async fn veto_propagates_to_caller() {
    let bus = EventBus::new();
    bus.subscribe_around(
        "w",
        0,
        Arc::new(|_payload, _next| Box::pin(async { Err(Stop::Veto) })),
    );
    let err = bus
        .waterfall("w", json!({}), |_p| Box::pin(async { Ok(json!(null)) }))
        .await
        .unwrap_err();
    assert_eq!(err, Stop::Veto);
}

#[tokio::test]
async fn retry_plugin_semantics_on_around() {
    // retry 插件语义:默认行为失败时重试(调 next 多次),
    // 重试次数有界;成功后结果正常返回
    let bus = EventBus::new();
    let attempts = Arc::new(AtomicUsize::new(0));
    let a = Arc::clone(&attempts);
    bus.subscribe_around(
        "transport/stream",
        10,
        Arc::new(move |payload, next| {
            Box::pin(async move {
                let max = 3;
                for _ in 0..max {
                    if let Ok(v) = next.invoke(payload.clone()).await {
                        return Ok(v);
                    }
                }
                Err(Stop::Error("retries exhausted".into()))
            })
        }),
    );
    let a2 = Arc::clone(&a);
    let result = bus
        .waterfall("transport/stream", json!({ "q": 1 }), move |_p| {
            let a2 = Arc::clone(&a2);
            Box::pin(async move {
                if a2.fetch_add(1, Ordering::SeqCst) < 2 {
                    Err("transient".to_string())
                } else {
                    Ok(json!({ "ok": true }))
                }
            })
        })
        .await
        .expect("third attempt succeeds");
    assert_eq!(result["ok"], true);
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn bus_transport_end_to_end_with_retry_plugin() {
    // 完整插件管线:engine → 闸门 → BusTransport → 总线(重试插件 around)
    //   → 内层传输(前两次瞬态失败)。第三次成功,loop 拿到结果。
    use dsh_agent_loop::{LlmEvent, LlmTransport, LoopEngine, RequestHeader, TransportError};
    use dsh_host::BusTransport;
    use dsh_llm::InvariantGate;
    use dsh_session::EventLog;
    use std::sync::Mutex as StdMutex;

    let bus = Arc::new(EventBus::new());
    bus.subscribe_around(
        "llm/stream",
        10,
        Arc::new(|payload, next| {
            Box::pin(async move {
                for _ in 0..3 {
                    if let Ok(v) = next.invoke(payload.clone()).await {
                        return Ok(v);
                    }
                }
                Err(Stop::Error("retries exhausted".into()))
            })
        }),
    );

    // 内层传输:前两次瞬态失败
    struct Flaky {
        attempts: StdMutex<usize>,
    }
    impl dsh_agent_loop::Summarizer for Flaky {
        fn summarize<'a>(
            &'a mut self,
            _header: &'a RequestHeader,
            _messages: &'a serde_json::Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
        {
            Box::pin(async { Ok(String::new()) })
        }
    }
    impl LlmTransport for Flaky {
        async fn stream(
            &mut self,
            _header: &RequestHeader,
            _messages: &serde_json::Value,
        ) -> Result<Vec<LlmEvent>, TransportError> {
            let n = {
                let mut a = self.attempts.lock().unwrap();
                *a += 1;
                *a
            };
            if n < 3 {
                Err(TransportError::Transport("transient".into()))
            } else {
                Ok(vec![LlmEvent::Chunk("recovered".into()), LlmEvent::Done])
            }
        }
    }
    let flaky = Flaky {
        attempts: StdMutex::new(0),
    };
    let bus_transport = BusTransport::new(Arc::clone(&bus), flaky);

    let log = Arc::new(StdMutex::new(EventLog::new()));
    let mut engine = LoopEngine::new(
        RequestHeader {
            model: "m".into(),
            system: String::new(),
            temperature: 0.0,
            reasoning_effort: None,
            tools: Vec::new(),
        },
        Arc::clone(&log),
    );
    let mut gate = InvariantGate::new(bus_transport, Arc::clone(&log));
    let clock = || 0_i64;
    let mut sink = |_ev: &dsh_session::EventEnvelope| {};
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
        .expect("turn with retry plugin");
    assert_eq!(outcome.assistant_message, "recovered");
}
