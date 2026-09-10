//! 轨迹视图(事件台账)。结构自上而下:工具栏(Duration/Turns/Calls/搜索)→ Overview
//! 时间线(3 轨道)→ 台账表 + 右侧检查器。行模型/交互语义 =
//! 台账表 / 时间线 / 工具栏三件套;
//! kind 色板派生自 dark token(常量处注释),骨架色用 theme.rs 现有项。

#![allow(non_snake_case)] // kind 色板取值 fn 保持原常量调用形态(随 theme 双盘)
use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use dsh_core::trajectory::{TrajectoryRecord, TrajectoryRequest, TrajectoryUsage};
use gpui_kit::component::input::Input;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, IconName, Sizable as _, StyledExt};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Bounds, CursorStyle, Div, Entity, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Rgba, ScrollWheelEvent,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::features::trajectory::{InspectTarget, TrajectoryView};
use crate::kits::icons::{DshIcon, fixed};
use crate::kits::theme;
use crate::shell::store::AppStore;

// ── kind 色板(源 dark 值派生;浅盘取白底可读变体)────────────
/// hex → Rgba(本地色板便捷构造)
fn rgb(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a: 1.0,
    }
}
// ASSISTANT 紫 = 源 brand-new(dark)60% 混 error-secondary 的合成结果
fn ASSISTANT_VIOLET() -> Rgba {
    if theme::is_dark() {
        rgb(0x9474BC)
    } else {
        rgb(0x6E4FA3)
    }
}
// TTFT 弱紫 = 解码紫 54% 混卡片底(运行时混合,随盘反演)
fn TTFT_VIOLET() -> Rgba {
    mix(ASSISTANT_VIOLET(), theme::CARD(), 0.54)
}
// TOOL 琥珀 = 源 warn-label(#DD8629)
fn TOOL_AMBER() -> Rgba {
    if theme::is_dark() {
        rgb(0xDD8629)
    } else {
        rgb(0xB45309)
    }
}
// JSON 高亮(VSCode Dark+ / Light+):字符串值
fn JSON_STRING() -> Rgba {
    if theme::is_dark() {
        rgb(0xCE9178)
    } else {
        rgb(0xA31515)
    }
}
// JSON 高亮:数字
fn JSON_NUMBER() -> Rgba {
    if theme::is_dark() {
        rgb(0xB5CEA8)
    } else {
        rgb(0x098658)
    }
}
// JSON 树(源 JsonTree dark theme 变量):键蓝 / 标点白 / 箭头灰
fn JSON_PROPERTY() -> Rgba {
    if theme::is_dark() {
        rgb(0x5DB0D7)
    } else {
        rgb(0x0451A5)
    }
}
fn JSON_PUNCT() -> Rgba {
    if theme::is_dark() {
        rgb(0xE8EAED)
    } else {
        rgb(0x3B3B3B)
    }
}
fn JSON_EXPANDER() -> Rgba {
    if theme::is_dark() {
        rgb(0x9AA0A6)
    } else {
        rgb(0x8E8E93)
    }
}

/// 颜色混合(t = a 的权重)
fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    Rgba {
        r: a.r * t + b.r * (1. - t),
        g: a.g * t + b.g * (1. - t),
        b: a.b * t + b.b * (1. - t),
        a: 1.0,
    }
}

/// kind → 台账标签文本(源 KIND_LABEL)
fn kind_label(kind: &str) -> &'static str {
    match kind {
        "system" => "SYSTEM",
        "user" => "USER",
        "context" => "CONTEXT",
        "compacted" => "COMPACTED",
        "message" => "ASSISTANT",
        _ => "TOOL",
    }
}

/// kind → 标签配色(源 kindTag 色板 dark 映射)
fn kind_colors(kind: &str) -> (Rgba, Rgba) {
    match kind {
        // USER:business 蓝前景 + 蓝 15% 底
        "user" => (theme::BRAND(), mix(theme::BRAND(), theme::BASE(), 0.15)),
        // ASSISTANT:解码紫前景 + 紫 15% 底
        "message" => (
            ASSISTANT_VIOLET(),
            mix(ASSISTANT_VIOLET(), theme::BASE(), 0.15),
        ),
        // TOOL:琥珀前景 + 琥珀 15% 底
        "tool" => (TOOL_AMBER(), mix(TOOL_AMBER(), theme::BASE(), 0.15)),
        // CONTEXT:success 绿前景 + 绿 15% 底(源 tagContext:success 主色混合)
        "context" => (
            mix(theme::SUCCESS(), theme::CAPTION(), 0.32),
            mix(theme::SUCCESS(), theme::BASE(), 0.15),
        ),
        // SYSTEM / COMPACTED:中性
        _ => (theme::LABEL_2(), theme::DOCK()),
    }
}

/// kind → 时间线轨道(源 laneFor:Input/Model/Tools)
fn lane_of(kind: &str) -> u8 {
    match kind {
        "system" | "user" | "context" => 0,
        "message" | "compacted" => 1,
        _ => 2,
    }
}

/// 工具行文本拆分:text = "{name} {args_json}" → (name, args)
fn split_tool_text(text: &str) -> (&str, &str) {
    match text.split_once(' ') {
        Some((name, args)) => (name, args),
        None => (text, ""),
    }
}

// ── 格式化 ────────────────────────────────────────────────────

/// 千分位整数
fn thousands(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if neg { format!("-{out}") } else { out }
}

/// 毫秒时长(源 formatElapsedSeconds 语义:千分位 ms)
fn fmt_ms(ms: i64) -> String {
    if ms <= 0 {
        "—".into()
    } else {
        format!("{} ms", thousands(ms))
    }
}

/// token 数千分位
fn fmt_tok(v: u64) -> String {
    thousands(v as i64)
}

/// epoch ms → 本地日期时间(Started 值格式:2026-08-18 22:59:59.493)
fn fmt_clock(ms: i64) -> String {
    if ms <= 0 {
        return "—".into();
    }
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
        .unwrap_or_else(|| "—".into())
}

/// Timing source 行值(有会话时间戳数据 = Session timestamps,
/// 否则 Not available)
fn timing_source(available: bool) -> String {
    if available {
        "Session timestamps".into()
    } else {
        "Not available".into()
    }
}

// ── 台账行模型(源 layout.ts:折叠/摘要/边界重建)──────────────

/// 一行台账(渲染模型)
enum LedgerRow<'a> {
    /// Turn 折叠摘要行(20px)
    TurnSummary {
        turn: u64,
        steps: usize,
        tools: usize,
    },
    /// Calls 折叠摘要行(20px)
    CallSummary {
        message_index: u64,
        count: usize,
        names: Vec<String>,
    },
    /// 记录行(30px);turn_start 为过滤后重算的轮首
    Record {
        rec: &'a TrajectoryRecord,
        turn_start: bool,
    },
}

/// 搜索命中(空格分词 AND、大小写不敏感;源 search() 语义)
fn search_hit(rec: &TrajectoryRecord, query: &str) -> bool {
    let tokens: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
    if tokens.is_empty() {
        return true;
    }
    let mut hay = format!(
        "{} {} {} {} ",
        rec.kind,
        rec.text,
        rec.result.as_deref().unwrap_or(""),
        rec.group,
    );
    if let Some(t) = rec.turn {
        hay.push_str(&format!("turn {t} "));
    }
    for f in [&rec.payload, &rec.output_detail, &rec.thinking_detail]
        .into_iter()
        .flatten()
    {
        hay.push(' ');
        hay.push_str(&f.to_lowercase());
    }
    let hay = hay.to_lowercase();
    tokens.iter().all(|t| hay.contains(t))
}

/// 折叠判定输入(渲染期从 store 快照)
struct CollapseState {
    all_turns: bool,
    turns: HashSet<u64>,
    all_calls: bool,
    calls: HashSet<u64>,
}

impl CollapseState {
    fn turn_collapsed(&self, turn: u64) -> bool {
        self.all_turns ^ self.turns.contains(&turn)
    }
    fn call_collapsed(&self, message_index: u64) -> bool {
        self.all_calls ^ self.calls.contains(&message_index)
    }
}

/// 可见记录 → 渲染行(搜索/时间线选区过滤 + turn/calls 折叠 + 轮首重算)
fn build_rows<'a>(
    records: &'a [TrajectoryRecord],
    visible: &[bool],
    collapse: &CollapseState,
) -> Vec<LedgerRow<'a>> {
    let mut rows: Vec<LedgerRow> = Vec::new();
    let mut seen_turns: HashSet<u64> = HashSet::new();
    let mut i = 0;
    while i < records.len() {
        if !visible[i] {
            i += 1;
            continue;
        }
        let rec = &records[i];
        let turn_start = rec.turn.is_some_and(|t| seen_turns.insert(t));
        // 轮折叠:保留首条,其余计步数(源 summarizeTurn)
        if let Some(t) = rec.turn
            && turn_start
            && collapse.turn_collapsed(t)
        {
            let mut steps = 0usize;
            let mut tools = 0usize;
            let mut j = i + 1;
            while j < records.len() {
                let r = &records[j];
                if !visible[j] {
                    j += 1;
                    continue;
                }
                if r.turn != Some(t) {
                    break;
                }
                steps += 1;
                if r.kind == "tool" {
                    tools += 1;
                }
                j += 1;
            }
            rows.push(LedgerRow::Record { rec, turn_start });
            rows.push(LedgerRow::TurnSummary {
                turn: t,
                steps,
                tools,
            });
            i = j;
            continue;
        }
        // Calls 折叠:assistant 步消息后的连续工具行并入摘要
        if rec.kind == "message"
            && rec.group.starts_with("Step")
            && collapse.call_collapsed(rec.index)
        {
            let mut j = i + 1;
            let mut names: Vec<String> = Vec::new();
            let mut count = 0usize;
            while j < records.len() {
                let r = &records[j];
                if !visible[j] {
                    j += 1;
                    continue;
                }
                if r.kind != "tool" || r.group != rec.group {
                    break;
                }
                let (name, _) = split_tool_text(&r.text);
                let name = name.to_string();
                if !names.contains(&name) {
                    names.push(name);
                }
                count += 1;
                j += 1;
            }
            rows.push(LedgerRow::Record { rec, turn_start });
            if count > 0 {
                rows.push(LedgerRow::CallSummary {
                    message_index: rec.index,
                    count,
                    names,
                });
            }
            i = j;
            continue;
        }
        rows.push(LedgerRow::Record { rec, turn_start });
        i += 1;
    }
    rows
}

// ── 时间线投影(源 timeline.ts:sequence/duration 双模式)──────

/// 一条投影条形(域归一化 0..1)
#[derive(Clone)]
struct TlSpan {
    x0: f64,
    x1: f64,
    record_index: u64,
    kind: String,
    is_error: bool,
    /// 助手条 TTFT 分界(条内 0..1;None = 单色)
    ttft_split: Option<f64>,
}

/// 投影:sequence = 等宽序列;duration = 按耗时、压缩 idle gap
fn build_spans(records: &[TrajectoryRecord], duration_mode: bool) -> Vec<TlSpan> {
    let entries: Vec<&TrajectoryRecord> =
        records.iter().filter(|r| r.started_at.is_some()).collect();
    if entries.is_empty() {
        return vec![];
    }
    let n = entries.len();
    if !duration_mode {
        return entries
            .iter()
            .enumerate()
            .map(|(i, r)| TlSpan {
                x0: i as f64 / n as f64,
                x1: (i + 1) as f64 / n as f64,
                record_index: r.index,
                kind: r.kind.clone(),
                is_error: r.is_error,
                ttft_split: assistant_ttft_split(r),
            })
            .collect();
    }
    let durs: Vec<f64> = entries
        .iter()
        .map(|r| r.time_seconds.unwrap_or(0.) * 1000.)
        .collect();
    let total: f64 = durs.iter().sum();
    if total <= 0. {
        return build_spans(records, false);
    }
    let mut acc = 0.;
    entries
        .iter()
        .zip(durs)
        .map(|(r, d)| {
            let x0 = acc / total;
            acc += d;
            TlSpan {
                x0,
                x1: acc / total,
                record_index: r.index,
                kind: r.kind.clone(),
                is_error: r.is_error,
                ttft_split: assistant_ttft_split(r),
            }
        })
        .collect()
}

/// 助手条 TTFT 分界 = ttft / 总时长(计时完整才有)
fn assistant_ttft_split(rec: &TrajectoryRecord) -> Option<f64> {
    if rec.kind != "message" {
        return None;
    }
    let ttft = rec.ttft_ms? as f64;
    let total = rec.time_seconds? * 1000.;
    if ttft <= 0. || total <= ttft {
        return None;
    }
    Some(ttft / total)
}

/// 条形主色(源 span 配色:USER 蓝/TOOL 琥珀/error 红/ASSISTANT 紫)
fn span_color(span: &TlSpan) -> Rgba {
    if span.is_error {
        return theme::DANGER();
    }
    match span.kind.as_str() {
        "user" => theme::BRAND(),
        "tool" => TOOL_AMBER(),
        "message" => ASSISTANT_VIOLET(),
        _ => theme::LABEL_3(),
    }
}

// ── 渲染期快照 ─────────────────────────────────────────────────

/// store 一次性读出(避免借用交叉;同 chat_pane 的预取模式)
struct Snap {
    view: TrajectoryView,
    duration: bool,
    collapse: CollapseState,
    inspector: Option<InspectTarget>,
    inspector_tab: Option<&'static str>,
    last_tab: &'static str,
    raw_thinking: bool,
    json_expanded: HashSet<String>,
    expanded_tools: HashSet<String>,
    selection: Option<(f64, f64)>,
    viewport: Option<(f64, f64)>,
    /// 拖拽锚点(track 原始分数 0..1,非视口映射域分数)
    drag: Option<f64>,
    draft: Option<(f64, f64)>,
    search: String,
    inspector_width: f32,
}

fn snap(store: &Entity<AppStore>, cx: &App) -> Snap {
    let st = store.read(cx);
    Snap {
        view: st.trajectory.trajectory.clone(),
        duration: st.trajectory.trajectory_duration,
        collapse: CollapseState {
            all_turns: st.trajectory.all_turns_collapsed,
            turns: st.trajectory.collapsed_turns.clone(),
            all_calls: st.trajectory.all_calls_collapsed,
            calls: st.trajectory.collapsed_calls.clone(),
        },
        inspector: st.trajectory.inspector,
        inspector_tab: st.trajectory.inspector_tab,
        last_tab: st.trajectory.inspector_last_tab,
        raw_thinking: st.trajectory.inspector_raw_thinking,
        json_expanded: st.trajectory.json_expanded.clone(),
        expanded_tools: st.trajectory.expanded_inspector_tools.clone(),
        selection: st.trajectory.timeline_selection,
        viewport: st.trajectory.timeline_viewport,
        drag: st.trajectory.timeline_drag,
        draft: st.trajectory.timeline_draft,
        search: st
            .trajectory
            .trajectory_search
            .as_ref()
            .map(|e| e.read(cx).value().to_string())
            .unwrap_or_default(),
        inspector_width: st.trajectory.inspector_width,
    }
}

/// track bounds 共享单元格(渲染期 canvas 写入,事件闭包读取;
/// GPUI 渲染单线程串行,thread-local 安全)
fn track_bounds_cell() -> Rc<Cell<Option<Bounds<Pixels>>>> {
    thread_local! {
        static CELL: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::default();
    }
    CELL.with(|c| c.clone())
}

/// 窗口 x → track 原始分数 0..1
fn local_frac(cell: &Rc<Cell<Option<Bounds<Pixels>>>>, window_x: Pixels) -> Option<f64> {
    let b = cell.get()?;
    let w = f32::from(b.size.width).max(1.) as f64;
    Some(f32::from(window_x - b.origin.x) as f64 / w)
}

// ── 主渲染 ────────────────────────────────────────────────────

/// 轨迹视图根:工具栏 + 时间线 + (台账表 | 检查器)
pub fn render(store: &Entity<AppStore>, window: &mut Window, cx: &mut App) -> impl IntoElement {
    let resizing = store.read(cx).trajectory.inspector_resize_anchor.is_some();
    let s = snap(store, cx);
    let records = &s.view.records;
    let spans = build_spans(records, s.duration);
    let searching = !s.search.trim().is_empty();
    let range = s.selection.or(s.draft);

    // 可见性 = 搜索 ∧ 时间线选区(选区按投影条重叠;无条目记录被移出)
    let visible: Vec<bool> = records
        .iter()
        .map(|r| {
            let ok_search = !searching || search_hit(r, &s.search);
            let ok_range = match range {
                None => true,
                Some((a, b)) => spans
                    .iter()
                    .any(|sp| sp.record_index == r.index && sp.x1 > a && sp.x0 < b),
            };
            ok_search && ok_range
        })
        .collect();

    // 检查器拖宽中:窗口级 move/up(渲染期注册;结束即自然消失)
    if resizing {
        let m = store.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, _, _, cx| {
            m.update(cx, |st, cx| {
                st.inspector_resize_move(f32::from(ev.position.x), cx)
            });
        });
        let u = store.clone();
        window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
            u.update(cx, |st, cx| st.inspector_resize_end(cx));
        });
    }

    // 时间线拖拽中:窗口级 move/up
    if s.drag.is_some() {
        let bounds = track_bounds_cell();
        let m = store.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, _, _, cx| {
            if let Some(frac) = local_frac(&bounds, ev.position.x) {
                m.update(cx, |st, cx| st.move_timeline_drag(frac, cx));
            }
        });
        let u = store.clone();
        let u_bounds = track_bounds_cell();
        let u_spans = spans.clone();
        window.on_mouse_event(move |ev: &MouseUpEvent, _, _, cx| {
            let (anchor, draft) = {
                let st = u.read(cx);
                (st.trajectory.timeline_drag, st.trajectory.timeline_draft)
            };
            let Some(anchor) = anchor else {
                u.update(cx, |st, cx| st.clear_timeline_drag(cx));
                return;
            };
            let track_w = u_bounds
                .get()
                .map(|b| f32::from(b.size.width))
                .unwrap_or(0.) as f64;
            let cur = local_frac(&u_bounds, ev.position.x).unwrap_or(anchor);
            // 位移 <3px 视为点击:命中条形 → 选记录;空白 → 最小窗口选区
            let is_click = (cur - anchor).abs() * track_w < 3.;
            u.update(cx, |st, cx| {
                st.clear_timeline_drag(cx);
                if is_click {
                    let (v0, v1) = st.trajectory.timeline_viewport.unwrap_or((0., 1.));
                    let domain = cur * (v1 - v0) + v0;
                    if let Some(sp) = u_spans.iter().find(|sp| domain >= sp.x0 && domain <= sp.x1) {
                        let ix = sp.record_index;
                        st.select_trajectory_record(ix, cx);
                    } else {
                        // 最小窗口 = 4 个操作宽,以点击点为中心
                        let n = u_spans.len().max(1) as f64;
                        let half = 2. / n;
                        st.set_timeline_selection(Some((domain - half, domain + half)), cx);
                    }
                } else if let Some(d) = draft {
                    st.set_timeline_selection(Some(d), cx);
                }
            });
        });
    }

    let rows = build_rows(records, &visible, &s.collapse);

    div()
        .id("trajectory-view")
        .v_flex()
        .size_full()
        .min_h(px(0.))
        .overflow_hidden()
        .debug_selector(|| "trajectory-view".to_string())
        .child(toolbar(store, &s, cx))
        .child(timeline(store, &s, &spans))
        .child(
            div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .child(ledger(store, &s, rows, cx))
                .children(inspector(store, &s, window, cx)),
        )
}

// ── 工具栏(源 TrajectoryToolbar)─────────────────────────────

fn toolbar(store: &Entity<AppStore>, s: &Snap, cx: &App) -> impl IntoElement {
    let input = store
        .read(cx)
        .trajectory
        .trajectory_search
        .as_ref()
        .map(|e| {
            div()
                .flex()
                .w(px(164.))
                .h(px(24.))
                .items_center()
                .child(Input::new(e).small())
        });
    div()
        .flex()
        .flex_shrink_0()
        .h(px(32.))
        .items_center()
        .gap(px(2.))
        .px(px(8.))
        .border_b_1()
        .border_color(theme::BORDER())
        .child(toggle_button(
            "traj-toolbar-duration",
            "Duration",
            s.duration,
            fixed(DshIcon::Clock, 12.),
            {
                let s = store.clone();
                move |_, _, cx| {
                    s.update(cx, |st, cx| st.toggle_trajectory_duration(cx));
                }
            },
        ))
        .child(action_button(
            "traj-toolbar-turns",
            "Turns",
            s.collapse.all_turns,
            {
                let s = store.clone();
                move |_, _, cx| {
                    s.update(cx, |st, cx| st.toggle_all_turns(cx));
                }
            },
        ))
        .child(action_button(
            "traj-toolbar-calls",
            "Calls",
            s.collapse.all_calls,
            {
                let s = store.clone();
                move |_, _, cx| {
                    s.update(cx, |st, cx| st.toggle_all_calls(cx));
                }
            },
        ))
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .truncate()
                .text_size(px(11.))
                .text_color(theme::CAPTION())
                .pl(px(8.))
                .child(format!(
                    "{} / {} 条 · {} 次请求",
                    s.view.records.len(),
                    s.view.total,
                    s.view.requests.len()
                )),
        )
        .children(input)
}

/// 工具栏切换钮(源 .toggle:模式开关,pressed = 高亮;恒显自身图标)
fn toggle_button(
    id: &'static str,
    label: &'static str,
    pressed: bool,
    icon: Icon,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let sel = id.to_string();
    div()
        .id(id)
        .debug_selector(move || sel.clone())
        .flex()
        .h(px(20.))
        .items_center()
        .gap(px(4.))
        .rounded(px(6.))
        .px(px(8.))
        .cursor_pointer()
        .text_size(px(11.))
        .when(pressed, |el| {
            el.bg(theme::GLASS_BG())
                .border_1()
                .border_color(theme::GLASS_BORDER())
                .text_color(theme::LABEL())
        })
        .when(!pressed, |el| {
            el.text_color(theme::LABEL_3())
                .hover(|s| s.bg(theme::BORDER()))
        })
        .child(icon)
        .child(label.to_string())
        .on_click(move |ev, w, cx| on_click(ev, w, cx))
}

/// 工具栏动作钮(源 .action:展开/折叠动作,无 pressed 态;图标随状态
/// 翻转——全折叠显 ⊞(点=展开),展开显 ⊟(点=折叠),等宽字体同源)
fn action_button(
    id: &'static str,
    label: &'static str,
    all_collapsed: bool,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let sel = id.to_string();
    div()
        .id(id)
        .debug_selector(move || sel.clone())
        .flex()
        .h(px(20.))
        .items_center()
        .gap(px(4.))
        .rounded(px(6.))
        .px(px(5.))
        .cursor_pointer()
        .text_size(px(11.))
        .text_color(theme::LABEL_3())
        .hover(|s| s.bg(theme::BORDER()).text_color(theme::LABEL_2()))
        .child(
            div()
                .font_family("Menlo")
                .text_size(px(12.))
                .line_height(gpui_kit::relative(1.))
                .child(if all_collapsed { "⊞" } else { "⊟" }),
        )
        .child(label.to_string())
        .on_click(move |ev, w, cx| on_click(ev, w, cx))
}

// ── Overview 时间线(源 TrajectoryTimeline)────────────────────

fn timeline(store: &Entity<AppStore>, s: &Snap, spans: &[TlSpan]) -> impl IntoElement {
    let labels = ["Input", "Model", "Tools"];
    let labels_col = div().w(px(44.)).flex_shrink_0().relative().children(
        labels
            .iter()
            .enumerate()
            .map(|(lane, l)| {
                div()
                    .absolute()
                    .left_0()
                    .right(px(6.))
                    .top(px(6. + lane as f32 * 14.))
                    .text_right()
                    .text_size(px(10.))
                    .text_color(theme::CAPTION())
                    .child(l.to_string())
            })
            .collect::<Vec<_>>(),
    );

    // 视口映射:域分数 → track 分数
    let (v0, v1) = s.viewport.unwrap_or((0., 1.));
    let vspan = (v1 - v0).max(1e-6);
    let to_track = |x: f64| ((x - v0) / vspan).clamp(0., 1.7);

    let selected_record = match s.inspector {
        Some(InspectTarget::Record(ix)) => Some(ix),
        _ => None,
    };
    let mut bars: Vec<gpui_kit::AnyElement> = Vec::new();
    for (i, sp) in spans.iter().enumerate() {
        let x0 = to_track(sp.x0);
        let x1 = to_track(sp.x1);
        if x1 <= 0. || x0 >= 1. {
            continue;
        }
        let lane = lane_of(&sp.kind) as f32;
        let top = px(7. + lane * 14.);
        let left_pct = x0.clamp(0., 1.);
        let right_pct = x1.clamp(0., 1.);
        let width = right_pct - left_pct;
        let is_current = selected_record == Some(sp.record_index);
        let store2 = store.clone();
        let ix = sp.record_index;
        // 条构造闭包(TTFT 分色需建两次;Stateful 不可 Clone)
        let make = |left: f32, w: f32| {
            let s3 = store2.clone();
            div()
                .id(("tl-span", i))
                .absolute()
                .top(top)
                .h(px(8.))
                .rounded(px(1.))
                .left(gpui_kit::relative(left))
                .w(gpui_kit::relative(w))
                .when(is_current, |el| el.border_1().border_color(theme::BRAND()))
                .hover(|st| st.border_1().border_color(theme::LABEL_3()))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    s3.update(cx, |st, cx| st.select_trajectory_record(ix, cx));
                })
        };
        if let Some(split) = sp.ttft_split {
            // TTFT/解码分色:左段弱紫 + 右段解码紫(源渐变分界的离散近似)
            let total = width.max(1e-4) as f32;
            let left_w = (total * split as f32).max(0.002);
            bars.push(
                make(left_pct as f32, left_w)
                    .bg(TTFT_VIOLET())
                    .into_any_element(),
            );
            bars.push(
                make(
                    (left_pct + left_w as f64) as f32,
                    (total - left_w).max(0.002),
                )
                .bg(ASSISTANT_VIOLET())
                .into_any_element(),
            );
        } else {
            bars.push(
                make(left_pct as f32, width.max(0.004) as f32)
                    .bg(span_color(sp))
                    .into_any_element(),
            );
        }
    }

    // turn 边界竖线(turn_start 记录的条形位置)
    let mut turn_lines: Vec<gpui_kit::AnyElement> = Vec::new();
    for r in s.view.records.iter().filter(|r| r.turn_start) {
        if let Some(sp) = spans.iter().find(|sp| sp.record_index == r.index) {
            let x = to_track(sp.x0).clamp(0., 1.);
            if x <= 0. {
                continue;
            }
            turn_lines.push(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(gpui_kit::relative(x as f32))
                    .w(px(1.))
                    .bg(theme::BORDER())
                    .into_any_element(),
            );
        }
    }

    // 选区/草稿视觉:填充 + 两侧边线 + 选区外遮罩(源 selection 样式)
    let sel = s.draft.or(s.selection);
    let mut overlays: Vec<gpui_kit::AnyElement> = Vec::new();
    if let Some((a, b)) = sel {
        let a = to_track(a).clamp(0., 1.);
        let b = to_track(b).clamp(0., 1.);
        if b > a {
            overlays.push(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(gpui_kit::relative(a as f32))
                    .w(gpui_kit::relative((b - a) as f32))
                    .bg(gpui_kit::Rgba {
                        a: 0.12,
                        ..theme::BRAND()
                    })
                    .into_any_element(),
            );
            for x in [a, b] {
                overlays.push(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(gpui_kit::relative(x as f32))
                        .w(px(2.))
                        .bg(theme::BRAND())
                        .into_any_element(),
                );
            }
            if a > 0. {
                overlays.push(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .w(gpui_kit::relative(a as f32))
                        .bg(gpui_kit::Rgba {
                            a: 0.58,
                            ..theme::INK()
                        })
                        .into_any_element(),
                );
            }
            if b < 1. {
                overlays.push(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(gpui_kit::relative(b as f32))
                        .right_0()
                        .bg(gpui_kit::Rgba {
                            a: 0.58,
                            ..theme::INK()
                        })
                        .into_any_element(),
                );
            }
        }
    }

    // 空计时数据文案
    let empty = spans.is_empty().then(|| {
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .right_0()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(10.))
            .text_color(theme::CAPTION())
            .child("No timing data")
    });

    let bounds = track_bounds_cell();
    let down_store = store.clone();
    let left_store = down_store.clone();
    let down_bounds = bounds.clone();
    let wheel_store = store.clone();
    let wheel_bounds = bounds.clone();
    let wheel_n = spans.len().max(1);

    // 「…」加载更早(左缘,仅 has_older)
    let load_earlier = s.view.has_older.then(|| {
        let s2 = store.clone();
        div()
            .id("tl-load-earlier")
            .absolute()
            .left_0()
            .top_0()
            .bottom_0()
            .w(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(11.))
            .text_color(theme::LABEL_3())
            .bg(theme::DOCK())
            .rounded_r(px(6.))
            .opacity(0.85)
            .hover(|st| st.opacity(1.))
            .cursor_pointer()
            .child("…")
            .on_click(move |_, _, cx| {
                s2.update(cx, |st, cx| st.load_earlier_trajectory(cx));
            })
    });

    let track = div()
        .id("timeline-track")
        .relative()
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .overflow_hidden()
        .cursor(CursorStyle::Crosshair)
        .debug_selector(|| "timeline-track".to_string())
        // 渲染期写入 bounds(canvas 包一层 div 以取 Styled)
        .child(div().absolute().size_full().child(gpui_kit::canvas(
            move |b, _, _| {
                bounds.set(Some(b));
            },
            |_, _, _, _| {},
        )))
        .children(turn_lines)
        .children(bars)
        .children(overlays)
        .children(load_earlier)
        .children(empty)
        .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, _, cx| {
            // 双击清选区;按下开启拖拽(锚点 = track 原始分数)
            if ev.click_count == 2 {
                left_store.update(cx, |st, cx| st.set_timeline_selection(None, cx));
                return;
            }
            let Some(frac) = local_frac(&down_bounds, ev.position.x) else {
                return;
            };
            cx.stop_propagation();
            left_store.update(cx, |st, cx| st.begin_timeline_drag(frac, cx));
        })
        .on_mouse_down(MouseButton::Right, {
            let right_store = down_store.clone();
            move |_, _, cx| {
                cx.stop_propagation();
                right_store.update(cx, |st, cx| st.set_timeline_selection(None, cx));
            }
        })
        .on_scroll_wheel(move |ev: &ScrollWheelEvent, _, cx| {
            // 滚轮缩放:光标锚点,domain × exp(ΔY×0.0015),下限 4 操作
            let Some(b) = wheel_bounds.get() else { return };
            let w = f32::from(b.size.width).max(1.) as f64;
            let local = f32::from(ev.position.x - b.origin.x) as f64 / w;
            if !(0. ..=1.).contains(&local) {
                return;
            }
            let dy = match ev.delta {
                gpui_kit::ScrollDelta::Lines(l) => l.y as f64 * 40.,
                gpui_kit::ScrollDelta::Pixels(p) => f32::from(p.y) as f64,
            };
            let min_span = 4. / wheel_n as f64;
            wheel_store.update(cx, |st, cx| {
                let (a0, b0) = st.trajectory.timeline_viewport.unwrap_or((0., 1.));
                let span = b0 - a0;
                if span >= 1. && dy < 0. {
                    return; // 已全览继续放大:无操作
                }
                let ratio = (dy * 0.0015).exp();
                let new_span = (span * ratio).clamp(min_span, 1.);
                let anchor = a0 + local * span;
                let na = (anchor - local * new_span).clamp(0., 1. - new_span);
                let nb = na + new_span;
                st.set_timeline_viewport(Some((na, nb)), cx);
            });
        });

    div()
        .flex()
        .flex_shrink_0()
        .h(px(50.))
        .bg(theme::CARD())
        .border_b_1()
        .border_color(theme::BORDER())
        .child(labels_col)
        .child(track)
        .child(
            div()
                .flex()
                .items_center()
                .px(px(6.))
                .text_size(px(9.))
                .text_color(theme::CAPTION())
                .child("拖选筛选 · 滚轮缩放 · 右键清除"),
        )
}

// ── 台账表(源 TrajectoryTable)────────────────────────────────

fn ledger(
    store: &Entity<AppStore>,
    s: &Snap,
    rows: Vec<LedgerRow<'_>>,
    cx: &App,
) -> impl IntoElement {
    let scroll = store.read(cx).trajectory.trajectory_scroll.clone();
    let loading_initial = s.view.loading && s.view.records.is_empty();
    let empty_table = !loading_initial && s.view.records.is_empty();

    let mut children: Vec<gpui_kit::AnyElement> = Vec::new();
    if s.view.has_older {
        let s2 = store.clone();
        children.push(
            div()
                .id("load-earlier")
                .flex()
                .h(px(30.))
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(theme::LABEL_3())
                .hover(|st| st.bg(theme::LAYER()).text_color(theme::LABEL_2()))
                .debug_selector(|| "load-earlier".to_string())
                .when(s.view.loading_older, |el| {
                    el.child(Spinner::new().xsmall())
                        .child("正在加载更早记录…".to_string())
                })
                .when(!s.view.loading_older, |el| {
                    el.child(format!(
                        "加载更早记录(还有 {} 条)",
                        s.view.total.saturating_sub(s.view.records.len() as u64)
                    ))
                })
                .on_click(move |_, _, cx| {
                    s2.update(cx, |st, cx| st.load_earlier_trajectory(cx));
                })
                .into_any_element(),
        );
    }
    for row in &rows {
        children.push(match row {
            LedgerRow::TurnSummary { turn, steps, tools } => {
                turn_summary_row(store, *turn, *steps, *tools).into_any_element()
            }
            LedgerRow::CallSummary {
                message_index,
                count,
                names,
            } => call_summary_row(store, *message_index, *count, names).into_any_element(),
            LedgerRow::Record { rec, turn_start } => {
                record_row(store, s, rec, *turn_start).into_any_element()
            }
        });
    }
    if loading_initial {
        children.push(
            div()
                .id("trajectory-loading")
                .flex()
                .h(px(40.))
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(theme::CAPTION())
                .child(Spinner::new().xsmall())
                .child("正在折叠轨迹…")
                .into_any_element(),
        );
    }
    if empty_table {
        children.push(
            div()
                .id("trajectory-empty")
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(120.))
                .items_center()
                .justify_center()
                .gap(px(8.))
                .py(px(40.))
                .text_size(px(12.))
                .text_color(theme::CAPTION())
                .debug_selector(|| "trajectory-empty".to_string())
                .child(fixed(IconName::Inbox, 16.))
                .child("会话尚无事件——发送首条消息后生成轨迹")
                .into_any_element(),
        );
    }

    let s2 = store.clone();
    let s3 = store.clone();
    div()
        .id("trajectory-scroll")
        .track_scroll(&scroll)
        .v_flex()
        .flex_1()
        .min_w(px(0.))
        .min_h(px(0.))
        .overflow_y_scroll()
        .debug_selector(|| "trajectory-scroll".to_string())
        .on_scroll_wheel(move |_, _, cx| {
            s3.update(cx, |st, cx| st.on_trajectory_scroll(cx));
        })
        // 点表空白:关检查器 + 清时间线选区(行内点击已 stop_propagation,
        // 到达此处的必是背景点击——源 clearAllSelections 语义)
        .on_click(move |_, _, cx| {
            s2.update(cx, |st, cx| {
                st.close_inspector(cx);
                st.set_timeline_selection(None, cx);
            });
        })
        .children(children)
}

/// Turn 折叠摘要行(20px;源 summarizeTurn)
fn turn_summary_row(
    store: &Entity<AppStore>,
    turn: u64,
    steps: usize,
    tools: usize,
) -> impl IntoElement {
    let s = store.clone();
    div()
        .id(("turn-summary", turn as usize))
        .flex()
        .h(px(20.))
        .flex_shrink_0()
        .items_center()
        .pl(px(40.))
        .cursor_pointer()
        .text_size(px(11.))
        .text_color(theme::CAPTION())
        .hover(|st| st.text_color(theme::LABEL_3()))
        .debug_selector(move || format!("turn-summary-{turn}"))
        .child(format!("… {steps} 步 · {tools} 个工具调用"))
        .on_click(move |_, _, cx| {
            s.update(cx, |st, cx| st.toggle_turn(turn, cx));
        })
}

/// Calls 折叠摘要行(20px;源 assistant 折叠)
fn call_summary_row(
    store: &Entity<AppStore>,
    message_index: u64,
    count: usize,
    names: &[String],
) -> impl IntoElement {
    let s = store.clone();
    div()
        .id(("call-summary", message_index as usize))
        .flex()
        .h(px(20.))
        .flex_shrink_0()
        .items_center()
        .pl(px(40.))
        .cursor_pointer()
        .text_size(px(11.))
        .text_color(theme::CAPTION())
        .hover(|st| st.text_color(theme::LABEL_3()))
        .debug_selector(move || format!("call-summary-{message_index}"))
        .child(format!("… {count} 个工具调用 · {}", names.join(", ")))
        .on_click(move |_, _, cx| {
            s.update(cx, |st, cx| st.toggle_call(message_index, cx));
        })
}

/// 记录行(30px):event 列 122px(轮角标 + Request 圆点 + kindTag)+
/// content 列(摘要 / 工具双栏)
fn record_row(
    store: &Entity<AppStore>,
    s: &Snap,
    rec: &TrajectoryRecord,
    turn_start: bool,
) -> impl IntoElement {
    let selected = s.inspector == Some(InspectTarget::Record(rec.index));
    let selected_turn = match s.inspector {
        Some(InspectTarget::Record(ix)) => s
            .view
            .records
            .iter()
            .find(|r| r.index == ix)
            .and_then(|r| r.turn)
            .is_some_and(|t| Some(t) == rec.turn),
        _ => false,
    };

    let rail_color = if rec.is_error {
        mix(theme::DANGER(), theme::BASE(), 0.22)
    } else {
        mix(theme::BRAND(), theme::BASE(), 0.22)
    };

    // event 列
    let mut event = div()
        .relative()
        .w(px(122.))
        .flex_shrink_0()
        .h_full()
        .flex()
        .items_center()
        .pl(px(36.))
        .pr(px(4.));
    if selected_turn && !selected {
        event = event.child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(2.))
                .bg(rail_color),
        );
    }
    if turn_start && let Some(t) = rec.turn {
        event = event.child(
            div()
                .absolute()
                .left(px(2.))
                .top(px(1.))
                .rounded_b(px(2.))
                .bg(theme::DOCK())
                .px(px(5.))
                .py(px(1.))
                .font_family("Menlo")
                .text_size(px(8.))
                .text_color(theme::LABEL_3())
                .child(format!("Turn {t}")),
        );
    }
    if let Some(n) = rec.request_number {
        let s2 = store.clone();
        let dot_color = if s
            .view
            .requests
            .iter()
            .any(|q| q.number == n && q.status == "error")
        {
            theme::DANGER()
        } else if s.inspector == Some(InspectTarget::Request(n)) {
            theme::BRAND()
        } else {
            theme::CAPTION()
        };
        let active_req = s.inspector == Some(InspectTarget::Request(n));
        event = event.child(
            div()
                .id(("traj-request", n as usize))
                .size(px(16.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .debug_selector(move || format!("traj-request-{n}"))
                .child(
                    div()
                        .size(px(5.))
                        .rounded_full()
                        .bg(dot_color)
                        .when(active_req, |el| el.border_1().border_color(theme::BRAND())),
                )
                .hover(|st| st.opacity(0.85))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    s2.update(cx, |st, cx| st.select_trajectory_request(n, cx));
                }),
        );
    }
    let (fg, bg) = kind_colors(&rec.kind);
    event = event.child(div().flex_1());
    event = event.child(
        div()
            .h(px(19.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(3.))
            .rounded(px(4.))
            .bg(bg)
            .px(px(5.))
            .text_size(px(10.))
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
            .text_color(fg)
            .child(kind_label(&rec.kind).to_string()),
    );

    // content 列
    let content: gpui_kit::AnyElement = if rec.kind == "tool" {
        let (name, args) = split_tool_text(&rec.text);
        let result_color = if rec.is_error {
            theme::DANGER()
        } else if rec.result.as_deref() == Some("No output") {
            theme::CAPTION()
        } else {
            theme::LABEL_3()
        };
        div()
            .flex()
            .min_w(px(0.))
            .flex_1()
            .items_center()
            .gap(px(7.))
            .px(px(8.))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .font_family("Menlo")
                    .text_color(theme::LABEL_2())
                    .child(name.to_string()),
            )
            .child(
                div()
                    .min_w(px(0.))
                    .flex_1()
                    .truncate()
                    .text_size(px(12.))
                    .font_family("Menlo")
                    .text_color(theme::LABEL_3())
                    .child(args.to_string()),
            )
            .when_some(rec.result.clone(), |el, r| {
                el.child(
                    div()
                        .flex()
                        .min_w(px(0.))
                        .max_w(gpui_kit::relative(0.4))
                        .flex_shrink_0()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::CAPTION())
                                .child("→"),
                        )
                        .child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .text_size(px(12.))
                                .text_color(result_color)
                                .child(r),
                        ),
                )
            })
            .into_any_element()
    } else {
        let color = match rec.kind.as_str() {
            "user" => theme::LABEL(),
            "message" if rec.text == "(tool call only)" => theme::CAPTION(),
            "message" => theme::LABEL_2(),
            _ => theme::LABEL_3(),
        };
        div()
            .min_w(px(0.))
            .flex_1()
            .truncate()
            .px(px(8.))
            .text_size(px(12.))
            .text_color(color)
            .child(rec.text.clone())
            .into_any_element()
    };

    let s2 = store.clone();
    let ix = rec.index;
    div()
        .id(("traj-row", ix as usize))
        .relative()
        .flex()
        .h(px(30.))
        .flex_shrink_0()
        .items_center()
        .when(turn_start, |el| {
            el.border_t_2().border_color(theme::BORDER())
        })
        .when(selected, |el| el.bg(theme::LAYER()))
        .when(!selected, |el| el.hover(|st| st.bg(theme::LAYER())))
        .cursor_pointer()
        .debug_selector(move || format!("trajectory-row-{ix}"))
        .child(event)
        .child(content)
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            s2.update(cx, |st, cx| st.select_trajectory_record(ix, cx));
        })
}

// ── 检查器(源 details aside)──────────────────────────────────

/// 检查器标签页集合(按数据在场裁剪;diff_available = 更新记录且
/// 前一 SYSTEM 快照在场)
fn inspector_tabs_for(rec: Option<&TrajectoryRecord>, diff_available: bool) -> Vec<&'static str> {
    let Some(r) = rec else {
        return vec!["summary", "usage", "timing"]; // 请求
    };
    match r.kind.as_str() {
        "tool" => {
            // 源 tab 集:Summary / Payload?/ Result?/ Schema / Timing
            // —— Schema、Timing 恒在(数据缺席由页内缺省文案兜底)
            let mut tabs = vec!["summary"];
            if r.payload.is_some() {
                tabs.push("payload");
            }
            if r.result.is_some() || r.output_detail.is_some() {
                tabs.push("result");
            }
            tabs.push("schema");
            tabs.push("timing");
            tabs
        }
        "message" => {
            // 源 isMarkdownRecord(message)→ [Summary, Preview, Raw]
            // 恒三页(内容缺席由页内缺省文案兜底,同源空态)
            vec!["summary", "preview", "raw"]
        }
        "context" => {
            // 源 isMarkdownRecord(context)→ [Summary, Preview, Raw],
            // source 染色在场 → 尾加 Source(源 messageSource 条件)
            let mut tabs = vec!["summary", "preview", "raw"];
            if r.source.is_some() {
                tabs.push("source");
            }
            tabs
        }
        "user" => vec!["summary", "payload"],
        "system" => {
            // 源 SYSTEM_PROMPT_TABS / SYSTEM_UPDATE_TABS:无 Summary 页;
            // Initial = System Prompt + Tools,更新 = Diff 在首。System/Tools
            // 恒在(老日志无快照由页内缺省文案兜底,同源空态)
            let mut tabs = Vec::new();
            if diff_available {
                tabs.push("diff");
            }
            tabs.push("system");
            tabs.push("tools");
            tabs
        }
        "compacted" => {
            if r.output_detail.is_some() {
                vec!["summary", "result"]
            } else {
                vec!["summary"]
            }
        }
        _ => vec!["summary"],
    }
}

/// 前一 SYSTEM 快照(Diff 页左侧;更新记录按行序向前找带快照的 system)
fn previous_system_snapshot<'a>(
    records: &'a [TrajectoryRecord],
    current: &TrajectoryRecord,
) -> Option<&'a TrajectoryRecord> {
    records
        .iter()
        .rfind(|r| r.kind == "system" && r.index < current.index && r.system_prompt.is_some())
}

fn inspector(
    store: &Entity<AppStore>,
    s: &Snap,
    _window: &mut Window,
    _cx: &mut App,
) -> Option<impl IntoElement> {
    let target = s.inspector?;
    let record = match target {
        InspectTarget::Record(ix) => s.view.records.iter().find(|r| r.index == ix),
        InspectTarget::Request(_) => None,
    };
    let request = match target {
        InspectTarget::Request(n) => s.view.requests.iter().find(|q| q.number == n),
        InspectTarget::Record(_) => record
            .and_then(|r| r.request_number)
            .and_then(|n| s.view.requests.iter().find(|q| q.number == n)),
    };

    let diff_available = record.is_some_and(|r| {
        r.kind == "system"
            && r.text != "Initial System Prompt"
            && previous_system_snapshot(&s.view.records, r).is_some()
    });
    let tabs = inspector_tabs_for(record, diff_available);
    let active = s
        .inspector_tab
        .filter(|t| tabs.contains(t))
        .unwrap_or_else(|| {
            if tabs.contains(&s.last_tab) {
                s.last_tab
            } else {
                // 源 setActiveTab(recent ?? tabs[0]):记忆页不在集合 → 首页
                tabs.first().copied().unwrap_or("summary")
            }
        });

    // 头(请求 = 圆点 + Request #N + Turn;记录 = kindTag + Turn · Step)
    let header: gpui_kit::AnyElement = match (target, record, request) {
        (InspectTarget::Request(n), _, Some(q)) => div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .size(px(5.))
                    .rounded_full()
                    .bg(if q.status == "error" {
                        theme::DANGER()
                    } else {
                        theme::BRAND()
                    }),
            )
            .child(
                div()
                    .font_family("Menlo")
                    .text_size(px(12.))
                    .text_color(theme::LABEL_2())
                    .child(format!("Request #{n}")),
            )
            .child(
                div()
                    .font_family("Menlo")
                    .text_size(px(11.))
                    .text_color(theme::CAPTION())
                    .child(format!("Turn {}", q.turn)),
            )
            .into_any_element(),
        (_, Some(r), _) => {
            let (fg, bg) = kind_colors(&r.kind);
            let location = match (&r.turn, r.group.as_str()) {
                (Some(t), g) if g.starts_with("Step") => format!("Turn {t} · {g}"),
                (Some(t), _) => format!("Turn {t} · Message"),
                _ => "Between turns".into(),
            };
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                    div()
                        .h(px(19.))
                        .flex()
                        .items_center()
                        .rounded(px(4.))
                        .bg(bg)
                        .px(px(5.))
                        .text_size(px(10.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(fg)
                        .child(kind_label(&r.kind).to_string()),
                )
                .child(
                    div()
                        .font_family("Menlo")
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child(location),
                )
                .into_any_element()
        }
        _ => div().into_any_element(),
    };

    let close_store = store.clone();
    let body: gpui_kit::AnyElement = match (record, request, active) {
        // ── 请求 ──
        (None, Some(q), "usage") => usage_body(q).into_any_element(),
        (None, Some(q), "timing") => request_timing_body(q).into_any_element(),
        (None, Some(q), _) => request_summary_body(store, s, q).into_any_element(),
        // ── 记录 ──
        (Some(r), _, "payload") => payload_body(store, s, r).into_any_element(),
        (Some(r), _, "result") => result_body(store, s, r).into_any_element(),
        (Some(r), _, "raw") => raw_body(store, s, r).into_any_element(),
        (Some(r), _, "preview") => match r.kind.as_str() {
            "message" => assistant_preview_body(store, s, r).into_any_element(),
            _ => preview_tab_body(r).into_any_element(),
        },
        (Some(r), _, "source") => source_tab_body(r).into_any_element(),
        (Some(r), _, "system") => system_body(r).into_any_element(),
        (Some(r), _, "tools") => tools_body(store, s, r).into_any_element(),
        (Some(r), _, "diff") => diff_body(s, r).into_any_element(),
        (Some(r), _, "schema") => schema_body(store, s, r).into_any_element(),
        (Some(r), _, "timing") => timing_body(r).into_any_element(),
        (Some(r), _, _) => summary_body(store, s, r).into_any_element(),
        // 目标数据已不在窗口(翻页/直播后):占位
        (None, None, _) => div().child(empty_text("Not available")).into_any_element(),
    };

    let resize_store = store.clone();
    Some(
        div()
            .id("trajectory-inspector")
            .relative()
            .flex_shrink_0()
            .v_flex()
            .min_h(px(0.))
            .w(px(s.inspector_width))
            .border_l_1()
            .border_color(theme::BORDER())
            .bg(theme::LAYER())
            .debug_selector(|| "trajectory-inspector".to_string())
            // 左缘拖宽把手(320..720;move/up 在 render 期窗口级注册)
            .child(
                div()
                    .id("inspector-resize")
                    .absolute()
                    .left(px(-4.))
                    .top_0()
                    .bottom_0()
                    .w(px(8.))
                    .cursor(CursorStyle::ResizeLeftRight)
                    .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        resize_store.update(cx, |st, cx| {
                            st.inspector_resize_begin(f32::from(ev.position.x), cx)
                        });
                    }),
            )
            .child(
                div()
                    .flex()
                    .h(px(42.))
                    .flex_shrink_0()
                    .items_center()
                    .px(px(12.))
                    .gap(px(4.))
                    .border_b_1()
                    .border_color(theme::BORDER())
                    .child(header)
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("inspector-close")
                            .flex()
                            .size(px(24.))
                            .items_center()
                            .justify_center()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_color(theme::LABEL_3())
                            .hover(|st| st.bg(theme::DOCK()))
                            .child(fixed(IconName::Close, 14.))
                            .on_click(move |_, _, cx| {
                                close_store.update(cx, |st, cx| st.close_inspector(cx));
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .h(px(34.))
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(2.))
                    .px(px(8.))
                    .border_b_1()
                    .border_color(theme::BORDER())
                    .children(tabs.iter().enumerate().map(|(ti, t)| {
                        let s2 = store.clone();
                        let name = *t;
                        let is_active = name == active;
                        div()
                            .id(("inspector-tab", ti))
                            .flex()
                            .h(px(24.))
                            .items_center()
                            .px(px(8.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .when(is_active, |el| {
                                el.bg(theme::GLASS_BG())
                                    .text_color(theme::LABEL())
                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                            })
                            .when(!is_active, |el| {
                                el.text_color(theme::LABEL_3())
                                    .hover(|st| st.text_color(theme::LABEL_2()))
                            })
                            .debug_selector(move || format!("inspector-tab-{name}"))
                            .child(tab_label(name).to_string())
                            .on_click(move |_, _, cx| {
                                s2.update(cx, |st, cx| st.set_inspector_tab(name, cx));
                            })
                    })),
            )
            .child(
                div()
                    .id("inspector-body")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .p(px(12.))
                    .child(body),
            ),
    )
}

fn tab_label(name: &str) -> &'static str {
    match name {
        "payload" => "Payload",
        "result" => "Result",
        "timing" => "Timing",
        "raw" => "Raw",
        "usage" => "Usage",
        "system" => "System Prompt",
        "preview" => "Preview",
        "source" => "Source",
        "tools" => "Tools",
        "diff" => "Diff",
        "schema" => "Schema",
        _ => "Summary",
    }
}

// ── 检查器主体(tab 内容)──────────────────────────────────────

/// 信息行(源 dl.overview:96px 标签列)
fn dl_row(label: &str, value: impl IntoElement) -> Div {
    div()
        .flex()
        .items_start()
        .gap(px(8.))
        .py(px(2.))
        .child(
            div()
                .w(px(96.))
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(theme::CAPTION())
                .child(label.to_string()),
        )
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .text_size(px(12.))
                .text_color(theme::LABEL_2())
                .line_height(gpui_kit::relative(1.5))
                .child(value),
        )
}

/// 小节标题(源 OverviewSection;Stateful 供调用方链 on_click)
fn section(id: &'static str, title: &str) -> gpui_kit::Stateful<Div> {
    let sel = id.to_string();
    // 源 overviewTitle:标题 + `>` 跳转箭头(点击进完整 tab;调用方挂 on_click)
    div()
        .id(id)
        .mt(px(6.))
        .mb(px(2.))
        .flex()
        .items_center()
        .gap(px(2.))
        .cursor_pointer()
        .text_size(px(11.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(theme::CAPTION())
        .hover(|st| st.text_color(theme::LABEL_3()))
        .child(title.to_string())
        .child(fixed(IconName::ChevronRight, 11.).text_color(theme::CAPTION()))
        .debug_selector(move || sel.clone())
}

/// 层级跳转链接(源 overviewHierarchyNavLink:文字 + 小箭头)
fn nav_link(id: &'static str, text: String) -> gpui_kit::Stateful<Div> {
    let sel = id.to_string();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(2.))
        .font_family("Menlo")
        .text_size(px(12.))
        .text_color(theme::BRAND())
        .cursor_pointer()
        .hover(|st| st.opacity(0.8))
        .child(text)
        .child(fixed(IconName::ChevronRight, 10.).text_color(theme::BRAND()))
        .debug_selector(move || sel.clone())
}

/// 等宽文本块(源 pre/code:max_h 内滚动)
fn mono_block(id: impl Into<gpui_kit::ElementId>, text: &str, color: Rgba) -> impl IntoElement {
    div()
        .id(id)
        .rounded(px(8.))
        .bg(theme::SIDEBAR())
        .p(px(10.))
        .text_size(px(12.))
        .text_color(color)
        .font_family("Menlo")
        .line_height(gpui_kit::relative(1.55))
        .child(text.to_string())
}

/// JSON 语法高亮 token 类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JKind {
    /// 对象键(`"…":`)
    Key,
    /// 字符串值
    Str,
    /// 数字
    Num,
    /// true / false / null
    Kw,
    /// 标点/缩进/其他
    Plain,
}

/// 单行 JSON 分词(键 = 字符串后紧跟冒号;转义引号不截断字符串)
fn json_tokens(line: &str) -> Vec<(JKind, String)> {
    fn flush(plain: &mut String, out: &mut Vec<(JKind, String)>) {
        if !plain.is_empty() {
            out.push((JKind::Plain, std::mem::take(plain)));
        }
    }
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            let start = i;
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' {
                    i += 2;
                    continue;
                }
                if chars[i] == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let i = i.min(chars.len());
            let s: String = chars[start..i].iter().collect();
            // 后续(跳空白)是冒号 → 键
            let mut j = i;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            let is_key = j < chars.len() && chars[j] == ':';
            flush(&mut plain, &mut out);
            out.push((if is_key { JKind::Key } else { JKind::Str }, s));
        } else if c.is_ascii_digit()
            || (c == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
        {
            let start = i;
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_digit() || matches!(chars[i], '.' | 'e' | 'E' | '+' | '-'))
            {
                i += 1;
            }
            flush(&mut plain, &mut out);
            out.push((JKind::Num, chars[start..i].iter().collect()));
        } else if c.is_ascii_alphabetic() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            let w: String = chars[start..i].iter().collect();
            flush(&mut plain, &mut out);
            let kind = match w.as_str() {
                "true" | "false" | "null" => JKind::Kw,
                _ => JKind::Plain,
            };
            out.push((kind, w));
        } else {
            plain.push(c);
            i += 1;
        }
    }
    flush(&mut plain, &mut out);
    out
}

fn jkind_color(kind: JKind) -> Rgba {
    match kind {
        JKind::Key => theme::BRAND(),
        JKind::Str => JSON_STRING(),
        JKind::Num => JSON_NUMBER(),
        JKind::Kw => ASSISTANT_VIOLET(),
        JKind::Plain => theme::LABEL_3(),
    }
}

// ── JSON 树(源 JsonTree)────────────────────────────────────────
//
// 顶层恒展开、子级默认折叠(展开集合在 store);可展开节点 = 箭头 +
// 键名 + 单行内联预览(object ≤4 项 / array ≤5 项 / 深度 ≤2,超出 …);
// 键名无引号;12px/16px code,token 分色同源暗色主题。行内 token 为
// 固有宽 flex 子项,长行溢出裁切不折行(同源单行语义)。

fn jt_span(color: Rgba, text: impl Into<String>) -> gpui_kit::AnyElement {
    div()
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

fn jt_row(depth: usize, children: Vec<gpui_kit::AnyElement>) -> gpui_kit::AnyElement {
    div()
        .flex()
        .items_start()
        .when(depth > 0, |el| el.pl(px(14. * depth as f32)))
        .children(children)
        .into_any_element()
}

fn json_brackets(v: &serde_json::Value) -> (&'static str, &'static str) {
    if v.is_array() { ("[", "]") } else { ("{", "}") }
}

fn json_entries(v: &serde_json::Value) -> Vec<(String, &serde_json::Value)> {
    match v {
        serde_json::Value::Object(m) => m.iter().map(|(k, val)| (k.clone(), val)).collect(),
        serde_json::Value::Array(a) => a
            .iter()
            .enumerate()
            .map(|(i, val)| (i.to_string(), val))
            .collect(),
        _ => Vec::new(),
    }
}

/// 源 pathId:数组下标 `n{i}`,字符串键 `s{len}:{key}`
fn jt_child_path(key_path: &str, key: &str, index: usize, is_array: bool) -> String {
    if is_array {
        format!("{key_path}/n{index}")
    } else {
        format!("{key_path}/s{}:{}", key.chars().count(), key)
    }
}

fn json_leaf_token(value: &serde_json::Value) -> (Rgba, String) {
    match value {
        serde_json::Value::String(s) => {
            (JSON_STRING(), serde_json::to_string(s).unwrap_or_default())
        }
        serde_json::Value::Number(n) => (JSON_NUMBER(), n.to_string()),
        serde_json::Value::Bool(b) => (JSON_NUMBER(), b.to_string()),
        serde_json::Value::Null => (JSON_NUMBER(), "null".into()),
        _ => (JSON_PUNCT(), value.to_string()),
    }
}

/// 源 previewValue:折叠态单行内联预览;键名取标点色(同源
/// previewProperty),深度 ≥2 的容器只显示 `{…}`
fn json_preview_tokens(value: &serde_json::Value, depth: usize) -> Vec<(Rgba, String)> {
    let mut out = Vec::new();
    match value {
        serde_json::Value::Object(m) => {
            out.push((JSON_PUNCT(), "{".into()));
            let limit = 4;
            if depth < 2 && !m.is_empty() {
                for (i, (k, v)) in m.iter().enumerate() {
                    if i >= limit {
                        out.push((JSON_PUNCT(), ", …".into()));
                        break;
                    }
                    if i > 0 {
                        out.push((JSON_PUNCT(), ", ".into()));
                    }
                    out.push((JSON_PUNCT(), format!("{k}: ")));
                    out.extend(json_preview_tokens(v, depth + 1));
                }
            } else if !m.is_empty() {
                out.push((JSON_PUNCT(), "…".into()));
            }
            out.push((JSON_PUNCT(), "}".into()));
        }
        serde_json::Value::Array(a) => {
            out.push((JSON_PUNCT(), "[".into()));
            let limit = 5;
            if depth < 2 && !a.is_empty() {
                for (i, v) in a.iter().enumerate() {
                    if i >= limit {
                        out.push((JSON_PUNCT(), ", …".into()));
                        break;
                    }
                    if i > 0 {
                        out.push((JSON_PUNCT(), ", ".into()));
                    }
                    out.extend(json_preview_tokens(v, depth + 1));
                }
            } else if !a.is_empty() {
                out.push((JSON_PUNCT(), "…".into()));
            }
            out.push((JSON_PUNCT(), "]".into()));
        }
        _ => {
            let (c, t) = json_leaf_token(value);
            out.push((c, t));
        }
    }
    out
}

/// JSON 树渲染上下文(store/记录索引随递归不变)
struct JtCtx<'a> {
    store: &'a Entity<AppStore>,
    s: &'a Snap,
    ix: u64,
}

/// 单个子树的全部行(header 行 + 展开时的 children)
fn json_tree_rows(
    ctx: &JtCtx,
    key_path: String,
    field: Option<&str>,
    value: &serde_json::Value,
    last: bool,
    depth: usize,
) -> Vec<gpui_kit::AnyElement> {
    let JtCtx { store, s, ix } = *ctx;
    let mut row: Vec<gpui_kit::AnyElement> = Vec::new();
    if let Some(f) = field {
        row.push(jt_span(JSON_PROPERTY(), format!("{f}:")));
    }
    match value {
        serde_json::Value::Object(_) | serde_json::Value::Array(_)
            if !json_entries(value).is_empty() => {}
        _ => {
            // 叶子 / 空容器
            if value.is_object() || value.is_array() {
                let (o, c) = json_brackets(value);
                row.push(jt_span(JSON_PUNCT(), format!("{o}{c}")));
            } else {
                let (c, t) = json_leaf_token(value);
                row.push(jt_span(c, t));
            }
            if !last {
                row.push(jt_span(JSON_PUNCT(), ","));
            }
            return vec![jt_row(depth, row)];
        }
    }

    let expanded = s.json_expanded.contains(&format!("{ix}/{key_path}"));
    let (o, c) = json_brackets(value);
    let s2 = store.clone();
    let k2 = key_path.clone();
    row.push(
        div()
            .id(format!("jt-{ix}-{key_path}"))
            .flex_shrink_0()
            .cursor_pointer()
            .text_color(JSON_EXPANDER())
            .child(fixed(
                if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
                10.,
            ))
            .on_click(move |_, _, cx| {
                let k = format!("{ix}/{k2}");
                s2.update(cx, |st, cx| st.toggle_json_node(&k, cx));
            })
            .into_any_element(),
    );
    row.push(jt_span(JSON_PUNCT(), o));
    for t in json_preview_tokens(value, 0) {
        row.push(jt_span(t.0, t.1));
    }
    row.push(jt_span(JSON_PUNCT(), c));
    if !last {
        row.push(jt_span(JSON_PUNCT(), ","));
    }
    let mut rows = vec![jt_row(depth, row)];
    if expanded {
        let is_array = value.is_array();
        let entries = json_entries(value);
        let n = entries.len();
        for (i, (k, v)) in entries.into_iter().enumerate() {
            let child_path = jt_child_path(&key_path, &k, i, is_array);
            rows.extend(json_tree_rows(
                ctx,
                child_path,
                Some(&k),
                v,
                i == n - 1,
                depth + 1,
            ));
        }
    }
    rows
}

/// JSON 树整体(顶层 `{` / children / `}`;调用方保证 object/array)
fn json_tree_block(store: &Entity<AppStore>, s: &Snap, ix: u64, value: &serde_json::Value) -> Div {
    let ctx = JtCtx { store, s, ix };
    let is_array = value.is_array();
    let entries = json_entries(value);
    let n = entries.len();
    let (open, close) = json_brackets(value);
    let mut rows: Vec<gpui_kit::AnyElement> = vec![jt_span(JSON_PUNCT(), open)];
    for (i, (k, v)) in entries.into_iter().enumerate() {
        let path = jt_child_path("", &k, i, is_array);
        rows.extend(json_tree_rows(&ctx, path, Some(&k), v, i == n - 1, 1));
    }
    rows.push(jt_span(JSON_PUNCT(), close));
    div()
        .v_flex()
        .text_size(px(12.))
        .font_family("Menlo")
        .line_height(gpui_kit::relative(1.33))
        .children(rows)
}

/// JSON 高亮块:逐行分色 token(pretty JSON 行短,行内不换行)。
/// 高度不封顶——检查器主体(inspector-body)整页滚,内容全高展开
fn json_block(id: &'static str, text: &str) -> impl IntoElement {
    div()
        .id(id)
        .rounded(px(8.))
        .bg(theme::SIDEBAR())
        .p(px(10.))
        .v_flex()
        .text_size(px(12.))
        .font_family("Menlo")
        .line_height(gpui_kit::relative(1.55))
        .children(text.lines().map(|line| {
            div().flex().children(
                json_tokens(line)
                    .into_iter()
                    .map(|(kind, s)| div().text_color(jkind_color(kind)).child(s))
                    .collect::<Vec<_>>(),
            )
        }))
}

/// 代码块选择:内容为 JSON 对象/数组 → 高亮;否则等宽纯文本
fn code_block(id: &'static str, text: &str, plain_color: Rgba) -> gpui_kit::AnyElement {
    let is_json = serde_json::from_str::<serde_json::Value>(text)
        .map(|v| v.is_object() || v.is_array())
        .unwrap_or(false);
    if is_json {
        json_block(id, text).into_any_element()
    } else {
        mono_block(id, text, plain_color).into_any_element()
    }
}

/// JSON 美化(失败原样)
fn pretty_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| s.to_string())
}

/// 记录所属请求:message 优先 request_number;工具按 turn + Step N 归属
/// (源 Hierarchy 的 Request #N 数据面)
fn owning_request<'a>(
    r: &TrajectoryRecord,
    requests: &'a [TrajectoryRequest],
) -> Option<&'a TrajectoryRequest> {
    if r.kind == "message"
        && let Some(n) = r.request_number
        && let Some(q) = requests.iter().find(|q| q.number == n)
    {
        return Some(q);
    }
    let step = r.group.strip_prefix("Step ")?.parse::<u64>().ok()?;
    requests
        .iter()
        .find(|q| q.turn == r.turn.unwrap_or(0) && q.step == step)
}

/// 工具的发起消息(同轮同步中、更早的 assistant message)
fn parent_message<'a>(
    r: &TrajectoryRecord,
    records: &'a [TrajectoryRecord],
) -> Option<&'a TrajectoryRecord> {
    records.iter().rfind(|m| {
        m.kind == "message" && m.turn == r.turn && m.group == r.group && m.index < r.index
    })
}

/// Summary tab(记录;源 dl.overview + overviewSections):
/// Hierarchy 跳转 → Status(工具含 Pending)→ Tokens/Duration →
/// Payload/Result 预览 → Request Timing / Timing 小节
fn summary_body(store: &Entity<AppStore>, s: &Snap, r: &TrajectoryRecord) -> Div {
    let requests = &s.view.requests;
    let req = owning_request(r, requests);
    let mut col = div().v_flex().gap(px(2.));

    // CONTEXT(源 isMarkdownRecord 的 overview:Source › / Status /
    // Duration + Preview 小节;Preview › 跳渲染页)
    if r.kind == "context" {
        if let Some(source) = &r.source {
            let s2 = store.clone();
            col = col.child(dl_row(
                "Source",
                nav_link("goto-source", message_source_label(source)).on_click(move |_, _, cx| {
                    s2.update(cx, |st, cx| st.set_inspector_tab("source", cx));
                }),
            ));
        }
        col = col.child(dl_row("Status", "Completed"));
        col = col.child(dl_row("Duration", fmt_ms(rec_total_ms(r).unwrap_or(0))));
        let s2 = store.clone();
        col = col
            .child(section("sec-preview", "Preview").on_click(move |_, _, cx| {
                s2.update(cx, |st, cx| st.set_inspector_tab("preview", cx));
            }))
            .child(context_markdown_body(
                r,
                &format!("traj-ctx-prev-{}", r.index),
            ));
        return col;
    }

    // Hierarchy:Request #N(所属请求)+ Assistant Message(工具发起消息;
    // 源另有 Tool Call 嵌套链接——无 SUBTOOL 数据,不渲染)
    let parent = if r.kind == "tool" {
        parent_message(r, &s.view.records)
    } else {
        None
    };
    if req.is_some() || parent.is_some() {
        let mut dd = div().v_flex().gap(px(2.));
        if let Some(q) = req {
            let s2 = store.clone();
            let n = q.number;
            dd = dd.child(nav_link("goto-request", format!("Request #{n}")).on_click(
                move |_, _, cx| {
                    s2.update(cx, |st, cx| st.select_trajectory_request(n, cx));
                },
            ));
        }
        if let Some(p) = parent {
            let s2 = store.clone();
            let ix = p.index;
            dd = dd.child(
                nav_link("goto-message", "Assistant Message".into()).on_click(move |_, _, cx| {
                    s2.update(cx, |st, cx| st.select_trajectory_record(ix, cx));
                }),
            );
        }
        // 源 dt:assistant request 目标在场 = "Source"(details.source),
        // 否则 "Hierarchy"
        let dt = if req.is_some() { "Source" } else { "Hierarchy" };
        col = col.child(dl_row(dt, dd.into_any_element()));
    }

    // Status(message;源 statusLabel:Failed 红 / Completed)
    if r.kind == "message" {
        let (label, color) = if r.is_error {
            ("Failed", Some(theme::DANGER()))
        } else {
            ("Completed", None)
        };
        col = col.child(dl_row(
            "Status",
            div()
                .when_some(color, |el, c| el.text_color(c))
                .child(label),
        ));
    }

    // Preview 小节(message;源 overviewSections 首节,内容 = rendered
    // preview 形态:Thinking 折叠 + 正文 + 工具调用行)
    if r.kind == "message" {
        let s2 = store.clone();
        col = col
            .child(section("sec-preview", "Preview").on_click(move |_, _, cx| {
                s2.update(cx, |st, cx| st.set_inspector_tab("preview", cx));
            }))
            .child(assistant_preview_body(store, s, r));
    }

    // Status(源 statusLabel:Failed 红 / Pending 无结果 / Completed)
    if r.kind == "tool" {
        let (label, color) = if r.is_error {
            ("Failed", Some(theme::DANGER()))
        } else if r.result.is_none() {
            ("Pending", Some(theme::WARN()))
        } else {
            ("Completed", None)
        };
        col = col.child(dl_row(
            "Status",
            div()
                .when_some(color, |el, c| el.text_color(c))
                .child(label),
        ));
    }

    // Tokens(message;源 TokenRows)
    if r.kind == "message" && r.output.is_some() {
        let out = r.output.unwrap_or(0);
        let think = r.think.unwrap_or(0);
        col = col
            .child(dl_row("Tokens", format!("{} tok", fmt_tok(out))))
            .child(dl_row("   Reasoning", fmt_tok(think)))
            .child(dl_row("   Content", fmt_tok(out.saturating_sub(think))));
    }
    // Duration(user)
    if r.kind == "user"
        && let Some(sec) = r.time_seconds
    {
        col = col.child(dl_row("Duration", fmt_ms((sec * 1000.) as i64)));
    }

    // Payload / Result / Schema 小节(非 markdown 记录专属——
    // 工具恒渲染,缺席显示缺省文案;message 走 Preview/Raw 不进这里,
    // 否则跳转会落进 tab 集不存在的孤页)
    if r.kind != "message" && (r.kind == "tool" || r.payload.is_some()) {
        let s2 = store.clone();
        col = col
            .child(section("sec-payload", "Payload").on_click(move |_, _, cx| {
                s2.update(cx, |st, cx| st.set_inspector_tab("payload", cx));
            }))
            .child(preview_block(store, s, r, "payload"));
    }
    if r.kind != "message" && (r.kind == "tool" || r.result.is_some() || r.output_detail.is_some())
    {
        let s2 = store.clone();
        col = col
            .child(section("sec-result", "Result").on_click(move |_, _, cx| {
                s2.update(cx, |st, cx| st.set_inspector_tab("result", cx));
            }))
            .child(preview_block(store, s, r, "result"));
    }
    if r.kind == "tool" {
        let s2 = store.clone();
        col = col
            .child(section("sec-schema", "Schema").on_click(move |_, _, cx| {
                s2.update(cx, |st, cx| st.set_inspector_tab("schema", cx));
            }))
            .child(preview_block(store, s, r, "schema"));
    }

    let total = rec_total_ms(r);

    // Request Timing 小节(所属请求存在;标题点击 → 请求检查器 Timing;
    // 内容 = 本记录计时:助手含生成指标,工具为 Started/Duration)
    if let Some(q) = req {
        let s2 = store.clone();
        let n = q.number;
        let timing_rows: Vec<Div> = if r.kind == "message" {
            let generation = match (r.ttft_ms, total) {
                (Some(t), Some(ms)) if ms > t => fmt_ms(ms - t),
                _ => "—".into(),
            };
            let throughput = match (r.output, total) {
                (Some(tok), Some(ms)) if ms > 0 => {
                    format!("{:.1} tok/s", tok as f64 / (ms as f64 / 1000.))
                }
                _ => "—".into(),
            };
            vec![
                dl_row(
                    "Started",
                    r.started_at.map(fmt_clock).unwrap_or_else(|| "—".into()),
                ),
                dl_row("Total", fmt_ms(q.duration_ms)),
                dl_row("TTFT", r.ttft_ms.map(fmt_ms).unwrap_or_else(|| "—".into())),
                dl_row("Generation", generation),
                dl_row("Throughput", throughput),
                dl_row("Timing source", timing_source(total.is_some())),
            ]
        } else {
            vec![
                dl_row(
                    "Started",
                    r.started_at.map(fmt_clock).unwrap_or_else(|| "—".into()),
                ),
                dl_row("Duration", total.map(fmt_ms).unwrap_or_else(|| "—".into())),
                dl_row("Timing source", timing_source(total.is_some())),
            ]
        };
        col = col
            .child(
                section("sec-req-timing", "Request Timing").on_click(move |_, _, cx| {
                    s2.update(cx, |st, cx| {
                        st.select_trajectory_request(n, cx);
                        st.set_inspector_tab("timing", cx);
                    });
                }),
            )
            .children(timing_rows);
    }

    // Timing 小节(工具;源 tool 专属 OverviewSection,三行同源 RecordTiming)
    if r.kind == "tool" {
        let s2 = store.clone();
        let timing_sec = section("sec-timing", "Timing").on_click(move |_, _, cx| {
            s2.update(cx, |st, cx| st.set_inspector_tab("timing", cx));
        });
        col = col
            .child(timing_sec)
            .child(dl_row(
                "Started",
                r.started_at.map(fmt_clock).unwrap_or_else(|| "—".into()),
            ))
            .child(dl_row(
                "Duration",
                total.map(fmt_ms).unwrap_or_else(|| "—".into()),
            ))
            .child(dl_row("Timing source", timing_source(total.is_some())));
    }
    col
}

/// 预览块(全文内部滚;标题 `>` 负责跳转,预览本身不抢点击)。
/// 缺席显示源缺省文案(No payload / No result / Schema unavailable);
/// JSON 容器内容走 JsonTree 紧凑形态(源 overviewSections preview)
fn preview_block(
    store: &Entity<AppStore>,
    s: &Snap,
    r: &TrajectoryRecord,
    tab: &'static str,
) -> impl IntoElement {
    let missing = match tab {
        "payload" => "No payload captured",
        "result" => "No result captured",
        _ => "Schema unavailable",
    };
    let text = match tab {
        // SYSTEM:快照在场时预览真实 prompt 头部(字符数行只是信封摘要)
        "payload" if r.kind == "system" => r
            .system_prompt
            .clone()
            .or_else(|| r.payload.clone())
            .unwrap_or_default(),
        "payload" => r.payload.clone().unwrap_or_default(),
        "result" => r
            .output_detail
            .clone()
            .or_else(|| r.result.clone())
            .unwrap_or_default(),
        _ => r.schema_detail.clone().unwrap_or_default(),
    };
    // Schema 小节:工具名 + 描述 + Parameters 树
    if tab == "schema"
        && let Some(spec) = serde_json::from_str::<serde_json::Value>(&text).ok()
        && spec.is_object()
    {
        let name = spec_name(&spec);
        let description = spec_field(&spec, "description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if let Some(params) = spec_field(&spec, "parameters") {
            let mut col = div().v_flex();
            if !name.is_empty() {
                col = col.child(
                    div()
                        .text_size(px(12.))
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(theme::LABEL())
                        .child(name),
                );
            }
            if !description.is_empty() {
                col = col.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::LABEL_2())
                        .line_height(gpui_kit::relative(1.5))
                        .child(description),
                );
            }
            col = col
                .child(
                    div()
                        .mt(px(4.))
                        .text_size(px(11.))
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(theme::CAPTION())
                        .child("Parameters"),
                )
                .child(json_tree_block(store, s, r.index, params));
            return div()
                .id(("sec-tree", r.index))
                .max_h(px(96.))
                .overflow_y_scroll()
                .child(col);
        }
    }
    let shown = if text.is_empty() {
        missing.to_string()
    } else {
        text
    };
    let s2 = store.clone();
    div()
        .id((tab, 1usize))
        .max_h(px(96.))
        .overflow_y_scroll()
        .rounded(px(8.))
        .bg(theme::SIDEBAR())
        .p(px(10.))
        .text_size(px(12.))
        .text_color(theme::LABEL_3())
        .font_family("Menlo")
        .line_height(gpui_kit::relative(1.55))
        .child(shown)
        .on_click(move |_, _, cx| {
            s2.update(cx, |st, cx| st.set_inspector_tab(tab, cx));
        })
}

/// Payload tab(源 RecordPayload input:JSON 容器 → JsonTree;否则原文等宽)
fn payload_body(store: &Entity<AppStore>, s: &Snap, r: &TrajectoryRecord) -> Div {
    match &r.payload {
        None => div().child(empty_text("No payload captured")),
        Some(p) => match serde_json::from_str::<serde_json::Value>(p) {
            Ok(v) if v.is_object() || v.is_array() => {
                div().child(json_tree_block(store, s, r.index, &v))
            }
            _ => div().child(code_block(
                "mono-payload",
                &pretty_json(p),
                theme::LABEL_3(),
            )),
        },
    }
}

/// Result tab(源 RecordPayload output:错误全套红;JSON 容器 → JsonTree)
fn result_body(store: &Entity<AppStore>, s: &Snap, r: &TrajectoryRecord) -> Div {
    let color = if r.is_error {
        theme::DANGER()
    } else {
        theme::LABEL_3()
    };
    let text = match (&r.output_detail, &r.result) {
        (None, None) => {
            return div().child(empty_text("No result captured"));
        }
        (Some(d), _) => d,
        (None, Some(s)) => s,
    };
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) if v.is_object() || v.is_array() => {
            div().child(json_tree_block(store, s, r.index, &v))
        }
        _ => div().child(code_block("mono-result", text, color)),
    }
}

/// Raw tab(ASSISTANT:Thinking 折叠 + 输出全文)
fn raw_body(store: &Entity<AppStore>, s: &Snap, r: &TrajectoryRecord) -> Div {
    let mut col = div().v_flex().gap(px(8.));
    // CONTEXT(源 MarkdownRecordContent !rendered → SourceBlocks:注入
    // 文本为单 text 块,「Block #1 text」头 + 等宽原文)
    if r.kind == "context" {
        return col.child(context_source_block(r));
    }
    // MESSAGE(源 SourceBlocks:thinking/text/tool-call 连续编号)
    if r.kind == "message" {
        return assistant_source_blocks(store, s, r);
    }
    if let Some(t) = &r.thinking_detail {
        let open = s.raw_thinking;
        let s2 = store.clone();
        let t2 = t.clone();
        col = col.child(
            div()
                .id("inspector-thinking")
                .v_flex()
                .gap(px(4.))
                .border_l_2()
                .border_color(theme::BORDER())
                .pl(px(10.))
                .child(
                    div()
                        .id("inspector-thinking-toggle")
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .cursor_pointer()
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .hover(|st| st.text_color(theme::LABEL_3()))
                        .child("Thinking")
                        .child(fixed(
                            if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            },
                            12.,
                        ))
                        .on_click(move |_, _, cx| {
                            s2.update(cx, |st, cx| st.toggle_inspector_thinking(cx));
                        }),
                )
                .when(open, move |el| {
                    el.child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::LABEL_3())
                            .line_height(gpui_kit::relative(1.55))
                            .child(t2.clone()),
                    )
                }),
        );
    }
    match &r.output_detail {
        Some(o) => col = col.child(mono_block("mono-raw", o, theme::LABEL_3())),
        None => col = col.child(empty_text("Not available")),
    }
    col
}

/// Timing tab(记录)
fn timing_body(r: &TrajectoryRecord) -> Div {
    let total = rec_total_ms(r);
    let mut col = div().v_flex().gap(px(2.));
    col = col.child(dl_row(
        "Started",
        r.started_at.map(fmt_clock).unwrap_or_else(|| "—".into()),
    ));
    col = col.child(dl_row(
        "Duration",
        total.map(fmt_ms).unwrap_or_else(|| "—".into()),
    ));
    if r.kind == "message" {
        col = col.child(dl_row(
            "TTFT",
            r.ttft_ms.map(fmt_ms).unwrap_or_else(|| "—".into()),
        ));
        col = col.child(dl_row(
            "Generation",
            match (r.ttft_ms, total) {
                (Some(t), Some(ms)) if ms > t => fmt_ms(ms - t),
                _ => "—".into(),
            },
        ));
    }
    col.child(dl_row("Timing source", timing_source(total.is_some())))
}

/// Summary tab(请求;源 request overview:Status/Provider/Model/Tool calls/
/// …/Result 跳转行——链到该请求产出的助手消息或压缩记录)
fn request_summary_body(store: &Entity<AppStore>, s: &Snap, q: &TrajectoryRequest) -> Div {
    let mut col = div().v_flex().gap(px(2.));
    col = col.child(dl_row(
        "Status",
        if q.status == "error" {
            div().text_color(theme::DANGER()).child("Failed")
        } else {
            div().child("Complete")
        },
    ));
    col = col.child(dl_row("Provider", q.provider.clone()));
    col = col.child(dl_row("Model", q.model.clone()));
    col = col.child(dl_row("Tool calls", fmt_tok(q.tool_calls)));
    if let Some(e) = &q.reasoning_effort {
        col = col.child(dl_row("Reasoning", e.clone()));
    }
    col = col.child(dl_row("Started", fmt_clock(q.started_at)));
    // Result:该请求产出的记录(message/compacted),`>` 跳转其 Summary
    // (源 overviewHierarchyNavLink:Assistant Message / Compacted)
    if let Some(res) = s.view.records.iter().find(|r| {
        r.request_number == Some(q.number) && matches!(r.kind.as_str(), "message" | "compacted")
    }) {
        let label = if res.kind == "compacted" {
            "Compacted"
        } else {
            "Assistant Message"
        };
        let s2 = store.clone();
        let ix = res.index;
        col = col.child(dl_row(
            "Result",
            nav_link("goto-req-result", label.into()).on_click(move |_, _, cx| {
                s2.update(cx, |st, cx| st.select_trajectory_record(ix, cx));
            }),
        ));
    }
    col
}

/// Usage tab(请求:This request / Session cumulative 两组五桶)
fn usage_body(q: &TrajectoryRequest) -> Div {
    let mut col = div().v_flex().gap(px(2.));
    match &q.usage {
        None => col = col.child(empty_text("Usage not reported")),
        Some(u) => col = col.child(usage_group("This request", u)),
    }
    col.child(section("sec-cumulative", "Session cumulative"))
        .child(usage_group("", &q.cumulative))
}

/// 用量组(Input/Cached/Other/Output/Reasoning/Content)
fn usage_group(title: &str, u: &TrajectoryUsage) -> Div {
    div()
        .v_flex()
        .when(!title.is_empty(), |el| {
            el.child(
                div()
                    .mb(px(2.))
                    .text_size(px(11.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(theme::CAPTION())
                    .child(title.to_string()),
            )
        })
        .child(dl_row("Input", format!("{} tok", fmt_tok(u.input))))
        .child(dl_row("   Cached", fmt_tok(u.cached)))
        .child(dl_row("   Other", fmt_tok(u.other)))
        .child(dl_row("Output", format!("{} tok", fmt_tok(u.output))))
        .child(dl_row("   Reasoning", fmt_tok(u.reasoning)))
        .child(dl_row("   Content", fmt_tok(u.content())))
}

/// Timing tab(请求)
fn request_timing_body(q: &TrajectoryRequest) -> Div {
    div()
        .v_flex()
        .gap(px(2.))
        .child(dl_row("Started", fmt_clock(q.started_at)))
        .child(dl_row(
            "Completed",
            if q.completed_at > 0 {
                fmt_clock(q.completed_at)
            } else {
                "—".into()
            },
        ))
        .child(dl_row("Total", fmt_ms(q.duration_ms)))
        .child(dl_row(
            "TTFT",
            q.ttft_ms.map(fmt_ms).unwrap_or_else(|| "—".into()),
        ))
}

/// 缺失文案(源 .noPayload)
fn empty_text(text: &str) -> Div {
    div()
        .py(px(14.))
        .text_size(px(12.))
        .text_color(theme::LABEL_3())
        .child(text.to_string())
}

/// 记录总时长 ms
fn rec_total_ms(r: &TrajectoryRecord) -> Option<i64> {
    r.time_seconds.map(|s| (s * 1000.) as i64)
}

/// Source 标签(源 messageSourceLabel:kind 特例 + 首字母大写兜底,en)
fn message_source_label(source: &serde_json::Value) -> String {
    let kind = source["kind"].as_str().unwrap_or_default();
    match kind {
        "user" => "User".into(),
        "plugin" => match source["plugin"].as_str() {
            Some(p) if !p.is_empty() => format!("Plugin · {p}"),
            _ => "Plugin".into(),
        },
        "goal" => match source["round"].as_u64() {
            Some(round) if round > 0 => format!("Goal · Round {round}"),
            _ => "Goal".into(),
        },
        "" => "Unknown".into(),
        other => format!("{}{}", other[..1].to_uppercase(), &other[1..]),
    }
}

// ── ASSISTANT(message)详情(Summary / Preview / Raw,源 isMarkdownRecord)──

/// 本消息(同 turn + Step)发起的工具调用记录(源 sourceBlocks 的
/// tool-call 块数据面——RS 由记录分组反查,不另存块)
fn step_tool_calls<'a>(
    r: &TrajectoryRecord,
    records: &'a [TrajectoryRecord],
) -> Vec<&'a TrajectoryRecord> {
    records
        .iter()
        .filter(|t| t.kind == "tool" && t.turn == r.turn && t.group == r.group)
        .collect()
}

/// 工具调用行(源 RecordListText 单行形态:扳手 + name + 空格 + args
/// 同行截断——args 取 text 的紧凑段,payload 是 pretty 多行 JSON 不可用;
/// 400 12px Menlo/label-primary, payloads label-secondary。点击跳工具记录)
fn assistant_tool_call_row(store: &Entity<AppStore>, call: &TrajectoryRecord) -> impl IntoElement {
    let (name, args) = match call.text.split_once(' ') {
        Some((n, a)) => (n, a),
        None => (call.text.as_str(), ""),
    };
    let s2 = store.clone();
    let ix = call.index;
    div()
        .id(("assistant-call", ix))
        .flex()
        .items_center()
        .gap(px(5.))
        .min_w(px(0.))
        .py(px(1.))
        .cursor_pointer()
        .hover(|st| st.opacity(0.8))
        .child(fixed(DshIcon::Wrench, 12.))
        .child(
            div()
                .flex_shrink_0()
                .font_family("Menlo")
                .text_size(px(12.))
                .text_color(theme::LABEL_2())
                .child(name.to_string()),
        )
        .child(
            div()
                .min_w(px(0.))
                .font_family("Menlo")
                .text_size(px(12.))
                .text_color(theme::LABEL_3())
                .truncate()
                .child(args.to_string()),
        )
        .on_click(move |_, _, cx| {
            s2.update(cx, |st, cx| st.select_trajectory_record(ix, cx));
        })
        .debug_selector(move || format!("assistant-call-{ix}"))
}

/// Preview 页(Summary 小节同款):Thinking 折叠(默认收)+ 正文
/// markdown + 工具调用行(源 MarkdownRecordContent rendered)
fn assistant_preview_body(store: &Entity<AppStore>, s: &Snap, r: &TrajectoryRecord) -> Div {
    let mut col = div()
        .debug_selector(move || format!("traj-preview-{}", r.index))
        .v_flex();
    if let Some(t) = &r.thinking_detail {
        let open = s.raw_thinking;
        let s2 = store.clone();
        let t2 = t.clone();
        col = col.child(
            div()
                .id("assistant-preview-thinking")
                .v_flex()
                .border_l_2()
                .border_color(theme::BORDER())
                .pl(px(10.))
                .child(
                    div()
                        .id("assistant-preview-thinking-toggle")
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(theme::LABEL_2())
                        .child("Thinking")
                        .child(fixed(
                            if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            },
                            12.,
                        ))
                        .on_click(move |_, _, cx| {
                            s2.update(cx, |st, cx| st.toggle_inspector_thinking(cx));
                        }),
                )
                .when(open, move |el| {
                    el.child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::LABEL_2())
                            .line_height(gpui_kit::relative(1.5))
                            .child(t2.clone()),
                    )
                }),
        );
    }
    if let Some(o) = &r.output_detail {
        col = col.child(div().mt(px(6.)).child(crate::kits::markdown::render(
            &format!("traj-preview-{}", r.index),
            o,
        )));
    }
    if r.output_detail.is_none() && r.thinking_detail.is_none() {
        col = col.child(empty_text("No content"));
    }
    for c in step_tool_calls(r, &s.view.records) {
        col = col.child(assistant_tool_call_row(store, c));
    }
    col
}

/// Raw 块头(源 sourceBlockLabel:「Block #N type」;tool-call 带 › 跳
/// 工具记录,同源 onOpenCall)
fn source_block_header(
    store: &Entity<AppStore>,
    n: usize,
    kind: &'static str,
    jump: Option<u64>,
) -> gpui_kit::AnyElement {
    let label = format!("Block #{n} {kind}");
    match jump {
        Some(ix) => {
            let s2 = store.clone();
            div()
                .id(("block-jump", ix))
                .flex()
                .items_center()
                .gap(px(2.))
                .cursor_pointer()
                .hover(|st| st.opacity(0.8))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child(label),
                )
                .child(fixed(IconName::ChevronRight, 12.))
                .on_click(move |_, _, cx| {
                    s2.update(cx, |st, cx| st.select_trajectory_record(ix, cx));
                })
                .debug_selector(move || format!("block-jump-{ix}"))
                .into_any_element()
        }
        None => div()
            .text_size(px(11.))
            .text_color(theme::CAPTION())
            .child(label)
            .into_any_element(),
    }
}

/// Raw 页块形态(源 SourceBlocks:thinking / text / tool-call 按模型
/// 输出序连续编号;RS 块序 = reasoning → 正文 → 同步工具调用)
fn assistant_source_blocks(store: &Entity<AppStore>, s: &Snap, r: &TrajectoryRecord) -> Div {
    let mut col = div().v_flex().gap(px(6.));
    let mut n = 0usize;
    if let Some(t) = &r.thinking_detail {
        n += 1;
        col = col
            .child(source_block_header(store, n, "thinking", None))
            .child(mono_block(("mono-block", n), t, theme::LABEL()));
    }
    if let Some(o) = &r.output_detail {
        n += 1;
        col = col
            .child(source_block_header(store, n, "text", None))
            .child(mono_block(("mono-block", n), o, theme::LABEL()));
    }
    if n == 0 {
        n += 1;
        col = col
            .child(source_block_header(store, n, "text", None))
            .child(mono_block(("mono-block", n), &r.text, theme::LABEL()));
    }
    for c in step_tool_calls(r, &s.view.records) {
        n += 1;
        col = col
            .child(source_block_header(store, n, "tool-call", Some(c.index)))
            .child(mono_block(
                ("mono-block", n),
                c.payload.as_deref().unwrap_or_default(),
                theme::LABEL(),
            ));
    }
    col
}

/// Summary 的注入文本预览 + Preview tab(源 MarkdownRecordContent
/// rendered 形态:markdown 渲染;Summary 内为 preview 紧凑形)
fn context_markdown_body(r: &TrajectoryRecord, key: &str) -> Div {
    let mut col = div().v_flex();
    match r.payload.as_deref() {
        Some(text) if !text.is_empty() => {
            col = col.child(crate::kits::markdown::render(key, text));
        }
        _ => col = col.child(empty_text("No content")),
    }
    col
}

/// Preview tab(完整渲染)
fn preview_tab_body(r: &TrajectoryRecord) -> Div {
    let key = format!("traj-preview-{}", r.index);
    let mut col = div().debug_selector(move || key.clone()).v_flex();
    match r.payload.as_deref() {
        Some(text) if !text.is_empty() => {
            col = col.child(crate::kits::markdown::render(
                &format!("traj-preview-{}", r.index),
                text,
            ));
        }
        _ => col = col.child(empty_text("No content")),
    }
    col
}

/// Raw tab 的块形态(源 SourceBlocks:text 块 = 「Block #1 text」头 + 原文)
fn context_source_block(r: &TrajectoryRecord) -> Div {
    let mut col = div().v_flex();
    let Some(text) = r.payload.as_deref() else {
        return col.child(empty_text("No content"));
    };
    col = col
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme::CAPTION())
                .pb(px(4.))
                .child("Block #1 text"),
        )
        .child(mono_block("mono-context-raw", text, theme::LABEL()));
    col
}

/// Source tab(源 MessageSource:染色对象的 JSON 树;RS 无树组件,
/// 以「Message JSON」标签 + 高亮块呈现,内容一致)
fn source_tab_body(r: &TrajectoryRecord) -> Div {
    let mut col = div().v_flex();
    let Some(source) = &r.source else {
        return col.child(empty_text("Source not recorded"));
    };
    col = col
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme::CAPTION())
                .pb(px(4.))
                .child("Message JSON"),
        )
        .child(
            div().child(
                json_block(
                    "mono-context-source",
                    &serde_json::to_string_pretty(source).unwrap_or_default(),
                )
                .into_any_element(),
            ),
        );
    col
}

// ── SYSTEM 详情(System Prompt / Tools / Diff)与 TOOL Schema ────

/// System Prompt 页:Markdown 渲染(空则缺省文案)
fn system_body(r: &TrajectoryRecord) -> Div {
    let mut col = div().v_flex();
    match &r.system_prompt {
        Some(p) if !p.is_empty() => {
            let key = format!("traj-sys-{}", r.index);
            col = col.child(crate::kits::markdown::render(&key, p));
        }
        _ => col = col.child(empty_text("No system prompt in this request")),
    }
    col
}

/// 工具 spec 取字段(OpenAI function 包裹优先,扁平兜底;非 null 即在场)
fn spec_field<'a>(t: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    let v = &t["function"][key];
    if !v.is_null() {
        return Some(v);
    }
    let v = &t[key];
    (!v.is_null()).then_some(v)
}

/// spec 名(目录卡标识 / schema 头)
fn spec_name(t: &serde_json::Value) -> String {
    spec_field(t, "name")
        .and_then(|v| v.as_str())
        .unwrap_or("(unnamed)")
        .to_string()
}

/// Tools 页:工具目录(源 ToolCatalog:扁平行 + 底部分隔线;折叠行 =
/// chevron + 图标 + mono 名称 + 内联灰描述单行截断;展开 = 完整描述 +
/// 参数 JSON)
fn tools_body(store: &Entity<AppStore>, s: &Snap, r: &TrajectoryRecord) -> Div {
    let mut col = div().v_flex();
    let Some(catalog) = &r.tools_catalog else {
        return col.child(empty_text("No tools in this request"));
    };
    if catalog.is_empty() {
        return col.child(empty_text("No tools in this request"));
    }
    for (ti, t) in catalog.iter().enumerate() {
        let name = spec_name(t);
        let description = spec_field(t, "description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let parameters = spec_field(t, "parameters").cloned();
        let open = s.expanded_tools.contains(&name);
        let s2 = store.clone();
        let toggle_name = name.clone();
        // 折叠行(源 toolCatalogSummary:12px chevron 列 + 12px 图标列 +
        // max-content 名称 + minmax(0,1fr) 描述;min-h 30,padding 4/12)
        let header = div()
            .flex()
            .items_center()
            .min_h(px(30.))
            .px(px(12.))
            .py(px(4.))
            .gap(px(5.))
            .hover(|st| st.bg(theme::DOCK()))
            .child(fixed(
                if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
                12.,
            ))
            .child(crate::kits::icons::tool_icon(&name))
            .child(
                div()
                    .font_family("Menlo")
                    .text_size(px(12.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(theme::LABEL())
                    .child(name.clone()),
            )
            .child(
                div()
                    .min_w(px(0.))
                    .flex_1()
                    .text_size(px(12.))
                    .text_color(theme::LABEL_3())
                    .truncate()
                    .child(description.clone()),
            );
        // 条目(源 details.toolCatalogItem:行 + 展开体,底部分隔线;
        // 点击只绑折叠行——展开体内点击不收起)
        let mut item = div()
            .v_flex()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme::BORDER())
            .child(
                div()
                    .id(("inspector-tool", ti))
                    .cursor_pointer()
                    .child(header)
                    .on_click(move |_, _, cx| {
                        s2.update(cx, |st, cx| st.toggle_inspector_tool(&toggle_name, cx));
                    }),
            );
        // 展开体(源 toolCatalogDefinition:左缩进对齐名称列 29px)
        if open {
            if !description.is_empty() {
                item = item.child(
                    div()
                        .pl(px(29.))
                        .pt(px(6.))
                        .pb(px(4.))
                        .pr(px(14.))
                        .text_size(px(12.))
                        .text_color(theme::LABEL_2())
                        .line_height(gpui_kit::relative(1.5))
                        .child(description.clone()),
                );
            }
            if let Some(p) = parameters {
                item = item.child(
                    div()
                        .v_flex()
                        .pl(px(29.))
                        .pb(px(8.))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme::CAPTION())
                                .mb(px(4.))
                                .child(format!("{name} parameters JSON")),
                        )
                        .child(
                            div().pr(px(6.)).child(
                                json_block(
                                    "mono-tool-params",
                                    &serde_json::to_string_pretty(&p).unwrap_or_default(),
                                )
                                .into_any_element(),
                            ),
                        ),
                );
            }
        }
        col = col.child(item);
    }
    col
}

/// Diff 行操作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffOp {
    Same,
    Add,
    Del,
}

/// 行级 diff(LCS;系统提示/工具目录均为百行内,DP 足够)
fn line_diff(a: &[&str], b: &[&str]) -> Vec<(DiffOp, String)> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((DiffOp::Same, a[i].to_string()));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push((DiffOp::Del, a[i].to_string()));
            i += 1;
        } else {
            out.push((DiffOp::Add, b[j].to_string()));
            j += 1;
        }
    }
    while i < n {
        out.push((DiffOp::Del, a[i].to_string()));
        i += 1;
    }
    while j < m {
        out.push((DiffOp::Add, b[j].to_string()));
        j += 1;
    }
    out
}

/// Diff 页:对照前一 SYSTEM 快照,分 System Prompt / Tools 两节
/// (源 SystemPromptDiff:structuredPatch 语义的行级近似)
fn diff_body(s: &Snap, r: &TrajectoryRecord) -> Div {
    let prev = previous_system_snapshot(&s.view.records, r);
    let mut col = div().v_flex().gap(px(8.));
    let Some(prev) = prev else {
        return col.child(empty_text("Not available"));
    };
    let mut has_diff = false;
    // System Prompt 节
    if let (Some(a), Some(b)) = (prev.system_prompt.as_deref(), r.system_prompt.as_deref())
        && a != b
    {
        has_diff = true;
        col = col
            .child(section("sec-diff-system", "System Prompt"))
            .child(diff_block("diff-system", a, b));
    }
    // Tools 节(目录 pretty 序列化后行 diff)
    if let (Some(a), Some(b)) = (&prev.tools_catalog, &r.tools_catalog)
        && a != b
    {
        has_diff = true;
        let pa = serde_json::to_string_pretty(a).unwrap_or_default();
        let pb = serde_json::to_string_pretty(b).unwrap_or_default();
        col = col
            .child(section("sec-diff-tools", "Tools"))
            .child(diff_block("diff-tools", &pa, &pb));
    }
    if !has_diff {
        col = col.child(empty_text("Not available"));
    }
    col
}

/// diff 渲染块(加绿/删红/同灰;等宽 11px)
fn diff_block(id: &'static str, a: &str, b: &str) -> impl IntoElement {
    let la: Vec<&str> = a.lines().collect();
    let lb: Vec<&str> = b.lines().collect();
    div()
        .id(id)
        .max_h(px(360.))
        .overflow_y_scroll()
        .rounded(px(8.))
        .bg(theme::SIDEBAR())
        .py(px(6.))
        .v_flex()
        .font_family("Menlo")
        .text_size(px(11.))
        .line_height(gpui_kit::relative(1.5))
        .children(line_diff(&la, &lb).into_iter().map(|(op, line)| {
            let (prefix, color, bg) = match op {
                DiffOp::Add => (
                    "+ ",
                    theme::SUCCESS(),
                    gpui_kit::Rgba {
                        a: 0.10,
                        ..theme::SUCCESS()
                    },
                ),
                DiffOp::Del => (
                    "- ",
                    theme::DANGER(),
                    gpui_kit::Rgba {
                        a: 0.10,
                        ..theme::DANGER()
                    },
                ),
                DiffOp::Same => ("  ", theme::LABEL_3(), theme::TRANSPARENT()),
            };
            div()
                .flex()
                .px(px(8.))
                .bg(bg)
                .text_color(color)
                .child(format!("{prefix}{line}"))
        }))
}

/// Schema 页(TOOL):name + description + Parameters(高亮)
fn schema_body(store: &Entity<AppStore>, s: &Snap, r: &TrajectoryRecord) -> Div {
    let Some(raw) = &r.schema_detail else {
        return div().v_flex().child(empty_text("Schema unavailable"));
    };
    let Ok(spec) = serde_json::from_str::<serde_json::Value>(raw) else {
        return div()
            .v_flex()
            .child(mono_block("mono-schema", raw, theme::LABEL_3()));
    };
    let name = spec_name(&spec);
    let description = spec_field(&spec, "description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let parameters = spec_field(&spec, "parameters").cloned();
    let mut col = div().v_flex().gap(px(6.));
    if !name.is_empty() {
        col = col.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(crate::kits::icons::tool_icon(&name))
                .child(
                    div()
                        .font_family("Menlo")
                        .text_size(px(13.))
                        .text_color(theme::LABEL_2())
                        .child(name),
                ),
        );
    }
    if !description.is_empty() {
        col = col.child(
            div()
                .text_size(px(12.))
                .text_color(theme::LABEL_3())
                .line_height(gpui_kit::relative(1.5))
                .child(description),
        );
    }
    match parameters {
        Some(p) if p.is_object() || p.is_array() => {
            col = col
                .child(section("sec-schema-params", "Parameters"))
                .child(json_tree_block(store, s, r.index, &p))
        }
        Some(p) => {
            let pretty = serde_json::to_string_pretty(&p).unwrap_or_default();
            col = col
                .child(section("sec-schema-params", "Parameters"))
                .child(code_block("mono-schema-params", &pretty, theme::LABEL_3()))
        }
        None => col = col.child(empty_text("Schema unavailable")),
    }
    col
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(index: u64, kind: &str, turn: Option<u64>, group: &str) -> TrajectoryRecord {
        TrajectoryRecord {
            index,
            seq: index,
            kind: kind.into(),
            turn,
            group: group.into(),
            turn_start: false,
            text: "x".into(),
            result: None,
            is_error: false,
            time_seconds: None,
            started_at: Some(1000 + index as i64 * 100),
            request_number: None,
            input: None,
            output: None,
            think: None,
            ttft_ms: None,
            payload: None,
            output_detail: None,
            thinking_detail: None,
            system_prompt: None,
            tools_catalog: None,
            schema_detail: None,
            source: None,
        }
    }

    fn collapse() -> CollapseState {
        CollapseState {
            all_turns: false,
            turns: HashSet::new(),
            all_calls: false,
            calls: HashSet::new(),
        }
    }

    /// 搜索 AND 分词 + 字段命中
    #[test]
    fn search_tokens_and_fields() {
        let mut r = rec(1, "tool", Some(1), "Step 1");
        r.text = "bash ls -la".into();
        r.result = Some("file-a".into());
        assert!(search_hit(&r, "bash"));
        assert!(search_hit(&r, "BASH file"));
        assert!(search_hit(&r, "turn 1"));
        assert!(!search_hit(&r, "bash missing"));
        // 空查询 = 全过
        assert!(search_hit(&r, "  "));
    }

    /// turn 折叠:保留首条 + 摘要行;步数/工具数正确
    #[test]
    fn turn_collapse_keeps_first_and_summarizes() {
        let records = vec![
            rec(1, "user", Some(1), "Message"),
            rec(2, "message", Some(1), "Step 1"),
            rec(3, "tool", Some(1), "Step 1"),
            rec(4, "tool", Some(1), "Step 1"),
            rec(5, "user", Some(2), "Message"),
        ];
        let mut c = collapse();
        c.turns.insert(1);
        let rows = build_rows(&records, &[true; 5], &c);
        let kinds: Vec<String> = rows
            .iter()
            .map(|r| match r {
                LedgerRow::Record { rec, .. } => rec.index.to_string(),
                LedgerRow::TurnSummary { turn, .. } => format!("sum:{turn}"),
                LedgerRow::CallSummary { .. } => "call".into(),
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["1", "sum:1", "5"],
            "turn1 首条+摘要,turn2 不折叠"
        );
        if let LedgerRow::TurnSummary { steps, tools, .. } = &rows[1] {
            assert_eq!((*steps, *tools), (3, 2));
        } else {
            panic!("第二行应为 TurnSummary");
        }
    }

    /// calls 折叠:assistant 步内连续工具行并入摘要(同组界止)
    #[test]
    fn call_collapse_groups_tool_run() {
        let records = vec![
            rec(1, "user", Some(1), "Message"),
            rec(2, "message", Some(1), "Step 1"),
            rec(3, "tool", Some(1), "Step 1"),
            rec(4, "tool", Some(1), "Step 1"),
            rec(5, "message", Some(1), "Step 2"),
        ];
        let mut c = collapse();
        c.calls.insert(2);
        let rows = build_rows(&records, &[true; 5], &c);
        let shape: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                LedgerRow::Record { rec, .. } => Some(match rec.kind.as_str() {
                    "tool" => "tool",
                    _ => "rec",
                }),
                LedgerRow::CallSummary { .. } => Some("call"),
                LedgerRow::TurnSummary { .. } => None,
            })
            .collect();
        assert_eq!(shape, vec!["rec", "rec", "call", "rec"]);
    }

    /// 时间线等宽投影 + TTFT 分界
    #[test]
    fn spans_sequence_and_ttft() {
        let mut records = vec![
            rec(1, "system", None, "Message"),
            rec(2, "user", Some(1), "Message"),
        ];
        let spans = build_spans(&records, false);
        assert_eq!(spans.len(), 2);
        assert!((spans[0].x1 - spans[1].x0).abs() < 1e-9);
        let mut m = rec(3, "message", Some(1), "Step 1");
        m.ttft_ms = Some(300);
        m.time_seconds = Some(1.5); // 1500ms
        records.push(m);
        let spans = build_spans(&records, false);
        let split = spans[2].ttft_split.expect("分界存在");
        assert!((split - 0.2).abs() < 1e-9, "300/1500");
        assert!(spans[0].ttft_split.is_none());
    }

    /// duration 模式:按耗时累计,零总时退化等宽
    #[test]
    fn spans_duration_mode() {
        let mut a = rec(1, "tool", Some(1), "Step 1");
        a.time_seconds = Some(1.0);
        let mut b = rec(2, "tool", Some(1), "Step 1");
        b.time_seconds = Some(3.0);
        let spans = build_spans(&[a, b], true);
        assert!((spans[0].x0 - 0.).abs() < 1e-9);
        assert!((spans[0].x1 - 0.25).abs() < 1e-9, "1s/4s = 0.25");
        assert!((spans[1].x1 - 1.).abs() < 1e-9);
        // 全零:退化
        let z = vec![
            rec(1, "tool", Some(1), "Step 1"),
            rec(2, "tool", Some(1), "Step 1"),
        ];
        let spans = build_spans(&z, true);
        assert!((spans[0].x1 - 0.5).abs() < 1e-9);
    }

    /// 千分位
    #[test]
    fn thousands_groups() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1542), "1,542");
        assert_eq!(thousands(1200000), "1,200,000");
        assert_eq!(thousands(-1542), "-1,542");
    }

    /// 行 diff:公共行保持、增删配对
    #[test]
    fn line_diff_ops() {
        let a = vec!["l1", "l2", "l3"];
        let b = vec!["l1", "x", "l3"];
        let d = line_diff(&a, &b);
        assert_eq!(
            d,
            vec![
                (DiffOp::Same, "l1".into()),
                (DiffOp::Del, "l2".into()),
                (DiffOp::Add, "x".into()),
                (DiffOp::Same, "l3".into()),
            ]
        );
        // 纯追加
        let d = line_diff(&["a"], &["a", "b"]);
        assert_eq!(
            d,
            vec![(DiffOp::Same, "a".into()), (DiffOp::Add, "b".into())]
        );
        // 空侧
        assert_eq!(line_diff(&[], &["z"]), vec![(DiffOp::Add, "z".into())]);
    }

    /// spec 字段提取:OpenAI function 包裹优先,扁平兜底,对象值不漏
    #[test]
    fn spec_field_shapes() {
        let wrapped = serde_json::json!({
            "type": "function",
            "function": { "name": "bash", "description": "d", "parameters": { "type": "object" } }
        });
        assert_eq!(spec_name(&wrapped), "bash");
        assert_eq!(
            spec_field(&wrapped, "parameters").and_then(|v| v["type"].as_str()),
            Some("object")
        );
        let flat = serde_json::json!({ "name": "read", "parameters": { "type": "object" } });
        assert_eq!(spec_name(&flat), "read");
        assert!(spec_field(&flat, "description").is_none());
    }

    /// 工具按 turn + Step N 归属请求;消息优先 request_number
    #[test]
    fn owning_request_matches_step() {
        use dsh_core::trajectory::TrajectoryRequest;
        let req = |number: u64, turn: u64, step: u64| TrajectoryRequest {
            number,
            turn,
            step,
            model: "m".into(),
            provider: "p".into(),
            reasoning_effort: None,
            status: "complete".into(),
            started_at: 0,
            completed_at: 0,
            duration_ms: 0,
            ttft_ms: None,
            usage: None,
            cumulative: Default::default(),
            tool_calls: 0,
        };
        let requests = vec![req(1, 1, 1), req(2, 1, 2), req(3, 2, 1)];
        // 工具:Step 2 的工具 → Request #2
        let tool = rec(4, "tool", Some(1), "Step 2");
        assert_eq!(owning_request(&tool, &requests).map(|q| q.number), Some(2));
        // 消息:request_number 优先(即便 group 归属另指)
        let mut m = rec(3, "message", Some(1), "Step 1");
        m.request_number = Some(3);
        assert_eq!(owning_request(&m, &requests).map(|q| q.number), Some(3));
        // 轮外/Message 组 → 无归属
        let user = rec(2, "user", Some(1), "Message");
        assert!(owning_request(&user, &requests).is_none());
    }

    /// 工具的发起消息 = 同轮同步中更早的 assistant message
    #[test]
    fn parent_message_lookup() {
        let records = vec![
            rec(1, "user", Some(1), "Message"),
            rec(2, "message", Some(1), "Step 1"),
            rec(3, "tool", Some(1), "Step 1"),
            rec(4, "message", Some(1), "Step 2"),
            rec(5, "tool", Some(1), "Step 2"),
        ];
        // Step 2 的工具 → 消息 4;Step 1 的工具 → 消息 2
        assert_eq!(
            parent_message(&records[4], &records).map(|r| r.index),
            Some(4)
        );
        assert_eq!(
            parent_message(&records[2], &records).map(|r| r.index),
            Some(2)
        );
        // 消息自身无父
        assert!(parent_message(&records[3], &records).is_none());
    }

    /// JSON 分词:键/字符串/数字/关键字/标点,转义引号不截断
    #[test]
    fn json_tokens_classify() {
        let toks = json_tokens("  \"command\": \"echo hi\",");
        assert_eq!(
            toks,
            vec![
                (JKind::Plain, "  ".into()),
                (JKind::Key, "\"command\"".into()),
                (JKind::Plain, ": ".into()),
                (JKind::Str, "\"echo hi\"".into()),
                (JKind::Plain, ",".into()),
            ]
        );
        let toks = json_tokens("    \"n\": -42.5, \"ok\": true, \"x\": null");
        assert!(toks.contains(&(JKind::Num, "-42.5".into())));
        assert!(toks.contains(&(JKind::Kw, "true".into())));
        assert!(toks.contains(&(JKind::Kw, "null".into())));
        // 值内转义引号:字符串完整读出,不被截断
        let toks = json_tokens("\"a\": \"x\\\"y\"");
        assert_eq!(toks[0], (JKind::Key, "\"a\"".into()));
        assert_eq!(toks[2], (JKind::Str, "\"x\\\"y\"".into()));
        // 非键字符串(数组元素)
        let toks = json_tokens("  \"ls -la\"");
        assert_eq!(toks[1], (JKind::Str, "\"ls -la\"".into()));
    }
}
