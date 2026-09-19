//! 窗口底状态栏(26px 全宽条):会话统计两 pill 居中(照源 StatsPills——
//! 仪表 pill「N 轮 M 步 · T tok/s」弹会话统计卡,数据库 pill「75M tok ·
//! 缓存命中 99.7%」弹 Token 用量卡;详情进卡片,状态栏只留摘要)。
//! 右侧:当前 provider 计费徽标(余额 / 5h·7d 两窗;不同源,另拍板保留)+
//! preset 模式指示。

use gpui_kit::component::StyledExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, div, px,
};

use crate::features::settings::usage_bar;
use crate::kits::fmt::{
    fmt_cache_hit, fmt_duration_compact, fmt_exact_count, fmt_tokens_abbrev, fmt_tps,
};
use crate::kits::icons::{LiumaIcon, fixed};
use crate::kits::theme;
use crate::shell::store::{AppStore, StatsCardKind};

/// 状态栏整体(统计 pill 居中;右侧计费徽标 + preset 模式;无数据占位)
pub fn render(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let (preset_id, label) = {
        let st = store.read(cx);
        let id = st.current_cfg_or_default().preset.clone();
        if id.is_empty() {
            (String::new(), String::new())
        } else {
            let label = st.preset_label(&id);
            (id, label)
        }
    };
    let mode = if preset_id.is_empty() {
        None
    } else {
        Some((preset_id, label))
    };
    div()
        .id("statusbar")
        .debug_selector(|| "statusbar".to_string())
        .flex()
        .w_full()
        .h(px(26.))
        .flex_shrink_0()
        .items_center()
        .border_t_1()
        .border_color(theme::BORDER())
        // 与内容画布同底(透 Root 毛玻璃涂层),仅顶缘发丝线分层
        .px(px(16.))
        .text_size(px(11.))
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .flex()
                .justify_center()
                .children(stats_pills(store, cx)),
        )
        // 右下角:当前 provider 计费徽标(余额 / 5h·7d 用量;未配置不渲染)
        .children(billing_badge(store, cx))
        .when_some(mode, |el, (_id, label)| {
            el.child(
                div()
                    .id("statusbar-mode")
                    .debug_selector(|| "statusbar-mode".to_string())
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(12.))
                    .text_color(theme::LABEL_2())
                    .child(fixed(LiumaIcon::AgentPreset, 14.))
                    .child(div().text_color(theme::LABEL_2()).child(label)),
            )
        })
}

/// 当前会话统计(无会话/无数据 → None)
fn current_stats(st: &AppStore) -> Option<&serde_json::Value> {
    let id = st.state.current_id.as_deref()?;
    st.stats_by_id.get(id)
}

/// 会话统计 pill 组(照源 StatsPills:steps==0 且无 token 整组不渲染;
/// steps==0 隐仪表 pill,总量 0 隐用量 pill)。两 pill 各自 bounds 捕获,
/// 点击恒开对应卡(关闭走外点全关;嵌套 on_click toggle 真机连发禁 toggle)
fn stats_pills(store: &Entity<AppStore>, cx: &App) -> Option<AnyElement> {
    let st = store.read(cx);
    let stats = current_stats(st)?;
    let turns = stats["turns"].as_u64().unwrap_or(0);
    let steps = stats["steps"].as_u64().unwrap_or(0);
    let tps = stats["tokensPerSecond"].as_u64().unwrap_or(0);
    let input = stats["inputTokens"].as_u64().unwrap_or(0);
    let output = stats["outputTokens"].as_u64().unwrap_or(0);
    let read = stats["cacheReadTokens"].as_u64().unwrap_or(0);
    if turns == 0 {
        return None;
    }
    let mut row = div().flex().items_center().gap(px(6.));
    if steps > 0 {
        let mut label = format!("{turns} 轮 {steps} 步");
        if tps > 0 {
            label.push_str(&format!(" · {} tok/s", fmt_tps(tps as f64)));
        }
        row = row.child(stats_chip(
            store,
            "statusbar-stats-time",
            fixed(LiumaIcon::Gauge, 12.).into_any_element(),
            label,
            StatsCardKind::Time,
            |st, b| st.stats_time_bounds = Some(b),
        ));
    }
    let total = input + output;
    if total > 0 {
        let mut label = format!("{} tok", fmt_tokens_abbrev(total));
        if let Some(hit) = fmt_cache_hit(read, input) {
            label.push_str(&format!(" · 缓存命中 {hit}%"));
        }
        row = row.child(stats_chip(
            store,
            "statusbar-stats-usage",
            fixed(gpui_kit::assets::IconName::Database, 12.).into_any_element(),
            label,
            StatsCardKind::Usage,
            |st, b| st.stats_usage_bounds = Some(b),
        ));
    }
    Some(row.into_any_element())
}

/// 统计 pill(billing 徽标同款 chip 形制:h22/rounded6/hover DOCK;
/// 渲染期 bounds 捕获进指定字段,卡片根级渲染锚定用)
fn stats_chip(
    store: &Entity<AppStore>,
    sel: &'static str,
    icon: AnyElement,
    label: String,
    kind: StatsCardKind,
    set_bounds: impl Fn(&mut AppStore, gpui_kit::Bounds<gpui_kit::Pixels>) + Copy + 'static,
) -> AnyElement {
    let s = store.clone();
    div()
        .relative()
        .flex_shrink_0()
        .child(
            div()
                .id(SharedString::from(sel))
                .debug_selector(move || sel.to_string())
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(6.))
                .h(px(22.))
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::DOCK()))
                .text_color(theme::LABEL_2())
                .child(icon)
                .child(label)
                .on_click(move |_, _, cx| {
                    s.update(cx, |st, cx| {
                        st.stats_card = Some(kind);
                        cx.notify();
                    });
                }),
        )
        // 渲染期 bounds 捕获(根级卡片锚定分子,同计费徽标)
        .child(div().absolute().inset_0().child({
            let cap = store.clone();
            gpui_kit::canvas(
                move |b, _, cx| {
                    cap.update(cx, |st, _| set_bounds(st, b));
                },
                |_, _, _, _| {},
            )
            .size_full()
        }))
        .into_any_element()
}

/// 会话统计卡(仪表 pill 详情):模型用时/工具调用用时/首 token 平均/输出速度
pub(crate) fn session_stats_card(store: &Entity<AppStore>, cx: &App) -> AnyElement {
    let stats = current_stats(store.read(cx)).cloned();
    let mut card = detail_card();
    card = card.child(card_head(
        fixed(LiumaIcon::Gauge, 14.).into_any_element(),
        "会话统计",
        None,
    ));
    if let Some(s) = stats {
        card = card
            .child(detail_row(
                "模型用时",
                fmt_duration_compact(s["llmMs"].as_i64().unwrap_or(0)),
            ))
            .child(detail_row(
                "工具调用用时",
                fmt_duration_compact(s["toolMs"].as_i64().unwrap_or(0)),
            ))
            .child(detail_row(
                "首 token 平均（TTFT）",
                fmt_duration_compact(s["firstTokenMs"].as_i64().unwrap_or(0)),
            ))
            .child(detail_row(
                "输出速度（TPS）",
                format!(
                    "{} tok/s",
                    fmt_tps(s["tokensPerSecond"].as_f64().unwrap_or(0.0))
                ),
            ));
    }
    card.into_any_element()
}

/// Token 用量卡(数据库 pill 详情):头部总数 + 缓存命中/未缓存输入/
/// 缓存读取/缓存写入(≠0 才显)/输出,精确值千分位
pub(crate) fn token_usage_card(store: &Entity<AppStore>, cx: &App) -> AnyElement {
    let stats = current_stats(store.read(cx)).cloned();
    let mut card = detail_card();
    let (total, rows) = match stats {
        Some(s) => {
            let input = s["inputTokens"].as_u64().unwrap_or(0);
            let output = s["outputTokens"].as_u64().unwrap_or(0);
            let read = s["cacheReadTokens"].as_u64().unwrap_or(0);
            let write = s["cacheWriteTokens"].as_u64().unwrap_or(0);
            let uncached = s["uncachedInputTokens"].as_u64().unwrap_or(0);
            let total = input + output;
            let mut rows = vec![];
            if let Some(hit) = fmt_cache_hit(read, input) {
                rows.push(("缓存命中".to_string(), format!("{hit}%")));
            }
            rows.push((
                "未缓存输入".to_string(),
                format!("{} tok", fmt_exact_count(uncached)),
            ));
            rows.push((
                "缓存读取".to_string(),
                format!("{} tok", fmt_exact_count(read)),
            ));
            if write != 0 {
                rows.push((
                    "缓存写入".to_string(),
                    format!("{} tok", fmt_exact_count(write)),
                ));
            }
            rows.push((
                "输出".to_string(),
                format!("{} tok", fmt_exact_count(output)),
            ));
            (Some(total), rows)
        }
        None => (None, vec![]),
    };
    card = card.child(card_head(
        fixed(gpui_kit::assets::IconName::Database, 14.).into_any_element(),
        "Token 用量",
        total.map(|t| format!("{} tok", fmt_exact_count(t))),
    ));
    for (label, value) in rows {
        card = card.child(detail_row(&label, value));
    }
    card.into_any_element()
}

/// 详情卡容器(pilling 徽标卡同款:p14/min_w260/行距 10)
fn detail_card() -> gpui_kit::Div {
    div().v_flex().gap(px(10.)).p(px(14.)).min_w(px(260.))
}

/// 卡头部(图标+标题,可选右对齐总数)+ 发丝分隔线
fn card_head(icon: AnyElement, title: &str, total: Option<String>) -> AnyElement {
    div()
        .v_flex()
        .gap(px(10.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(icon)
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::LABEL())
                        .child(title.to_string()),
                )
                .when_some(total, |el, t| {
                    el.child(div().flex_1()).child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::LABEL())
                            .child(t),
                    )
                }),
        )
        .child(div().h(px(1.)).w_full().bg(theme::BORDER()))
        .into_any_element()
}

/// 详情卡行(label 左侧灰 / 值右对齐)
fn detail_row(label: &str, value: String) -> AnyElement {
    div()
        .flex()
        .items_baseline()
        .justify_between()
        .gap(px(16.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme::LABEL_2())
                .child(label.to_string()),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme::LABEL())
                .child(value),
        )
        .into_any_element()
}

/// 左下角计费徽标:当前工作区生效 provider 有 billing_cache 才显示
/// (余额「¥ 9.52」/ 用量两窗「5小时 [bar] 0% · 1周 [bar] 53%」;
/// 点击弹小卡片看恢复时间,根级渲染见 shell/mod)
fn billing_badge(store: &Entity<AppStore>, cx: &App) -> Option<AnyElement> {
    let st = store.read(cx);
    let snap = &st.settings.settings_snapshot;
    let pid = st
        .state
        .active_workspace
        .as_deref()
        .and_then(|ws| snap["workspaceProviders"][ws].as_str())
        .unwrap_or_else(|| snap["defaultProvider"].as_str().unwrap_or_default());
    let provider = snap["providers"]
        .as_array()?
        .iter()
        .find(|p| p["id"].as_str() == Some(pid))?;
    let cache = &provider["billing_cache"];
    let s_click = store.clone();
    let text = match cache["kind"].as_str() {
        Some("balance") => {
            let amount = cache["amount"].as_str()?;
            match cache["currency"].as_str() {
                Some(cur) => format!("¥ {amount} {cur}").replace("¥ CNY", "¥"),
                None => format!("¥ {amount}"),
            }
        }
        Some("usage") => {
            // 每窗:标签 + 进度条 + 百分比(点击弹卡片看恢复时间)
            let p5 = cache["pct_5h"].as_u64();
            let p7 = cache["pct_7d"].as_u64();
            if p5.is_none() && p7.is_none() {
                return None;
            }
            return Some(
                div()
                    .relative()
                    .flex_shrink_0()
                    .child(
                        div()
                            .id("statusbar-billing")
                            .debug_selector(|| "statusbar-billing".to_string())
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .px(px(6.))
                            .h(px(22.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::DOCK()))
                            .child(fixed(LiumaIcon::Gauge, 12.).text_color(theme::LABEL_2()))
                            .when_some(p5, |el, v| {
                                el.child(window_label("5小时"))
                                    .child(usage_bar(v, 20.))
                                    .child(pct_label(v))
                            })
                            .when_some(p7, |el, v| {
                                el.child(div().w(px(1.)).h(px(10.)).bg(theme::BORDER()))
                                    .child(window_label("1周"))
                                    .child(usage_bar(v, 20.))
                                    .child(pct_label(v))
                            })
                            .on_click(move |_, _, cx| {
                                // 绝对方向恒开(toggle 禁:真机嵌套 on_click 连发),
                                // 关闭走外点全关
                                s_click.update(cx, |st, cx| {
                                    st.billing_card_open = true;
                                    cx.notify();
                                });
                            }),
                    )
                    // 渲染期 bounds 捕获(根级卡片锚定分子,同权限 chip)
                    .child(div().absolute().inset_0().child({
                        let cap = store.clone();
                        gpui_kit::canvas(
                            move |b, _, cx| {
                                cap.update(cx, |st, _| {
                                    st.billing_chip_bounds = Some(b);
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .size_full()
                    }))
                    .into_any_element(),
            );
        }
        _ => return None,
    };
    Some(
        div()
            .id("statusbar-billing")
            .debug_selector(|| "statusbar-billing".to_string())
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(4.))
            .pr(px(12.))
            .text_color(theme::LABEL_2())
            .child(fixed(LiumaIcon::Gauge, 12.))
            .child(text)
            .into_any_element(),
    )
}

/// 徽标窗标签(11px 三级色)
fn window_label(text: &str) -> AnyElement {
    div()
        .text_size(px(11.))
        .text_color(theme::CAPTION())
        .child(text.to_string())
        .into_any_element()
}

/// 计费小卡片(徽标点击弹出;每窗标签行「pct · 恢复时间」+ 粗进度条;
/// 恢复时间 5 小时窗 = 本地时刻 HH:MM,周窗 = 本地日期 M月D日)
pub(crate) fn billing_card(store: &Entity<AppStore>, cx: &App) -> AnyElement {
    let st = store.read(cx);
    let snap = &st.settings.settings_snapshot;
    let pid = st
        .state
        .active_workspace
        .as_deref()
        .and_then(|ws| snap["workspaceProviders"][ws].as_str())
        .unwrap_or_else(|| snap["defaultProvider"].as_str().unwrap_or_default());
    let cache = snap["providers"]
        .as_array()
        .and_then(|ps| {
            ps.iter()
                .find(|p| p["id"].as_str() == Some(pid))
                .map(|p| &p["billing_cache"])
        })
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let blocks = [
        (
            "5 小时",
            cache["pct_5h"].as_u64(),
            cache["resets"].as_str().and_then(|r| reset_time(r, false)),
            theme::BRAND(),
        ),
        (
            "1 周",
            cache["pct_7d"].as_u64(),
            cache["resets_7d"]
                .as_str()
                .and_then(|r| reset_time(r, true)),
            theme::SUCCESS(),
        ),
    ];
    let mut col = div().v_flex().gap(px(12.)).p(px(14.)).min_w(px(200.));
    for (label, pct, resets, color) in blocks {
        let Some(v) = pct else { continue };
        col = col.child(
            div()
                .v_flex()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme::LABEL())
                                .child(label),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme::LABEL())
                                .child(format!("{v}%")),
                        )
                        .children(resets.map(|t| {
                            div()
                                .text_size(px(12.))
                                .text_color(theme::CAPTION())
                                .child(format!("· {t}"))
                        })),
                )
                .child(
                    // 粗进度条(h6,固定宽与卡体内容区对齐):窗色填充
                    div()
                        .w(px(172.))
                        .h(px(6.))
                        .rounded(px(3.))
                        .bg(theme::BORDER_2())
                        .overflow_hidden()
                        .when(v > 0, |el| {
                            el.child(
                                div()
                                    .w(px(172. * (v.min(100) as f32) / 100.))
                                    .h_full()
                                    .rounded(px(3.))
                                    .bg(color),
                            )
                        }),
                ),
        );
    }
    col.into_any_element()
}

/// 恢复时刻(本地时区):weekly = 周窗日期「M月D日」,否则「HH:MM」
fn reset_time(resets: &str, weekly: bool) -> Option<String> {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::Local
        .timestamp_millis_opt(resets.parse::<i64>().ok()?)
        .single()?;
    Some(if weekly {
        format!("{}月{}日", dt.month(), dt.day())
    } else {
        dt.format("%H:%M").to_string()
    })
}

/// 用量百分比标签(11px 三级色;与进度条同组)
fn pct_label(v: u64) -> AnyElement {
    div()
        .text_size(px(11.))
        .text_color(theme::CAPTION())
        .child(format!("{v}%"))
        .into_any_element()
}
