//! macOS Dock 图标(运行时绘制)。
//!
//! NSImage 的 app-icon 位不支持 SVG,纯 raster 载体:图标源 =
//! `assets/logo.svg`(设计定稿,唯一权威):编译期内嵌,提取各
//! `<path>` 的 d(仅 M/C/V/Z)/fill 颜色/fill-rule 与 viewBox,
//! 极简解析为路径操作 → core-graphics 位图上下文按 viewBox 等比
//! 缩放居中分层绘制(深色圆角底 = 主题 BASE #212734 + 品牌色
//! 马形/圆点;evenodd 走 even-odd 填充,孔洞透底)→ CGImage →
//! NSImage → `setApplicationIconImage`。dev 构建每启一次重绘,
//! release 源码重编才变。
//!
//! 为什么运行时绘制:qlmanage 栅格化 `app-icon.svg.png` 输出纯黑
//! (缩放 transform 处理问题),弃离线产物走绘制;全 objc2 系(与
//! gpui 同源锁定版本),无新增 C 交付物。解析/布局是纯函数可单测;
//! 绘制失败仅 eprintln 跳过,绝不 panic(装饰性图标,失败无害)。

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSImage};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColor, CGColorSpace, CGContext,
    CGImageAlphaInfo,
};
use objc2_foundation::NSSize;

// NSImage::alloc() 由 AnyThread trait 提供(0.3.2 绑定将 NSImage 声明为
// AnyThread;MainThreadOnly 类型才带 mtm 参数版本)
use objc2::AnyThread as _;

/// 图标边长(高于 dock 默认 512 渲染;尺寸不足会被上采样变糊)
const SIZE: f64 = 1024.0;

/// 底 = 纯黑;马形/圆点按 logo.svg 声明的品牌色分层绘制
const BG_RGB: (f64, f64, f64) = (0.0, 0.0, 0.0);

/// 图标源 = `assets/logo.svg`(设计定稿:流马原样姿态;改动文件
/// 重编译即生效)。本文件是唯一权威,path/颜色/fill-rule/viewBox
/// 均从它解析。
const LOGO_SVG: &str = include_str!("../assets/logo.svg");

/// 单个 `<path>` 图层:解析产物 + 品牌色 + 填充规则
#[derive(Debug)]
struct LogoLayer {
    path: LogoPath,
    rgb: (f64, f64, f64),
    /// `fill-rule="evenodd"`(本数据两层的孔洞均按 evenodd 设计)
    even_odd: bool,
}

/// SVG path 解析产物(本图标数据实测仅 M/C/V/Z 四种命令)
#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Move(f64, f64),
    Curve(f64, f64, f64, f64, f64, f64),
    /// 绝对直线(V 转换后的产物:终点为解析时的当前点)
    Line(f64, f64),
    Close,
}

/// 解析结果:路径操作 + 包围盒(含控制点,防曲线外凸出界)
#[derive(Debug)]
struct LogoPath {
    ops: Vec<Op>,
    /// (x0, y0, x1, y1),SVG 坐标系(y 向下)
    bbox: (f64, f64, f64, f64),
}

/// 布局变换(纯函数可单测):
/// `p_out = (tx + scale·x, ty − scale·y)`——SVG y 向下 ↔ CG y 向上翻转
#[derive(Debug, Clone, Copy, PartialEq)]
struct Layout {
    scale: f64,
    tx: f64,
    ty: f64,
}

/// SVG `xMidYMid meet` 语义:viewBox(x, y, w, h)等比缩放、居中放进
/// 目标框(CG 坐标角点 x0, y0, x1, y1)。缩放取两轴较小者,另一轴居中。
fn fit_centered(viewbox: (f64, f64, f64, f64), frame: (f64, f64, f64, f64)) -> Layout {
    let (vx, vy, vw, vh) = viewbox;
    let (fx0, fy0, fx1, fy1) = frame;
    let scale = ((fx1 - fx0) / vw).min((fy1 - fy0) / vh);
    let (vcx, vcy) = (vx + vw / 2.0, vy + vh / 2.0);
    let (fcx, fcy) = ((fx0 + fx1) / 2.0, (fy0 + fy1) / 2.0);
    Layout {
        scale,
        tx: fcx - scale * vcx,
        ty: fcy + scale * vcy,
    }
}

/// 极简 SVG path 解析(M/C/V/Z 大写命令;不支持相对坐标与其余命令)。
/// 数据不满足 → `None`(调用方跳过图标,绝不 panic)。
/// 隐式重复:命令字母后连续出现的数字组按该命令的下一组参数处理
/// (SVG 规范;本数据为 M(2 数)/C(6 数)/V(1 数)/Z,见测试断言)。
fn parse_svg_path(d: &str) -> Option<LogoPath> {
    let bytes = d.as_bytes();
    let mut i = 0;
    let mut ops = Vec::new();
    let mut current = (0.0, 0.0); // V 需要上一终点坐标
    let mut bbox: Option<(f64, f64, f64, f64)> = None;
    let mut include = |x: f64, y: f64| {
        let b = bbox.get_or_insert((x, y, x, y));
        b.0 = b.0.min(x);
        b.1 = b.1.min(y);
        b.2 = b.2.max(x);
        b.3 = b.3.max(y);
    };
    while i < bytes.len() {
        // 命令字母
        let cmd = match bytes[i] {
            c @ (b'M' | b'C' | b'V' | b'Z') => {
                i += 1;
                c as char
            }
            c if c.is_ascii_whitespace() || c == b',' => {
                i += 1;
                continue;
            }
            _ => return None, // 相对坐标/其余命令/裸数字:不支持
        };
        match cmd {
            'M' => {
                let (x, y) = (num(bytes, &mut i)?, num(bytes, &mut i)?);
                current = (x, y);
                include(x, y);
                ops.push(Op::Move(x, y));
            }
            'C' => loop {
                let (c1x, c1y) = (num(bytes, &mut i)?, num(bytes, &mut i)?);
                let (c2x, c2y) = (num(bytes, &mut i)?, num(bytes, &mut i)?);
                let (x, y) = (num(bytes, &mut i)?, num(bytes, &mut i)?);
                include(c1x, c1y);
                include(c2x, c2y);
                include(x, y);
                current = (x, y);
                ops.push(Op::Curve(c1x, c1y, c2x, c2y, x, y));
                // 数字耗尽或下一字符是命令字母 → 本组结束
                let next = *bytes.get(i)?;
                if next.is_ascii_digit() || next == b'-' || next == b'.' {
                    continue;
                }
                break;
            },
            'V' => {
                // 竖直线:到 (当前 x, y) 的绝对直线
                let y = num(bytes, &mut i)?;
                let (x, _) = current;
                include(x, y);
                ops.push(Op::Line(x, y));
                current = (x, y);
            }
            'Z' => ops.push(Op::Close),
            _ => unreachable!(),
        }
    }
    Some(LogoPath { ops, bbox: bbox? })
}

/// 从 logo.svg 提取单个属性值(自 `from` 起找 `name="...";找不到返回 None)
fn extract_attr<'a>(svg: &'a str, name: &str, from: usize) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let start = svg[from..].find(&needle)? + from + needle.len();
    let end = svg[start..].find('"')? + start;
    Some(&svg[start..end])
}

/// `#rrggbb` → (r, g, b)(0-1;解析失败返回 None)
fn hex_rgb(hex: &str) -> Option<(f64, f64, f64)> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let ch = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(f64::from);
    Some((ch(0)? / 255.0, ch(2)? / 255.0, ch(4)? / 255.0))
}

/// 从 logo.svg 提取全部 path 图层(d + fill 色 + fill-rule;任一层
/// 缺 d/颜色或解析失败 → None,调用方跳过图标)
fn extract_layers(svg: &str) -> Option<Vec<LogoLayer>> {
    let mut layers = Vec::new();
    let mut from = 0;
    while let Some(tag_at) = svg[from..].find("<path") {
        let tag_start = from + tag_at;
        let d = extract_attr(svg, "d", tag_start)?;
        let fill = extract_attr(svg, "fill", tag_start)?;
        let even_odd = extract_attr(svg, "fill-rule", tag_start) == Some("evenodd");
        layers.push(LogoLayer {
            path: parse_svg_path(d)?,
            rgb: hex_rgb(fill)?,
            even_odd,
        });
        from = tag_start + 5;
    }
    (!layers.is_empty()).then_some(layers)
}

/// 从 logo.svg 提取 viewBox(x y w h;解析失败返回 None)
fn extract_viewbox(svg: &str) -> Option<(f64, f64, f64, f64)> {
    let start = svg.find("viewBox=\"")? + "viewBox=\"".len();
    let end = svg[start..].find('"')? + start;
    let nums: Vec<f64> = svg[start..end]
        .split_whitespace()
        .map(|s| s.parse::<f64>())
        .collect::<Result<_, _>>()
        .ok()?;
    Some((nums[0], nums[1], nums[2], nums[3]))
}

/// viewBox 必须完整包住流马 bbox(含控制点)。设计 viewBox
/// 四边等距;防护将来误改 logo.svg 造成裁剪(wordmark 的 clip
/// 曾切掉吻尖控制区)。
fn viewbox_covers(bbox: (f64, f64, f64, f64), viewbox: (f64, f64, f64, f64)) -> bool {
    let (x0, y0, x1, y1) = bbox;
    let (vx, vy, vw, vh) = viewbox;
    x0 >= vx && y0 >= vy && x1 <= vx + vw && y1 <= vy + vh
}

/// 读一个浮点数(空格/逗号分隔;失败即整条解析失败)
fn num(bytes: &[u8], i: &mut usize) -> Option<f64> {
    while *i < bytes.len() && (bytes[*i].is_ascii_whitespace() || bytes[*i] == b',') {
        *i += 1;
    }
    let start = *i;
    while *i < bytes.len()
        && (bytes[*i].is_ascii_digit() || matches!(bytes[*i], b'.' | b'-' | b'+' | b'e' | b'E'))
    {
        *i += 1;
    }
    if start == *i {
        return None;
    }
    std::str::from_utf8(&bytes[start..*i])
        .ok()?
        .parse::<f64>()
        .ok()
}

/// 圆角矩形路径(k = 0.5523 圆角控制点系数,CG 无直接 API)。
///
/// 四边四角齐全:move 后先走底边到 (x1−r, y0) 再进角弧——缺这条边时
/// 角弧从底边左端起笔,右下角被一条畸形长弧切掉(实测
/// 「Dock 图标右下角歪」的根因;path 仍闭合可填充,故肉眼只是角不对)。
fn add_rounded_rect(ctx: &CGContext, x0: f64, y0: f64, x1: f64, y1: f64, r: f64) {
    let k = 0.5523 * r;
    CGContext::begin_path(Some(ctx));
    CGContext::move_to_point(Some(ctx), x0 + r, y0);
    CGContext::add_line_to_point(Some(ctx), x1 - r, y0);
    CGContext::add_curve_to_point(Some(ctx), x1 - r + k, y0, x1, y0 + r - k, x1, y0 + r);
    CGContext::add_line_to_point(Some(ctx), x1, y1 - r);
    CGContext::add_curve_to_point(Some(ctx), x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    CGContext::add_line_to_point(Some(ctx), x0 + r, y1);
    CGContext::add_curve_to_point(Some(ctx), x0 + r - k, y1, x0, y1 - r + k, x0, y1 - r);
    CGContext::add_line_to_point(Some(ctx), x0, y0 + r);
    CGContext::add_curve_to_point(Some(ctx), x0, y0 + r - k, x0 + r - k, y0, x0 + r, y0);
    CGContext::close_path(Some(ctx));
}

/// 图标网格:824 方身居中于 1024 画布(Apple 模板),圆角 ≈185。
/// 背景按 **macOS 图标网格**:四周留透明边——铺满全幅观感「比标准
/// 图标大一圈 + 角变形」(实测)。
const GRID: f64 = 824.0;

/// 底色方身:深色圆角矩形(824 网格,圆角 22.5%;画布其余透明)。
/// 返回流马的 meet 内框(背景内再留 4% 边距)。
fn draw_container(ctx: &CGContext) -> (f64, f64, f64, f64) {
    let bg = CGColor::new_generic_rgb(BG_RGB.0, BG_RGB.1, BG_RGB.2, 1.0);
    CGContext::set_fill_color_with_color(Some(ctx), Some(&bg));
    let m = (SIZE - GRID) / 2.0;
    add_rounded_rect(ctx, m, m, SIZE - m, SIZE - m, GRID * 0.225);
    CGContext::fill_path(Some(ctx));
    let inset = GRID * 0.04;
    (m + inset, m + inset, SIZE - m - inset, SIZE - m - inset)
}

/// 单层:整个 viewBox `meet` 进内框,按图层品牌色填充。
/// translate 后 scale(s, −s)——CTM 后乘,得 p_out = T·S(p),
/// 翻转 y 使 SVG 坐标(向下)映射为 CG 坐标(向上);
/// evenodd 走 even-odd 填充(孔洞透底,非零绕法会填实)。
fn draw_layer(
    ctx: &CGContext,
    layer: &LogoLayer,
    viewbox: (f64, f64, f64, f64),
    frame: (f64, f64, f64, f64),
) {
    let layout = fit_centered(viewbox, frame);
    let fg = CGColor::new_generic_rgb(layer.rgb.0, layer.rgb.1, layer.rgb.2, 1.0);
    CGContext::set_fill_color_with_color(Some(ctx), Some(&fg));
    CGContext::translate_ctm(Some(ctx), layout.tx, layout.ty);
    CGContext::scale_ctm(Some(ctx), layout.scale, -layout.scale);
    CGContext::begin_path(Some(ctx));
    for op in &layer.path.ops {
        match *op {
            Op::Move(x, y) => CGContext::move_to_point(Some(ctx), x, y),
            Op::Curve(c1x, c1y, c2x, c2y, x, y) => {
                CGContext::add_curve_to_point(Some(ctx), c1x, c1y, c2x, c2y, x, y)
            }
            Op::Line(x, y) => CGContext::add_line_to_point(Some(ctx), x, y),
            Op::Close => CGContext::close_path(Some(ctx)),
        }
    }
    if layer.even_odd {
        CGContext::eo_fill_path(Some(ctx));
    } else {
        CGContext::fill_path(Some(ctx));
    }
}

/// 完整图标(容器 + 各层)绘制到位图上下文;render 与测试共用同一
/// 绘制体(杜绝复制漂移)。
fn draw_icon(ctx: &CGContext, layers: &[LogoLayer], viewbox: (f64, f64, f64, f64)) {
    let frame = draw_container(ctx);
    for layer in layers {
        draw_layer(ctx, layer, viewbox, frame);
    }
}

/// 位图绘制并桥接 NSImage(失败返回 None)。
fn render(layers: &[LogoLayer], viewbox: (f64, f64, f64, f64)) -> Option<Retained<NSImage>> {
    let space = CGColorSpace::new_device_rgb()?;
    let ctx = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            SIZE as usize,
            SIZE as usize,
            8,
            0,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )?
    };
    draw_icon(&ctx, layers, viewbox);
    let image = CGBitmapContextCreateImage(Some(&ctx))?;
    Some(NSImage::initWithCGImage_size(
        NSImage::alloc(),
        &image,
        NSSize {
            width: SIZE,
            height: SIZE,
        },
    ))
}

/// 设置 Dock 图标(须主线程;失败仅 eprintln,不 panic)
pub fn set_app_icon_1024() {
    let Some(marker) = MainThreadMarker::new() else {
        eprintln!("[icon] 非主线程,跳过 Dock 图标");
        return;
    };
    let Some(layers) = extract_layers(LOGO_SVG) else {
        eprintln!("[icon] logo.svg path 图层解析失败,跳过 Dock 图标");
        return;
    };
    let Some((vx, vy, vw, vh)) = extract_viewbox(LOGO_SVG) else {
        eprintln!("[icon] logo.svg viewBox 解析失败,跳过 Dock 图标");
        return;
    };
    // covers 用 (x, y, w, h) 原始 viewBox;布局用角点形式 (x, y, x+w, y+h)
    if !layers
        .iter()
        .all(|l| viewbox_covers(l.path.bbox, (vx, vy, vw, vh)))
    {
        eprintln!("[icon] viewBox 未包住流马,跳过 Dock 图标(检查 logo.svg)");
        return;
    }
    let Some(ns_image) = render(&layers, (vx, vy, vw, vh)) else {
        eprintln!("[icon] 位图绘制失败,跳过 Dock 图标");
        return;
    };
    let app = NSApplication::sharedApplication(marker);
    unsafe {
        app.setApplicationIconImage(Some(&ns_image));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用位图:建 1024 RGBA 上下文 → 绘制 → 回读像素
    /// (容器对称性回归与人工落盘共用)
    fn render_pixels(draw: impl FnOnce(&CGContext)) -> Vec<u8> {
        use objc2_core_graphics::CGBitmapContextGetData;
        let space = CGColorSpace::new_device_rgb().unwrap();
        let ctx = unsafe {
            CGBitmapContextCreate(
                std::ptr::null_mut(),
                SIZE as usize,
                SIZE as usize,
                8,
                0,
                Some(&space),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
            .unwrap()
        };
        draw(&ctx);
        let data = CGBitmapContextGetData(Some(&ctx));
        assert!(!data.is_null());
        unsafe { std::slice::from_raw_parts(data as *const u8, SIZE as usize * SIZE as usize * 4) }
            .to_vec()
    }

    fn row_span(px: &[u8], y: usize) -> Option<(usize, usize)> {
        let (mut lo, mut hi) = (None, None);
        for x in 0..SIZE as usize {
            if px[(y * SIZE as usize + x) * 4 + 3] > 128 {
                lo = lo.or(Some(x));
                hi = Some(x);
            }
        }
        lo.zip(hi)
    }

    /// 容器对称性回归(根因:add_rounded_rect 缺底边线,右下
    /// 角被畸形长弧切掉——path 仍闭合可填充,肉眼仅「角歪」)。
    /// 位图实测:任意行的占位区间必须与镜像行一致。
    #[test]
    fn container_fill_is_point_symmetric() {
        let px = render_pixels(|ctx| {
            draw_container(ctx);
        });
        let n = SIZE as usize;
        let tol = 3; // 抗锯齿容差
        for y in 0..n {
            let Some((l0, h0)) = row_span(&px, y) else {
                continue;
            };
            let Some((l1, h1)) = row_span(&px, n - 1 - y) else {
                panic!("行 {y} 有占位而镜像行空");
            };
            assert!(
                (l0 as i32 - l1 as i32).abs() <= tol && (h0 as i32 - h1 as i32).abs() <= tol,
                "行 {y} 占位 ({l0},{h0}) 与镜像行 ({l1},{h1}) 不对称(右下角歪回归)"
            );
            let Some((cl0, ch0)) = col_span(&px, y) else {
                continue;
            };
            let Some((cl1, ch1)) = col_span(&px, n - 1 - y) else {
                panic!("列 {y} 有占位而镜像列空");
            };
            assert!(
                (cl0 as i32 - cl1 as i32).abs() <= tol && (ch0 as i32 - ch1 as i32).abs() <= tol,
                "列 {y} 占位 ({cl0},{ch0}) 与镜像列 ({cl1},{ch1}) 不对称"
            );
        }
    }

    fn col_span(px: &[u8], x: usize) -> Option<(usize, usize)> {
        let (mut lo, mut hi) = (None, None);
        for y in 0..SIZE as usize {
            if px[(y * SIZE as usize + x) * 4 + 3] > 128 {
                lo = lo.or(Some(y));
                hi = Some(y);
            }
        }
        lo.zip(hi)
    }

    /// 完整图标落盘 /tmp/liuma_icon_dump.rgba(rgba→png 后人工比对用;
    /// 仅写 /tmp,不进仓库)
    #[test]
    fn icon_dump_for_inspection() {
        let layers = extract_layers(LOGO_SVG).unwrap();
        let viewbox = extract_viewbox(LOGO_SVG).unwrap();
        let px = render_pixels(|ctx| draw_icon(ctx, &layers, viewbox));
        std::fs::write("/tmp/liuma_icon_dump.rgba", px).unwrap();
    }

    /// 数一层 path 的 (Move, Curve, Close) 操作数
    fn op_counts(path: &LogoPath) -> (usize, usize, usize) {
        let mut m = 0;
        let mut c = 0;
        let mut z = 0;
        for o in &path.ops {
            match o {
                Op::Move(..) => m += 1,
                Op::Curve(..) => c += 1,
                Op::Line(..) => {}
                Op::Close => z += 1,
            }
        }
        (m, c, z)
    }

    /// 内嵌图层结构校验(用户设计定稿):马形 + 圆点两层,纯 M/C/Z,
    /// 包围盒均含于 viewBox(1024 画布)
    #[test]
    fn liuma_path_structure_and_bbox() {
        let layers = extract_layers(LOGO_SVG).expect("logo.svg 应解析出图层");
        assert_eq!(layers.len(), 2, "马形 + 圆点两层");
        let horse = &layers[0];
        assert_eq!(
            op_counts(&horse.path),
            (20, 249, 20),
            "马形 Move/Curve/Close"
        );
        assert!(horse.even_odd, "马形 evenodd(孔洞透底)");
        assert_eq!(horse.rgb, (187.0 / 255.0, 138.0 / 255.0, 60.0 / 255.0));
        let dots = &layers[1];
        assert_eq!(op_counts(&dots.path), (4, 8, 4), "圆点 Move/Curve/Close");
        assert_eq!(dots.rgb, (53.0 / 255.0, 225.0 / 255.0, 243.0 / 255.0));
        let (x0, y0, x1, y1) = horse.path.bbox;
        assert!(
            (164.0..=875.0).contains(&x0)
                && (280.0..=752.0).contains(&y0)
                && x1 <= 876.0
                && y1 <= 753.0,
            "马形 bbox 应与设计定稿一致"
        );
        assert!(x1 - x0 > y1 - y0, "流马横卧(宽 > 高)");
    }

    /// 解析器拒绝:相对坐标/未知命令/裸数字/非数字字符
    #[test]
    fn parse_rejects_unsupported() {
        assert!(parse_svg_path("m1 2").is_none(), "相对命令不支持");
        assert!(parse_svg_path("M 1 2 L 3 4").is_none(), "L 命令不支持");
        assert!(
            parse_svg_path("M 1 2 3").is_none(),
            "M 后裸数字组按隐式 L 拒绝"
        );
        assert!(parse_svg_path("garbage").is_none());
        assert!(parse_svg_path("").is_none(), "空串无 bbox");
        assert!(parse_svg_path("M 1 2 C 1 2 3 4 5").is_none(), "C 参数不齐");
        // V 单参数竖直线:复用当前 x(2,3)→(2,5);参数缺失拒绝
        let v = parse_svg_path("M 1 2 C 1 2 3 4 5 6 V 9").expect("V 应支持");
        assert!(v.ops.contains(&Op::Line(5.0, 9.0)));
        assert!(parse_svg_path("M 1 2 V").is_none(), "V 缺参数拒绝");
    }

    /// fit_centered:meet 语义——宽约束取小缩放、两轴居中、流马落框内
    #[test]
    fn fit_centered_meets_and_centers() {
        let (vx, vy, vw, vh) = extract_viewbox(LOGO_SVG).unwrap();
        // 方形目标框(模拟背景内框)
        let frame = (100.0, 100.0, 924.0, 924.0);
        let layout = fit_centered((vx, vy, vw, vh), frame);
        // 方形 viewBox:两轴缩放相同,取 min 仍是该值
        let expect_scale = (frame.2 - frame.0) / vw;
        assert!((layout.scale - expect_scale).abs() < 1e-9, "取两轴较小缩放");
        // viewBox 中心 → 框中心(CG y 翻转不影响中心点)
        let map = |x: f64, y: f64| (layout.tx + layout.scale * x, layout.ty - layout.scale * y);
        let (cx, cy) = map(vx + vw / 2.0, vy + vh / 2.0);
        assert!(
            (cx - 512.0).abs() < 1e-9 && (cy - 512.0).abs() < 1e-9,
            "中心对齐"
        );
        // viewBox 四角映射进框(viewBox 高度方向留白居中)
        let (_, top_y) = map(vx, vy + vh);
        let bottom_y = map(vx, vy).1;
        let out_h = layout.scale * vh;
        assert!((top_y - (512.0 - out_h / 2.0)).abs() < 1e-9, "上下留白居中");
        assert!(bottom_y > top_y, "y 翻转:SVG 顶边在画布上方");
    }
}

/// logo.svg 来源与 viewBox 布局:提取正确、viewBox 四边等距包住
/// 流马 bbox(设计意图)、按 viewBox meet 缩放垂直居中
#[cfg(test)]
mod logo_svg_tests {
    use super::*;

    #[test]
    fn logo_svg_extracts_path_and_viewbox() {
        let layers = extract_layers(LOGO_SVG).expect("应提取到图层");
        assert!(
            layers[0]
                .path
                .ops
                .first()
                .is_some_and(|o| matches!(o, Op::Move(..))),
            "首层以 M 开头"
        );
        let (vx, vy, vw, vh) = extract_viewbox(LOGO_SVG).expect("应提取到 viewBox");
        assert_eq!((vx, vy, vw, vh), (0.0, 0.0, 1024.0, 1024.0));
    }

    /// covers 语义回归:入参是 (x, y, w, h) 原始 viewBox,不是角点;
    /// 曾误传角点导致右边界差 0.06 误判失败、图标被跳过。
    /// 真实数据只验「原始形式覆盖」;「角点形式被拒」用合成数据构造
    /// 可判别用例(是否误拒取决于具体数字,不能押在 logo 数据上)。
    #[test]
    fn viewbox_covers_accepts_raw_viewbox() {
        let (vx, vy, vw, vh) = extract_viewbox(LOGO_SVG).unwrap();
        let layers = extract_layers(LOGO_SVG).unwrap();
        assert!(
            layers
                .iter()
                .all(|l| viewbox_covers(l.path.bbox, (vx, vy, vw, vh))),
            "原始 viewBox (x,y,w,h) 应包住所有图层"
        );
        // 合成:角点 (0,0,9.9,9.9) 被当 (x,y,w,h) 时右界 9.9 < bbox x1 10.0 → 拒
        assert!(
            !viewbox_covers((9.0, 9.0, 10.0, 10.0), (0.0, 0.0, 9.9, 9.9)),
            "角点形式应被拒绝(w 不是右边界)"
        );
    }

    /// 画布占用:1024 viewBox 里流马四边留白(不贴边)且合并包围盒
    /// 两个方向占用画布 ≥40%(设计构图饱满;防误缩/误裁)
    #[test]
    fn viewbox_wraps_liuma_with_padding() {
        let layers = extract_layers(LOGO_SVG).unwrap();
        let (vx, vy, vw, vh) = extract_viewbox(LOGO_SVG).unwrap();
        let (mut ux0, mut uy0, mut ux1, mut uy1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for l in &layers {
            let (x0, y0, x1, y1) = l.path.bbox;
            assert!(
                x0 > vx && y0 > vy && x1 < vx + vw && y1 < vy + vh,
                "图层贴边"
            );
            ux0 = ux0.min(x0);
            uy0 = uy0.min(y0);
            ux1 = ux1.max(x1);
            uy1 = uy1.max(y1);
        }
        assert!(
            (ux1 - ux0) / vw > 0.4 && (uy1 - uy0) / vh > 0.4,
            "流马占画布比例过低"
        );
    }

    /// render 网格不变量:流马映射后完整落在 824 方身内框里
    #[test]
    fn liuma_fits_inside_icon_grid() {
        const GRID: f64 = 824.0;
        let (vx, vy, vw, vh) = extract_viewbox(LOGO_SVG).unwrap();
        let layers = extract_layers(LOGO_SVG).unwrap();
        let mark = &layers[0].path;
        let m = (1024.0 - GRID) / 2.0;
        let inset = GRID * 0.04;
        let frame = (m + inset, m + inset, 1024.0 - m - inset, 1024.0 - m - inset);
        let layout = fit_centered((vx, vy, vw, vh), frame);
        // viewBox 四角映射应在框内(meet 保证)
        let map = |x: f64, y: f64| (layout.tx + layout.scale * x, layout.ty - layout.scale * y);
        for (x, y) in [(vx, vy), (vx + vw, vy), (vx, vy + vh), (vx + vw, vy + vh)] {
            let (ox, oy) = map(x, y);
            assert!(
                ox >= frame.0 - 1e-9 && ox <= frame.2 + 1e-9,
                "viewBox 角 x 出框:{ox}"
            );
            assert!(
                oy >= frame.1 - 1e-9 && oy <= frame.3 + 1e-9,
                "viewBox 角 y 出框:{oy}"
            );
        }
        // 流马实际 bbox(真 SVG 点,含控制点)更靠内
        let (x0, y0, x1, y1) = mark.bbox;
        let (ox0, _) = map(x0, y0);
        let (ox1, _) = map(x1, y1);
        assert!(ox0 > m && ox1 < 1024.0 - m, "流马含在方身内");
    }
}
