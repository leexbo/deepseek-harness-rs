//! 窗口底状态栏(26px 全宽条):会话统计单行居中(`session_stats`
//! 打开/running 边沿拉取 + 2s 轮询)。
//! 右侧显示当前 preset 模式(trio 图标);其余徽标已精简
//! (权限/模型·等级为只读展示,设置入口在 composer 底排;git 分支迁标题栏)。

use gpui_kit::component::StyledExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, px,
};

use crate::features::settings::usage_bar;
use crate::kits::icons::{DshIcon, fixed};
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 状态栏整体(统计居中;右侧 preset 模式;无数据/turns 时空串占位)
pub fn render(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let stats_text = stats_line(store.read(cx));
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
                .truncate()
                .text_center()
                .text_color(theme::LABEL_3())
                .child(stats_text),
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
                    .child(fixed(DshIcon::AgentPreset, 14.))
                    .child(div().text_color(theme::LABEL_2()).child(label)),
            )
        })
}

/// 统计串(无 turns → 空串;数据键对齐 web StatsBar)
fn stats_line(st: &AppStore) -> String {
    let Some(id) = st.state.current_id.as_deref() else {
        return String::new();
    };
    let Some(stats) = st.stats_by_id.get(id) else {
        return String::new();
    };
    if stats["turns"].as_u64() == Some(0) {
        return String::new();
    }
    format!(
        "{} 轮 · {} 步 · LLM {} · 工具调用 {} · 首 token 平均 {} · {} tok/s · 缓存命中 {}% · 输入 {} · 输出 {}",
        stats["turns"].as_u64().unwrap_or(0),
        stats["steps"].as_u64().unwrap_or(0),
        fmt_ms(stats["llmMs"].as_i64().unwrap_or(0)),
        fmt_ms(stats["toolMs"].as_i64().unwrap_or(0)),
        fmt_ms(stats["firstTokenMs"].as_i64().unwrap_or(0)),
        stats["tokensPerSecond"].as_u64().unwrap_or(0),
        stats["cacheHitPercent"].as_u64().unwrap_or(0),
        fmt_tok_k(stats["inputTokens"].as_u64().unwrap_or(0)),
        fmt_tok_k(stats["outputTokens"].as_u64().unwrap_or(0)),
    )
}

/// 左下角计费徽标:当前工作区生效 provider 有 billing_cache 才显示
/// (余额「¥ 9.52」/ 用量两窗「5小时 [bar] 0% · 1周 [bar] 53%」;
/// 点击弹小卡片看恢复时间,根级渲染见 shell/mod)
fn billing_badge(store: &Entity<AppStore>, cx: &App) -> Option<gpui_kit::AnyElement> {
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
                            .child(fixed(DshIcon::Gauge, 12.).text_color(theme::LABEL_2()))
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
            .child(fixed(DshIcon::Gauge, 12.))
            .child(text)
            .into_any_element(),
    )
}

/// 徽标窗标签(11px 三级色)
fn window_label(text: &str) -> gpui_kit::AnyElement {
    div()
        .text_size(px(11.))
        .text_color(theme::CAPTION())
        .child(text.to_string())
        .into_any_element()
}

/// 计费小卡片(徽标点击弹出;每窗标签行「pct · 恢复时间」+ 粗进度条;
/// 恢复时间 5 小时窗 = 本地时刻 HH:MM,周窗 = 本地日期 M月D日)
pub(crate) fn billing_card(store: &Entity<AppStore>, cx: &App) -> gpui_kit::AnyElement {
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
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .text_color(theme::LABEL())
                                .child(label),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
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
fn pct_label(v: u64) -> gpui_kit::AnyElement {
    div()
        .text_size(px(11.))
        .text_color(theme::CAPTION())
        .child(format!("{v}%"))
        .into_any_element()
}

/// token 数(状态栏输入/输出):最小单位 k,≥1M 自动进位
fn fmt_tok_k(v: u64) -> String {
    if v >= 1_000_000 {
        format!("{:.1}M", v as f64 / 1_000_000.0)
    } else {
        format!("{:.1}k", v as f64 / 1000.0)
    }
}

/// 毫秒 → 人读时长
fn fmt_ms(ms: i64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms > 0 {
        format!("{ms}ms")
    } else {
        "—".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_ms_buckets() {
        assert_eq!(fmt_ms(0), "—");
        assert_eq!(fmt_ms(120), "120ms");
        assert_eq!(fmt_ms(2400), "2.4s");
    }

    /// 输入/输出最小单位 k、≥1M 进位
    #[test]
    fn fmt_tok_k_buckets() {
        assert_eq!(fmt_tok_k(0), "0.0k");
        assert_eq!(fmt_tok_k(860), "0.9k");
        assert_eq!(fmt_tok_k(12_400), "12.4k");
        assert_eq!(fmt_tok_k(1_280_000), "1.3M");
    }
}
