//! TodoDock(web `TodoPanel`):composer 上方的计划条,默认折叠一行
//! (标题 + 计数);展开为条目列表。空列表不渲染。

use gpui_kit::component::IconName;
use gpui_kit::component::StyledExt;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, px,
};

use super::projection::TodoItem;
use crate::kits::icons::{DshIcon, fixed};
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 计划条整体(todos 为空返回空占位由调用方跳过)
pub fn render(store: &Entity<AppStore>, cx: &App) -> Option<impl IntoElement> {
    let st = store.read(cx);
    let chat = st.current_chat()?;
    if chat.todos.is_empty() {
        return None;
    }
    let open = st.chat.todo_open;
    let counts = todo_counts(&chat.todos);
    let s = store.clone();
    Some(
        div()
            .id("todo-dock")
            // 测试钩子:tab 门控断言(release 空操作)
            .debug_selector(|| "todo-dock".to_string())
            .w_full()
            .v_flex()
            .rounded(px(14.))
            .border_1()
            .border_color(theme::BORDER())
            .bg(theme::LAYER())
            .overflow_hidden()
            .child(
                div()
                    .id("todo-dock-head")
                    .flex()
                    .h(px(30.))
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .cursor_pointer()
                    .child(fixed(DshIcon::ListChecks, 14.).text_color(theme::LABEL_2()))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::LABEL_2())
                            .child("计划"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.))
                            .text_color(theme::CAPTION())
                            .child(format!(
                                "{done} 完成 · {active} 进行 · {pending} 待办",
                                done = counts.0,
                                active = counts.1,
                                pending = counts.2,
                            )),
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
                        s.update(cx, |st, cx| st.toggle_todo(cx));
                    }),
            )
            .when(open, |el| {
                el.child(
                    div()
                        .v_flex()
                        .gap(px(2.))
                        .px(px(12.))
                        .pb(px(8.))
                        .children(chat.todos.iter().map(todo_row).collect::<Vec<_>>()),
                )
            }),
    )
}

/// 单条 todo(状态点 + 内容;todo_write 工具卡展开体复用同一视觉)
pub(crate) fn todo_row(item: &TodoItem) -> impl IntoElement {
    let dot = match item.status.as_str() {
        "completed" => theme::SUCCESS(),
        "in_progress" => theme::BRAND(),
        _ => theme::CAPTION(),
    };
    let fg = if item.status == "completed" {
        theme::CAPTION()
    } else {
        theme::LABEL_2()
    };
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .py(px(2.))
        .child(div().size(px(6.)).flex_shrink_0().rounded_full().bg(dot))
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .text_size(px(12.))
                .text_color(fg)
                .child(item.content.clone()),
        )
}

/// (done, in_progress, pending) 计数
fn todo_counts(todos: &[TodoItem]) -> (usize, usize, usize) {
    let mut c = (0, 0, 0);
    for t in todos {
        match t.status.as_str() {
            "completed" => c.0 += 1,
            "in_progress" => c.1 += 1,
            _ => c.2 += 1,
        }
    }
    c
}
