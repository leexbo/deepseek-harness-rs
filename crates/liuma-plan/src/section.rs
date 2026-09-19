//! plan prompt 段(plan:policy 约束段 + active-plan 段;文本即终态,
//! 无部署时配置面——预置正文固定,工具名两侧一致)。

use liuma_prompt::PromptSection;
use liuma_session::EventLog;

use crate::state::plan_state;

/// plan 模式约束段正文(预置正文逐字固定;
/// 每请求渲染,未激活不注入)。
pub const PLAN_MODE_SECTION_BODY: &str = "You are in plan mode. Stay in plan mode until exit_plan_mode succeeds or the user switches the session mode. Imperative language to implement changes means plan the implementation, not execute it. A user's conversational agreement — including an answer confirming something you asked — approves nothing and does not end plan mode; fold the confirmed decision into the plan and submit it through exit_plan_mode.\n\nExplore first. Use non-mutating reads, searches, static analysis, and checks to ground the plan in the actual repository. Do not edit or write files, change configuration, run formatters or code generation that rewrites tracked files, commit, or otherwise carry out the plan. Prefer existing functions and patterns over new machinery.\n\nThe tool catalog stays the same across modes for request-cache stability. These plan-mode rules override any later tool description or guidance that suggests using mutation tools; those tools remain listed to keep the tool catalog unchanged. Do not use todo_write to track this planning phase: it tracks implementation after an approved plan, while the plan itself belongs in exit_plan_mode.\n\nResolve discoverable facts by inspection. Use ask_user_question only for user-owned choices or material ambiguity that inspection cannot answer. Do not ask the user where code lives or how current behavior works when you can find out.\n\nMake the plan decision-complete: state the goal and success criteria; group implementation changes by subsystem; identify public API, schema, and data-flow changes; cover edge cases, failure modes, tests, acceptance criteria, and explicit assumptions. Keep it concise enough to review but detailed enough that another engineer can implement it without making design decisions.\n\nWhen ready, call exit_plan_mode with the complete plan markdown, starting with a # title. Make exit_plan_mode the only and final tool call in that assistant response: it presents the plan for approval, and implementation begins only in a later step after approval. Do not paste the final plan as a plain reply or ask \"should I proceed?\" through prose or ask_user_question. If review rejects it, incorporate the feedback and present again. If the review channel is unavailable or aborted, stay in plan mode and ask the user to switch modes manually; do not proceed with implementation.";

/// active-plan 段正文模板(本仓库扩展段:批准后计划持续注入,压缩折叠后
/// 仍可循;无此段时计划只活在 tool-call/result 历史)。
pub const ACTIVE_PLAN_SECTION_BODY: &str =
    "The user approved this plan. Implement it; track progress with the todo_write tool.";

/// 折叠日志得出的 header prompt 段组(喂 `liuma_prompt::AssembleContext`;
/// 段位由组装器维持:active-plan 在环境段后、plan-mode 在末尾)。
#[derive(Debug, Clone, Default)]
pub struct HeaderPlanSections {
    /// 活跃计划段(最近 `plan/approved`;标准态引导实现)
    pub active: Option<PromptSection>,
    /// plan 模式约束段(当前态为 plan 时)
    pub mode: Option<PromptSection>,
}

/// 折叠共享日志产出 plan 段组。
pub fn header_sections(log: &EventLog) -> HeaderPlanSections {
    let (plan_mode, active_plan) = plan_state(log);
    HeaderPlanSections {
        active: active_plan.map(|plan| PromptSection {
            title: "active-plan".into(),
            body: format!("{ACTIVE_PLAN_SECTION_BODY}\n\n{plan}"),
        }),
        mode: plan_mode.then(|| PromptSection {
            title: "plan-mode".into(),
            body: PLAN_MODE_SECTION_BODY.into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liuma_session::EventEnvelope;
    use serde_json::json;

    fn log_of(events: &[(&str, serde_json::Value)]) -> EventLog {
        let mut log = EventLog::new();
        for (i, (ty, data)) in events.iter().enumerate() {
            log.append(EventEnvelope::new(ty, i as i64, data.clone()))
                .unwrap();
        }
        log
    }

    #[test]
    fn empty_log_yields_no_sections() {
        let s = header_sections(&EventLog::new());
        assert!(s.active.is_none());
        assert!(s.mode.is_none());
    }

    #[test]
    fn plan_mode_renders_policy_section() {
        let log = log_of(&[("session/mode", json!({ "mode": "plan" }))]);
        let s = header_sections(&log);
        assert!(s.active.is_none());
        let mode = s.mode.expect("plan 态应有约束段");
        assert_eq!(mode.title, "plan-mode");
        // 全文关键句在场(逐字锁)
        assert!(
            mode.body
                .contains("Stay in plan mode until exit_plan_mode succeeds")
        );
        assert!(mode.body.contains(
            "A user's conversational agreement — including an answer confirming something you asked — approves nothing"
        ));
        assert!(
            mode.body
                .contains("Do not use todo_write to track this planning phase")
        );
        assert!(mode.body.contains("Make the plan decision-complete"));
        assert!(
            mode.body
                .contains("Make exit_plan_mode the only and final tool call")
        );
        assert!(
            mode.body
                .contains("If review rejects it, incorporate the feedback and present again")
        );
    }

    #[test]
    fn approved_plan_renders_active_section() {
        let log = log_of(&[
            ("session/mode", json!({ "mode": "plan" })),
            ("plan/approved", json!({ "plan": "# fix\n1. step" })),
            ("session/mode", json!({ "mode": "standard" })),
        ]);
        let s = header_sections(&log);
        assert!(s.mode.is_none());
        let active = s.active.expect("批准后应有活跃段");
        assert_eq!(active.title, "active-plan");
        assert!(active.body.contains("todo_write tool"));
        assert!(active.body.contains("# fix\n1. step"));
    }
}
