//! Mermaid 图查看器(**纯图放大态**):全屏遮罩 + 角落关闭钮,无工具栏、
//! 无分段,仅放大的图与 Esc/遮罩/滚轮缩放/拖拽平移。
//!
//! 打开链路:卡片「放大」按钮 → `store.open_mermaid_viewer`;覆盖层挂
//! `WorkspaceView` 根层(树序 = z 序,与 lightbox 同层同构——内容卡
//! `overflow_hidden` 会裁剪非根层覆盖层)。所有控件(图表/代码、±缩放、
//! 下载、放大)都落在**内嵌卡片**上,见 `kits/mermaid::diagram` 的卡片
//! 工具条——放大只放大图片,不把控件带进来。
//!
//! **视口裁剪光栅**(对齐 web 内存行为):web 持矢量任意缩放零像素开销;
//! gpui 无彩色矢量路径,位图管线下只光栅化**可见区域**(`kits::mermaid::
//! raster_viewport`)——位图 ≤ 视口尺寸(几十 MB),与图多大/放大多少
//! 无关,且区域内任意倍数全分辨率。平移(拖拽)/缩放(滚轮)都换新视
//! 口档,防抖 300ms 静止后后台渲染;期间旧档平移/拉伸过渡(不空窗)。
//!
//! 比例语义:「默认自适应」——打开时按视口与自然逻辑尺寸(SVG 头解析,
//! 免光栅)算 contain 倍数回写;**滚轮一律缩放**且**视口中心锚定**(缩放
//! 前后中心指向同一图点,见 `store::set_mermaid_zoom`);**拖拽平移**
//! 双向钳制 [0, 整图−视口]。画布不设内建 overflow scroll——滚轮专用于
//! 缩放(自定义 `on_scroll_wheel` 已 `stop_propagation`,内建 scroll
//! offset 不监听它,但双通道仍会抢事件)。

use gpui_kit::InteractiveElement;
use gpui_kit::StatefulInteractiveElement;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, CursorStyle, Entity, ImageSource, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, ScrollDelta, Styled, Window, div, img, px, rgba,
};

use crate::kits::theme;
use crate::shell::store::AppStore;

/// 画布与遮罩边缘的可见环宽(环上点击 = 遮罩关闭;画布 inset 即视口边)
const RING: f32 = 6.0;

/// 自适应初始倍数:contain 布满视口(可放大到上限 8,与卡片 ± 一致)。
/// 纯函数(单测直查);`nat_*` 为自然逻辑尺寸。
fn fit_zoom(avail_w: f32, avail_h: f32, nat_w: f32, nat_h: f32) -> f32 {
    ((avail_w / nat_w).min(avail_h / nat_h)).clamp(0.25, 8.0)
}

/// 视口逻辑尺寸 = 整窗 − 两侧环宽(纯图态无工具条)。
fn viewport_of(size: gpui_kit::Size<gpui_kit::Pixels>) -> (f32, f32) {
    (
        (size.width.as_f32() - RING * 2.0).max(1.0),
        (size.height.as_f32() - RING * 2.0).max(1.0),
    )
}

/// 查看器覆盖层(仅 `None` 之外由根层 `.when` 挂载;防御性分支同样
/// 返回空元素)。纯图:遮罩 + 画布(视口光栅)+ 角落关闭钮。
pub fn mermaid_viewer(
    store: &Entity<AppStore>,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let store = store.clone();
    let st = store.read(cx);
    let Some(viewer) = st.chat.mermaid_viewer.clone() else {
        return div().into_any_element();
    };

    // 视口回写(变更才重排防抖任务):视口决定裁剪区域与 pan 钳制范围
    let viewport = viewport_of(window.bounds().size);
    store.update(cx, |s, cx| {
        s.set_mermaid_viewer_viewport(viewport.0, viewport.1, cx)
    });

    // 自然逻辑尺寸:SVG 头解析(缓存命中,零渲染;失败 = 渲染必失败,
    // svg_for 失败记忆短路,下帧同样 None 不重试)
    let nat = crate::kits::mermaid::natural_size(&viewer.source);
    let nat_known = nat.is_some();
    let (nat_w, nat_h) = nat.unwrap_or((1.0, 1.0));

    // 自适应:zoom 哨兵 0.0 = 未定 → 按当前视口与自然尺寸 contain 计算
    // 并回写(渲染期回写先例同 chat 列宽;回写走 set_mermaid_viewer_fit:
    // 落定后同样排防抖重光栅任务,本帧即用计算值)
    let zoom = if viewer.zoom <= 0.0 {
        let fit = fit_zoom(viewport.0, viewport.1, nat_w, nat_h);
        if nat_known {
            store.update(cx, |s, cx| s.set_mermaid_viewer_fit(fit, cx));
        }
        fit
    } else {
        viewer.zoom
    };

    // 显示档解析(**只读** 单槽,渲染路径零重活):槽有图(精确档或
    // 同图旧档)→ 图 + 渲染档上下文;槽空 → **卡片光栅占位**(档上下文
    // = 卡片 zoom 的全图、pan 0,atlas 已有零额外纹理)+ kick 一次
    // 0 延迟后台渲染首档(主线程绝不同步 usvg 解析——首开含系统字体
    // 加载可达秒级,曾冻 UI)。两路载荷同构:显示方一律地图式过渡。
    let mut display = crate::kits::mermaid::viewer_display(&viewer.source);
    if display.is_none() {
        if let Some(ph) = viewer.placeholder.clone() {
            let ph_zoom = viewer.placeholder_zoom.max(0.05);
            display = Some((
                ph,
                crate::kits::mermaid::RenderedView {
                    zoom_q: ph_zoom,
                    pan_q: (0.0, 0.0),
                    region: (nat_w * ph_zoom, nat_h * ph_zoom),
                },
            ));
        }
        if !viewer.kicked {
            store.update(cx, |s, cx| s.kick_mermaid_reraster(cx));
        }
    }

    div()
        .id("mv")
        .debug_selector(|| "mv".to_string())
        .absolute()
        .inset_0()
        .bg(rgba(0x000000d9))
        // 遮罩空白(画布外环)点击关闭;画布 stop 拦截
        .on_mouse_down(MouseButton::Left, {
            let mask_store = store.clone();
            move |_ev, _window, cx| {
                mask_store.update(cx, |s, cx| s.close_mermaid_viewer(cx));
            }
        })
        // 拖拽松手兜底:抬起可能落在画布外(遮罩环/窗外),画布的
        // on_mouse_up 收不到 → drag 态滞留,之后悬停移动会继续平移
        //(「松不开」)。根层收一切左键抬起,任何位置松手都收尾。
        .on_mouse_up(MouseButton::Left, {
            let up_store = store.clone();
            move |_: &MouseUpEvent, _window, cx| {
                up_store.update(cx, |s, cx| s.set_mermaid_drag(None, cx));
            }
        })
        .child(chart_view(
            &store,
            viewer,
            zoom,
            (nat_w, nat_h),
            viewport,
            display,
        ))
        .child(close_button(&store))
        .into_any_element()
}

/// 角落关闭钮(同 modal_close 风格;与遮罩环点击/Esc 三路入口)
fn close_button(store: &Entity<AppStore>) -> impl IntoElement {
    let store = store.clone();
    div()
        .id("mv-close")
        .debug_selector(|| "mv-close".to_string())
        .absolute()
        .top(px(16.))
        .right(px(16.))
        .size(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .bg(theme::DOCK())
        .border_1()
        .border_color(theme::BORDER_2())
        .text_color(theme::LABEL_2())
        .hover(|st| st.bg(theme::LAYER()))
        .on_mouse_down(
            MouseButton::Left,
            |_: &MouseDownEvent, _: &mut Window, cx: &mut App| {
                cx.stop_propagation();
            },
        )
        .on_click(move |_, _, cx| store.update(cx, |s, cx| s.close_mermaid_viewer(cx)))
        .child(crate::kits::icons::fixed(
            gpui_kit::component::IconName::Close,
            14.,
        ))
}

/// 图表画布:视口裁剪显示 + **地图式 stale 反馈**。当前档 (zoom, pan)
/// 与渲染档 (r.zoom_q, r.pan_q, r.region) 唯一决定显示:k = z'/z_r,
/// 显示尺寸 = region×k,画布内位置 = 图原点 + pan_r·k —— 精确命中
/// k=1 落回原生裁剪位;拖拽(同 zoom)整图随手反向平移;滚轮(zoom
/// 变)绕光标锚点缩放,旧档/占位图一律即时跟随,后台清晰档就位后
/// 无缝替换。**滚轮一律缩放**(纯图查看器:滚轮 = 缩放,最直觉;
/// mac 滚轮/触控板无需按修饰键——真实滚轮 `modifiers.control` 恒 false)。
fn chart_view(
    store: &Entity<AppStore>,
    viewer: super::store::MermaidViewer,
    zoom: f32,
    natural: (f32, f32),
    viewport: (f32, f32),
    display: Option<crate::kits::mermaid::ViewerDisplay>,
) -> impl IntoElement {
    // 布局宽与光栅档须用同一量化 zoom(光栅内部量化;布局不量化 → ≤2.5%
    // 拉伸)。连续滚轮贴近的瞬时值吸附同档 → 槽命中不重光栅。
    let zoom_q = crate::kits::mermaid::quantize_zoom(zoom.max(0.25));
    let fig = (natural.0 * zoom_q, natural.1 * zoom_q);
    let pan_q = crate::kits::mermaid::clamp_pan(viewer.pan, fig, viewport);
    // 整图原点(画布坐标,逐轴独立):图大于视口 → 由 pan 决定(可拖
    // 平移);小于视口(fit 全览)→ 居中
    let origin = (
        if fig.0 > viewport.0 {
            -pan_q.0
        } else {
            (viewport.0 - fig.0) / 2.0
        },
        if fig.1 > viewport.1 {
            -pan_q.1
        } else {
            (viewport.1 - fig.1) / 2.0
        },
    );
    // 绝对定位 img(**不用 margin/m_auto**):img 的负 margin 会把收缩
    // 包装的居中容器压小、m_auto 重排再吃掉位移 —— 拖拽半速/零视觉
    // 的根源。画布自身 absolute 即定位上下文。
    let figure = display.map(|(image, r)| {
        let k = zoom_q / r.zoom_q.max(0.05);
        div()
            .debug_selector(|| "mv-figure".to_string())
            .absolute()
            .left(px(origin.0 + r.pan_q.0 * k))
            .top(px(origin.1 + r.pan_q.1 * k))
            .w(px(r.region.0 * k))
            .h(px(r.region.1 * k))
            .child(img(ImageSource::Render(image)).size_full())
            .into_any_element()
    });

    div()
        .id("mv-canvas")
        .debug_selector(|| "mv-canvas".to_string())
        .absolute()
        .inset(px(RING))
        .overflow_hidden()
        // 放大可平移(拖拽)时抓手光标;fit 全览(整图 ≤ 视口)箭头
        .cursor(if fig.0 > viewport.0 + 1.0 || fig.1 > viewport.1 + 1.0 {
            CursorStyle::OpenHand
        } else {
            CursorStyle::Arrow
        })
        .when(viewer.drag_last.is_some(), |el| {
            el.cursor(CursorStyle::ClosedHand)
        })
        // 拖拽平移:按下记指针位,移动按增量 pan(反向 = 图随手),抬起
        // (画布或根层兜底)收尾排最终档。点击不冒泡(遮罩关闭只留环上)。
        // **move 读活状态**(`store.read`):监听器来自上一次绘制,闭包
        // 捕获的渲染快照在按下→首帧重绘间是旧的,快照判 drag 会丢移动。
        .on_mouse_down(MouseButton::Left, {
            let store = store.clone();
            move |ev: &MouseDownEvent, _window, cx| {
                cx.stop_propagation();
                store.update(cx, |s, cx| {
                    s.set_mermaid_drag(Some((ev.position.x.as_f32(), ev.position.y.as_f32())), cx)
                });
            }
        })
        .on_mouse_move({
            let store = store.clone();
            move |ev: &MouseMoveEvent, _window, cx| {
                let Some((lx, ly)) = store
                    .read(cx)
                    .chat
                    .mermaid_viewer
                    .as_ref()
                    .and_then(|v| v.drag_last)
                else {
                    return;
                };
                let (x, y) = (ev.position.x.as_f32(), ev.position.y.as_f32());
                if (x - lx).abs() < 0.5 && (y - ly).abs() < 0.5 {
                    return;
                }
                store.update(cx, |s, cx| {
                    s.pan_mermaid_viewer(x - lx, y - ly, cx);
                    s.set_mermaid_drag(Some((x, y)), cx);
                });
            }
        })
        .on_mouse_up(MouseButton::Left, {
            let store = store.clone();
            move |_: &MouseUpEvent, _window, cx| {
                store.update(cx, |s, cx| s.set_mermaid_drag(None, cx));
            }
        })
        .on_scroll_wheel({
            let wheel_store = store.clone();
            move |ev, _window, cx| {
                let dy = match ev.delta {
                    ScrollDelta::Pixels(p) => p.y.as_f32(),
                    ScrollDelta::Lines(l) => l.y * 40.0,
                };
                let ratio = (dy * 0.0015).exp();
                // 光标锚定缩放(视口坐标 = 窗口坐标 − 环宽;画布 inset
                // 恰为 RING):滚轮下的图点缩放前后不动,地图直觉
                let anchor = (
                    (ev.position.x.as_f32() - RING).max(0.0),
                    (ev.position.y.as_f32() - RING).max(0.0),
                );
                wheel_store.update(cx, |s, cx| {
                    s.set_mermaid_zoom(ratio, Some(anchor), cx);
                });
                cx.stop_propagation();
            }
        })
        .children(figure)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_zoom_contain_scales_to_fill() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
        // 宽图:受宽约束(0.5 < 高约束 8)
        assert!(close(fit_zoom(1000., 800., 2000., 100.), 0.5));
        // 高图:受高约束(0.5 < 宽约束 5)
        assert!(close(fit_zoom(1000., 800., 200., 1600.), 0.5));
        // 小图:contain 可放大到填满视口(上限 8;原先卡死 1.0 致查看器显不出放大)
        assert!(close(fit_zoom(1200., 900., 300., 100.), 4.0));
        // 极大可用 → 封顶上限 8(不无限放大)
        assert!(close(fit_zoom(10000., 10000., 300., 100.), 8.0));
        // 极小视口:下限 0.25
        assert!(close(fit_zoom(10., 10., 500., 500.), 0.25));
    }

    #[test]
    fn viewport_releases_ring() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
        let size = gpui_kit::Size {
            width: px(1000.),
            height: px(800.),
        };
        let (vw, vh) = viewport_of(size);
        assert!(close(vw, 1000. - RING * 2.0));
        assert!(close(vh, 800. - RING * 2.0));
    }

    #[test]
    fn region_is_viewport_capped() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
        // 整图小于视口:区域 = 整图(fit 全览)
        let r = crate::kits::mermaid::region_of((400., 300.), (1000., 800.));
        assert!(close(r.0, 400.) && close(r.1, 300.));
        // 放大超出视口:区域 = 视口(裁剪!)
        let r = crate::kits::mermaid::region_of((4000., 3000.), (1000., 800.));
        assert!(close(r.0, 1000.) && close(r.1, 800.));
        // 跨轴混合:宽超 高不超
        let r = crate::kits::mermaid::region_of((4000., 300.), (1000., 800.));
        assert!(close(r.0, 1000.) && close(r.1, 300.));
    }

    #[test]
    fn clamp_pan_bounds_and_quantizes() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
        // 整图 ≤ 视口:恒 0
        let p = crate::kits::mermaid::clamp_pan((50., 50.), (400., 300.), (1000., 800.));
        assert!(close(p.0, 0.) && close(p.1, 0.));
        // 上界 = 整图 − 视口;负值钳 0;8px 量化吸附
        let p = crate::kits::mermaid::clamp_pan((-5., 2900.), (1000., 3000.), (1000., 800.));
        assert!(close(p.0, 0.) && close(p.1, 2200.));
        let p = crate::kits::mermaid::clamp_pan((1003., 0.), (2000., 100.), (1000., 800.));
        assert!(close(p.0, 1000.)); // 1003 → 量子 1000
    }
}
