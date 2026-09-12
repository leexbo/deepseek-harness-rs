//! 全局检索索引(走 turso 自研 FTS)。
//!
//! 定位:派生索引(`~/.dshrs/search.db`),可由各会话 JSONL 全量重建;
//! **懒增量**——每次检索前把各会话高水位之后的事件投影进来,冷/热
//! 同一条重放路径,零侵入 engine(不挂第二 sink)。
//!
//! turso FTS 形态(spike 验证,见 tests/turso_fts_spike.rs):
//! - `features=["fts"]` + `Builder::experimental_index_method(true)`;
//! - `CREATE INDEX ... USING fts (content) WITH (tokenizer='ngram')`
//!   (tantivy 后端;2-3 gram);
//! - 查询 `WHERE content MATCH '...'`(多词 = OR)。
//!
//! 已知限制(显式记录):前缀 `provid*` 不命中、单字不命中
//! (ngram 下限 2)、`fts_highlight` 对 CJK 不标注(default 分词的独立
//! 函数,与 ngram 索引不联动)——摘要/高亮由应用层做。

use std::path::Path;

use dsh_session::EventEnvelope;
use dsh_session::envelope::decode_envelope;
use turso::Value as Tv;

use super::PersistenceError;

/// schema 语句(turso execute 单语句执行,逐条跑)
const SCHEMA: [&str; 3] = [
    "CREATE TABLE IF NOT EXISTS docs (
        session TEXT NOT NULL,
        seq     INTEGER NOT NULL,
        kind    TEXT NOT NULL,
        content TEXT NOT NULL,
        PRIMARY KEY (session, seq)
    )",
    "CREATE INDEX IF NOT EXISTS docs_fts ON docs USING fts (content) WITH (tokenizer='ngram')",
    "CREATE TABLE IF NOT EXISTS indexed (
        session TEXT PRIMARY KEY,
        seq     INTEGER NOT NULL
    )",
];

/// 全局检索索引(turso;单连接)
pub struct SearchIndex {
    conn: turso::Connection,
}

/// 一条检索命中(按会话/seq 升序)
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// 会话 id(ws/stem)
    pub session: String,
    /// 事件 seq(结果跳转定位锚)
    pub seq: u64,
    /// 文档种类(user / assistant / tool)
    pub kind: String,
    /// 命中文档全文(摘要由调用层截取)
    pub content: String,
}

/// 事件 → 可检索文档投影(纯函数;None = 不进索引)。
/// user/assistant 对话文本 + 工具调用(名 + 参数)——与源 session-query
/// 的可检索面同构
pub fn project(ev: &EventEnvelope) -> Option<(&'static str, String)> {
    let d = &ev.data;
    // content 两态:块数组([{type:text,text}…])或纯字符串(引擎落档
    // 的 user/message 即纯字符串)
    let text = |v: &serde_json::Value| -> String {
        match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b["type"].as_str() == Some("text"))
                .map(|b| b["text"].as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        }
    };
    match ev.r#type.as_str() {
        "user/message" => {
            // 注入上下文(user/message + source.kind != "user")不进索引:
            // 注入是旁车上下文,不占可检索用户消息面
            if d["source"]["kind"].as_str().unwrap_or("user") != "user" {
                return None;
            }
            let t = text(&d["content"]);
            (!t.is_empty()).then_some(("user", t))
        }
        "assistant/message" => {
            // 引擎落档:content 顶层;块数组形态(翻译面)兜底
            let t = if d["content"].is_string() {
                text(&d["content"])
            } else {
                text(&d["message"]["content"])
            };
            (!t.is_empty()).then_some(("assistant", t))
        }
        "tool/call" => {
            let name = d["name"].as_str().unwrap_or_default();
            let args = d["arguments"].as_str().unwrap_or_default();
            (!name.is_empty() || !args.is_empty()).then_some(("tool", format!("{name} {args}")))
        }
        _ => None,
    }
}

impl SearchIndex {
    /// 打开(或创建)索引库并确保 schema 就绪
    pub async fn open(path: &str) -> Result<Self, PersistenceError> {
        let db = turso::Builder::new_local(path)
            .experimental_index_method(true) // fts 索引方法为实验特性(spike 结论)
            .build()
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        let conn = db
            .connect()
            .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        for stmt in SCHEMA {
            conn.execute(stmt, ())
                .await
                .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        }
        Ok(Self { conn })
    }

    /// 单会话增量投影:读 JSONL,把高水位之后可投影的事件写入 docs
    /// 并推进水位。幂等——重复同步无重复行(主键 (session, seq))。
    pub async fn sync_session(
        &self,
        session: &str,
        log_path: &Path,
    ) -> Result<u64, PersistenceError> {
        let watermark = self.watermark(session).await?;
        let text = match std::fs::read_to_string(log_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(watermark),
            Err(e) => return Err(PersistenceError::Io(e)),
        };
        let mut latest = watermark;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let raw: serde_json::Value = serde_json::from_str(line)
                .map_err(|e| PersistenceError::Turso(format!("{session}: 行解析失败 {e}")))?;
            let ev = decode_envelope(&raw)?;
            if ev.seq <= watermark {
                continue;
            }
            latest = latest.max(ev.seq);
            if let Some((kind, content)) = project(&ev) {
                self.conn
                    .execute(
                        "INSERT OR REPLACE INTO docs (session, seq, kind, content) VALUES (?, ?, ?, ?)",
                        (
                            Tv::Text(session.into()),
                            Tv::Integer(ev.seq as i64),
                            Tv::Text(kind.into()),
                            Tv::Text(content),
                        ),
                    )
                    .await
                    .map_err(|e| PersistenceError::Turso(e.to_string()))?;
            }
        }
        if latest > watermark {
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO indexed (session, seq) VALUES (?, ?)",
                    (Tv::Text(session.into()), Tv::Integer(latest as i64)),
                )
                .await
                .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        }
        Ok(latest)
    }

    /// 会话已索引水位(0 = 未索引)
    async fn watermark(&self, session: &str) -> Result<u64, PersistenceError> {
        let sql = format!(
            "SELECT seq FROM indexed WHERE session = '{}'",
            session.replace('\'', "''")
        );
        let mut rows = self
            .conn
            .query(&sql, ())
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        if let Some(row) = rows
            .next()
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?
            && let Ok(Tv::Integer(seq)) = row.get_value(0)
        {
            return Ok(seq.max(0) as u64);
        }
        Ok(0)
    }

    /// 索引中的会话清单(与磁盘清单对账用)
    pub async fn sessions(&self) -> Result<Vec<String>, PersistenceError> {
        let mut rows = self
            .conn
            .query("SELECT session FROM indexed", ())
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?
        {
            if let Ok(Tv::Text(s)) = row.get_value(0) {
                out.push(s);
            }
        }
        Ok(out)
    }

    /// 会话索引整体移除(docs + 水位;幂等)。会话删除不经过索引路径,
    /// 检索前的对账把磁盘已消失的会话在此收口
    pub async fn remove_session(&self, session: &str) -> Result<(), PersistenceError> {
        let esc = session.replace('\'', "''");
        for sql in [
            format!("DELETE FROM docs WHERE session = '{esc}'"),
            format!("DELETE FROM indexed WHERE session = '{esc}'"),
        ] {
            self.conn
                .execute(&sql, ())
                .await
                .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        }
        Ok(())
    }

    /// 全文检索(命中按会话/seq 升序;多词 = OR,turso MATCH 语义)
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, PersistenceError> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(vec![]);
        }
        let sql = format!(
            "SELECT session, seq, kind, content FROM docs WHERE content MATCH '{}' \
             ORDER BY session, seq LIMIT {}",
            q.replace('\'', "''"),
            limit
        );
        let mut rows = self
            .conn
            .query(&sql, ())
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?;
        let mut hits = vec![];
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| PersistenceError::Turso(e.to_string()))?
        {
            let (
                Ok(Tv::Text(session)),
                Ok(Tv::Integer(seq)),
                Ok(Tv::Text(kind)),
                Ok(Tv::Text(content)),
            ) = (
                row.get_value(0),
                row.get_value(1),
                row.get_value(2),
                row.get_value(3),
            )
            else {
                continue;
            };
            hits.push(SearchHit {
                session: session.to_string(),
                seq: seq.max(0) as u64,
                kind: kind.to_string(),
                content: content.to_string(),
            });
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::EventLog;

    fn tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "dsh-search-{tag}-{}-{}",
            std::process::id(),
            dsh_uuid()
        ))
    }

    fn dsh_uuid() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0)
    }

    /// 建库 + 写两个会话的 JSONL(user/assistant/tool 事件)→ 同步 → 检索
    #[tokio::test]
    async fn project_sync_search_roundtrip() {
        let dir = tmp("rt");
        std::fs::create_dir_all(&dir).unwrap();
        let log_a = dir.join("a.jsonl");
        let log_b = dir.join("b.jsonl");

        let mut log = EventLog::new();
        log.append(EventEnvelope::new(
            "user/message",
            1,
            serde_json::json!({
                "content": [ { "type": "text", "text": "修复队列持久化的方案" } ]
            }),
        ))
        .unwrap();
        log.append(EventEnvelope::new("assistant/message", 2, serde_json::json!({
            "message": { "content": [ { "type": "text", "text": "方案:durable splice replay" } ] }
        })))
        .unwrap();
        log.append(EventEnvelope::new(
            "tool/call",
            3,
            serde_json::json!({
                "callId": "c1", "name": "bash", "arguments": "{\"command\":\"ls\"}"
            }),
        ))
        .unwrap();
        std::fs::write(&log_a, log.iter().map(envelope_line).collect::<String>()).unwrap();

        let mut log2 = EventLog::new();
        log2.append(EventEnvelope::new(
            "user/message",
            1,
            serde_json::json!({
                "content": [ { "type": "text", "text": "provider 注册表怎么配" } ]
            }),
        ))
        .unwrap();
        std::fs::write(&log_b, log2.iter().map(envelope_line).collect::<String>()).unwrap();

        let index = SearchIndex::open(dir.join("search.db").to_str().unwrap())
            .await
            .expect("建索引");
        index.sync_session("ws/a", &log_a).await.expect("同步 a");
        index.sync_session("ws/b", &log_b).await.expect("同步 b");

        // 中文子串
        let hits = index.search("持久", 10).await.expect("检索");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session, "ws/a");
        assert_eq!(hits[0].seq, 1);
        assert_eq!(hits[0].kind, "user");

        // 英文子串
        let hits = index.search("splice", 10).await.expect("检索");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 2);
        assert_eq!(hits[0].kind, "assistant");

        // 工具调用(名 + 参数)
        let hits = index.search("bash", 10).await.expect("检索");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 3);
        assert_eq!(hits[0].kind, "tool");

        // 多词 OR
        let hits = index.search("注册表 持久", 10).await.expect("检索");
        assert_eq!(hits.len(), 2, "两会话各中一条");

        // 增量:追写新事件后再同步,只有新增进索引且幂等
        let mut log_more = EventLog::new();
        for ev in log.iter() {
            log_more.append(ev.clone()).unwrap();
        }
        log_more
            .append(EventEnvelope::new(
                "user/message",
                4,
                serde_json::json!({
                    "content": [ { "type": "text", "text": "增量补充凭据链路" } ]
                }),
            ))
            .unwrap();
        std::fs::write(
            &log_a,
            log_more.iter().map(envelope_line).collect::<String>(),
        )
        .unwrap();
        index.sync_session("ws/a", &log_a).await.expect("再同步");
        index
            .sync_session("ws/a", &log_a)
            .await
            .expect("幂等再同步");
        let hits = index.search("凭据链路", 10).await.expect("检索");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 4);
        let hits = index.search("持久", 10).await.expect("检索");
        assert_eq!(hits.len(), 1, "旧文档不重复");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 会话删除收口:remove_session 整会话移除(docs + 水位),检索
    /// 不再命中;sessions() 列出的对账清单同步收敛
    #[tokio::test]
    async fn remove_session_drops_docs_and_watermark() {
        let dir = tmp("rm");
        std::fs::create_dir_all(&dir).unwrap();
        let log_a = dir.join("a.jsonl");
        let mut log = EventLog::new();
        log.append(EventEnvelope::new(
            "user/message",
            1,
            serde_json::json!({
                "content": [ { "type": "text", "text": "修复队列持久化的方案" } ]
            }),
        ))
        .unwrap();
        std::fs::write(&log_a, log.iter().map(envelope_line).collect::<String>()).unwrap();

        let index = SearchIndex::open(dir.join("search.db").to_str().unwrap())
            .await
            .expect("建索引");
        index.sync_session("ws/a", &log_a).await.expect("同步");
        assert_eq!(index.search("持久", 10).await.unwrap().len(), 1);
        assert_eq!(index.sessions().await.unwrap(), vec!["ws/a".to_string()]);

        index.remove_session("ws/a").await.expect("移除");
        assert!(
            index.search("持久", 10).await.unwrap().is_empty(),
            "已删会话不再命中"
        );
        assert!(index.sessions().await.unwrap().is_empty(), "对账清单收敛");
        // 幂等:重复移除无害
        index.remove_session("ws/a").await.expect("幂等移除");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// envelope → JSONL 行
    fn envelope_line(ev: &EventEnvelope) -> String {
        format!("{}\n", serde_json::to_string(ev).unwrap())
    }
}
