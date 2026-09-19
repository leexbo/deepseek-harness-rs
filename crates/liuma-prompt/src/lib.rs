//! prompt 组件:system-prompt 组装(纯函数面)。
//!
//! system-prompt 组装保持纯函数性质:输入是数据(身份/环境/预渲染段),
//! 输出是字符串;无 IO、无时钟——天然可重放。plan 段不在此层解析:
//! liuma-plan 折叠日志产出预渲染段(active-plan/plan-mode),此层只管槽位。
//! compaction 策略只立接口。

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
    /// 活跃计划段(已批准计划;由 liuma-plan 折叠日志产出,此层不解析日志)
    pub active_plan_section: Option<PromptSection>,
    /// preset 追加段(声明式组合;渲染为 "# additional" 段)
    pub append: Option<String>,
    /// @file 引用提示(context:file-reference——@ 前缀文件用 read 工具读)
    pub file_reference: Option<String>,
    /// 在场工具的使用指南节(tool:<name> section;装配期收集,
    /// 无标题纯段落)
    pub tool_sections: Vec<String>,
    /// plan 模式约束段(由 liuma-plan 产出;置于段序末尾)
    pub plan_mode_section: Option<PromptSection>,
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
    if let Some(sec) = ctx.active_plan_section.clone() {
        sections.push(sec);
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
    if let Some(sec) = ctx.plan_mode_section.clone() {
        sections.push(sec);
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

    #[test]
    fn assembles_sections_in_order() {
        let ctx = AssembleContext {
            identity: "You are liuma.".into(),
            env_info: "cwd=/tmp".into(),
            active_plan_section: None,
            append: None,
            file_reference: None,
            tool_sections: vec![],
            plan_mode_section: None,
        };
        let out = assemble(&ctx);
        assert!(out.contains("# identity\n\nYou are liuma."));
        assert!(out.contains("# environment\n\ncwd=/tmp"));
    }

    #[test]
    fn append_section_optional() {
        let ctx = AssembleContext {
            identity: "id".into(),
            env_info: String::new(),
            active_plan_section: None,
            append: Some("always answer in Chinese".into()),
            file_reference: None,
            tool_sections: vec![],
            plan_mode_section: None,
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
    fn plan_sections_pass_through_at_fixed_slots() {
        // 段位:active-plan 在环境段后、plan-mode 在末尾(段文本由
        // liuma-plan 产出,此层只管槽位)
        let ctx = AssembleContext {
            identity: "id".into(),
            env_info: "env".into(),
            active_plan_section: Some(PromptSection {
                title: "active-plan".into(),
                body: "approved plan body".into(),
            }),
            append: None,
            file_reference: None,
            tool_sections: vec!["tool guide".into()],
            plan_mode_section: Some(PromptSection {
                title: "plan-mode".into(),
                body: "plan policy body".into(),
            }),
        };
        let out = assemble(&ctx);
        assert!(out.contains("# active-plan\n\napproved plan body"));
        assert!(out.contains("# plan-mode\n\nplan policy body"));
        let active_at = out.find("# active-plan").expect("在场");
        let tool_at = out.find("tool guide").expect("在场");
        let plan_at = out.find("# plan-mode").expect("在场");
        assert!(
            active_at < tool_at && tool_at < plan_at,
            "段序:active-plan→工具节→plan-mode"
        );

        let none = assemble(&AssembleContext {
            active_plan_section: None,
            plan_mode_section: None,
            ..ctx
        });
        assert!(!none.contains("active-plan"));
        assert!(!none.contains("plan-mode"));
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
