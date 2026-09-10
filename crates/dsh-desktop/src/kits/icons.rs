//! 自定义图标与资产合并。
//!
//! gpui-component 0.5.1 未内置的 lucide 图标(ISC,
//! `assets/icons/` 下 24x24 stroke 图;图标集已定格)经 [`DshIcon`] + [`IconNamed`]
//! 提供;资产经 [`MergedAssets`](自有 include_bytes! 静态表优先,
//! miss 回落 gpui-component 内置),零新增依赖。
//!
//! 尺寸纪律:0.5.1 的 `with_size` 经 render 链恒覆盖
//! em 回退,独立渲染的 Icon 尺寸确定性成立;但宿主组件(Button/
//! Tab/Select)会强制覆写传入 icon 的尺寸——本 crate 全自绘 div,
//! 不受影响。所有 Icon 一律经 [`fixed`] 定尺寸。

use gpui_kit::component::{Icon, IconName, IconNamed, Sizable};
use gpui_kit::{AssetSource, Result, SharedString, px};
use std::borrow::Cow;

/// dsh 自有图标(仅内置 [`IconName`] 缺失者;内置的直接用 `IconName::X`)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DshIcon {
    /// preset `standard` 徽章 / 未注册工具兜底
    Sparkles,
    /// preset 非 `standard` / flash 系模型
    Zap,
    /// pro·reasoner 系模型 / Think 折叠行
    Brain,
    /// 其余模型
    Gauge,
    /// 权限 `read-only`
    Shield,
    /// 权限 `full-access`
    ShieldAlert,
    /// 权限 `workspace-write`
    ShieldCheck,
    /// 重命名 / 文件编辑类工具
    Pencil,
    /// 分叉
    GitBranch,
    /// 归档
    Archive,
    /// 计划 / todo 类
    ListChecks,
    /// 会话行
    MessageSquare,
    /// hero 品牌鲸鱼(assets/logo.svg,设计定稿)
    Logo,
    /// Session log 导出
    Download,
    /// Duration 切换(轨迹工具栏)
    Clock,
    /// code 类工具
    Code,
    /// 问答类工具
    CircleHelp,
    /// `goal` 工具
    Target,
    /// `jobs` 工具
    Briefcase,
    /// `workflow` 工具
    Workflow,
    /// `ralph` 工具
    Infinity,
    /// 外观「跟随系统」cube(显示器)
    Monitor,
    /// preset 模式(最简/标准)徽章 — trio(agent preset outline 16)
    AgentPreset,
    /// LLM 请求重试行(lucide refresh-cw)
    RefreshCw,
    /// 助手消息工具调用行(lucide wrench;统一扳手形)
    Wrench,
    /// 附件入口(命令菜单「图片附件」行)
    Paperclip,
}

impl IconNamed for DshIcon {
    fn path(self) -> SharedString {
        let name = match self {
            Self::Sparkles => "sparkles",
            Self::Zap => "zap",
            Self::Brain => "brain",
            Self::Gauge => "gauge",
            Self::Shield => "shield",
            Self::ShieldAlert => "shield-alert",
            Self::ShieldCheck => "shield-check",
            Self::Pencil => "pencil",
            Self::GitBranch => "git-branch",
            Self::Archive => "archive",
            Self::ListChecks => "list-checks",
            Self::MessageSquare => "message-square",
            Self::Logo => "logo",
            Self::Download => "download",
            Self::Clock => "clock",
            Self::Code => "code",
            Self::CircleHelp => "circle-help",
            Self::Target => "target",
            Self::Briefcase => "briefcase",
            Self::Workflow => "workflow",
            Self::Infinity => "infinity",
            Self::Monitor => "monitor",
            Self::AgentPreset => "agent-preset",
            Self::RefreshCw => "refresh-cw",
            Self::Wrench => "wrench",
            Self::Paperclip => "paperclip",
        };
        format!("icons/_dsh/{name}.svg").into()
    }
}

/// 合并资产源:dsh 自有图标(编译期内嵌)优先,其余回落
/// gpui-component 内置资产(86 个 IconName SVG)
pub struct MergedAssets;

/// 自有图标静态表(路径 ↔ SVG 字节,与 [`DshIcon::path`] 一一对应)
const DSH_ICONS: &[(&str, &[u8])] = &[
    (
        "icons/_dsh/sparkles.svg",
        include_bytes!("../../assets/icons/sparkles.svg"),
    ),
    (
        "icons/_dsh/zap.svg",
        include_bytes!("../../assets/icons/zap.svg"),
    ),
    (
        "icons/_dsh/brain.svg",
        include_bytes!("../../assets/icons/brain.svg"),
    ),
    (
        "icons/_dsh/gauge.svg",
        include_bytes!("../../assets/icons/gauge.svg"),
    ),
    (
        "icons/_dsh/shield.svg",
        include_bytes!("../../assets/icons/shield.svg"),
    ),
    (
        "icons/_dsh/shield-alert.svg",
        include_bytes!("../../assets/icons/shield-alert.svg"),
    ),
    (
        "icons/_dsh/shield-check.svg",
        include_bytes!("../../assets/icons/shield-check.svg"),
    ),
    (
        "icons/_dsh/pencil.svg",
        include_bytes!("../../assets/icons/pencil.svg"),
    ),
    (
        "icons/_dsh/git-branch.svg",
        include_bytes!("../../assets/icons/git-branch.svg"),
    ),
    (
        "icons/_dsh/archive.svg",
        include_bytes!("../../assets/icons/archive.svg"),
    ),
    (
        "icons/_dsh/list-checks.svg",
        include_bytes!("../../assets/icons/list-checks.svg"),
    ),
    (
        "icons/_dsh/message-square.svg",
        include_bytes!("../../assets/icons/message-square.svg"),
    ),
    (
        "icons/_dsh/logo.svg",
        include_bytes!("../../assets/logo.svg"),
    ),
    (
        "icons/_dsh/download.svg",
        include_bytes!("../../assets/icons/download.svg"),
    ),
    (
        "icons/_dsh/clock.svg",
        include_bytes!("../../assets/icons/clock.svg"),
    ),
    (
        "icons/_dsh/code.svg",
        include_bytes!("../../assets/icons/code.svg"),
    ),
    (
        "icons/_dsh/circle-help.svg",
        include_bytes!("../../assets/icons/circle-help.svg"),
    ),
    (
        "icons/_dsh/target.svg",
        include_bytes!("../../assets/icons/target.svg"),
    ),
    (
        "icons/_dsh/briefcase.svg",
        include_bytes!("../../assets/icons/briefcase.svg"),
    ),
    (
        "icons/_dsh/workflow.svg",
        include_bytes!("../../assets/icons/workflow.svg"),
    ),
    (
        "icons/_dsh/infinity.svg",
        include_bytes!("../../assets/icons/infinity.svg"),
    ),
    (
        "icons/_dsh/monitor.svg",
        include_bytes!("../../assets/icons/monitor.svg"),
    ),
    (
        "icons/_dsh/agent-preset.svg",
        include_bytes!("../../assets/icons/agent-preset.svg"),
    ),
    (
        "icons/_dsh/refresh-cw.svg",
        include_bytes!("../../assets/icons/refresh-cw.svg"),
    ),
    (
        "icons/_dsh/wrench.svg",
        include_bytes!("../../assets/icons/wrench.svg"),
    ),
    (
        "icons/_dsh/paperclip.svg",
        include_bytes!("../../assets/icons/paperclip.svg"),
    ),
];

impl AssetSource for MergedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, data)) = DSH_ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(data)));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut out = gpui_kit::assets::Assets.list(path)?;
        out.extend(
            DSH_ICONS
                .iter()
                .filter(|(p, _)| p.starts_with(path))
                .map(|(p, _)| SharedString::from(*p)),
        );
        Ok(out)
    }
}

/// 定尺寸图标(独立渲染必经此处,见模块文档尺寸纪律)
pub fn fixed(icon: impl Into<Icon>, size: f32) -> Icon {
    Icon::new(icon).with_size(px(size))
}

/// 按资产路径直接构造定尺寸图标(内置/自有图标统一出口)
fn fixed_path(path: SharedString, size: f32) -> Icon {
    Icon::empty().path(path).with_size(px(size))
}

/// 工具行图标(14px)。工具名权威域 = dsh-tools 各 `spec()` 共 12 个;
/// 另含 web/旧夹具遗留别名与未注册工具兜底(Sparkles)
pub fn tool_icon(name: &str) -> Icon {
    fixed_path(tool_path(name), 14.)
}

fn tool_path(name: &str) -> SharedString {
    match name {
        "bash" => IconName::SquareTerminal.path(),
        "file_read" | "read" | "web_fetch" => IconName::BookOpen.path(),
        "file_edit" | "edit" | "write" => DshIcon::Pencil.path(),
        "file_search" | "grep" | "glob" => IconName::Search.path(),
        "web_search" => IconName::Globe.path(),
        "todo_write" | "exit_plan_mode" => DshIcon::ListChecks.path(),
        "ask" => DshIcon::CircleHelp.path(),
        "code" => DshIcon::Code.path(),
        "goal" => DshIcon::Target.path(),
        "jobs" => DshIcon::Briefcase.path(),
        "workflow" => DshIcon::Workflow.path(),
        "ralph" => DshIcon::Infinity.path(),
        "subagent" => IconName::Bot.path(),
        "subagent_list" => IconName::GalleryVerticalEnd.path(),
        _ => DshIcon::Sparkles.path(),
    }
}

/// 权限 chip 图标(13px;read-only→Shield / full-access→ShieldAlert /
/// 其余(含 workspace-write)→ShieldCheck)
pub fn permission_icon(mode: &str) -> Icon {
    fixed_path(permission_path(mode), 13.)
}

fn permission_path(mode: &str) -> SharedString {
    match mode {
        "read-only" => DshIcon::Shield,
        "full-access" => DshIcon::ShieldAlert,
        _ => DshIcon::ShieldCheck,
    }
    .path()
}

/// 模型 chip 图标(13px;名含 flash→Zap / 含 pro·reasoner→Brain /
/// 其余→Gauge)
pub fn model_icon(model: &str) -> Icon {
    fixed_path(model_path(model), 13.)
}

fn model_path(model: &str) -> SharedString {
    let m = model.to_lowercase();
    let icon = if m.contains("flash") {
        DshIcon::Zap
    } else if m.contains("pro") || m.contains("reasoner") {
        DshIcon::Brain
    } else {
        DshIcon::Gauge
    };
    icon.path()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 枚举全变体的 path 必须命中 DSH_ICONS 静态表(防加枚举忘加 SVG)
    #[test]
    fn dsh_icon_paths_all_embedded() {
        const ALL: [DshIcon; 26] = [
            DshIcon::Sparkles,
            DshIcon::Zap,
            DshIcon::Brain,
            DshIcon::Gauge,
            DshIcon::Shield,
            DshIcon::ShieldAlert,
            DshIcon::ShieldCheck,
            DshIcon::Pencil,
            DshIcon::GitBranch,
            DshIcon::Archive,
            DshIcon::ListChecks,
            DshIcon::MessageSquare,
            DshIcon::Logo,
            DshIcon::Download,
            DshIcon::Clock,
            DshIcon::Code,
            DshIcon::CircleHelp,
            DshIcon::Target,
            DshIcon::Briefcase,
            DshIcon::Workflow,
            DshIcon::Infinity,
            DshIcon::Monitor,
            DshIcon::AgentPreset,
            DshIcon::RefreshCw,
            DshIcon::Wrench,
            DshIcon::Paperclip,
        ];
        for icon in ALL {
            let p = icon.path();
            assert!(
                DSH_ICONS
                    .iter()
                    .any(|(ep, data)| p == *ep && !data.is_empty()),
                "{icon:?} path {p:?} 未在 DSH_ICONS 静态表或内容为空"
            );
        }
        // 反向:静态表条目也应对得上某个枚举变体(防残留孤儿资产)
        for (ep, _) in DSH_ICONS {
            assert!(
                ALL.iter().any(|i| i.path().as_ref() == *ep),
                "静态表条目 {ep:?} 无对应枚举变体"
            );
        }
    }

    /// 工具名映射:12 个 spec 工具 + web 遗留别名 + 兜底,逐条断言资产路径
    #[test]
    fn tool_icon_covers_all_spec_tools() {
        // (工具名, 期望资产路径);前 12 项 = dsh-tools spec 权威域
        let expected: &[(&str, &str)] = &[
            ("bash", "icons/square-terminal.svg"),
            ("file_read", "icons/book-open.svg"),
            ("file_edit", "icons/_dsh/pencil.svg"),
            ("file_search", "icons/search.svg"),
            ("todo_write", "icons/_dsh/list-checks.svg"),
            ("exit_plan_mode", "icons/_dsh/list-checks.svg"),
            ("goal", "icons/_dsh/target.svg"),
            ("jobs", "icons/_dsh/briefcase.svg"),
            ("workflow", "icons/_dsh/workflow.svg"),
            ("ralph", "icons/_dsh/infinity.svg"),
            ("subagent", "icons/bot.svg"),
            ("subagent_list", "icons/gallery-vertical-end.svg"),
            // 遗留/扩展别名
            ("read", "icons/book-open.svg"),
            ("web_fetch", "icons/book-open.svg"),
            ("edit", "icons/_dsh/pencil.svg"),
            ("write", "icons/_dsh/pencil.svg"),
            ("grep", "icons/search.svg"),
            ("glob", "icons/search.svg"),
            ("web_search", "icons/globe.svg"),
            ("ask", "icons/_dsh/circle-help.svg"),
            ("code", "icons/_dsh/code.svg"),
            // 兜底
            ("mystery-tool", "icons/_dsh/sparkles.svg"),
        ];
        for (name, path) in expected {
            assert_eq!(tool_path(name).as_ref(), *path, "工具 {name} 图标映射不符");
        }
    }

    /// 合并资产源:自有命中 + 内置回落
    #[test]
    fn merged_assets_fallback() {
        let own = MergedAssets
            .load("icons/_dsh/sparkles.svg")
            .expect("自有图标应可加载");
        assert!(own.is_some());
        let builtin = MergedAssets
            .load("icons/arrow-up.svg")
            .expect("内置图标应经回落加载");
        assert!(builtin.is_some());
        // list 合并两边
        let names = MergedAssets.list("icons/_dsh/").expect("list 不应失败");
        assert_eq!(names.len(), DSH_ICONS.len());
    }

    /// 权限 / 模型两组语义映射 + trio 模式图标路径
    #[test]
    fn preset_permission_model_mapping() {
        assert_eq!(
            permission_path("read-only").as_ref(),
            "icons/_dsh/shield.svg"
        );
        assert_eq!(
            permission_path("workspace-write").as_ref(),
            "icons/_dsh/shield-check.svg"
        );
        assert_eq!(
            permission_path("full-access").as_ref(),
            "icons/_dsh/shield-alert.svg"
        );

        assert_eq!(
            model_path("gemini-2.5-flash").as_ref(),
            "icons/_dsh/zap.svg"
        );
        assert_eq!(
            model_path("deepseek-reasoner").as_ref(),
            "icons/_dsh/brain.svg"
        );
        assert_eq!(
            model_path("DeepSeek-V4-Pro").as_ref(),
            "icons/_dsh/brain.svg"
        );
        assert_eq!(model_path("deepseek-v4").as_ref(), "icons/_dsh/gauge.svg");
        // trio preset 模式图标
        assert_eq!(
            DshIcon::AgentPreset.path().as_ref(),
            "icons/_dsh/agent-preset.svg"
        );
    }
}
