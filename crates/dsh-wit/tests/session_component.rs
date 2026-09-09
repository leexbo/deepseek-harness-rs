//! session 组件契约测试(三层测试之二:组件契约)。
//!
//! wasmtime 实际实例化 `wasm32-wasip2` 产物,验证 world 满足 + 接口语义往返:
//! event-log 的 append/get/query/snapshot + seq 连续拒绝 + 投影三件套。
//! 不变式(seq 守卫)与确定性重放是必测项(硬性原则 3)。
//!
//! 调用约定:宿主 call_* 返回 `Result<WIT 返回值, wasmtime::Error>`(外层 trap,
//! 内层为 WIT result 类型),断言需双层展开。

use std::path::PathBuf;
use std::process::Command;

use dsh_host::{Arena, HostEngine};
use dsh_wit::session::exports::dsh::session::event_log::Event;

/// 定位组件产物;缺失时现场构建(保证 `cargo test --workspace` 自洽)
fn session_wasm() -> PathBuf {
    if let Ok(p) = std::env::var("DSH_SESSION_WASM") {
        return PathBuf::from(p);
    }
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/debug/dsh_session.wasm");
    if !p.exists() {
        let status = Command::new("cargo")
            .args(["build", "-p", "dsh-session", "--target", "wasm32-wasip2"])
            .status()
            .expect("spawn cargo");
        assert!(status.success(), "dsh-session wasm 构建失败");
    }
    p
}

fn wit_event(type_: &str, data: serde_json::Value, seq: u64) -> Event {
    Event {
        type_: type_.to_string(),
        seq,
        time: 0,
        data: serde_json::to_vec(&data).expect("serialize data"),
        surface_op: None,
        source_event_seqs: None,
        ignorable: false,
    }
}

fn user_event(content: &str, seq: u64) -> Event {
    wit_event(
        "user/message",
        serde_json::json!({ "content": content }),
        seq,
    )
}

async fn setup() -> (HostEngine, Arena) {
    let engine = HostEngine::new().expect("engine");
    let wasm = std::fs::read(session_wasm()).expect("read component");
    engine.register("session", &wasm).expect("register");
    let arena = Arena::new(&engine, "session").await.expect("instantiate");
    (engine, arena)
}

#[tokio::test]
async fn event_log_roundtrip_and_seq_guard() {
    let (_engine, mut arena) = setup().await;
    let instance = *arena.instance("session").expect("instance");
    let session = dsh_wit::session::Session::new(arena.store_mut(), &instance).expect("world wrap");
    let log = session.dsh_session_event_log();

    // append 自动分配 seq(内层 Ok)
    assert_eq!(
        log.call_append(arena.store_mut(), &user_event("hi", 0))
            .expect("call")
            .expect("append"),
        1
    );
    assert_eq!(
        log.call_append(arena.store_mut(), &user_event("again", 0))
            .expect("call")
            .expect("append"),
        2
    );

    // get:往返后信封字段保真
    let got = log
        .call_get(arena.store_mut(), 1)
        .expect("call")
        .expect("get ok")
        .expect("seq 1 exists");
    assert_eq!(got.type_, "user/message");
    assert_eq!(got.seq, 1);
    let data: serde_json::Value = serde_json::from_slice(&got.data).expect("decode data");
    assert_eq!(data["content"], "hi");

    // seq 连续强制:跳号被拒(内层 Err)
    let err = log
        .call_append(arena.store_mut(), &user_event("gap", 9))
        .expect("call")
        .expect_err("gap must be rejected");
    assert!(err.contains("not contiguous"), "got: {err}");

    // query 过滤
    let hits = log
        .call_query(arena.store_mut(), Some("user/message"))
        .expect("query");
    assert_eq!(hits.len(), 2);

    // snapshot 含格式版本与全部事件
    let snap = log
        .call_snapshot(arena.store_mut())
        .expect("call")
        .expect("snapshot");
    let snap: serde_json::Value = serde_json::from_slice(&snap).expect("decode snapshot");
    assert_eq!(snap["version"], dsh_session::SESSION_FORMAT_VERSION);
    assert_eq!(snap["events"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn projection_fold_and_view() {
    let (_engine, mut arena) = setup().await;
    let instance = *arena.instance("session").expect("instance");
    let session = dsh_wit::session::Session::new(arena.store_mut(), &instance).expect("world wrap");
    let proj = session.dsh_session_projection();

    let state = proj
        .call_init(arena.store_mut(), &br#"{}"#.to_vec())
        .expect("call")
        .expect("init");
    let state = proj
        .call_apply(arena.store_mut(), &state, &user_event("hi", 1))
        .expect("call")
        .expect("apply 1");
    let state = proj
        .call_apply(
            arena.store_mut(),
            &state,
            &wit_event(
                "assistant/message",
                serde_json::json!({ "content": "hello" }),
                2,
            ),
        )
        .expect("call")
        .expect("apply 2");
    // 非消息事件不进入投影
    let state = proj
        .call_apply(
            arena.store_mut(),
            &state,
            &wit_event("turn/start", serde_json::json!({}), 3),
        )
        .expect("call")
        .expect("apply 3");

    let view = proj
        .call_view(arena.store_mut(), &state)
        .expect("call")
        .expect("view");
    let messages: serde_json::Value = serde_json::from_slice(&view).expect("decode view");
    assert_eq!(
        messages,
        serde_json::json!([
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": "hello" },
        ]),
        "投影保序折叠消息面事件,忽略非消息事件"
    );
}

#[tokio::test]
async fn replay_determinism_across_instances() {
    // 重放基础设施:实例 A 的快照在全新实例 B 按序重放,状态 bit-exact 相等。
    // 这是竞技场级事务替换(E2)与 crash 恢复的底座。
    let (engine, mut arena_a) = setup().await;
    let instance = *arena_a.instance("session").expect("instance");
    let session_a = dsh_wit::session::Session::new(arena_a.store_mut(), &instance).expect("wrap");
    let log_a = session_a.dsh_session_event_log();

    for ev in [
        user_event("hello", 0),
        wit_event(
            "assistant/message",
            serde_json::json!({ "content": "world" }),
            0,
        ),
        wit_event("turn/end", serde_json::json!({}), 0),
    ] {
        log_a
            .call_append(arena_a.store_mut(), &ev)
            .expect("call")
            .expect("append");
    }
    let snapshot = log_a
        .call_snapshot(arena_a.store_mut())
        .expect("call")
        .expect("snapshot");
    let snapshot: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();

    // 全新竞技场(新 Store、新实例)重放
    let mut arena_b = Arena::new(&engine, "session").await.expect("instantiate");
    let instance_b = *arena_b.instance("session").expect("instance");
    let session_b = dsh_wit::session::Session::new(arena_b.store_mut(), &instance_b).expect("wrap");
    let log_b = session_b.dsh_session_event_log();
    for raw in snapshot["events"].as_array().expect("events") {
        let ev = Event {
            type_: raw["type"].as_str().expect("type").to_string(),
            seq: raw["seq"].as_u64().expect("seq"),
            time: raw["time"].as_i64().expect("time"),
            data: serde_json::to_vec(&raw["data"]).expect("data"),
            surface_op: None,
            source_event_seqs: None,
            ignorable: raw["ignorable"].as_bool().unwrap_or(false),
        };
        log_b
            .call_append(arena_b.store_mut(), &ev)
            .expect("call")
            .expect("replay append");
    }
    let snapshot_b = log_b
        .call_snapshot(arena_b.store_mut())
        .expect("call")
        .expect("snapshot");
    let snapshot_b: serde_json::Value = serde_json::from_slice(&snapshot_b).unwrap();
    assert_eq!(snapshot, snapshot_b, "跨实例重放必须 bit-exact");
}
