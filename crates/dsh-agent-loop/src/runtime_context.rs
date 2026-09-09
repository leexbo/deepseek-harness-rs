//! 会话级 runtime-context 投影。
//!
//! 语义:维护会话级 `retained` 快照(上一次注入的 runtime-context user/message 的
//! `{seq, text}`)。每步调 `project(current, sections)`,仅在「当前渲染快照文本 ≠ retained.text」
//! 时生成一条候选注入载荷;否则返回 `None`(不重复注入)。当 `retained` 被历史折叠遮蔽时失效
//! (下次文本变化会重新注入)。
//!
//! retained 失效的判定:因 WIT 契约 `surface-op: option<string>` 固定、日志层无 replace 读写,
//! 在 `project()` 调用时比对「当前日志最近 `compaction/summary` 的 `shadowedRange.end`」——若
//! `retained.seq <= shadowedRange.end`,说明 retained 已被折叠遮蔽,视为失效。该实现保持
//! 「折叠遮蔽命中 retained → 失效」语义,且不破坏 gate/engine 同源 derive 不变式。
//!
//! 投影只管理 `@deepseek-ai/dsh-system-prompt` 的 `snapshot`-form 上下文(sandbox/approval 策略)。
//! AGENTS.md(`agent-instructions`)走独立注入路径,不归本投影管。

use serde_json::Value;

use dsh_session::EventEnvelope;

/// 投影所有者的 source.plugin 标识。
pub const CONTEXT_SOURCE_PLUGIN: &str = "@deepseek-ai/dsh-system-prompt";

/// 清空时的占位文本。
pub const CLEARED_CONTEXT: &str =
    "Current runtime context: none. Earlier runtime-context snapshots no longer apply.";

/// 一条已渲染的 runtime-context section(name 归因用,text 模型可见)。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextSection {
    /// section 名(sandbox:policy / approval:policy),归因用
    pub name: String,
    /// 模型可见文本
    pub text: String,
}

/// 上次注入的快照。
#[derive(Debug)]
struct Retained {
    /// 注入 user/message 的事件 seq
    seq: u64,
    /// 注入文本(= 当前快照)
    text: String,
}

/// 会话级 runtime-context 投影。跨 turn 保留 `retained`(引擎持有)。
#[derive(Debug, Default)]
pub struct RuntimeContextProjection {
    retained: Option<Retained>,
}

impl RuntimeContextProjection {
    /// 空投影(尚无快照;`retained = None` = 从未注入过)。
    pub fn new() -> Self {
        Self { retained: None }
    }

    /// 回放日志,反扫最近一条 owned user/message 恢复 `retained`。
    ///
    /// owned = `source.kind=plugin` 且 `source.plugin = CONTEXT_SOURCE_PLUGIN`。若该事件仍在
    /// 可见面(未被折叠遮蔽)则恢复为 retained;否则保持 None(下次变化会重新注入)。
    pub fn restore(&mut self, log: &[EventEnvelope]) {
        for ev in log.iter().rev() {
            if ev.r#type != "user/message" || !is_owned(&ev.data) {
                continue;
            }
            let Some(text) = snapshot_text_of(&ev.data) else {
                continue;
            };
            // 若 retained 本身已被折叠遮蔽,则视为从未注入(重新注入)。
            if fold_shadows(log, ev.seq) {
                continue;
            }
            self.retained = Some(Retained { seq: ev.seq, text });
            break;
        }
    }

    /// 清掉被历史折叠遮蔽的 retained(引擎在每步 `project()` 前调用;读日志判定)。
    /// 折叠遮蔽命中 retained → 失效:当最新 `compaction/summary` 的
    /// `shadowedRange.end >= retained.seq` 时,retained 已被折叠压缩,清空使其下次重注入。
    pub fn refresh_fold(&mut self, log: &[EventEnvelope]) {
        if let Some(r) = &self.retained
            && fold_shadows(log, r.seq)
        {
            self.retained = None;
        }
    }

    /// 当前 retained 文本(仅测试/审计用)。
    #[cfg(test)]
    pub fn retained_text(&self) -> Option<&str> {
        self.retained.as_ref().map(|r| r.text.as_str())
    }

    /// project:给定当前渲染的 `current` 文本与贡献 `sections`,返回候选注入 user/message 载荷,
    /// 或 `None`(无需注入)。规则:
    /// - `retained == None && current` 为空 → 无注入(从未有过,也无需清空)。
    /// - `current` 为空 → 用 CLEARED_CONTEXT 占位。
    /// - `retained.text == snapshot` → 无注入(去重)。
    /// - 否则生成注入载荷(`source.kind=plugin, plugin, form=snapshot, sections` 或清空无 sections)。
    ///
    /// 每次调用前应先用 [`RuntimeContextProjection::observe_fold`] 处理新落日志的折叠遮蔽
    /// (见引擎),使 retained 在折叠命中时先失效。
    pub fn project(
        &mut self,
        current: &str,
        sections: &[ContextSection],
    ) -> Option<(Value, String)> {
        if self.retained.is_none() && current.is_empty() {
            return None;
        }
        let snapshot = if current.is_empty() {
            CLEARED_CONTEXT.to_string()
        } else {
            current.to_string()
        };
        if self.retained.as_ref().map(|r| r.text.as_str()) == Some(snapshot.as_str()) {
            return None;
        }
        let source = if sections.is_empty() {
            Value::Object({
                let mut o = serde_json::Map::new();
                o.insert("kind".into(), Value::String("plugin".into()));
                o.insert("plugin".into(), Value::String(CONTEXT_SOURCE_PLUGIN.into()));
                o
            })
        } else {
            let section_val = sections
                .iter()
                .map(|s| {
                    Value::Object({
                        let mut o = serde_json::Map::new();
                        o.insert("name".into(), Value::String(s.name.clone()));
                        o.insert("text".into(), Value::String(s.text.clone()));
                        o
                    })
                })
                .collect::<Vec<_>>();
            Value::Object({
                let mut o = serde_json::Map::new();
                o.insert("kind".into(), Value::String("plugin".into()));
                o.insert("plugin".into(), Value::String(CONTEXT_SOURCE_PLUGIN.into()));
                o.insert("form".into(), Value::String("snapshot".into()));
                o.insert("sections".into(), Value::Array(section_val));
                o
            })
        };
        let payload = serde_json::json!({
            "content": [ { "type": "text", "text": snapshot } ],
            "source": source,
        });
        Some((payload, snapshot))
    }

    /// 记入一条新落档事件的 retained 更新/失效。
    /// 引擎在每次 commit 后调用。
    /// - `user/message` 且 owned → 更新 retained(新快照注入)。
    /// - 遇 `compaction/summary` 且 `shadowedRange.end >= retained.seq` → 清空 retained(被折叠遮蔽)。
    pub fn observe_event(&mut self, ev: &EventEnvelope) {
        if ev.r#type == "user/message" && is_owned(&ev.data) {
            if let Some(text) = snapshot_text_of(&ev.data) {
                self.retained = Some(Retained { seq: ev.seq, text });
            }
        } else if ev.r#type == "compaction/summary" {
            let end = ev.data["shadowedRange"]["end"].as_u64().unwrap_or(0);
            if let Some(r) = &self.retained {
                if r.seq <= end {
                    self.retained = None;
                }
            } else if end == 0 {
                // 无 retained,无动作。
            }
        }
    }
}

/// source 是否为 owned(runtime-context 快照)。
fn is_owned(data: &Value) -> bool {
    data["source"]["kind"] == Value::String("plugin".into())
        && data["source"]["plugin"] == Value::String(CONTEXT_SOURCE_PLUGIN.into())
}

/// 从 owned user/message 载荷取注入文本
/// (content 为单 text 块时取 text;否则 None)。
fn snapshot_text_of(data: &Value) -> Option<String> {
    let content = data["content"].as_array()?;
    if content.len() == 1 && content[0]["type"] == Value::String("text".into()) {
        content[0]["text"].as_str().map(String::from)
    } else {
        None
    }
}

/// 最近一次 `compaction/summary` 是否遮蔽了给定 seq(折叠命中 → retained 失效)。
fn fold_shadows(log: &[EventEnvelope], seq: u64) -> bool {
    log.iter()
        .rev()
        .find(|e| e.r#type == "compaction/summary")
        .map(|e| e.data["shadowedRange"]["end"].as_u64().unwrap_or(0) >= seq)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(_seq: u64, content: &str) -> EventEnvelope {
        EventEnvelope::new(
            "user/message",
            0,
            serde_json::json!({
                "content": [ { "type": "text", "text": content } ],
                "source": { "kind": "plugin", "plugin": CONTEXT_SOURCE_PLUGIN, "form": "snapshot" },
            }),
        )
    }

    #[test]
    fn project_injects_only_on_change() {
        let mut p = RuntimeContextProjection::new();
        let sec = [ContextSection {
            name: "sandbox:policy".into(),
            text: "sandbox text".into(),
        }];
        let (payload, text) = p.project("snapshot A", &sec).expect("首次注入");
        assert!(text.contains("snapshot A"));
        assert_eq!(payload["source"]["kind"], "plugin");
        assert_eq!(payload["source"]["form"], "snapshot");
        // 模拟 commit:以 payload 构造 owned 事件,observe 更新 retained
        let committed = EventEnvelope::new("user/message", 0, payload.clone());
        p.observe_event(&committed);
        // 文本未变(不含 id,payload 相同)→ 不重复注入
        assert!(p.project("snapshot A", &sec).is_none());
        // 文本变 → 再注入
        assert!(p.project("snapshot B", &sec).is_some());
    }

    #[test]
    fn observe_event_updates_and_survives() {
        let mut p = RuntimeContextProjection::new();
        let sec = [ContextSection {
            name: "sandbox:policy".into(),
            text: "t".into(),
        }];
        p.project("snap", &sec).unwrap();
        // 观察新注入事件 → retained 更新为 seq
        p.observe_event(&user_msg(10, "snap"));
        // 同文本不重复
        assert!(p.project("snap", &sec).is_none());
    }

    #[test]
    fn fold_shadows_clears_retained() {
        let mut p = RuntimeContextProjection::new();
        let sec = [ContextSection {
            name: "sandbox:policy".into(),
            text: "t".into(),
        }];
        p.project("snap1", &sec).unwrap();
        p.observe_event(&user_msg(10, "snap1"));
        // 折叠遮蔽 retained.seq=10 → 失效
        let fold = EventEnvelope::new(
            "compaction/summary",
            0,
            serde_json::json!({ "summary": "s", "shadowedRange": { "start": 1, "end": 12 } }),
        );
        p.observe_event(&fold);
        assert!(p.retained.is_none());
        // 折叠后文本变化 → 重新注入
        assert!(p.project("snap2", &sec).is_some());
    }

    #[test]
    fn restore_replays_owned() {
        let log = vec![user_msg(5, "old snap")];
        let mut p = RuntimeContextProjection::new();
        p.restore(&log);
        assert_eq!(p.retained_text(), Some("old snap"));
    }
}
