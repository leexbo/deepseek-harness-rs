//! prompt 组件:system-prompt 组装(纯函数面)。
//!
//! system-prompt 组装保持纯函数性质:输入是数据(身份/环境/计划态),
//! 输出是字符串;无 IO、无时钟——天然可重放。
//! plan-mode 折叠与 skill 渲染此阶段为最小实现,compaction 策略只立接口。

use serde_json::Value;

/// 一个提示词段(标题 + 内容;渲染为 Markdown 分段)
#[derive(Debug, Clone, PartialEq)]
pub struct PromptSection {
    /// 段标题
    pub title: String,
    /// 段内容
    pub body: String,
}

/// 组装上下文(纯数据)
#[derive(Debug, Clone, Default)]
pub struct AssembleContext {
    /// 身份段(产品自述/行为约束)
    pub identity: String,
    /// 环境段(cwd/平台/日期等;由宿主注入,组件不读时钟)
    pub env_info: String,
    /// 指令文件内容(AGENTS.md;由宿主读取注入,prompt 保持纯函数)
    /// 计划模式(折叠为一段约束)
    pub plan_mode: bool,
    /// 计划内容(plan_mode 时渲染)
    pub plan: Option<Value>,
    /// 已批准的活跃计划(标准模式渲染为 active-plan 段;引导实现)
    pub active_plan: Option<String>,
    /// preset 追加段(声明式组合;渲染为 "# additional" 段)
    pub append: Option<String>,
    /// @file 引用提示(context:file-reference——@ 前缀文件用 read 工具读)
    pub file_reference: Option<String>,
    /// 在场工具的使用指南节(tool:<name> section;装配期收集,
    /// 无标题纯段落)
    pub tool_sections: Vec<String>,
}

/// 组装 system prompt:段依序拼接,Markdown 分段(XML-ish 标题风格)。
pub fn assemble(ctx: &AssembleContext) -> String {
    let mut sections = vec![PromptSection {
        title: "identity".into(),
        body: ctx.identity.clone(),
    }];
    if !ctx.env_info.is_empty() {
        sections.push(PromptSection {
            title: "environment".into(),
            body: ctx.env_info.clone(),
        });
    }
    if let Some(active) = ctx.active_plan.as_ref().filter(|s| !s.is_empty()) {
        sections.push(PromptSection {
            title: "active-plan".into(),
            body: format!("The user approved this plan. Implement it; track progress with the todo tool.\n\n{active}"),
        });
    }
    if let Some(extra) = ctx.append.as_ref().filter(|s| !s.is_empty()) {
        sections.push(PromptSection {
            title: "additional".into(),
            body: extra.clone(),
        });
    }
    if let Some(hint) = ctx.file_reference.as_ref().filter(|s| !s.is_empty()) {
        sections.push(PromptSection {
            title: "file-reference".into(),
            body: hint.clone(),
        });
    }
    // 工具指南节(tool:<name> sections:纯段落,排在工具目录语义位)
    for ts in &ctx.tool_sections {
        if !ts.is_empty() {
            sections.push(PromptSection {
                title: String::new(),
                body: ts.clone(),
            });
        }
    }
    if ctx.plan_mode {
        sections.push(render_plan(ctx.plan.as_ref()));
    }
    render(&sections)
}

/// 渲染:标题段化拼接(标题空 = 无标题纯段落,同 section 形态)
pub fn render(sections: &[PromptSection]) -> String {
    sections
        .iter()
        .map(|s| {
            if s.title.is_empty() {
                format!("{}\n", s.body)
            } else {
                format!("# {}\n\n{}\n", s.title, s.body)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// plan-mode 折叠:研究约束 + 经 exit_plan_mode 提交计划的通道
/// (工具目录跨模式不变,request-cache 稳定;
/// 禁止变更由本段约束,不撤目录)
fn render_plan(plan: Option<&Value>) -> PromptSection {
    let body = match plan {
        Some(p) => format!(
            "You are in plan mode. Stay in plan mode until the user approves a plan through exit_plan_mode. Imperative language means plan the change, not execute it.\n\nDraft context so far:\n{}",
            serde_json::to_string_pretty(p).unwrap_or_default()
        ),
        None => "You are in plan mode. Explore first with non-mutating tools (file_read, file_search, read-only commands). Do not edit files or run mutating commands; imperative language means plan the change, not execute it. When ready, call exit_plan_mode with the complete plan as markdown (goal, steps grouped by subsystem, tests, risks). exit_plan_mode must be the only tool call in that response.".to_string(),
    };
    PromptSection {
        title: "plan-mode".into(),
        body,
    }
}

/// compaction 策略接口(骨架,compaction 语义定稿时扩展)
pub trait CompactionPolicy {
    /// 判断是否应压缩(输入:当前消息数/估算 token)
    fn should_compact(&self, message_count: usize, estimated_tokens: u64) -> bool;
}

/// 简单阈值策略(占位实现:token 上限的 80%)
pub struct ThresholdPolicy {
    /// token 上限
    pub limit: u64,
}

impl CompactionPolicy for ThresholdPolicy {
    fn should_compact(&self, _message_count: usize, estimated_tokens: u64) -> bool {
        estimated_tokens >= self.limit * 4 / 5
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assembles_sections_in_order() {
        let ctx = AssembleContext {
            identity: "You are dsh.".into(),
            env_info: "cwd=/tmp".into(),
            plan_mode: false,
            plan: None,
            active_plan: None,
            append: None,
            file_reference: None,
            tool_sections: vec![],
        };
        let out = assemble(&ctx);
        assert!(out.contains("# identity\n\nYou are dsh."));
        assert!(out.contains("# environment\n\ncwd=/tmp"));
        assert!(!out.contains("plan-mode"));
    }

    #[test]
    fn append_section_optional() {
        let ctx = AssembleContext {
            identity: "id".into(),
            env_info: String::new(),
            plan_mode: false,
            plan: None,
            active_plan: None,
            append: Some("always answer in Chinese".into()),
            file_reference: None,
            tool_sections: vec![],
        };
        let out = assemble(&ctx);
        assert!(out.contains("# additional"));
        assert!(out.contains("always answer in Chinese"));

        let none = assemble(&AssembleContext {
            append: None,
            ..ctx
        });
        assert!(!none.contains("# additional"));
    }

    #[test]
    fn active_plan_section_renders() {
        let ctx = AssembleContext {
            identity: "id".into(),
            env_info: String::new(),
            plan_mode: false,
            plan: None,
            active_plan: Some("1. do the thing".into()),
            append: None,
            file_reference: None,
            tool_sections: vec![],
        };
        let out = assemble(&ctx);
        assert!(out.contains("# active-plan"));
        assert!(out.contains("1. do the thing"));
        let none = assemble(&AssembleContext {
            active_plan: None,
            ..ctx
        });
        assert!(!none.contains("# active-plan"));
    }

    #[test]
    fn plan_mode_folds_in() {
        let ctx = AssembleContext {
            identity: "id".into(),
            env_info: String::new(),
            plan_mode: true,
            plan: Some(json!({ "steps": ["a", "b"] })),
            active_plan: None,
            append: None,
            file_reference: None,
            tool_sections: vec![],
        };
        let out = assemble(&ctx);
        assert!(out.contains("# plan-mode"));
        assert!(out.contains("\"steps\""));
    }

    #[test]
    fn tool_sections_render_as_untitled_paragraphs() {
        // tool:<name> section 形态:纯段落,无 "# 标题"
        let ctx = AssembleContext {
            tool_sections: vec!["Section A content.".into()],
            ..Default::default()
        };
        let out = assemble(&ctx);
        assert!(out.contains("Section A content."));
        assert!(!out.contains("# Section A content."));
        // 空节跳过
        let empty = assemble(&AssembleContext {
            tool_sections: vec![String::new()],
            ..Default::default()
        });
        assert_eq!(empty.trim_end(), "# identity");
    }

    #[test]
    fn threshold_policy() {
        let p = ThresholdPolicy { limit: 1000 };
        assert!(!p.should_compact(10, 700));
        assert!(p.should_compact(10, 800));
    }

    #[test]
    fn file_reference_section_renders_when_set() {
        let ctx = AssembleContext {
            file_reference: Some("@ prefix = use read tool".into()),
            ..Default::default()
        };
        let out = assemble(&ctx);
        assert!(out.contains("# file-reference"));
        assert!(out.contains("use read tool"));
        // 缺省不注入
        let none = assemble(&AssembleContext::default());
        assert!(!none.contains("# file-reference"));
    }
}
