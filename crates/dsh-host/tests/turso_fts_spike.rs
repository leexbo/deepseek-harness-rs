//! turso FTS 支持性验证 spike。
//! turso 的 FTS **不是** SQLite FTS5 虚表,而是自研索引方法:
//! `CREATE INDEX ... USING fts (列) WITH (tokenizer='...')`(tantivy 后端),
//! 查询用元组 MATCH。分词器五选(default/raw/simple/whitespace/ngram),
//! ngram(2-3)对中文子串友好。本 spike 验证:CJK 子串/整词、英文前缀、
//! 单字限制、highlight 函数。

use std::time::{SystemTime, UNIX_EPOCH};

fn rand_tag() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
}

async fn tmp_db(tag: &str) -> (turso::Connection, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "dsh-fts-spike-{tag}-{}.db",
        std::process::id() * 1000 + rand_tag() as u32
    ));
    let db = turso::Builder::new_local(path.to_str().unwrap())
        .experimental_index_method(true) // fts 索引方法为实验特性
        .build()
        .await
        .expect("建库");
    let conn = db.connect().expect("连接");
    (conn, path)
}

async fn texts(conn: &turso::Connection, sql: &str) -> Vec<String> {
    let mut out = vec![];
    let Ok(mut rows) = conn.query(sql, ()).await else {
        eprintln!("[spike] 查询失败: {sql}");
        return out;
    };
    while let Ok(Some(row)) = rows.next().await {
        if let Ok(turso::Value::Text(t)) = row.get_value(0) {
            out.push(t.to_string());
        }
    }
    out
}

#[tokio::test]
async fn fts_ngram_cjk_and_prefix() {
    let (conn, path) = tmp_db("ngram").await;

    conn.execute(
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, content TEXT)",
        (),
    )
    .await
    .expect("建表");
    let r = conn
        .execute(
            "CREATE INDEX docs_fts ON docs USING fts (content) WITH (tokenizer='ngram')",
            (),
        )
        .await;
    assert!(r.is_ok(), "fts 索引应可建: {r:?}");

    let docs = [
        (1, "修复了队列持久化的 bug"),
        (2, "added durable queue splice replay"),
        (3, "设置页面增加了 provider 注册表"),
        (4, "provider registry with credentials"),
    ];
    for (id, d) in docs {
        conn.execute(
            &format!("INSERT INTO docs (id, content) VALUES ({id}, '{d}')"),
            (),
        )
        .await
        .expect("插入");
    }

    // 中文子串(ngram 2-3)
    let sub = texts(&conn, "SELECT content FROM docs WHERE content MATCH '持久'").await;
    eprintln!("[spike] ngram 中文子串「持久」: {sub:?}");
    assert_eq!(sub.len(), 1, "子串「持久」应命中 1 条");

    // 中文整词
    let whole = texts(&conn, "SELECT content FROM docs MATCH '队列持久化'").await;
    eprintln!("[spike] ngram 整词「队列持久化」: {whole:?}");

    // 英文前缀(tantivy 查询语法)
    let pfx = texts(
        &conn,
        "SELECT content FROM docs WHERE content MATCH 'provid*'",
    )
    .await;
    eprintln!("[spike] ngram 前缀 provid*: {pfx:?}");

    // 单字(ngram 下限 2 的预期限制)
    let single = texts(&conn, "SELECT content FROM docs WHERE content MATCH '队'").await;
    eprintln!("[spike] ngram 单字「队」: {single:?}(ngram 下限 2,单字预期不中)");

    // highlight 函数(fts_highlight)
    let hl = texts(
        &conn,
        "SELECT fts_highlight(content, '持久', '[', ']') FROM docs WHERE id = 1",
    )
    .await;
    eprintln!("[spike] highlight: {hl:?}");

    // 英文子串(ngram 对查询侧同样切 gram)
    let en = texts(
        &conn,
        "SELECT content FROM docs WHERE content MATCH 'queue'",
    )
    .await;
    eprintln!("[spike] ngram 英文子串 queue: {en:?}");

    // 多词(AND 组合)
    let multi = texts(
        &conn,
        "SELECT content FROM docs WHERE content MATCH '设置 provider'",
    )
    .await;
    eprintln!("[spike] ngram 多词「设置 provider」: {multi:?}");

    let _ = std::fs::remove_file(&path);
}
