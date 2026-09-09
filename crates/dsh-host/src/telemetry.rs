//! 遥测导出器:从事件日志派生 otel 形状的 span 记录。
//!
//! 设计与 WIT `dsh:host/telemetry`(span + attrs)对齐,但派生方式
//! 遵循项目 DNA:**span 是日志的确定性投影**——turn/step 是区间 span,
//! `audit/call` 是点 span(属性带 sourceEventSeqs 归因)。
//! 同一日志两次导出 bit-exact(重放确定性,E2 的遥测面收益);
//! OTLP wire 序列化在此之上分层接入,不改动派生逻辑。
//!
//! span 层级:turn(span_id = turn/start 的 seq)
//! → step(span_id = step/start 的 seq)
//! → audit/call(点 span,父 = 所在 step)。

use std::io::Write;
use std::path::Path;

use dsh_session::EventLog;
use serde::Serialize;
use serde_json::{Value, json};

/// 一条导出的 span(字段名对齐 otel SDK JSON 编码)
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpanRecord {
    /// trace 标识(日志确定性派生;重放稳定)
    #[serde(rename = "traceId")]
    pub trace_id: String,
    /// span 标识 = 事件 seq
    #[serde(rename = "spanId")]
    pub span_id: u64,
    /// 父 span(None = 根)
    #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<u64>,
    /// span 名(turn / step / boundary.operation)
    pub name: String,
    /// 起始时间(纳秒;事件 time 为毫秒,×10⁶)
    #[serde(rename = "startTimeUnixNano")]
    pub start_time_unix_nano: i64,
    /// 结束时间(纳秒;audit 点 span 等于起始)
    #[serde(rename = "endTimeUnixNano")]
    pub end_time_unix_nano: i64,
    /// 属性(audit/call 携带 detail 与 sourceEventSeqs 归因)
    pub attributes: Value,
}

/// 导出错误
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// 写文件失败
    #[error("span export io: {0}")]
    Io(#[from] std::io::Error),
}

/// trace_id:日志首事件与格式版本的确定性哈希(FNV-1a,十六进制)。
///
/// 重放稳定性依据:同一日志的信封字节一致 ⇒ trace_id 一致。
fn trace_id_of(log: &EventLog) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in dsh_session::SESSION_FORMAT_VERSION.to_string().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    if let Some(first) = log.iter().next() {
        for byte in format!("{}|{}", first.r#type, first.time).as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

/// 从日志派生全部 span(保序:turn → step → audit)。
pub fn export_spans(log: &EventLog) -> Vec<SpanRecord> {
    let trace_id = trace_id_of(log);
    let mut spans = Vec::new();
    let mut turn: Option<(u64, i64)> = None; // (span_id, start time)
    let mut step: Option<(u64, i64)> = None;

    for ev in log.iter() {
        match ev.r#type.as_str() {
            "turn/start" => turn = Some((ev.seq, ev.time)),
            "step/start" => step = Some((ev.seq, ev.time)),
            "turn/end" => {
                if let Some((span_id, start)) = turn.take() {
                    spans.push(SpanRecord {
                        trace_id: trace_id.clone(),
                        span_id,
                        parent_id: None,
                        name: "turn".into(),
                        start_time_unix_nano: start * 1_000_000,
                        end_time_unix_nano: ev.time * 1_000_000,
                        attributes: json!({ "endSeq": ev.seq }),
                    });
                }
                step = None;
            }
            "step/end" => {
                if let (Some((turn_id, _)), Some((span_id, start))) = (turn, step.take()) {
                    spans.push(SpanRecord {
                        trace_id: trace_id.clone(),
                        span_id,
                        parent_id: Some(turn_id),
                        name: "step".into(),
                        start_time_unix_nano: start * 1_000_000,
                        end_time_unix_nano: ev.time * 1_000_000,
                        attributes: json!({ "endSeq": ev.seq }),
                    });
                }
            }
            "audit/call" => {
                let parent = step.map(|(id, _)| id).or(turn.map(|(id, _)| id));
                spans.push(SpanRecord {
                    trace_id: trace_id.clone(),
                    span_id: ev.seq,
                    parent_id: parent,
                    name: format!(
                        "{}.{}",
                        ev.data["boundary"].as_str().unwrap_or("?"),
                        ev.data["operation"].as_str().unwrap_or("?")
                    ),
                    start_time_unix_nano: ev.time * 1_000_000,
                    end_time_unix_nano: ev.time * 1_000_000,
                    attributes: json!({
                        "detail": ev.data["detail"],
                        "sourceEventSeqs": ev.source_event_seqs.clone().unwrap_or_default(),
                    }),
                });
            }
            _ => {}
        }
    }
    spans
}

/// 以 otel SDK JSON 编码(每行一 span)写出导出文件。
pub fn write_otlp_jsonl(path: &Path, spans: &[SpanRecord]) -> Result<(), TelemetryError> {
    let mut file = std::fs::File::create(path)?;
    for span in spans {
        serde_json::to_writer(&mut file, span).map_err(std::io::Error::other)?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::{EventEnvelope, audit::audit_call_event};
    use serde_json::json;

    fn sample_log() -> EventLog {
        let mut log = EventLog::new();
        for (t, time, data) in [
            ("turn/start", 1, json!({})),
            ("user/message", 2, json!({ "content": "hi" })),
            ("step/start", 3, json!({})),
            ("assistant/message", 4, json!({ "content": "yo" })),
            ("step/end", 5, json!({})),
            ("turn/end", 6, json!({})),
        ] {
            log.append(EventEnvelope::new(t, time, data)).unwrap();
        }
        // 插到 step 内的 audit(seq 4 之后语义上;seq 由 append 决定)
        log.append(audit_call_event(
            7,
            "llm",
            "request",
            json!({ "model": "m" }),
            vec![2],
        ))
        .unwrap();
        log
    }

    #[test]
    fn spans_hierarchy_and_determinism() {
        let log = sample_log();
        let spans = export_spans(&log);
        // 顺序按区间闭合先后:step/end(seq5)先于 turn/end(seq6),
        // audit(seq7)在 turn/end 之后 → 父回退策略:step 已闭合且 turn 已闭合 → None
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].name, "step");
        assert_eq!(spans[0].span_id, 3);
        assert_eq!(spans[0].parent_id, Some(1));
        assert_eq!(spans[1].name, "turn");
        assert_eq!(spans[1].span_id, 1);
        assert_eq!(spans[1].parent_id, None);
        assert_eq!(spans[2].name, "llm.request");
        assert_eq!(spans[2].parent_id, None, "turn/step 均已闭合:点 span 挂根");
        assert_eq!(
            spans[2].attributes["sourceEventSeqs"],
            json!([2]),
            "audit span 属性带归因"
        );

        // 确定性:同一日志两次导出相等;重放重建后仍相等
        assert_eq!(spans, export_spans(&log));
        let snapshot = log.snapshot();
        let rebuilt = EventLog::from_snapshot(&snapshot).unwrap();
        assert_eq!(spans, export_spans(&rebuilt), "重放导出 bit-exact");
    }
}
