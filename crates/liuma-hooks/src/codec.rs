//! codec:hook 进程输出解码(源 hook-protocol/codec.ts 逐字对齐)。
//!
//! 退出码契约:exit 2 = 阻塞(stderr trim 为 reason,空 stderr 无
//! reason,此时 stdout 无效);exit 0 = 仅当 trim 后 stdout 以 `{` 开头
//! 才尝试 JSON(坏 JSON 宽容 = 无结构输出);其余退出码(含没能运行)
//! = 非阻塞错误、无 decision。决策双通道:顶层 `decision` 只认
//! approve/block;`hookSpecificOutput.permissionDecision` 认 allow/deny/ask
//! 且覆盖顶层;判别名缺失/不符 ⇒ 块内事件级字段全部丢弃、顶层保留。

use serde_json::Value;

/// 阻塞退出码(源 BLOCKING_EXIT_CODE):stderr → reason
pub const BLOCKING_EXIT_CODE: i32 = 2;

/// 统一决策枚举(源 HookOutput.decision):block/deny 禁止,
/// approve/allow 放行,ask 请求确认。allow/deny/ask 只能来自
/// permissionDecision,顶层 decision 只认 approve/block。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Allow,
    Block,
    Deny,
    Ask,
}

impl Decision {
    #[allow(dead_code)] // 词汇完备性:parse_* 三件套对称保留
    fn parse(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(Self::Approve),
            "allow" => Some(Self::Allow),
            "block" => Some(Self::Block),
            "deny" => Some(Self::Deny),
            "ask" => Some(Self::Ask),
            _ => None,
        }
    }

    fn parse_top_level(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(Self::Approve),
            "block" => Some(Self::Block),
            // allow/deny/ask 在顶层无效(两参考 schema 保留给
            // permissionDecision)——越界值忽略,不得成为真阻塞决策
            _ => None,
        }
    }

    fn parse_permission(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            "ask" => Some(Self::Ask),
            _ => None,
        }
    }

    /// hook/result 的 decision 字段串(逐字小写)
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Approve => "approve",
            Decision::Allow => "allow",
            Decision::Block => "block",
            Decision::Deny => "deny",
            Decision::Ask => "ask",
        }
    }
}

/// 方言中立解码结果(源 HookOutput;全部字段可选 = 钩子可行使任意
/// 子集,桥按 hook 点决定哪些字段有意义)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookOutput {
    /// 进程退出码(None = 没能运行/信号死 → 非阻塞)
    pub exit_code: Option<i32>,
    /// trim 后 stderr(exit 2 阻塞原因来源)
    pub stderr: String,
    /// trim 后 stdout 原文(CC 渲染为 output;Codex SessionStart/
    /// UserPromptSubmit 无结构上下文时当 additionalContext)
    pub stdout: String,
    /// false ⇒ 钩子请求停止(continue:false;仅记录,run-level halt 照源不做)
    pub continue_: Option<bool>,
    /// continue:false 的人类可读原因
    pub stop_reason: Option<String>,
    /// 统一阻塞决策(None = 无显式决策,退出码治理)
    pub decision: Option<Decision>,
    /// 决策理由
    pub reason: Option<String>,
    /// hookSpecificOutput 声称的事件判别名(不符也保留,供日志)
    pub hook_event_name: Option<String>,
    /// 注入下一请求的额外上下文
    pub additional_context: Option<String>,
    /// 给用户的告警(照源仅记录不上浮)
    pub system_message: Option<String>,
    /// 工具入参改写请求(解析但不执行,warn)
    pub updated_input: Option<Value>,
}

/// 读 JSON 对象的字符串字段(缺失/类型不符 = None)
fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(String::from)
}

fn bool_field(v: &Value, key: &str) -> Option<bool> {
    v.get(key)?.as_bool()
}

/// 解码进程输出(源 parseHookOutput;全函数:坏 JSON = 无结构输出,
/// 绝不抛)。`expected_event_name` 给定时,`hookSpecificOutput` 的
/// `hookEventName` 缺失或不符 ⇒ 该块事件级字段丢弃(顶层与已声称的
/// 判别名保留);None = 不设防(照源 opt-out)。
pub fn parse_hook_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    expected_event_name: Option<&str>,
) -> HookOutput {
    let trimmed_err = stderr.trim();
    let trimmed_out = stdout.trim();
    let mut output = HookOutput {
        exit_code,
        stderr: trimmed_err.to_string(),
        stdout: trimmed_out.to_string(),
        ..Default::default()
    };

    // 两方言一致:exit 2 = block,stderr 为因(stdout 无效)
    if exit_code == Some(BLOCKING_EXIT_CODE) {
        output.decision = Some(Decision::Block);
        if !trimmed_err.is_empty() {
            output.reason = Some(trimmed_err.to_string());
        }
    }

    // 结构化 stdout 仅对干净退出有意义,且必须以 `{` 开头(其余
    // stdout 是纯文本,不是错误——照参考引擎宽容)
    if exit_code == Some(0)
        && trimmed_out.starts_with('{')
        && let Ok(parsed) = serde_json::from_str::<Value>(trimmed_out)
        && parsed.is_object()
    {
        apply_structured(&mut output, &parsed, expected_event_name);
    }

    output
}

/// 折叠结构化 stdout 对象(源 applyStructured)
fn apply_structured(output: &mut HookOutput, parsed: &Value, expected_event_name: Option<&str>) {
    if let Some(cont) = bool_field(parsed, "continue") {
        output.continue_ = Some(cont);
    }
    if let Some(r) = str_field(parsed, "stopReason") {
        output.stop_reason = Some(r);
    }
    if let Some(m) = str_field(parsed, "systemMessage") {
        output.system_message = Some(m);
    }

    // 顶层 legacy decision(approve/block ONLY)+ reason
    if let Some(top) = str_field(parsed, "decision")
        .as_deref()
        .and_then(Decision::parse_top_level)
    {
        output.decision = Some(top);
    }
    if let Some(r) = str_field(parsed, "reason") {
        output.reason = Some(r);
    }

    // hookSpecificOutput:permissionDecision 覆盖顶层;additionalContext /
    // updatedInput 也在此块。判别名不符 ⇒ 块内事件级字段全部丢弃。
    let Some(hso) = parsed.get("hookSpecificOutput").filter(|v| v.is_object()) else {
        return;
    };
    let event_name = str_field(hso, "hookEventName");
    // 判别名恒保留(日志/诊断要看到畸形块声称了什么)
    if let Some(name) = &event_name {
        output.hook_event_name = Some(name.clone());
    }
    if let Some(expected) = expected_event_name {
        match &event_name {
            Some(name) if name == expected => {}
            // 缺失或 mism ⇒ 丢弃(missing 与 mismatch 同罪,照源)
            _ => return,
        }
    }
    if let Some(p) = str_field(hso, "permissionDecision")
        .as_deref()
        .and_then(Decision::parse_permission)
    {
        output.decision = Some(p);
    }
    if let Some(r) = str_field(hso, "permissionDecisionReason") {
        output.reason = Some(r);
    }
    if let Some(c) = str_field(hso, "additionalContext") {
        output.additional_context = Some(c);
    }
    if let Some(u) = hso.get("updatedInput").filter(|v| v.is_object()) {
        output.updated_input = Some(u.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exit_codes_semantics() {
        // exit 0 无输出 = 中性
        let o = parse_hook_output(Some(0), "", "", None);
        assert_eq!(o.decision, None);
        // exit 2 = block + stderr 为 reason
        let o = parse_hook_output(Some(2), "ignored stdout", "  blocked!  ", None);
        assert_eq!(o.decision, Some(Decision::Block));
        assert_eq!(o.reason.as_deref(), Some("blocked!"));
        // exit 2 空 stderr = block 无 reason
        let o = parse_hook_output(Some(2), "", "  \n ", None);
        assert_eq!(o.decision, Some(Decision::Block));
        assert_eq!(o.reason, None);
        // 其它退出码 / 没能运行 = 非阻塞
        for code in [Some(1), Some(127), None] {
            let o = parse_hook_output(code, "", "err", None);
            assert_eq!(o.decision, None);
            assert_eq!(o.stderr, "err");
        }
        // exit 2 时 stdout 里的 approve 无效
        let o = parse_hook_output(Some(2), r#"{"decision":"approve"}"#, "no", None);
        assert_eq!(o.decision, Some(Decision::Block));
    }

    #[test]
    fn structured_stdout_top_level_fields() {
        let o = parse_hook_output(
            Some(0),
            r#"{"continue":false,"stopReason":"done","systemMessage":"warn"}"#,
            "",
            None,
        );
        assert_eq!(o.continue_, Some(false));
        assert_eq!(o.stop_reason.as_deref(), Some("done"));
        assert_eq!(o.system_message.as_deref(), Some("warn"));
    }

    #[test]
    fn top_level_decision_only_approve_block() {
        let o = parse_hook_output(Some(0), r#"{"decision":"approve","reason":"ok"}"#, "", None);
        assert_eq!(o.decision, Some(Decision::Approve));
        assert_eq!(o.reason.as_deref(), Some("ok"));
        // 顶层 deny 无效并忽略
        let o = parse_hook_output(Some(0), r#"{"decision":"deny"}"#, "", None);
        assert_eq!(o.decision, None);
        // 未知串不强制转换
        let o = parse_hook_output(Some(0), r#"{"decision":"maybe"}"#, "", None);
        assert_eq!(o.decision, None);
    }

    #[test]
    fn permission_decision_overrides_top_level() {
        let o = parse_hook_output(
            Some(0),
            r#"{"decision":"approve","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"nope"}}"#,
            "",
            Some("PreToolUse"),
        );
        assert_eq!(o.decision, Some(Decision::Deny));
        assert_eq!(o.reason.as_deref(), Some("nope"));
    }

    #[test]
    fn event_name_guard_matrix() {
        let block = r#"{"decision":"block","hookSpecificOutput":{"hookEventName":"NAME","additionalContext":"ctx"}}"#;
        // 匹配 → 应用
        let o = parse_hook_output(Some(0), block, "", Some("NAME"));
        assert_eq!(o.additional_context.as_deref(), Some("ctx"));
        assert_eq!(o.decision, Some(Decision::Block));
        // mismatch → 块内丢弃,顶层与判别名保留
        let o = parse_hook_output(Some(0), block, "", Some("OTHER"));
        assert_eq!(o.additional_context, None);
        assert_eq!(o.hook_event_name.as_deref(), Some("NAME"));
        assert_eq!(o.decision, Some(Decision::Block));
        // 缺判别名 = 与 mismatch 同罪
        let o = parse_hook_output(
            Some(0),
            r#"{"hookSpecificOutput":{"additionalContext":"ctx"}}"#,
            "",
            Some("NAME"),
        );
        assert_eq!(o.additional_context, None);
        // 省略 expected = 不设防
        let o = parse_hook_output(Some(0), block, "", None);
        assert_eq!(o.additional_context.as_deref(), Some("ctx"));
    }

    #[test]
    fn lenient_malformed_and_non_json() {
        // 坏 JSON 宽容 = 无结构输出,stdout 原文保留
        let o = parse_hook_output(Some(0), "{not json", "", None);
        assert_eq!(o.stdout, "{not json");
        assert_eq!(o.additional_context, None);
        // 非 `{` 开头完全不尝试
        let o = parse_hook_output(Some(0), "[1,2,3]", "", None);
        assert_eq!(o.stdout, "[1,2,3]");
        // JSON 数组不解析为对象
        let o = parse_hook_output(Some(0), r#"[{"a":1}]"#, "", None);
        assert_eq!(o.updated_input, None);
    }

    #[test]
    fn updated_input_parsed_not_applied() {
        let o = parse_hook_output(
            Some(0),
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"evil"}}}"#,
            "",
            Some("PreToolUse"),
        );
        assert_eq!(o.updated_input, Some(json!({"command":"evil"})));
    }
}
