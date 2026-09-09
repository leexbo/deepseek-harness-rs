//! 血缘 UI:页头
//! 「{count} 个子代理」触发器(运行中带活动圆点)+ 树形下拉目录——
//! 每行 = 状态点 + 任务名 + 副行(prompt 截断 · 结算 detail)+ 运行计时,
//! 点击跳子会话。数据以 jobs 帧权威,子会话清单补位。
//! (从 ui::topbar 切出;trigger 挂于顶栏标题右侧操作组。)

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    div, px,
};
use gpui_kit::component::{IconName, StyledExt};

use crate::kits::icons::{DshIcon, fixed};
use crate::kits::theme;
use crate::shell::store::AppStore;

use super::store::LineageRow;

/// 状态点:running=活动蓝,completed=done 绿,
/// failed=红,killed=黄
fn state_dot(dot: &'static str) -> gpui_kit::AnyElement {
    let color = match dot {
        "running" => theme::ONGOING(),
        "failed" => theme::DANGER(),
        "killed" => theme::WARN(),
        _ => theme::SUCCESS(),
    };
    div()
        .debug_selector(move || format!("lineage-dot-{dot}"))
        .size(px(6.))
        .flex_shrink_0()
        .rounded_full()
        .bg(color)
        .into_any_element()
}

/// 运行计时文案:运行中 = 墙钟差走秒(菜单开时 lineage_tick 每秒刷新);
/// 已结束 = 起止差定格
fn duration_text(row: &LineageRow) -> String {
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    };
    let Some(started) = row.started_at else {
        return String::new();
    };
    let end = if row.running {
        now_ms()
    } else {
        row.finished_at.unwrap_or_else(now_ms)
    };
    let secs = (end - started).max(0) / 1000;
    if secs < 60 {
        format!("{secs}秒")
    } else {
        format!("{}分{:02}秒", secs / 60, secs % 60)
    }
}

/// 子代理任务面板(任务列表卡——与
/// TodoDock 同一视觉语言):标题行(Workflow 图标 + 「子代理」 + 运行
/// 摘要 + 折叠 chevron);展开为行列表 = 「主线」行(仅子会话视图,
/// 一键返回)+ 运行中子代理行(状态点+任务名+走秒计时)+ 当前查看的
/// 子会话行(已结束也高亮定位),点击行切换查看。集合语义:运行中
/// 全部 + 当前查看;全无 → 整卡不渲染。
pub(crate) fn task_bar(store: &Entity<AppStore>, cx: &App) -> Option<gpui_kit::AnyElement> {
    let st = store.read(cx);
    let current = st.state.current_id.clone()?;
    let anchor = st.subagent_anchor_of(&current);
    let viewing_child = anchor != current;
    let rows = st.lineage_rows(&anchor);
    // 集合:运行中全部;当前查看的子会话(已结束)补位高亮
    let mut chips: Vec<LineageRow> = rows.iter().filter(|r| r.running).cloned().collect();
    if viewing_child
        && !chips.iter().any(|r| r.session_id == current)
        && let Some(row) = rows.iter().find(|r| r.session_id == current)
    {
        chips.push(row.clone());
    }
    if chips.is_empty() {
        return None;
    }
    let running_n = chips.iter().filter(|r| r.running).count();
    let open = st.subagents.task_bar_open;
    let s = store.clone();
    let mut head = div()
        .id("task-bar-head")
        .debug_selector(|| "task-bar-head".to_string())
        .flex()
        .h(px(30.))
        .items_center()
        .gap(px(8.))
        .px(px(12.))
        .cursor_pointer()
        .child(fixed(DshIcon::Workflow, 14.).text_color(theme::LABEL_2()))
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme::LABEL_2())
                .child("子代理"),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(11.))
                .text_color(theme::CAPTION())
                .child(if running_n > 0 {
                    format!("{running_n} 个运行中")
                } else {
                    "已结束".to_string()
                }),
        )
        .child(
            fixed(
                if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                },
                14.,
            )
            .text_color(theme::CAPTION()),
        )
        .on_click(move |_, _, cx| {
            s.update(cx, |st, cx| st.toggle_task_bar(cx));
        });
    // 子会话视图:标题行尾缀「主线」返回钮(不挤列表行)
    if viewing_child {
        let s_main = store.clone();
        let main_id = anchor.clone();
        head = head.child(
            div()
                .id("task-chip-main")
                .debug_selector(|| "task-chip-main".to_string())
                .flex()
                .items_center()
                .gap(px(4.))
                .rounded(px(10.))
                .border_1()
                .border_color(theme::BRAND())
                .px(px(8.))
                .h(px(20.))
                .cursor_pointer()
                .hover(|st| st.bg(theme::DOCK()))
                .text_size(px(11.))
                .text_color(theme::BRAND())
                .on_click(move |_, _, cx| {
                    let id = main_id.clone();
                    s_main.update(cx, |st, cx| st.open_session(&id, cx));
                })
                .child(fixed(gpui_kit::component::IconName::ArrowLeft, 11.))
                .child("主线"),
        );
    }
    // 行元素先建(闭包 move 所有权,避免借用 chips 越过函数尾)
    let list_rows: Vec<gpui_kit::AnyElement> = chips
        .iter()
            .map(|chip| {
                let s_chip = store.clone();
                let chip = chip.clone();
                let chip_id = chip.session_id.clone();
                let running = chip.running;
                let viewing = chip.session_id == current;
                let timing = duration_text(&chip);
                div()
                    .id(gpui_kit::SharedString::from(format!(
                        "task-row-{}",
                        chip.session_id
                    )))
                    .debug_selector(|| "task-chip".to_string())
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(8.))
                    .px(px(10.))
                    .py(px(4.))
                    .bg(if viewing { theme::DOCK() } else { theme::TRANSPARENT() })
                    .cursor_pointer()
                    .hover(|st| st.bg(theme::DOCK()))
                    .on_click(move |_, _, cx| {
                        let id = chip_id.clone();
                        s_chip.update(cx, |st, cx| st.open_session(&id, cx));
                    })
                    .when_some(chip.dot, |el, dot| el.child(state_dot(dot)))
                    .child(
                        div()
                            .min_w(px(0.))
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(if viewing { theme::LABEL() } else { theme::LABEL_2() })
                            .truncate()
                            .child(chip.label.clone()),
                    )
                    .when(!timing.is_empty(), |el| {
                        el.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(11.))
                                .text_color(theme::CAPTION())
                                .child(timing),
                        )
                    })
                    .when(viewing, |el| {
                        el.child(
                            fixed(gpui_kit::component::IconName::Check, 12.)
                                .text_color(theme::BRAND()),
                        )
                    })
                    // 运行中行尾打断钮(自绘方块与 composer 停止钮同款
                    // 视觉);豁免冒泡——点击打断不触发行切换
                    .when(running, |el| {
                        let s_stop = store.clone();
                        let stop_id = chip.session_id.clone();
                        el.child(
                            div()
                                .id(gpui_kit::SharedString::from(format!(
                                    "task-stop-{}",
                                    chip.session_id
                                )))
                                .debug_selector(|| "task-stop".to_string())
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .h(px(20.))
                                .px(px(8.))
                                .rounded(px(10.))
                                .border_1()
                                .border_color(theme::BORDER_2())
                                .cursor_pointer()
                                .hover(|st| {
                                    st.bg(theme::DANGER()).border_color(theme::DANGER())
                                })
                                .text_size(px(11.))
                                .text_color(theme::LABEL_2())
                                .child("打断")
                                .on_mouse_down(
                                    gpui_kit::MouseButton::Left,
                                    |_, _, cx| cx.stop_propagation(),
                                )
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    let id = stop_id.clone();
                                    s_stop.update(cx, |st, cx| st.interrupt_subagent(&id, cx));
                                }),
                        )
                    })
                    .into_any_element()
            })
        .collect();
    let list = open.then(|| {
        div()
            .v_flex()
            .gap(px(2.))
            .px(px(8.))
            .pb(px(8.))
            .children(list_rows)
    });
    Some(
        div()
            .id("task-bar")
            .debug_selector(|| "task-bar".to_string())
            .w_full()
            .v_flex()
            .rounded(px(14.))
            .border_1()
            .border_color(theme::BORDER())
            .bg(theme::LAYER())
            .overflow_hidden()
            .child(head)
            .when_some(list, |el, list| el.child(list))
            .into_any_element(),
    )
}

