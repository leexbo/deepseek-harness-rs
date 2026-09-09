//! macOS Dock 图标(运行时绘制)。
//!
//! NSImage 的 app-icon 位不支持 SVG,纯 raster 载体:图标源 =
//! `assets/logo.svg`(设计定稿,唯一权威):编译期内嵌,提取
//! path d(仅 M/C/V/Z 单 fill)与 viewBox(四边等距包住整条鲸鱼,
//! 不转正不裁剪),极简解析为路径操作 → core-graphics 位图上下文
//! 按 viewBox 等比缩放居中绘制(深色圆角底 = 主题 BASE #151517 +
//! 近白鲸鱼,即 dark 模式配色)→ CGImage → NSImage →
//! `setApplicationIconImage`。dev 构建每启一次重绘,release 源码
//! 重编才变。
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

/// 底 = 主题 BASE #151517;鲸鱼近白(dark 模式 fill #fff 同款)
const BG_RGB: (f64, f64, f64) = (21.0 / 255.0, 21.0 / 255.0, 23.0 / 255.0);
const WHALE_RGB: (f64, f64, f64) = (242.0 / 255.0, 242.0 / 255.0, 244.0 / 255.0);

/// 图标源 = `assets/logo.svg`(设计定稿:鲸鱼原样姿态,viewBox
/// 四边等距包住整条鲸鱼——不转正不裁剪;改动文件重编译即生效)。
/// 本文件是唯一权威,path/viewBox 均从它解析。
const LOGO_SVG: &str = include_str!("../assets/logo.svg");

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
struct WhalePath {
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
fn parse_svg_path(d: &str) -> Option<WhalePath> {
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
    Some(WhalePath { ops, bbox: bbox? })
}

/// 从 logo.svg 提取 path `d`(格式固定:path 标签内 `d="..."` 属性;
/// svg 标签无 d 属性,首个即鲸鱼)
fn extract_path_d(svg: &str) -> Option<&str> {
    let start = svg.find("d=\"")? + 3;
    let end = svg[start..].find('"')? + start;
    Some(&svg[start..end])
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

/// viewBox 必须完整包住鲸鱼 bbox(含控制点)。设计 viewBox
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
/// 返回鲸鱼的 meet 内框(背景内再留 4% 边距)。
fn draw_container(ctx: &CGContext) -> (f64, f64, f64, f64) {
    let bg = CGColor::new_generic_rgb(BG_RGB.0, BG_RGB.1, BG_RGB.2, 1.0);
    CGContext::set_fill_color_with_color(Some(ctx), Some(&bg));
    let m = (SIZE - GRID) / 2.0;
    add_rounded_rect(ctx, m, m, SIZE - m, SIZE - m, GRID * 0.225);
    CGContext::fill_path(Some(ctx));
    let inset = GRID * 0.04;
    (m + inset, m + inset, SIZE - m - inset, SIZE - m - inset)
}

/// 鲸鱼:整个 viewBox(含设计边距)`meet` 进内框。
/// translate 后 scale(s, −s)——CTM 后乘,得 p_out = T·S(p),
/// 翻转 y 使 SVG 坐标(向下)映射为 CG 坐标(向上)
fn draw_whale(
    ctx: &CGContext,
    whale: &WhalePath,
    viewbox: (f64, f64, f64, f64),
    frame: (f64, f64, f64, f64),
) {
    let layout = fit_centered(viewbox, frame);
    let fg = CGColor::new_generic_rgb(WHALE_RGB.0, WHALE_RGB.1, WHALE_RGB.2, 1.0);
    CGContext::set_fill_color_with_color(Some(ctx), Some(&fg));
    CGContext::translate_ctm(Some(ctx), layout.tx, layout.ty);
    CGContext::scale_ctm(Some(ctx), layout.scale, -layout.scale);
    CGContext::begin_path(Some(ctx));
    for op in &whale.ops {
        match *op {
            Op::Move(x, y) => CGContext::move_to_point(Some(ctx), x, y),
            Op::Curve(c1x, c1y, c2x, c2y, x, y) => {
                CGContext::add_curve_to_point(Some(ctx), c1x, c1y, c2x, c2y, x, y)
            }
            Op::Line(x, y) => CGContext::add_line_to_point(Some(ctx), x, y),
            Op::Close => CGContext::close_path(Some(ctx)),
        }
    }
    CGContext::fill_path(Some(ctx));
}

/// 完整图标(容器 + 鲸鱼)绘制到位图上下文;render 与测试共用同一
/// 绘制体(杜绝复制漂移)。
fn draw_icon(ctx: &CGContext, whale: &WhalePath, viewbox: (f64, f64, f64, f64)) {
    let frame = draw_container(ctx);
    draw_whale(ctx, whale, viewbox, frame);
}

/// 位图绘制并桥接 NSImage(失败返回 None)。
fn render(whale: &WhalePath, viewbox: (f64, f64, f64, f64)) -> Option<Retained<NSImage>> {
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
    draw_icon(&ctx, whale, viewbox);
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
    let Some(d) = extract_path_d(LOGO_SVG) else {
        eprintln!("[icon] logo.svg path 解析失败,跳过 Dock 图标");
        return;
    };
    let Some((vx, vy, vw, vh)) = extract_viewbox(LOGO_SVG) else {
        eprintln!("[icon] logo.svg viewBox 解析失败,跳过 Dock 图标");
        return;
    };
    let Some(whale) = parse_svg_path(d) else {
        eprintln!("[icon] 鲸鱼 path 解析失败,跳过 Dock 图标");
        return;
    };
    // covers 用 (x, y, w, h) 原始 viewBox;布局用角点形式 (x, y, x+w, y+h)
    if !viewbox_covers(whale.bbox, (vx, vy, vw, vh)) {
        eprintln!("[icon] viewBox 未包住鲸鱼,跳过 Dock 图标(检查 logo.svg)");
        return;
    }
    let Some(ns_image) = render(&whale, (vx, vy, vw, vh)) else {
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

    /// 完整图标落盘 /tmp/dsh_icon_dump.rgba(rgba→png 后人工比对用;
    /// 仅写 /tmp,不进仓库)
    #[test]
    fn icon_dump_for_inspection() {
        let d = extract_path_d(LOGO_SVG).unwrap();
        let viewbox = extract_viewbox(LOGO_SVG).unwrap();
        let whale = parse_svg_path(d).unwrap();
        let px = render_pixels(|ctx| draw_icon(ctx, &whale, viewbox));
        std::fs::write("/tmp/dsh_icon_dump.rgba", px).unwrap();
    }

    /// 内嵌 path 结构校验:仅 M/C/Z,4 子路径,434 数 = 4×2 + 71×6,
    /// 包围盒在 viewBox 内且宽 > 高(鲸鱼横卧)
    #[test]
    fn whale_path_structure_and_bbox() {
        let whale = parse_svg_path(extract_path_d(LOGO_SVG).expect("logo.svg 应有 d 属性"))
            .expect("内嵌 path 应可解析");
        let moves = whale
            .ops
            .iter()
            .filter(|o| matches!(o, Op::Move(..)))
            .count();
        let curves = whale
            .ops
            .iter()
            .filter(|o| matches!(o, Op::Curve(..)))
            .count();
        let lines = whale
            .ops
            .iter()
            .filter(|o| matches!(o, Op::Line(..)))
            .count();
        let closes = whale.ops.iter().filter(|o| matches!(o, Op::Close)).count();
        assert_eq!(moves, 4, "身体/气孔/眼/鳍四个子路径");
        assert_eq!(curves, 71);
        assert_eq!(lines, 1, "V 命令转为绝对直线");
        assert_eq!(closes, 4);
        let (x0, y0, x1, y1) = whale.bbox;
        assert!(
            x0 > -1.0 && y0 > 0.0 && x1 < 28.0 && y1 < 22.0,
            "bbox 应含于 logo viewBox(28×22)"
        );
        assert!(x1 - x0 > y1 - y0, "鲸鱼横卧(宽 > 高)");
        assert!(x0 < 0.0, "鲸吻越过左缘(源数据特征)");
        assert!(y1 > 21.0, "鲸腹贴近下缘(源数据特征)");
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

    /// fit_centered:meet 语义——宽约束取小缩放、两轴居中、鲸鱼落框内
    #[test]
    fn fit_centered_meets_and_centers() {
        let (vx, vy, vw, vh) = extract_viewbox(LOGO_SVG).unwrap();
        // 方形目标框(模拟背景内框)
        let frame = (100.0, 100.0, 924.0, 924.0);
        let layout = fit_centered((vx, vy, vw, vh), frame);
        // 宽约束(28.91 > 21.77)
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
/// 鲸鱼 bbox(设计意图)、按 viewBox meet 缩放垂直居中
#[cfg(test)]
mod logo_svg_tests {
    use super::*;

    #[test]
    fn logo_svg_extracts_path_and_viewbox() {
        let d = extract_path_d(LOGO_SVG).expect("应提取到 d");
        assert!(d.starts_with("M26.5174"), "path 以鲸鱼 M 开头");
        let whale = parse_svg_path(d).expect("d 应可解析");
        let curves = whale
            .ops
            .iter()
            .filter(|o| matches!(o, Op::Curve(..)))
            .count();
        assert_eq!(curves, 71, "path 结构应与定稿一致");
        let (vx, vy, vw, vh) = extract_viewbox(LOGO_SVG).expect("应提取到 viewBox");
        assert_eq!((vx, vy, vw, vh), (-1.09, 0.72, 28.91, 21.77));
    }

    /// covers 语义回归:入参是 (x, y, w, h) 原始 viewBox,不是角点;
    /// 曾误传角点导致右边界差 0.06 误判失败、图标被跳过
    #[test]
    fn viewbox_covers_accepts_raw_viewbox() {
        let (vx, vy, vw, vh) = extract_viewbox(LOGO_SVG).unwrap();
        let d = extract_path_d(LOGO_SVG).unwrap();
        let whale = parse_svg_path(d).unwrap();
        assert!(
            viewbox_covers(whale.bbox, (vx, vy, vw, vh)),
            "原始 viewBox (x,y,w,h) 应包住鲸鱼"
        );
        // 角点形式(w 被当成右边界)应误判——防调用方传错
        assert!(
            !viewbox_covers(whale.bbox, (vx, vy, vx + vw, vy + vh)),
            "角点形式应被拒绝(w 不是右边界)"
        );
    }

    #[test]
    fn viewbox_wraps_whale_with_equal_margins() {
        let d = extract_path_d(LOGO_SVG).unwrap();
        let whale = parse_svg_path(d).unwrap();
        let (x0, y0, x1, y1) = whale.bbox;
        let (vx, vy, vw, vh) = extract_viewbox(LOGO_SVG).unwrap();
        // 设计:viewBox 四边与鲸鱼 bbox 等距(≈1.0,含控制点)
        let (l, r, t, b) = (x0 - vx, (vx + vw) - x1, y0 - vy, (vy + vh) - y1);
        assert!(
            (l - r).abs() < 0.05 && (t - b).abs() < 0.05,
            "左右/上下边距应相等"
        );
        assert!(l > 0.9 && l < 1.1, "边距 ≈1.0(设计定稿值)");
    }

    /// render 网格不变量:鲸鱼映射后完整落在 824 方身内框里
    #[test]
    fn whale_fits_inside_icon_grid() {
        const GRID: f64 = 824.0;
        let (vx, vy, vw, vh) = extract_viewbox(LOGO_SVG).unwrap();
        let whale = parse_svg_path(extract_path_d(LOGO_SVG).unwrap()).unwrap();
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
        // 鲸鱼实际 bbox(真 SVG 点,含控制点)更靠内
        let (x0, y0, x1, y1) = whale.bbox;
        let (ox0, _) = map(x0, y0);
        let (ox1, _) = map(x1, y1);
        assert!(ox0 > m && ox1 < 1024.0 - m, "鲸鱼含在方身内");
    }
}
