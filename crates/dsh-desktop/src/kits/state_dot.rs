//! 会话「进行中」状态点(源 web `StateDot.tsx` 的 ongoing 分支便携版):
//! 3x3 点阵追逐——8 个外圈格,各自 opacity 依相位错开循环(离散阶梯,
//! 无 tween,retro 像素感),颜色 = 进行蓝(源 --dsw-static-deepseek-450)。
//!
//! 点阵用多个绝对定位 [`div`] 模拟(GPUI 的 `svg()` 只光栅化整张静态图,
//! 无法逐元素动画),配合 [`AnimationExt::with_animation`] 声明式逐帧驱动
//! opacity;每格相位错开 1/8 周期(源 animation-delay 负偏移的等效)。

use gpui_kit::{Animation, AnimationExt as _, IntoElement, ParentElement, Styled, div, px};
use std::time::Duration;

use crate::kits::theme;

/// 外圈 8 格(2px 像素格,10px 网格,顺时针从左上;源 MATRIX_CELLS)
const MATRIX_CELLS: [(f32, f32); 8] = [
    (0., 0.),
    (4., 0.),
    (8., 0.),
    (8., 4.),
    (8., 8.),
    (4., 8.),
    (0., 8.),
    (0., 4.),
];

/// 离散阶梯 opacity(源 keyframes dsh-state-dot-chase:0/12.5/25/37.5 四分位)
fn phase_opacity(phase: f32) -> f32 {
    let p = phase.rem_euclid(1.0);
    if p < 0.125 {
        1.0
    } else if p < 0.25 {
        0.6
    } else if p < 0.375 {
        0.35
    } else {
        0.15
    }
}

/// 进行中状态点:`size` 为外径(px;点阵按 size/10 等比缩放)。
/// 自动逐帧追逐,元素 drop 即停(声明式动画无泄漏)。
pub fn ongoing_dot(size: f32) -> impl IntoElement {
    let scale = size / 10.0;
    let cell = px(2.0 * scale);
    div()
        .relative()
        .size(px(size))
        .children(MATRIX_CELLS.into_iter().enumerate().map(|(i, (cx, cy))| {
            let offset = i as f32 / MATRIX_CELLS.len() as f32;
            div()
                .absolute()
                .left(px(cx * scale))
                .top(px(cy * scale))
                .size(cell)
                .bg(theme::ONGOING())
                .with_animation(
                    ("dsh-state-dot-chase", i),
                    Animation::new(Duration::from_secs(1)).repeat(),
                    move |el, delta| el.opacity(phase_opacity(delta - offset)),
                )
                .into_any_element()
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_opacity_four_quarter_buckets() {
        // 源 keyframes 四分位离散阶梯(无 tween)
        assert_eq!(phase_opacity(0.0), 1.0);
        assert_eq!(phase_opacity(0.10), 1.0);
        assert_eq!(phase_opacity(0.13), 0.6);
        assert_eq!(phase_opacity(0.24), 0.6);
        assert_eq!(phase_opacity(0.26), 0.35);
        assert_eq!(phase_opacity(0.37), 0.35);
        assert_eq!(phase_opacity(0.40), 0.15);
        assert_eq!(phase_opacity(0.9), 0.15);
    }

    #[test]
    fn phase_opacity_wraps_to_cycle() {
        // 相位跨周期回绕;负数(格相位 < 0 起步)等效 rem_euclid
        assert_eq!(phase_opacity(-0.05), 0.15);
        assert_eq!(phase_opacity(1.05), 1.0);
    }

    #[test]
    fn matrix_has_8_outer_cells() {
        // 3x3 外圈(剔除中心)共 8 格,坐标在 10px 网格上
        assert_eq!(MATRIX_CELLS.len(), 8);
        for (x, y) in MATRIX_CELLS {
            assert!([0., 4., 8.].contains(&x));
            assert!([0., 4., 8.].contains(&y));
        }
    }
}
