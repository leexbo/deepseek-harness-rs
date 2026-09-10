//! 计划审批卡(接管输入区):**计划查看与批准
//! 分离**——卡不内嵌计划正文(计划经聊天归档卡「查看」/右栏计划标签
//! 阅读)。交互 = **选择→批准两步**:
//! 点选项行只标记选择(高亮),底部「批准」主钮才提交——
//! ①是,实施此计划 ②否,并告诉它应该如何做不同(选中②展开行内输入;
//! 有反馈 = 拒绝+反馈经引导轮直送模型,空反馈 = 仅拒绝);
//! ✕ = 取消请求回到对话。

use gpui_kit::component::IconName;
use gpui_kit::component::StyledExt;
use gpui_kit::component::input::Textarea;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Window, div, px,
};

use crate::kits::icons::{DshIcon, fixed};
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 审批卡(当前会话有待审计划时渲染;批准/拒绝/取消 → host.respond)
pub fn render(
    store: &Entity<AppStore>,
    window: &mut Window,
    cx: &mut App,
) -> Option<impl IntoElement> {
    // 只读状态预取(read 借用与渲染期 &mut cx 互斥)
    let plan = store.read(cx).state.pending_plan.clone()?;
    let current = store.read(cx).state.current_id.clone()?;
    if plan.session_id != current {
        return None;
    }
    store.update(cx, |st, cx| st.ensure_plan_decline_input(window, cx));
    let (choose_approve, choose_decline, submit, dismiss, view) = (
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
    );
    let decline_input = store.read(cx).ask.plan_decline_input.clone();
    let selection = store.read(cx).ask.plan_selection;
    let title = plan
        .question
        .header
        .clone()
        .unwrap_or_else(|| plan.question.question.clone());
    Some(
        div()
            .id("plan-review")
            .debug_selector(|| "plan-review".to_string())
            .w_full()
            .v_flex()
            .gap(px(10.))
            .rounded(px(14.))
            .border_1()
            .border_color(theme::BORDER())
            .bg(theme::LAYER())
            .p(px(14.))
            // 标题行:查看入口开右栏计划标签(复用查看链路);✕ = 取消
            // 请求回到对话(原「去聊天里说」语义)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(theme::LABEL())
                            .child(title),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("plan-view")
                            .debug_selector(|| "plan-view".to_string())
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .h(px(20.))
                            .px(px(6.))
                            .rounded(px(5.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(theme::CAPTION())
                            .hover(|s| s.bg(theme::DOCK()).text_color(theme::LABEL_2()))
                            .child(fixed(IconName::Eye, 12.))
                            .child("查看")
                            .on_click(move |_, _, cx| {
                                view.update(cx, |st, cx| {
                                    st.open_panel_tab(crate::shell::panel::PanelTab::Plan, cx)
                                });
                            }),
                    )
                    .child(
                        div()
                            .id("plan-dismiss")
                            .debug_selector(|| "plan-dismiss".to_string())
                            .size(px(24.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .text_color(theme::CAPTION())
                            .hover(|s| s.bg(theme::DOCK()))
                            .child(fixed(IconName::Close, 12.))
                            .on_click(move |_, _, cx| {
                                dismiss.update(cx, |st, cx| st.dismiss_plan(cx));
                            }),
                    ),
            )
            // 选项 1:是,实施此计划(点击=选择,不提交;选中高亮)
            .child(
                div()
                    .id("plan-approve")
                    .debug_selector(|| "plan-approve".to_string())
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(38.))
                    .px(px(10.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(if selection == Some(true) {
                        theme::WARN()
                    } else {
                        theme::BORDER()
                    })
                    .when(selection == Some(true), |el| el.bg(theme::DOCK()))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::DOCK()))
                    .child(
                        div()
                            .flex_shrink_0()
                            .size(px(18.))
                            .rounded_full()
                            .border_1()
                            .border_color(if selection == Some(true) {
                                theme::WARN()
                            } else {
                                theme::CAPTION()
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.))
                            .text_color(theme::CAPTION())
                            .child("1"),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme::LABEL())
                            .child("是,实施此计划"),
                    )
                    .on_click(move |_, _, cx| {
                        choose_approve.update(cx, |st, cx| st.select_plan_option(true, cx));
                    }),
            )
            // 选项 2:否,并告诉它应该如何做不同(点击=选择;选中展开
            // 行内输入;有反馈=拒绝+反馈,空反馈=仅拒绝)
            .child(
                div()
                    .id("plan-decline")
                    .debug_selector(|| "plan-decline".to_string())
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .min_h(px(38.))
                    .py(px(6.))
                    .px(px(10.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(if selection == Some(false) {
                        theme::WARN()
                    } else {
                        theme::BORDER()
                    })
                    .when(selection == Some(false), |el| el.bg(theme::DOCK()))
                    .cursor_pointer()
                    .child(
                        fixed(DshIcon::Pencil, 14.)
                            .flex_shrink_0()
                            .text_color(theme::CAPTION()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(13.))
                            .text_color(theme::LABEL())
                            .child("否,并告诉它应该如何做不同"),
                    )
                    .on_click(move |_, _, cx| {
                        choose_decline.update(cx, |st, cx| st.select_plan_option(false, cx));
                    }),
            )
            // 选中②展开的行内输入(反馈;Enter 同「批准」钮提交)
            .when(selection == Some(false), |el| {
                el.child(
                    div()
                        .id("plan-decline-input")
                        .debug_selector(|| "plan-decline-input".to_string())
                        .flex()
                        .items_center()
                        .min_h(px(34.))
                        .px(px(10.))
                        .py(px(4.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(theme::BORDER())
                        .child(
                            decline_input
                                .map(|input| {
                                    Textarea::new(&input)
                                        .appearance(false)
                                        .text_size(px(13.))
                                        .flex_1()
                                        .into_any_element()
                                })
                                .unwrap_or_else(|| div().flex_1().into_any_element()),
                        ),
                )
            })
            // 底部动作条:批准主钮(选择后可用;选择→批准
            // 两步,防误触一键批准)
            .child(
                div().flex().justify_end().child(
                    div()
                        .id("plan-confirm")
                        .debug_selector(|| "plan-confirm".to_string())
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(30.))
                        .min_w(px(72.))
                        .px(px(14.))
                        .rounded(px(15.))
                        .text_size(px(13.))
                        .when_some(selection, |el, _| {
                            el.bg(theme::LABEL())
                                .text_color(theme::LAYER())
                                .cursor_pointer()
                                .on_click(move |_, _, cx| {
                                    submit.update(cx, |st, cx| st.submit_plan_selection(cx));
                                })
                        })
                        .when(selection.is_none(), |el| {
                            el.border_1()
                                .border_color(theme::BORDER())
                                .text_color(theme::CAPTION())
                        })
                        .child(if selection == Some(false) {
                            "提交"
                        } else {
                            "批准"
                        }),
                ),
            ),
    )
}
