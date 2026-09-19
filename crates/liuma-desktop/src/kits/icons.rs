//! 自定义图标与资产合并。
//!
//! gpui-component 0.5.1 未内置的 lucide 图标(ISC,
//! `assets/icons/` 下 24x24 stroke 图;图标集已定格)经 [`LiumaIcon`] + [`IconNamed`]
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

/// liuma 自有图标(仅内置 [`IconName`] 缺失者;内置的直接用 `IconName::X`)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LiumaIcon {
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
    /// hero 品牌流马(assets/logo.svg,设计定稿)
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
    /// 附件入口(输入卡底排独立钮)
    Paperclip,
    // ── 文件类型族(lucide;组件默认 101 图标集缺,自有内嵌)──
    /// 文件树标签/目录树
    FolderTree,
    /// 图片类文件
    FileImage,
    /// 代码类文件
    FileCode,
    /// 压缩包
    FileArchive,
    /// json 系
    FileBraces,
    /// rust(齿轮)
    FileCog,
    /// git 元数据
    FileDiff,
    /// env 系
    FileKey,
    /// 锁文件
    FileLock,
    /// shell 脚本
    FileTerminal,
    /// 表格
    FileSpreadsheet,
    /// 视频
    FileVideoCamera,
    /// 音频
    FileVolume,
    /// symlink/other 行
    FileSymlink,
    /// 图表(ppt 族)
    FileChartColumn,
    /// 通用类型文件(pdf/config/word/字体)
    FileType,
    /// 换行开关(预览头行)
    TextWrap,
}

impl IconNamed for LiumaIcon {
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
            Self::FolderTree => "folder-tree",
            Self::FileImage => "file-image",
            Self::FileCode => "file-code",
            Self::FileArchive => "file-archive",
            Self::FileBraces => "file-braces",
            Self::FileCog => "file-cog",
            Self::FileDiff => "file-diff",
            Self::FileKey => "file-key",
            Self::FileLock => "file-lock",
            Self::FileTerminal => "file-terminal",
            Self::FileSpreadsheet => "file-spreadsheet",
            Self::FileVideoCamera => "file-video-camera",
            Self::FileVolume => "file-volume",
            Self::FileSymlink => "file-symlink",
            Self::FileChartColumn => "file-chart-column",
            Self::FileType => "file-type",
            Self::TextWrap => "text-wrap",
        };
        format!("icons/_liuma/{name}.svg").into()
    }
}

/// 合并资产源:liuma 自有图标(编译期内嵌)优先,其余回落
/// gpui-component 内置资产(86 个 IconName SVG)
pub struct MergedAssets;

/// 自有图标静态表(路径 ↔ SVG 字节,与 [`LiumaIcon::path`] 一一对应)
const LIUMA_ICONS: &[(&str, &[u8])] = &[
    (
        "icons/_liuma/sparkles.svg",
        include_bytes!("../../assets/icons/sparkles.svg"),
    ),
    (
        "icons/_liuma/zap.svg",
        include_bytes!("../../assets/icons/zap.svg"),
    ),
    (
        "icons/_liuma/brain.svg",
        include_bytes!("../../assets/icons/brain.svg"),
    ),
    (
        "icons/_liuma/gauge.svg",
        include_bytes!("../../assets/icons/gauge.svg"),
    ),
    (
        "icons/_liuma/shield.svg",
        include_bytes!("../../assets/icons/shield.svg"),
    ),
    (
        "icons/_liuma/shield-alert.svg",
        include_bytes!("../../assets/icons/shield-alert.svg"),
    ),
    (
        "icons/_liuma/shield-check.svg",
        include_bytes!("../../assets/icons/shield-check.svg"),
    ),
    (
        "icons/_liuma/pencil.svg",
        include_bytes!("../../assets/icons/pencil.svg"),
    ),
    (
        "icons/_liuma/git-branch.svg",
        include_bytes!("../../assets/icons/git-branch.svg"),
    ),
    (
        "icons/_liuma/archive.svg",
        include_bytes!("../../assets/icons/archive.svg"),
    ),
    (
        "icons/_liuma/list-checks.svg",
        include_bytes!("../../assets/icons/list-checks.svg"),
    ),
    (
        "icons/_liuma/message-square.svg",
        include_bytes!("../../assets/icons/message-square.svg"),
    ),
    (
        "icons/_liuma/logo.svg",
        include_bytes!("../../assets/logo.svg"),
    ),
    (
        "icons/_liuma/download.svg",
        include_bytes!("../../assets/icons/download.svg"),
    ),
    (
        "icons/_liuma/clock.svg",
        include_bytes!("../../assets/icons/clock.svg"),
    ),
    (
        "icons/_liuma/code.svg",
        include_bytes!("../../assets/icons/code.svg"),
    ),
    (
        "icons/_liuma/circle-help.svg",
        include_bytes!("../../assets/icons/circle-help.svg"),
    ),
    (
        "icons/_liuma/target.svg",
        include_bytes!("../../assets/icons/target.svg"),
    ),
    (
        "icons/_liuma/briefcase.svg",
        include_bytes!("../../assets/icons/briefcase.svg"),
    ),
    (
        "icons/_liuma/workflow.svg",
        include_bytes!("../../assets/icons/workflow.svg"),
    ),
    (
        "icons/_liuma/infinity.svg",
        include_bytes!("../../assets/icons/infinity.svg"),
    ),
    (
        "icons/_liuma/monitor.svg",
        include_bytes!("../../assets/icons/monitor.svg"),
    ),
    (
        "icons/_liuma/agent-preset.svg",
        include_bytes!("../../assets/icons/agent-preset.svg"),
    ),
    (
        "icons/_liuma/refresh-cw.svg",
        include_bytes!("../../assets/icons/refresh-cw.svg"),
    ),
    (
        "icons/_liuma/wrench.svg",
        include_bytes!("../../assets/icons/wrench.svg"),
    ),
    (
        "icons/_liuma/paperclip.svg",
        include_bytes!("../../assets/icons/paperclip.svg"),
    ),
    (
        "icons/_liuma/folder-tree.svg",
        include_bytes!("../../assets/icons/folder-tree.svg"),
    ),
    (
        "icons/_liuma/file-image.svg",
        include_bytes!("../../assets/icons/file-image.svg"),
    ),
    (
        "icons/_liuma/file-code.svg",
        include_bytes!("../../assets/icons/file-code.svg"),
    ),
    (
        "icons/_liuma/file-archive.svg",
        include_bytes!("../../assets/icons/file-archive.svg"),
    ),
    (
        "icons/_liuma/file-braces.svg",
        include_bytes!("../../assets/icons/file-braces.svg"),
    ),
    (
        "icons/_liuma/file-cog.svg",
        include_bytes!("../../assets/icons/file-cog.svg"),
    ),
    (
        "icons/_liuma/file-diff.svg",
        include_bytes!("../../assets/icons/file-diff.svg"),
    ),
    (
        "icons/_liuma/file-key.svg",
        include_bytes!("../../assets/icons/file-key.svg"),
    ),
    (
        "icons/_liuma/file-lock.svg",
        include_bytes!("../../assets/icons/file-lock.svg"),
    ),
    (
        "icons/_liuma/file-terminal.svg",
        include_bytes!("../../assets/icons/file-terminal.svg"),
    ),
    (
        "icons/_liuma/file-spreadsheet.svg",
        include_bytes!("../../assets/icons/file-spreadsheet.svg"),
    ),
    (
        "icons/_liuma/file-video-camera.svg",
        include_bytes!("../../assets/icons/file-video-camera.svg"),
    ),
    (
        "icons/_liuma/file-volume.svg",
        include_bytes!("../../assets/icons/file-volume.svg"),
    ),
    (
        "icons/_liuma/file-symlink.svg",
        include_bytes!("../../assets/icons/file-symlink.svg"),
    ),
    (
        "icons/_liuma/file-chart-column.svg",
        include_bytes!("../../assets/icons/file-chart-column.svg"),
    ),
    (
        "icons/_liuma/file-type.svg",
        include_bytes!("../../assets/icons/file-type.svg"),
    ),
    (
        "icons/_liuma/text-wrap.svg",
        include_bytes!("../../assets/icons/text-wrap.svg"),
    ),
];

impl AssetSource for MergedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, data)) = LIUMA_ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(data)));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut out = gpui_kit::assets::Assets.list(path)?;
        out.extend(
            LIUMA_ICONS
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

/// 工具行图标(14px)。工具名权威域 = liuma-tools 各 `spec()` 共 12 个;
/// 另含 web/旧夹具遗留别名与未注册工具兜底(Sparkles)
pub fn tool_icon(name: &str) -> Icon {
    fixed_path(tool_path(name), 14.)
}

fn tool_path(name: &str) -> SharedString {
    match name {
        "bash" => IconName::SquareTerminal.path(),
        "file_read" | "read" | "web_fetch" => IconName::BookOpen.path(),
        "file_edit" | "edit" | "write" => LiumaIcon::Pencil.path(),
        "file_search" | "grep" | "glob" => IconName::Search.path(),
        "web_search" => IconName::Globe.path(),
        "todo_write" | "exit_plan_mode" => LiumaIcon::ListChecks.path(),
        "ask" => LiumaIcon::CircleHelp.path(),
        "code" => LiumaIcon::Code.path(),
        "goal" => LiumaIcon::Target.path(),
        "jobs" => LiumaIcon::Briefcase.path(),
        "workflow" => LiumaIcon::Workflow.path(),
        "ralph" => LiumaIcon::Infinity.path(),
        "subagent" => IconName::Bot.path(),
        "subagent_list" => IconName::GalleryVerticalEnd.path(),
        _ => LiumaIcon::Sparkles.path(),
    }
}

/// 权限 chip 图标(13px;read-only→Shield / full-access→ShieldAlert /
/// 其余(含 workspace-write)→ShieldCheck)
pub fn permission_icon(mode: &str) -> Icon {
    fixed_path(permission_path(mode), 13.)
}

fn permission_path(mode: &str) -> SharedString {
    match mode {
        "read-only" => LiumaIcon::Shield,
        "full-access" => LiumaIcon::ShieldAlert,
        _ => LiumaIcon::ShieldCheck,
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
        LiumaIcon::Zap
    } else if m.contains("pro") || m.contains("reasoner") {
        LiumaIcon::Brain
    } else {
        LiumaIcon::Gauge
    };
    icon.path()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 枚举全变体的 path 必须命中 LIUMA_ICONS 静态表(防加枚举忘加 SVG)
    #[test]
    fn liuma_icon_paths_all_embedded() {
        const ALL: [LiumaIcon; 43] = [
            LiumaIcon::Sparkles,
            LiumaIcon::Zap,
            LiumaIcon::Brain,
            LiumaIcon::Gauge,
            LiumaIcon::Shield,
            LiumaIcon::ShieldAlert,
            LiumaIcon::ShieldCheck,
            LiumaIcon::Pencil,
            LiumaIcon::GitBranch,
            LiumaIcon::Archive,
            LiumaIcon::ListChecks,
            LiumaIcon::MessageSquare,
            LiumaIcon::Logo,
            LiumaIcon::Download,
            LiumaIcon::Clock,
            LiumaIcon::Code,
            LiumaIcon::CircleHelp,
            LiumaIcon::Target,
            LiumaIcon::Briefcase,
            LiumaIcon::Workflow,
            LiumaIcon::Infinity,
            LiumaIcon::Monitor,
            LiumaIcon::AgentPreset,
            LiumaIcon::RefreshCw,
            LiumaIcon::Wrench,
            LiumaIcon::Paperclip,
            LiumaIcon::FolderTree,
            LiumaIcon::FileImage,
            LiumaIcon::FileCode,
            LiumaIcon::FileArchive,
            LiumaIcon::FileBraces,
            LiumaIcon::FileCog,
            LiumaIcon::FileDiff,
            LiumaIcon::FileKey,
            LiumaIcon::FileLock,
            LiumaIcon::FileTerminal,
            LiumaIcon::FileSpreadsheet,
            LiumaIcon::FileVideoCamera,
            LiumaIcon::FileVolume,
            LiumaIcon::FileSymlink,
            LiumaIcon::FileChartColumn,
            LiumaIcon::FileType,
            LiumaIcon::TextWrap,
        ];
        for icon in ALL {
            let p = icon.path();
            assert!(
                LIUMA_ICONS
                    .iter()
                    .any(|(ep, data)| p == *ep && !data.is_empty()),
                "{icon:?} path {p:?} 未在 LIUMA_ICONS 静态表或内容为空"
            );
        }
        // 反向:静态表条目也应对得上某个枚举变体(防残留孤儿资产)
        for (ep, _) in LIUMA_ICONS {
            assert!(
                ALL.iter().any(|i| i.path().as_ref() == *ep),
                "静态表条目 {ep:?} 无对应枚举变体"
            );
        }
    }

    /// 工具名映射:12 个 spec 工具 + web 遗留别名 + 兜底,逐条断言资产路径
    #[test]
    fn tool_icon_covers_all_spec_tools() {
        // (工具名, 期望资产路径);前 12 项 = liuma-tools spec 权威域
        let expected: &[(&str, &str)] = &[
            ("bash", "icons/square-terminal.svg"),
            ("file_read", "icons/book-open.svg"),
            ("file_edit", "icons/_liuma/pencil.svg"),
            ("file_search", "icons/search.svg"),
            ("todo_write", "icons/_liuma/list-checks.svg"),
            ("exit_plan_mode", "icons/_liuma/list-checks.svg"),
            ("goal", "icons/_liuma/target.svg"),
            ("jobs", "icons/_liuma/briefcase.svg"),
            ("workflow", "icons/_liuma/workflow.svg"),
            ("ralph", "icons/_liuma/infinity.svg"),
            ("subagent", "icons/bot.svg"),
            ("subagent_list", "icons/gallery-vertical-end.svg"),
            // 遗留/扩展别名
            ("read", "icons/book-open.svg"),
            ("web_fetch", "icons/book-open.svg"),
            ("edit", "icons/_liuma/pencil.svg"),
            ("write", "icons/_liuma/pencil.svg"),
            ("grep", "icons/search.svg"),
            ("glob", "icons/search.svg"),
            ("web_search", "icons/globe.svg"),
            ("ask", "icons/_liuma/circle-help.svg"),
            ("code", "icons/_liuma/code.svg"),
            // 兜底
            ("mystery-tool", "icons/_liuma/sparkles.svg"),
        ];
        for (name, path) in expected {
            assert_eq!(tool_path(name).as_ref(), *path, "工具 {name} 图标映射不符");
        }
    }

    /// 合并资产源:自有命中 + 内置回落
    #[test]
    fn merged_assets_fallback() {
        let own = MergedAssets
            .load("icons/_liuma/sparkles.svg")
            .expect("自有图标应可加载");
        assert!(own.is_some());
        let builtin = MergedAssets
            .load("icons/arrow-up.svg")
            .expect("内置图标应经回落加载");
        assert!(builtin.is_some());
        // list 合并两边
        let names = MergedAssets.list("icons/_liuma/").expect("list 不应失败");
        assert_eq!(names.len(), LIUMA_ICONS.len());
    }

    /// 权限 / 模型两组语义映射 + trio 模式图标路径
    #[test]
    fn preset_permission_model_mapping() {
        assert_eq!(
            permission_path("read-only").as_ref(),
            "icons/_liuma/shield.svg"
        );
        assert_eq!(
            permission_path("workspace-write").as_ref(),
            "icons/_liuma/shield-check.svg"
        );
        assert_eq!(
            permission_path("full-access").as_ref(),
            "icons/_liuma/shield-alert.svg"
        );

        assert_eq!(
            model_path("gemini-2.5-flash").as_ref(),
            "icons/_liuma/zap.svg"
        );
        assert_eq!(
            model_path("deepseek-reasoner").as_ref(),
            "icons/_liuma/brain.svg"
        );
        assert_eq!(
            model_path("DeepSeek-V4-Pro").as_ref(),
            "icons/_liuma/brain.svg"
        );
        assert_eq!(model_path("deepseek-v4").as_ref(), "icons/_liuma/gauge.svg");
        // trio preset 模式图标
        assert_eq!(
            LiumaIcon::AgentPreset.path().as_ref(),
            "icons/_liuma/agent-preset.svg"
        );
    }
}
