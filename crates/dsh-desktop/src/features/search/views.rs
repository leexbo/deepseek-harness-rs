//! 全库检索 UI(从 ui::sidebar 切出):侧栏搜索框(本地过滤 + Enter
//! 触发全库检索)与命中面板(替换会话列表;行点击 = 开会话切轨迹定位)。

use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, px,
};
use gpui_kit::component::IconName;
use gpui_kit::component::Sizable;
use gpui_kit::component::StyledExt;
use gpui_kit::component::input::Input;

use crate::kits::icons::{DshIcon, fixed};
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 搜索框(本地过滤;前导放大镜)
pub(crate) fn search_row(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let st = store.read(cx);
    // 图标经 x_padding 让位嵌进输入框内部(左内嵌)
    let input = st.search.search_input.as_ref().map(|e| {
        div().flex_1().min_w(px(0.)).h(px(28.)).child(
            Input::new(e)
                .small()
                .prefix(fixed(IconName::Search, 13.).text_color(theme::CAPTION())),
        )
    });
    div().flex().h(px(34.)).items_center().children(input)
}

/// 全库检索命中面板:回车检索后替换会话列表;行 = 会话标题
/// + 命中内容摘要,点击 = 开会话切轨迹定位台账行;清空搜索框即回列表
pub(crate) fn search_hits_panel(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let st = store.read(cx);
    let hits = st.search.search_hits.clone().unwrap_or_default();
    let back = store.clone();
    let mut rows: Vec<gpui_kit::AnyElement> = vec![
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::CAPTION())
                    .flex_1()
                    .child(format!("全库检索 · {} 条命中", hits.len())),
            )
            .child(
                div()
                    .id("search-hits-back")
                    .debug_selector(|| "search-hits-back".to_string())
                    .flex()
                    .h(px(20.))
                    .items_center()
                    .px(px(6.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(theme::LABEL_3())
                    .hover(|s| s.bg(theme::DOCK()))
                    .child("返回列表")
                    .on_click(move |_, window, cx| {
                        back.update(cx, |st, cx| {
                            st.search.search_hits = None;
                            if let Some(input) = &st.search.search_input {
                                input.update(cx, |i, cx| i.set_value("", window, cx));
                            }
                            cx.notify();
                        });
                    }),
            )
            .into_any_element(),
    ];
    if hits.is_empty() {
        rows.push(
            div()
                .text_size(px(12.))
                .text_color(theme::LABEL_3())
                .child("无命中")
                .into_any_element(),
        );
    }
    for (ix, h) in hits.iter().enumerate() {
        let s = store.clone();
        let sid = h["sessionId"].as_str().unwrap_or_default().to_string();
        let seq = h["seq"].as_u64().unwrap_or(0);
        let title = st.title_for(&sid);
        let kind = h["kind"].as_str().unwrap_or_default();
        let preview: String = {
            let c = h["content"].as_str().unwrap_or_default();
            let first = c.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            first.chars().take(40).collect()
        };
        let sel = format!("search-hit-{ix}");
        rows.push(
            div()
                .id(("search-hit", ix))
                .debug_selector(move || sel.clone())
                .flex()
                .h(px(34.))
                .items_center()
                .gap(px(8.))
                .rounded(px(8.))
                .pl(px(8.))
                .pr(px(8.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::LAYER()))
                .child(fixed(DshIcon::MessageSquare, 14.).text_color(theme::LABEL_3()))
                .child(
                    div()
                        .v_flex()
                        .min_w(px(0.))
                        .flex_1()
                        .gap(px(1.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .truncate()
                                .text_color(theme::LABEL_2())
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .truncate()
                                .text_color(theme::CAPTION())
                                .child(format!("[{kind}] {preview}")),
                        ),
                )
                .on_click(move |_, _, cx| {
                    let (sid, seq) = (sid.clone(), seq);
                    s.update(cx, |st, cx| st.open_search_hit(&sid, seq, cx));
                })
                .into_any_element(),
        );
    }
    div()
        .id("search-hits-panel")
        .debug_selector(|| "search-hits-panel".to_string())
        .v_flex()
        .min_h(px(0.))
        .flex_1()
        .overflow_y_scroll()
        .gap(px(2.))
        .pb(px(8.))
        .children(rows)
}
