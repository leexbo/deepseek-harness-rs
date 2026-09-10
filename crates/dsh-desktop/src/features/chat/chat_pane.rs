//! 消息滚动区(web `ChatView`):统一列宽居中(见 shell::metrics)。
//! gpui 内建 [`gpui_kit::list`] 虚拟化:逐项测高缓存(变高 markdown/卡片
//! 原生支持)、Bottom 对齐(聊天自底语义,logical 为 None 即钉底跟随)、
//! overdraw 预渲染缓冲;节点数变化经 store 的 splice 增量通知。
//! 节点渲染:用户气泡/助手正文(流式 markdown)/工具行(点击展开,
//! 渲染意图卡路由)/Think 行(点击展开)/回合收尾。

use gpui_kit::component::IconName;
use gpui_kit::component::StyledExt;
use gpui_kit::component::WindowExt;
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, App, Div, Entity, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};

use super::projection::{
    ChatNode, NavAnchor, PlanStatus, RetryState, RowSlot, ToolState, nav_anchors,
};
use crate::kits::icons::{self, DshIcon, fixed};
use crate::kits::theme;
use crate::shell::metrics::{H_PAD, NAV_GUTTER_W, RUN_CLOCK_AFTER_SECS, SCROLLBAR_GUTTER_W};
use std::sync::Arc;

use crate::shell::store::AppStore;

/// 消息区整体(相对容器 + 虚拟化列 + 回底钮)
pub fn render(store: &Entity<AppStore>, window: &mut Window, cx: &mut App) -> impl IntoElement {
    // 列宽先算(短借用;长借用与 window 互不冲突)
    let sidebar_collapsed = store.read(cx).sidebar_collapsed;
    let sidebar_px = store.read(cx).sidebar_px;
    let (panel_open, panel_px) = (store.read(cx).panel_open, store.read(cx).panel_px);
    // 内容列绝对像素宽:taffy 的文本测量只在「祖先有绝对 style 宽」
    // 时按宽 wrap(百分比/auto 全链无锚点 → MaxContent 单行测量 →
    // 盒高按单行、内容溢出与相邻消息重叠)。宽经 metrics 策略由
    // viewport−侧栏确定性推出(与 composer 同式同宽;见 shell::metrics)。
    let col_w = crate::shell::metrics::window_chat_col_w(
        window,
        sidebar_collapsed,
        sidebar_px,
        panel_open,
        panel_px,
    );

    let list_state = store.read(cx).chat.chat_list.clone();

    // 导航轨(刻度列 + 正常滚动条)派生:锚点 = 用户消息
    // (轮次开始);显示条件 = 有锚点且内容明显可滚(> 1/4 视口——刚溢出
    // 几行的短会话不值得一条轨;viewport 未布局时为 0,守卫跳过)。当前
    // 位置 = 视口顶最近用户锚(白色刻度标记读到哪一轮;跨轮才移动)
    let (nav_anchors_vec, show_nav_rail, nav_current_key) = {
        let st = store.read(cx);
        let anchors = nav_anchors(&st.chat.row_slots, st.current_nodes());
        let vp_h = f32::from(list_state.viewport_bounds().size.height);
        let scrollable = f32::from(list_state.max_offset_for_scrollbar().y);
        let show = !anchors.is_empty() && vp_h > 0. && scrollable > vp_h * 0.25;
        let top_ix = list_state.logical_scroll_top().item_ix;
        let max_slot = st.chat.row_slots.len().saturating_sub(1);
        let current = anchors
            .iter()
            .rev()
            .find(|a| a.slot_ix <= top_ix.min(max_slot))
            .map(|a| a.key.clone());
        (anchors, show, current)
    };
    // 轨道几何(paint 捕获),供刻度带等距居中
    let nav_track = store.read(cx).chat.nav_track;

    // 转写区进行中指示:running 恒显示
    // 「深入探索中…」+ shimmer 呼吸,时长时钟仅 ≥15s 出现(<15s 短回合
    // 只出纯标签)。
    let run_status = {
        let st = store.read(cx);
        st.state
            .current_id
            .clone()
            .and_then(|id| st.is_running(&id).then_some(id))
            .map(|id| {
                let dur = st
                    .run_elapsed(&id)
                    .filter(|d| *d >= std::time::Duration::from_secs(RUN_CLOCK_AFTER_SECS))
                    .map(crate::shell::reducer::format_run_duration);
                (dur, st.state.current_id.clone())
            })
    };

    // 逐项闭包持 store 实体:虚拟化下只有可视(+overdraw)项被
    // 渲染,每项单次 read 借用(与旧全量 to_vec 相比,流式重绘成本
    // 恒定于可视项数)。列 gap(16)由每项包裹容器 py(8) 承担。
    // 行源 = 行槽(Node 平铺 / 轮过程组行),非 nodes 原始序
    let item_store = store.clone();
    let list = gpui_kit::list(list_state, move |ix, _window, cx| {
        let st = item_store.read(cx);
        // 流尾伪行:插队待投递气泡(session/queue 权威快照;行号 = 行槽之后)
        let Some(slot) = st.chat.row_slots.get(ix) else {
            let off = ix - st.chat.row_slots.len();
            let Some(id) = st.state.current_id.as_deref() else {
                return div().into_any_element();
            };
            let Some(chat) = st.state.chats.get(id) else {
                return div().into_any_element();
            };
            let entries: Vec<&crate::features::chat::QueueEntry> = chat
                .queue
                .iter()
                .filter(|e| e.placement == crate::features::chat::QueuePlacement::Steering)
                .collect();
            let Some(entry) = entries.get(off) else {
                return div().into_any_element();
            };
            return pending_steering_bubble(entry, col_w).into_any_element();
        };
        // test 钩子 selector 沿用节点原始序号(组行另行标注):布局
        // 回归测试按 node-{n} 检索,折叠收拢的节点以缺席跳过
        // (release 下 node_ix 无消费者)
        #[cfg_attr(not(test), allow(unused_variables))]
        let (el, node_ix) = match slot {
            RowSlot::Node(n) => {
                let Some(node) = st.current_nodes().get(*n) else {
                    return div().into_any_element();
                };
                // 零高节点不占行距:统一 py(8) 会留下 16px 空隙,行间
                // 疏密不均(实测展开组 16/32px 两档交替)
                if crate::features::chat::projection::invisible_node(node) {
                    return div().into_any_element();
                }
                let el = render_node(
                    &item_store,
                    cx,
                    &st.chat.open_reasoning,
                    &st.chat.open_context,
                    &st.chat.expanded_tools,
                    &st.chat.open_retries,
                    *n,
                    node,
                    col_w,
                )
                .into_any_element();
                (el, Some(*n))
            }
            // 展开态组成员:缩进 + 左侧引导线(层级包裹感,防展开迷失)
            RowSlot::GroupMember(n) => {
                let Some(node) = st.current_nodes().get(*n) else {
                    return div().into_any_element();
                };
                // 零高成员(空正文+无思考的定稿 Assistant,纯 tool_calls
                // 步的占位)不渲染引导线段:整行不可见
                if crate::features::chat::projection::invisible_node(node) {
                    return div().into_any_element();
                }
                let inner = render_node(
                    &item_store,
                    cx,
                    &st.chat.open_reasoning,
                    &st.chat.open_context,
                    &st.chat.expanded_tools,
                    &st.chat.open_retries,
                    *n,
                    node,
                    col_w,
                )
                .into_any_element();
                let el = div()
                    .relative()
                    .pl(px(18.))
                    .child(
                        // 引导线段:每行画自己的一段,视觉连成贯穿竖线
                        div()
                            .debug_selector(|| "group-rail".to_string())
                            .absolute()
                            .left(px(5.))
                            .top_0()
                            .bottom_0()
                            .w(px(2.))
                            .rounded(px(1.))
                            .bg(theme::BORDER()),
                    )
                    .child(inner)
                    .into_any_element();
                (el, Some(*n))
            }
            RowSlot::Group {
                turn_key,
                first,
                last,
            }
            | RowSlot::GroupOpen {
                turn_key,
                first,
                last,
            } => {
                let open = matches!(slot, RowSlot::GroupOpen { .. });
                let el = turn_group_row(&item_store, cx, turn_key, *first, *last, open)
                    .into_any_element();
                (el, None)
            }
        };
        let anim_key = slot_key_for_anim(slot, st.current_nodes());
        let el = enter_anim(el, &anim_key, st.current_chat());
        // 布局回归测试钩子:节点 bounds 可经 debug_bounds 检索(release 无操作)
        #[cfg(test)]
        let el = {
            use gpui_kit::InteractiveElement as _;
            let sel = node_ix
                .map(|n| format!("node-{n}"))
                .unwrap_or_else(|| format!("row-{ix}"));
            div()
                .py(px(8.))
                .debug_selector(move || sel)
                .child(el)
                .into_any_element()
        };
        #[cfg(not(test))]
        let el = div().py(px(8.)).child(el).into_any_element();
        // 列体本身拉满整行承接滚轮(中栏两侧空白也要能滚——此前列表只有
        // col_w 宽,滚轮命中区随之变窄);行内容限宽居中,**包在最外层**,
        // 上面的 selector bounds 保持与中栏一致(布局测试按它断言缩进)。
        // 包裹层必须 w_full:taffy 里 auto 宽度收缩到内容宽,justify_center
        // 就没有自由空间可分配(居中失效 = 全部贴左)
        div()
            .w_full()
            .flex()
            .justify_center()
            .child(div().w(col_w).child(el))
            .into_any_element()
    });

    div()
        .relative()
        // 列向:滚动容器经 main-axis(flex_1+min_h0)收缩到可视高
        .v_flex()
        .min_h(px(0.))
        .flex_1()
        .child(
            div()
                .flex()
                .min_w(px(0.))
                .min_h(px(0.))
                .flex_1()
                // 左锚点槽 + 右滚动条槽(与 metrics::chat_col_w 的扣减同
                // 源):窄窗下列吃满可用宽时,刻度/滚动条 thumb 不得叠上
                // 文字。所有列对齐
                // 容器(turn_status/底部栈/hero)同款 padding 保中心线
                .pl(px(H_PAD + NAV_GUTTER_W))
                .pr(px(H_PAD + SCROLLBAR_GUTTER_W))
                // 列表满宽:滚轮命中区 = 整个消息区(行级居中由 item
                // 包裹层承担,见上方 justify_center)
                .child(list.h_full().w_full().py(px(8.))),
        )
        .when_some(run_status, |el, (dur, _sid)| {
            el.child(
                div()
                    // 列对齐容器同款槽 padding(与列表容器同中心线)
                    .pl(px(H_PAD + NAV_GUTTER_W))
                    .pr(px(H_PAD + SCROLLBAR_GUTTER_W))
                    .child(
                        div()
                            .mx_auto()
                            .w(col_w)
                            .px(px(4.))
                            .pb(px(8.))
                            .text_size(px(13.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(theme::BRAND())
                            .child(
                                div()
                                    .debug_selector(|| "turn-status".to_string())
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .child("深入探索中…")
                                    .when_some(dur, |el, label| {
                                        el.child(
                                            div()
                                                .text_size(px(12.))
                                                .font_weight(gpui_kit::FontWeight::NORMAL)
                                                .text_color(theme::CAPTION())
                                                .child(label),
                                        )
                                    })
                                    // 进行中 shimmer(1.8s 透明度呼吸;running
                                    // 消失即元素卸载,动画随停)
                                    .with_animation(
                                        "dsh-turn-status",
                                        Animation::new(std::time::Duration::from_millis(1800))
                                            .repeat()
                                            .with_easing(gpui_kit::pulsating_between(0.45, 0.95)),
                                        |el, delta| el.opacity(delta),
                                    ),
                            ),
                    ),
            )
        })
        .when(show_nav_rail, |el| {
            let s = store.read(cx);
            let ui = NavUiState {
                current_key: nav_current_key.clone(),
                hovered: s.chat.nav_hover,
            };
            // 刻度列挂面板左缘;滚动条用组件库默认件,挂内容列(shell/mod.rs)
            el.child(nav_ticks(store, &nav_anchors_vec, nav_track, ui))
        })
}

/// 新节点入场窗口(140ms 淡入 + 6px 上移)
const NODE_ENTER_MS: std::time::Duration = std::time::Duration::from_millis(140);

/// 新节点入场(桌面增量):140ms 淡入 + 轻微上移。**年龄门控**
/// ——出生超窗直接原样返回:gpui list 虚拟化会把滚出 overdraw 的项重挂,
/// 无门控则每次滚回都重放入场。`relative().top()` 视觉位移不改布局
/// 高度(list 测高稳定);动画完成态 = 原样,超窗摘除 wrapper 无缝。
/// with_animation 内建尊重系统 reduce-motion。历史载入(merge_history)
/// 不记 born,整段会话重放不触发。
fn enter_anim(
    el: gpui_kit::AnyElement,
    key: &str,
    chat: Option<&super::projection::ChatState>,
) -> gpui_kit::AnyElement {
    let Some(born) = chat.and_then(|c| c.node_born.get(key)) else {
        return el;
    };
    if born.elapsed() >= NODE_ENTER_MS {
        return el;
    }
    let sel = format!("node-enter-{key}");
    div()
        .debug_selector(move || sel.clone())
        .relative()
        .child(el)
        .with_animation(
            gpui_kit::SharedString::from(format!("node-enter-{key}")),
            Animation::new(NODE_ENTER_MS)
                .with_easing(gpui_kit::component::animation::ease_out_cubic),
            |wrapper, delta| {
                wrapper
                    .opacity(0.2 + 0.8 * delta)
                    .top(px(-6.0 * (1.0 - delta)))
            },
        )
        .into_any_element()
}

/// 工具行扫光周期(2.6s)
const TOOL_SWEEP_MS: std::time::Duration = std::time::Duration::from_millis(2600);

/// 工具行运行中扫光(2.6s/轮、90% 后停右的呼吸节拍;
/// 与前导 ongoing_dot 共存 = 「状态点 + 扫光」双要素)。gpui 渐变仅
/// 两止点,以「透明→低透明白」近似 300px 三止带;`left(relative(..))`
/// 相对行宽位移无需知道行宽;repeat 动画元素卸载即停(同 state_dot)。
fn tool_sweep(ix: usize) -> impl IntoElement {
    div()
        .debug_selector(move || format!("tool-sweep-{ix}"))
        .absolute()
        .top_0()
        .bottom_0()
        .w(px(110.))
        .bg(gpui_kit::linear_gradient(
            90.,
            gpui_kit::linear_color_stop(gpui_kit::transparent_black(), 0.),
            gpui_kit::linear_color_stop(theme::SWEEP(), 1.),
        ))
        .with_animation(
            ("dsh-tool-sweep", ix),
            Animation::new(TOOL_SWEEP_MS).repeat(),
            |band, delta| {
                // 0..90% 行程(二次缓出),90%..100% 停右留白
                let f = (delta / 0.9).min(1.0);
                let f = 1.0 - (1.0 - f) * (1.0 - f);
                band.left(gpui_kit::relative(f * 1.3 - 0.2))
            },
        )
}

/// 刻度长度表(hover 渐变,逐像素实测:激活 26,邻线随距离
/// 递减 20/14/10,距离 ≥4 及常态一律 6;左对齐、当前轮只变白不改长)
const TICK_WIDTHS: [f32; 5] = [26., 20., 14., 10., 6.];

/// 导航轨 UI 态(渲染期从 ListState 读好传入,轨内不再取 cx)
struct NavUiState {
    current_key: Option<String>,
    hovered: Option<usize>,
}

/// 消息锚点刻度列(**窗口左缘**,定稿形态:「短横线是左侧集中
/// 居中密集状态」「激活的时候线变长,周围的线也渐变长」+ 截图逐像素
/// 实测):一条用户消息一根短横线,**全部左对齐**(线左缘距面板左 24),
/// **固定间距 10px 密排成带、整带纵向居中**(63 刻度实测 pitch 恒 10、
/// 带中心 = 视口中心)。常态 6×2 灰(白 22%);**当前轮变白不改长**;
/// hover 激活:线变白加长(26)且邻线按距离渐变长([20,14,10],≥4 回
/// 常态,见 TICK_WIDTHS),浮出多行摘要卡(标题粗体白 + 正文预览,
/// 卡挂刻度右侧、垂直居中对准激活线)。点刻度跳轮次。容器不做鼠标
/// 阻挡(卡要伸出容器右缘,挡了会吃掉消息区点击);点击阻挡下沉到
/// 刻度行与卡本体。
fn nav_ticks(
    store: &Entity<AppStore>,
    anchors: &[NavAnchor],
    track: Option<(f32, f32)>,
    ui: NavUiState,
) -> impl IntoElement {
    let NavUiState {
        current_key,
        hovered,
        ..
    } = ui;
    let current_key = current_key.as_deref();
    let (_, track_h) = match track {
        Some((top, bottom)) => (top, (bottom - top).max(0.)),
        None => (0., 0.),
    };
    // 固定间距 10px 密排,整带纵向居中;锚多到放不下时间距压缩(上下
    // 留 16 边距)
    let n = anchors.len();
    let pitch = if n > 1 {
        ((track_h - 32.) / (n - 1) as f32).min(10.)
    } else {
        10.
    };
    let band_top = (track_h - (n.max(1) - 1) as f32 * pitch) / 2.;
    // hover 激活的刻度序号(锚点列表下标),渐变宽度按距离取
    let hovered_ix = hovered.and_then(|slot| anchors.iter().position(|a| a.slot_ix == slot));
    let tick = |i: usize, a: &NavAnchor, y_track: f32| {
        let slot = a.slot_ix;
        let sc = store.clone();
        let hh = store.clone();
        let sel = format!("nav-point-{slot}");
        let line_sel = format!("nav-line-{slot}");
        let dist = hovered_ix
            .map(|h| (h as i32 - i as i32).unsigned_abs() as usize)
            .unwrap_or(usize::MAX);
        let w = TICK_WIDTHS[dist.min(4)];
        let is_active = dist == 0;
        let is_current = current_key == Some(a.key.as_str());
        let color = if is_active || is_current {
            theme::LABEL()
        } else {
            theme::TICK_IDLE()
        };
        // 刻度行:热区高 16,线左缘 = pl(24)(实测 23.5);行本体挡点击
        let mut el = div()
            .debug_selector(move || sel.clone())
            .id(gpui_kit::ElementId::Name(SharedString::from(format!(
                "nav-point-{slot}"
            ))))
            .absolute()
            .left_0()
            .w(px(56.))
            .h(px(16.))
            .top(px(y_track - 8.))
            .flex()
            .items_center()
            .pl(px(24.))
            .cursor_pointer()
            .block_mouse_except_scroll()
            .on_hover(move |entered, _, cx| {
                if *entered {
                    hh.update(cx, |st, cx| st.set_nav_hover(Some(slot), cx));
                } else if hh.read(cx).chat.nav_hover == Some(slot) {
                    // 移出即清(带守卫:先进入邻行/卡时不清,deferred 按
                    // 绘制序执行,enter 在后则守卫挡掉旧行的 exit)
                    hh.update(cx, |st, cx| st.set_nav_hover(None, cx));
                }
            })
            .on_click(move |_, _, cx| {
                sc.update(cx, |st, cx| st.jump_to_nav(slot, cx));
            })
            .child(
                div()
                    .debug_selector(move || line_sel.clone())
                    .flex_shrink_0()
                    .w(px(w))
                    .h(px(2.))
                    .rounded(px(1.))
                    .bg(color),
            );
        if is_active {
            let title = a.title.clone();
            let body = a.preview.clone();
            // 摘要卡:挂靠刻度行,伸到刻度列右侧(卡左缘 ~60,垂直居中
            // 对准激活线;单行消息卡矮一半,top 相应减半)。贴轨顶的首
            // 刻度卡下移让位,不越窗顶(轨顶距窗顶仅 12px)
            let has_body = !body.is_empty();
            let card_top = if y_track < 60. {
                8.
            } else if has_body {
                -50.
            } else {
                -26.
            };
            // 卡自带 hover 双向:行退出先清、卡进入再置回(deferred 按
            // 绘制序,卡是行后代后画,指针从行移入卡不闪卡)
            let cc = store.clone();
            let mut card = div()
                .debug_selector(move || format!("nav-card-{slot}"))
                .id(gpui_kit::ElementId::Name(SharedString::from(format!(
                    "nav-card-{slot}"
                ))))
                .absolute()
                .left(px(60.))
                .top(px(card_top))
                .w(px(320.))
                .p(px(16.))
                .rounded(px(12.))
                .bg(theme::DOCK())
                .shadow_md()
                .block_mouse_except_scroll()
                .on_hover(move |entered, _, cx| {
                    if *entered {
                        cc.update(cx, |st, cx| st.set_nav_hover(Some(slot), cx));
                    } else if cc.read(cx).chat.nav_hover == Some(slot) {
                        cc.update(cx, |st, cx| st.set_nav_hover(None, cx));
                    }
                })
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(theme::LABEL())
                        .truncate()
                        .child(title),
                );
            if has_body {
                card = card.child(
                    div()
                        .mt(px(6.))
                        .max_h(px(63.))
                        .overflow_hidden()
                        .text_size(px(13.))
                        .text_color(theme::LABEL_2())
                        .line_height(gpui_kit::relative(1.5))
                        .child(body),
                );
            }
            el = el.child(card);
        }
        el
    };
    div()
        .debug_selector(|| "nav-rail".to_string())
        .id("nav-rail")
        .absolute()
        .left_0()
        .top(px(12.))
        .bottom(px(64.))
        // 容器宽到能罩住摘要卡;不做鼠标阻挡、不挂 hover(行与卡自带
        // 双向 hover——行带 BlockMouseExceptScroll 后,hit_test 的 hover
        // 计数在行处冻结,容器永远轮不到,挂了也是死代码)
        .w(px(384.))
        // 列表区 span 捕获(刻度带居中用;滚动条全高后的 span 另存
        // nav_scroll_track,两套几何互不干扰)
        .child(div().absolute().inset_0().child({
            let cap = store.clone();
            gpui_kit::canvas(
                move |b, _, cx| {
                    let top = b.origin.y.as_f32();
                    cap.update(cx, |st, cx| {
                        let bottom = st.chat.nav_track.map(|(_, b)| b).unwrap_or(top);
                        st.note_nav_track(top, bottom, cx);
                    });
                },
                |_, _, _, _| {},
            )
        }))
        .child(div().absolute().left_0().right_0().bottom_0().child({
            let cap = store.clone();
            gpui_kit::canvas(
                move |b, _, cx| {
                    let bottom = b.origin.y.as_f32();
                    cap.update(cx, |st, cx| {
                        let top = st.chat.nav_track.map(|(t, _)| t).unwrap_or(bottom);
                        st.note_nav_track(top, bottom, cx);
                    });
                },
                |_, _, _, _| {},
            )
        }))
        .children(
            anchors
                .iter()
                .enumerate()
                .map(|(i, a)| tick(i, a, band_top + i as f32 * pitch)),
        )
}

/// 注入行的展示头:从 `source` 派生 role 与 label。
/// role = recall(source.kind=session-reference)| inject(其余);label = 可读来源名
/// (references 会话名 / plugin 插件名 / instructions 路径 / kind 兜底)。
pub fn context_provenance(source: &serde_json::Value) -> (&'static str, String) {
    let kind = source["kind"].as_str().unwrap_or("context");
    if kind == "session-reference" {
        // 召回:join 被引会话 label;缺省回退 kind
        let labels: Vec<String> = source["references"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| r["label"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let label = if labels.is_empty() {
            "session-reference".to_string()
        } else {
            labels.join(", ")
        };
        ("recall", label)
    } else if kind == "plugin" {
        // 插件来源:label 读 source.plugin(如 @deepseek-ai/dsh-system-prompt)
        let label = source["plugin"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| "plugin".to_string());
        ("inject", label)
    } else if kind == "agent-instructions" {
        // 工作区指令:label = changes[].path 去重连接(文件清单
        // 语义),其次 paths,再 path 兜底
        let mut labels: Vec<String> = Vec::new();
        for item in source["changes"].as_array().into_iter().flatten() {
            if let Some(p) = item["path"].as_str()
                && !labels.iter().any(|l| l == p)
            {
                labels.push(p.to_string());
            }
        }
        if labels.is_empty() {
            labels = source["paths"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|p| p.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
        }
        let label = if labels.is_empty() {
            source["path"].as_str().unwrap_or(kind).to_string()
        } else {
            labels.join(", ")
        };
        ("inject", label)
    } else {
        let label = source["path"]
            .as_str()
            .or_else(|| source["name"].as_str())
            .map(String::from)
            .unwrap_or_else(|| kind.to_string());
        ("inject", label)
    }
}

/// 上下文注入行:折叠头为图标+标题(注入·来源)+摘要;
/// 点击展开主体(模型可见文本)。recall → 会话图标,其余 → 文件图标。
/// source.kind=subagent-settled 分流为独立通知卡(通知形态)。
fn context_block(
    store: &Entity<AppStore>,
    _cx: &App,
    open_context: &std::collections::HashSet<String>,
    ix: usize,
    key: &str,
    content: &str,
    source: &serde_json::Value,
) -> impl IntoElement {
    if matches!(
        source["kind"].as_str(),
        Some("subagent-settled") | Some("subagent-message")
    ) {
        return notice_card(store, open_context, ix, key, content, source).into_any_element();
    }
    let open = open_context.contains(key);
    let s = store.clone();
    let key = key.to_string();
    let click_key = key.clone();
    let (role, label) = context_provenance(source);
    let title = if role == "recall" {
        format!("召回·{label}")
    } else {
        format!("注入·{label}")
    };
    let icon = if role == "recall" {
        fixed(DshIcon::MessageSquare, 14.).into_any_element()
    } else {
        fixed(IconName::File, 14.).into_any_element()
    };
    // 折叠摘要 = 注入文本首行(截断);与 Think 行同构
    let summary = summary_line(content);
    let content_owned = content.to_string();
    div()
        .id(("context", ix))
        .v_flex()
        .rounded(px(8.))
        .bg(theme::LAYER())
        .px(px(10.))
        .cursor_pointer()
        .when(open, |el| el.py(px(8.)))
        .when(!open, |el| el.py(px(6.)))
        .child(collapse_row_header(
            icon,
            &title,
            (!open).then_some(summary),
            open,
        ))
        .when(open, |el| {
            el.child(
                div()
                    .mt(px(4.))
                    .text_size(px(13.))
                    .text_color(theme::LABEL_3())
                    .line_height(gpui_kit::relative(1.5))
                    .whitespace_normal()
                    .child(content_owned),
            )
        })
        .on_click(move |_, _, cx| {
            let key = click_key.clone();
            s.update(cx, |st, cx| st.toggle_context(&key, cx));
        })
        .into_any_element()
}

/// 子代理通知卡:Bot 图标+状态标题+折叠摘要
/// (closing 首行)+展开正文与「查看子会话」跳转(senderSessionId → open_session,
/// 血缘会话不经侧栏)。
/// 形态:kind=subagent-settled(已完成/已停止/已失败/已恢复,状态标题自结算摘要
/// 动词派生)/ kind=subagent-message(子代理·消息,正文=消息本体)。
fn notice_card(
    store: &Entity<AppStore>,
    open_context: &std::collections::HashSet<String>,
    ix: usize,
    key: &str,
    content: &str,
    source: &serde_json::Value,
) -> impl IntoElement {
    let open = open_context.contains(key);
    let s = store.clone();
    let click_key = key.to_string();
    let child_id = source["senderSessionId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let kind = source["kind"].as_str().unwrap_or("subagent-settled");
    // 状态标签从结算摘要派生(settlementSummary 变体的动词)
    let summary = source["summary"].as_str().unwrap_or_default();
    let (status, closing) = if kind == "subagent-message" {
        // 回发消息:正文 = 前缀行之后的消息本体
        (
            "子代理·消息",
            content
                .split_once(":\n\n")
                .map(|(_, rest)| rest.trim().to_string()),
        )
    } else if summary.contains("was stopped") {
        ("子代理·已停止", closing_of_settlement(content))
    } else if summary.contains("was interrupted") {
        ("子代理·已恢复", closing_of_settlement(content))
    } else if summary.contains("failed")
        || summary.contains("declined")
        || summary.contains("ended abnormally")
    {
        ("子代理·已失败", closing_of_settlement(content))
    } else {
        ("子代理·已完成", closing_of_settlement(content))
    };
    let folded_summary = closing
        .as_ref()
        .map(|c| summary_line(c))
        .unwrap_or_else(|| "无收尾消息".to_string());
    let jump = child_id.clone();
    div()
        .id(("notice", ix))
        .v_flex()
        .rounded(px(8.))
        .bg(theme::LAYER())
        .px(px(10.))
        .cursor_pointer()
        .when(open, |el| el.py(px(8.)))
        .when(!open, |el| el.py(px(6.)))
        .child(collapse_row_header(
            fixed(IconName::Bot, 14.).into_any_element(),
            status,
            (!open).then_some(folded_summary),
            open,
        ))
        .when(open, |el| {
            let body = el.child(
                div()
                    .mt(px(4.))
                    .text_size(px(13.))
                    .text_color(theme::LABEL_3())
                    .line_height(gpui_kit::relative(1.5))
                    .whitespace_normal()
                    .child(closing.unwrap_or_else(|| "无收尾消息".into())),
            );
            if child_id.is_empty() {
                return body;
            }
            let s = s.clone();
            body.child(
                div()
                    .id(("notice-jump", ix))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .mt(px(6.))
                    .text_size(px(12.))
                    .text_color(theme::BRAND())
                    .cursor_pointer()
                    // 嵌套点击:跳转不触发卡片折叠切换
                    .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation()
                    })
                    .on_click(move |_, _, cx| {
                        s.update(cx, |st, cx| st.open_session(&jump, cx));
                    })
                    .child("查看子会话")
                    .child(fixed(IconName::ArrowRight, 12.)),
            )
        })
        .on_click(move |_, _, cx| {
            let key = click_key.clone();
            s.update(cx, |st, cx| st.toggle_context(&key, cx));
        })
}

/// 结算通知的 closing message(固定分节之后;无收尾 → None)
fn closing_of_settlement(content: &str) -> Option<String> {
    content
        .split_once("Its closing message:\n\n")
        .map(|(_, rest)| rest.trim().to_string())
        .filter(|c| !c.is_empty())
}

/// 折叠行公共头行(Think / 注入行族同构骨架,原为两处逐字复制):
/// 图标 + 标题 + 折叠摘要(仅折叠态,truncate + flex-1)/ 展开弹性
/// 占位 + 展开箭头。容器(底色/内边距/点击区)归各块自有;工具行
/// (摘要常显 13px)与计划卡(徽标头)形态不同,不入此族
fn collapse_row_header(icon: AnyElement, title: &str, summary: Option<String>, open: bool) -> Div {
    div()
        .flex()
        .min_w(px(0.))
        .items_center()
        .gap(px(4.))
        .text_size(px(12.))
        .text_color(theme::CAPTION())
        .child(icon)
        .child(title.to_string())
        .when(open, |el| el.child(div().flex_1()))
        .when(!open, |el| {
            el.child(
                div()
                    .min_w(px(0.))
                    .flex_1()
                    .truncate()
                    .child(summary.unwrap_or_default()),
            )
        })
        .child(fixed(
            if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            },
            14.,
        ))
}

/// 注入文本折叠摘要(取首行截断到 ~160 字符;内容超长时截断)
fn summary_line(text: &str) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let first = first.trim();
    if first.chars().count() > 160 {
        let cut: String = first.chars().take(160).collect();
        format!("{cut}…")
    } else {
        first.to_string()
    }
}

/// 行槽 → 入场动画门控 key:Node/GroupMember 沿用节点 key(查 born 表);
/// 组行无 born 记录(折叠不产生新节点)→ 动画自然跳过
fn slot_key_for_anim(slot: &RowSlot, nodes: &[ChatNode]) -> String {
    match slot {
        RowSlot::Node(n) | RowSlot::GroupMember(n) => nodes
            .get(*n)
            .map(|nd| nd.key().to_string())
            .unwrap_or_default(),
        RowSlot::Group { turn_key, .. } | RowSlot::GroupOpen { turn_key, .. } => turn_key.clone(),
    }
}

/// 轮过程组摘要行(**节标题**样式,刻意与成员卡片区分层级):
/// 无底色、更矮(h28)、Workflow 图标 +「思考与工具 · N 步 · M 个调用」
/// (M=0 省略)+ 展开箭头;成员展开后缩进 + 左引导线归属其下。
/// 点击展开/收拢该轮(行数回缩走 store 侧锚定 reset)
fn turn_group_row(
    store: &Entity<AppStore>,
    cx: &App,
    turn_key: &str,
    first: usize,
    last: usize,
    open: bool,
) -> impl IntoElement {
    let (steps, tools) =
        super::projection::group_counts(store.read(cx).current_nodes(), first, last);
    let mut label = format!("思考与工具 · {steps} 步");
    if tools > 0 {
        label.push_str(&format!(" · {tools} 个调用"));
    }
    let s = store.clone();
    let key = turn_key.to_string();
    let sel = format!("turn-group-{turn_key}");
    div()
        .id(("turn-group", first))
        .flex()
        .min_h(px(28.))
        .flex_shrink_0()
        .items_center()
        .gap(px(6.))
        .rounded(px(6.))
        .px(px(4.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::LAYER()))
        .text_size(px(12.))
        .debug_selector(move || sel.clone())
        .child(fixed(DshIcon::Workflow, 14.).text_color(theme::CAPTION()))
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .text_color(theme::LABEL_2())
                .child(label),
        )
        .child(
            fixed(
                if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
                14.,
            )
            .text_color(theme::CAPTION()),
        )
        .on_click(move |_, _, cx| {
            let key = key.clone();
            s.update(cx, |st, cx| st.toggle_turn_group(&key, cx));
        })
}

/// 单节点分发
#[allow(clippy::too_many_arguments)]
fn render_node(
    store: &Entity<AppStore>,
    cx: &App,
    open_reasoning: &std::collections::HashSet<String>,
    open_context: &std::collections::HashSet<String>,
    expanded_tools: &std::collections::HashSet<String>,
    open_retries: &std::collections::HashSet<String>,
    ix: usize,
    node: &ChatNode,
    col_w: gpui_kit::Pixels,
) -> impl IntoElement {
    match node {
        ChatNode::User {
            key, text, images, ..
        } => user_bubble(store, cx, ix, key, text, images, col_w).into_any_element(),
        ChatNode::Context {
            key,
            content,
            source,
        } => context_block(store, cx, open_context, ix, key, content, source).into_any_element(),
        ChatNode::Assistant {
            key,
            text,
            reasoning,
            streaming,
            message_id,
            ..
        } => assistant_block(
            store,
            cx,
            open_reasoning,
            ix,
            key,
            text,
            reasoning,
            *streaming,
            message_id,
        )
        .into_any_element(),
        ChatNode::Tool {
            key,
            name,
            summary,
            state,
            arguments,
            output,
            view,
        } => tool_block(
            store,
            expanded_tools,
            cx,
            ix,
            key,
            name,
            summary,
            *state,
            arguments,
            output.as_deref(),
            view.as_ref(),
        )
        .into_any_element(),
        ChatNode::TurnTail {
            aborted,
            meta,
            deliverables,
            ..
        } => turn_tail(store, *aborted, meta.as_deref(), deliverables).into_any_element(),
        ChatNode::Notice { text, .. } => notice(text).into_any_element(),
        ChatNode::Plan { key, plan, status } => {
            plan_archive_card(store, cx, ix, key, plan, *status).into_any_element()
        }
        ChatNode::Retry {
            key,
            retry,
            max_retries,
            delay_ms,
            message,
            state,
            ..
        } => retry_row(
            store,
            cx,
            open_retries,
            ix,
            key,
            *retry,
            *max_retries,
            *delay_ms,
            message,
            *state,
        )
        .into_any_element(),
    }
}

/// 计划归档卡(plan/submitted 落档):标题行(图标 + 「计划」 + 状态
/// 徽标 + 展开箭头)常显;正文 markdown 仅展开时渲染(默认折叠,
/// 归档可随时查看)。状态:待批准 WARN / 已批准 SUCCESS / 已取消 CAPTION。
fn plan_archive_card(
    store: &Entity<AppStore>,
    cx: &App,
    ix: usize,
    key: &str,
    plan: &str,
    status: PlanStatus,
) -> impl IntoElement {
    let open = store.read(cx).chat.open_plans.contains(key);
    let (status_text, status_color) = match status {
        PlanStatus::Pending => ("待批准", theme::WARN()),
        PlanStatus::Approved => ("已批准", theme::SUCCESS()),
        PlanStatus::Cancelled => ("已取消", theme::CAPTION()),
    };
    let s_toggle = store.clone();
    let key_owned = key.to_string();
    let body_sel = format!("plan-body-{ix}");
    div()
        .id(gpui_kit::ElementId::Name(SharedString::from(format!(
            "plan-node-{ix}"
        ))))
        .debug_selector(move || format!("plan-node-{ix}"))
        .w_full()
        .v_flex()
        .gap(px(6.))
        .rounded(px(10.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::LAYER())
        .p(px(10.))
        // 标题行:点击展开/收起
        .child(
            div()
                .id(gpui_kit::ElementId::Name(SharedString::from(format!(
                    "plan-node-head-{ix}"
                ))))
                .flex()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::DOCK()))
                .child(fixed(DshIcon::ListChecks, 14.).text_color(theme::LABEL_2()))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(theme::LABEL())
                        .child("计划"),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(status_color)
                        .child(status_text),
                )
                .child(div().flex_1())
                // 「查看」:开右栏计划标签。
                // 只拦 click(不触发行展开);mousedown 放行使根级外点
                // 关菜单照常收口。ghost 形态(无常驻底色,hover 才显):
                // 显式 12px——缺省字号继承后比 13px 标题还大,实测突兀
                .child({
                    let s_view = store.clone();
                    div()
                        .id(gpui_kit::ElementId::Name(SharedString::from(format!(
                            "plan-view-chip-{ix}"
                        ))))
                        .debug_selector(move || format!("plan-view-chip-{ix}"))
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
                            cx.stop_propagation();
                            s_view.update(cx, |st, cx| {
                                st.open_panel_tab(crate::shell::panel::PanelTab::Plan, cx)
                            });
                        })
                })
                .child(
                    fixed(
                        if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        },
                        12.,
                    )
                    .text_color(theme::CAPTION()),
                )
                .on_click(move |_, _, cx| {
                    s_toggle.update(cx, |st, cx| {
                        if !st.chat.open_plans.remove(&key_owned) {
                            st.chat.open_plans.insert(key_owned.clone());
                        }
                        cx.notify();
                    });
                }),
        )
        // 正文:仅展开时渲染;block 形态 + max_h 内滚(超长计划不撑爆)
        .when(open, |el| {
            el.child(
                div()
                    .id(gpui_kit::ElementId::Name(SharedString::from(
                        body_sel.clone(),
                    )))
                    .debug_selector(move || body_sel.clone())
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .text_size(px(13.))
                    .text_color(theme::LABEL_2())
                    .child(crate::kits::markdown::render(key, plan)),
            )
        })
}

/// 用户气泡:绝对宽 = 列宽 70%(原 525/748;同内容列,防测量塌陷),
/// 圆角 22,底色 #2b2b2c,整体靠右(气泡 + 动作行随右缘,动作行在文档流内)
fn user_bubble(
    store: &Entity<AppStore>,
    cx: &App,
    ix: usize,
    key: &str,
    text: &str,
    images: &[serde_json::Value],
    col_w: gpui_kit::Pixels,
) -> impl IntoElement {
    let bw = crate::shell::metrics::bubble_w(col_w);
    div()
        .v_flex()
        .flex_shrink_0()
        .items_end()
        .gap(px(4.))
        .child(
            div()
                // 内容自适应宽,封顶 70% 列宽:短消息(单 @token / 短语)气泡
                // 紧贴内容,长消息在 max 内 wrap。
                // 撤 `.w(bw)` 固定宽——那会把「📄 justfile」撑成整列宽度。
                .max_w(bw)
                .rounded(px(22.))
                .bg(theme::BUBBLE())
                .px(px(16.))
                .py(px(10.))
                .text_size(px(14.))
                .text_color(theme::LABEL())
                // 统一行高 = 1.5(24px @16px 同比例):
                // 文本 div 不再继承 gpui 默认 phi()(1.618→22.5px),避免
                // 与胶囊/图标混排时行盒高度不一致造成垂直错位
                .line_height(gpui_kit::relative(1.5))
                .v_flex()
                .items_end()
                .gap(px(8.))
                // 布局测试钩子:气泡自身 bounds(短消息贴合内容/长消息封顶 wrap)
                .debug_selector(move || format!("user-bubble-{ix}"))
                // 图消息先渲染缩略(单图 single/多图 tile),后接文本。
                // 仅当真有图时才挂图块——否则空 div + gap(8) 会凭空把内容
                // 挤出气泡垂直中心,造成「内容不居中」错位。
                .when(!images.is_empty(), |el| {
                    el.child(crate::features::attachments::message_images(
                        store, images, cx,
                    ))
                })
                .when(!text.is_empty(), |el| el.child(bubble_rich_text(text))),
        )
        .child(copy_button(store, cx, ("copy-user", ix), key, text))
}

/// 用户气泡富文本:`@file`/`@folder`/`@session` 渲染成胶囊(源 refChip),
/// 其余文本原样分段。GPUI 无真正 inline 混排,以 flex-wrap 近似:
/// 文本片段与胶囊同为 flex item,断行由 wrap 承担。
fn bubble_rich_text(text: &str) -> impl IntoElement {
    let tokens = super::reference::scan_at_tokens(text);
    if tokens.is_empty() {
        return div().child(text.to_string()).into_any_element();
    }
    let mut children: Vec<gpui_kit::AnyElement> = Vec::new();
    let mut cursor = 0;
    for tok in &tokens {
        if tok.start > cursor {
            children.push(
                div()
                    .child(text[cursor..tok.start].to_string())
                    .into_any_element(),
            );
        }
        children.push(bubble_ref_chip(tok).into_any_element());
        cursor = tok.end;
    }
    if cursor < text.len() {
        children.push(div().child(text[cursor..].to_string()).into_any_element());
    }
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(2.))
        .children(children)
        .into_any_element()
}

/// 单个 @ 引用胶囊:图标 + 主题蓝 label(源 .refChip)
fn bubble_ref_chip(tok: &super::reference::AtToken) -> impl IntoElement {
    use super::reference::AtKind;
    let (icon, label) = match tok.kind {
        AtKind::Session => (
            fixed(DshIcon::MessageSquare, 14.),
            format!("@{}", tok.label),
        ),
        AtKind::Folder => (fixed(IconName::Folder, 14.), basename_of(&tok.label)),
        AtKind::File => (fixed(IconName::File, 14.), basename_of(&tok.label)),
    };
    let chip_debug = label.clone();
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(4.))
        .mx(px(2.))
        .text_color(theme::BRAND())
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .line_height(gpui_kit::relative(1.5))
        .whitespace_nowrap()
        .debug_selector(move || format!("ref-chip-{chip_debug}"))
        .child(icon)
        .child(label)
}

/// 胶囊显示 label = 路径 basename(源 displayLabel:切片后取末尾段)
fn basename_of(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .trim_end_matches('/')
        .to_string()
}

/// 助手正文 + Think 折叠行(流式尾显光标)
#[allow(clippy::too_many_arguments)]
fn assistant_block(
    store: &Entity<AppStore>,
    cx: &App,
    open_reasoning: &std::collections::HashSet<String>,
    ix: usize,
    key: &str,
    text: &str,
    reasoning: &str,
    streaming: bool,
    message_id: &str,
) -> impl IntoElement {
    let open = open_reasoning.contains(key);
    let s = store.clone();
    let key = key.to_string();
    let click_key = key.clone();
    // gap 10:Think 折叠行与正文之间留呼吸感(6 过贴,过程与结论糊在一起)
    let mut col = div().v_flex().flex_shrink_0().relative().gap(px(10.));
    if !reasoning.is_empty() {
        col = col.child(
            div()
                .id(("think", ix))
                .v_flex()
                .rounded(px(8.))
                .bg(theme::LAYER())
                .px(px(10.))
                .cursor_pointer()
                .when(open, |el| el.py(px(8.)))
                .when(!open, |el| el.py(px(6.)))
                .child(
                    // 头行:脑图标 + 标签 + 摘要(截断)/ 弹性占位 + 折叠箭头
                    // (公共折叠头,与注入行同族)
                    collapse_row_header(
                        fixed(DshIcon::Brain, 14.).into_any_element(),
                        "Think",
                        (!open).then(|| {
                            // 折叠摘要:直播中取**尾部**(实时跟随正在思考的
                            // 末尾);定稿后取**开头**(思考首句与正文主题
                            // 呼应——尾部是下一步动作预告,常与正文措辞
                            // 对不上,实测观感「thinking 和输出对不上」)
                            let summary = if streaming {
                                tail_line(reasoning)
                            } else {
                                head_line(reasoning)
                            };
                            if streaming {
                                format!("{summary}▍")
                            } else {
                                summary
                            }
                        }),
                        open,
                    ),
                )
                .when(open, |el| {
                    el.child(
                        div()
                            .mt(px(4.))
                            .text_size(px(13.))
                            .text_color(theme::LABEL_3())
                            .line_height(gpui_kit::relative(1.5))
                            .child(reasoning.to_string()),
                    )
                })
                .on_click({
                    let s = s.clone();
                    move |_, _, cx| {
                        let key = click_key.clone();
                        s.update(cx, |st, cx| st.toggle_reasoning(&key, cx));
                    }
                }),
        );
    }
    // 正文区:仅在有内容时出现——推理期活动指示由 Think 行的
    // 尾部摘要 + 光标承担(此前的孤立 ▍ 行视觉上不成指示)
    if !text.is_empty() {
        // 流式与定稿统一 markdown 渲染(所见即所得;parse 按节点
        // 缓存,重解析只发生在文本真正变化的 chunk 帧)。流式光标
        // 由 markdown::render_streaming 在渲染期追加到末块(缓存
        // 哈希不含光标 → chunk 间的纯 vsync 帧直接命中缓存)
        // mermaid 卡片集:动作钩子 + per-key 状态快照(控件在卡片上,
        // 放大/缩放/下载/图表-代码切换;查看器是纯图,不把控件带进去)
        let cards = build_mermaid_cards(&s, cx);
        col = col.child(if streaming {
            crate::kits::markdown::render_streaming_clickable(&key, text, Some(cards.clone()))
                .into_any_element()
        } else {
            crate::kits::markdown::render_clickable(&key, text, Some(cards)).into_any_element()
        });
        // 定稿后可复制(流式中复制半截无意义);正文下方左对齐
        // 常显动作行(文档流内,非浮层)= 复制 + 消息反馈(赞/踩/备注)
        if !streaming {
            col = col.child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(4.))
                    .child(copy_button(store, cx, ("copy-asst", ix), &key, text))
                    .children(crate::features::feedback::actions(store, message_id, cx)),
            );
        }
    }
    col
}

/// 组装单条助手消息的 mermaid 卡片集:动作钩子(捕获 store entity,用户
/// 点击卡片工具条时经 `store.update` 调 store 方法)+ 每卡片 key 的状态
/// 快照(读取 store 里已持久化的 per-card `MermaidCard`)。`cards.states`
/// 来自 store 的 `mermaid_cards` 映射,按 `{prefix}-md-mermaid-{ix}` 键。
fn build_mermaid_cards(s: &Entity<AppStore>, cx: &App) -> crate::kits::mermaid::MermaidCards {
    let store = s.clone();
    let states = s
        .read(cx)
        .chat
        .mermaid_cards
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                crate::kits::mermaid::MermaidCardState {
                    show_code: v.show_code,
                    copied: v.copied,
                },
            )
        })
        .collect();
    let callbacks = crate::kits::mermaid::MermaidCardCallbacks {
        toggle_code: {
            let store = store.clone();
            Arc::new(move |card_key, _w, cx| {
                store.update(cx, |st, cx| st.toggle_mermaid_code(card_key, cx));
            })
        },
        copy: {
            // 复制反馈在按钮本体(store 侧绿色「已复制」态切换),
            // 不用异步通知——反馈须锚定在动作发生处
            let store = store.clone();
            Arc::new(move |card_key, source, _w, cx| {
                store.update(cx, |st, cx| st.copy_mermaid_source(card_key, &source, cx));
            })
        },
        enlarge: {
            let store = store.clone();
            Arc::new(move |card_key, source, _w, cx| {
                // 开图占位 = 卡片在档光栅(缓存命中零渲染;卡片固定
                // 1.0 档,与内嵌预览共用缓存)——占位的渲染档上下文:
                // 全图、pan 0。查看器首档走后台渲染,就位前地图式过渡
                // 显示占位,主线程不同步 usvg 解析
                let placeholder = crate::kits::mermaid::raster_at_zoom(card_key, &source, cx, 1.0)
                    .ok()
                    .map(|img| (img, 1.0));
                store.update(cx, |st, cx| {
                    st.open_mermaid_enlarged(source, placeholder, cx)
                });
            })
        },
        download: {
            // 下载完成提示用 can-gpui-component Notification(自动消失),
            // 而非 push_local_notice(插入消息流 → 永久)。导出为同步纯函数
            //(固定 1.0 自然档);通知需 window(WindowExt::push_notification),
            // 故回调收 &mut Window。
            Arc::new(|_card_key, source, window, cx| {
                let msg = match crate::kits::mermaid::export_diagram_png(&source, 1.0, cx) {
                    Ok(p) => (
                        "已导出:".to_string() + &p.display().to_string(),
                        NotificationType::Success,
                    ),
                    Err(e) => (format!("导出失败:{e}"), NotificationType::Error),
                };
                window.push_notification(
                    Notification::new()
                        .id::<MermaidDownloadNotice>()
                        .message(msg.0)
                        .with_type(msg.1),
                    cx,
                );
            })
        },
    };
    crate::kits::mermaid::MermaidCards { states, callbacks }
}

/// 下载通知的稳定类型 id:同类型通知互相替换(连续导出不无限堆叠)
struct MermaidDownloadNotice;
#[allow(clippy::too_many_arguments)]
fn tool_block(
    store: &Entity<AppStore>,
    expanded_tools: &std::collections::HashSet<String>,
    cx: &App,
    ix: usize,
    key: &str,
    name: &str,
    summary: &str,
    state: ToolState,
    arguments: &str,
    output: Option<&str>,
    view: Option<&serde_json::Value>,
) -> impl IntoElement {
    let expanded = expanded_tools.contains(key);
    let s = store.clone();
    let key = key.to_string();
    let click_key = key.clone();
    // 错误行折叠摘要 = 失败首行(错误色);
    // todo_write 行摘要 = 解析该次调用 args(计数 + 首个进行中,
    // 非全局当前态 —— 每行反映本次写入);
    // 正常摘要 = 键序取值,路径工具做工作区相对化。
    // 折叠行**不带状态圆点**(密度优先,运行中有扫光、失败有
    // 红色摘要;状态展示留给展开面板)
    let failure_line = if state == ToolState::Error {
        output.and_then(|o| o.lines().find(|l| !l.trim().is_empty()))
    } else {
        None
    };
    let todo_row = if name == "todo_write" && failure_line.is_none() {
        super::projection::todo_row_summary(arguments)
    } else {
        None
    };
    let ws_root = ws_root_of(store.read(cx));
    let summary_display = match name {
        "file_read" | "file_edit" => super::projection::relativize(ws_root.as_deref(), summary),
        _ => summary.to_string(),
    };
    let summary_line = failure_line
        .map(str::to_string)
        .or(todo_row.as_ref().map(|s| s.text.clone()))
        .unwrap_or(summary_display);
    let mut col = div().v_flex().flex_shrink_0().gap(px(4.));
    let row_sel = format!("tool-row-{key}");
    col = col.child(
        div()
            .id(("tool", ix))
            .debug_selector(move || row_sel.clone())
            .flex()
            .min_h(px(30.))
            .items_center()
            .gap(px(8.))
            .rounded(px(8.))
            .bg(theme::LAYER())
            .px(px(12.))
            .py(px(4.))
            .cursor_pointer()
            .hover(|s| s.opacity(0.9))
            // 运行中扫光的定位上下文 + 圆角裁剪(光带随行圆角出入)
            .relative()
            .overflow_hidden()
            .child(icons::tool_icon(name))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(13.))
                    .text_color(theme::LABEL_2())
                    // todo_write 行标题(「更新任务清单」)
                    .child(if name == "todo_write" {
                        "更新任务清单".to_string()
                    } else {
                        name.to_string()
                    }),
            )
            .child(
                div()
                    .min_w(px(0.))
                    .flex_1()
                    .truncate()
                    .text_size(px(13.))
                    .when(failure_line.is_some(), |el| el.text_color(theme::DANGER()))
                    .when(failure_line.is_none(), |el| el.text_color(theme::LABEL_3()))
                    .child(summary_line),
            )
            .when(todo_row.as_ref().is_some_and(|s| s.extra > 0), |el| {
                // 并行进行中额外数:不收缩后缀(窄行不剪)
                el.child(
                    div()
                        .debug_selector(|| "todo-row-extra".to_string())
                        .flex_shrink_0()
                        .text_size(px(13.))
                        .text_color(theme::LABEL_3())
                        .child(format!("+{}", todo_row.as_ref().unwrap().extra)),
                )
            })
            .child(
                // 展开态箭头(行尾;展开/收起由此表达)
                fixed(
                    if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    },
                    14.,
                )
                .text_color(theme::CAPTION()),
            )
            // 运行中扫光(Done/Error 无;叠加层不拦截点击 —— 纯 div 无
            // hitbox,行点击穿透)
            .when(state == ToolState::Running, |el| el.child(tool_sweep(ix)))
            .on_click(move |_, _, cx| {
                let key = click_key.clone();
                s.update(cx, |st, cx| st.toggle_tool(&key, cx));
            }),
    );
    if expanded {
        col = col.child(tool_expanded_body(
            store, cx, ix, &key, name, state, arguments, output, view,
        ));
    }
    col
}

/// 展开体路由:视图在场按 `card` 分发(终端/read/search/diff),
/// bash 前台例外认名取命令素材(运行中/旧会话无视图同走终端卡);
/// 其余与窄化失败 → IN/OUT 通用卡
#[allow(clippy::too_many_arguments)]
fn tool_expanded_body(
    store: &Entity<AppStore>,
    cx: &App,
    ix: usize,
    key: &str,
    name: &str,
    state: ToolState,
    arguments: &str,
    output: Option<&str>,
    view: Option<&serde_json::Value>,
) -> gpui_kit::AnyElement {
    use super::toolcard::{self, CardView};
    let narrowed = view.and_then(toolcard::narrow);
    let term = match &narrowed {
        Some(CardView::Terminal(t)) => Some(t),
        _ => None,
    };
    // 执行错误(Error 且非信号终止:spawn 失败/取消,无退出状态)与
    // 后台启动(输出是 job id 文本)走通用卡;信号终止经视图携带
    let signal = term.and_then(|t| t.signal.as_deref());
    let execution_error = state == ToolState::Error && signal.is_none();
    let body: gpui_kit::AnyElement = if name == "bash"
        && !execution_error
        && let Some(command) = bash_command(arguments)
    {
        let t = term;
        super::terminal::render(
            store,
            cx,
            ix,
            key,
            &command,
            t.and_then(|t| t.cwd.as_deref()),
            output,
            state,
            t.and_then(|t| t.exit_code),
            t.and_then(|t| t.signal.as_deref()),
        )
        .into_any_element()
    } else {
        match narrowed {
            Some(CardView::Read(card)) => {
                let ws_root = ws_root_of(store.read(cx));
                toolcard::render_read(store, cx, ix, key, &card, ws_root.as_deref())
                    .into_any_element()
            }
            Some(CardView::Search(card)) => {
                let mut body = div()
                    .v_flex()
                    .child(toolcard::render_search(store, cx, ix, key, &card));
                // 截断 recovery footer:卡下 tertiary 尾注(原始输出的截断行)
                if let Some(note) = toolcard::search_recovery_footer(output) {
                    body = body.child(
                        div()
                            .ml(px(4.))
                            .text_size(px(13.))
                            .text_color(theme::LABEL_3())
                            .child(note),
                    );
                }
                body.into_any_element()
            }
            Some(CardView::Diff(card)) => {
                toolcard::render_diff(store, cx, ix, key, &card).into_any_element()
            }
            // todo_write 展开体 = 该次写入的任务列表(结构化渲染,弃
            // IN/OUT JSON 卡;与 todo_dock 同一视觉语言),失败附错误首行
            _ if name == "todo_write" => {
                todo_write_expanded(ix, arguments, output, state == ToolState::Error)
            }
            _ => io_card(ix, arguments, output, state == ToolState::Error),
        }
    };
    // 展开体底部恒挂 Inspect 药丸,点击跳到轨迹该调用。
    let inspect = inspect_button(store, ix, key);
    div()
        .v_flex()
        .items_start()
        .gap(px(4.))
        // 卡体显式满列宽:外层 items_start 会按内容宽收缩卡(短内容 →
        // 半宽的 file_read);宽卡再包 w_full 后内部 overflow_scroll 才有
        // 锚点,超宽的 bash 输出在卡内横向滚而非溢出卡外。
        .child(div().w_full().child(body))
        .child(inspect)
        .into_any_element()
}

/// 展开体底部的 Inspect 药丸(源 ToolRow.inspect → inspectCall(callId)):
/// 点击切到轨迹 tab 并打开该 tool 调用的检查器。样式对齐 deliverable_chip。
fn inspect_button(store: &Entity<AppStore>, ix: usize, key: &str) -> gpui_kit::AnyElement {
    let s = store.clone();
    let k = key.to_string();
    let sel = format!("inspect-{key}");
    div()
        .id(("inspect", ix))
        // 左对齐,贴住卡体(容 gap(4));不右拉(源 .inspectButton 位于 bodyWrap
        // 内容流底部,非右对齐)
        .flex()
        .flex_shrink_0()
        .h(px(24.))
        .items_center()
        .gap(px(5.))
        .rounded(px(12.))
        .px(px(10.))
        .bg(theme::DOCK())
        .cursor_pointer()
        .hover(|st| st.bg(theme::BORDER()))
        .text_size(px(12.))
        .text_color(theme::LABEL_2())
        .debug_selector(move || sel.clone())
        .child(fixed(DshIcon::Code, 12.).into_any_element())
        .child("Inspect")
        .on_click(move |_, _, cx| {
            let k = k.clone();
            s.update(cx, |st, cx| st.inspect_call(&k, cx));
        })
        .into_any_element()
}

/// 当前会话工作区根(折叠行路径相对化与卡横幅共用)
fn ws_root_of(st: &AppStore) -> Option<String> {
    let id = st.state.current_id.as_deref()?;
    let default = st.default_workspace();
    let ws = crate::shell::reducer::workspace_of(id, &default);
    st.sessions
        .ws_paths
        .get(ws)
        .map(|p| p.display().to_string())
}

/// bash 调用的终端卡素材:参数里的 command(后台启动/参数异常 →
/// None 走通用卡)
fn bash_command(arguments: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(arguments).ok()?;
    if v["run_in_background"].as_bool().unwrap_or(false) {
        return None;
    }
    v["command"].as_str().map(str::to_string)
}

/// IN/OUT 卡(代码块表面 + l1 描边,IN/OUT 两个
/// 段落,中缝 l2 发丝线;段落各自 150px 封顶内部滚;失败调用的 OUT
/// 段用错误色)
/// todo_write 展开卡:该次调用写入的整表任务列表(状态点 + 内容,
/// 与 todo_dock 行同构);空表显「(空)」,失败附错误首行。
/// 坏 JSON(流中截断/坏参数)回落 IN/OUT 通用卡。
fn todo_write_expanded(
    ix: usize,
    arguments: &str,
    output: Option<&str>,
    is_error: bool,
) -> gpui_kit::AnyElement {
    let parsed: Option<Vec<super::projection::TodoItem>> =
        serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| {
                v["todos"].as_array().map(|list| {
                    list.iter()
                        .map(|t| super::projection::TodoItem {
                            content: t["content"].as_str().unwrap_or_default().to_string(),
                            status: t["status"].as_str().unwrap_or("pending").to_string(),
                        })
                        .collect()
                })
            });
    let Some(todos) = parsed else {
        return io_card(ix, arguments, output, is_error);
    };
    let mut card = div()
        .id(("todo-write-card", ix))
        .debug_selector(|| "todo-write-card".to_string())
        .v_flex()
        .ml(px(4.))
        .rounded(px(12.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::LAYER())
        .overflow_hidden()
        .px(px(12.))
        .py(px(8.))
        .when(todos.is_empty(), |el| {
            el.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::CAPTION())
                    .child("(空)"),
            )
        })
        .children(
            todos
                .iter()
                .map(super::todo_dock::todo_row)
                .collect::<Vec<_>>(),
        );
    if is_error {
        card = card.child(
            div().text_size(px(12.)).text_color(theme::DANGER()).child(
                output
                    .and_then(|o| o.lines().find(|l| !l.trim().is_empty()))
                    .unwrap_or("未知错误")
                    .to_string(),
            ),
        );
    }
    card.into_any_element()
}

fn io_card(
    ix: usize,
    arguments: &str,
    output: Option<&str>,
    is_error: bool,
) -> gpui_kit::AnyElement {
    let card_sel = format!("io-card-{ix}");
    let mut card = div()
        .id(("io-card", ix))
        .debug_selector(move || card_sel.clone())
        .v_flex()
        .ml(px(4.))
        .rounded(px(12.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::CODE())
        .overflow_hidden()
        .font_family("Menlo")
        .text_size(px(12.))
        .line_height(gpui_kit::relative(1.5))
        .child(io_section(
            "io-in",
            ix,
            "IN",
            &pretty_json(arguments),
            false,
        ));
    if let Some(o) = output {
        card = card
            .child(
                div()
                    .h(px(1.))
                    .w_full()
                    .flex_shrink_0()
                    .bg(theme::BORDER_2()),
            )
            .child(io_section("io-out", ix, "OUT", o, is_error));
    }
    card.into_any_element()
}

/// 一个 IN/OUT 段落:[标签 | 文本] 两栏,pre-wrap 换行,完整展示。
/// 不做段内 max_h 内滚:list 行内嵌滚动容器的高度测量与绘制脱节
/// (行槽按未裁剪内容计、绘制按 max_h 裁,展开即叠绘错乱,真机
/// subagent 超长参数首触此坑);聊天列表本身即滚动容器,不嵌套。
fn io_section(
    id: &'static str,
    ix: usize,
    label: &str,
    body: &str,
    error: bool,
) -> impl IntoElement {
    let sel = format!("{id}-{ix}");
    div()
        .id((id, ix))
        .debug_selector(move || sel.clone())
        .flex()
        .items_baseline()
        .gap(px(14.))
        .px(px(16.))
        .py(px(12.))
        .child(
            div()
                .flex_shrink_0()
                .text_color(theme::CAPTION())
                .child(label.to_string()),
        )
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .text_color(if error {
                    theme::DANGER()
                } else {
                    theme::LABEL_2()
                })
                .child(body.to_string()),
        )
}

/// 消息复制钮(文档流内常显,20px 命中区;点击写剪贴板,图标
/// Copy→Check——图标恒可见,hover
/// 只加底色。此前的浮层方案锚在内容列外,被滚动容器横向裁剪,
/// 生产不可见不可点)
fn copy_button(
    store: &Entity<AppStore>,
    cx: &App,
    id: impl Into<gpui_kit::ElementId>,
    key: &str,
    text: &str,
) -> impl IntoElement {
    let copied = store.read(cx).chat.copied_key.as_deref() == Some(key);
    let s = store.clone();
    let k = key.to_string();
    let t = text.to_string();
    let sel = format!("copy-{}", key);
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .size(px(20.))
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .cursor_pointer()
        .text_color(theme::CAPTION())
        .hover(|st| st.bg(theme::LAYER()).text_color(theme::LABEL_2()))
        .child(if copied {
            fixed(IconName::Check, 13.)
                .text_color(theme::BRAND())
                .into_any_element()
        } else {
            fixed(IconName::Copy, 13.).into_any_element()
        })
        // 测试钩子:按消息 key 稳定检索(release 空操作)
        .debug_selector(move || sel.clone())
        .on_click(move |_, _, cx| {
            let (k, t) = (k.clone(), t.clone());
            s.update(cx, |st, cx| st.copy_message(&k, &t, cx));
        })
}

/// 回合收尾行(状态 + 用量摘要)+ 产物行(diff/edit 路径 chip)
fn turn_tail(
    store: &Entity<AppStore>,
    aborted: bool,
    meta: Option<&str>,
    deliverables: &[String],
) -> impl IntoElement {
    let mut text = if aborted {
        "已中断".to_string()
    } else {
        "回合结束".to_string()
    };
    if let Some(m) = meta {
        text.push_str(&format!(" · {m}"));
    }
    div()
        .v_flex()
        .flex_shrink_0()
        .gap(px(4.))
        .child(
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(4.))
                .text_size(px(12.))
                .text_color(theme::CAPTION())
                .child(fixed(
                    if aborted {
                        IconName::TriangleAlert
                    } else {
                        IconName::Check
                    },
                    12.,
                ))
                .child(text),
        )
        .children((!deliverables.is_empty()).then(|| deliverables_row(store, deliverables)))
}

/// 产物行:basename chip + 完整路径 title,点击系统打开
fn deliverables_row(store: &Entity<AppStore>, deliverables: &[String]) -> impl IntoElement {
    // 简化:最多显示 6 个(资源行)
    let shown = &deliverables[..deliverables.len().min(6)];
    let more = deliverables.len().saturating_sub(shown.len());
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme::CAPTION())
                .child("产物"),
        )
        .children(shown.iter().map(|p| deliverable_chip(store, p)))
        .when(more > 0, |el| {
            el.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::CAPTION())
                    .child(format!("+ {more} 个文件")),
            )
        })
}

/// 单个产物 chip(basename 显示,完整路径 title;点击系统打开)
fn deliverable_chip(store: &Entity<AppStore>, path: &str) -> impl IntoElement {
    let base = path.rsplit('/').next().unwrap_or(path).to_string();
    let full = path.to_string();
    let full_sel = full.clone();
    let s = store.clone();
    div()
        .id(gpui_kit::SharedString::from(format!("deliv-{full}")))
        .debug_selector(move || format!("deliv-{full_sel}").to_string())
        .flex()
        .h(px(24.))
        .items_center()
        .rounded(px(12.))
        .px(px(10.))
        .bg(theme::DOCK())
        .cursor_pointer()
        .hover(|s| s.bg(theme::BORDER()))
        .text_size(px(12.))
        .text_color(theme::LABEL_2())
        .on_click(move |_, _, cx| {
            let p = full.clone();
            s.update(cx, |st, cx| st.open_deliverable(&p, cx));
        })
        .child(base)
}

/// 通告行(回合出错等;错误语义用 DANGER 红,非 WARN 黄)
/// LLM 请求重试行(llm/retry 折叠行):头行 = 图标 + 状态行
/// 「{label}（{retry}/{maximum}） · {seconds}s」(全角括号);
/// 展开 =「重试延迟：{delay}ms」「失败原因：{message}」。
/// 等待态直播中每秒倒计时(sync_retry_tick 触发重绘,渲染期推秒,
/// 下限 1);重放与已终态为静态排定值
#[allow(clippy::too_many_arguments)]
fn retry_row(
    store: &Entity<AppStore>,
    cx: &App,
    open_retries: &std::collections::HashSet<String>,
    ix: usize,
    key: &str,
    retry: u32,
    max_retries: u32,
    delay_ms: u64,
    message: &str,
    state: RetryState,
) -> impl IntoElement {
    let open = open_retries.contains(key);
    let now = std::time::Instant::now();
    let deadline = store
        .read(cx)
        .current_chat()
        .and_then(|c| c.retry_deadlines.get(key))
        .copied();
    let live = deadline.is_some_and(|d| d > now);
    // 秒数:等待态直播取剩余(下限 1);其余取排定值
    let seconds = match (state, deadline) {
        (RetryState::Waiting, Some(d)) if live => (d.duration_since(now).as_millis() as u64)
            .div_ceil(1000)
            .max(1),
        _ => delay_ms.div_ceil(1000).max(1),
    };
    let label = match state {
        RetryState::Waiting if live => "正在重试模型请求",
        RetryState::Waiting => "等待重试模型请求",
        RetryState::Started => "已重试模型请求",
        RetryState::Cancelled => "模型请求重试已取消",
    };
    let status = format!("{label}（{retry}/{max_retries}） · {seconds}s");
    let s = store.clone();
    let click_key = key.to_string();
    let detail_delay = format!("重试延迟：{delay_ms}ms");
    let detail_message = format!("失败原因：{message}");
    div()
        .id(("retry", ix))
        .debug_selector(move || format!("retry-row-{ix}"))
        .v_flex()
        .rounded(px(8.))
        .bg(theme::LAYER())
        .px(px(10.))
        .cursor_pointer()
        .when(open, |el| el.py(px(8.)))
        .when(!open, |el| el.py(px(6.)))
        .child(
            div()
                .flex()
                .min_w(px(0.))
                .items_center()
                .gap(px(4.))
                .text_size(px(12.))
                .text_color(theme::CAPTION())
                .child(fixed(DshIcon::RefreshCw, 14.))
                .child(div().min_w(px(0.)).flex_1().truncate().child(status))
                .child(fixed(
                    if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    },
                    14.,
                )),
        )
        .when(open, |el| {
            el.child(
                div()
                    .mt(px(4.))
                    .v_flex()
                    .gap(px(2.))
                    .text_size(px(12.))
                    .text_color(theme::LABEL_3())
                    .child(detail_delay)
                    .child(detail_message),
            )
        })
        .on_click(move |_, _, cx| {
            let key = click_key.clone();
            s.update(cx, |st, cx| st.toggle_retry(&key, cx));
        })
}

fn notice(text: &str) -> impl IntoElement {
    div()
        .debug_selector(|| "turn-notice".to_string())
        .flex()
        .flex_shrink_0()
        .items_start()
        .gap(px(6.))
        .text_size(px(12.))
        .text_color(theme::DANGER())
        // 图标 12px vs 文字行高 18px(12×1.5):下移补差,中心与首行文字对齐
        .child(
            div()
                .flex_shrink_0()
                .mt(px(3.))
                .child(fixed(IconName::TriangleAlert, 12.)),
        )
        // 文本块 flex_1 + min_w(0):在定宽列内自动换行(长错误信息
        // 单行会溢出;图标对齐首行)
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .line_height(gpui_kit::relative(1.5))
                .child(text.to_string()),
        )
}

/// 尾部摘要:最后一条非空行,超 60 字取**末** 60 字(推理实时进行时
/// 看到的是正在思考的末尾,而非静态开头)
fn tail_line(s: &str) -> String {
    let Some(l) = s.lines().rev().find(|l| !l.trim().is_empty()) else {
        return String::new();
    };
    let chars: Vec<char> = l.chars().collect();
    if chars.len() > 60 {
        let tail: String = chars[chars.len() - 60..].iter().collect();
        format!("…{tail}")
    } else {
        l.to_string()
    }
}

/// 头部摘要:首条非空行,超 60 字取**前** 60 字(定稿思考摘要与正文
/// 主题呼应;与 [`tail_line`] 对称)
fn head_line(s: &str) -> String {
    let Some(l) = s.lines().find(|l| !l.trim().is_empty()) else {
        return String::new();
    };
    let l = l.trim();
    let chars: Vec<char> = l.chars().collect();
    if chars.len() > 60 {
        let head: String = chars[..60].iter().collect();
        format!("{head}…")
    } else {
        l.to_string()
    }
}

/// JSON 串美化(失败原样)
fn pretty_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| s.to_string())
}

/// 插队待投递气泡(源 PendingSteeringBubble):用户气泡同款视觉,
/// pending 态 = 70% 透明 + 「插队 · 待投递」小标;右对齐
fn pending_steering_bubble(
    entry: &crate::features::chat::QueueEntry,
    col_w: gpui_kit::Pixels,
) -> impl IntoElement {
    div()
        .debug_selector(move || format!("pending-steering-{}", entry.id))
        .w(col_w)
        .flex()
        .justify_end()
        .px(px(0.))
        .child(
            div()
                .v_flex()
                .items_end()
                .gap(px(2.))
                .max_w(px(560.))
                .opacity(0.7)
                .child(
                    div()
                        .rounded(px(16.))
                        .bg(theme::BUBBLE())
                        .px(px(12.))
                        .py(px(8.))
                        .text_size(px(14.))
                        .text_color(theme::LABEL_2())
                        .max_w(px(560.))
                        .child(entry.preview.clone()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child("插队 · 待投递"),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::{context_provenance, head_line, tail_line};
    use serde_json::json;

    /// 折叠 Think 摘要 = 尾部内容(流式实时跟随)
    #[test]
    fn tail_line_takes_last_content() {
        assert_eq!(
            tail_line("开头静态内容\n中间\n正在思考的末尾"),
            "正在思考的末尾"
        );
        // 尾部空白行跳过
        assert_eq!(tail_line("行一\n行二\n\n"), "行二");
        // 超长取末 60 字带前缀省略
        let long = "字".repeat(100);
        let out = tail_line(&long);
        assert_eq!(out.chars().count(), 61);
        assert!(out.starts_with('…'));
        assert_eq!(tail_line(""), "");
    }

    /// 定稿摘要 = 头部内容(与正文主题呼应;与 tail_line 对称)
    #[test]
    fn head_line_takes_first_content() {
        assert_eq!(
            head_line("正在思考的开头\n中间\n结尾预告"),
            "正在思考的开头"
        );
        // 首部空白行跳过
        assert_eq!(head_line("\n \n行二"), "行二");
        // 超长取前 60 字带后缀省略
        let long = "字".repeat(100);
        let out = head_line(&long);
        assert_eq!(out.chars().count(), 61);
        assert!(out.ends_with('…'));
        assert_eq!(head_line(""), "");
    }

    /// 4a:provenance 派生 — recall(session-reference)/ inject(其余)与 label。
    #[test]
    fn context_provenance_derives_role_and_label() {
        let src = json!({ "kind": "session-reference", "references": [
            { "sessionId": "s1", "label": "设计讨论" },
        ] });
        let (role, label) = context_provenance(&src);
        assert_eq!(role, "recall");
        assert_eq!(label, "设计讨论");

        let src = json!({ "kind": "agent-instructions", "path": "/w/AGENTS.md" });
        let (role, label) = context_provenance(&src);
        assert_eq!(role, "inject");
        assert_eq!(label, "/w/AGENTS.md");

        // agent-instructions:changes[].path 去重连接优先(多层发现语义)
        let src = json!({ "kind": "agent-instructions", "path": "/w/AGENTS.md", "changes": [
            { "action": "set", "scope": ".\u{0}AGENTS.md", "path": "AGENTS.md", "digest": "a" },
            { "action": "set", "scope": "sub\u{0}AGENTS.md", "path": "sub/AGENTS.md", "digest": "b" },
            { "action": "set", "scope": ".\u{0}AGENTS.md", "path": "AGENTS.md", "digest": "c" }
        ] });
        let (_, label) = context_provenance(&src);
        assert_eq!(label, "AGENTS.md, sub/AGENTS.md");

        // paths 数组次之
        let src = json!({ "kind": "agent-instructions", "paths": ["AGENTS.md"] });
        let (_, label) = context_provenance(&src);
        assert_eq!(label, "AGENTS.md");

        // plugin 来源 → label 读 source.plugin
        let src = json!({ "kind": "plugin", "plugin": "@deepseek-ai/dsh-system-prompt" });
        let (role, label) = context_provenance(&src);
        assert_eq!(role, "inject");
        assert_eq!(label, "@deepseek-ai/dsh-system-prompt");

        // 无扩展字段 → kind 兜底
        let src = json!({ "kind": "plugin" });
        let (role, label) = context_provenance(&src);
        assert_eq!(role, "inject");
        assert_eq!(label, "plugin");
    }
}
