//! turso 派生索引后端:SQLite 纯 Rust 重写的嵌入式引擎。
//!
//! 定位:查询/索引加速层,不是事实源——可随时从 JSONL 主格式全量重建
//! ([`TursoBackend::replace_all`])。async 原生(builder/connect/execute 全异步),
//! 与宿主 tokio runtime 同栈。

use dsh_session::EventEnvelope;
use dsh_session::envelope::decode_envelope;

use super::PersistenceError;

/// turso 后端(单连接;SQLite 写串行化语义,连接数非瓶颈)
pub struct TursoBackend {
    db: turso::Database,
    conn: turso::Connection,
}

/// 事件表:seq 主键 + 类型索引列 + 完整信封 JSON blob(无损)
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS events (
    seq        INTEGER PRIMARY KEY,
    type       TEXT NOT NULL,
    time       INTEGER NOT NULL,
    ignorable  INTEGER NOT NULL,
    envelope   BLOB NOT NULL
)";

impl TursoBackend {
    /// 打开(或创建)数据库并确保 schema 就绪
    pub async fn open(path: &str) -> Result<Self, PersistenceError> {
        let db = turso::Builder::new_local(path)
            .build()
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        let conn = db
            .connect()
            .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        let backend = Self { db, conn };
        backend
            .conn
            .execute(SCHEMA, ())
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        Ok(backend)
    }

    /// 底层句柄(高级查询入口,网关用)
    pub fn handle(&self) -> (&turso::Database, &turso::Connection) {
        (&self.db, &self.conn)
    }

    /// 追加单事件(信封整体入 blob,索引列冗余存储)
    pub async fn append(&self, ev: &EventEnvelope) -> Result<(), PersistenceError> {
        let envelope =
            serde_json::to_vec(ev).map_err(|e| PersistenceError::Turso(e.to_string()))?;
        self.conn
            .execute(
                "INSERT INTO events (seq, type, time, ignorable, envelope) VALUES (?, ?, ?, ?, ?)",
                (
                    turso::Value::Integer(ev.seq as i64),
                    turso::Value::Text(ev.r#type.clone()),
                    turso::Value::Integer(ev.time),
                    turso::Value::Integer(i64::from(ev.ignorable)),
                    turso::Value::Blob(envelope),
                ),
            )
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        Ok(())
    }

    /// 全量读取:按 seq 升序,信封 blob 过读取方守卫
    pub async fn load(&self) -> Result<Vec<EventEnvelope>, PersistenceError> {
        let mut rows = self
            .conn
            .query("SELECT envelope FROM events ORDER BY seq", ())
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        let mut events = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?
        {
            let blob = match row.get_value(0) {
                Ok(turso::Value::Blob(b)) => b,
                other => {
                    return Err(PersistenceError::Turso(format!(
                        "envelope 列非 blob:{other:?}"
                    )));
                }
            };
            let raw: serde_json::Value = serde_json::from_slice(&blob)
                .map_err(|e| PersistenceError::Turso(e.to_string()))?;
            events.push(decode_envelope(&raw)?);
        }
        Ok(events)
    }

    /// 索引重建路径:清空并从主格式(或任意事件源)全量重灌。
    ///
    /// 引擎级问题后的恢复动作;也是「SQLite 后端是派生索引而非事实源」的机制保证。
    pub async fn replace_all(&self, envelopes: &[EventEnvelope]) -> Result<(), PersistenceError> {
        self.conn
            .execute("DELETE FROM events", ())
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        for ev in envelopes {
            self.append(ev).await?;
        }
        Ok(())
    }
}
