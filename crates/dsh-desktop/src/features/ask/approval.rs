//! 沙箱升级审批卡(intent = sandbox-escalation):bash 命令被沙箱拒绝
//! 后,模型带 `sandbox_permissions` + `justification` 请求一次性加宽;
//! 卡片信任锚 = **命令原文 + 目标模式**(justification 是模型写的不可信
//! 文本)。交互 = 一步两钮(批准一次 / 拒绝,✕ = 取消)——源语义:被拒
//! 对该命令终局,无反馈通道;批准只盖本次执行,不落 sandbox/mode。

use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, px,
};
use gpui_kit::component::IconName;
use gpui_kit::component::StyledExt;

use crate::kits::icons::fixed;
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 审批卡(当前会话有待批升级时渲染;批准/拒绝/取消 → host.respond)
pub fn render(store: &Entity<AppStore>, cx: &App) -> Option<impl IntoElement> {
    let approval = store.read(cx).state.pending_approval.clone()?;
    let current = store.read(cx).state.current_id.clone()?;
    if approval.session_id != current {
        return None;
    }
    let data = approval.question.data.clone().unwrap_or_default();
    let command = data["command"].as_str().unwrap_or_default().to_string();
    let current_mode = data["currentMode"].as_str().unwrap_or_default().to_string();
    let target_mode = data["targetMode"].as_str().unwrap_or_default().to_string();
    let tool_name = data["toolName"].as_str().unwrap_or("bash").to_string();
    let justification = approval.question.question.clone();
    let (approve, reject, dismiss) = (store.clone(), store.clone(), store.clone());
    Some(
        div()
            .id("approval-card")
            .debug_selector(|| "approval-card".to_string())
            .w_full()
            .v_flex()
            .gap(px(10.))
            .rounded(px(14.))
            .border_1()
            .border_color(theme::BORDER())
            .bg(theme::LAYER())
            .p(px(14.))
            // 标题行:工具名 + 模式迁移;✕ = 取消请求
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
                            .child("沙箱升级审批"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::CAPTION())
                            .child(format!("{tool_name} · {current_mode} → {target_mode}(仅本次)")),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("approval-dismiss")
                            .debug_selector(|| "approval-dismiss".to_string())
                            .size(px(24.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme::CAPTION())
                            .hover(|s| s.bg(theme::DOCK()))
                            .on_click(move |_, _, cx| {
                                dismiss.update(cx, |st, cx| st.dismiss_approval(cx));
                            })
                            .child(fixed(IconName::Close, 12.)),
                    ),
            )
            // 命令原文(信任锚:全量可读,不截断)
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .rounded(px(8.))
                    .bg(theme::DOCK())
                    .p(px(10.))
                    .text_size(px(12.))
                    .text_color(theme::LABEL())
                    .child(command),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::CAPTION())
                    .child(justification),
            )
            // 一步两钮:批准一次(allow-once)/ 拒绝(对该命令终局)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        div()
                            .id("approval-reject")
                            .debug_selector(|| "approval-reject".to_string())
                            .flex()
                            .h(px(28.))
                            .items_center()
                            .justify_center()
                            .rounded(px(14.))
                            .border_1()
                            .border_color(theme::BORDER())
                            .px(px(16.))
                            .cursor_pointer()
                            .text_size(px(13.))
                            .text_color(theme::LABEL_2())
                            .hover(|s| s.bg(theme::DOCK()))
                            .on_click(move |_, _, cx| {
                                reject.update(cx, |st, cx| st.answer_approval(false, cx));
                            })
                            .child("拒绝"),
                    )
                    .child(
                        div()
                            .id("approval-approve")
                            .debug_selector(|| "approval-approve".to_string())
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
                                approve.update(cx, |st, cx| st.answer_approval(true, cx));
                            })
                            .child("批准一次"),
                    ),
            ),
    )
}
