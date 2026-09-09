//! 侧栏:会话列表(web `Sidebar.tsx`,280px;收起完全隐藏)。
//! 按工作区分组(前缀推导),支持本地搜索过滤、新建、切换。

use std::time::{SystemTime, UNIX_EPOCH};

use dsh_core::proto::SessionSummary;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, StatefulInteractiveElement, Styled, div, px,
};
use gpui_kit::component::Icon;
use gpui_kit::component::IconName;
use gpui_kit::component::InteractiveElementExt as _;
use gpui_kit::component::StyledExt;

use crate::features::search;
use crate::features::settings;
use crate::kits::icons::{DshIcon, fixed};
use crate::kits::theme;
use crate::shell::reducer::{relative_time, workspace_of};
use crate::shell::store::AppStore;

/// 侧栏整体(展开 280px 胶囊卡;收起完全隐藏不渲染——
/// 折叠不再保留 56px rail,展开入口仅标题栏缩进钮)。
/// 设置模式 = 侧栏切换为设置菜单:
/// 标题 + 通用/模型/关于 导航项 + 底部「返回」行。
///
/// 展开态右缘挂拖宽把手,宽存于 `sidebar_px`。
pub fn render(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    if store.read(cx).sidebar_collapsed {
        return div().into_any_element();
    }
    if store.read(cx).settings.settings_open {
        return settings::menu(store, cx).into_any_element();
    }
    let st = store.read(cx);
    // 拖宽锚点在场:窗口级 move/up 经 canvas.paint(Paint 相位)注册,非 render
    // (render 在 Prepaint 相位跑,on_mouse_event 会 panic;canvas.paint 每帧
    // 重跑,指针移出把手仍收拖动事件,无指针捕获的 GPUI 惯例)。
    // 锚点为 None 时每次 paint 仍注册但内层早退(no-op),开销可忽略;结束
    // 后下一帧 anchor 清空,不再拖动。
    let drag_active = st.sidebar_resize_anchor.is_some();
    let mut body = div()
        .v_flex()
        .h_full()
        .w(crate::shell::metrics::sidebar_width_for(
            false,
            st.sidebar_px,
        ))
        .flex_shrink_0()
        // 扁平面板(去胶囊卡):SIDEBAR 色阶 + 右缘发丝线与内容区分界;
        // 组件层(行 hover/搜索框)浮于其上
        .bg(theme::SIDEBAR())
        .border_r_1()
        .border_color(theme::BORDER())
        // 测试钩子:布局回归断言双栏分离(release 空操作)
        .debug_selector(|| "sidebar-card".to_string())
        .px(px(12.))
        // 顶部让位:macOS 交通灯浮于卡上(方案 B 全高侧栏);该区为
        // 窗口拖拽条(见 drag_strip)
        .pt(px(36.))
        .pb(px(10.))
        .gap(px(8.))
        .child(drag_strip())
        .child(new_session_row(store))
        .child(search::search_row(store, cx))
        .when(store.read(cx).search.search_hits.is_some(), |el| {
            el.child(search::search_hits_panel(store, cx))
        })
        .when(store.read(cx).search.search_hits.is_none(), |el| {
            el.child(session_list(store, cx))
        })
        .child(settings::settings_row(store))
        .child(sidebar_resize_handle(store));
    // 拖宽进行中:整窗 canvas 覆盖层负责窗口级 move/up 注册(Paint 相位)。
    if drag_active {
        body = body.child(drag_overlay(store));
    }
    body.into_any_element()
}

/// 右缘竖向拖宽把手(8px 命中区,右缘外沿;源 DragHandle 表意)。折叠态
/// 不渲染(源「no resize handle while closed」)。on_mouse_down 在 paint 相位
/// 注册(div 惯例),开始拖拽只置锚点;move/up 由 [`drag_overlay`] 接管。
fn sidebar_resize_handle(store: &Entity<AppStore>) -> impl IntoElement {
    let s = store.clone();
    div()
        .id("sidebar-resize")
        .debug_selector(|| "sidebar-resize".to_string())
        .absolute()
        // 右缘外侧 4px,命中区 8px 覆盖边界
        .right(px(-4.))
        .top_0()
        .bottom_0()
        .w(px(8.))
        .cursor(gpui_kit::CursorStyle::ResizeLeftRight)
        .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
            s.update(cx, |st, cx| {
                st.sidebar_resize_begin(f32::from(ev.position.x), cx)
            });
        })
}

/// 拖宽进行中的窗口级 move/up 注册(Paint 相位):canvas.paint 每帧重跑,
/// 指针移出把手仍收拖动事件;锚点为 None 时内层早退(no-op)。
fn drag_overlay(store: &Entity<AppStore>) -> impl IntoElement {
    let m = store.clone();
    let u = store.clone();
    gpui_kit::canvas(
        // prepaint:无自定义绘制
        |_, _, _| (),
        move |_, _, window, cx| {
            // 锚点在场才注册移动;清空后早退(闭包仍每帧跑,paint 相位合法)
            if m.read(cx).sidebar_resize_anchor.is_some() {
                let m2 = m.clone();
                window.on_mouse_event(move |ev: &MouseMoveEvent, _, _, cx| {
                    m2.update(cx, |st, cx| {
                        st.sidebar_resize_move(f32::from(ev.position.x), cx)
                    });
                });
            }
            let u2 = u.clone();
            window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
                u2.update(cx, |st, cx| st.sidebar_resize_end(cx));
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
    .into_any_element()
}

/// 卡顶拖拽条(方案 B:交通灯浮于全高侧栏卡上,顶部让位区兼作窗口
/// 拖拽/双击缩放;区域内无交互子元素,mousedown 即拖)
pub(crate) fn drag_strip() -> impl IntoElement {
    div()
        .id("sidebar-drag-strip")
        .debug_selector(|| "sidebar-drag-strip".to_string())
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(px(34.))
        .cursor(gpui_kit::CursorStyle::Arrow)
        .on_mouse_down(MouseButton::Left, |_, window, _| {
            window.start_window_move();
        })
        .on_double_click(|_, window, _| {
            window.titlebar_double_click();
        })
}

/// 「新建会话」行
fn new_session_row(store: &Entity<AppStore>) -> impl IntoElement {
    let s = store.clone();
    div().flex().h(px(30.)).items_center().child(
        div()
            .id("new-session")
            .flex()
            .flex_1()
            .h(px(30.))
            .items_center()
            .justify_center()
            .gap(px(4.))
            .rounded(px(8.))
            .bg(theme::DOCK())
            .cursor_pointer()
            .text_size(px(13.))
            .text_color(theme::LABEL_2())
            .hover(|s| s.bg(theme::LAYER()))
            .child(fixed(IconName::Plus, 14.))
            .child("新建会话")
            .on_click(move |_, _, cx| {
                s.update(cx, |st, cx| st.create_session(cx));
            }),
    )
}

/// 会话树:按工作区分组(首见序),搜索过滤
fn session_list(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let st = store.read(cx);
    let default = st.default_workspace();
    let query = st
        .search
        .search_input
        .as_ref()
        .map(|e| e.read(cx).value().trim().to_lowercase())
        .unwrap_or_default();

    // 分组(组 = 工作区清单全量,清单序;无会话的工作区仍渲染组头——
    // 删除会话后组不可消失)。清单外的工作区名防御性追加在尾部
    let mut groups: Vec<(String, Vec<usize>)> = st
        .state
        .host_info
        .workspaces
        .iter()
        .map(|w| (w.clone(), Vec::new()))
        .collect();
    for (ix, s) in st.state.sessions.iter().enumerate() {
        // subagent 会话在侧栏隐藏(仅经页头目录进入)
        if s.origin.as_deref() == Some("subagent") {
            continue;
        }
        if !query.is_empty() && !st.title_for(&s.session_id).to_lowercase().contains(&query) {
            continue;
        }
        let key = workspace_of(&s.session_id, &default).to_string();
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(ix),
            None => groups.push((key, vec![ix])),
        }
    }
    // 搜索时隐藏无匹配会话的组(空组对搜索不可达,保持既有搜索语义)
    if !query.is_empty() {
        groups.retain(|(_, v)| !v.is_empty());
    }

    let mut children: Vec<gpui_kit::AnyElement> = vec![];
    for (gi, (ws, idxs)) in groups.into_iter().enumerate() {
        // 折叠态隐藏组内行;搜索时忽略折叠(否则折叠组内匹配不可达)
        let collapsed = st.sessions.collapsed_workspaces.contains(&ws) && query.is_empty();
        children.push(group_header(store, cx, &ws, gi, collapsed).into_any_element());
        if !collapsed {
            for ix in idxs {
                let s = &st.state.sessions[ix];
                children.push(session_row(store, cx, s, ix).into_any_element());
            }
        }
    }

    div()
        .id("sidebar-sessions")
        .v_flex()
        .min_h(px(0.))
        .flex_1()
        .overflow_y_scroll()
        .pb(px(8.))
        .gap(px(2.))
        .children(children)
}

/// 工作区组头(树形层级第一级):chevron 折叠钮 + 文件夹 + 标题(显示名
/// 覆盖)+ hover「+」(该工作区新建)与 ⋯(整理菜单);行主体点击切换
/// 工作区(兼自动展开)
fn group_header(
    store: &Entity<AppStore>,
    cx: &App,
    ws: &str,
    gi: usize,
    collapsed: bool,
) -> impl IntoElement {
    let st = store.read(cx);
    let active = st.state.active_workspace.as_deref() == Some(ws);
    let display = st.title_for_workspace(ws);
    let (s, s_add, s_fold, s_menu) = (store.clone(), store.clone(), store.clone(), store.clone());
    let (ws_row, ws_fold, ws_add, ws_menu, ws_head) = (
        ws.to_string(),
        ws.to_string(),
        ws.to_string(),
        ws.to_string(),
        ws.to_string(),
    );
    let fg = if active { theme::LABEL() } else { theme::LABEL_3() };
    let sel = format!("ws-chevron-{}", if collapsed { "closed" } else { "open" });
    div()
        .id(("ws", gi))
        .debug_selector(move || format!("ws-head-{ws_head}"))
        .flex()
        .h(px(28.))
        .flex_shrink_0()
        .items_center()
        .rounded(px(6.))
        .pl(px(4.))
        .pr(px(4.))
        .gap(px(4.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::LAYER()))
        .text_size(px(12.))
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(fg)
        // 折叠钮(独立点击:只切折叠,不切工作区)
        .child(
            div()
                .id(("ws-fold", gi))
                .flex()
                .size(px(18.))
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .hover(|s| s.bg(theme::DOCK()))
                .text_color(theme::CAPTION())
                .child(fixed(
                    if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    },
                    13.,
                ))
                // 测试钩子:开/合两态异键(消除断言只增 map 的歧义)
                .debug_selector(move || sel.clone())
                .on_click({
                    let ws = ws_fold.clone();
                    move |_, _, cx| {
                        cx.stop_propagation();
                        let ws = ws.clone();
                        s_fold.update(cx, |st, cx| st.toggle_workspace_collapsed(&ws, cx));
                    }
                }),
        )
        .child(
            // 选中工作区:开页文件夹 + 品牌色(对齐 web)
            fixed(
                if active {
                    IconName::FolderOpen
                } else {
                    IconName::FolderClosed
                },
                16.,
            )
            .text_color(if active { theme::BRAND() } else { theme::LABEL_3() }),
        )
        .child(div().min_w(px(0.)).truncate().child(display))
        .child(div().flex_1())
        // hover ⋯:工作区整理菜单(重命名/排序/移除)
        .child(
            div()
                .id(("ws-menu", gi))
                .debug_selector(|| "ws-menu-btn".to_string())
                .flex()
                .size(px(20.))
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .opacity(0.)
                .hover(|s| s.opacity(1.).bg(theme::DOCK()))
                .text_color(theme::CAPTION())
                .child(fixed(IconName::Ellipsis, 13.))
                .on_click(move |ev: &gpui_kit::ClickEvent, _, cx| {
                    cx.stop_propagation();
                    let pos = match ev {
                        gpui_kit::ClickEvent::Mouse(m) => m.down.position,
                        gpui_kit::ClickEvent::Keyboard(_) => gpui_kit::Point::default(),
                        gpui_kit::ClickEvent::Touch(_) => gpui_kit::Point::default(),
                    };
                    s_menu.update(cx, |st, cx| {
                        if st.sessions.menu_open_ws.as_deref() == Some(&ws_menu) {
                            st.sessions.menu_open_ws = None;
                            cx.notify();
                        } else {
                            st.open_ws_menu_at(&ws_menu, pos, cx);
                        }
                    });
                }),
        )
        // hover「+」:该工作区新建会话(web 同位)
        .child(
            div()
                .id(("ws-new", gi))
                .flex()
                .size(px(20.))
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .opacity(0.)
                .hover(|s| s.opacity(1.).bg(theme::DOCK()))
                .text_color(theme::CAPTION())
                .child(fixed(IconName::Plus, 13.))
                .on_click({
                    let ws = ws_add.clone();
                    move |_, _, cx| {
                        cx.stop_propagation();
                        let ws = ws.clone();
                        s_add.update(cx, |st, cx| st.create_session_in(&ws, cx));
                    }
                }),
        )
        .on_click(move |_, _, cx| {
            let ws = ws_row.clone();
            s.update(cx, |st, cx| st.select_workspace(&ws, cx));
        })
}

/// 单个会话行(行高 34:标题 + 相对时间;当前选中高亮)
fn session_row(
    store: &Entity<AppStore>,
    cx: &App,
    s: &SessionSummary,
    ix: usize,
) -> impl IntoElement {
    let st = store.read(cx);
    let active = st.state.current_id.as_deref() == Some(&s.session_id);
    let title = st.title_for(&s.session_id);
    let running = st.is_running(&s.session_id);
    let menu_open = st.sessions.menu_open_session.as_deref() == Some(&s.session_id);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let time = relative_time(now, s.updated_at);
    let (bg, fg) = if active {
        (theme::LAYER(), theme::LABEL())
    } else {
        (theme::TRANSPARENT(), theme::LABEL_2())
    };
    // 后台子代理运行中(自身非 running 时)替代时间位
    let sub_running = st.running_subagent_count(&s.session_id);
    let target = store.clone();
    let id = s.session_id.clone();
    let sel = format!("session-row-{id}");
    div()
        .id(("session", ix))
        .relative()
        .flex()
        .h(px(34.))
        .flex_shrink_0()
        .items_center()
        .rounded(px(8.))
        .bg(bg)
        // 树形第二级:组头之下一层缩进(pl 30 ≈ 组头 chevron+图标宽)
        .pl(px(30.))
        .pr(px(8.))
        .gap(px(8.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::LAYER()))
        // 会话图标(web 无此位,按需新增;选中态提亮)
        .child(fixed(DshIcon::MessageSquare, 14.).text_color(if active {
            theme::LABEL_2()
        } else {
            theme::LABEL_3()
        }))
        .child(
            div()
                .flex()
                .min_w(px(0.))
                .flex_1()
                .truncate()
                .text_size(px(13.))
                .text_color(fg)
                .child(title),
        )
        .child(if running {
            running_dot()
        } else if sub_running > 0 {
            sub_running_badge(sub_running)
        } else {
            plain_time(&time)
        })
        .child(row_menu_button(store, &s.session_id, ix, menu_open))
        // 测试钩子:按会话 id 稳定检索(release 空操作)
        .debug_selector(move || sel.clone())
        .on_click(move |_, _, cx| {
            let id = id.clone();
            target.update(cx, |st, cx| st.open_session(&id, cx));
        })
}

/// 行尾 ⋯ 钮(点击开菜单;不冒泡到行打开)。开态挂 mousedown
/// 豁免:根级外点关闭先关再被 toggle 重开,豁免后由 toggle 自身关闭
fn row_menu_button(store: &Entity<AppStore>, id: &str, ix: usize, open: bool) -> impl IntoElement {
    let s = store.clone();
    let id = id.to_string();
    let sel = format!("row-menu-btn-{ix}");
    div()
        .id(("row-menu", ix))
        .debug_selector(move || sel.clone())
        .flex()
        .size(px(20.))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|st| st.bg(theme::DOCK()))
        .text_color(theme::CAPTION())
        .child(fixed(IconName::Ellipsis, 14.))
        .when(open, |el| {
            el.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        })
        .on_click(move |ev: &gpui_kit::ClickEvent, _, cx| {
            cx.stop_propagation();
            let id = id.clone();
            let pos = match ev {
                gpui_kit::ClickEvent::Mouse(m) => m.down.position,
                gpui_kit::ClickEvent::Keyboard(_) => gpui_kit::Point::default(),
                gpui_kit::ClickEvent::Touch(_) => gpui_kit::Point::default(),
            };
            s.update(cx, |st, cx| {
                if st.sessions.menu_open_session.as_deref() == Some(&id) {
                    st.toggle_row_menu(&id, cx);
                } else {
                    st.open_row_menu_at(&id, pos, cx);
                }
            });
        })
}

/// 会话行菜单卡(重命名/分叉/归档/导出日志;根级渲染按点击坐标
/// 定位,向左展开避开窗口右缘)。整卡挂 mousedown 豁免(同 composer
/// 菜单:防根级外点关闭吞掉菜单项点击)
pub fn row_menu_card(
    store: &Entity<AppStore>,
    id: &str,
    pos: gpui_kit::Point<gpui_kit::Pixels>,
) -> impl IntoElement {
    let (rename, fork, archive, delete, export_log) = (
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
    );
    let id = id.to_string();
    let (rid, fid, aid, did, eid) = (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
    div()
        .id("row-menu-card")
        .absolute()
        // 阻断鼠标命中向卡后方穿透(否则点击会落到后面的会话行上)
        .occlude()
        .debug_selector(|| "row-menu-card".to_string())
        // 锚在 ⋯ 钮下方,向左展开(钮在侧栏右缘,右展会出窗)
        .top(pos.y + px(14.))
        .left(pos.x - px(112.) - px(8.))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .v_flex()
        .w(px(112.))
        .gap(px(2.))
        .rounded(px(10.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::LAYER())
        .p(px(4.))
        .shadow_md()
        .child(menu_item(
            "重命名",
            fixed(DshIcon::Pencil, 13.),
            move |_, window, cx| {
                let id = rid.clone();
                rename.update(cx, |st, cx| st.open_rename(&id, window, cx));
            },
        ))
        .child(menu_item(
            "分叉",
            fixed(DshIcon::GitBranch, 13.),
            move |_, _, cx| {
                let id = fid.clone();
                fork.update(cx, |st, cx| {
                    st.fork(&id, cx);
                    st.sessions.menu_open_session = None;
                });
            },
        ))
        .child(menu_item(
            "归档",
            fixed(DshIcon::Archive, 13.),
            move |_, _, cx| {
                let id = aid.clone();
                archive.update(cx, |st, cx| {
                    st.archive(&id, cx);
                    st.sessions.menu_open_session = None;
                });
            },
        ))
        .child(menu_item(
            "删除",
            fixed(IconName::Delete, 13.),
            move |_, _, cx| {
                let id = did.clone();
                delete.update(cx, |st, cx| st.ask_delete_session(&id, cx));
            },
        ))
        .child(menu_item(
            "导出日志",
            fixed(DshIcon::Download, 13.),
            move |_, _, cx| {
                let id = eid.clone();
                export_log.update(cx, |st, cx| {
                    st.export_session_log(&id, cx);
                    st.sessions.menu_open_session = None;
                });
            },
        ))
}

/// 菜单项(图标 + 文字)
fn menu_item(
    label: &'static str,
    icon: Icon,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut gpui_kit::App) + 'static,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(label)
        .debug_selector(move || label.to_string())
        .flex()
        .h(px(26.))
        .items_center()
        .gap(px(6.))
        .px(px(8.))
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|st| st.bg(theme::DOCK()))
        .text_size(px(12.))
        .text_color(theme::LABEL_2())
        .child(icon)
        .child(label)
        .on_click(move |ev, w, cx| {
            cx.stop_propagation();
            on_click(ev, w, cx)
        })
}

/// 工作区分组头 ⋯ 菜单卡(整理面:重命名/上移/下移/移除;
/// 根级渲染按点击坐标定位,向左展开。首项无上移、末项无下移、
/// 默认工作区不提供移除)
pub fn ws_menu_card(
    store: &Entity<AppStore>,
    cx: &App,
    ws: &str,
    pos: gpui_kit::Point<gpui_kit::Pixels>,
) -> impl IntoElement {
    let st = store.read(cx);
    let names = st.workspace_order();
    let ix = names.iter().position(|n| n == ws);
    let (has_up, has_down, is_default) = match ix {
        Some(i) => (i > 0, i + 1 < names.len(), i == 0),
        None => (false, false, false),
    };
    let (rename, up, down, remove) = (store.clone(), store.clone(), store.clone(), store.clone());
    let (wid_r, wid_u, wid_d, wid_x) = (
        ws.to_string(),
        ws.to_string(),
        ws.to_string(),
        ws.to_string(),
    );
    div()
        .id("ws-menu-card")
        .absolute()
        .occlude()
        .debug_selector(|| "ws-menu-card".to_string())
        .top(pos.y + px(14.))
        .left(pos.x - px(112.) - px(8.))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .v_flex()
        .w(px(112.))
        .gap(px(2.))
        .rounded(px(10.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::LAYER())
        .p(px(4.))
        .shadow_md()
        .child(menu_item(
            "重命名",
            fixed(DshIcon::Pencil, 13.),
            move |_, window, cx| {
                let id = wid_r.clone();
                rename.update(cx, |st, cx| st.open_rename_workspace(&id, window, cx));
            },
        ))
        .when(has_up, |el| {
            el.child(menu_item(
                "上移",
                fixed(IconName::ArrowUp, 13.),
                move |_, _, cx| {
                    let id = wid_u.clone();
                    up.update(cx, |st, cx| st.move_workspace(&id, true, cx));
                },
            ))
        })
        .when(has_down, |el| {
            el.child(menu_item(
                "下移",
                fixed(IconName::ArrowDown, 13.),
                move |_, _, cx| {
                    let id = wid_d.clone();
                    down.update(cx, |st, cx| st.move_workspace(&id, false, cx));
                },
            ))
        })
        .when(!is_default, |el| {
            el.child(menu_item(
                "移除",
                fixed(IconName::Delete, 13.),
                move |_, _, cx| {
                    let id = wid_x.clone();
                    remove.update(cx, |st, cx| st.remove_workspace(&id, cx));
                },
            ))
        })
}

/// 执行中徽点(替代时间位;点阵追逐动画)
/// 「N 个子代理运行中」行尾状态
/// (优先级低于自身运行中,替代相对时间位)
fn sub_running_badge(n: usize) -> gpui_kit::AnyElement {
    div()
        .debug_selector(|| "session-row-sub-running".to_string())
        .flex()
        .items_center()
        .gap(px(4.))
        .flex_shrink_0()
        .child(
            div()
                .size(px(6.))
                .rounded_full()
                .bg(theme::ONGOING()),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme::CAPTION())
                .child(if n == 1 {
                    "1 个子代理".to_string()
                } else {
                    format!("{n} 个子代理")
                }),
        )
        .into_any_element()
}

fn running_dot() -> gpui_kit::AnyElement {
    crate::kits::state_dot::ongoing_dot(8.).into_any_element()
}

/// 相对时间
fn plain_time(time: &str) -> gpui_kit::AnyElement {
    div()
        .flex_shrink_0()
        .text_size(px(11.))
        .text_color(theme::CAPTION())
        .child(time.to_string())
        .into_any_element()
}
