//! 全库检索的 store 域:侧栏搜索输入(Enter 触发)、命中面板
//! 数据、跳转定位(先登记 seq,轨迹数据就绪后由 locate_search_hit
//! 收尾)。命中面板视图见 features::search::views。

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::{AppContext, Context, Entity, Window};

use crate::shell::panel::PanelTab;
use crate::shell::store::AppStore;

/// 全库检索功能切片状态(侧栏搜索输入/命中面板/跳转定位)。
#[derive(Default)]
pub(crate) struct SearchStore {
    /// 侧栏搜索输入态(挂窗后建;渲染时读值过滤;Enter = 全库检索)
    pub search_input: Option<Entity<InputState>>,
    /// 全库检索命中(Some = 结果面板在场,空 = 无命中)
    pub search_hits: Option<Vec<serde_json::Value>>,
    /// 检索跳转待定位 seq(切轨迹后按 seq 选台账行)
    pub search_locate: Option<(String, u64)>,
}

impl AppStore {
    /// 懒建侧栏搜索输入(Enter = 全库检索;挂窗态由 store::attach_window_state 调用)
    pub(crate) fn ensure_search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.search_input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("搜索会话"));
            cx.subscribe(&input, |this, _i, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.run_global_search(cx);
                }
            })
            .detach();
            self.search.search_input = Some(input);
        }
    }

    /// 全库检索:回车触发,异步 search_sessions → 命中面板
    pub fn run_global_search(&mut self, cx: &mut Context<Self>) {
        let Some(input) = &self.search.search_input else {
            return;
        };
        let query = input.read(cx).value().trim().to_string();
        if query.is_empty() {
            self.search.search_hits = None;
            cx.notify();
            return;
        }
        let host = self.bridge.host().clone();
        let q = query.clone();
        let rx = self
            .bridge
            .call(async move { host.search_sessions(&q, 30, None).await });
        let store = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            if let Ok(Ok(out)) = rx.await {
                store.update(cx, |s, cx| {
                    s.search.search_hits =
                        Some(out["hits"].as_array().cloned().unwrap_or_default());
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 检索命中点击:开会话 → 开轨迹面板标签 → 记录待定位 seq
    pub fn open_search_hit(&mut self, sid: &str, seq: u64, cx: &mut Context<Self>) {
        self.search.search_locate = Some((sid.to_string(), seq));
        self.open_session(sid, cx);
        self.open_panel_tab(PanelTab::Trajectory, cx);
    }

    /// 轨迹数据就绪后按 seq 定位台账行(检索跳转收尾;展开所属 turn)
    pub fn locate_search_hit(&mut self, cx: &mut Context<Self>) {
        let Some((sid, seq)) = self.search.search_locate.clone() else {
            return;
        };
        if self.state.current_id.as_deref() != Some(sid.as_str()) {
            return;
        }
        if let Some(rec) = self
            .trajectory
            .trajectory
            .records
            .iter()
            .find(|r| r.seq == seq)
        {
            let turn = rec.turn;
            if let Some(t) = turn {
                self.trajectory.collapsed_turns.remove(&t);
            }
            self.select_trajectory_record(rec.index, cx);
            self.search.search_locate = None;
        }
    }
}
