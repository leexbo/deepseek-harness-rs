//! 窗口底状态栏(26px 全宽条):会话统计单行居中(`session_stats`
//! 打开/running 边沿拉取 + 2s 轮询)。
//! 右侧显示当前 preset 模式(trio 图标);其余徽标已精简
//! (权限/模型·等级为只读展示,设置入口在 composer 底排;git 分支迁标题栏)。

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{App, Entity, InteractiveElement, IntoElement, ParentElement, Styled, div, px};

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
        // 与内容画布同底(BASE),仅顶缘发丝线分层
        .bg(theme::BASE())
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
        .children(billing_badge(store.read(cx)))
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

/// 左下角计费徽标:当前 defaultProvider 有 billing_cache 才显示
/// (余额「¥ 9.52」/ 用量「5h 6% · 7d 6% · 4d22h」)
fn billing_badge(st: &AppStore) -> Option<impl IntoElement> {
    let default_id = st.settings.settings_snapshot["defaultProvider"]
        .as_str()?
        .to_string();
    let provider = st.settings.settings_snapshot["providers"]
        .as_array()?
        .iter()
        .find(|p| p["id"].as_str() == Some(default_id.as_str()))?;
    let cache = &provider["billing_cache"];
    let text = match cache["kind"].as_str() {
        Some("balance") => {
            let amount = cache["amount"].as_str()?;
            match cache["currency"].as_str() {
                Some(cur) => format!("¥ {amount} {cur}").replace("¥ CNY", "¥"),
                None => format!("¥ {amount}"),
            }
        }
        Some("usage") => {
            let p5 = cache["pct_5h"].as_u64();
            let p7 = cache["pct_7d"].as_u64();
            let resets = cache["resets"].as_str();
            match (p5, p7) {
                (None, None) => return None,
                (p5, p7) => {
                    let mut parts = Vec::new();
                    if let Some(v) = p5 {
                        parts.push(format!("5h {v}%"));
                    }
                    if let Some(v) = p7 {
                        parts.push(format!("7d {v}%"));
                    }
                    if let Some(r) = resets {
                        parts.push(r.to_string());
                    }
                    parts.join(" · ")
                }
            }
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
            .child(text),
    )
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
