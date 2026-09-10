//! 问答卡:
//! 模型 `ask_user_question` 抛出的问题集,pager 分页一题一答(单选/多选),
//! 可自定义文本;提交(整批)或放弃(取消)。应答经 host.respond 回填,
//! 工具结果作为同一 tool-call 的 tool/result。

use gpui_kit::component::IconName;
use gpui_kit::component::StyledExt;
use gpui_kit::component::input::Textarea;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Window, div, px,
};

use crate::kits::icons::fixed;
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 问答卡整体(无 pending_ask → None;挂在消息列 children 链)。
pub fn render(
    store: &Entity<AppStore>,
    window: &mut Window,
    cx: &mut App,
) -> Option<impl IntoElement> {
    let ask = store.read(cx).state.pending_ask.clone()?;
    let current = store.read(cx).state.current_id.clone()?;
    if ask.session_id != current {
        eprintln!(
            "[q] 问答卡跳过:帧 session={} != 当前 {}",
            ask.session_id, current
        );
        return None;
    }
    let total = ask.questions.len();
    let index = store
        .read(cx)
        .ask
        .ask_state
        .as_ref()
        .map(|s| s.index)
        .unwrap_or(0)
        .min(total.saturating_sub(1));
    let q = ask.questions.get(index)?;
    let selected: Vec<String> = store
        .read(cx)
        .ask
        .ask_state
        .as_ref()
        .and_then(|s| s.selected.get(index))
        .cloned()
        .unwrap_or_default();
    let custom: String = store
        .read(cx)
        .ask
        .ask_state
        .as_ref()
        .and_then(|s| s.custom.get(index))
        .cloned()
        .unwrap_or_default();
    // 「其他」输入懒建 + 值随当前题同步(与 composer 清空同:渲染期 set_value)
    store.update(cx, |st, cx| st.ensure_ask_input(window, cx));
    if let Some(input) = store.read(cx).ask.ask_input.clone() {
        let displayed = input.read(cx).value().to_string();
        if displayed != custom {
            input.update(cx, |s, cx| s.set_value(custom.clone(), window, cx));
        }
    }
    let multi = q.multi_select.unwrap_or(false);
    let options = q.options.clone().unwrap_or_default();

    let toggle = store.clone();
    let idx = index;
    let mut option_rows: Vec<gpui_kit::AnyElement> = Vec::new();
    for (oi, opt) in options.iter().enumerate() {
        let label = opt.label.clone();
        let desc = opt.description.clone();
        let is_sel = selected.contains(&label);
        let s2 = toggle.clone();
        let label_c = label.clone();
        option_rows.push(
            div()
                .id(("ask-opt", oi))
                .debug_selector(move || format!("ask-opt-{}", label_c).to_string())
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .rounded(px(8.))
                .px(px(10.))
                .py(px(8.))
                .cursor_pointer()
                .bg(if is_sel {
                    theme::DOCK()
                } else {
                    theme::LAYER()
                })
                .border_1()
                .border_color(if is_sel {
                    theme::BRAND()
                } else {
                    theme::BORDER()
                })
                .on_click({
                    let l = label.clone();
                    move |_, _, cx| {
                        let l = l.clone();
                        s2.update(cx, |st, cx| st.toggle_ask_option(&l, cx));
                    }
                })
                .child(
                    div()
                        .size(px(18.))
                        .flex_shrink_0()
                        .rounded_full()
                        .border_1()
                        .border_color(if is_sel {
                            theme::BRAND()
                        } else {
                            theme::CAPTION()
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(div().when(is_sel, |el| {
                            el.child(fixed(IconName::Check, 12.).text_color(theme::BRAND()))
                        })),
                )
                .child(
                    // min_w(0):flex item 缺省最小宽 = 内容 max-content,长
                    // ASCII 词元(不可断行)会把卡片撑出弹窗——压回可用宽度
                    // 让文本换行(D54 @补全同款修复)
                    div()
                        .v_flex()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme::LABEL())
                                .child(label),
                        )
                        .when(desc.is_some(), |el| {
                            let d = desc.clone();
                            el.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme::CAPTION())
                                    .child(d.unwrap_or_default()),
                            )
                        }),
                )
                .into_any_element(),
        );
    }

    let submit = store.clone();
    let cancel = store.clone();
    let prev = store.clone();
    let next = store.clone();
    Some(
        div()
            .id("ask-question")
            .debug_selector(|| "ask-question".to_string())
            .w_full()
            .v_flex()
            .rounded(px(14.))
            .border_1()
            .border_color(theme::BORDER())
            .bg(theme::LAYER())
            .p(px(14.))
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(theme::LABEL())
                            .child(q.header.clone().unwrap_or_else(|| q.question.clone())),
                    )
                    .child(
                        div()
                            .id("ask-cancel")
                            .size(px(24.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme::CAPTION())
                            .hover(|s| s.bg(theme::DOCK()))
                            .on_click(move |_, _, cx| cancel.update(cx, |st, cx| st.cancel_ask(cx)))
                            .child(fixed(IconName::Close, 12.)),
                    ),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(theme::LABEL())
                    .child(q.question.clone()),
            )
            .children(option_rows)
            // 「其他」自定义行——checkbox + 可输入框;
            // 输入即视为「其他」答案(单选取清选中,多选保留已勾)。
            .child(
                div()
                    .id("ask-custom")
                    .flex()
                    .min_h(px(34.))
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(if custom.is_empty() {
                        theme::BORDER()
                    } else {
                        theme::BRAND()
                    })
                    .bg(if custom.is_empty() {
                        theme::BASE()
                    } else {
                        theme::DOCK()
                    })
                    .px(px(10.))
                    // 与上方选项一致的 checkbox:单选圆形、多选方形,有值即打勾
                    .child(
                        div()
                            .flex_shrink_0()
                            .size(px(18.))
                            .rounded(if multi { px(4.) } else { px(9.) })
                            .border_1()
                            .border_color(if custom.is_empty() {
                                theme::CAPTION()
                            } else {
                                theme::BRAND()
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().when(!custom.is_empty(), |el| {
                                el.child(fixed(IconName::Check, 12.).text_color(theme::BRAND()))
                            })),
                    )
                    .when(custom.is_empty(), |el| {
                        el.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(12.))
                                .text_color(theme::CAPTION())
                                .child("其他"),
                        )
                    })
                    .child(
                        store
                            .read(cx)
                            .ask
                            .ask_input
                            .clone()
                            .map(|input| {
                                Textarea::new(&input)
                                    .appearance(false)
                                    .text_size(px(13.))
                                    .into_any_element()
                            })
                            .unwrap_or_else(|| div().child(custom.clone()).into_any_element()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    // 单题无需翻页(源 pager 单题也隐藏)
                    .when(total > 1, |el| {
                        el.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(nav_button(
                                    "ask-prev",
                                    "上一题",
                                    index > 0,
                                    move |_, _, cx| {
                                        let idx = idx.saturating_sub(1);
                                        prev.update(cx, |st, cx| st.set_ask_index(idx, cx));
                                    },
                                ))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme::CAPTION())
                                        .child(format!("{}/{}", index + 1, total)),
                                )
                                .child(nav_button(
                                    "ask-next",
                                    "下一题",
                                    index + 1 < total,
                                    move |_, _, cx| {
                                        let idx = idx + 1;
                                        next.update(cx, |st, cx| st.set_ask_index(idx, cx));
                                    },
                                )),
                        )
                    })
                    .child(
                        div()
                            .id("ask-submit")
                            .flex()
                            .h(px(28.))
                            .items_center()
                            .justify_center()
                            .rounded(px(14.))
                            .bg(theme::BRAND())
                            .px(px(16.))
                            .cursor_pointer()
                            .text_size(px(13.))
                            .text_color(gpui_kit::white())
                            .on_click(move |_, _, cx| {
                                submit.update(cx, |st, cx| st.submit_ask(cx));
                            })
                            .child("提交"),
                    ),
            ),
    )
}

/// 翻题小钮(disabled 时隐藏)
fn nav_button(
    id: &'static str,
    label: &str,
    enabled: bool,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut App) + 'static,
) -> impl IntoElement {
    let sel = id.to_string();
    div()
        .id(id)
        .debug_selector(move || sel.clone())
        .flex()
        .h(px(24.))
        .items_center()
        .rounded(px(12.))
        .px(px(10.))
        .text_size(px(12.))
        .text_color(theme::CAPTION())
        .when(enabled, |el| {
            el.cursor_pointer()
                .hover(|s| s.bg(theme::DOCK()))
                .on_click(on_click)
        })
        .child(label.to_string())
}
