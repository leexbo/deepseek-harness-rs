//! JSONL 主格式后端:追加式事实流。
//!
//! 长会话日志流式落盘、不进组件内存(重写方案 §7);每事件一行、写后即 flush
//! (正确性优先:append-only 日志的崩溃边界 = 行边界)。

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use dsh_session::EventEnvelope;
use dsh_session::envelope::decode_envelope;

use super::PersistenceError;

/// JSONL 落盘后端(线程安全:单写者经互斥锁串行化)。
/// Clone = 共享同一文件句柄(durable 队列改造:泵任务与驱动任务
/// 各持克隆落 splice/turn 事件,行完整性由写者互斥保证)
#[derive(Clone)]
pub struct JsonlBackend {
    path: PathBuf,
    writer: std::sync::Arc<Mutex<BufWriter<File>>>,
}

impl JsonlBackend {
    /// 新建(截断既有文件)——新会话的事实流起点
    pub fn create(path: impl Into<PathBuf>) -> Result<Self, PersistenceError> {
        let path = path.into();
        let file = File::create(&path)?;
        Ok(Self {
            path,
            writer: std::sync::Arc::new(Mutex::new(BufWriter::new(file))),
        })
    }

    /// 打开(或创建)既有会话日志:追加模式,不截断——重开会话的
    /// 正确语义(load_log 重载历史后继续 append)
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, PersistenceError> {
        let path = path.into();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            writer: std::sync::Arc::new(Mutex::new(BufWriter::new(file))),
        })
    }

    /// 日志文件路径
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加单事件:一行 JSON,写后 flush
    pub fn append(&self, ev: &EventEnvelope) -> Result<(), PersistenceError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| PersistenceError::Io(std::io::Error::other("jsonl writer 锁中毒")))?;
        serde_json::to_writer(&mut *writer, ev)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }

    /// 全量读取:逐行解码,读取方守卫生效(未知未标 ignorable → 拒绝整份日志)
    pub fn load(&self) -> Result<Vec<EventEnvelope>, PersistenceError> {
        load_jsonl(&self.path)
    }
}

/// 从 JSONL 文件全量读取(静态入口,重放/重建路径共用)
pub fn load_jsonl(path: &Path) -> Result<Vec<EventEnvelope>, PersistenceError> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let raw: serde_json::Value = serde_json::from_str(&line).map_err(|e| {
            PersistenceError::Turso(format!("{path:?}: 第 {} 行非 JSON: {e}", idx + 1))
        })?;
        // 守卫:未知且未标 ignorable → 拒绝
        events.push(decode_envelope(&raw)?);
    }
    Ok(events)
}
