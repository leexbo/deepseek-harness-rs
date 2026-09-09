//! session 组件的 guest 实现:WIT 接口 → 本 crate rlib 逻辑的适配层。
//!
//! 组件内不读时钟/随机(信封 time 由宿主在调用前注入)——重放确定性的前提。
//! 状态经 `static Mutex<EventLog>` 持有:单实例单 Store,宿主竞技场即隔离边界。

use std::sync::Mutex;

use crate::envelope::EventEnvelope;
use crate::exports::dsh::session::{event_log, projection};
use crate::log::EventLog;

static LOG: Mutex<Option<EventLog>> = Mutex::new(None);

fn with_log<T>(f: impl FnOnce(&mut EventLog) -> Result<T, String>) -> Result<T, String> {
    let mut guard = LOG
        .lock()
        .map_err(|_| "session log lock poisoned (guest bug)".to_string())?;
    let log = guard.get_or_insert_with(EventLog::new);
    f(log)
}

/// WIT event(json 为序列化字节)→ 信封
fn wit_event_to_envelope(ev: &event_log::Event) -> Result<EventEnvelope, String> {
    let data: serde_json::Value = serde_json::from_slice(&ev.data)
        .map_err(|e| format!("event data is not valid JSON: {e}"))?;
    Ok(EventEnvelope {
        r#type: ev.type_.clone(),
        seq: ev.seq,
        time: ev.time,
        data,
        surface_op: ev.surface_op.clone(),
        source_event_seqs: ev.source_event_seqs.clone(),
        ignorable: ev.ignorable,
    })
}

/// 信封 → WIT event 记录
fn envelope_to_wit_event(ev: &EventEnvelope) -> event_log::Event {
    event_log::Event {
        type_: ev.r#type.clone(),
        seq: ev.seq,
        time: ev.time,
        data: serde_json::to_vec(&ev.data).expect("data 序列化不可失败"),
        surface_op: ev.surface_op.clone(),
        source_event_seqs: ev.source_event_seqs.clone(),
        ignorable: ev.ignorable,
    }
}

/// 组件导出实现
pub struct SessionComponent;

impl event_log::Guest for SessionComponent {
    fn append(ev: event_log::Event) -> Result<u64, String> {
        let envelope = wit_event_to_envelope(&ev)?;
        with_log(|log| log.append(envelope).map_err(|e| e.to_string()))
    }

    fn get(seq: u64) -> Result<Option<event_log::Event>, String> {
        with_log(|log| Ok(log.get(seq).map(envelope_to_wit_event)))
    }

    fn query(type_filter: Option<String>) -> Vec<event_log::Event> {
        with_log(|log| {
            Ok(log
                .query(type_filter.as_deref())
                .into_iter()
                .map(envelope_to_wit_event)
                .collect())
        })
        .unwrap_or_default()
    }

    fn snapshot() -> Result<Vec<u8>, String> {
        with_log(|log| Ok(serde_json::to_vec(&log.snapshot()).expect("快照序列化不可失败")))
    }
}

impl projection::Guest for SessionComponent {
    fn init(schema: Vec<u8>) -> Result<Vec<u8>, String> {
        let schema: serde_json::Value =
            serde_json::from_slice(&schema).map_err(|e| format!("schema: {e}"))?;
        serde_json::to_vec(&serde_json::json!({
            "schema": schema,
            "messages": [],
        }))
        .map_err(|e| e.to_string())
    }

    fn apply(state: Vec<u8>, ev: event_log::Event) -> Result<Vec<u8>, String> {
        let mut state: serde_json::Value =
            serde_json::from_slice(&state).map_err(|e| format!("state: {e}"))?;
        let envelope = wit_event_to_envelope(&ev)?;
        // 投影折叠 = 唯一权威规则 message_from_event(与 derive_messages 同源)
        if let Some(message) = crate::events::message_from_event(&envelope.r#type, &envelope.data) {
            state["messages"]
                .as_array_mut()
                .ok_or("projection state missing messages array")?
                .push(message);
        }
        serde_json::to_vec(&state).map_err(|e| e.to_string())
    }

    fn view(state: Vec<u8>) -> Result<Vec<u8>, String> {
        let state: serde_json::Value =
            serde_json::from_slice(&state).map_err(|e| format!("state: {e}"))?;
        Ok(serde_json::to_vec(&state["messages"].clone()).expect("序列化不可失败"))
    }
}
