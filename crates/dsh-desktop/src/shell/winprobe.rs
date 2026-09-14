//! 窗口毛玻璃取证探针(macOS,`DSH_WINPROBE=1` 触发):开窗后解剖
//! 原生视图层级,打印 NSVisualEffectView 的存在性/材质/混合模式与
//! 图层类名、隐藏状态、NSWindow 透明位——用于验证窗口毛玻璃是否
//! 真实渲染。零新特性依赖,全 `msg_send` 不挑 objc2-app-kit 版本。

use objc2::class;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSArray, NSRect, NSString};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// 打印窗口原生视图层级与毛玻璃相关状态
pub fn dump(window: &gpui_kit::Window) {
    // Window 有同名固有方法(返回 AnyWindowHandle),须全限定取 trait 版
    let Ok(handle) = <gpui_kit::Window as HasWindowHandle>::window_handle(window) else {
        eprintln!("[winprobe] 无 window handle");
        return;
    };
    let RawWindowHandle::AppKit(app) = handle.as_raw() else {
        eprintln!("[winprobe] 非 AppKit 平台");
        return;
    };
    unsafe {
        let view: &AnyObject = app.ns_view.cast::<AnyObject>().as_ref();
        // BlurredView 经 addSubview positioned:NSWindowBelow 插在
        // contentView 的同级——从 superview 起遍历才能覆盖整棵窗口树
        let sup: Option<&AnyObject> = msg_send![view, superview];
        match sup {
            Some(sup) => walk(sup, 0),
            None => walk(view, 0),
        }
        let win: Option<&AnyObject> = msg_send![view, window];
        if let Some(win) = win {
            let opaque: bool = msg_send![win, isOpaque];
            let bg: Option<&AnyObject> = msg_send![win, backgroundColor];
            let appearance: Option<&AnyObject> = msg_send![win, effectiveAppearance];
            let appearance_name: Option<Retained<NSString>> =
                appearance.and_then(|a| msg_send![a, name]);
            eprintln!(
                "[winprobe] NSWindow isOpaque={opaque} backgroundColor={} appearance={}",
                desc(bg),
                appearance_name
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "nil".into()),
            );
        }
    }
}

/// 递归打印视图节点:类名/隐藏/框架,毛玻璃视图附材质与图层详情
unsafe fn walk(node: &AnyObject, depth: usize) {
    let pad = "  ".repeat(depth);
    let name = desc(Some(class_of(node)));
    let hidden: bool = msg_send![node, isHidden];
    let frame: NSRect = msg_send![node, frame];
    let mut extra = String::new();
    let is_effect: bool = msg_send![node, isKindOfClass: class!(NSVisualEffectView)];
    if is_effect {
        let material: u64 = msg_send![node, material];
        let state: u64 = msg_send![node, state];
        let blending: u64 = msg_send![node, blendingMode];
        extra = format!(" material={material} state={state} blending={blending}");
    }
    let layer: Option<&AnyObject> = msg_send![node, layer];
    let layer_info = match layer {
        None => "layer=nil".to_string(),
        Some(layer) => {
            let lhidden: bool = msg_send![layer, isHidden];
            // 子层类名:NSVisualEffectView 的模糊机制挂载后其层内会
            // 出现效果子层(类名可判载体真伪);空列表说明仍是空载体
            let sublayers: Option<Retained<NSArray<AnyObject>>> = msg_send![layer, sublayers];
            let sub_names = sublayers
                .map(|s| {
                    (0..s.len())
                        .map(|i| desc(Some(class_of(&s.objectAtIndex(i)))))
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .unwrap_or_default();
            // 不查 layer.backgroundColor:CALayer 该属性返回 CGColorRef
            // (非 ObjC 对象),msg_send 返回类型断言必炸(GPUI 视图
            // 即 CAMetalLayer,实测踩坑)
            format!(
                " layer={} layerHidden={lhidden} layerSublayers=[{sub_names}]",
                desc(Some(class_of(layer)))
            )
        }
    };
    eprintln!(
        "[winprobe] {pad}{name} hidden={hidden} frame=({:.0},{:.0} {:.0}x{:.0}){extra} {layer_info}",
        frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
    );
    let subs: Option<Retained<NSArray<AnyObject>>> = msg_send![node, subviews];
    if let Some(subs) = subs {
        for i in 0..subs.len() {
            unsafe { walk(&subs.objectAtIndex(i), depth + 1) };
        }
    }
}

/// 对象的 ObjC 类(类对象的 description 即类名)
fn class_of(obj: &AnyObject) -> &AnyObject {
    unsafe { msg_send![obj, class] }
}

/// description 字符串(nil 安全)
fn desc(obj: Option<&AnyObject>) -> String {
    let Some(obj) = obj else {
        return "nil".to_string();
    };
    unsafe {
        let s: Option<Retained<NSString>> = msg_send![obj, description];
        s.map(|s| s.to_string()).unwrap_or_else(|| "?".into())
    }
}
