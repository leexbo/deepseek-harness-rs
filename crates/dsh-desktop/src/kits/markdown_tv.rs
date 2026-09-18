//! TextView 适配层(gpui-kit 富文本;流式 markdown 主路径)。
//!
//! 静态内容走 [`tv_static`](keyed state 惰性建、每帧 `set_text` 幂等,
//! ≤4KiB 同步解析首帧精确高);流式内容走 [`TvStreamRegistry`]:渲染
//! **前**的 flush 阶段(store.update 上下文)按节点 key diff——前缀
//! 匹配 `push_str` 尾增量(后台增量块解析,未闭合围栏不劈裂),否则
//! `set_text` 全量回退;渲染闭包只取挂载,无副作用。state 跨折叠
//! 重建按 key 存活;会话切换整体 `clear`。
//!
//! 历史坑背景(0.5.1 弃用理由,0.6.1 已带修复与上游回归):批量创建
//! 异步丢文本、measured 高度塌陷——本批在 layout_tests 复刻验证。

use std::collections::HashMap;

use gpui_kit::component::text::{TextView, TextViewState, TextViewStyle};
use gpui_kit::{
    App, AppContext as _, Entity, IntoElement, ParentElement as _, SharedString, StyleRefinement,
    Styled, div, px, relative,
};

/// 正文排版(照旧手调渲染器指标,双盘):
/// 正文 14px/1.75、标题 16/15/14、块距 8px、代码块 13px/黑底。
/// TextView 的正文字号/行高走继承,由外层包装承担;标题/间距/代码块
/// 经 [`TextViewStyle`] 调
fn styled_view(view: TextView) -> gpui_kit::AnyElement {
    div()
        .text_size(px(14.))
        .line_height(relative(1.75))
        // 滚动条槽预留(旧渲染器世界由列宽计算扣除,TextView 路径
        // 丢失 = 换行宽度吃满整行,右缘数个字符被裁);行尾不可断段
        // (行内代码 chip)的少量溢出由裁剪兜底
        .pr(px(crate::shell::metrics::SCROLLBAR_GUTTER_W))
        .overflow_hidden()
        .child(view)
        .into_any_element()
}

fn view_style() -> TextViewStyle {
    TextViewStyle {
        // 旧块距 mb(8)
        paragraph_gap: gpui_kit::rems(0.5),
        heading_base_font_size: px(14.),
        // 标题与正文同字号,仅粗细区分(多级字号混排显「大大小小」)
        heading_font_size: Some(std::sync::Arc::new(|_level: u8, base| base)),
        // 代码块字号 13;配色全部走 theme 派生默认(不另行改色)
        code_block: StyleRefinement::default().text_size(px(13.)),
        is_dark: crate::kits::theme::is_dark(),
        ..Default::default()
    }
}

/// 静态 markdown 挂载(keyed 便捷构造;id 需调用点稳定)
pub(crate) fn tv_static(id: impl Into<gpui_kit::ElementId>, text: &str) -> gpui_kit::AnyElement {
    styled_view(TextView::markdown(id, text).style(view_style()))
}

/// 驱动结果
pub(crate) enum DriveOutcome {
    /// 新建视图(>4KiB 历史的首轮解析是异步的,落地晚于挂载)
    Created(Entity<TextViewState>),
    /// 既有视图文本变化(push_str 增量或 set_text 回退)
    Updated,
    /// 无变化
    None,
}

/// 流式驱动注册表(挂 ChatStore;渲染前 flush 驱动,渲染闭包只读)
#[derive(Default)]
pub(crate) struct TvStreamRegistry {
    /// 节点 key → (state, 已同步文本记账;TextViewState 无 text
    /// getter,记账由本层维护)
    map: HashMap<String, (Entity<TextViewState>, String)>,
}

impl TvStreamRegistry {
    /// 渲染前 flush 驱动:前缀匹配 → push_str 增量;文本漂移(回退/
    /// 重写)→ set_text 全量;新 key → 建状态。幂等(无变化零开销)。
    /// 返回结果类别(新建视图须挂观察者:异步解析落地会改变高度,
    /// 外层虚拟化列表的行高缓存不会自愈)
    pub(crate) fn drive(&mut self, key: &str, text: &str, cx: &mut App) -> DriveOutcome {
        match self.map.get_mut(key) {
            Some((state, last)) => {
                if text.len() > last.len() && text.starts_with(last.as_str()) {
                    let delta = text[last.len()..].to_string();
                    state.update(cx, |s, cx| s.push_str(&delta, cx));
                    last.push_str(&delta);
                    return DriveOutcome::Updated;
                }
                if text != last {
                    state.update(cx, |s, cx| s.set_text(text, cx));
                    *last = text.to_string();
                    return DriveOutcome::Updated;
                }
                DriveOutcome::None
            }
            None => {
                let state = cx.new(|cx| TextViewState::markdown(text, cx));
                self.map
                    .insert(key.to_string(), (state.clone(), text.to_string()));
                DriveOutcome::Created(state)
            }
        }
    }

    /// 带组合的挂载(state 缺失 = flush 未及,兜底 keyed 静态;插件等
    /// TextView 级修饰经 compose,样式统一在适配层收口)
    pub(crate) fn view_composed(
        &self,
        key: &str,
        fallback_text: &str,
        compose: impl FnOnce(TextView) -> TextView,
    ) -> gpui_kit::AnyElement {
        let view = match self.map.get(key) {
            Some((state, _)) => compose(TextView::new(state)),
            None => compose(TextView::markdown(
                SharedString::from(key.to_string()),
                fallback_text,
            )),
        };
        styled_view(view.style(view_style()))
    }

    /// 会话切换清理(state 随旧会话焚毁,重开重解析一次)
    pub(crate) fn clear(&mut self) {
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::component::{Root, StyledExt as _};
    use gpui_kit::{
        InteractiveElement as _, IntoElement, ListAlignment, ListState, Render,
        StatefulInteractiveElement as _, Styled, TestAppContext, VisualTestContext, Window, div,
        px,
    };

    fn init(cx: &mut TestAppContext) {
        cx.update(|app| {
            gpui_kit::component::init(app);
            crate::kits::theme::init(app);
        });
    }

    /// 批量创建丢文本验证(0.5.1 历史坑 ①):200 条 keyed TextView 一次
    /// 全部在场,每条高度非零(丢文本 = 塌 0)。注意必须走 cx.refresh()
    /// 自然渲染——裸 window.draw 在 request_layout 期无 view 上下文,
    /// use_keyed_state 会 panic(既有记录)
    #[gpui_kit::test]
    fn tv_batch_creation_keeps_all_text(cx: &mut TestAppContext) {
        init(cx);
        struct Batch;
        impl Render for Batch {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut gpui_kit::Context<Self>,
            ) -> impl IntoElement {
                let items: Vec<_> = (0..200)
                    .map(|ix| {
                        div()
                            .id(gpui_kit::SharedString::from(format!("tvb-{ix}")))
                            .debug_selector(move || format!("tv-batch-{ix}"))
                            .w(px(400.))
                            .child(tv_static(
                                gpui_kit::SharedString::from(format!("tv-{ix}")),
                                &format!("第 {ix} 条:正文段落,包含 **加粗** 与 `code`。\n"),
                            ))
                    })
                    .collect();
                div()
                    .id("tv-batch-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .v_flex()
                    .children(items)
            }
        }
        let (_view, cx) = cx.add_window_view(|window, cx| {
            let v = cx.new(|_| Batch);
            Root::new(v, window, cx)
        });
        cx.refresh().expect("刷新失败");
        cx.run_until_parked();
        let mut zero = 0usize;
        for ix in 0..200 {
            let sel: &'static str = Box::leak(format!("tv-batch-{ix}").into_boxed_str());
            let h = cx
                .debug_bounds(sel)
                .map(|b| f32::from(b.size.height))
                .unwrap_or(0.);
            if h <= 0. {
                zero += 1;
            }
        }
        assert_eq!(zero, 0, "200 条批量创建不应有任何一条丢文本塌 0");
    }

    /// 高度塌陷/滚动抖动验证(0.5.1 历史坑 ②):虚拟化 list 里 40 条
    /// markdown,滚动前后内容总高稳定(delta < 2px;离屏块零高会令
    /// 总高缩水)。measure_all + 同步小替换是上游修复面
    #[gpui_kit::test]
    fn tv_list_total_height_stable_while_scrolling(cx: &mut TestAppContext) {
        init(cx);
        struct ListView {
            list: ListState,
        }
        impl Render for ListView {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut gpui_kit::Context<Self>,
            ) -> impl IntoElement {
                let state = self.list.clone();
                div().size_full().child(gpui_kit::list(state, |ix, _window, _cx| {
                    div()
                        .id(gpui_kit::SharedString::from(format!("tvl-{ix}")))
                        .debug_selector(move || format!("tv-list-{ix}"))
                        .w(px(400.))
                        .child(tv_static(
                            gpui_kit::SharedString::from(format!("l-{ix}")),
                            &format!(
                                "## 标题 {ix}\n\n段落一行,包含列表:\n\n- 项 A\n- 项 B\n\n```rust\nfn f{ix}() {{}}\n```\n"
                            ),
                        ))
                        .into_any_element()
                }))
            }
        }
        let list = ListState::new(40, ListAlignment::Top, px(600.));
        let list_in_view = list.clone();
        let (_root, cx) = cx.add_window_view(|window, cx| {
            let v = cx.new(move |_| ListView { list: list_in_view });
            Root::new(v, window, cx)
        });
        let content_h = |cx: &mut VisualTestContext| {
            cx.refresh().expect("刷新失败");
            cx.run_until_parked();
            list.max_offset_for_scrollbar().y
        };
        let h1 = f32::from(content_h(cx));
        assert!(h1 > 0., "初始应有内容高度");
        list.scroll_by(px(600.));
        let h2 = f32::from(content_h(cx));
        assert!(
            (h2 - h1).abs() < 2.,
            "滚动后内容总高应稳定(实测 {h1} → {h2})"
        );
    }

    /// 流式增量 == 一次全量(行为等价 + 不丢块):分 5 段 push_str 喂的
    /// TextView 与全量构造的 TextView 高度一致(±2px),且逐段单调
    #[gpui_kit::test]
    fn tv_stream_push_str_matches_full(cx: &mut TestAppContext) {
        init(cx);
        struct StreamView {
            reg: TvStreamRegistry,
        }
        impl Render for StreamView {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut gpui_kit::Context<Self>,
            ) -> impl IntoElement {
                div()
                    .id("tv-stream-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .v_flex()
                    .child(
                        div()
                            .id("tv-stream-inc")
                            .debug_selector(|| "tv-stream-inc".to_string())
                            .w(px(400.))
                            .child(self.reg.view_composed("k", "", |v| v)),
                    )
                    .child(
                        div()
                            .id("tv-stream-full")
                            .debug_selector(|| "tv-stream-full".to_string())
                            .w(px(400.))
                            .child(tv_static("full", Self::TARGET)),
                    )
            }
        }
        impl StreamView {
            const TARGET: &'static str =
                "开篇段落。\n\n- 列表项一\n- 列表项二\n\n```rust\nfn a() {}\n```\n\n收尾段落。\n";
        }
        let inner = std::rc::Rc::new(std::cell::RefCell::new(
            None::<gpui_kit::Entity<StreamView>>,
        ));
        let cell = inner.clone();
        let (_root, cx) = cx.add_window_view(|window, cx| {
            let v = cx.new(|_| StreamView {
                reg: TvStreamRegistry::default(),
            });
            *cell.borrow_mut() = Some(v.clone());
            Root::new(v, window, cx)
        });
        let view = inner.borrow().clone().expect("内层实体应在场");
        let target = StreamView::TARGET;
        // 切点取字符边界(多字节文本不能按裸字节切)
        let boundaries: Vec<usize> = target
            .char_indices()
            .map(|(i, _)| i)
            .chain([target.len()])
            .collect();
        let pick = |want: usize| *boundaries.iter().find(|b| **b >= want).expect("边界存在");
        let mut prev_h = 0.0f32;
        for want in [8usize, 20, 45, 70, target.len()] {
            let part = target[..pick(want)].to_string();
            view.update(cx, |v, cx| v.reg.drive("k", &part, cx));
            cx.refresh().expect("刷新失败");
            cx.run_until_parked();
            let h = cx
                .debug_bounds("tv-stream-inc")
                .map(|b| f32::from(b.size.height))
                .unwrap_or(0.);
            assert!(h >= prev_h, "流式高度应单调不减({prev_h} → {h})");
            prev_h = h;
        }
        cx.refresh().expect("刷新失败");
        cx.run_until_parked();
        let full_h = f32::from(
            cx.debug_bounds("tv-stream-full")
                .map(|b| b.size.height)
                .unwrap_or(px(0.)),
        );
        assert!(
            (prev_h - full_h).abs() < 2.,
            "增量喂成高度应与全量一致(增量 {prev_h} vs 全量 {full_h})"
        );
    }
}
