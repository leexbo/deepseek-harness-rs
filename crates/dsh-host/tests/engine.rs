//! 组件管理器测试:Engine/InstancePre 池与竞技场隔离 + epoch 硬停。
//!
//! 核心断言:
//! 同一组件的两个 Store 并发实例化互不渗透——组件代码经 InstancePre 共享,
//! 会话状态各归各的 Arena。
//!
//! epoch 硬停用例自 tests/process.rs 迁入(E4 属引擎能力;沙箱簇已拆出
//! dsh-sandbox crate,process/pty 测试随迁)。

use dsh_host::hello::{HELLO_WAT, Hello};
use dsh_host::{Arena, HostEngine};

#[tokio::test]
async fn two_arenas_share_code_isolate_state() {
    let engine = HostEngine::new().expect("engine");
    engine
        .register("hello", HELLO_WAT.as_bytes())
        .expect("register");

    // 并发建两个竞技场:同一 InstancePre,各自 Store
    let (a1, a2) = tokio::join!(Arena::new(&engine, "hello"), Arena::new(&engine, "hello"));
    let mut a1 = a1.expect("arena 1");
    let mut a2 = a2.expect("arena 2");

    // 各自独立调用组件函数,结果一致(共享代码)、状态隔离(各自 Store)
    let v1 = call_version(&mut a1).await;
    let v2 = call_version(&mut a2).await;
    assert_eq!(v1, v2);
    assert_eq!(dsh_host::hello::decode_semver(v1), "0.1.0");
}

#[tokio::test]
async fn unregistered_component_rejected() {
    let engine = HostEngine::new().expect("engine");
    let err = Arena::new(&engine, "nope")
        .await
        .err()
        .expect("should fail");
    assert!(matches!(
        err,
        dsh_host::ArenaError::Engine(dsh_host::EngineError::NotRegistered(_))
    ));
}

async fn call_version(arena: &mut Arena) -> u32 {
    let instance = *arena.instance("hello").expect("instance");
    let hello = Hello::new(arena.store_mut(), &instance).expect("wrap");
    hello.call_version(arena.store_mut()).expect("call version")
}

#[tokio::test]
async fn epoch_hard_stops_runaway_component() {
    // E4:死循环组件被 epoch 硬停(比 exec.signal 更强的最终手段)
    use std::time::Duration;

    const SPIN_WAT: &str = r#"
(component
  (core module $m
    (func (export "spin") (loop br 0))
  )
  (core instance $i (instantiate $m))
  (func (export "spin")
    (canon lift (core func $i "spin"))
  )
)
"#;
    let engine = HostEngine::new().expect("engine");
    engine
        .register("spin", SPIN_WAT.as_bytes())
        .expect("register");
    let mut arena = Arena::new(&engine, "spin").await.expect("instantiate");
    let instance = *arena.instance("spin").expect("instance");
    let func = instance
        .get_func(arena.store_mut(), "spin")
        .expect("spin export");
    arena.set_epoch_deadline(1); // 1 个 epoch(≤20ms)后硬停
    let started = std::time::Instant::now();
    let result = func.call_async(arena.store_mut(), &[], &mut []).await;
    let err = result.expect_err("死循环必须被硬停");
    let interrupted = matches!(
        err.downcast_ref::<wasmtime::Trap>(),
        Some(wasmtime::Trap::Interrupt)
    ) || err.to_string().contains("epoch");
    assert!(interrupted, "应为 epoch 中断 trap,got: {err}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "硬停应在秒级内发生"
    );
}
