//! 双盘主题(macOS 原生灰阶):深色盘对照 Finder 暗色的石墨灰阶,
//! 浅色盘对照系统亮色;色板内联为唯一来源 → gpui-component
//! [`ThemeColor`] 映射。设置页「外观」三档(浅色/深色/跟随系统)经
//! [`apply`] 实装:启动读 settings.json、设置点击即切、「跟随系统」
//! 由窗口外观观察者驱动(shell::store 挂 `observe_window_appearance`)。
//!
//! 调用点形态:`theme::BASE()` —— SCREAMING_CASE 取值 fn 保持原常量
//! 调用形态;运行时按 [`MODE`]
//! 原子分发双盘,渲染热路径 = 一次 Relaxed load + 字段拷贝。

#![allow(non_snake_case)]

use std::sync::atomic::{AtomicU8, Ordering};

use gpui_kit::{App, Rgba, Window, WindowAppearance};
use gpui_kit::component::{Theme, ThemeMode};

// ── 外观档位(与 registry settings.json appearance 字段同词汇)──

/// 设置档:浅色 / 深色 / 跟随系统(registry 校验 light/dark/system;
/// 判别值即声明序 as u8,勿重排)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Appearance {
    /// 浅色
    Light = 0,
    /// 深色
    Dark = 1,
    /// 跟随系统(窗口外观观察者驱动实时切换)
    System = 2,
}

impl Appearance {
    /// settings.json 值 → 档位(未知值回落深色,与 registry 缺省一致)
    pub fn parse(s: &str) -> Self {
        match s {
            "light" => Self::Light,
            "system" => Self::System,
            _ => Self::Dark,
        }
    }
}

// ── 双盘色板 ─────────────────────────────────────────────────

/// 一套完整色板(深浅两盘同构;字段语义见各取值 fn 文档)
pub struct Palette {
    pub base: Rgba,
    pub sidebar: Rgba,
    pub ink: Rgba,
    pub layer: Rgba,
    pub card: Rgba,
    pub dock: Rgba,
    pub brand: Rgba,
    pub danger: Rgba,
    pub success: Rgba,
    pub warn: Rgba,
    pub bubble: Rgba,
    pub label: Rgba,
    pub label_2: Rgba,
    pub label_3: Rgba,
    pub caption: Rgba,
    pub border: Rgba,
    pub border_2: Rgba,
    pub code: Rgba,
    pub ongoing: Rgba,
    pub glass_bg: Rgba,
    pub glass_border: Rgba,
    /// 工具卡扫光渐变端点(半透明,亮随暗反色)
    pub sweep: Rgba,
    /// 时间刻度非激活色(半透明,亮随暗反色)
    pub tick_idle: Rgba,
}

/// 不透明 hex → RGBA(常量构造:gpui 的 `rgb()` 非 const,字段为 0..1 f32)
const fn color(hex: u32, a: f32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a,
    }
}

/// 深色盘:Finder 石墨版偏灰,取更深黑;
/// 层级 = 背景最黑、面亮一档、hover 再亮。文字取
/// label 族 alpha 语义色,语义色取系统色暗形态
const fn dark_palette() -> Palette {
    Palette {
        base: color(0x141416, 1.0),
        sidebar: color(0x1A1A1C, 1.0),
        ink: color(0x000000, 1.0),
        layer: color(0x202023, 1.0),
        card: color(0x232326, 1.0),
        dock: color(0x2E2E31, 1.0),
        brand: color(0x0A84FF, 1.0),
        danger: color(0xFF453A, 1.0),
        success: color(0x30D158, 1.0),
        warn: color(0xFF9F0A, 1.0),
        bubble: color(0x262629, 1.0),
        label: color(0xFFFFFF, 1.0),
        label_2: color(0xEBEBF5, 0.72),
        label_3: color(0xEBEBF5, 0.55),
        caption: color(0xEBEBF5, 0.38),
        border: color(0xFFFFFF, 0.08),
        border_2: color(0xFFFFFF, 0.14),
        code: color(0x101012, 1.0),
        ongoing: color(0x0A84FF, 1.0),
        glass_bg: color(0xFFFFFF, 0.10),
        glass_border: color(0xFFFFFF, 0.16),
        sweep: color(0xFFFFFF, 0.07),
        tick_idle: color(0xFFFFFF, 0.22),
    }
}

/// 浅色盘:macOS 亮色系统色(灰阶与深盘同构反演;文字取 label 族
/// alpha 语义色,语义色取系统色亮形态)
const fn light_palette() -> Palette {
    Palette {
        base: color(0xFFFFFF, 1.0),
        sidebar: color(0xF0F0F2, 1.0),
        ink: color(0x000000, 1.0),
        layer: color(0xECECEE, 1.0),
        card: color(0xFFFFFF, 1.0),
        dock: color(0xE9E9EB, 1.0),
        brand: color(0x007AFF, 1.0),
        danger: color(0xFF3B30, 1.0),
        success: color(0x34C759, 1.0),
        warn: color(0xFF9500, 1.0),
        bubble: color(0xE9E9EB, 1.0),
        label: color(0x1D1D1F, 1.0),
        label_2: color(0x3C3C43, 0.72),
        label_3: color(0x3C3C43, 0.50),
        caption: color(0x3C3C43, 0.35),
        border: color(0x000000, 0.10),
        border_2: color(0x000000, 0.16),
        code: color(0xF7F7F9, 1.0),
        ongoing: color(0x007AFF, 1.0),
        glass_bg: color(0x000000, 0.06),
        glass_border: color(0x000000, 0.14),
        sweep: color(0x000000, 0.08),
        tick_idle: color(0x000000, 0.22),
    }
}

static PALETTES: [Palette; 2] = [light_palette(), dark_palette()];

// 0 = light / 1 = dark(下标即 PALETTES 下标)
const M_LIGHT: u8 = 0;
const M_DARK: u8 = 1;

/// 当前生效盘(apply 写,取值 fn 读)
static MODE: AtomicU8 = AtomicU8::new(M_DARK);

fn cur() -> &'static Palette {
    &PALETTES[MODE.load(Ordering::Relaxed) as usize]
}

/// 指定模式对应盘(mermaid 纯函数化测试用)
pub(crate) fn palette_of(mode: ThemeMode) -> &'static Palette {
    &PALETTES[if mode.is_dark() { M_DARK as usize } else { M_LIGHT as usize }]
}

// ── 取值 fn(调用点保持原常量形态;语义文档在此处)──────────

/// 主背景(深盘 macOS 暗色窗口底石墨灰 / 浅盘纯白)
pub fn BASE() -> Rgba {
    cur().base
}
/// 侧栏/状态栏面板底(深盘 Finder 暗色侧栏,略浅于内容区 / 浅盘
/// 浅灰;gpui-component list 面同源)
pub fn SIDEBAR() -> Rgba {
    cur().sidebar
}
/// 纯黑双盘锚(轨迹 diff 遮挡罩、gpui-component 侧栏方案色 token;
/// 不随盘反色——遮挡语义恒为暗)
pub fn INK() -> Rgba {
    cur().ink
}
/// 浮层/hover 层底
pub fn LAYER() -> Rgba {
    cur().layer
}
/// 输入卡/卡片底(深盘 macOS systemGray5 #2C2C2E / 浅盘纯白——
/// 白画布上灰底显脏,靠边框+阴影分层)
pub fn CARD() -> Rgba {
    cur().card
}
/// 按钮底/次级填充(深盘 systemGray4 系 / 浅盘极浅灰——chip 类填充
/// 在白底上只求隐约成形,过深即灰蒙蒙)
pub fn DOCK() -> Rgba {
    cur().dock
}
/// 品牌蓝(macOS systemBlue:深盘 #0A84FF / 浅盘 #007AFF)
pub fn BRAND() -> Rgba {
    cur().brand
}
/// 危险(systemRed)
pub fn DANGER() -> Rgba {
    cur().danger
}
/// 成功(systemGreen)
pub fn SUCCESS() -> Rgba {
    cur().success
}
/// 警告(systemOrange)
pub fn WARN() -> Rgba {
    cur().warn
}
/// 用户气泡底
pub fn BUBBLE() -> Rgba {
    cur().bubble
}
/// 文字一级(深盘纯白 / 浅盘近黑)
pub fn LABEL() -> Rgba {
    cur().label
}
/// 文字二级(macOS label 族 alpha 语义色)
pub fn LABEL_2() -> Rgba {
    cur().label_2
}
/// 文字三级
pub fn LABEL_3() -> Rgba {
    cur().label_3
}
/// 说明文字
pub fn CAPTION() -> Rgba {
    cur().caption
}
/// 弱边框(近景白/黑 8%/10%)
pub fn BORDER() -> Rgba {
    cur().border
}
/// 二级发丝线(终端卡横幅与输出的分界)
pub fn BORDER_2() -> Rgba {
    cur().border_2
}
/// 代码块/终端卡表面(深盘比 BASE 深一阶 / 浅盘比 BASE 灰一阶)
pub fn CODE() -> Rgba {
    cur().code
}
/// 运行进行色(终端卡 running 状态点/进行态指示;StateDot ongoing
/// 同色,随 BRAND 走 systemBlue)
pub fn ONGOING() -> Rgba {
    cur().ongoing
}
/// 玻璃态填充(激活 tab pill;无 backdrop blur 以半透明近似磨砂)
pub fn GLASS_BG() -> Rgba {
    cur().glass_bg
}
/// 玻璃态描边
pub fn GLASS_BORDER() -> Rgba {
    cur().glass_border
}
/// 工具卡扫光渐变端点(亮随暗反色)
pub fn SWEEP() -> Rgba {
    cur().sweep
}
/// 时间刻度非激活色(亮随暗反色)
pub fn TICK_IDLE() -> Rgba {
    cur().tick_idle
}
/// 全透明(非激活行底;双盘同值)
pub fn TRANSPARENT() -> Rgba {
    color(0x000000, 0.0)
}

// ── 运行时状态与三档应用 ─────────────────────────────────────

/// 用户档位(0/1/2 = Light/Dark/System)
static CHOICE: AtomicU8 = AtomicU8::new(1);
/// 对 NSApp 的强制外观(0 none / 1 light / 2 dark / 255 未设过)
static FORCED: AtomicU8 = AtomicU8::new(255);
/// 上次 apply 的 (档位, 生效盘) —— 幂等守卫,防「强制外观 → 系统
/// 观察者 → 再 apply」回环
static LAST_CHOICE: AtomicU8 = AtomicU8::new(255);
static LAST_MODE: AtomicU8 = AtomicU8::new(255);

/// 当前用户档位(设置页高亮与外观观察者判别用)
pub fn current_appearance() -> Appearance {
    match CHOICE.load(Ordering::Relaxed) {
        0 => Appearance::Light,
        2 => Appearance::System,
        _ => Appearance::Dark,
    }
}

/// 当前生效盘是否深色(自绘语法高亮/轨迹配色的分发开关)
pub fn is_dark() -> bool {
    MODE.load(Ordering::Relaxed) == M_DARK
}

/// 当前生效盘对应 ThemeMode
pub fn mode() -> ThemeMode {
    if is_dark() {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    }
}

/// 应用外观档(启动装配与设置页切换同一入口)。
///
/// 流程:System 档先解除 NSApp 强制外观(带着强制值读到的是被强制的
/// 外观,不是真实系统外观)→ `cx.window_appearance()` 解析生效盘 →
/// (档位,生效盘)均未变即幂等返回 → [`Theme::change`] 打底(库默认
/// 盘整铺,会冲掉手改 token)→ 重铺 dsh token → `sync_base`(Base 层
/// 镜像:滚动条等,global_mut 直改不下推)→ 全窗刷新。显式浅/深档同
/// 时强制 NSApp 外观,原生交通灯/边框跟主题,避免「应用深色 + 系统
/// 浅色」的原生 chrome 撕裂。
pub fn apply(choice: Appearance, window: Option<&mut Window>, cx: &mut App) {
    set_forced_appearance(
        match choice {
            Appearance::Light => Some(WindowAppearance::Light),
            Appearance::Dark => Some(WindowAppearance::Dark),
            Appearance::System => None,
        },
        cx,
    );
    // 显式浅/深档直接定盘;仅 System 档读系统外观(此时强制已解除,
    // 读到的是真实系统值)
    let m = match choice {
        Appearance::Light => ThemeMode::Light,
        Appearance::Dark => ThemeMode::Dark,
        Appearance::System => ThemeMode::from(cx.window_appearance()),
    };
    let mode_flag = u8::from(m.is_dark());
    let choice_flag = choice as u8;
    if LAST_CHOICE.load(Ordering::Relaxed) == choice_flag
        && LAST_MODE.load(Ordering::Relaxed) == mode_flag
    {
        return;
    }
    LAST_CHOICE.store(choice_flag, Ordering::Relaxed);
    LAST_MODE.store(mode_flag, Ordering::Relaxed);
    CHOICE.store(choice_flag, Ordering::Relaxed);
    MODE.store(mode_flag, Ordering::Relaxed);
    Theme::change(m, window, cx);
    apply_tokens(m, cx);
    cx.refresh_windows();
}

/// 测试装配:固定深色盘(UI 测试同源基线;生产入口走 main.rs 直读
/// settings.json 档位的 apply)。每个测试是全新 App,而幂等守卫是
/// 进程级——先复位,否则第二个测试的打底会被短路。
#[cfg(test)]
pub fn init(cx: &mut App) {
    LAST_CHOICE.store(255, Ordering::Relaxed);
    LAST_MODE.store(255, Ordering::Relaxed);
    apply(Appearance::Dark, None, cx);
}

/// NSApp 强制外观(值未变不重设,防观察者空转)
fn set_forced_appearance(target: Option<WindowAppearance>, cx: &mut App) {
    let flag = match target {
        None => 0,
        Some(WindowAppearance::Dark | WindowAppearance::VibrantDark) => 2,
        Some(WindowAppearance::Light | WindowAppearance::VibrantLight) => 1,
    };
    if FORCED.load(Ordering::Relaxed) != flag {
        FORCED.store(flag, Ordering::Relaxed);
        cx.set_window_appearance(target);
    }
}

/// dsh 色板 → gpui-component token(双盘同构映射;`Theme::change`
/// 用库默认盘整铺后必须重铺一遍)。
fn apply_tokens(m: ThemeMode, cx: &mut App) {
    let p = palette_of(m);
    // 品牌/语义色填充面上的前景:双盘均取白(浅盘 label 近黑,不能
    // 用作填充面上的前景)
    let on_fill = if m.is_dark() {
        p.label.into()
    } else {
        color(0xFFFFFF, 1.0).into()
    };
    let t = Theme::global_mut(cx);
    let c = &mut t.colors;
    c.background = p.base.into();
    c.foreground = p.label.into();
    c.border = p.border.into();
    c.input = p.border.into();
    c.caret = p.brand.into();
    c.ring = p.brand.into();
    c.selection = Rgba { a: 0.3, ..p.brand }.into();
    c.primary = p.brand.into();
    c.primary_foreground = on_fill;
    c.primary_hover = p.brand.into();
    c.primary_active = p.brand.into();
    c.secondary = p.dock.into();
    c.secondary_foreground = p.label_2.into();
    c.secondary_hover = p.dock.into();
    c.secondary_active = p.dock.into();
    c.muted = p.layer.into();
    c.muted_foreground = p.label_3.into();
    c.accent = p.layer.into();
    c.accent_foreground = p.label.into();
    c.danger = p.danger.into();
    c.danger_foreground = on_fill;
    c.success = p.success.into();
    c.success_foreground = on_fill;
    c.popover = p.layer.into();
    c.popover_foreground = p.label.into();
    c.list = p.sidebar.into();
    c.list_hover = p.layer.into();
    c.list_active = p.layer.into();
    c.sidebar = p.ink.into();
    c.sidebar_border = p.border.into();
    c.sidebar_foreground = p.label_2.into();
    c.sidebar_accent = p.layer.into();
    c.sidebar_accent_foreground = p.label.into();
    c.scrollbar = p.base.into();
    c.scrollbar_thumb = p.dock.into();
    // 标题栏融入窗口画布(BASE):侧栏/内容区胶囊卡浮于其上,
    // 顶条与页边距同色才有「浮动」层次,不再画底线
    c.title_bar = p.base.into();
    c.title_bar_border = p.base.into();
    t.radius = gpui_kit::px(8.);
    t.radius_lg = gpui_kit::px(12.);
    // Base 层镜像(滚动条等直接取样 gpui_base::Theme,global_mut 直改不下推)
    Theme::sync_base(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{Hsla, TestAppContext};

    /// 双盘锚定:深盘 = macOS 石墨基值,浅盘 = 纯白底;关键字段两盘
    /// 互异;INK 双盘恒黑(diff 遮挡罩语义不随盘反色)
    #[test]
    fn palettes_distinct_and_anchored() {
        let l = &PALETTES[M_LIGHT as usize];
        let d = &PALETTES[M_DARK as usize];
        assert_eq!(d.base, color(0x141416, 1.0));
        assert_eq!(l.base, color(0xFFFFFF, 1.0));
        assert_ne!(d.label, l.label);
        assert_ne!(d.brand, l.brand);
        assert_ne!(d.code, l.code);
        assert_eq!(d.ink, l.ink);
        assert_eq!(d.ink, color(0x000000, 1.0));
    }

    /// 档位解析与 registry settings.json 词汇一致,未知值回落深色
    #[test]
    fn appearance_parse_matches_registry() {
        assert_eq!(Appearance::parse("light"), Appearance::Light);
        assert_eq!(Appearance::parse("dark"), Appearance::Dark);
        assert_eq!(Appearance::parse("system"), Appearance::System);
        assert_eq!(Appearance::parse("whatever"), Appearance::Dark);
    }

    /// dsh token 铺设:只动本 App 的 Theme global,不触进程级盘静态
    /// (与并发测试无竞争);Light 下组件面吃浅盘值
    #[gpui_kit::test]
    fn apply_tokens_paints_component_theme(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::component::init(cx);
            Theme::change(ThemeMode::Light, None, cx);
            // Theme::change 用库默认亮盘整铺;apply_tokens 后关键 token
            // 必须等于浅盘值(danger/scrollbar_thumb 与库默认不同,相等
            // 即证明铺设发生)
            apply_tokens(ThemeMode::Light, cx);
            let t = Theme::global(cx);
            assert!(!t.is_dark());
            assert_eq!(
                t.colors.background,
                Hsla::from(palette_of(ThemeMode::Light).base)
            );
            assert_eq!(
                t.colors.scrollbar_thumb,
                Hsla::from(palette_of(ThemeMode::Light).dock)
            );
            // 填充面前景:浅盘下仍为纯白(非近黑 label)
            assert_eq!(t.colors.primary_foreground, Hsla::from(color(0xFFFFFF, 1.0)));
        });
    }
}
