//! 持久化后端测试:JSONL 主格式 + turso 派生索引的往返、守卫与重建。

use dsh_host::{JsonlBackend, TursoBackend};
use dsh_session::envelope::decode_envelope;
use dsh_session::{EventEnvelope, EventLog};

/// 真实流:先经 EventLog.append 分配连续 seq,再持久化已提交信封
fn committed(envelopes: Vec<EventEnvelope>) -> Vec<EventEnvelope> {
    let mut log = EventLog::new();
    for ev in envelopes {
        log.append(ev).expect("log append");
    }
    log.iter().cloned().collect()
}

fn user_msg(content: &str, time: i64) -> EventEnvelope {
    EventEnvelope::new(
        "user/message",
        time,
        serde_json::json!({ "content": content }),
    )
}

#[tokio::test]
async fn jsonl_roundtrip_with_guard() {
    let dir = std::env::temp_dir().join(format!("dsh-test-jsonl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.jsonl");

    let committed = committed(vec![
        user_msg("a", 0),
        EventEnvelope::new(
            "assistant/message",
            1,
            serde_json::json!({ "content": "b" }),
        ),
    ]);
    let backend = JsonlBackend::create(&path).unwrap();
    for ev in &committed {
        backend.append(ev).unwrap();
    }

    // 往返:读取等值
    let loaded = backend.load().unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].r#type, "user/message");
    assert_eq!(loaded[0].seq, 1);
    assert_eq!(loaded[1].seq, 2);

    // 守卫:手工注入未知未标事件 → 整份日志被拒
    drop(backend);
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"user/message","seq":1,"time":0,"data":{},"ignorable":false}"#,
            "\n",
            r#"{"type":"evil/unknown","seq":2,"time":0,"data":{},"ignorable":false}"#,
            "\n",
        ),
    )
    .unwrap();
    // create 会截断;守卫路径用静态读取入口(等价 open+load)
    let err = dsh_host::persistence::jsonl::load_jsonl(&path).unwrap_err();
    assert!(err.to_string().contains("refusing"), "守卫未生效:{err}");
}

#[tokio::test]
async fn turso_roundtrip_and_rebuild_from_jsonl() {
    let dir = std::env::temp_dir().join(format!("dsh-test-turso-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("session.db");
    let jsonl_path = dir.join("session.jsonl");
    let db_path = db_path.to_str().unwrap().to_string();

    // 主格式写入(经 log 分配 seq)
    let committed = committed(vec![user_msg("a", 0), user_msg("b", 1)]);
    let jsonl = JsonlBackend::create(&jsonl_path).unwrap();
    for ev in &committed {
        jsonl.append(ev).unwrap();
    }

    // turso 索引:同一事件流 append + load 往返
    let turso_be = TursoBackend::open(&db_path).await.unwrap();
    for ev in &committed {
        turso_be.append(ev).await.unwrap();
    }
    let loaded = turso_be.load().await.unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].data["content"], "a");

    // 重建路径:清空后从主格式全量重灌(引擎级问题后的恢复动作)
    turso_be.replace_all(&[]).await.unwrap();
    assert_eq!(turso_be.load().await.unwrap().len(), 0);
    let from_jsonl = jsonl.load().unwrap();
    turso_be.replace_all(&from_jsonl).await.unwrap();
    assert_eq!(turso_be.load().await.unwrap(), from_jsonl);

    // 守卫一致性:伪造未知未标信封 JSON 直接入库 → load 拒绝
    // (blob 内守卫与 JSONL 行级守卫同一实现 decode_envelope)
    let evil = serde_json::json!({
        "type": "evil/unknown", "seq": 9, "time": 0, "data": {}, "ignorable": false
    });
    assert!(decode_envelope(&evil).is_err());
}

#[tokio::test]
async fn jsonl_and_turso_same_envelope_stream() {
    // 双后端对同一事件流产出同一读取结果(LogBackend 语义一致性)
    let dir = std::env::temp_dir().join(format!("dsh-test-dual-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jsonl = JsonlBackend::create(dir.join("a.jsonl")).unwrap();
    let turso_be = TursoBackend::open(dir.join("a.db").to_str().unwrap())
        .await
        .unwrap();

    let events = [
        user_msg("1", 0),
        EventEnvelope::new(
            "assistant/message",
            1,
            serde_json::json!({ "content": "2" }),
        ),
        EventEnvelope::new_ignorable("assistant/chunk", 2, serde_json::json!({ "delta": "x" })),
    ];
    let events = committed(events.to_vec());
    for ev in &events {
        jsonl.append(ev).unwrap();
        turso_be.append(ev).await.unwrap();
    }
    assert_eq!(jsonl.load().unwrap(), turso_be.load().await.unwrap());
}
