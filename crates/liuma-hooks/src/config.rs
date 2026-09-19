//! config:两方言 hooks.json 解析(源 hooks-claude-code/config.ts +
//! hooks-codex/config.ts 逐字对齐)。

use serde_json::Value;
use std::collections::BTreeMap;

use crate::events::HookDialect;
use crate::matcher::{MatcherMode, matcher_diagnostic};

/// Claude Code 七事件点(源 CLAUDE_EVENTS)
pub const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "SubagentStart",
    "SubagentStop",
];

/// Codex 五事件点(源 CODEX_EVENTS)
pub const CODEX_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
];

/// 无 matcher 主语的事件(UserPromptSubmit / Stop 的 matcher 字段解析期
/// 直接丢弃,坏 matcher 也不报错——照源)
fn ignores_matchers(point: &str) -> bool {
    point == "UserPromptSubmit" || point == "Stop"
}

/// 一个 command 钩子(源 CommandHook;wire 单位 timeout 秒,运行时转 ms)
#[derive(Debug, Clone, PartialEq)]
pub struct CommandHook {
    pub command: String,
    pub timeout_sec: Option<f64>,
}

/// 一个 matcher 组(源 MatcherGroup)
#[derive(Debug, Clone, PartialEq)]
pub struct MatcherGroup {
    pub matcher: Option<String>,
    pub hooks: Vec<CommandHook>,
}

/// 解析后的方言配置:事件名 → matcher 组
pub type HookConfig = BTreeMap<String, Vec<MatcherGroup>>;

/// 被跳过的非 command 钩子(桥逐个 warn)
#[derive(Debug, Clone, PartialEq)]
pub struct SkippedHook {
    pub event: String,
    pub reason: String,
}

/// 解析产物:可运行组 + 跳过清单
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedConfig {
    pub config: HookConfig,
    pub skipped: Vec<SkippedHook>,
}

/// 桥方言:决定事件点集合、matcher 模式、替换与跳过规则
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeDialect {
    ClaudeCode,
    Codex,
}

impl BridgeDialect {
    pub fn matcher_mode(self) -> MatcherMode {
        match self {
            BridgeDialect::ClaudeCode => MatcherMode::ClaudeCode,
            BridgeDialect::Codex => MatcherMode::Codex,
        }
    }

    pub fn events(self) -> &'static [&'static str] {
        match self {
            BridgeDialect::ClaudeCode => CLAUDE_EVENTS,
            BridgeDialect::Codex => CODEX_EVENTS,
        }
    }

    pub fn dialect(self) -> HookDialect {
        match self {
            BridgeDialect::ClaudeCode => HookDialect::ClaudeCode,
            BridgeDialect::Codex => HookDialect::Codex,
        }
    }
}

/// CC 解析期命令替换(源 substituteCommand):全出现处替换,变量未设
/// token 原样保留
pub fn substitute_command(
    command: &str,
    plugin_root: Option<&str>,
    project_dir: Option<&str>,
) -> String {
    let mut out = command.to_string();
    if let Some(root) = plugin_root {
        out = out.replace("${CLAUDE_PLUGIN_ROOT}", root);
    }
    if let Some(dir) = project_dir {
        out = out.replace("${CLAUDE_PROJECT_DIR}", dir);
    }
    out
}

fn as_object(v: &Value) -> Option<&serde_json::Map<String, Value>> {
    v.as_object()
}

/// 解析 hooks.json(源 parseClaudeCodeConfig / parseCodexConfig 共形):
/// 接受 `{hooks:{…}}` 包装或裸事件映射;不支持事件整体忽略(其上的坏
/// matcher 不拖垮支持钩子);非 command(CC)/ `async:true`(Codex)进
/// skipped;CC 做替换、Codex 不做;含 matcher 的可运行组带无效 regex ⇒
/// Err(整份配置拒绝,诊断串含事件名)。
pub fn parse_hook_config(
    dialect: BridgeDialect,
    raw: &Value,
    plugin_root: Option<&str>,
    project_dir: Option<&str>,
) -> Result<ParsedConfig, String> {
    let mut config: HookConfig = BTreeMap::new();
    let mut skipped: Vec<SkippedHook> = Vec::new();

    let Some(root) = as_object(raw) else {
        return Ok(ParsedConfig { config, skipped });
    };
    let hooks_map = root.get("hooks").and_then(as_object).unwrap_or(root);

    for event in dialect.events() {
        let Some(raw_groups) = hooks_map.get(*event).and_then(Value::as_array) else {
            continue;
        };
        let mut groups: Vec<MatcherGroup> = Vec::new();
        for raw_group in raw_groups {
            let Some(group) = as_object(raw_group) else {
                continue;
            };
            let Some(raw_hooks) = group.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            let mut commands: Vec<CommandHook> = Vec::new();
            for raw_hook in raw_hooks {
                let Some(hook) = as_object(raw_hook) else {
                    continue;
                };
                // type 缺省 = 'command'(CC 默认);Codex 跳 async: true
                let hook_type = hook
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("command");
                if hook_type != "command" {
                    skipped.push(SkippedHook {
                        event: (*event).to_string(),
                        reason: format!(
                            "unsupported \"{hook_type}\" hook (only command hooks run)"
                        ),
                    });
                    continue;
                }
                if dialect == BridgeDialect::Codex
                    && hook.get("async").and_then(Value::as_bool) == Some(true)
                {
                    skipped.push(SkippedHook {
                        event: (*event).to_string(),
                        reason: "async hook".to_string(),
                    });
                    continue;
                }
                let Some(command) = hook.get("command").and_then(Value::as_str) else {
                    continue;
                };
                let command = match dialect {
                    BridgeDialect::ClaudeCode => {
                        substitute_command(command, plugin_root, project_dir)
                    }
                    BridgeDialect::Codex => command.to_string(),
                };
                // timeout:CC 单键;Codex `timeout` / `timeoutSec` 别名等价
                let timeout_sec = hook
                    .get("timeout")
                    .or(if dialect == BridgeDialect::Codex {
                        hook.get("timeoutSec")
                    } else {
                        None
                    })
                    .and_then(Value::as_f64);
                commands.push(CommandHook {
                    command,
                    timeout_sec,
                });
            }
            if commands.is_empty() {
                continue;
            }
            // 无主语事件的 matcher 字段先行丢弃(坏 matcher 也不报错);
            // 其余非字符串视为缺省
            let matcher = if ignores_matchers(event) {
                None
            } else {
                group
                    .get("matcher")
                    .and_then(Value::as_str)
                    .map(String::from)
            };
            let diagnostic = matcher_diagnostic(matcher.as_deref(), dialect.matcher_mode());
            if let Some(d) = diagnostic {
                return Err(format!("{d} on event {event:?}"));
            }
            groups.push(MatcherGroup {
                matcher,
                hooks: commands,
            });
        }
        if !groups.is_empty() {
            config.insert((*event).to_string(), groups);
        }
    }

    Ok(ParsedConfig { config, skipped })
}

/// CC 配置解析(桥侧薄封装)
pub fn parse_claude_code_config(
    raw: &Value,
    plugin_root: Option<&str>,
    project_dir: Option<&str>,
) -> Result<ParsedConfig, String> {
    parse_hook_config(BridgeDialect::ClaudeCode, raw, plugin_root, project_dir)
}

/// Codex 配置解析(桥侧薄封装)
pub fn parse_codex_config(raw: &Value) -> Result<ParsedConfig, String> {
    parse_hook_config(BridgeDialect::Codex, raw, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn substitution_verbatim_and_partial() {
        assert_eq!(
            substitute_command(
                "run ${CLAUDE_PLUGIN_ROOT}/x ${CLAUDE_PROJECT_DIR}",
                Some("/p"),
                Some("/d")
            ),
            "run /p/x /d"
        );
        // 变量未设 token 原样保留
        assert_eq!(
            substitute_command("run ${CLAUDE_PLUGIN_ROOT}/x", None, None),
            "run ${CLAUDE_PLUGIN_ROOT}/x"
        );
    }

    #[test]
    fn bare_map_equals_hooks_wrapped() {
        let bare = json!({"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"a","timeout":30}]}]});
        let wrapped = json!({"hooks": bare});
        for raw in [bare.clone(), wrapped] {
            let p = parse_claude_code_config(&raw, None, None).unwrap();
            let groups = &p.config["PreToolUse"];
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].matcher.as_deref(), Some("Bash"));
            assert_eq!(groups[0].hooks[0].timeout_sec, Some(30.0));
        }
    }

    #[test]
    fn skipped_types_recorded_and_type_default_is_command() {
        let raw = json!({"PreToolUse":[
            {"hooks":[{"type":"prompt","command":"x"},{"command":"ok"}]},
            {"hooks":[{"type":"http","command":"y"}]}
        ]});
        let p = parse_claude_code_config(&raw, None, None).unwrap();
        assert_eq!(p.skipped.len(), 2);
        assert_eq!(
            p.skipped[0].reason,
            "unsupported \"prompt\" hook (only command hooks run)"
        );
        // 无 type = command 照跑
        assert_eq!(p.config["PreToolUse"][0].hooks.len(), 1);
        assert_eq!(p.config["PreToolUse"][0].hooks[0].command, "ok");
    }

    #[test]
    fn codex_skips_async_and_accepts_timeout_aliases_no_substitution() {
        let raw = json!({"PreToolUse":[
            {"hooks":[{"command":"a","async":true},{"command":"b","timeoutSec":5},{"command":"c ${NOT_SUBSTITUTED}","timeout":7}]}
        ]});
        let p = parse_codex_config(&raw).unwrap();
        assert_eq!(p.skipped.len(), 1);
        assert_eq!(p.skipped[0].reason, "async hook");
        let hooks = &p.config["PreToolUse"][0].hooks;
        assert_eq!(hooks[0].timeout_sec, Some(5.0));
        assert_eq!(hooks[1].timeout_sec, Some(7.0));
        // Codex 不做替换
        assert_eq!(hooks[1].command, "c ${NOT_SUBSTITUTED}");
    }

    #[test]
    fn unsupported_event_bad_matcher_does_not_break_supported() {
        let raw = json!({
            "Notification": [{"hooks": []}],
            "PreToolUse": [{"matcher":"Bash","hooks":[{"command":"a"}]}]
        });
        let p = parse_claude_code_config(&raw, None, None).unwrap();
        assert!(p.config.contains_key("PreToolUse"));
        assert!(!p.config.contains_key("Notification"));
    }

    #[test]
    fn invalid_matcher_rejects_whole_config_with_event_in_message() {
        let raw = json!({"PreToolUse":[{"matcher":"(","hooks":[{"command":"a"}]}]});
        let err = parse_claude_code_config(&raw, None, None).unwrap_err();
        assert_eq!(
            err,
            "invalid claude-code regex matcher \"(\" on event \"PreToolUse\""
        );
    }

    #[test]
    fn user_prompt_submit_and_stop_matchers_discarded_before_validation() {
        // 坏 matcher 也不报错(无主语事件先行丢弃)
        for event in ["UserPromptSubmit", "Stop"] {
            let raw = json!({ event: [{"matcher":"(","hooks":[{"command":"a"}]}]});
            let p = parse_claude_code_config(&raw, None, None).unwrap();
            assert!(p.config.contains_key(event));
            assert_eq!(p.config[event][0].matcher, None);
        }
    }

    #[test]
    fn codex_only_five_events() {
        let raw = json!({
            "SubagentStop": [{"hooks":[{"command":"a"}]}],
            "Notification": [{"hooks":[{"command":"b"}]}],
            "PreToolUse": [{"hooks":[{"command":"c"}]}]
        });
        let p = parse_codex_config(&raw).unwrap();
        assert!(p.config.contains_key("PreToolUse"));
        assert!(!p.config.contains_key("SubagentStop"));
        assert!(!p.config.contains_key("Notification"));
    }

    #[test]
    fn malformed_entries_silently_dropped() {
        let raw = json!({"PreToolUse":[
            "string-entry",
            {"no-hooks": 1},
            {"hooks":["not-object", {"type":"command"}, {"type":"command","command":42}, {"type":"command","command":"ok"}]}
        ]});
        let p = parse_claude_code_config(&raw, None, None).unwrap();
        assert_eq!(p.config["PreToolUse"][0].hooks.len(), 1);
    }
}
