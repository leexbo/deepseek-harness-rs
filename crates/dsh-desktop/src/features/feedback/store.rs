//! 消息反馈功能的状态与行为切片:赞/踩/备注 + 备注弹窗。
//!
//! 承载 [`FeedbackStore`](AppStore 的 `feedback` 字段)与该域的
//! `impl AppStore` 扩展块。反馈是 host sidecar(per-session JSON),
//! 不进模型上下文;语义源 toggle:点当前评分 = 删除,点另一评分 =
//! put 带原备注前移。

use std::collections::HashMap;

use dsh_core::registry::MessageFeedbackItem;
use gpui_kit::{AppContext, Context, Entity, Window};
use gpui_kit::component::input::{InputEvent, TextareaState};

use crate::shell::store::AppStore;

/// 消息反馈功能切片状态(赞/踩/备注;host sidecar 缓存)。
/// 作为 [`AppStore::feedback`] 单字段组合入根;默认空。
#[derive(Default)]
pub(crate) struct FeedbackStore {
    /// 会话消息反馈缓存(messageId → item;list 加载后填充;rat/delete 更新)
    pub feedback_by_message: HashMap<String, MessageFeedbackItem>,
    /// 备注弹窗开态(Some = (messageId, 当前文本初值))
    pub feedback_note_editor: Option<(String, String)>,
    /// 备注弹窗锚点(触发钮的窗口坐标;root 级绝对定位锚在按钮下方)
    pub feedback_note_anchor: Option<gpui_kit::Point<gpui_kit::Pixels>>,
    /// 备注弹窗输入(可聚焦;挂窗后经 ensure_feedback_input 懒建,
    /// Change 订阅写回 feedback_note_editor 文本)
    pub feedback_input: Option<Entity<TextareaState>>,
}

impl AppStore {
    // ── 消息反馈(赞/踩 + 备注;host sidecar)──────────────────────

    /// 加载某 session 的反馈到缓存(懒加载;assistant 消息 action 行触发)
    pub fn load_feedback(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.state.current_id.clone() else {
            return;
        };
        if !self.feedback.feedback_by_message.is_empty() {
            return; // 已加载
        }
        let host = self.bridge.host().clone();
        for item in host.message_feedback_list(&id) {
            self.feedback
                .feedback_by_message
                .insert(item.message_id.clone(), item);
        }
        cx.notify();
    }

    /// 赞/踩 + 备注:语义(源 toggle)——点当前评分 = 删除(清反馈);
    /// 点另一评分 = put 新评分、带原备注前移;note 为空清备注留评分。
    pub fn rate_message(
        &mut self,
        message_id: &str,
        rating: &str,
        note: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.state.current_id.clone() else {
            return;
        };
        let host = self.bridge.host().clone();
        let existing = self.feedback.feedback_by_message.get(message_id).cloned();
        // 源 toggle:点当前评分(无 note 变更)= 删
        if let Some(it) = &existing
            && it.rating == rating
            && note.is_none()
        {
            let ver = it.version.clone();
            let _ = host.message_feedback_delete(&id, message_id, &ver);
            self.feedback.feedback_by_message.remove(message_id);
            cx.notify();
            return;
        }
        let if_version = existing.as_ref().map(|it| it.version.clone());
        let note = match note {
            Some(n) if !n.is_empty() => Some(n.to_string()),
            _ => existing.as_ref().and_then(|it| it.note.clone()),
        };
        if let Ok(item) = host.message_feedback_put(
            &id,
            message_id,
            rating,
            note.as_deref(),
            if_version.as_deref(),
        ) {
            self.feedback
                .feedback_by_message
                .insert(message_id.to_string(), item);
        }
        cx.notify();
    }

    /// 保存备注(评分不变或补评;空 note = 清备注留评分)
    pub fn save_feedback_note(&mut self, message_id: &str, note: &str, cx: &mut Context<Self>) {
        let existing = self.feedback.feedback_by_message.get(message_id).cloned();
        let rating = existing
            .as_ref()
            .map(|it| it.rating.clone())
            .unwrap_or_else(|| "positive".to_string());
        let note = note.trim();
        self.rate_message(
            message_id,
            &rating,
            if note.is_empty() { None } else { Some(note) },
            cx,
        );
    }

    /// 打开备注弹窗(初值 = 现有备注);懒建并聚焦可输入框
    pub fn open_feedback_note(
        &mut self,
        window: &mut Window,
        message_id: &str,
        anchor: gpui_kit::Point<gpui_kit::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.load_feedback(cx);
        let initial = self
            .feedback
            .feedback_by_message
            .get(message_id)
            .and_then(|it| it.note.clone())
            .unwrap_or_default();
        self.feedback.feedback_note_editor = Some((message_id.to_string(), initial.clone()));
        self.feedback.feedback_note_anchor = Some(anchor);
        self.ensure_feedback_input(window, cx);
        if let Some(input) = &self.feedback.feedback_input {
            input.update(cx, |s, cx| s.set_value(initial, window, cx));
            input.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    /// 更新弹窗草稿文本(备注输入框 Change 接线)
    pub fn set_feedback_note_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some((_, t)) = self.feedback.feedback_note_editor.as_mut() {
            *t = text.to_string();
        }
        cx.notify();
    }

    /// 懒建备注输入(需要 Window;render 期首现调用)。
    /// 订阅 Change → set_feedback_note_text 写回弹窗草稿。
    pub fn ensure_feedback_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.feedback.feedback_input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder("这条回答哪里好，或哪里有问题？（可选）")
        });
        cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                let value = input.read(cx).value().to_string();
                this.set_feedback_note_text(&value, cx);
            }
        })
        .detach();
        self.feedback.feedback_input = Some(input);
    }

    /// 关闭弹窗(不保存)
    pub fn close_feedback_note(&mut self, cx: &mut Context<Self>) {
        self.feedback.feedback_note_editor = None;
        self.feedback.feedback_note_anchor = None;
        cx.notify();
    }

    /// 保存备注(空 = 清备注留评分)并关弹窗
    pub fn commit_feedback_note(&mut self, cx: &mut Context<Self>) {
        if let Some((mid, text)) = self.feedback.feedback_note_editor.clone() {
            self.save_feedback_note(&mid, &text, cx);
            self.feedback.feedback_note_editor = None;
            self.feedback.feedback_note_anchor = None;
        }
        cx.notify();
    }
}
