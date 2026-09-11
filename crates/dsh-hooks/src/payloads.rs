//! payloads:两方言 stdin 载荷构造(源 hooks-claude-code/index.ts
//! payload builders + hooks-codex/index.ts payload builders 逐字对齐)。
#![allow(clippy::too_many_arguments)] // 载荷形状 = wire 契约,参数面照源
//!
//! CC:基座 session_id/transcript_path('')/cwd/hook_event_name,带尾换行;
//! PreToolUse 的 tool_input = 原始 arguments。Codex:snake_case + model +
//! permission_mode:'default',turn 域事件 + turn_id,transcript_path 恒
//! null,tool_input = 简化形 {command},无尾换行。

use serde_json::{Value, json};

use crate::events::HookDialect;

/// CC 基座(源 base;cwd 缺省由调用方传会话工作区或进程 cwd)
fn cc_base(session_id: &str, cwd: &str, event: &str) -> Value {
    json!({
        "session_id": session_id,
        "transcript_path": "",
        "cwd": cwd,
        "hook_event_name": event,
    })
}

/// Codex 基座(源 base:session_id/transcript_path:null/cwd/
/// hook_event_name/model/permission_mode)
fn codex_base(session_id: &str, cwd: &str, event: &str, model: &str) -> Value {
    json!({
        "session_id": session_id,
        "transcript_path": null,
        "cwd": cwd,
        "hook_event_name": event,
        "model": model,
        "permission_mode": "default",
    })
}

fn codex_turn_base(session_id: &str, cwd: &str, event: &str, model: &str, turn: u64) -> Value {
    let mut v = codex_base(session_id, cwd, event, model);
    v["turn_id"] = json!(turn.to_string());
    v
}

/// Codex 的 tool_input 简化形:取 arguments.command 字符串,缺失/非串 ⇒ ''
fn command_of(arguments: &Value) -> Value {
    match arguments.get("command").and_then(Value::as_str) {
        Some(c) => json!({ "command": c }),
        None => json!({ "command": "" }),
    }
}

/// stdin 序列化(源 runHook:JSON + 尾换行按方言)
pub fn serialize_stdin(dialect: HookDialect, payload: &Value) -> Vec<u8> {
    let mut s = serde_json::to_string(payload).expect("payload 序列化必成功");
    if dialect == HookDialect::ClaudeCode {
        s.push('\n');
    }
    s.into_bytes()
}

/// SessionStart 载荷(CC + source / Codex + source)
pub fn session_start(
    dialect: HookDialect,
    session_id: &str,
    cwd: &str,
    source: &str,
    model: &str,
) -> Value {
    match dialect {
        HookDialect::ClaudeCode => {
            let mut v = cc_base(session_id, cwd, "SessionStart");
            v["source"] = json!(source);
            v
        }
        HookDialect::Codex => {
            let mut v = codex_base(session_id, cwd, "SessionStart", model);
            v["source"] = json!(source);
            v
        }
    }
}

/// UserPromptSubmit 载荷(prompt = 文本拼接;Codex 加 turn_id)
pub fn prompt_submit(
    dialect: HookDialect,
    session_id: &str,
    cwd: &str,
    prompt: &str,
    model: &str,
    turn: u64,
) -> Value {
    match dialect {
        HookDialect::ClaudeCode => {
            let mut v = cc_base(session_id, cwd, "UserPromptSubmit");
            v["prompt"] = json!(prompt);
            v
        }
        HookDialect::Codex => {
            let mut v = codex_turn_base(session_id, cwd, "UserPromptSubmit", model, turn);
            v["prompt"] = json!(prompt);
            v
        }
    }
}

/// PreToolUse 载荷(CC tool_input = 原始 arguments / Codex {command})
pub fn pre_tool_use(
    dialect: HookDialect,
    session_id: &str,
    cwd: &str,
    tool_name: &str,
    tool_use_id: &str,
    arguments: &Value,
    model: &str,
    turn: u64,
) -> Value {
    match dialect {
        HookDialect::ClaudeCode => {
            let mut v = cc_base(session_id, cwd, "PreToolUse");
            v["tool_name"] = json!(tool_name);
            v["tool_input"] = arguments.clone();
            v["tool_use_id"] = json!(tool_use_id);
            v
        }
        HookDialect::Codex => {
            let mut v = codex_turn_base(session_id, cwd, "PreToolUse", model, turn);
            v["tool_name"] = json!(tool_name);
            v["tool_input"] = command_of(arguments);
            v["tool_use_id"] = json!(tool_use_id);
            v
        }
    }
}

/// PostToolUse 载荷(= PreToolUse + tool_response 文本)
pub fn post_tool_use(
    dialect: HookDialect,
    session_id: &str,
    cwd: &str,
    tool_name: &str,
    tool_use_id: &str,
    arguments: &Value,
    tool_response: &str,
    model: &str,
    turn: u64,
) -> Value {
    let mut v = pre_tool_use(
        dialect,
        session_id,
        cwd,
        tool_name,
        tool_use_id,
        arguments,
        model,
        turn,
    );
    // 事件名修正(pre_tool_use 已写 PreToolUse)
    v["hook_event_name"] = json!("PostToolUse");
    v["tool_response"] = json!(tool_response);
    v
}

/// Stop 载荷(CC stop_hook_active:false / Codex + turn_id +
/// last_assistant_message:null)
pub fn stop(dialect: HookDialect, session_id: &str, cwd: &str, model: &str, turn: u64) -> Value {
    match dialect {
        HookDialect::ClaudeCode => {
            let mut v = cc_base(session_id, cwd, "Stop");
            v["stop_hook_active"] = json!(false);
            v
        }
        HookDialect::Codex => {
            let mut v = codex_turn_base(session_id, cwd, "Stop", model, turn);
            v["stop_hook_active"] = json!(false);
            v["last_assistant_message"] = Value::Null;
            v
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_newline_dialect_rule() {
        let p = json!({"a":1});
        assert_eq!(serialize_stdin(HookDialect::ClaudeCode, &p), b"{\"a\":1}\n");
        assert_eq!(serialize_stdin(HookDialect::Codex, &p), b"{\"a\":1}");
    }

    #[test]
    fn cc_base_fields_and_transcript_path_empty() {
        let v = pre_tool_use(
            HookDialect::ClaudeCode,
            "s-1",
            "/ws",
            "bash",
            "call-9",
            &json!({"command":"ls"}),
            "",
            3,
        );
        assert_eq!(v["session_id"], "s-1");
        assert_eq!(v["transcript_path"], "");
        assert_eq!(v["cwd"], "/ws");
        assert_eq!(v["hook_event_name"], "PreToolUse");
        assert_eq!(v["tool_name"], "bash");
        assert_eq!(v["tool_input"], json!({"command":"ls"}));
        assert_eq!(v["tool_use_id"], "call-9");
        // CC 无 model/turn_id/permission_mode
        assert!(v.get("model").is_none());
        assert!(v.get("turn_id").is_none());
        assert!(v.get("permission_mode").is_none());
    }

    #[test]
    fn codex_base_fields_and_reduced_tool_input() {
        let v = pre_tool_use(
            HookDialect::Codex,
            "s-1",
            "/ws",
            "bash",
            "call-9",
            &json!({"command":"ls","other":1}),
            "deepseek-v4",
            3,
        );
        assert_eq!(v["transcript_path"], Value::Null);
        assert_eq!(v["model"], "deepseek-v4");
        assert_eq!(v["permission_mode"], "default");
        assert_eq!(v["turn_id"], "3");
        // 简化形:只取 command
        assert_eq!(v["tool_input"], json!({"command":"ls"}));
        // command 缺失/非串 ⇒ ''
        let v = pre_tool_use(
            HookDialect::Codex,
            "s",
            "/ws",
            "read",
            "c",
            &json!({"path":"x"}),
            "",
            1,
        );
        assert_eq!(v["tool_input"], json!({"command":""}));
    }

    #[test]
    fn post_tool_and_stop_shapes() {
        let v = post_tool_use(
            HookDialect::ClaudeCode,
            "s",
            "/ws",
            "bash",
            "c",
            &json!({}),
            "out",
            "",
            2,
        );
        assert_eq!(v["hook_event_name"], "PostToolUse");
        assert_eq!(v["tool_response"], "out");

        let v = stop(HookDialect::ClaudeCode, "s", "/ws", "", 1);
        assert_eq!(v["stop_hook_active"], false);
        assert!(v.get("last_assistant_message").is_none());

        let v = stop(HookDialect::Codex, "s", "/ws", "m", 1);
        assert_eq!(v["stop_hook_active"], false);
        assert_eq!(v["last_assistant_message"], Value::Null);
        assert_eq!(v["turn_id"], "1");
    }

    #[test]
    fn plain_stdout_context_gate_shape() {
        // SessionStart 载荷带 source(Codex/CC 同)
        let v = session_start(HookDialect::Codex, "s", "/ws", "startup", "m");
        assert_eq!(v["source"], "startup");
        assert_eq!(v["model"], "m");
    }
}
