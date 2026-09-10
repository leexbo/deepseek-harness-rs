//! 权限策略纯函数面:沙箱模式 + 审批策略的**日志 fold 读取**、**预设推导**、
//! **runtime-context 快照文本合成**。
//!
//! 纯函数:输入 = 会话日志事件 + 工作区路径,输出 = 当前策略文本;无 IO、
//! 无时钟——直播/冷会话重放同源,面板与轨迹不依赖会话存活。
//!
//! 语义分界:权限预设把 **sandbox mode + approval policy 两个 knob 捆绑**;
//! 值是会话日志里的 log-only 事件(`sandbox/mode`、`approval/policy`、
//! `permission/preset`),fold 出当前值。模型只从 runtime-context 快照文本
//! 得知策略,执行侧(build_tools)也 fold 同一份日志。

use serde_json::Value;

use dsh_session::EventEnvelope;

/// 沙箱访问模式(read-only / workspace-write / full-access)。
pub const SANDBOX_MODES: &[&str] = &["read-only", "workspace-write", "full-access"];

/// 审批策略(ask / never)。仅两值,非 ask/never/always。
pub const APPROVAL_POLICIES: &[&str] = &["ask", "never"];

/// 缺省沙箱模式(对齐现有 `session_permission` 默认 workspace-write)
pub const DEFAULT_SANDBOX_MODE: &str = "workspace-write";

/// 缺省审批策略(ask 默认:委托 answerers,无则 fail-closed)
pub const DEFAULT_APPROVAL_POLICY: &str = "ask";

/// 一个权限预设:捆绑 sandbox + approval 两个 knob。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionPresetSpec {
    /// 预设名(如 workspace-write / full-access)
    pub name: &'static str,
    /// 捆绑的沙箱模式
    pub sandbox: &'static str,
    /// 捆绑的审批策略
    pub approval: &'static str,
}

/// 内置权限预设表(核心两条之外补 read-only——UI 三项 仅可查看/工作区内修改/
/// 完全权限):`read-only`→(read-only, ask)、
/// `workspace-write`→(workspace-write, ask)、`full-access`→
/// (full-access, never)。
pub const PRESETS: &[PermissionPresetSpec] = &[
    PermissionPresetSpec {
        name: "read-only",
        sandbox: "read-only",
        approval: "ask",
    },
    PermissionPresetSpec {
        name: "workspace-write",
        sandbox: "workspace-write",
        approval: "ask",
    },
    PermissionPresetSpec {
        name: "full-access",
        sandbox: "full-access",
        approval: "never",
    },
];

/// 派生态(当前 knob 值匹配不到表内预设;非可切换项)。
pub const CUSTOM_PRESET: &str = "custom";

/// fold 出的 knob 状态(None = 无该事件覆盖)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnobState {
    /// 最后 `permission/preset` 载荷,None = 未记录
    pub preset: Option<String>,
    /// 最后 `sandbox/mode` 载荷,None = 未覆盖
    pub sandbox: Option<String>,
    /// 最后 `approval/policy` 载荷,None = 未覆盖
    pub approval: Option<String>,
}

/// 在事件切片里向后找最后一个 `type` 匹配的字符串载荷。
fn last_type_string(events: &[EventEnvelope], ty: &str, key: &str) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|ev| ev.r#type.as_str() == ty)
        .and_then(|ev| ev.data[key].as_str().map(str::to_owned))
}

/// 向后 fold 最后一个、且属于 `known` 集合的取值;无命中或值未知回落 `default`。
fn last_known<'a>(
    events: &[EventEnvelope],
    ty: &str,
    key: &str,
    known: &'a [&'a str],
    default: &'a str,
) -> &'a str {
    for ev in events.iter().rev() {
        if ev.r#type.as_str() != ty {
            continue;
        }
        if let Some(s) = ev.data[key].as_str()
            && let Some(&m) = known.iter().find(|m| **m == s)
        {
            return m;
        }
    }
    default
}

/// 会话当前沙箱模式(fold 最后一个已知 `sandbox/mode`;无则默认 workspace-write)。
pub fn sandbox_mode_of(events: &[EventEnvelope]) -> &'static str {
    last_known(
        events,
        "sandbox/mode",
        "mode",
        SANDBOX_MODES,
        DEFAULT_SANDBOX_MODE,
    )
}

/// 机器值 → 执行侧模式(权限事件 → 工具动态源模式的桥)
pub fn sandbox_mode_of_name(name: &str) -> dsh_sandbox::SandboxMode {
    match name {
        "read-only" => dsh_sandbox::SandboxMode::ReadOnly,
        "full-access" => dsh_sandbox::SandboxMode::FullAccess,
        _ => dsh_sandbox::SandboxMode::WorkspaceWrite,
    }
}

/// 执行侧模式 → 机器值(升级请求/审计文案的桥)
pub fn sandbox_mode_name(mode: dsh_sandbox::SandboxMode) -> &'static str {
    match mode {
        dsh_sandbox::SandboxMode::ReadOnly => "read-only",
        dsh_sandbox::SandboxMode::WorkspaceWrite => "workspace-write",
        dsh_sandbox::SandboxMode::FullAccess => "full-access",
    }
}

/// 会话当前审批策略(fold 最后一个已知 `approval/policy`;无则默认 ask)。
pub fn approval_policy_of(events: &[EventEnvelope]) -> &'static str {
    last_known(
        events,
        "approval/policy",
        "policy",
        APPROVAL_POLICIES,
        DEFAULT_APPROVAL_POLICY,
    )
}

/// 会话最后记录的权限预设(fold 最后一个 `permission/preset`;无则 None)。
pub fn permission_preset_of(events: &[EventEnvelope]) -> Option<String> {
    last_type_string(events, "permission/preset", "preset")
}

/// 合并三个 fold。
pub fn knob_state_of(events: &[EventEnvelope]) -> KnobState {
    KnobState {
        preset: last_type_string(events, "permission/preset", "preset"),
        sandbox: last_type_string(events, "sandbox/mode", "mode"),
        approval: last_type_string(events, "approval/policy", "policy"),
    }
}

/// 由 knob 状态推导预设名:最后选中的预设若仍匹配当前 knob 则沿用,否则
/// 匹配表内首个捆绑,再否则 `custom`。
pub fn derive_preset(state: &KnobState) -> String {
    let sandbox = state.sandbox.as_deref().unwrap_or(DEFAULT_SANDBOX_MODE);
    let approval = state.approval.as_deref().unwrap_or(DEFAULT_APPROVAL_POLICY);
    let matches =
        |spec: &PermissionPresetSpec| spec.sandbox == sandbox && spec.approval == approval;
    if let Some(name) = state.preset.as_deref()
        && let Some(spec) = PRESETS.iter().find(|p| p.name == name)
        && matches(spec)
    {
        return name.to_string();
    }
    PRESETS
        .iter()
        .find(|spec| matches(spec))
        .map(|spec| spec.name.to_string())
        .unwrap_or_else(|| CUSTOM_PRESET.to_string())
}

/// 沙箱策略的模型可见文本(full-access 句不拼机器值 `full-access`——
/// "danger" 词元本身向安全训练的模型暗示风险、推高犹豫率;安全边界由框架
/// 执行,提示词只做事实陈述,故展示名 "full access" 替代。wire/日志/
/// 预设表机器值不动)。
pub fn render_sandbox_policy(mode: &str, workspace_root: Option<&str>) -> String {
    match mode {
        "read-only" => "Current DSH file policy: read-only. Any available operation enforced \
            by the DSH file sandbox cannot modify files in the standing mode. Do not refuse \
            a required modification from this policy alone: try an available tool normally \
            and follow any denial and escalation guidance it returns."
            .into(),
        "workspace-write" => {
            let root = workspace_root
                .map(|p| serde_json::to_string(p).unwrap_or_else(|_| p.to_string()))
                .unwrap_or_else(|| "\"<workspace>\"".into());
            format!(
                "Current DSH file policy: workspace-write. Any available operation enforced \
                by the DSH file sandbox may modify files under the session workspace: {root}. \
                Some platform temporary areas may also be writable."
            )
        }
        _ => "Current DSH file policy: full access. The DSH file sandbox does not \
            restrict file modifications by available operations."
            .into(),
    }
}

/// 审批策略的模型可见文本(`NEVER_SENTENCE` / `ASK_SENTENCE`)。
pub fn render_approval_policy(policy: &str) -> String {
    match policy {
        "never" => "Approval prompts are disabled in this session: actions that require \
            approval are rejected automatically — do not request sandbox escalation \
            (do not set `sandbox_permissions`)."
            .into(),
        _ => "Approval policy: ask. Operations that require approval may ask through the \
            configured answerers; without an available answerer, the request fails closed."
            .into(),
    }
}

/// 一条已渲染的 runtime-context section。
/// `name` 归因用,`text` 是模型可见文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotSection {
    /// section 名(sandbox:policy / approval:policy),归因用
    pub name: String,
    /// 模型可见文本
    pub text: String,
}

/// 合成当前会话的 runtime-context 快照 sections。
/// 依序:先 sandbox:policy(order 110)后 approval:policy(order 115)。
/// 空文本的 section 略去(`.filter(len>0)`)。无 section 时返回空 Vec。
pub fn snapshot_sections(events: &[EventEnvelope], workspace_root: &str) -> Vec<SnapshotSection> {
    let sections = vec![
        SnapshotSection {
            name: "sandbox:policy".into(),
            text: render_sandbox_policy(sandbox_mode_of(events), Some(workspace_root)),
        },
        SnapshotSection {
            name: "approval:policy".into(),
            text: render_approval_policy(approval_policy_of(events)),
        },
    ];
    sections
        .into_iter()
        .filter(|s| !s.text.is_empty())
        .collect()
}

/// 拼出完整快照文本:头部 + section 文本以空行
/// 分隔。无 section 返回空串。
pub fn join_snapshot_text(sections: &[SnapshotSection]) -> String {
    if sections.is_empty() {
        return String::new();
    }
    let body = sections
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "Current runtime context. This snapshot supersedes earlier runtime-context snapshots.\n\n{body}"
    )
}

/// 把 runtime-context 快照组装为 `user/message` 注入载荷
/// (`RuntimeContextProjection.project` 产出的 user message 来源;source.kind=plugin
/// 标记注入上下文)。返回(快照文本, data 载荷);无 section 或文本为空返回 None。
pub fn snapshot_payload(events: &[EventEnvelope], workspace_root: &str) -> Option<(String, Value)> {
    let sections = snapshot_sections(events, workspace_root);
    let text = join_snapshot_text(&sections);
    if text.is_empty() {
        return None;
    }
    let section_val = sections
        .iter()
        .map(|s| serde_json::json!({ "name": s.name, "text": s.text }))
        .collect::<Vec<_>>();
    Some((
        text.clone(),
        serde_json::json!({
            "content": [ { "type": "text", "text": text } ],
            "source": {
                "kind": "plugin",
                "plugin": "@deepseek-ai/dsh-system-prompt",
                "form": "snapshot",
                "sections": section_val,
            },
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(r#type: &str, data: Value) -> EventEnvelope {
        EventEnvelope::new(r#type, 0, data)
    }

    #[test]
    fn fold_defaults_when_no_override() {
        assert_eq!(sandbox_mode_of(&[]), "workspace-write");
        assert_eq!(approval_policy_of(&[]), "ask");
        assert_eq!(permission_preset_of(&[]), None);
        assert_eq!(
            knob_state_of(&[]),
            KnobState {
                preset: None,
                sandbox: None,
                approval: None,
            }
        );
    }

    #[test]
    fn fold_takes_last_event() {
        let log = vec![
            ev(
                "sandbox/mode",
                json!({ "mode": "read-only", "source": "delegation" }),
            ),
            ev("sandbox/mode", json!({ "mode": "full-access" })),
            ev("approval/policy", json!({ "policy": "ask" })),
            ev("approval/policy", json!({ "policy": "never" })),
            ev("permission/preset", json!({ "preset": "workspace-write" })),
        ];
        assert_eq!(sandbox_mode_of(&log), "full-access");
        assert_eq!(approval_policy_of(&log), "never");
        assert_eq!(
            permission_preset_of(&log).as_deref(),
            Some("workspace-write")
        );
        let ks = knob_state_of(&log);
        assert_eq!(ks.sandbox.as_deref(), Some("full-access"));
        assert_eq!(ks.approval.as_deref(), Some("never"));
        assert_eq!(ks.preset.as_deref(), Some("workspace-write"));
    }

    #[test]
    fn fold_ignores_unknown_values_to_default() {
        // 旧日志/用户注入的未知字符串 → 回落默认,不 panic 不泄漏
        let log = vec![ev("sandbox/mode", json!({ "mode": "bogus" }))];
        assert_eq!(sandbox_mode_of(&log), "workspace-write");
        let log2 = vec![ev("approval/policy", json!({ "policy": "sometimes" }))];
        assert_eq!(approval_policy_of(&log2), "ask");
    }

    #[test]
    fn derive_matches_preset_or_custom() {
        // 表内捆绑 → 同名预设
        let ws = KnobState {
            preset: Some("workspace-write".into()),
            sandbox: Some("workspace-write".into()),
            approval: Some("ask".into()),
        };
        assert_eq!(derive_preset(&ws), "workspace-write");
        // 值匹配但预设名不同 → 推导表内首个匹配
        let dfa = KnobState {
            preset: Some("workspace-write".into()),
            sandbox: Some("full-access".into()),
            approval: Some("never".into()),
        };
        assert_eq!(derive_preset(&dfa), "full-access");
        // 值不匹配任何预设 → custom
        let custom = KnobState {
            preset: Some("workspace-write".into()),
            sandbox: Some("read-only".into()),
            approval: Some("never".into()),
        };
        assert_eq!(derive_preset(&custom), "custom");
    }

    #[test]
    fn render_sandbox_policy_three_branches() {
        assert!(render_sandbox_policy("read-only", None).contains("read-only"));
        let ws = render_sandbox_policy("workspace-write", Some("/tmp/ws"));
        assert!(ws.contains("workspace-write"));
        assert!(ws.contains("/tmp/ws"));
        let dfa = render_sandbox_policy("full-access", None);
        assert!(dfa.contains("full access"));
        assert!(dfa.to_lowercase().contains("does not restrict"));
        // 模型面禁现 "danger" 词元(安全边界由框架
        // 执行,提示词不做风险暗示);机器值仍为 full-access
        assert!(
            !dfa.to_lowercase().contains("danger"),
            "模型可见文本不得含 danger 词元: {dfa}"
        );
    }

    #[test]
    fn render_approval_policy_two_branches() {
        assert!(render_approval_policy("ask").contains("Approval policy: ask"));
        assert!(render_approval_policy("never").contains("disabled"));
    }

    #[test]
    fn snapshot_sections_and_payload_roundtrip() {
        let log = vec![
            ev(
                "sandbox/mode",
                json!({ "mode": "full-access", "source": "delegation" }),
            ),
            ev(
                "approval/policy",
                json!({ "policy": "never", "source": "delegation" }),
            ),
        ];
        let sections = snapshot_sections(&log, "/tmp/ws");
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].name, "sandbox:policy");
        assert_eq!(sections[1].name, "approval:policy");
        let text = join_snapshot_text(&sections);
        assert!(text.starts_with("Current runtime context. This snapshot supersedes earlier"));
        let (payload_text, payload) = snapshot_payload(&log, "/tmp/ws").expect("有快照");
        assert_eq!(payload_text, text);
        assert_eq!(payload["source"]["kind"], "plugin");
        assert_eq!(
            payload["source"]["plugin"],
            "@deepseek-ai/dsh-system-prompt"
        );
        assert_eq!(payload["source"]["sections"][0]["name"], "sandbox:policy");
        assert_eq!(payload["content"][0]["text"], text);
    }

    #[test]
    fn snapshot_empty_when_no_sections() {
        // sandbox 文本恒非空(默认 workspace-write),approval 恒非空,故总是有
        assert!(!snapshot_sections(&[], "/tmp/ws").is_empty());
        let (_, payload) = snapshot_payload(&[], "/tmp/ws").expect("无事件也应给默认快照");
        assert_eq!(payload["source"]["form"], "snapshot");
    }
}
