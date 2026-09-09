//! 消息反馈 UI:
//! assistant 消息动作行的赞/踩/备注按钮 + 备注弹窗。
//! 反馈是 host sidecar(per-session JSON),不进模型上下文;
//! 语义:点当前评分 = 删除;点另一评分 = put 带原备注前移。

use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, px,
};
use gpui_kit::component::IconName;
use gpui_kit::component::StyledExt;
use gpui_kit::component::input::Textarea;

use crate::kits::icons::fixed;
use crate::kits::theme;
use crate::shell::store::AppStore;

/// assistant 动作行的反馈控件(赞/踩/备注;返回子元素数组)。
pub fn actions(store: &Entity<AppStore>, message_id: &str, cx: &App) -> Vec<gpui_kit::AnyElement> {
    let st = store.read(cx);
    if message_id.is_empty() {
        return Vec::new();
    }
    let item = st.feedback.feedback_by_message.get(message_id).cloned();
    let rating = item.as_ref().map(|it| it.rating.as_str()).unwrap_or("");
    let note = item.as_ref().and_then(|it| it.note.clone());
    let mut els: Vec<gpui_kit::AnyElement> = Vec::new();

    // 赞
    let like_store = store.clone();
    let like_sel = message_id.to_string();
    let like_active = rating == "positive";
    els.push(
        feedback_btn(
            gpui_kit::SharedString::from(format!("fb-like-{like_sel}")),
            IconName::ThumbsUp,
            like_active,
            move |cx| {
                let s = like_store.clone();
                let m = like_sel.clone();
                s.update(cx, |st, cx| {
                    st.rate_message(&m, "positive", None, cx);
                });
            },
        )
        .into_any_element(),
    );
    // 踩
    let dislike_store = store.clone();
    let dislike_sel = message_id.to_string();
    let dislike_active = rating == "negative";
    els.push(
        feedback_btn(
            gpui_kit::SharedString::from(format!("fb-dislike-{dislike_sel}")),
            IconName::ThumbsDown,
            dislike_active,
            move |cx| {
                let s = dislike_store.clone();
                let m = dislike_sel.clone();
                s.update(cx, |st, cx| {
                    st.rate_message(&m, "negative", None, cx);
                });
            },
        )
        .into_any_element(),
    );
    // 备注钮:仅在有评分时出现(源 MessageFeedbackActions.tsx:262 语义
    // `rating !== undefined &&`)。无评分时动作行 = 复制+赞/踩,不常驻
    // 「补充说明」文字;有了评分才让用户补一句说明。
    if rating.is_empty() {
        return els;
    }
    let note_store = store.clone();
    let note_sel = message_id.to_string();
    els.push(
        div()
            .id(gpui_kit::SharedString::from(format!("fb-note-{note_sel}")))
            .flex()
            .h(px(24.))
            .items_center()
            .rounded(px(12.))
            .px(px(8.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::DOCK()))
            .text_size(px(11.))
            .text_color(if note.is_some() {
                theme::BRAND()
            } else {
                theme::CAPTION()
            })
            .on_click(move |ev: &gpui_kit::ClickEvent, window, cx| {
                let s = note_store.clone();
                let m = note_sel.clone();
                let pos = match ev {
                    gpui_kit::ClickEvent::Mouse(mi) => mi.down.position,
                    gpui_kit::ClickEvent::Keyboard(_) => gpui_kit::Point::default(),
                    gpui_kit::ClickEvent::Touch(_) => gpui_kit::Point::default(),
                };
                s.update(cx, |st, cx| st.open_feedback_note(window, &m, pos, cx));
            })
            .child(if note.is_some() {
                note.clone().unwrap_or_default()
            } else {
                "补充说明".to_string()
            })
            .into_any_element(),
    );
    els
}

/// 单个评分钮(选中态高亮 + 点击评分)
fn feedback_btn(
    id: impl Into<gpui_kit::ElementId>,
    icon: IconName,
    active: bool,
    on_click: impl Fn(&mut App) + 'static,
) -> impl IntoElement {
    let sel = "feedback-btn".to_string();
    div()
        .id(id)
        .debug_selector(move || sel.clone())
        .size(px(24.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|s| s.bg(theme::DOCK()))
        .text_color(if active { theme::BRAND() } else { theme::CAPTION() })
        .on_click(move |_, _, cx| on_click(cx))
        .child(fixed(icon, 12.))
}

/// 备注弹窗(根级渲染;open → 锚定在「补充说明」钮下方的 popover,非居中模态。
/// 定位循源 useAnchoredPosition:trigger 下缘 + gap(4px),面板靠右展开。
/// 透明全屏层捕获外点关闭(无视觉遮罩),卡片 occlude 防穿透)
pub fn render_note_editor(store: &Entity<AppStore>, cx: &mut App) -> Option<impl IntoElement> {
    let (_message_id, text) = store.read(cx).feedback.feedback_note_editor.clone()?;
    let anchor = store.read(cx).feedback.feedback_note_anchor?;
    let close = store.clone();
    let close2 = store.clone();
    let save = store.clone();
    Some(
        // 透明全屏命中层:点卡片外任意处关闭(无遮罩视觉层)
        div()
            .id("fb-note-dismiss")
            .absolute()
            .inset_0()
            .on_mouse_down(gpui_kit::MouseButton::Left, move |_, _, cx| {
                close.update(cx, |st, cx| st.close_feedback_note(cx))
            })
            .child(
                div()
                    .id("fb-note-pop")
                    .debug_selector(|| "fb-note-pop".to_string())
                    .absolute()
                    // 锚在触发钮下方:按钮下缘(锚点 y + 约钮高)= 弹层顶;
                    .top(anchor.y + px(30.))
                    .left(anchor.x + px(4.))
                    // 阻断鼠标命中向后方穿透(否则点面板会落到下面消息上)
                    .occlude()
                    .v_flex()
                    .gap(px(10.))
                    .w(px(320.))
                    .max_w(px(360.))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(theme::BORDER())
                    .bg(theme::LAYER())
                    .p(px(12.))
                    .shadow_md()
                    .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .id("fb-note-input")
                            .flex()
                            .min_h(px(72.))
                            .items_center()
                            .rounded(px(8.))
                            .border_1()
                            .border_color(theme::BORDER())
                            .bg(theme::BASE())
                            .px(px(10.))
                            .py(px(8.))
                            .child(
                                store
                                    .read(cx)
                                    .feedback
                                    .feedback_input
                                    .clone()
                                    .map(|input| {
                                        Textarea::new(&input)
                                            .appearance(false)
                                            .text_size(px(13.))
                                            .line_height(gpui_kit::relative(1.5))
                                            .into_any_element()
                                    })
                                    .unwrap_or_else(|| {
                                        div().child(text.clone()).into_any_element()
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap(px(8.))
                            .child(
                                div()
                                    .id("fb-note-save")
                                    .flex()
                                    .h(px(28.))
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(14.))
                                    // 白色主按钮(白底深字)
                                    .bg(theme::LABEL())
                                    .px(px(16.))
                                    .text_size(px(13.))
                                    .text_color(gpui_kit::black())
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme::LABEL_2()))
                                    .on_click(move |_, _, cx| {
                                        save.update(cx, |st, cx| st.commit_feedback_note(cx));
                                    })
                                    .child("保存"),
                            )
                            .child(
                                div()
                                    .id("fb-note-cancel")
                                    .flex()
                                    .h(px(28.))
                                    .items_center()
                                    .rounded(px(14.))
                                    .px(px(12.))
                                    .text_size(px(13.))
                                    .text_color(theme::CAPTION())
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme::DOCK()))
                                    .on_click(move |_, _, cx| {
                                        close2.update(cx, |st, cx| st.close_feedback_note(cx));
                                    })
                                    .child("取消"),
                            ),
                    ),
            ),
    )
}
