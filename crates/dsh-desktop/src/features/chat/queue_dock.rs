//! 队列条带(源 QueueDock 对应物):composer 上方渲染 `queued` 落位的
//! 待运行条目。单条直显;多条 = 计数头 + 可折叠列表。行内动作:
//! 编辑(行内输入)/ 立即投递(steer)/ 移除——走 host `update_queue`
//! (edit/remove/steer),变更后 `session/queue` 帧自动广播回填。
//! `steering` 落位不在本组件(消息流尾部的插队气泡,见 chat_pane)。

use gpui_kit::component::IconName;
use gpui_kit::component::Sizable;
use gpui_kit::component::StyledExt;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::features::chat::{QueueEntry, QueuePlacement};
use crate::kits::icons::{DshIcon, fixed};
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 队列条带(空 = 不渲染;仅 `queued` 落位;挂 composer 正上方)
pub fn render(store: &Entity<AppStore>, window: &mut Window, cx: &mut App) -> impl IntoElement {
    let (col_w, queued, collapsed, editing, session_id) = {
        let st = store.read(cx);
        let session_id = st.state.current_id.clone();
        let Some(session_id) = session_id else {
            return div().into_any_element();
        };
        let Some(chat) = st.state.chats.get(&session_id) else {
            return div().into_any_element();
        };
        let queued: Vec<QueueEntry> = chat
            .queue
            .iter()
            .filter(|e| e.placement == QueuePlacement::Queued)
            .cloned()
            .collect();
        let col_w = crate::shell::metrics::window_chat_col_w(
            window,
            st.sidebar_collapsed,
            st.sidebar_px,
            st.panel_open,
            st.panel_px,
        );
        let editing = st.chat.queue_editing.clone();
        let collapsed = st.chat.queue_dock_collapsed;
        (col_w, queued, collapsed, editing, session_id)
    };
    if queued.is_empty() {
        return div().into_any_element();
    }
    // 单条恒直显;多条默认折叠成计数头(点开列表)
    let collapsed = collapsed && queued.len() > 1;
    let is_editing = editing.as_deref().is_some();
    let mut dock = div()
        .debug_selector(|| "queue-dock".to_string())
        .v_flex()
        .mx_auto()
        .w(col_w)
        .mb(px(8.))
        .rounded(px(10.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::LAYER())
        .p(px(6.))
        .gap(px(4.));
    // 多条 = 计数头(可折叠;编辑态强制展开由 list_visible 承担)
    if queued.len() > 1 {
        let s = store.clone();
        let collapsed_now = st_chat_collapsed(store, cx);
        dock = dock.child(
            div()
                .id("queue-dock-header")
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(26.))
                .px(px(4.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(theme::LABEL_2())
                .hover(|s| s.bg(theme::DOCK()))
                .child(fixed(DshIcon::ListChecks, 13.))
                .child(format!("队列 {} 条", queued.len()))
                .child(div().flex_1())
                .child(fixed(
                    if collapsed_now {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    },
                    12.,
                ))
                .on_click(move |_, _, cx| {
                    s.update(cx, |st, cx| st.toggle_queue_dock_collapse(cx));
                }),
        );
    }
    let list_visible = !collapsed || is_editing;
    if list_visible {
        for entry in &queued {
            dock = dock.child(queue_row(store, entry, &session_id, &editing, window, cx));
        }
    }
    dock.into_any_element()
}

fn st_chat_collapsed(store: &Entity<AppStore>, cx: &App) -> bool {
    store.read(cx).chat.queue_dock_collapsed
}

/// 队列行:preview(或行内编辑输入)+ 动作钮(保存/取消 或 编辑/立即投递/移除)
fn queue_row(
    store: &Entity<AppStore>,
    entry: &QueueEntry,
    session_id: &str,
    editing: &Option<String>,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let is_editing = editing.as_deref() == Some(entry.id.as_str());
    let editable = entry.text.is_some();
    // 编辑态:输入框(值存 ChatStore.queue_edit_text;渲染期惰建由 store
    // 保证——进入编辑态时已建)
    let mut row = div()
        .id(sid("queue-row", &entry.id))
        .debug_selector(move || format!("queue-row-{}", entry.id))
        .flex()
        .items_center()
        .gap(px(6.))
        .min_h(px(30.))
        .px(px(6.))
        .rounded(px(8.))
        .hover(|s| s.bg(theme::DOCK()))
        .child(fixed(DshIcon::ListChecks, 13.).text_color(theme::CAPTION()));
    if is_editing {
        let st = store.read(cx);
        let input = st.chat.queue_edit_input.clone();
        if let Some(input) = input {
            row = row.child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h(px(28.))
                    .child(gpui_kit::component::input::Input::new(&input).small()),
            );
        }
    } else {
        row = row.child(
            div()
                .min_w(px(0.))
                .flex_1()
                .truncate()
                .text_size(px(13.))
                .text_color(theme::LABEL_2())
                .child(entry.preview.clone()),
        );
    }
    row.child(queue_actions(
        store, entry, session_id, is_editing, editable, window, cx,
    ))
}

/// 行内动作钮组(编辑态 = 保存/取消;常态 = 编辑/立即投递/移除)
#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
fn queue_actions(
    store: &Entity<AppStore>,
    entry: &QueueEntry,
    session_id: &str,
    is_editing: bool,
    editable: bool,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let session_id = SharedString::from(session_id.to_string());
    let item_id = entry.id.clone();
    let _ = (window, cx);
    if is_editing {
        let (s_save, s_cancel) = (store.clone(), store.clone());
        let (sid_c, iid_c) = (session_id.clone(), item_id.clone());
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(
                div()
                    .id("queue-save")
                    .flex()
                    .size(px(22.))
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(theme::SUCCESS())
                    .hover(|s| s.bg(theme::BUBBLE()))
                    .child(fixed(IconName::Check, 13.))
                    .on_click(move |_, _, cx| {
                        s_save.update(cx, |st, cx| {
                            st.queue_save_edit(&sid_c, &iid_c, cx);
                        });
                    }),
            )
            .child(
                div()
                    .id("queue-cancel-edit")
                    .flex()
                    .size(px(22.))
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(theme::CAPTION())
                    .hover(|s| s.bg(theme::BUBBLE()))
                    .child(fixed(IconName::Close, 13.))
                    .on_click(move |_, _, cx| {
                        s_cancel.update(cx, |st, cx| st.queue_cancel_edit(cx));
                    }),
            )
            .into_any_element()
    } else {
        let (s_edit, s_steer, s_remove) = (store.clone(), store.clone(), store.clone());
        let (sid_e, iid_e) = (session_id.clone(), item_id.clone());
        let (sid_s, iid_s) = (session_id.clone(), item_id.clone());
        let (sid_r, iid_r) = (session_id.clone(), item_id.clone());
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .when(editable, |el| {
                el.child(
                    div()
                        .id("queue-edit")
                        .flex()
                        .size(px(22.))
                        .items_center()
                        .justify_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .hover(|s| s.bg(theme::BUBBLE()).text_color(theme::LABEL()))
                        .child("编辑")
                        .on_click(move |_, window, cx| {
                            s_edit.update(cx, |st, cx| {
                                st.queue_begin_edit(&sid_e, &iid_e, window, cx);
                            });
                        }),
                )
            })
            .child(
                div()
                    .id("queue-steer")
                    .flex()
                    .size(px(22.))
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(theme::CAPTION())
                    .hover(|s| s.bg(theme::BUBBLE()).text_color(theme::LABEL()))
                    .child(fixed(IconName::ArrowUp, 13.))
                    .on_click(move |_, _, cx| {
                        s_steer.update(cx, |st, cx| {
                            st.queue_action(
                                &sid_s,
                                &iid_s,
                                serde_json::json!({ "kind": "steer" }),
                                cx,
                            );
                        });
                    }),
            )
            .child(
                div()
                    .id("queue-remove")
                    .flex()
                    .size(px(22.))
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(theme::CAPTION())
                    .hover(|s| s.bg(theme::BUBBLE()).text_color(theme::DANGER()))
                    .child(fixed(IconName::Close, 13.))
                    .on_click(move |_, _, cx| {
                        s_remove.update(cx, |st, cx| {
                            st.queue_action(
                                &sid_r,
                                &iid_r,
                                serde_json::json!({ "kind": "remove" }),
                                cx,
                            );
                        });
                    }),
            )
            .into_any_element()
    }
}

fn sid(prefix: &str, key: &str) -> SharedString {
    SharedString::from(format!("{prefix}-{key}"))
}
