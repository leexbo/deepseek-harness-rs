//! 应用侧窗口毛玻璃装配(macOS only)。
//!
//! macOS 26(Tahoe)起第三方 `NSVisualEffectView` 的 behindWindow
//! 模糊大面积失效(tauri wry#1636、cmux#2459 等生态实证;本机探针
//! 亦证三种结构挂载均渲染空转)。按 window-vibrancy 的
//! `apply_liquid_glass` 标准做法走 Liquid Glass(NSGlassEffectView):
//! 把 GPUIView 从窗口树摘下装进玻璃视图的内容槽,玻璃视图顶替其
//! 原位——玻璃即画布背板。macOS 26 以下回退 NSVisualEffectView 同级
//! 方案。纯 `msg_send` + AppKit 枚举契约值,零新特性依赖,不 patch 库。

use objc2::class;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSArray, NSRect};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// NSGlassEffectViewStyle.regular(Liquid Glass 默认形态)
const GLASS_STYLE_REGULAR: i64 = 0;
/// NSViewAutoresizingMask:宽 + 高随父级缩放(2 | 16)
const AUTORESIZING_WH: u64 = 18;
/// NSWindowOrderingMode.below
const ORDER_BELOW: i64 = -1;
/// NSVisualEffectMaterial.underPageBackground(macOS 26 前回退材质)
const MATERIAL_UNDER_PAGE: u64 = 14;
/// NSVisualEffectBlendingMode.behindWindow
const BLEND_BEHIND_WINDOW: u64 = 0;
/// NSVisualEffectState.active
const STATE_ACTIVE: u64 = 1;

/// 在窗口中装配毛玻璃(幂等;非 macOS 无操作)
pub fn install(window: &gpui_kit::Window) {
    let Ok(handle) = <gpui_kit::Window as HasWindowHandle>::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(app) = handle.as_raw() else {
        return;
    };
    unsafe {
        // handle 的 ns_view 即窗口 contentView(GPUIView),其 superview
        // 是窗口根容器
        let content: &AnyObject = app.ns_view.cast::<AnyObject>().as_ref();
        let root: Option<&AnyObject> = msg_send![content, superview];
        let Some(root) = root else { return };

        // macOS 26+:Liquid Glass——玻璃视图顶替画布原位并收编画布
        if let Some(glass_cls) = AnyClass::get(c"NSGlassEffectView") {
            install_glass(root, content, glass_cls);
            return;
        }
        // 旧系统回退:NSVisualEffectView 同级下插(behindWindow)
        install_effect_view(root, content);
    }
}

/// Liquid Glass 路线(macOS 26+):画布装进玻璃内容槽,玻璃顶替原位
///
/// # Safety
/// msg_send 调用链,`root`/`content` 须为主线程上合法的窗口视图
unsafe fn install_glass(root: &AnyObject, content: &AnyObject, glass_cls: &'static AnyClass) {
    // 幂等:画布已被收编(父级不再是窗口根)即跳过
    let parent: Option<&AnyObject> = msg_send![content, superview];
    if !parent.is_none_or(|p| std::ptr::eq(p, root)) {
        return;
    }
    let frame: NSRect = msg_send![content, frame];
    let glass: *mut AnyObject = msg_send![glass_cls, alloc];
    let glass: *mut AnyObject = msg_send![glass, initWithFrame: frame];
    if glass.is_null() {
        return;
    }
    let _: () = msg_send![glass, setStyle: GLASS_STYLE_REGULAR];
    let _: () = msg_send![glass, setCornerRadius: 0f64];
    let _: () = msg_send![glass, setAutoresizingMask: AUTORESIZING_WH];
    // 画布装进玻璃的内容槽(无默认槽则玻璃视图自身充当容器)
    let glass_content: Option<&AnyObject> = msg_send![glass, contentView];
    let target: &AnyObject = glass_content.unwrap_or(unsafe { &*glass });
    let _: () = msg_send![content, removeFromSuperview];
    let _: () = msg_send![target, addSubview: content];
    let _: () = msg_send![content, setFrame: frame];
    let _: () = msg_send![content, setAutoresizingMask: AUTORESIZING_WH];
    let _: () = msg_send![root, addSubview: glass, positioned: ORDER_BELOW, relativeTo: std::ptr::null_mut::<AnyObject>()];
}

/// 旧 macOS 回退:NSVisualEffectView 插画布同级之下(behindWindow)
///
/// # Safety
/// msg_send 调用链,`root`/`content` 须为主线程上合法的窗口视图
unsafe fn install_effect_view(root: &AnyObject, content: &AnyObject) {
    let subs: Option<Retained<NSArray<AnyObject>>> = msg_send![root, subviews];
    if let Some(subs) = subs {
        for i in 0..subs.len() {
            let sub: &AnyObject = &subs.objectAtIndex(i);
            let is_effect: bool = msg_send![sub, isKindOfClass: class!(NSVisualEffectView)];
            if is_effect {
                return;
            }
        }
    }
    let frame: NSRect = msg_send![content, bounds];
    let effect: *mut AnyObject = msg_send![class!(NSVisualEffectView), alloc];
    let effect: *mut AnyObject = msg_send![effect, initWithFrame: frame];
    if effect.is_null() {
        return;
    }
    let _: () = msg_send![effect, setMaterial: MATERIAL_UNDER_PAGE];
    let _: () = msg_send![effect, setBlendingMode: BLEND_BEHIND_WINDOW];
    let _: () = msg_send![effect, setState: STATE_ACTIVE];
    // gpui 的教训:不设 wantsLayer 本机 AppKit 不建层,材质空转
    let _: () = msg_send![effect, setWantsLayer: true];
    let _: () = msg_send![root, addSubview: effect, positioned: ORDER_BELOW, relativeTo: std::ptr::null_mut::<AnyObject>()];
}
