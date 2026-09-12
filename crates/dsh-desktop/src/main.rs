//! dsh 桌面客户端(GPUI 原生 UI)。
//!
//! 单进程装配:进程内构造 [`dsh_core::registry::AppHost`](经
//! [`host::HostBridge`],不起 loopback HTTP);UI 布局
//! (280px 侧栏 + 统一列宽居中的消息列/输入卡,策略见 shell::metrics;
//! 桌面为唯一 UI)。关窗即退出。
//!
//! 用法:`dsh-desktop [--workspace <dir>] [--fake]`
//! - workspace 默认当前目录(api key 从环境变量解析);
//! - fake 用假 provider 驱动(自检/演示)。

// Windows release 下不弹控制台
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
mod app_icon;
mod features;
mod gitinfo;
mod kits;
mod shell;

use std::path::PathBuf;

use futures::StreamExt as _;
use gpui_kit::{AppContext, WindowBounds, WindowOptions, px, size};

use crate::shell::host::HostBridge;
use crate::shell::store::AppStore;

/// 命令行(dsh-desktop 自有极简面,不引入 clap)
struct Args {
    workspace: Option<PathBuf>,
    fake: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        workspace: None,
        fake: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--workspace" => args.workspace = it.next().map(PathBuf::from),
            "--fake" => args.fake = true,
            _ => {}
        }
    }
    args
}

fn main() {
    let args = parse_args();
    let workspace = args
        .workspace
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    // 真实模式凭据:环境变量有则注入(最高优先);缺席不拒启——
    // 凭据链还可在运行时从设置(provider api_key)解析,首运行
    // onboarding 也依赖「无 key 可进应用」
    let api_key = if args.fake {
        String::new()
    } else {
        dsh_core::Resolved::resolve_api_key(None).unwrap_or_default()
    };
    let (bridge, frames_rx) = match HostBridge::new(workspace, args.fake, &api_key) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("宿主装配失败:{e}");
            std::process::exit(1);
        }
    };
    let probe_host = bridge.host().clone();

    // 官方入口形态(gpui_kit::application 内部即
    // Application::with_platform(current_platform(false)),非 wasm 分支逐字等价)
    gpui_kit::application()
        // 自有图标(_dsh)优先,回落 gpui-component 内置(见 icons.rs)
        .with_assets(crate::kits::icons::MergedAssets)
        .run(move |cx| {
            // Dock 图标(favicon 同款鲸鱼,深色圆角方底同主题
            // BASE;NSImage 的 app-icon 位不支持 SVG,运行时绘制,见
            // app_icon.rs)。必须在此设置:gpui 在 Application 构造
            // 时已把 NSApplication 单例创建为 GPUIApplication 子类
            // (带 platform ivar);若提前调用 sharedApplication,单例
            // 会是基类 NSApplication 实例,gpui 读写 platform ivar
            // 时崩「Ivar platform not found」(release 实测)
            #[cfg(target_os = "macos")]
            app_icon::set_app_icon_1024();

            // 必须先于任何 gpui-component 组件使用(gpui_kit::init 按
            // feature 转发 component::init,官方入口形态)
            gpui_kit::init(cx);
            // 外观三档:开窗前读持久化档位(HostBridge 在 run
            // 前已装配,启动无闪色);theme::apply 同步双盘 + 组件 token
            crate::kits::theme::apply(
                crate::kits::theme::Appearance::parse(&bridge.host().appearance()),
                None,
                cx,
            );
            // 应用全局键表(⇧⌘P 等;测试装配同源,见 shell::bind_global_keys)
            crate::shell::bind_global_keys(cx);
            // 关窗即退出(单窗口应用;订阅泄漏存续于进程生命周期)
            std::mem::forget(cx.on_window_closed(|cx, _| cx.quit()));

            // DSH_PROBE 探针:3s 后经 bridge runtime 建会话并发一条消息
            // (与 UI store.send 同一路径;免 GUI 输入测生产 runloop 节奏)
            if let Some(text) = std::env::var_os("DSH_PROBE")
                && let Some(text) = text.to_str()
            {
                let host = probe_host.clone();
                let text = text.to_string();
                bridge.spawn_on_host(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let id = host.create_session(None, None, None);
                    eprintln!("[probe] session {id} prompt");
                    let r = host
                        .prompt(
                            &id,
                            &[serde_json::json!({ "type": "text", "text": text })],
                            "queue",
                        )
                        .await;
                    eprintln!("[probe] prompt result: {}", r.is_ok());
                });
            }

            let store = cx.new(|cx| AppStore::new(bridge, cx));

            // 帧泵:mux/host 双流 → reducer
            let pump_store = store.clone();
            cx.spawn(async move |cx| {
                let probe = std::env::var_os("DSH_PROBE").is_some();
                let t0 = std::time::Instant::now();
                let mut rx = frames_rx;
                while let Some(frame) = rx.next().await {
                    if probe {
                        let ty = frame.payload["event"]["type"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        eprintln!(
                            "[t2] +{}ms {} {}",
                            t0.elapsed().as_millis(),
                            frame.method,
                            ty
                        );
                    }
                    pump_store.update(cx, |s, cx| s.apply_frame(frame, cx));
                }
                Ok::<(), anyhow::Error>(())
            })
            .detach();

            // 自绘标题栏(隐系统标题,交通灯悬浮;见 ui 根布局 TitleBar)。
            // 必须以 TitleBar::window_options() 打底:app_owns_titlebar_drag
            // 压掉系统原生「双击标题栏缩放」——组件在 macOS 分支已自挂
            // on_double_click,两通道并存 = 双击进全屏立刻弹回(各执行一次;
            // 源码注释明言),且原生区还会吃掉标题栏单击做双击判定
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1440.), px(900.)), cx)),
                window_min_size: Some(size(px(960.), px(640.))),
                ..gpui_kit::component::TitleBar::window_options()
            };
            let view_store = store.clone();
            cx.spawn(async move |cx| {
                cx.open_window(options, move |window, cx| {
                    view_store.update(cx, |s, cx| s.attach_window_state(window, cx));
                    let view =
                        cx.new(|cx| crate::shell::WorkspaceView::new(view_store.clone(), cx));
                    // 窗口第一层 view 必须是 Root
                    cx.new(|cx| gpui_kit::component::Root::new(view, window, cx))
                })?;
                Ok::<_, anyhow::Error>(())
            })
            .detach();
        });
}
