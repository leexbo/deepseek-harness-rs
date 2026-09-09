//! 上下文占用圆环(web `ContextMeter` 同位数据源:session_stats 的
//! contextUsed/contextWindow,最新请求 prompt 侧采样)。canvas +
//! PathBuilder 多段折线近似圆弧(gpui 0.2.2 无 svg 动态资产/弧原语)。
//! 点击详情面板在 composer(context_card:构成三段条 + 图例)。

use gpui_kit::{Bounds, IntoElement, ParentElement, Pixels, Styled, canvas, div, point, px};

use super::store::ContextOccupancy;
use crate::kits::theme;

/// 圆环:track 整圈 + 进度弧(顶端起顺时针;0% 只剩 track)。
/// `size` 建议与容器一致(web 14px)
pub fn ring(percent: f64, size: f32) -> impl IntoElement {
    // web 比例:14px 盒 / 2px 描边
    let stroke = (size / 7.).max(1.5);
    div().flex().flex_shrink_0().size(px(size)).child(
        canvas(
            move |_, _, _| {},
            move |bounds: Bounds<Pixels>, _, window, _| {
                let center = point(
                    bounds.origin.x + bounds.size.width / 2.,
                    bounds.origin.y + bounds.size.height / 2.,
                );
                let radius = size / 2. - stroke / 2. - 0.5;
                if let Ok(track) = arc(center, radius, stroke, 1.0) {
                    window.paint_path(track, theme::BORDER());
                }
                if percent > 0.005
                    && let Ok(fill) = arc(center, radius, stroke, percent.clamp(0., 1.))
                {
                    window.paint_path(fill, ring_color(percent));
                }
            },
        )
        .size_full(),
    )
}

/// 圆弧路径(圆心/半径/描边宽/整圈占比;顶端 -90° 起顺时针,折线近似)
fn arc(
    center: gpui_kit::Point<Pixels>,
    radius: f32,
    stroke: f32,
    fraction: f64,
) -> Result<gpui_kit::Path<Pixels>, anyhow::Error> {
    let mut builder = gpui_kit::PathBuilder::stroke(px(stroke));
    let segments = ((fraction * 64.0).ceil() as usize).clamp(2, 64);
    let pts: Vec<gpui_kit::Point<Pixels>> = (0..=segments)
        .map(|i| {
            let t = fraction * (i as f64) / (segments as f64);
            let angle = -std::f64::consts::FRAC_PI_2 + t * std::f64::consts::TAU;
            point(
                center.x + px((angle.cos() * radius as f64) as f32),
                center.y + px((angle.sin() * radius as f64) as f32),
            )
        })
        .collect();
    builder.add_polygon(&pts, false);
    builder.build()
}

/// 环色:常态品牌蓝,≥70% 警戒黄,≥90% 危险红
fn ring_color(percent: f64) -> gpui_kit::Rgba {
    if percent >= 0.9 {
        theme::DANGER()
    } else if percent >= 0.7 {
        theme::WARN()
    } else {
        theme::BRAND()
    }
}

/// token 数人读(1.2M / 345.6k / 789)
pub fn fmt_tok(v: u64) -> String {
    if v >= 1_000_000 {
        format!("{:.1}M", v as f64 / 1_000_000.0)
    } else if v >= 1_000 {
        format!("{:.1}k", v as f64 / 1_000.0)
    } else {
        format!("{v}")
    }
}

/// 占用详情卡内容行数据(系统提示/工具定义/会话消息)
pub fn breakdown_rows(o: &ContextOccupancy) -> [(&'static str, gpui_kit::Rgba, u64); 3] {
    [
        ("系统提示", theme::BRAND(), o.system),
        ("工具定义", theme::WARN(), o.tools),
        ("会话消息", theme::SUCCESS(), o.messages),
    ]
}
