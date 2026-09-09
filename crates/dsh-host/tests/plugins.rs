//! 插件生命周期测试 + title 插件 + 事务替换实验计时。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dsh_host::bus::{EventBus, ListenerResult, Stop};
use dsh_host::config::{DshConfig, validate_config};
use dsh_host::plugins::{PluginError, PluginRegistry, RegistryState};
use serde_json::json;

/// title 插件:首个用户消息派生会话标题
///
/// 演示完整的插件形态:配置(schema 校验)→ 订阅总线 → 状态自持 → 销毁回收。
pub struct TitlePlugin {
    /// 派生出的标题(Option:首条 user/message 到达前为 None)
    pub title: Option<String>,
}

impl TitlePlugin {
    /// 插件配置 schema(lifecycle.config-schema 的 native 对应物)
    pub fn schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": { "max_len": { "type": "number" } },
        })
    }

    /// 安装到总线:订阅 user/message emit 事件;返回标题共享句柄
    pub fn install(
        bus: &EventBus,
        config: &serde_json::Value,
        title: Arc<Mutex<Option<String>>>,
    ) -> u64 {
        validate_config(&Self::schema(), config).expect("插件配置校验");
        let max_len = config["max_len"].as_u64().unwrap_or(40) as usize;
        bus.subscribe(
            "user/message",
            0,
            Arc::new(move |payload| {
                let t = Arc::clone(&title);
                Box::pin(async move {
                    let mut title = t.lock().expect("title 锁中毒");
                    if title.is_none()
                        && let Some(content) = payload["content"].as_str()
                    {
                        *title = Some(content.chars().take(max_len).collect());
                    }
                    Ok(ListenerResult::Continue)
                })
            }),
        )
    }
}

#[tokio::test]
async fn plugin_lifecycle_reverse_dispose_with_containment() {
    let bus = Arc::new(EventBus::new());
    let mut registry = PluginRegistry::new(Arc::clone(&bus));

    // 顺序:init A → init B;销毁回调记录顺序
    let order = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));

    let (sub_a, c1) = {
        let o = Arc::clone(&order);
        let c = Arc::clone(&calls);
        let sub = bus.subscribe(
            "e",
            0,
            Arc::new(move |_| {
                let c = Arc::clone(&c);
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(ListenerResult::Continue)
                })
            }),
        );
        (
            sub,
            Box::new(move || {
                o.lock().unwrap().push("A");
                Ok(())
            }) as Box<dyn FnOnce() -> Result<(), String> + Send>,
        )
    };
    registry
        .register("a", vec![("e".to_string(), sub_a)], Some(c1))
        .unwrap();

    let (sub_b, dispose_b_fails) = {
        let o = Arc::clone(&order);
        let sub = bus.subscribe(
            "e",
            0,
            Arc::new(|_| Box::pin(async { Ok(ListenerResult::Continue) })),
        );
        (
            sub,
            Box::new(move || {
                o.lock().unwrap().push("B");
                Err("boom".into())
            }) as Box<dyn FnOnce() -> Result<(), String> + Send>,
        )
    };
    registry
        .register("b", vec![("e".to_string(), sub_b)], Some(dispose_b_fails))
        .unwrap();

    // 销毁前事件可达
    bus.emit("e", json!({})).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // 逆序销毁:B 先于 A;B 失败被包含(A 仍销毁)
    let errors = registry.dispose_all();
    assert_eq!(
        order.lock().unwrap().as_slice(),
        &["B".to_string(), "A".to_string()],
        "销毁必须注册逆序"
    );
    assert_eq!(errors.len(), 1, "B 的销毁失败被包含");
    assert!(matches!(
        &errors[0],
        PluginError::DisposeFailed(name, _) if name == "b"
    ));
    assert_eq!(registry.state(), RegistryState::Destroyed);

    // 订阅随插件销毁退订
    bus.emit("e", json!({})).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "销毁后不再分发");

    // disposing/destroyed 拒绝新注册(继承 UNLOADING 语义)
    assert_eq!(
        registry.register("late", vec![], None),
        Err(PluginError::Refused)
    );
}

#[tokio::test]
async fn title_plugin_derives_title_from_first_user_message() {
    let bus = Arc::new(EventBus::new());
    let mut registry = PluginRegistry::new(Arc::clone(&bus));

    // 插件配置(schema 校验)+ 订阅 + 注册
    let title = Arc::new(Mutex::new(None));
    let sub = TitlePlugin::install(&bus, &json!({ "max_len": 10 }), Arc::clone(&title));
    registry
        .register("title", vec![("user/message".to_string(), sub)], None)
        .unwrap();

    bus.emit(
        "user/message",
        json!({ "content": "这是一个非常长的会话标题会被截断" }),
    )
    .await;
    bus.emit("user/message", json!({ "content": "第二条不该覆盖标题" }))
        .await;

    let derived = title.lock().unwrap().clone();
    assert_eq!(
        derived.as_deref(),
        Some("这是一个非常长的会话"), // take(10)
        "首条用户消息截断派生,后续不覆盖"
    );
    registry.dispose_all();
}

#[tokio::test]
async fn arena_replacement_experiment_timing() {
    // 竞技场级事务替换实验:dispose → 重放 → 重建的成本度量。
    // 事务性依据:重放确定性(已在 session_component 测试锁定),
    // 此处度量 O(会话长度) 的实际数字(输出仅作参考)。
    use dsh_session::{EventEnvelope, EventLog};

    for n in [100usize, 1000, 5000] {
        let mut log = EventLog::new();
        for i in 0..n {
            log.append(EventEnvelope::new(
                if i % 2 == 0 {
                    "user/message"
                } else {
                    "assistant/message"
                },
                i as i64,
                json!({ "content": format!("m{i}") }),
            ))
            .unwrap();
        }
        let started = std::time::Instant::now();
        let snapshot = log.snapshot();
        let rebuilt = EventLog::from_snapshot(&snapshot).expect("重建");
        assert_eq!(rebuilt.high_water(), n as u64, "重放完整");
        let elapsed = started.elapsed();
        eprintln!(
            "[WP4.4] 事务替换重放成本:n={n} events → {} μs (快照 {} bytes)",
            elapsed.as_micros(),
            serde_json::to_string(&snapshot).unwrap().len()
        );
        // 宽松上界:线性成本,防回归成平方
        assert!(
            elapsed.as_millis() < ((n as u64 / 10).max(50)) as u128,
            "重放成本应线性:n={n} 耗时 {elapsed:?}"
        );
    }
}

#[test]
fn retry_plugin_config_shape() {
    // retry 插件的配置契约(与 bus.rs 的重试语义测试对应)
    let schema = json!({
        "type": "object",
        "required": ["max_attempts"],
        "properties": { "max_attempts": { "type": "number" } },
    });
    assert!(validate_config(&schema, &json!({ "max_attempts": 3 })).is_ok());
    assert!(DshConfig::schema()["type"] == "object");
    // 安抚 Stop 未使用导入告警(类型在其它套件使用)
    let _ = Stop::Veto;
}
