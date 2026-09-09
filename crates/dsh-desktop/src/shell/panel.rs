//! 右侧面板:全高独立列,手动开关 +
//! 左缘拖宽 + 标签页壳。标签 = [`PanelTab`](当前仅「计划」,同类去重);
//! 无激活标签 = 居中快捷菜单,与「+」同份视图清单,面板不自动收。
//! ⇧⌘P = 开计划标签(应用首例键绑定:键表 shell::bind_global_keys +
//! WorkspaceView::new 注册的 App 级 on_action,无焦点可达)。后续视图
//! (浏览器/命令行类)加 `PanelTab` 变体即自动进「+」与空态清单。
//! 设置页整列接管时面板隐藏。

use gpui_kit::{
    actions, App, Entity, InteractiveElement, IntoElement, MouseButton, MouseMoveEvent,
    MouseUpEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_kit::component::{IconName, InteractiveElementExt as _, StyledExt};

use crate::features::chat::{ChatNode, PlanStatus};
use crate::kits::icons::{DshIcon, fixed};
use crate::kits::theme;
use crate::shell::store::AppStore;

actions!(panel, [OpenPanelPlan]);

/// 面板标签页(同类去重,序 = 打开序)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelTab {
    /// 计划(当前会话最新计划只读)
    Plan,
    /// 轨迹(时间线/台账/检查器整页,原样呈现)
    Trajectory,
}

impl PanelTab {
    /// 全部标签(「+」菜单与空态清单共用的视图源)
    pub const ALL: [PanelTab; 2] = [PanelTab::Plan, PanelTab::Trajectory];

    /// 标签标题
    pub fn title(self) -> &'static str {
        match self {
            PanelTab::Plan => "计划",
            PanelTab::Trajectory => "轨迹",
        }
    }

    /// 定尺寸图标(标签 pill 13 / 清单行 16)
    pub fn icon(self, size: f32) -> gpui_kit::component::Icon {
        match self {
            PanelTab::Plan => fixed(DshIcon::ListChecks, size),
            PanelTab::Trajectory => fixed(IconName::GalleryVerticalEnd, size),
        }
    }

    /// 稳定 id / debug selector 片段
    fn key(self) -> &'static str {
        match self {
            PanelTab::Plan => "plan",
            PanelTab::Trajectory => "trajectory",
        }
    }
}

/// 面板列(根 flex 第三子节点;settings 整列接管 / 收起时不渲染)
pub fn render(store: &Entity<AppStore>, window: &mut Window, cx: &mut App) -> impl IntoElement {
    let viewport_w = f32::from(window.viewport_size().width);
    let (col_w, resizing, tabs, active_tab) = {
        let st = store.read(cx);
        (
            f32::from(crate::shell::metrics::panel_width_for(
                st.panel_open,
                st.panel_px,
                viewport_w,
                st.sidebar_collapsed,
                st.sidebar_px,
            )),
            st.panel_resize_anchor.is_some(),
            st.panel_tabs.clone(),
            st.panel_active_tab,
        )
    };
    if col_w <= 0. {
        return div().into_any_element();
    }
    // 快捷键徽标文案从键表生成(单一事实源,不手写;未绑定时回落 action 名)
    let shortcut = window.keystroke_text_for(&OpenPanelPlan);
    let (s_resize, s_resize_move) = (store.clone(), store.clone());
    let mut col = div()
        .debug_selector(|| "right-panel".to_string())
        .flex_shrink_0()
        .h_full()
        .w(px(col_w))
        .v_flex()
        // 面板与聊天区同底(BASE,双模式一致)——分隔只靠左侧发丝线;
        // 深色下取 SIDEBAR 亮一档会显「灰底」
        .bg(theme::BASE())
        .border_l_1()
        .border_color(theme::BORDER())
        .child(panel_header(store, &tabs, active_tab))
        .child(match active_tab {
            Some(tab) => tab_body(store, tab, window, cx),
            None => empty_menu(store, &shortcut),
        });
    // 左缘拖宽把手(absolute 外沿 4px;同 sidebar_resize_handle)
    col = col.child(
        div()
            .id("panel-resize")
            .debug_selector(|| "panel-resize".to_string())
            .absolute()
            .left(px(-4.))
            .top(px(0.))
            .bottom(px(0.))
            .w(px(8.))
            .cursor(gpui_kit::CursorStyle::ResizeLeftRight)
            .on_mouse_down(
                MouseButton::Left,
                move |ev: &gpui_kit::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let vp = f32::from(window.viewport_size().width);
                    s_resize.update(cx, |st, cx| {
                        st.panel_resize_begin(ev.position.x.as_f32(), vp, cx);
                    });
                },
            ),
    );
    // 拖拽 move/up 窗口级注册(仅拖拽中挂;渲染期 canvas 注册,同侧栏
    // drag_overlay:move 带锚点守卫,up 独立注册保证抬起必收尾)
    if resizing {
        let m = s_resize_move.clone();
        let u = s_resize_move.clone();
        col = col.child(
            gpui_kit::canvas(
                // prepaint:无自定义绘制
                |_, _, _| (),
                move |_, _, window, cx| {
                    if m.read(cx).panel_resize_anchor.is_some() {
                        let m2 = m.clone();
                        window.on_mouse_event(move |ev: &MouseMoveEvent, _, _, cx| {
                            m2.update(cx, |st, cx| {
                                st.panel_resize_move(ev.position.x.as_f32(), cx)
                            });
                        });
                    }
                    let u2 = u.clone();
                    window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
                        u2.update(cx, |st, cx| st.panel_resize_end(cx));
                    });
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );
    }
    col.into_any_element()
}

/// 面板头部 40px(与内容区标题行同高对齐;36→40 给
/// 标签卡上下留间隙):窗口拖拽 + 双击缩放;左段 = 标签条 + 紧随的「+」
/// (视图清单菜单)——**无标签时左段全空**(不渲染默认「面板」标题与
/// 「+」,开视图走正文居中的空态清单);右缘 = 面板开关(内容区标题栏钮的开态形态整体挪入,点击收起;关态
/// 时同款钮回标题栏——原 ✕ 关闭钮撤除)。头部空白处 mousedown 即拖;
/// 交互子件自挂 mousedown stop_propagation,点击不触发窗口拖拽
/// (同 drag_strip「区域内无交互子元素」约定的子件侧豁免)
fn panel_header(store: &Entity<AppStore>, tabs: &[PanelTab], active: Option<PanelTab>) -> impl IntoElement {
    let mut strip: Vec<gpui_kit::AnyElement> = Vec::new();
    for tab in tabs {
        strip.push(panel_tab_pill(store, *tab, Some(*tab) == active).into_any_element());
    }
    let s_plus = store.clone();
    let s_toggle = store.clone();
    if !tabs.is_empty() {
        strip.push(
        div()
            .id("panel-plus")
            .debug_selector(|| "panel-plus".to_string())
            .flex()
            .size(px(22.))
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .text_color(theme::CAPTION())
            .hover(|s| s.bg(theme::DOCK()).text_color(theme::LABEL()))
            .child(fixed(IconName::Plus, 13.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |ev: &gpui_kit::ClickEvent, _, cx| {
                cx.stop_propagation();
                let pos = match ev {
                    gpui_kit::ClickEvent::Mouse(m) => m.down.position,
                    _ => gpui_kit::Point::default(),
                };
                s_plus.update(cx, |st, cx| st.open_panel_plus_menu_at(pos, cx));
            })
            .into_any_element(),
        );
    }
    div()
        .id("panel-drag")
        .flex()
        .flex_shrink_0()
        .h(px(40.))
        .items_center()
        .on_mouse_down(MouseButton::Left, |_, window, _| {
            window.start_window_move();
        })
        .on_double_click(|_, window, _| {
            window.titlebar_double_click();
        })
        .child(
            div()
                .flex()
                .items_center()
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .pl(px(10.))
                .overflow_hidden()
                .children(strip),
        )
        .child(
            div()
                .flex()
                .items_center()
                .pr(px(10.))
                .child(
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
                        // 开态形态(同 topbar 面板开关,不用蓝
                        // 色):前景提亮 + 素底,hover 显灰
                        .text_color(theme::LABEL())
                        .hover(|s| s.bg(theme::LAYER()))
                        .child(fixed(IconName::PanelRight, 14.))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(move |_, _, cx| {
                            s_toggle.update(cx, |st, cx| st.toggle_panel(cx));
                        }),
                ),
        )
}

/// 标签卡:图标 + 标题 + ✕ 方钮(浏览器标签语言)。
/// 激活 = LAYER 圆角卡底 + 一级字;✕ = 卡内右侧**常显**独立
/// 圆角小方钮(深盘 DOCK 比卡底亮一档/浅盘白 CARD 浮起——两模式
/// 分层语言),未激活素底 hover 才显底(触发钮全站约定);
/// ✕ 独立点击(stop_propagation,不冒泡激活标签也不触发窗口拖拽)
fn panel_tab_pill(store: &Entity<AppStore>, tab: PanelTab, active: bool) -> gpui_kit::AnyElement {
    let s_tab = store.clone();
    let s_close = store.clone();
    let key = tab.key();
    div()
        .id(SharedString::from(format!("panel-tab-{key}")))
        .debug_selector(move || format!("panel-tab-{}", tab.key()))
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(8.))
        .h(px(30.))
        .mr(px(6.))
        .pl(px(14.))
        .pr(px(10.))
        .rounded(px(8.))
        .cursor_pointer()
        .bg(if active {
            theme::LAYER()
        } else {
            theme::TRANSPARENT()
        })
        .text_color(if active {
            theme::LABEL()
        } else {
            theme::LABEL_3()
        })
        .hover(|s| s.bg(theme::LAYER()))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(tab.icon(14.))
        .child(div().text_size(px(13.)).child(tab.title()))
        .child(
            div()
                .id(SharedString::from(format!("panel-tab-close-{key}")))
                .ml(px(8.))
                .flex()
                .size(px(20.))
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .bg(match active {
                    true if theme::is_dark() => theme::DOCK(),
                    true => theme::CARD(),
                    false => theme::TRANSPARENT(),
                })
                .text_color(theme::CAPTION())
                .hover(|s| s.bg(theme::DOCK()).text_color(theme::LABEL()))
                .child(fixed(IconName::Close, 11.))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    s_close.update(cx, |st, cx| st.close_panel_tab(tab, cx));
                }),
        )
        .on_click(move |_, _, cx| {
            s_tab.update(cx, |st, cx| st.activate_panel_tab(tab, cx));
        })
        .into_any_element()
}

/// 标签正文:计划 = 最新计划快照只读;轨迹 = 整页视图自主区迁入(原样:
/// 工具栏 + 时间线 + 台账|检查器并排,检查器拖宽/时间线选区照旧)。
/// 外层 flex_1 承接面板列剩余高度,轨迹视图根 size_full 填满
fn tab_body(
    store: &Entity<AppStore>,
    tab: PanelTab,
    window: &mut Window,
    cx: &mut App,
) -> gpui_kit::AnyElement {
    match tab {
        PanelTab::Plan => {
            let latest = {
                let st = store.read(cx);
                let session_id = st.state.current_id.clone();
                session_id.as_deref().and_then(|sid| {
                    st.state.chats.get(sid).and_then(|c| {
                        c.nodes.iter().rev().find_map(|n| match n {
                            ChatNode::Plan { plan, status, .. } => Some((plan.clone(), *status)),
                            _ => None,
                        })
                    })
                })
            };
            div()
                .id("panel-body")
                .debug_selector(|| "panel-plan-view".to_string())
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .px(px(12.))
                .py(px(10.))
                .child(panel_plan_content(latest))
                .into_any_element()
        }
        PanelTab::Trajectory => div()
            .debug_selector(|| "panel-trajectory-view".to_string())
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .child(crate::features::trajectory::render(store, window, cx))
            .into_any_element(),
    }
}

/// 空态快捷菜单(面板开着、无激活标签):居中卡,
/// 每行 = 图标 + 标题 + 右侧快捷键徽标;与「+」菜单同份 [`PanelTab::ALL`]
fn empty_menu(store: &Entity<AppStore>, shortcut: &str) -> gpui_kit::AnyElement {
    let rows: Vec<gpui_kit::AnyElement> = PanelTab::ALL
        .iter()
        .map(|tab| {
            let s = store.clone();
            div()
                .id(SharedString::from(format!(
                    "panel-empty-row-{}",
                    tab.key()
                )))
                .debug_selector(move || format!("panel-empty-row-{}", tab.key()))
                .flex()
                .items_center()
                .gap(px(10.))
                .h(px(40.))
                .px(px(14.))
                .rounded(px(10.))
                .cursor_pointer()
                // 行面分模式:浅盘素底 hover 才灰(灰胶囊行在白面板上成
                // 灰砖);深盘微亮面 LAYER——纯素底在
                // 深面板上无边界,取深色行极弱亮面
                .bg(if theme::is_dark() {
                    theme::LAYER()
                } else {
                    theme::TRANSPARENT()
                })
                .hover(|s| s.bg(theme::DOCK()))
                .child(tab.icon(16.).text_color(theme::LABEL_2()))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::LABEL())
                        .child(tab.title()),
                )
                .child(div().flex_1())
                .child(
                    // 快捷键徽标:小灰 pill,须比行面亮一档才可读
                    div()
                        .rounded(px(5.))
                        .bg(if theme::is_dark() {
                            theme::DOCK()
                        } else {
                            theme::LAYER()
                        })
                        .px(px(6.))
                        .py(px(2.))
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child(shortcut.to_string()),
                )
                .on_click(move |_, _, cx| s.update(cx, |st, cx| st.open_panel_tab(*tab, cx)))
                .into_any_element()
        })
        .collect();
    div()
        .flex_1()
        .min_h(px(0.))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .debug_selector(|| "panel-empty-menu".to_string())
                .v_flex()
                .w(px(320.))
                .gap(px(8.))
                .children(rows),
        )
        .into_any_element()
}

/// 面板「+」菜单卡(root 级定位渲染,同 row/ws 菜单:occlude + 整卡
/// mousedown 豁免 + 根级外点关闭);锚「+」下方向左展开(面板贴窗右缘,
/// 右展出窗)。项与空态菜单同份 [`PanelTab::ALL`]
pub fn plus_menu_card(
    store: &Entity<AppStore>,
    pos: gpui_kit::Point<gpui_kit::Pixels>,
    shortcut: String,
) -> impl IntoElement {
    let items: Vec<gpui_kit::AnyElement> = PanelTab::ALL
        .iter()
        .map(|tab| {
            let s = store.clone();
            let shortcut = shortcut.clone();
            div()
                .id(SharedString::from(format!(
                    "panel-plus-item-{}",
                    tab.key()
                )))
                .debug_selector(move || format!("panel-plus-item-{}", tab.key()))
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(26.))
                .px(px(8.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(theme::LABEL_2())
                .hover(|st| st.bg(theme::DOCK()))
                .child(tab.icon(13.))
                .child(div().text_size(px(12.)).child(tab.title()))
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child(shortcut.to_string()),
                )
                .on_click(move |_, _, cx| s.update(cx, |st, cx| st.open_panel_tab(*tab, cx)))
                .into_any_element()
        })
        .collect();
    div()
        .id("panel-plus-menu")
        .absolute()
        // 阻断鼠标命中向卡后方穿透(否则外点会落到面板/聊天区上)
        .occlude()
        .debug_selector(|| "panel-plus-menu".to_string())
        .top(pos.y + px(12.))
        .left(pos.x - px(152.))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .v_flex()
        .w(px(176.))
        .gap(px(2.))
        .rounded(px(10.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::LAYER())
        .p(px(4.))
        .shadow_md()
        .children(items)
}

/// 计划 tab 内容:最新计划全文 + 状态徽标;无计划 = 空态
fn panel_plan_content(latest: Option<(String, PlanStatus)>) -> impl IntoElement {
    let mut col = div().v_flex().gap(px(8.));
    match latest {
        None => {
            col = col.child(
                div()
                    .py(px(20.))
                    .text_size(px(12.))
                    .text_color(theme::CAPTION())
                    .child("当前会话暂无计划。"),
            );
        }
        Some((plan, status)) => {
            let (status_text, status_color) = match status {
                PlanStatus::Pending => ("待批准", theme::WARN()),
                PlanStatus::Approved => ("已批准", theme::SUCCESS()),
                PlanStatus::Cancelled => ("已取消", theme::CAPTION()),
            };
            col = col
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .text_color(theme::LABEL_2())
                                .child("计划"),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(status_color)
                                .child(status_text),
                        ),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::LABEL_2())
                        .child(crate::kits::markdown::render("panel-plan", &plan)),
                );
        }
    }
    col
}
