//! 顶栏 = 窗口自绘标题栏内容行:侧栏缩进钮 + 工作区下拉 + git 分支
//! (StatusBar 迁入)+ 居中会话标题。轨迹页已迁右栏面板标签;
//! Session log 导出已移入侧栏会话行 ⋯ 菜单。

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_kit::component::IconName;

use crate::kits::icons::{DshIcon, fixed};
use crate::kits::theme;
use crate::shell::metrics::RUN_CLOCK_AFTER_SECS;
use crate::shell::store::AppStore;

/// 标题栏内容行(置于 TitleBar 内):工作区下拉 + git 分支 …… 会话
/// 标题**真居中**(对称内缩绝对区,避开两侧控件)。Session log 导出已
/// 移入侧栏会话行 ⋯ 菜单;分支自 StatusBar 迁入(工作区名右侧)。
/// 标题居中区两侧内缩(须 ≥ 左组[缩进钮+工作区钮+分支徽标]的最宽形态,
/// 对称取 400 → 居中不被遮挡;右侧已无控件,内缩仅服务真居中)
const TITLE_INSET: f32 = 400.;

pub fn title_bar_row(store: &Entity<AppStore>, window: &mut Window, cx: &App) -> impl IntoElement {
    let st = store.read(cx);
    let title = st
        .state
        .current_id
        .as_deref()
        .map(|id| st.title_for(id))
        .unwrap_or_else(|| "dsh".into());
    let branch = st.active_branch();
    // 运行时长(标题右侧实时「N 分钟」,形如「标题｜1分钟」)。
    // 阈值见 metrics::RUN_CLOCK_AFTER_SECS。
    let run_label = st
        .state
        .current_id
        .clone()
        .and_then(|id| st.is_running(&id).then_some(id))
        .and_then(|id| st.run_elapsed(&id))
        .filter(|d| *d >= std::time::Duration::from_secs(RUN_CLOCK_AFTER_SECS))
        .map(crate::shell::reducer::format_run_duration);
    // 收起态(侧栏完全隐藏)交通灯悬于 BASE 画布:三灯带约到 x=72
    // (macOS 标准 close/min/zoom,左缘 20 起三个 12px 圆);标题栏自窗
    // 缘起、仅 8px 内边距会让折叠钮直接叠在交通灯上(弃 rail 后
    // 无 56px 底垫,pl 须独自承担全部避让)。
    let rail_pl = st.sidebar_collapsed.then_some(px(76.));
    div()
        .relative()
        .flex()
        .h_full()
        .min_w(px(0.))
        .flex_1()
        .items_center()
        .gap(px(8.))
        .pr(px(8.))
        // 左缘留白:收起态避让交通灯,展开态回默认
        .when(st.sidebar_collapsed, |el| {
            el.pl(rail_pl.unwrap_or_default())
        })
        .child(sidebar_fold_button(store, cx))
        .child(workspace_trigger(store, cx))
        .children(branch.map(branch_badge))
        // 弹性占位:面板开关推到标题栏右缘(仅关态渲染;面板开着时
        // 同款钮挪入面板头右缘)
        .child(div().flex_1())
        .when(!st.panel_open, |el| el.child(panel_toggle_button(store, cx)))
        // 会话标题:居中区(字符串级预截断同旧约束——taffy 无绝对宽
        // 祖先按 MaxContent 单行测,须物理有界;区宽 = win − 2×inset)
        .child({
            let win_w = f32::from(window.bounds().size.width);
            let zone = (win_w - 2. * TITLE_INSET).max(52.);
            let max_chars = ((zone / 14.) as usize).max(4);
            let shown: String = if title.chars().count() > max_chars {
                let cut: String = title.chars().take(max_chars).collect();
                format!("{cut}…")
            } else {
                title.clone()
            };
            div()
                .absolute()
                .left(px(TITLE_INSET))
                .right(px(TITLE_INSET))
                .top_0()
                .bottom_0()
                .flex()
                .items_center()
                .justify_center()
                .children(
                    std::iter::once(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(px(14.))
                            // 粗体标题;前景取一级
                            // 色——粗体配二级灰立不住
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .text_color(theme::LABEL())
                            .child(shown)
                            .into_any_element(),
                    )
                    .chain(run_label.map(|label| {
                        div()
                            .flex_shrink_0()
                            .text_size(px(13.))
                            .text_color(theme::CAPTION())
                            .child(format!("｜{label}"))
                            .into_any_element()
                    })),
                )
        })
}

/// git 分支徽标(非 repo 省略;纯展示非交互)
fn branch_badge(branch: String) -> impl IntoElement {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(4.))
        .max_w(px(150.))
        .text_size(px(14.))
        .text_color(theme::LABEL_3())
        .child(fixed(DshIcon::GitBranch, 14.))
        .child(div().min_w(px(0.)).truncate().child(branch))
        .debug_selector(|| "topbar-branch".to_string())
}

/// 侧栏缩进钮(展开/折叠 280px ↔ 56px rail;原侧栏首行 logo 行已撤,
/// 折叠入口移此)。折叠态显「展开」图标、展开态显「收起」——是折叠
/// 控制的唯一入口(rail 不再重复放 expand)。形态与工作区触发钮同高共形
fn sidebar_fold_button(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let s = store.clone();
    let collapsed = store.read(cx).sidebar_collapsed;
    let icon = if collapsed {
        IconName::PanelLeftOpen
    } else {
        IconName::PanelLeftClose
    };
    div()
        .id("fold-sidebar")
        .flex()
        .size(px(26.))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .cursor_pointer()
        .hover(|st| st.bg(theme::LAYER()))
        .text_color(theme::LABEL_3())
        .debug_selector(|| "fold-sidebar".to_string())
        .child(fixed(icon, 14.))
        .on_click(move |_, _, cx| {
            s.update(cx, |st, cx| st.toggle_sidebar(cx));
        })
}

/// 工作区下拉触发钮(JetBrains 式:folder + 活动工作区名 + chevron)。
/// 卡片在 ui 根布局(root 级,TitleBar 内会被主内容盖住)
fn workspace_trigger(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let st = store.read(cx);
    let ws = st
        .state
        .active_workspace
        .clone()
        .unwrap_or_else(|| st.default_workspace());
    let open = st.sessions.workspace_menu_open;
    let s = store.clone();
    let label = ws.clone();
    div()
        .id("ws-trigger")
        .flex()
        .h(px(26.))
        .flex_shrink_0()
        .items_center()
        .gap(px(5.))
        .rounded(px(8.))
        .px(px(8.))
        .cursor_pointer()
        .hover(|st| st.bg(theme::LAYER()))
        .text_size(px(14.))
        .text_color(theme::LABEL_2())
        .child(fixed(IconName::FolderClosed, 16.))
        .child(div().max_w(px(140.)).truncate().child(label))
        .child(fixed(IconName::ChevronDown, 14.).text_color(theme::CAPTION()))
        // 开态豁免(根级外点关闭;关-再 toggle 重开竞态)
        .when(open, |el| {
            el.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        })
        .debug_selector(|| "ws-trigger".to_string())
        .on_click(move |_, _, cx| {
            s.update(cx, |st, cx| st.toggle_workspace_menu(cx));
        })
}

/// 右侧面板开关钮(左组尾;开态前景提亮、无底色——开关
/// 是普通 chrome 钮,不走品牌蓝高亮)
fn panel_toggle_button(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let (open, s) = (store.read(cx).panel_open, store.clone());
    div()
        .id("panel-toggle")
        .debug_selector(|| "panel-toggle".to_string())
        .flex()
        .size(px(26.))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .cursor_pointer()
        .text_color(if open { theme::LABEL() } else { theme::LABEL_3() })
        .hover(|s| s.bg(theme::LAYER()).text_color(theme::LABEL()))
        .child(fixed(IconName::PanelRight, 14.))
        .on_click(move |_, _, cx| {
            s.update(cx, |st, cx| st.toggle_panel(cx));
        })
}
