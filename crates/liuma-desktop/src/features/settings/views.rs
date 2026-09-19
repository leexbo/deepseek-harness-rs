//! 设置页(独立页载体):
//! 两栏壳 = 188px 左导航 + 内容列(header +
//! 滚动 options 面板)。
//! Models 区:标题+intro、provider 行卡
//! (名称 + 凭据圆点 + 编辑/移除文字胶囊钮)、**编辑卡在行卡内展开**
//! (主字段 API key,自定义字段折叠)、首运行 setup 姿态(未配置的
//! 默认 provider 直接渲染为打开的设置卡)、底部 dashed 添加钮、
//! 保存通告行、删除先经确认模态。数据源 = host `settings_view`
//! 快照(不含明文)。
//! 字号纪律:16 区标题 / 13 行主文 /
//! 12 动作钮与说明 / 11 注脚。

use gpui_kit::component::IconName;
use gpui_kit::component::InteractiveElementExt as _;
use gpui_kit::component::Sizable;
use gpui_kit::component::StyledExt;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, px,
};

use crate::features::settings::SettingsNav;
use crate::features::settings::store::{McpDetailMode, grouped_tokens};
use crate::kits::icons::{LiumaIcon, fixed};
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 动态元素 id(SharedString 进 ElementId)
fn sid(prefix: &str, key: &str) -> gpui_kit::SharedString {
    gpui_kit::SharedString::from(format!("{prefix}-{key}"))
}

/// 设置页内容(右列整列:顶部窄拖拽条 + 滚动内容;导航在左侧栏的
/// 设置菜单里)
pub fn render(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    div()
        .id("settings-page")
        .v_flex()
        .size_full()
        // 画布透 Root 毛玻璃涂层(同聊天区,不再自铺 base 叠涂)
        .debug_selector(|| "settings-page".to_string())
        // 顶部拖拽条(交通灯在左列;右列拖拽由此接手,无可见 chrome;
        // 高度与主标题行/右栏面板头 40 同高对齐)
        .child(
            div()
                .id("settings-drag")
                .flex()
                .flex_shrink_0()
                .h(px(40.))
                .cursor(gpui_kit::CursorStyle::Arrow)
                .on_mouse_down(gpui_kit::MouseButton::Left, |_, window, _| {
                    window.start_window_move();
                })
                .on_double_click(|_, window, _| {
                    window.titlebar_double_click();
                }),
        )
        .child(
            div()
                .id("settings-options")
                .flex()
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                // 横向主轴居中(items_center 是纵轴,此前内容贴左缘)
                .justify_center()
                .px(px(16.))
                .py(px(16.))
                // 区容器宽度(max-width 720)
                .child(div().v_flex().w(px(720.)).child(
                    match store.read(cx).settings.settings_nav {
                        SettingsNav::Models => models_section(store, cx).into_any_element(),
                        SettingsNav::Mcp => mcp_section(store, cx).into_any_element(),
                        SettingsNav::Hooks => hooks_section(store, cx).into_any_element(),
                        SettingsNav::General => general_section(store, cx).into_any_element(),
                        SettingsNav::About => about_section(store, cx).into_any_element(),
                    },
                )),
        )
}

/// 区说明行(13/tertiary)
fn intro_line(text: impl Into<String>) -> impl IntoElement {
    div()
        .text_size(px(14.))
        .text_color(theme::LABEL_3())
        .child(text.into())
}

/// 区标题(16/500)
fn section_title(text: &str) -> impl IntoElement {
    div()
        .text_size(px(16.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .child(text.to_string())
}

/// MCP Servers 区:server 行卡(id/command/enabled 开关/移除)+ 添加卡。
/// 通用字段输入行(标签 + 输入实体)。`sel` = 输入包装的布局回归锚
fn field_input(
    label: &str,
    sel: &'static str,
    input: &Option<gpui_kit::Entity<gpui_kit::component::input::InputState>>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .w(px(64.))
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(theme::CAPTION())
                .child(label.to_string()),
        )
        .children(input.as_ref().map(|e| {
            div()
                .id(gpui_kit::SharedString::from(sel))
                .debug_selector(move || sel.to_string())
                .flex_1()
                .min_w(px(0.))
                .h(px(32.))
                .child(Input::new(e).small())
        }))
}

fn mcp_section(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let st = store.read(cx);
    let st_add = store.clone();
    let Some(detail) = st.settings.mcp_detail.clone() else {
        // ── 列表页:行卡(id / command / 启停 Switch / 编辑 / 卸载)+ 添加钮 ──
        let servers = st.settings.settings_snapshot["mcpServers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut col = div()
            .v_flex()
            .gap(px(12.))
            .child(section_title("MCP Servers"))
            .children(st.settings.settings_notice.as_ref().map(|(ok, msg)| {
                div()
                    .debug_selector(|| "mcp-settings-notice".to_string())
                    .text_size(px(12.))
                    .text_color(if *ok {
                        theme::SUCCESS()
                    } else {
                        theme::DANGER()
                    })
                    .child(format!("{} {msg}", if *ok { "✓" } else { "⚠" }))
            }))
            .child(intro_line(
                "外部 MCP server(stdio)的工具桥接进工具面;保存即连接,所有会话共享。",
            ));
        // 列表头:「已安装 N」+ 新建主钮
        let total = servers.len();
        col = col.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .mt(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child(format!("已安装 {total}")),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("mcp-add")
                        .debug_selector(|| "mcp-add".to_string())
                        .flex()
                        .h(px(28.))
                        .items_center()
                        .px(px(12.))
                        .rounded(px(8.))
                        .bg(theme::BRAND())
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(gpui_kit::white())
                        .hover(|s| s.opacity(0.9))
                        .on_click({
                            let st_open = st_add.clone();
                            move |_, window, cx| {
                                st_open.update(cx, |st, cx| st.open_mcp_add(window, cx));
                            }
                        })
                        .child("+ 新建"),
                ),
        );

        let mut rows = div().v_flex().gap(px(8.));
        if servers.is_empty() {
            rows = rows.child(caption_line("尚未配置 MCP server。"));
        }
        for (ix, s) in servers.into_iter().enumerate() {
            let id = s["id"].as_str().unwrap_or_default().to_string();
            let command = s["command"].as_str().unwrap_or_default().to_string();
            let args: Vec<String> = s["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let enabled = s["enabled"].as_bool().unwrap_or(false);
            let status = st
                .settings
                .mcp_status_by_id
                .get(&id)
                .cloned()
                .or_else(|| {
                    // 帧未达时的兜底:设置快照里的宿主已知状态
                    st.settings.settings_snapshot["mcpStatus"]["servers"]
                        .as_array()?
                        .iter()
                        .find(|s| s["id"].as_str() == Some(id.as_str()))
                        .map(|s| {
                            (
                                s["status"].as_str().unwrap_or_default().to_string(),
                                s["error"].as_str().unwrap_or_default().to_string(),
                            )
                        })
                })
                .unwrap_or_else(|| ("stopped".into(), String::new()));
            let (st_switch, st_edit, st_remove) = (store.clone(), store.clone(), store.clone());
            let (id_switch_click, id_edit_click, id_remove_click) =
                (id.clone(), id.clone(), id.clone());
            let (id_sw_dbg, id_sw_click) = (id_switch_click.clone(), id_switch_click.clone());
            let (id_ed_dbg, id_ed_click) = (id_edit_click.clone(), id_edit_click.clone());
            let (id_rm_dbg, id_rm_click) = (id_remove_click.clone(), id_remove_click.clone());
            let (dot_color, status_text) = match status.0.as_str() {
                "ready" => (theme::SUCCESS(), String::new()),
                "connecting" => (theme::LABEL_2(), "连接中".to_string()),
                "reconnecting" => (theme::LABEL_2(), "重连中".to_string()),
                "failed" => (theme::DANGER(), "失败".to_string()),
                _ => (theme::CAPTION(), String::new()),
            };
            // 摘要随传输形态:url 在场 = http(host),否则 stdio(命令+参数)
            let is_http = s["url"].as_str().is_some_and(|u| !u.is_empty());
            let summary = if is_http {
                let url = s["url"].as_str().unwrap_or_default();
                let host = url
                    .split("://")
                    .nth(1)
                    .unwrap_or(url)
                    .split('/')
                    .next()
                    .unwrap_or(url);
                format!("http · {host}")
            } else {
                let mut sm = format!("stdio · {command}");
                if !args.is_empty() {
                    sm.push(' ');
                    sm.push_str(&args.join(" "));
                }
                sm
            };
            rows = rows.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .rounded(px(10.))
                    .bg(theme::LAYER())
                    .px(px(12.))
                    .py(px(10.))
                    // 左列:名称行(状态点 + id)/ 摘要行
                    .child(
                        div()
                            .v_flex()
                            .gap(px(2.))
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(div().size(px(7.)).rounded_full().bg(dot_color))
                                    .child(
                                        div()
                                            .text_size(px(13.))
                                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                                            .text_color(theme::LABEL())
                                            .child(id_switch_click.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .min_w(px(0.))
                                            .text_size(px(11.))
                                            .text_color(theme::CAPTION())
                                            .child(summary),
                                    )
                                    .children((!status_text.is_empty()).then(|| {
                                        div()
                                            .text_size(px(11.))
                                            .text_color(status_color_of(&status))
                                            .child(status_text)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .id(("mcp-switch", ix))
                            .debug_selector(move || format!("mcp-switch-{id_sw_dbg}"))
                            .on_mouse_down(gpui_kit::MouseButton::Left, {
                                let st_switch = st_switch.clone();
                                let id_sw = id_sw_click.clone();
                                move |_, _, cx| {
                                    st_switch.update(cx, |st, cx| st.toggle_mcp_server(&id_sw, cx));
                                }
                            })
                            .child(toggle_switch(enabled)),
                    )
                    .child(
                        div()
                            .id(("mcp-edit", ix))
                            .debug_selector(move || format!("mcp-edit-{id_ed_dbg}"))
                            .flex()
                            .h(px(22.))
                            .items_center()
                            .px(px(8.))
                            .rounded(px(11.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(theme::LABEL_2())
                            .hover(|s| s.bg(theme::DOCK()))
                            .on_click(move |_, window, cx| {
                                st_edit.update(cx, |st, cx| {
                                    st.open_mcp_edit(&id_ed_click, window, cx)
                                });
                            })
                            .child("编辑"),
                    )
                    .child(
                        div()
                            .id(("mcp-remove", ix))
                            .debug_selector(move || format!("mcp-remove-{id_rm_dbg}"))
                            .flex()
                            .h(px(22.))
                            .items_center()
                            .px(px(8.))
                            .rounded(px(11.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(theme::DANGER())
                            .hover(|s| s.bg(theme::DOCK()))
                            .on_click(move |_, _, cx| {
                                st_remove
                                    .update(cx, |st, cx| st.remove_mcp_server(&id_rm_click, cx));
                            })
                            .child("卸载"),
                    ),
            );
        }
        col = col.child(rows);
        return col.into_any_element();
    };

    // ── 详情页(新增/编辑;页签只切换编辑形态,保存是同一个动作)──
    let (title, intro) = match &detail.editing {
        Some(id) => (
            format!("编辑 MCP Server · {id}"),
            "修改当前 MCP 配置,保存后返回列表。",
        ),
        None => (
            "新增 MCP Server".into(),
            "注册新的 MCP server,保存后返回列表。",
        ),
    };
    let json_open = detail.mode == McpDetailMode::Json;
    let (st_close, st_save, _st_env_toggle, _st_env_add, st_uninstall) = (
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
    );
    let st_form = store.clone();
    let st_json = store.clone();
    let st_cancel = st_close.clone();

    // 标题 + 说明 + 页签(右上,一体胶囊组)
    let mut head = div()
        .flex()
        .items_start()
        .gap(px(12.))
        .child(
            div()
                .v_flex()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(px(15.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(theme::LABEL())
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child(intro),
                ),
        )
        .child(div().flex_1())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(2.))
                .rounded(px(10.))
                .bg(theme::LAYER())
                .p(px(2.))
                .child(
                    div()
                        .id("mcp-tab-form")
                        .debug_selector(|| "mcp-tab-form".to_string())
                        .flex()
                        .h(px(22.))
                        .items_center()
                        .px(px(10.))
                        .rounded(px(8.))
                        .cursor_pointer()
                        .text_size(px(11.))
                        .text_color(if json_open {
                            theme::CAPTION().into()
                        } else {
                            gpui_kit::white()
                        })
                        .bg(if json_open {
                            theme::LAYER()
                        } else {
                            theme::BRAND()
                        })
                        .on_click(move |_, window, cx| {
                            st_form.update(cx, |st, cx| {
                                if st.settings.mcp_detail.as_ref().map(|d| d.mode)
                                    != Some(McpDetailMode::Form)
                                {
                                    st.switch_mcp_mode(McpDetailMode::Form, window, cx);
                                }
                            });
                        })
                        .child("表单"),
                )
                .child(
                    div()
                        .id("mcp-tab-json")
                        .debug_selector(|| "mcp-tab-json".to_string())
                        .flex()
                        .h(px(22.))
                        .items_center()
                        .px(px(10.))
                        .rounded(px(8.))
                        .cursor_pointer()
                        .text_size(px(11.))
                        .text_color(if json_open {
                            gpui_kit::white()
                        } else {
                            theme::CAPTION().into()
                        })
                        .bg(if json_open {
                            theme::BRAND()
                        } else {
                            theme::LAYER()
                        })
                        .on_click(move |_, window, cx| {
                            st_json.update(cx, |st, cx| {
                                if st.settings.mcp_detail.as_ref().map(|d| d.mode)
                                    != Some(McpDetailMode::Json)
                                {
                                    st.switch_mcp_mode(McpDetailMode::Json, window, cx);
                                }
                            });
                        })
                        .child("JSON"),
                ),
        );
    // ✕(返回列表)
    head = head.child(
        div()
            .id("mcp-detail-close")
            .size(px(24.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme::CAPTION())
            .hover(|s| s.bg(theme::DOCK()))
            .on_click(move |_, _, cx| {
                st_close.update(cx, |st, cx| st.close_mcp_detail(cx));
            })
            .child(fixed(IconName::Close, 12.)),
    );

    // 详情卡(中性边框;蓝 BRAND 只给主钮)
    let mut card = div()
        .id("mcp-add-card")
        .debug_selector(|| "mcp-add-card".to_string())
        .v_flex()
        .gap(px(14.))
        .rounded(px(10.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::LAYER())
        .p(px(14.))
        .child(head);

    // 页签内容
    if json_open {
        if let Some(input) = &detail.json_input {
            card = card.child(
                div()
                    .v_flex()
                    .gap(px(6.))
                    .child(caption_line("完整配置"))
                    .child(
                        div()
                            .id("mcp-json-input")
                            .debug_selector(|| "mcp-json-input".to_string())
                            .w_full()
                            .min_w(px(0.))
                            .child(gpui_kit::component::input::Editor::new(input).h(px(280.))),
                    ),
            );
        }
        if let Some(Err(e)) = &detail.json_preview {
            card = card.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::DANGER())
                    .child(format!("⚠ {e}")),
            );
        }
        if let Some(Ok(entries)) = &detail.json_preview {
            let mut list = div().v_flex().gap(px(4.));
            for e in entries {
                list = list.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::LABEL_2())
                        .child(format!("{} · {}", e.id, e.command)),
                );
            }
            card = card.child(list);
        }
    } else {
        // 通用字段行(标签 64px 左置 + 输入 flex_1)
        let id_field: gpui_kit::AnyElement = if let Some(id) = &detail.editing {
            div()
                .id("mcp-id-input")
                .debug_selector(|| "mcp-id-input".to_string())
                .flex_1()
                .min_w(px(0.))
                .h(px(32.))
                .flex()
                .items_center()
                .text_size(px(12.))
                .text_color(theme::CAPTION())
                .child(id.clone())
                .into_any_element()
        } else {
            div()
                .id("mcp-id-input")
                .debug_selector(|| "mcp-id-input".to_string())
                .flex_1()
                .min_w(px(0.))
                .h(px(32.))
                .children(
                    detail
                        .form_id
                        .as_ref()
                        .map(|e| div().w_full().h(px(32.)).child(Input::new(e).small())),
                )
                .into_any_element()
        };
        card = card.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .w(px(64.))
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child("名称"),
                )
                .child(id_field),
        );
        // 传输形态(stdio / HTTP):字段集随形态显隐
        let form_http = detail.form_http;
        let st_transport = store.clone();
        let transport_chip = |sel: &'static str,
                              label: &'static str,
                              active: bool,
                              want: bool,
                              st: Entity<AppStore>| {
            let base = div()
                .id(sel)
                .flex()
                .h(px(26.))
                .items_center()
                .px(px(10.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.));
            let base = if active {
                base.bg(theme::ONGOING().opacity(0.14))
                    .text_color(theme::BRAND())
            } else {
                base.text_color(theme::CAPTION())
                    .hover(|s| s.bg(theme::DOCK()).text_color(theme::LABEL_2()))
            };
            base.child(label)
                .on_click(move |_, _, cx| {
                    // 绝对值方向:点击目标形态与当前不同才切换(连发幂等)
                    let active_now = st
                        .read(cx)
                        .settings
                        .mcp_detail
                        .as_ref()
                        .map(|d| d.form_http)
                        .unwrap_or(false);
                    if active_now != want {
                        st.update(cx, |st, cx| st.toggle_mcp_transport(cx));
                    }
                })
                .into_any_element()
        };
        let transport_row = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(transport_chip(
                "mcp-transport-stdio",
                "stdio",
                !form_http,
                false,
                st_transport.clone(),
            ))
            .child(transport_chip(
                "mcp-transport-http",
                "HTTP",
                form_http,
                true,
                st_transport,
            ));
        card = card.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .w(px(64.))
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child("传输"),
                )
                .child(transport_row),
        );
        if detail.form_http {
            card = card.child(field_input("URL", "mcp-url-input", &detail.form_url));
            // 请求头(键值对;原样透传,如 Authorization)
            let mut header_rows = div().v_flex().gap(px(4.));
            for (ix, (k, v)) in detail.form_headers.iter().enumerate() {
                let st_rm = store.clone();
                header_rows = header_rows.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .w(px(120.))
                                .flex_shrink_0()
                                .h(px(32.))
                                .child(Input::new(k).small()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .h(px(32.))
                                .child(Input::new(v).small()),
                        )
                        .child(
                            div()
                                .id(("mcp-header-rm", ix))
                                .flex()
                                .h(px(22.))
                                .items_center()
                                .px(px(6.))
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_size(px(11.))
                                .text_color(theme::CAPTION())
                                .hover(|s| s.bg(theme::DOCK()).text_color(theme::DANGER()))
                                .on_click({
                                    let st_rm = st_rm.clone();
                                    move |_, _, cx| {
                                        st_rm.update(cx, |st, cx| st.remove_mcp_header(ix, cx));
                                    }
                                })
                                .child("移除"),
                        ),
                );
            }
            let st_header_add = store.clone();
            card = card.child(
                div()
                    .v_flex()
                    .gap(px(4.))
                    .child(caption_line("请求头(可选;键 + 值,原样透传)"))
                    .child(header_rows)
                    .child(
                        div()
                            .id("mcp-header-add")
                            .flex()
                            .h(px(24.))
                            .items_center()
                            .justify_center()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(theme::CAPTION())
                            .hover(|s| s.bg(theme::DOCK()).text_color(theme::LABEL_2()))
                            .on_click({
                                let st_header_add = st_header_add.clone();
                                move |_, window, cx| {
                                    st_header_add
                                        .update(cx, |st, cx| st.add_mcp_header(window, cx));
                                }
                            })
                            .child("+ 添加请求头"),
                    ),
            );
        } else {
            card = card.child(field_input(
                "command",
                "mcp-command-input",
                &detail.form_command,
            ));
            card = card.child(field_input("cwd", "mcp-cwd-input", &detail.form_cwd));
            // 参数(每参数一条;含空格的参数单行化会吞内容)
            let mut arg_rows = div().v_flex().gap(px(4.));
            for (ix, arg) in detail.form_args.iter().enumerate() {
                let st_rm = store.clone();
                arg_rows = arg_rows.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .h(px(32.))
                                .child(Input::new(arg).small()),
                        )
                        .child(
                            div()
                                .id(("mcp-arg-rm", ix))
                                .flex()
                                .h(px(22.))
                                .items_center()
                                .px(px(6.))
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_size(px(11.))
                                .text_color(theme::CAPTION())
                                .hover(|s| s.bg(theme::DOCK()).text_color(theme::DANGER()))
                                .on_click({
                                    let st_rm = st_rm.clone();
                                    move |_, _, cx| {
                                        st_rm.update(cx, |st, cx| st.remove_mcp_arg(ix, cx));
                                    }
                                })
                                .child("移除"),
                        ),
                );
            }
            let st_arg_add = store.clone();
            card = card.child(
                div()
                    .v_flex()
                    .gap(px(4.))
                    .child(caption_line("参数(每个一条)"))
                    .child(arg_rows)
                    .child(
                        div()
                            .id("mcp-arg-add")
                            .flex()
                            .h(px(24.))
                            .items_center()
                            .justify_center()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(theme::CAPTION())
                            .hover(|s| s.bg(theme::DOCK()).text_color(theme::LABEL_2()))
                            .on_click({
                                let st_arg_add = st_arg_add.clone();
                                move |_, window, cx| {
                                    st_arg_add.update(cx, |st, cx| st.add_mcp_arg(window, cx));
                                }
                            })
                            .child("+ 添加参数"),
                    ),
            );
            // 环境变量(键值对,平铺)
            let mut env_rows = div().v_flex().gap(px(4.));
            for (ix, (k, v)) in detail.form_env.iter().enumerate() {
                let st_rm = store.clone();
                env_rows = env_rows.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .w(px(120.))
                                .flex_shrink_0()
                                .h(px(32.))
                                .child(Input::new(k).small()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .h(px(32.))
                                .child(Input::new(v).small()),
                        )
                        .child(
                            div()
                                .id(("mcp-env-rm", ix))
                                .flex()
                                .h(px(22.))
                                .items_center()
                                .px(px(6.))
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_size(px(11.))
                                .text_color(theme::CAPTION())
                                .hover(|s| s.bg(theme::DOCK()).text_color(theme::DANGER()))
                                .on_click({
                                    let st_rm = st_rm.clone();
                                    move |_, _, cx| {
                                        st_rm.update(cx, |st, cx| st.remove_mcp_env(ix, cx));
                                    }
                                })
                                .child("移除"),
                        ),
                );
            }
            let st_env_add = store.clone();
            card = card.child(
                div()
                    .v_flex()
                    .gap(px(4.))
                    .child(caption_line("环境变量(可选;键 + 值)"))
                    .child(env_rows)
                    .child(
                        div()
                            .id("mcp-env-add")
                            .flex()
                            .h(px(24.))
                            .items_center()
                            .justify_center()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(theme::CAPTION())
                            .hover(|s| s.bg(theme::DOCK()).text_color(theme::LABEL_2()))
                            .on_click({
                                let st_env_add = st_env_add.clone();
                                move |_, window, cx| {
                                    st_env_add.update(cx, |st, cx| st.add_mcp_env(window, cx));
                                }
                            })
                            .child("+ 添加环境变量"),
                    ),
            );
        }
        card = card.child(field_input(
            "超时 MS",
            "mcp-timeout-input",
            &detail.form_timeout,
        ));
        // 启用开关(启停由表单随保存落盘)
        let st_enable = store.clone();
        card = card.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(if detail.form_enabled {
                            theme::LABEL_2()
                        } else {
                            theme::CAPTION()
                        })
                        .child(if detail.form_enabled {
                            "已启用"
                        } else {
                            "未启用"
                        }),
                )
                .child(
                    div()
                        .id("mcp-enabled")
                        .on_mouse_down(gpui_kit::MouseButton::Left, {
                            let st_enable = st_enable.clone();
                            move |_, _, cx| {
                                st_enable.update(cx, |st, cx| {
                                    st.toggle_mcp_form_enabled(cx);
                                });
                            }
                        })
                        .child(toggle_switch(detail.form_enabled)),
                ),
        );
    }
    // 底部按钮组:左 = 卸载(编辑态);右 = 保存(统一动作)+ 取消
    let mut footer = div().flex().items_center().gap(px(8.));
    if detail.editing.is_some() {
        footer = footer.child(
            div()
                .id("mcp-uninstall")
                .flex()
                .h(px(28.))
                .items_center()
                .gap(px(4.))
                .px(px(8.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(theme::DANGER())
                .hover(|s| s.bg(theme::DOCK()))
                .on_click(move |_, _, cx| {
                    st_uninstall.update(cx, |st, cx| st.uninstall_mcp_detail(cx));
                })
                .child("卸载"),
        );
    }
    footer = footer.child(div().flex_1()).child(
        div()
            .id("mcp-submit")
            .debug_selector(|| "mcp-submit".to_string())
            .flex()
            .h(px(28.))
            .items_center()
            .justify_center()
            .rounded(px(8.))
            .bg(theme::BRAND())
            .px(px(18.))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(gpui_kit::white())
            .hover(|s| s.opacity(0.9))
            .on_click(move |_, _, cx| {
                st_save.update(cx, |st, cx| st.save_mcp_detail(cx));
            })
            .child("保存"),
    );
    footer = footer.child(
        div()
            .id("mcp-cancel")
            .flex()
            .h(px(28.))
            .items_center()
            .px(px(10.))
            .rounded(px(8.))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(theme::LABEL_2())
            .hover(|s| s.bg(theme::DOCK()))
            .on_click(move |_, _, cx| {
                st_cancel.update(cx, |st, cx| st.close_mcp_detail(cx));
            })
            .child("取消"),
    );
    card = card.child(footer);
    card.into_any_element()
}

/// MCP 连接状态着色(ready 绿 / connecting 中性 / failed 红 / 其余灰)
fn status_color_of(status: &(String, String)) -> gpui_kit::Rgba {
    match status.0.as_str() {
        "ready" => theme::SUCCESS(),
        "connecting" => theme::LABEL_2(),
        "failed" => theme::DANGER(),
        _ => theme::CAPTION(),
    }
}

/// 表单行(标签 + 输入实体;InputState 必须挂树才能聚焦输入)
/// Models 区:标题+intro+通告 / 行卡列表(编辑内嵌 / setup 姿态)/ 添加块
fn models_section(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let st = store.read(cx);
    let mut col = div()
        .v_flex()
        .gap(px(12.))
        .child(section_title("模型与 Provider"))
        .children(
            st.settings
                .saved_provider_notice
                .as_ref()
                .map(|name| saved_notice(name)),
        )
        // 设置动作页内通告(单槽覆盖;不走聊天区)
        .children(st.settings.settings_notice.as_ref().map(|(ok, msg)| {
            div()
                .id("settings-notice")
                .debug_selector(|| "settings-notice".to_string())
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(if *ok {
                    theme::SUCCESS()
                } else {
                    theme::DANGER()
                })
                .child(format!("{} {msg}", if *ok { "✓" } else { "⚠" }))
        }));
    let providers: Vec<serde_json::Value> = st.settings.settings_snapshot["providers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    // 行卡列表(与标题块之间 extra 12 空气,gap 8)
    let mut rows = div().v_flex().gap(px(8.)).mt(px(12.));
    if providers.is_empty() {
        rows = rows.child(caption_line(
            "注册表为空——「添加 Provider」,或重启后恢复内置默认。",
        ));
    }
    for p in &providers {
        let id = p["id"].as_str().unwrap_or_default();
        // 首运行 setup 姿态:未配置的默认 provider 直接是打开的设置卡
        if st.provider_setup_posture(id) {
            rows = rows.child(setup_card(store, cx, id));
            continue;
        }
        rows = rows.child(provider_row_card(store, cx, p));
    }
    col = col.child(rows).child(add_block(store, cx));
    col
}

/// 保存通告行(12/success)
fn saved_notice(name: &str) -> impl IntoElement {
    div()
        .id("provider-saved-notice")
        .text_size(px(12.))
        .text_color(theme::SUCCESS())
        .child(format!("已保存 {name}"))
}

/// 单个 provider 卡片:圆标 avatar + 名称 + URL 链接行;
/// 右侧 = 上次刷新「N 小时前」+ 刷新钮 + 计费行;当前 defaultProvider
/// = BRAND 蓝描边。点卡片展开编辑器
fn provider_row_card(
    store: &Entity<AppStore>,
    cx: &App,
    p: &serde_json::Value,
) -> impl IntoElement {
    let st = store.read(cx);
    let id = p["id"].as_str().unwrap_or_default().to_string();
    let name = p["display_name"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or(id.as_str())
        .to_string();
    let base_url = p["base_url"].as_str().unwrap_or_default().to_string();
    let cred_ready = p["credentialReady"].as_bool().unwrap_or(false);
    let is_default = st.settings.settings_snapshot["defaultProvider"].as_str() == Some(id.as_str());
    let open = st.settings.editing_provider.as_deref() == Some(id.as_str());
    let refreshing = st.settings.billing_refreshing.as_deref() == Some(id.as_str());
    let cache = p["billing_cache"].clone();
    let has_billing = p["billing"].is_object();
    let row_sel = sid("provider-row", &id);
    let mut card = div()
        .id(row_sel.clone())
        .debug_selector(move || row_sel.to_string())
        .v_flex()
        .gap(px(10.))
        .rounded(px(12.))
        .border_1()
        // 恒中性边框:默认态由「默认」chip 表达,常驻蓝框 = 误读为选中态
        .border_color(theme::BORDER())
        .p(px(12.))
        .pr(px(14.))
        // 主行:avatar + 名称/URL 两行 + 右侧状态区 + 编辑/移除
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(avatar(&name))
                .child(
                    div()
                        .v_flex()
                        .min_w(px(0.))
                        .flex_1()
                        .gap(px(2.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(
                                    div()
                                        .text_size(px(14.))
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .text_color(theme::LABEL())
                                        .child(name.clone()),
                                )
                                .when(is_default, |el| {
                                    el.child(
                                        div()
                                            .flex()
                                            .h(px(16.))
                                            .items_center()
                                            .px(px(5.))
                                            .rounded(px(4.))
                                            .border_1()
                                            .border_color(theme::BORDER())
                                            .text_size(px(11.))
                                            .text_color(theme::LABEL_3())
                                            .child("默认"),
                                    )
                                })
                                .child(credential_dot(cred_ready)),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::ONGOING())
                                .truncate()
                                .child(base_url.clone()),
                        ),
                )
                // 右侧状态区:上次刷新 + 刷新钮 / 计费行(配置了计费端点才有)
                .when(has_billing, |el| {
                    el.child(
                        div()
                            .v_flex()
                            .items_end()
                            .gap(px(3.))
                            .child(billing_refresh_line(store, &id, &cache, refreshing))
                            .children(billing_value_line(&cache)),
                    )
                })
                .child(div().flex_shrink_0().child(row_edit_button(store, &id)))
                .child(div().flex_shrink_0().child(row_remove_button(store, &id))),
        );
    if open {
        card = card.child(provider_editor(store, cx, &id, false));
    }
    card
}

/// 圆标 avatar(显示名首字符;LAYER 底 + 三级文字)
fn avatar(name: &str) -> impl IntoElement {
    let ch = name
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".into());
    div()
        .flex()
        .size(px(30.))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .bg(theme::LAYER())
        .border_1()
        .border_color(theme::BORDER())
        .text_size(px(13.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(theme::LABEL_2())
        .child(ch)
}

/// 计费刷新行:「N 小时前」+ 刷新钮(拖拽刷新中禁用)
fn billing_refresh_line(
    store: &Entity<AppStore>,
    id: &str,
    cache: &serde_json::Value,
    refreshing: bool,
) -> impl IntoElement {
    let s = store.clone();
    let pid = id.to_string();
    let fetched_at = cache["fetched_at_ms"].as_u64();
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .text_size(px(11.))
        .text_color(theme::CAPTION())
        .children(fetched_at.map(|ms| {
            div()
                .flex()
                .items_center()
                .gap(px(2.))
                .child(fixed(LiumaIcon::Clock, 11.))
                .child(relative_time(ms))
        }))
        .child(
            div()
                .id(sid("billing-refresh", id))
                .flex_shrink_0()
                .cursor_pointer()
                .text_color(theme::LABEL_3())
                .hover(|s| s.text_color(theme::LABEL()))
                .child(fixed(IconName::LoaderCircle, 12.))
                .when(refreshing, |el| el.text_color(theme::ONGOING()))
                .on_click(move |_, _, cx| {
                    let pid = pid.clone();
                    s.update(cx, |st, cx| {
                        if !refreshing {
                            st.refresh_billing_now(&pid, false, cx);
                        }
                    });
                }),
        )
}

/// 计费数值行:余额「剩余: 9.52 CNY」/ 用量「5小时: 6% 7天: 6% 4d22h」
fn billing_value_line(cache: &serde_json::Value) -> Option<impl IntoElement> {
    let row = div().flex().items_center().gap(px(6.)).text_size(px(12.));
    match cache["kind"].as_str() {
        Some("balance") => {
            let amount = cache["amount"].as_str()?;
            let currency = cache["currency"].as_str().unwrap_or("");
            Some(
                row.child("剩余:")
                    .child(
                        div()
                            .text_color(theme::SUCCESS())
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .child(amount.to_string()),
                    )
                    .child(
                        div()
                            .text_color(theme::LABEL_3())
                            .child(currency.to_string()),
                    )
                    .into_any_element(),
            )
        }
        Some("usage") => {
            // 每窗一行:标签 + 进度条 + 百分比;尾部重置倒计时
            let resets = cache["resets"].as_str().and_then(resets_countdown);
            let mut col = div().v_flex().items_end().gap(px(4.));
            let mut any = false;
            for (label, pct) in [
                ("5 小时", cache["pct_5h"].as_u64()),
                ("7 天", cache["pct_7d"].as_u64()),
            ] {
                let Some(v) = pct else { continue };
                any = true;
                col = col.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme::CAPTION())
                                .child(label),
                        )
                        .child(usage_bar(v, 56.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .text_color(theme::LABEL())
                                .child(format!("{v}%")),
                        ),
                );
            }
            if let Some(cd) = resets {
                any = true;
                col = col.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(2.))
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child(fixed(LiumaIcon::Clock, 11.))
                        .child(format!("{cd}后重置")),
                );
            }
            any.then(|| col.into_any_element())
        }
        _ => None,
    }
}

/// 用量迷你进度条(width px、4px 高;填充 <70% 正常绿,≥70% 接近限额红)
pub(crate) fn usage_bar(pct: u64, width: f32) -> gpui_kit::AnyElement {
    let pct = pct.min(100);
    div()
        .w(px(width))
        .h(px(4.))
        .rounded(px(2.))
        .bg(theme::BORDER_2())
        .overflow_hidden()
        .child(
            div()
                .w(px(width * pct as f32 / 100.))
                .h_full()
                .rounded(px(2.))
                .bg(if pct >= 70 {
                    theme::DANGER()
                } else {
                    theme::SUCCESS()
                }),
        )
        .into_any_element()
}

/// 重置倒计时:毫秒戳 → 剩余「4天22时 / 3时12分 / 45分」;缺席/非时间戳/已过 = None
pub(crate) fn resets_countdown(resets: &str) -> Option<String> {
    let ts = resets.parse::<u64>().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    let mins = ts.saturating_sub(now) / 60_000;
    if mins >= 60 * 24 {
        Some(format!(
            "{}天{}时",
            mins / (60 * 24),
            (mins % (60 * 24)) / 60
        ))
    } else if mins >= 60 {
        Some(format!("{}时{}分", mins / 60, mins % 60))
    } else if mins > 0 {
        Some(format!("{mins}分"))
    } else {
        None
    }
}

/// 毫秒时间戳 → 「N 分钟前 / N 小时前 / N 天前」
fn relative_time(ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mins = now.saturating_sub(ms) / 60_000;
    if mins < 60 {
        format!("{mins} 分钟前")
    } else if mins < 60 * 24 {
        format!("{} 小时前", mins / 60)
    } else {
        format!("{} 天前", mins / (60 * 24))
    }
}

/// 凭据状态圆点(8px 实心,success/error)
fn credential_dot(configured: bool) -> impl IntoElement {
    div()
        .flex()
        .size(px(8.))
        .flex_shrink_0()
        .rounded_full()
        .bg(if configured {
            theme::SUCCESS()
        } else {
            theme::DANGER()
        })
}

/// 行头「编辑」钮(28h r14 边框胶囊;再点收起)
fn row_edit_button(store: &Entity<AppStore>, id: &str) -> impl IntoElement {
    let s = store.clone();
    let pid = id.to_string();
    let sel = sid("provider-edit", id);
    div()
        .id(sel.clone())
        .debug_selector(move || sel.to_string())
        .flex()
        .flex_shrink_0()
        .h(px(28.))
        .items_center()
        .px(px(10.))
        .rounded(px(14.))
        .border_1()
        .border_color(theme::BORDER())
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(theme::LABEL_2())
        .hover(|s| s.bg(theme::DOCK()))
        .child("编辑")
        .on_click(move |_, window, cx| {
            let pid = pid.clone();
            s.update(cx, |st, cx| {
                if st.settings.editing_provider.as_deref() == Some(&pid) {
                    st.close_provider_editor(&pid, cx);
                } else {
                    st.open_provider_editor(&pid, window, cx);
                }
            });
        })
}

/// 行头「移除」钮(28h 胶囊,危险色文字)
fn row_remove_button(store: &Entity<AppStore>, id: &str) -> impl IntoElement {
    let s = store.clone();
    let pid = id.to_string();
    let sel = sid("provider-remove", id);
    div()
        .id(sel.clone())
        .debug_selector(move || sel.to_string())
        .flex()
        .flex_shrink_0()
        .h(px(28.))
        .items_center()
        .px(px(10.))
        .rounded(px(14.))
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(theme::DANGER())
        .hover(|s| s.bg(theme::DOCK()))
        .child("移除")
        .on_click(move |_, _, cx| {
            let pid = pid.clone();
            s.update(cx, |st, cx| st.ask_delete_provider(&pid, cx));
        })
}

/// 首运行 setup 卡(填充模块 = 该 provider 在页面上的
/// 存在形式,内嵌编辑卡且凭据必填)
fn setup_card(store: &Entity<AppStore>, cx: &App, id: &str) -> impl IntoElement {
    let sel = sid("provider-setup", id);
    div()
        .id(sel.clone())
        .debug_selector(move || sel.to_string())
        .v_flex()
        .rounded(px(12.))
        .bg(theme::SIDEBAR())
        .p(px(14.))
        .pr(px(16.))
        .child(provider_editor(store, cx, id, true))
}

/// 添加块(两入口):目录选择卡 / 自定义表单卡 /
/// 闭态 = 两个 dashed 添加钮(「添加提供方」= 目录流;「添加自定义
/// 提供方」= 自由表单)
fn add_block(store: &Entity<AppStore>, cx: &App) -> gpui_kit::AnyElement {
    let st = store.read(cx);
    if st.settings.adding_provider {
        return div()
            .id("provider-add-card")
            .debug_selector(|| "provider-add-card".to_string())
            .v_flex()
            .gap(px(14.))
            .rounded(px(12.))
            .bg(theme::SIDEBAR())
            .p(px(14.))
            .pr(px(16.))
            .child(
                div()
                    .v_flex()
                    .gap(px(6.))
                    .child(field_label("Provider ID(小写/连字符)"))
                    .children(
                        st.settings
                            .set_form_id
                            .as_ref()
                            .map(|e| div().h(px(32.)).child(Input::new(e).small())),
                    ),
            )
            .child(provider_editor(store, cx, "", false))
            .into_any_element();
    }
    let (s_catalog, s_custom) = (store.clone(), store.clone());
    div()
        .flex()
        .gap(px(10.))
        .child(
            div()
                .id("provider-add")
                .debug_selector(|| "provider-add".to_string())
                .flex_1()
                .h(px(44.))
                .flex()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .rounded(px(12.))
                .border_1()
                .border_color(theme::BORDER_2())
                .border_dashed()
                .cursor_pointer()
                .text_size(px(14.))
                .text_color(theme::LABEL_3())
                .hover(|s| s.bg(theme::LAYER()).text_color(theme::LABEL_2()))
                .child(fixed(IconName::Plus, 14.))
                .child("添加提供方")
                .on_click(move |_, window, cx| {
                    s_catalog.update(cx, |st, cx| st.open_provider_add_builtin(window, cx));
                }),
        )
        .child(
            div()
                .id("provider-add-custom")
                .debug_selector(|| "provider-add-custom".to_string())
                .flex_1()
                .h(px(44.))
                .flex()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .rounded(px(12.))
                .border_1()
                .border_color(theme::BORDER_2())
                .border_dashed()
                .cursor_pointer()
                .text_size(px(14.))
                .text_color(theme::LABEL_3())
                .hover(|s| s.bg(theme::LAYER()).text_color(theme::LABEL_2()))
                .child(fixed(IconName::Plus, 14.))
                .child("添加自定义提供方")
                .on_click(move |_, window, cx| {
                    s_custom.update(cx, |st, cx| st.open_provider_add(window, cx));
                }),
        )
        .into_any_element()
}

/// 编辑卡:内置模式(提供方下拉 + API 密钥 + 「自定义设置」折叠;
/// 适配器与目录绑定)/ 自定义模式(名称 / Base URL / API Key / API 格式
/// 三选 / 模型列表 / 计费端点,图3 字段序)。页脚右对齐 取消/应用。
/// setup/添加卡共用(无标题行)
fn provider_editor(store: &Entity<AppStore>, cx: &App, id: &str, setup: bool) -> impl IntoElement {
    let st = store.read(cx);
    let (s_cancel, s_apply, s_add_model, s_fetch, s_billing, s_kind) = (
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
    );
    let close_id = id.to_string();
    let fetch_pid = id.to_string();
    let billing_pid = id.to_string();
    let editor_sel = if id.is_empty() {
        "provider-editor".to_string()
    } else {
        format!("provider-editor-{id}")
    };
    let editor_id = if id.is_empty() {
        gpui_kit::SharedString::from("provider-editor")
    } else {
        sid("provider-editor", id)
    };
    let input_row =
        |label: &str, slot: &Option<Entity<InputState>>, id_fmt: String| -> gpui_kit::AnyElement {
            div()
                .v_flex()
                .gap(px(6.))
                .child(field_label(label))
                .children(slot.as_ref().map(|e| {
                    div()
                        .h(px(34.))
                        .debug_selector(move || id_fmt.clone())
                        .child(Input::new(e).small())
                }))
                .into_any_element()
        };
    let builtin = st.settings.builtin_mode;
    let picked_id = st.settings.builtin_picked.clone();
    let picked = liuma_core::settings::provider_catalog()
        .into_iter()
        .find(|e| e.id == picked_id);
    // 高级段(模型列表 + 计费端点):自定义模式平铺;内置模式收进
    // 「自定义设置」折叠
    let advanced = || -> Vec<gpui_kit::AnyElement> {
        vec![
            editor_models_block(store, cx, fetch_pid.clone()).into_any_element(),
            editor_billing_block(store, cx, id.to_string(), setup, billing_pid.clone())
                .into_any_element(),
        ]
    };
    let mut card = div()
        .id(editor_id)
        .debug_selector(move || editor_sel.clone())
        .v_flex()
        .gap(px(14.))
        .rounded(px(12.))
        .when(!setup && !id.is_empty(), |el| {
            el.bg(theme::SIDEBAR()).p(px(14.)).pr(px(16.))
        })
        .when(!setup && !id.is_empty(), |el| {
            el.child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .child(id.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::CAPTION())
                            .child("provider"),
                    ),
            )
        })
        // 内置模式:提供方下拉(编辑态锁定为该厂商;切换仅在添加态)
        .when(builtin, |el| {
            el.child(
                div()
                    .v_flex()
                    .gap(px(6.))
                    .child(field_label("提供方"))
                    .children(st.settings.builtin_select.as_ref().map(|s| {
                        div()
                            .w(px(260.))
                            .h(px(36.))
                            .line_height(gpui_kit::relative(1.4))
                            .child(Select::new(s))
                    })),
            )
        })
        .when(!builtin, |el| {
            el.child(input_row(
                "名称",
                &st.settings.set_form_name,
                "field-name".into(),
            ))
            .child(input_row(
                "Base URL",
                &st.settings.set_form_url,
                "field-url".into(),
            ))
        })
        .child(
            div()
                .v_flex()
                .gap(px(6.))
                .child(field_label(if builtin {
                    "API 密钥"
                } else if setup {
                    "API Key(必填)"
                } else {
                    "API Key"
                }))
                .children(st.settings.key_input.as_ref().map(|e| {
                    div()
                        .h(px(34.))
                        .debug_selector(|| "field-key".to_string())
                        .child(Input::new(e).small())
                })),
        )
        .when(builtin, |el| {
            // 「自定义设置」折叠:目录
            // 契约字段只读展示——适配器与目录绑定,改写走自定义流
            el.child(
                div()
                    .id("builtin-advanced-toggle")
                    .debug_selector(|| "builtin-advanced-toggle".to_string())
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(theme::LABEL_2())
                    .hover(|s| s.text_color(theme::LABEL()))
                    .child(if st.settings.builtin_advanced_open {
                        fixed(IconName::ArrowDown, 13.)
                    } else {
                        fixed(IconName::ChevronRight, 13.)
                    })
                    .child("自定义设置")
                    .on_mouse_down(gpui_kit::MouseButton::Left, {
                        let s = store.clone();
                        move |_, _, cx| {
                            s.update(cx, |st, cx| {
                                st.settings.builtin_advanced_open =
                                    !st.settings.builtin_advanced_open;
                                cx.notify();
                            });
                        }
                    }),
            )
            .when(st.settings.builtin_advanced_open, |el| {
                if let Some(entry) = &picked {
                    el.child(info_line("Base URL", entry.base_url.clone()))
                        .child(info_line("适配器", entry.dialect.clone()))
                        // 模型清单可编辑草稿(目录预填打底,端点拉取更新,
                        // 随「应用」落盘——模型列表会更新,不锁目录契约)
                        .child(editor_models_block(store, cx, fetch_pid.clone()).into_any_element())
                        .child(info_line(
                            "计费预设",
                            if entry.billing.is_some() {
                                "已内置".to_string()
                            } else {
                                "无(可手填计费端点)".to_string()
                            },
                        ))
                } else {
                    el.child(caption_line("目录条目缺失"))
                }
            })
        })
        .when(!builtin, |el| {
            el.child(
                div()
                    .v_flex()
                    .gap(px(6.))
                    .child(field_label("API 格式"))
                    .children(st.settings.dialect_select.as_ref().map(|s| {
                        div()
                            .w(px(260.))
                            .h(px(36.))
                            .line_height(gpui_kit::relative(1.4))
                            .child(Select::new(s))
                    })),
            )
            .children(advanced())
        });
    let _ = (&s_add_model, &s_fetch, &s_billing, &s_kind);
    // 页脚:右对齐 取消/应用(胶囊钮)
    card = card.child(
        div()
            .flex()
            .justify_end()
            .gap(px(8.))
            .child(
                div()
                    .id("provider-editor-cancel")
                    .debug_selector(|| "provider-editor-cancel".to_string())
                    .flex()
                    .h(px(36.))
                    .items_center()
                    .px(px(14.))
                    .rounded(px(18.))
                    .border_1()
                    .border_color(theme::BORDER())
                    .cursor_pointer()
                    .text_size(px(14.))
                    .text_color(theme::LABEL_2())
                    .hover(|s| s.bg(theme::DOCK()))
                    .child("取消")
                    .on_click(move |_, _, cx| {
                        let id = close_id.clone();
                        s_cancel.update(cx, |st, cx| st.close_provider_editor(&id, cx));
                    }),
            )
            .child(
                div()
                    .id("provider-editor-apply")
                    .debug_selector(|| "provider-editor-apply".to_string())
                    .flex()
                    .h(px(36.))
                    .items_center()
                    .px(px(14.))
                    .rounded(px(18.))
                    .bg(theme::DOCK())
                    .cursor_pointer()
                    .text_size(px(14.))
                    .text_color(theme::LABEL())
                    .hover(|s| s.bg(theme::BUBBLE()))
                    .child("应用")
                    .on_click(move |_, _, cx| {
                        s_apply.update(cx, |st, cx| st.apply_provider_editor(cx));
                    }),
            ),
    );
    card.into_any_element()
}

/// 模型列表块(图2:空态虚线框;行删除;端点拉取 + 手动添加)
fn editor_models_block(store: &Entity<AppStore>, cx: &App, fetch_pid: String) -> impl IntoElement {
    let st = store.read(cx);
    let (s_add_model, s_fetch) = (store.clone(), store.clone());
    div()
        .v_flex()
        .gap(px(8.))
        .child(field_label("模型列表"))
        .child(if st.settings.set_form_models.is_empty() {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(px(10.))
                .border_1()
                .border_color(theme::BORDER_2())
                .border_dashed()
                .px(px(12.))
                .py(px(14.))
                .text_size(px(13.))
                .text_color(theme::CAPTION())
                .child(fixed(IconName::Info, 14.))
                .child("当前没有配置模型,添加模型后可在聊天中使用。")
                .into_any_element()
        } else {
            div()
                .v_flex()
                .gap(px(4.))
                .children(
                    st.settings
                        .set_form_models
                        .iter()
                        .enumerate()
                        .map(|(ix, m)| model_draft_row(store, cx, ix, m)),
                )
                .into_any_element()
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .children(st.settings.set_form_model_input.as_ref().map(|e| {
                    div()
                        .flex_1()
                        .h(px(34.))
                        .child(Input::new(e).small())
                        .into_any_element()
                }))
                .child(
                    div()
                        .id("model-add")
                        .debug_selector(|| "model-add".to_string())
                        .flex()
                        .h(px(32.))
                        .flex_shrink_0()
                        .items_center()
                        .gap(px(4.))
                        .px(px(10.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(theme::BORDER())
                        .cursor_pointer()
                        .text_size(px(13.))
                        .text_color(theme::LABEL_2())
                        .hover(|s| s.bg(theme::DOCK()))
                        .child(fixed(IconName::Plus, 13.))
                        .child("添加模型")
                        .on_click(move |_, window, cx| {
                            s_add_model.update(cx, |st, cx| {
                                st.add_model_manual(window, cx);
                            });
                        }),
                )
                .child(
                    div()
                        .id("models-fetch")
                        .debug_selector(|| "models-fetch".to_string())
                        .flex()
                        .h(px(32.))
                        .items_center()
                        .gap(px(4.))
                        .px(px(10.))
                        .rounded(px(8.))
                        .cursor_pointer()
                        .text_size(px(13.))
                        .text_color(theme::LABEL_2())
                        .hover(|s| s.bg(theme::DOCK()))
                        .when(st.settings.model_fetch_loading, |el| {
                            el.text_color(theme::ONGOING())
                        })
                        .child(fixed(IconName::Globe, 13.))
                        .child("从端点获取")
                        .on_click(move |_, _, cx| {
                            let pid = fetch_pid.clone();
                            s_fetch.update(cx, |st, cx| {
                                st.open_fetch_models(&pid, cx);
                            });
                        }),
                ),
        )
        .child(caption_line(
            "点「窗口」设置该模型的上下文 token 数(可写 256K / 1M,1M = 1,000,000);留空使用默认,仅影响自动压缩阈值与上下文计量。",
        ))
}

/// 计费端点块(开关 + 形态 + URL + JSON 路径)
fn editor_billing_block(
    store: &Entity<AppStore>,
    cx: &App,
    id: String,
    setup: bool,
    billing_pid: String,
) -> impl IntoElement {
    let st = store.read(cx);
    let (s_billing, s_kind) = (store.clone(), store.clone());
    let input_row =
        |label: &str, slot: &Option<Entity<InputState>>, id_fmt: String| -> gpui_kit::AnyElement {
            div()
                .v_flex()
                .gap(px(6.))
                .child(field_label(label))
                .children(slot.as_ref().map(|e| {
                    div()
                        .h(px(34.))
                        .debug_selector(move || id_fmt.clone())
                        .child(Input::new(e).small())
                }))
                .into_any_element()
        };
    div()
        .v_flex()
        .gap(px(8.))
        .child(
            div()
                .id("billing-toggle")
                .debug_selector(|| "billing-toggle".to_string())
                .flex()
                .items_center()
                .justify_between()
                .child(field_label("计费端点(余额 / 用量展示)"))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(12.))
                        .text_color(if st.settings.set_form_billing_enabled {
                            theme::LABEL_2()
                        } else {
                            theme::CAPTION()
                        })
                        .child(if st.settings.set_form_billing_enabled {
                            "已启用"
                        } else {
                            "未启用"
                        })
                        .child(toggle_switch(st.settings.set_form_billing_enabled))
                        .on_mouse_down(gpui_kit::MouseButton::Left, {
                            let s = s_billing.clone();
                            move |_, _, cx| {
                                s.update(cx, |st, cx| st.toggle_billing_enabled(cx));
                            }
                        }),
                ),
        )
        .when(st.settings.set_form_billing_enabled, |el| {
            el.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(billing_kind_chip(
                        store,
                        "余额",
                        "balance",
                        &st.settings.set_form_billing_kind,
                    ))
                    .child(billing_kind_chip(
                        store,
                        "用量",
                        "usage",
                        &st.settings.set_form_billing_kind,
                    )),
            )
            .child(input_row(
                "查询 URL(GET)",
                &st.settings.set_form_billing_url,
                "field-billing-url".into(),
            ))
            .children(if st.settings.set_form_billing_kind == "usage" {
                vec![
                    input_row(
                        "5小时用量路径",
                        &st.settings.set_form_path_5h,
                        "field-p5h".into(),
                    ),
                    input_row(
                        "7天用量路径",
                        &st.settings.set_form_path_7d,
                        "field-p7d".into(),
                    ),
                    input_row(
                        "重置时间路径(可选)",
                        &st.settings.set_form_path_resets,
                        "field-presets".into(),
                    ),
                ]
            } else {
                vec![
                    input_row(
                        "余额金额路径",
                        &st.settings.set_form_path_balance,
                        "field-pbal".into(),
                    ),
                    input_row(
                        "货币路径(可选)",
                        &st.settings.set_form_path_currency,
                        "field-pcur".into(),
                    ),
                ]
            })
            .when(!setup && !id.is_empty(), |el| {
                el.child(
                    div()
                        .id("billing-refresh-now")
                        .flex()
                        .h(px(30.))
                        .w(px(88.))
                        .items_center()
                        .justify_center()
                        .rounded(px(8.))
                        .border_1()
                        .border_color(theme::BORDER())
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(
                            if st.settings.billing_refreshing.as_deref() == Some(id.as_str()) {
                                theme::ONGOING()
                            } else {
                                theme::LABEL_2()
                            },
                        )
                        .hover(|s| s.bg(theme::DOCK()))
                        .child(
                            if st.settings.billing_refreshing.as_deref() == Some(id.as_str()) {
                                "刷新中…"
                            } else {
                                "立即刷新"
                            },
                        )
                        .on_click(move |_, _, cx| {
                            let pid = billing_pid.clone();
                            s_kind.update(cx, |st, cx| {
                                st.refresh_billing_now(&pid, true, cx);
                            });
                        }),
                )
            })
        })
}

/// 草稿模型行:名称 + 上下文窗口 chip(展开行内编辑)+ 移除
fn model_draft_row(store: &Entity<AppStore>, cx: &App, ix: usize, model: &str) -> impl IntoElement {
    let st = store.read(cx);
    let editing = st.settings.context_window_edit.as_deref() == Some(model);
    // 窗口 chip:已覆盖 = 「窗口 128,000」;无覆盖 = 「窗口 默认」(点击展开编辑)
    let chip_text = st
        .settings
        .set_form_context_windows
        .get(model)
        .map(|v| format!("窗口 {}", grouped_tokens(*v)))
        .unwrap_or_else(|| "窗口 默认".to_string());
    let s_chip = store.clone();
    let s_remove = store.clone();
    let chip_model = model.to_string();
    div()
        .v_flex()
        .gap(px(4.))
        .child(
            div()
                .id(sid("model-draft", &ix.to_string()))
                .debug_selector(|| format!("model-draft-{ix}"))
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(30.))
                .px(px(10.))
                .rounded(px(8.))
                .bg(theme::SIDEBAR())
                .text_size(px(13.))
                .text_color(theme::LABEL_2())
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .child(model.to_string()),
                )
                .child(
                    div()
                        .id(sid("model-window", &ix.to_string()))
                        .debug_selector(|| format!("model-window-{ix}"))
                        .flex()
                        .h(px(22.))
                        .flex_shrink_0()
                        .items_center()
                        .px(px(8.))
                        .rounded(px(11.))
                        .border_1()
                        .border_color(if editing {
                            theme::BRAND()
                        } else {
                            theme::BORDER()
                        })
                        .cursor_pointer()
                        .text_size(px(11.))
                        .text_color(if editing {
                            theme::LABEL()
                        } else {
                            theme::CAPTION()
                        })
                        .hover(|s| s.text_color(theme::LABEL()))
                        .child(chip_text)
                        .on_click(move |_, window, cx| {
                            let m = chip_model.clone();
                            s_chip
                                .update(cx, |st, cx| st.begin_context_window_edit(&m, window, cx));
                        }),
                )
                .child(
                    div()
                        .id(sid("model-draft-remove", &ix.to_string()))
                        .flex()
                        .size(px(20.))
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_color(theme::CAPTION())
                        .hover(|s| s.bg(theme::DOCK()).text_color(theme::DANGER()))
                        .child(fixed(IconName::Close, 12.))
                        .on_click(move |_, _, cx| {
                            s_remove.update(cx, |st, cx| st.remove_form_model(ix, cx));
                        }),
                ),
        )
        .when(editing, |el| el.child(context_window_edit_row(store, cx)))
}

/// 窗口行内编辑行(展开态):输入 + 应用/取消;非法时行内提示
fn context_window_edit_row(store: &Entity<AppStore>, cx: &App) -> gpui_kit::AnyElement {
    let st = store.read(cx);
    let (s_apply, s_cancel) = (store.clone(), store.clone());
    div()
        .v_flex()
        .gap(px(4.))
        .px(px(10.))
        .pb(px(2.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .children(st.settings.context_window_input.as_ref().map(|e| {
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .h(px(30.))
                        .debug_selector(|| "model-window-input".to_string())
                        .child(Input::new(e).small())
                }))
                .child(
                    div()
                        .id("model-window-apply")
                        .debug_selector(|| "model-window-apply".to_string())
                        .flex()
                        .h(px(26.))
                        .flex_shrink_0()
                        .items_center()
                        .px(px(10.))
                        .rounded(px(13.))
                        .bg(theme::DOCK())
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(theme::LABEL())
                        .hover(|s| s.bg(theme::BUBBLE()))
                        .child("应用")
                        .on_click(move |_, _, cx| {
                            s_apply.update(cx, |st, cx| {
                                st.commit_context_window_edit(cx);
                            });
                        }),
                )
                .child(
                    div()
                        .id("model-window-cancel")
                        .debug_selector(|| "model-window-cancel".to_string())
                        .flex()
                        .h(px(26.))
                        .flex_shrink_0()
                        .items_center()
                        .px(px(10.))
                        .rounded(px(13.))
                        .border_1()
                        .border_color(theme::BORDER())
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(theme::LABEL_2())
                        .hover(|s| s.bg(theme::DOCK()))
                        .child("取消")
                        .on_click(move |_, _, cx| {
                            s_cancel.update(cx, |st, cx| st.cancel_context_window_edit(cx));
                        }),
                ),
        )
        .when(st.settings.context_window_error, |el| {
            el.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::DANGER())
                    .child("填 1 以上的整数或带单位(256K / 1M),或留空使用默认值"),
            )
        })
        .into_any_element()
}

/// 开关(toggle;开 = BRAND 底白点右,关 = DOCK 底灰点左)
fn toggle_switch(on: bool) -> impl IntoElement {
    div()
        .flex()
        .w(px(34.))
        .h(px(18.))
        .items_center()
        .rounded(px(9.))
        .bg(if on { theme::BRAND() } else { theme::DOCK() })
        .px(px(2.))
        .justify_end()
        .when(!on, |el| el.flex().justify_start())
        .child(div().size(px(14.)).rounded_full().bg(if on {
            theme::LABEL()
        } else {
            theme::LABEL_3()
        }))
}

/// 计费形态 chip(余额 / 用量)
fn billing_kind_chip(
    store: &Entity<AppStore>,
    label: &'static str,
    kind: &'static str,
    selected: &str,
) -> impl IntoElement {
    let s = store.clone();
    let active = kind == selected;
    div()
        .id(sid("billing-kind", kind))
        .flex()
        .h(px(28.))
        .items_center()
        .px(px(8.))
        .rounded(px(14.))
        .border_1()
        .border_color(if active {
            theme::BRAND()
        } else {
            theme::BORDER()
        })
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(if active {
            theme::LABEL()
        } else {
            theme::LABEL_3()
        })
        .hover(|s| s.bg(theme::DOCK()))
        .child(label.to_string())
        .on_click(move |_, _, cx| {
            let k = kind.to_string();
            s.update(cx, |st, cx| st.set_billing_kind(&k, cx));
        })
}

/// 字段标签(12/500 secondary)
fn field_label(text: &str) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(theme::LABEL_2())
        .child(text.to_string())
}

/// About 区:版本与产品定位
fn about_section(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let st = store.read(cx);
    let info = &st.state.host_info;
    div()
        .v_flex()
        .gap(px(12.))
        .child(section_title("关于"))
        .child(info_line("版本", info.version.clone()))
        .child(intro_line("流马 liuma —— Rust 桌面 agent harness。"))
}

/// 通用区(行序与形态按 settings.general.item):
/// 语言行(左标题 + 右选择 pill)→ 外观组(纵向:标题 + 三 cube,
/// 图标上文字下)→ 运行中 Enter 行为行(左标题+描述 / 右选择)
fn general_section(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let st = store.read(cx);
    let busy = st.settings.settings_snapshot["busyEnter"]
        .as_str()
        .unwrap_or("queue");
    let language = st.settings.settings_snapshot["language"]
        .as_str()
        .unwrap_or("zh");
    let appearance = st.settings.settings_snapshot["appearance"]
        .as_str()
        .unwrap_or("dark");
    let preset = st.settings.settings_snapshot["defaultPreset"]
        .as_str()
        .unwrap_or("standard");
    let permission = st.settings.settings_snapshot["defaultPermission"]
        .as_str()
        .unwrap_or("workspace-write");
    let preset_options = snapshot_options(&st.settings.settings_snapshot["presetOptions"]);
    let permission_options: Vec<(String, String)> =
        st.settings.settings_snapshot["permissionOptions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        let id = p.as_str()?;
                        let label = match id {
                            "read-only" => "仅可查看",
                            "workspace-write" => "工作区内修改",
                            "full-access" => "完全权限",
                            other => other,
                        };
                        Some((id.to_string(), label.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
    let busy_options = vec![
        ("queue".to_string(), "排队发送".to_string()),
        ("steer".to_string(), "插话发送".to_string()),
    ];
    let language_options = vec![("zh".to_string(), "中文".to_string())];
    div()
        .v_flex()
        .child(section_title("常规"))
        .mt(px(12.))
        // 行序:Agent 预设 / 权限 / 语言 / 外观 / 繁忙时 Enter 键行为
        .child(selector_row(
            "agent-preset",
            "Agent 预设",
            "对此后新建的会话生效。运行中的会话保持它开始时的预设。",
            &preset_options,
            preset,
            st.settings.preset_select.as_ref(),
        ))
        .child(selector_row(
            "permission",
            "权限",
            "选择新会话的默认权限模式",
            &permission_options,
            permission,
            st.settings.permission_select.as_ref(),
        ))
        .child(selector_row(
            "language",
            "语言",
            "",
            &language_options,
            language,
            st.settings.language_select.as_ref(),
        ))
        .child(appearance_group(store, appearance))
        .child(selector_row(
            "busy-enter",
            "繁忙时 Enter 键行为",
            "仅在智能体运行时生效;Cmd/Ctrl+Enter 使用另一行为",
            &busy_options,
            busy,
            st.settings.busy_enter_select.as_ref(),
        ))
}

/// 快照选项数组 → (id, name)
fn snapshot_options(v: &serde_json::Value) -> Vec<(String, String)> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    Some((
                        p["id"].as_str()?.to_string(),
                        p["name"].as_str().unwrap_or_default().to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 选择器行(左 title+desc,右 Select 下拉[gpui-component])
fn selector_row(
    id: &'static str,
    title: &str,
    desc: &str,
    options: &[(String, String)],
    current: &str,
    select: Option<&Entity<SelectState<Vec<gpui_kit::SharedString>>>>,
) -> impl IntoElement {
    let _ = options;
    let _ = current;
    let row_sel = sid("pref-row", id);
    div()
        .id(row_sel.clone())
        .debug_selector(move || row_sel.to_string())
        .v_flex()
        .py(px(16.))
        .border_b_1()
        .border_color(theme::BORDER())
        .child(
            div()
                .flex()
                .items_center()
                .child(
                    div()
                        .v_flex()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(4.))
                        .child(div().text_size(px(14.)).child(title.to_string()))
                        .when(!desc.is_empty(), |el| {
                            el.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::CAPTION())
                                    .child(desc.to_string()),
                            )
                        }),
                )
                // Select 自带 size_full:必须装进定尺寸容器,否则撑爆行高并挤塌文字列。
                // line_height 经 deferred 弹层沿元素树继承——组件项 padding 紧凑,
                // 默认行高对 CJK 偏窄(下拉选项字形相触)
                .children(select.map(|s| {
                    div()
                        .w(px(200.))
                        .h(px(36.))
                        .line_height(gpui_kit::relative(1.4))
                        .child(Select::new(s))
                })),
        )
}

/// 外观组(标题 + cube 行;cube = 图标上文字下,
/// r16,选中 = 模块填充 + 描边)
fn appearance_group(store: &Entity<AppStore>, current: &str) -> impl IntoElement {
    let cubes: [(&str, &str, gpui_kit::component::Icon); 3] = [
        ("light", "浅色", fixed(IconName::Sun, 20.)),
        ("dark", "深色", fixed(IconName::Moon, 20.)),
        ("system", "跟随系统", fixed(LiumaIcon::Monitor, 20.)),
    ];
    let mut row = div().flex().gap(px(8.));
    for (id, label, icon) in cubes {
        let s = store.clone();
        let active = id == current;
        let sel = sid("appearance-cube", id);
        row = row.child(
            div()
                .id(sel.clone())
                .debug_selector(move || sel.to_string())
                .flex()
                .flex_1()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(4.))
                .py(px(20.))
                .rounded(px(16.))
                .border_1()
                .border_color(if active {
                    theme::LABEL_3()
                } else {
                    theme::BORDER()
                })
                .when(active, |el| el.bg(theme::DOCK()))
                .cursor_pointer()
                .text_size(px(14.))
                .text_color(if active {
                    theme::LABEL()
                } else {
                    theme::LABEL_2()
                })
                .when(!active, |el| el.hover(|s| s.bg(theme::LAYER())))
                .child(icon)
                .child(label)
                .on_click(move |_, window, cx| {
                    // 落盘成功即实装生效:同步切主题盘 + 组件 token(点击
                    // 闭包已持 window,Theme::change 直刷本窗)
                    let ok = s.update(cx, |st, cx| st.set_appearance(id, cx));
                    if ok {
                        theme::apply(theme::Appearance::parse(id), Some(window), cx);
                    }
                }),
        );
    }
    div()
        .id("appearance-group")
        .debug_selector(|| "appearance-group".to_string())
        .v_flex()
        .gap(px(8.))
        .py(px(16.))
        .border_b_1()
        .border_color(theme::BORDER())
        .child(div().text_size(px(14.)).child("外观"))
        .child(row)
}

/// 信息行(label 11 说明号 + 值 13 正文号)
fn info_line(label: &str, value: impl Into<String>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .w(px(72.))
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(theme::CAPTION())
                .child(label.to_string()),
        )
        .child(
            div()
                .flex()
                .min_w(px(0.))
                .flex_1()
                .truncate()
                .text_size(px(13.))
                .text_color(theme::LABEL_2())
                .child(value.into()),
        )
}

/// 说明行(11 说明号)
fn caption_line(text: impl Into<String>) -> impl IntoElement {
    div()
        .text_size(px(11.))
        .text_color(theme::CAPTION())
        .child(text.into())
}

/// Provider 删除确认模态(shell/mod.rs 根级渲染)
/// 从端点获取模型弹层(候选多选 + 采纳;loading 态获取中)
pub fn provider_models_fetch_modal(store: &Entity<AppStore>, cx: &App) -> gpui_kit::AnyElement {
    let st = store.read(cx);
    let Some(mf) = &st.settings.model_fetch else {
        return div().into_any_element();
    };
    let loading = st.settings.model_fetch_loading;
    let picked_count = mf.picked.iter().filter(|p| **p).count();
    let (s_cancel, s_adopt, s_mask) = (store.clone(), store.clone(), store.clone());
    let mut rows: Vec<gpui_kit::AnyElement> = Vec::new();
    if loading {
        rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .py(px(20.))
                .justify_center()
                .text_size(px(13.))
                .text_color(theme::CAPTION())
                .child("获取中…")
                .into_any_element(),
        );
    } else {
        rows.extend(mf.candidates.iter().enumerate().map(|(ix, m)| {
            let s_toggle = store.clone();
            let picked = mf.picked.get(ix).copied().unwrap_or(false);
            div()
                .id(sid("fetch-cand", &ix.to_string()))
                .debug_selector(|| format!("fetch-cand-{ix}"))
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(30.))
                .px(px(8.))
                .rounded(px(8.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::DOCK()))
                .child(
                    div()
                        .flex()
                        .size(px(14.))
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(if picked {
                            theme::BRAND()
                        } else {
                            theme::BORDER()
                        })
                        .bg(if picked {
                            theme::BRAND()
                        } else {
                            theme::TRANSPARENT()
                        })
                        .text_color(theme::LABEL())
                        .children(picked.then(|| fixed(IconName::Check, 11.))),
                )
                .text_size(px(13.))
                .text_color(theme::LABEL_2())
                .child(m.clone())
                .on_click(move |_, _, cx| {
                    s_toggle.update(cx, |st, cx| st.toggle_fetch_pick(ix, cx));
                })
                .into_any_element()
        }));
    }
    div()
        .id("models-fetch-overlay")
        .absolute()
        .size_full()
        .top_0()
        .left_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui_kit::Rgba {
            a: 0.6,
            ..theme::BASE()
        })
        .on_mouse_down(gpui_kit::MouseButton::Left, move |_, _, cx| {
            s_mask.update(cx, |st, cx| st.close_fetch_modal(cx));
        })
        .child(
            div()
                .id("models-fetch-card")
                .debug_selector(|| "models-fetch-card".to_string())
                .v_flex()
                .w(px(440.))
                .gap(px(12.))
                .rounded(px(14.))
                .border_1()
                .border_color(theme::BORDER())
                .bg(theme::LAYER())
                .p(px(20.))
                .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation()
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .child("从端点获取模型"),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::CAPTION())
                                .child(format!("已选 {picked_count}")),
                        ),
                )
                .child(
                    div()
                        .id("models-fetch-list")
                        .v_flex()
                        .gap(px(2.))
                        .max_h(px(320.))
                        .overflow_y_scroll()
                        .children(rows),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            div()
                                .id("models-fetch-cancel")
                                .flex()
                                .h(px(32.))
                                .items_center()
                                .px(px(14.))
                                .rounded(px(16.))
                                .border_1()
                                .border_color(theme::BORDER())
                                .cursor_pointer()
                                .text_size(px(12.))
                                .text_color(theme::LABEL_2())
                                .hover(|s| s.bg(theme::DOCK()))
                                .child("取消")
                                .on_click(move |_, _, cx| {
                                    s_cancel.update(cx, |st, cx| st.close_fetch_modal(cx));
                                }),
                        )
                        .when(!loading, |el| {
                            el.child(
                                div()
                                    .id("models-fetch-adopt")
                                    .flex()
                                    .h(px(32.))
                                    .items_center()
                                    .px(px(14.))
                                    .rounded(px(16.))
                                    .bg(theme::DOCK())
                                    .cursor_pointer()
                                    .text_size(px(12.))
                                    .text_color(theme::LABEL())
                                    .hover(|s| s.bg(theme::BUBBLE()))
                                    .child(format!("采纳 {picked_count} 项"))
                                    .on_click(move |_, _, cx| {
                                        s_adopt.update(cx, |st, cx| {
                                            st.adopt_fetched_models(cx);
                                        });
                                    }),
                            )
                        }),
                ),
        )
        .into_any_element()
}

/// 首运行 onboarding 模态:无任何可用
/// 凭据时弹出,默认 deepseek provider,只填 key。稍后配置 = 完成引导
/// (不再弹);保存并继续 = key 写入 deepseek 并完成
pub(crate) fn onboarding_modal(store: &Entity<AppStore>, cx: &App) -> gpui_kit::AnyElement {
    let st = store.read(cx);
    let (s_save, s_later) = (store.clone(), store.clone());
    let mut card = div()
        .id("onboarding-card")
        .debug_selector(|| "onboarding-card".to_string())
        .v_flex()
        .w(px(460.))
        .gap(px(14.))
        .rounded(px(14.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::LAYER())
        .p(px(24.))
        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
            cx.stop_propagation()
        })
        .child(
            div()
                .text_size(px(17.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(theme::LABEL())
                .child("添加一个 API Key 开始使用"),
        )
        .child(
            div()
                .text_size(px(13.))
                .text_color(theme::LABEL_2())
                .child("配置 DeepSeek 官方模型，即可开始使用。"),
        )
        .child(
            div()
                .v_flex()
                .gap(px(6.))
                .child(field_label("API 密钥"))
                .children(st.settings.onboarding_key_input.as_ref().map(|e| {
                    div()
                        .id("onboarding-key")
                        .debug_selector(|| "onboarding-key".to_string())
                        .h(px(36.))
                        .child(Input::new(e).small())
                })),
        );
    if let Some(err) = &st.settings.onboarding_key_error {
        card = card.child(
            div()
                .id("onboarding-error")
                .debug_selector(|| "onboarding-error".to_string())
                .text_size(px(12.))
                .text_color(theme::DANGER())
                .child(err.clone()),
        );
    }
    card = card.child(
        div()
            .flex()
            .justify_end()
            .gap(px(10.))
            .child(
                div()
                    .id("onboarding-later")
                    .debug_selector(|| "onboarding-later".to_string())
                    .flex()
                    .h(px(34.))
                    .items_center()
                    .px(px(16.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(theme::BORDER())
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(theme::LABEL_2())
                    .hover(|s| s.bg(theme::DOCK()))
                    .child("稍后配置")
                    .on_click(move |_, _, cx| {
                        s_later.update(cx, |st, cx| st.onboarding_later(cx));
                    }),
            )
            .child(
                div()
                    .id("onboarding-save")
                    .debug_selector(|| "onboarding-save".to_string())
                    .flex()
                    .h(px(34.))
                    .items_center()
                    .px(px(16.))
                    .rounded(px(10.))
                    .bg(theme::BRAND())
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(theme::LABEL())
                    .hover(|s| s.opacity(0.9))
                    .child("保存并继续")
                    .on_click(move |_, _, cx| {
                        s_save.update(cx, |st, cx| st.onboarding_save(cx));
                    }),
            ),
    );
    div()
        .id("onboarding-overlay")
        .debug_selector(|| "onboarding-overlay".to_string())
        .absolute()
        .size_full()
        .top_0()
        .left_0()
        .flex()
        .items_start()
        .justify_center()
        .pt(px(140.))
        .bg(gpui_kit::Rgba {
            a: 0.6,
            ..theme::BASE()
        })
        .child(card)
        .into_any_element()
}

pub fn provider_delete_modal(store: &Entity<AppStore>, cx: &App) -> gpui_kit::AnyElement {
    let Some(id) = store.read(cx).settings.delete_provider_target.clone() else {
        return div().into_any_element();
    };
    let (s_cancel, s_confirm, s_mask) = (store.clone(), store.clone(), store.clone());
    div()
        .id("provider-delete-overlay")
        .absolute()
        .size_full()
        .top_0()
        .left_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui_kit::Rgba {
            a: 0.6,
            ..theme::BASE()
        })
        .on_mouse_down(gpui_kit::MouseButton::Left, move |_, _, cx| {
            s_mask.update(cx, |st, cx| st.cancel_delete_provider(cx));
        })
        .child(
            div()
                .id("provider-delete-card")
                .debug_selector(|| "provider-delete-card".to_string())
                .v_flex()
                .w(px(420.))
                .gap(px(12.))
                .rounded(px(14.))
                .border_1()
                .border_color(theme::BORDER())
                .bg(theme::LAYER())
                .p(px(20.))
                .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation()
                })
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(format!("移除 {id}")),
                )
                .child(caption_line(
                    "移除会移除其配置和存储的 API 密钥;工作区引用回落内置默认。",
                ))
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            div()
                                .id("provider-delete-cancel")
                                .debug_selector(|| "provider-delete-cancel".to_string())
                                .flex()
                                .h(px(32.))
                                .items_center()
                                .px(px(14.))
                                .rounded(px(16.))
                                .border_1()
                                .border_color(theme::BORDER())
                                .cursor_pointer()
                                .text_size(px(12.))
                                .text_color(theme::LABEL_2())
                                .hover(|s| s.bg(theme::DOCK()))
                                .child("取消")
                                .on_click(move |_, _, cx| {
                                    s_cancel.update(cx, |st, cx| st.cancel_delete_provider(cx));
                                }),
                        )
                        .child(
                            div()
                                .id("provider-delete-confirm")
                                .debug_selector(|| "provider-delete-confirm".to_string())
                                .flex()
                                .h(px(32.))
                                .items_center()
                                .px(px(14.))
                                .rounded(px(16.))
                                .border_1()
                                .border_color(theme::DANGER())
                                .cursor_pointer()
                                .text_size(px(12.))
                                .text_color(theme::DANGER())
                                .hover(|s| s.bg(theme::DOCK()))
                                .child("移除")
                                .on_click(move |_, _, cx| {
                                    s_confirm.update(cx, |st, cx| st.confirm_delete_provider(cx));
                                }),
                        ),
                ),
        )
        .into_any_element()
}

/// full-access 风险确认模态(根级渲染:
/// 警示标题 + 后果段落 + 能力清单盒 + 风险脚注 + 取消/红色确认)
pub fn full_access_modal(store: &Entity<AppStore>, _cx: &App) -> gpui_kit::AnyElement {
    let (s_cancel, s_confirm, s_mask) = (store.clone(), store.clone(), store.clone());
    div()
        .id("full-access-overlay")
        .absolute()
        .size_full()
        .top_0()
        .left_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui_kit::Rgba {
            a: 0.6,
            ..theme::BASE()
        })
        .on_mouse_down(gpui_kit::MouseButton::Left, move |_, window, cx| {
            s_mask.update(cx, |st, cx| st.cancel_full_access(window, cx));
        })
        .child(
            div()
                .id("full-access-card")
                .debug_selector(|| "full-access-card".to_string())
                .v_flex()
                .w(px(440.))
                .gap(px(14.))
                .rounded(px(14.))
                .border_1()
                .border_color(theme::BORDER())
                .bg(theme::LAYER())
                .p(px(22.))
                .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation()
                })
                // 标题:警示图标 + 问句
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(fixed(IconName::TriangleAlert, 18.).text_color(theme::LABEL()))
                        .child(
                            div()
                                .text_size(px(16.))
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(theme::LABEL())
                                .child("要开启完全权限吗？"),
                        ),
                )
                // 后果段落
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::LABEL_2())
                        .child(
                            "智能体将能够在未经您许可的情况下，在这台计算机上的任何位置运行命令、使用互联网，以及创建和编辑文件。这包括但不限于：",
                        ),
                )
                // 能力清单盒(略深一档底;行间发丝线)
                .child(
                    div()
                        .id("full-access-list")
                        .debug_selector(|| "full-access-list".to_string())
                        .v_flex()
                        .rounded(px(10.))
                        .bg(theme::DOCK())
                        .child(risk_row(
                            fixed(IconName::Folder, 16.),
                            "文件和文件夹",
                            "读取、创建、修改、上传或删除此计算机上任意位置的文件",
                        ))
                        .child(div().w_full().h(px(1.)).bg(theme::BORDER()))
                        .child(risk_row(
                            fixed(IconName::SquareTerminal, 16.),
                            "终端命令",
                            "运行命令、安装软件和更改系统设置",
                        ))
                        .child(div().w_full().h(px(1.)).bg(theme::BORDER()))
                        .child(risk_row(
                            fixed(IconName::Globe, 16.),
                            "互联网和已连接的应用",
                            "访问网站、发送数据并使用已启用的插件",
                        )),
                )
                // 风险脚注
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::CAPTION())
                        .child("这会带来敏感数据丢失或泄露、提示注入等风险。你可以将其关闭。"),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .id("full-access-cancel")
                                .debug_selector(|| "full-access-cancel".to_string())
                                .flex()
                                .h(px(32.))
                                .items_center()
                                .px(px(14.))
                                .rounded(px(16.))
                                .border_1()
                                .border_color(theme::BORDER())
                                .cursor_pointer()
                                .text_size(px(13.))
                                .text_color(theme::LABEL_2())
                                .hover(|s| s.bg(theme::DOCK()))
                                .child("取消")
                                .on_click(move |_, window, cx| {
                                    s_cancel.update(cx, |st, cx| {
                                        st.cancel_full_access(window, cx)
                                    });
                                }),
                        )
                        .child(
                            div()
                                .id("full-access-confirm")
                                .debug_selector(|| "full-access-confirm".to_string())
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .h(px(32.))
                                .px(px(14.))
                                .rounded(px(16.))
                                .bg(gpui_kit::Rgba {
                                    a: 0.14,
                                    ..theme::DANGER()
                                })
                                .cursor_pointer()
                                .text_size(px(13.))
                                .text_color(theme::DANGER())
                                .hover(|s| {
                                    s.bg(gpui_kit::Rgba {
                                        a: 0.22,
                                        ..theme::DANGER()
                                    })
                                })
                                .child(fixed(IconName::TriangleAlert, 13.))
                                .child("确认")
                                .on_click(move |_, _, cx| {
                                    s_confirm.update(cx, |st, cx| {
                                        st.confirm_full_access(cx)
                                    });
                                }),
                        ),
                ),
        )
        .into_any_element()
}

/// 风险确认弹窗能力行(图标 + 标题 + 灰描述;文本列 flex_1 换行,
/// 图标顶对齐标题线)
fn risk_row(icon: gpui_kit::component::Icon, title: &str, desc: &str) -> impl IntoElement {
    div()
        .flex()
        .items_start()
        .gap(px(10.))
        .px(px(12.))
        .py(px(10.))
        .child(icon.text_color(theme::LABEL_2()))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .v_flex()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::LABEL())
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::CAPTION())
                        .child(desc.to_string()),
                ),
        )
}

// ── 侧栏设置模式菜单与设置行(自 ui::sidebar 切出;git 历史在 sidebar 侧可循)──

/// 设置模式侧栏:顶部「返回工作区」+ 标题「设置」+ **分组导航**(
/// 基础设置/Agent 能力/数据与统计三组头,
/// 仅渲染已有实现的项——组内无实装项则不显示空组头)+ 底部「关于」。
/// 图标行形态:左图标右标签、激活整行 pill(DOCK),组头为
/// 小号说明字。插件/MCP/技能待实装后进各自组——入口迁移优于新增。
pub(crate) fn menu(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    use super::SettingsNav;
    let st = store.read(cx);
    let back = store.clone();
    // 「基础设置」组:常规 / 模型设置(模型行名为「模型」);
    // 「Agent 能力」「数据与统计」无已实装项,不渲染空组头
    let basic: [(SettingsNav, &str, gpui_kit::component::Icon); 4] = [
        (SettingsNav::General, "常规", fixed(IconName::Settings, 15.)),
        (
            SettingsNav::Models,
            "模型设置",
            fixed(LiumaIcon::Gauge, 15.),
        ),
        (SettingsNav::Mcp, "MCP", fixed(LiumaIcon::Infinity, 15.)),
        (SettingsNav::Hooks, "Hooks", fixed(LiumaIcon::Wrench, 15.)),
    ];
    let mut list = div().v_flex().gap(px(4.));
    list = list.child(nav_group_header("基础设置"));
    for (nav, label, icon) in basic {
        list = list.child(nav_item(store, nav, label, icon, st.settings.settings_nav));
    }
    div()
        .id("settings-menu")
        .debug_selector(|| "settings-menu".to_string())
        .v_flex()
        .h_full()
        .w(crate::shell::metrics::sidebar_width_for(
            false,
            st.sidebar_px,
        ))
        .flex_shrink_0()
        .bg(theme::SIDEBAR())
        .border_r_1()
        .border_color(theme::BORDER())
        .px(px(12.))
        .pt(px(36.))
        .pb(px(10.))
        .gap(px(8.))
        .child(crate::features::sessions::drag_strip())
        // 返回工作区按钮(整行显式按钮;标题「设置」独立一行在下)
        .child(
            div()
                .id("settings-back")
                .debug_selector(|| "settings-back".to_string())
                .flex()
                .h(px(32.))
                .items_center()
                .gap(px(6.))
                .rounded(px(8.))
                .px(px(8.))
                .cursor_pointer()
                .text_size(px(13.))
                .text_color(theme::LABEL_2())
                .hover(|s| s.bg(theme::LAYER()))
                .child(fixed(IconName::ArrowLeft, 15.))
                .child("返回工作区")
                .on_click(move |_, _, cx| {
                    back.update(cx, |st, cx| st.toggle_settings(cx));
                }),
        )
        .child(list)
        // 底部「关于」(RS 专有页;独立于三组,置底兜底)
        .child(div().v_flex().gap(px(4.)).child(nav_item(
            store,
            SettingsNav::About,
            "关于",
            fixed(IconName::Info, 15.),
            st.settings.settings_nav,
        )))
}

/// 分组导航组头(小号说明字)
fn nav_group_header(text: &str) -> impl IntoElement {
    div()
        .px(px(8.))
        .pt(px(8.))
        .pb(px(4.))
        .text_size(px(12.))
        .text_color(theme::CAPTION())
        .child(text.to_string())
}

/// 导航项行:左图标右标签,激活整行 pill(DOCK)高亮
fn nav_item(
    store: &Entity<AppStore>,
    nav: crate::features::settings::SettingsNav,
    label: &'static str,
    icon: gpui_kit::component::Icon,
    current: crate::features::settings::SettingsNav,
) -> impl IntoElement {
    let s = store.clone();
    let active = current == nav;
    let sel = format!("settings-nav-{label}");
    div()
        .id(gpui_kit::SharedString::from(format!(
            "settings-nav-item-{label}"
        )))
        .debug_selector(move || sel.clone())
        .flex()
        .h(px(36.))
        .items_center()
        .gap(px(8.))
        .rounded(px(8.))
        .px(px(8.))
        .cursor_pointer()
        .when(active, |el| el.bg(theme::DOCK()))
        .when(!active, |el| {
            el.hover(|s| s.bg(theme::LAYER()))
                .text_color(theme::LABEL_3())
        })
        .text_size(px(13.))
        .text_color(if active {
            theme::LABEL()
        } else {
            theme::LABEL_3()
        })
        .when(active, |el| el.font_weight(gpui_kit::FontWeight::MEDIUM))
        .child(icon)
        .child(label)
        .on_click(move |_, _, cx| {
            s.update(cx, |st, cx| st.set_settings_nav(nav, cx));
        })
}

/// 底部设置行(打开设置页;折叠 rail 的展开态对应物)
pub(crate) fn settings_row(store: &Entity<AppStore>) -> impl IntoElement {
    let s = store.clone();
    div()
        .id("settings")
        .debug_selector(|| "settings-row".to_string())
        .flex()
        .h(px(32.))
        .flex_shrink_0()
        .items_center()
        .rounded(px(8.))
        .px(px(8.))
        .gap(px(8.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::LAYER()))
        .text_size(px(13.))
        .text_color(theme::LABEL_3())
        .child(fixed(IconName::Settings, 16.))
        .child("设置")
        .on_click(move |_, _, cx| {
            s.update(cx, |st, cx| st.toggle_settings(cx));
        })
}

/// Hooks 区(Claude Code / Codex 桥;M4.2):行卡(id/方言/路径/启停/
/// 编辑/卸载)+ 添加卡 + 详情表单。视觉语言复刻 MCP Servers 区。
fn hooks_section(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    use super::store::HooksDetailState;
    let st = store.read(cx);
    let st_add = store.clone();
    let Some(detail) = st.settings.hooks_detail.clone() else {
        // ── 列表页 ──
        let bridges = st.settings.settings_snapshot["hookBridges"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut col = div()
            .v_flex()
            .gap(px(12.))
            .child(section_title("Hooks"))
            .child(intro_line(
                "把既有 Claude Code / Codex hooks.json 的 command 钩子接进会话:阻塞工具与提示、附加上下文、强制续跑;变更在下次会话附着时生效。",
            ));
        let total = bridges.len();
        col = col.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .mt(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child(format!("已安装 {total}")),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("hooks-add")
                        .debug_selector(|| "hooks-add".to_string())
                        .flex()
                        .h(px(28.))
                        .items_center()
                        .px(px(12.))
                        .rounded(px(8.))
                        .bg(theme::BRAND())
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(gpui_kit::white())
                        .hover(|s| s.opacity(0.9))
                        .on_click({
                            let st_open = st_add.clone();
                            move |_, window, cx| {
                                st_open.update(cx, |st, cx| st.open_hooks_add(window, cx));
                            }
                        })
                        .child("+ 新建"),
                ),
        );
        let mut rows = div().v_flex().gap(px(8.));
        if bridges.is_empty() {
            rows = rows.child(caption_line("尚未配置 hooks 桥。"));
        }
        for (ix, b) in bridges.into_iter().enumerate() {
            let id = b["id"].as_str().unwrap_or_default().to_string();
            let dialect = b["dialect"].as_str().unwrap_or_default().to_string();
            let config_path = b["configPath"].as_str().unwrap_or_default().to_string();
            let enabled = b["enabled"].as_bool().unwrap_or(false);
            let (st_switch, st_edit, st_remove) = (store.clone(), store.clone(), store.clone());
            let (id_switch_click, id_edit_click, id_remove_click) =
                (id.clone(), id.clone(), id.clone());
            let (id_sw_dbg, id_sw_click) = (id_switch_click.clone(), id_switch_click.clone());
            let (id_ed_dbg, id_ed_click) = (id_edit_click.clone(), id_edit_click.clone());
            let (id_rm_dbg, id_rm_click) = (id_remove_click.clone(), id_remove_click.clone());
            rows = rows.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .rounded(px(10.))
                    .bg(theme::LAYER())
                    .px(px(12.))
                    .py(px(10.))
                    .child(
                        div()
                            .v_flex()
                            .gap(px(2.))
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                                    .text_color(theme::LABEL())
                                    .child(id_switch_click.clone()),
                            )
                            .child(
                                div().flex().items_center().gap(px(6.)).child(
                                    div()
                                        .min_w(px(0.))
                                        .text_size(px(11.))
                                        .text_color(theme::CAPTION())
                                        .child(format!("{dialect} · {config_path}")),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .id(("hooks-switch", ix))
                            .debug_selector(move || format!("hooks-switch-{id_sw_dbg}"))
                            .on_mouse_down(gpui_kit::MouseButton::Left, {
                                let st_switch = st_switch.clone();
                                let id_sw = id_sw_click.clone();
                                move |_, _, cx| {
                                    st_switch
                                        .update(cx, |st, cx| st.toggle_hook_bridge(&id_sw, cx));
                                }
                            })
                            .child(toggle_switch(enabled)),
                    )
                    .child(
                        div()
                            .id(("hooks-edit", ix))
                            .debug_selector(move || format!("hooks-edit-{id_ed_dbg}"))
                            .flex()
                            .h(px(22.))
                            .items_center()
                            .px(px(8.))
                            .rounded(px(11.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(theme::LABEL_2())
                            .hover(|s| s.bg(theme::DOCK()))
                            .on_click(move |_, window, cx| {
                                st_edit.update(cx, |st, cx| {
                                    st.open_hooks_edit(&id_ed_click, window, cx)
                                });
                            })
                            .child("编辑"),
                    )
                    .child(
                        div()
                            .id(("hooks-remove", ix))
                            .debug_selector(move || format!("hooks-remove-{id_rm_dbg}"))
                            .flex()
                            .h(px(22.))
                            .items_center()
                            .px(px(8.))
                            .rounded(px(11.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(theme::DANGER())
                            .hover(|s| s.bg(theme::DOCK()))
                            .on_click(move |_, _, cx| {
                                st_remove
                                    .update(cx, |st, cx| st.remove_hook_bridge(&id_rm_click, cx));
                            })
                            .child("卸载"),
                    ),
            );
        }
        return col.child(rows).into_any_element();
    };

    // ── 详情页(新增/编辑表单)──
    let st_back = store.clone();
    let st_save = store.clone();
    let st_dialect_cc = store.clone();
    let st_dialect_codex = store.clone();
    let st_toggle = store.clone();
    let dialect_sel = |d: &HooksDetailState, want: &str| d.form_dialect == want;
    let mut col = div()
        .v_flex()
        .gap(px(12.))
        .child(section_title(if detail.editing.is_some() {
            "编辑 Hooks 桥"
        } else {
            "新建 Hooks 桥"
        }))
        .children(st.settings.settings_notice.as_ref().map(|(ok, msg)| {
            div()
                .debug_selector(|| "hooks-detail-notice".to_string())
                .text_size(px(12.))
                .text_color(if *ok {
                    theme::SUCCESS()
                } else {
                    theme::DANGER()
                })
                .child(format!("{} {msg}", if *ok { "✓" } else { "⚠" }))
        }))
        .child(
            div()
                .id("hooks-back")
                .flex()
                .h(px(28.))
                .w(px(72.))
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .bg(theme::LAYER())
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(theme::LABEL_2())
                .on_click({
                    let st_back = st_back.clone();
                    move |_, _, cx| {
                        st_back.update(cx, |st, cx| st.close_hooks_detail(cx));
                    }
                })
                .child("← 返回"),
        );
    // 方言
    {
        let (cc_sel, codex_sel) = (
            dialect_sel(&detail, "claude-code"),
            dialect_sel(&detail, "codex"),
        );
        col = col.child(section_title("方言")).child(
            div()
                .flex()
                .gap(px(8.))
                .child(
                    div()
                        .id("hooks-dialect-cc")
                        .flex()
                        .h(px(28.))
                        .items_center()
                        .px(px(12.))
                        .rounded(px(8.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(if cc_sel {
                            gpui_kit::white()
                        } else {
                            theme::LABEL_2().into()
                        })
                        .bg(if cc_sel {
                            theme::BRAND()
                        } else {
                            theme::LAYER()
                        })
                        .on_click({
                            let st_dialect_cc = st_dialect_cc.clone();
                            move |_, _, cx| {
                                st_dialect_cc
                                    .update(cx, |st, cx| st.set_hooks_dialect("claude-code", cx));
                            }
                        })
                        .child("claude-code"),
                )
                .child(
                    div()
                        .id("hooks-dialect-codex")
                        .flex()
                        .h(px(28.))
                        .items_center()
                        .px(px(12.))
                        .rounded(px(8.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(if codex_sel {
                            gpui_kit::white()
                        } else {
                            theme::LABEL_2().into()
                        })
                        .bg(if codex_sel {
                            theme::BRAND()
                        } else {
                            theme::LAYER()
                        })
                        .on_click({
                            let st_dialect_codex = st_dialect_codex.clone();
                            move |_, _, cx| {
                                st_dialect_codex
                                    .update(cx, |st, cx| st.set_hooks_dialect("codex", cx));
                            }
                        })
                        .child("codex"),
                ),
        );
    }
    // 启用开关
    col = col.child(section_title("启用")).child(
        div()
            .id("hooks-form-enabled")
            .on_mouse_down(gpui_kit::MouseButton::Left, {
                let st_toggle = st_toggle.clone();
                move |_, _, cx| {
                    st_toggle.update(cx, |st, cx| st.toggle_hooks_form_enabled(cx));
                }
            })
            .child(toggle_switch(detail.form_enabled)),
    );
    // 字段
    col = col
        .child(section_title("配置"))
        .child(field_input(
            "路径",
            "hooks-form-path",
            &detail.form_config_path,
        ))
        .child(field_input(
            "pluginRoot",
            "hooks-form-plugin-root",
            &detail.form_plugin_root,
        ))
        .child(field_input(
            "projectDir",
            "hooks-form-project-dir",
            &detail.form_project_dir,
        ))
        .child(field_input(
            "超时 ms",
            "hooks-form-timeout",
            &detail.form_timeout,
        ))
        .child(
            div()
                .id("hooks-save")
                .debug_selector(|| "hooks-save".to_string())
                .flex()
                .h(px(32.))
                .w(px(96.))
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .bg(theme::BRAND())
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(gpui_kit::white())
                .hover(|s| s.opacity(0.9))
                .on_click({
                    let st_save = st_save.clone();
                    move |_, window, cx| {
                        st_save.update(cx, |st, cx| st.save_hooks(window, cx));
                    }
                })
                .child("保存"),
        );
    col.into_any_element()
}

/// 重置倒计时格式化:剩余窗按量级取「天/时/分」,已过或非时间戳缺席
#[cfg(test)]
mod countdown_tests {
    use super::resets_countdown;

    #[test]
    fn formats_remaining_window() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        // +1s 缓冲:断言按分钟取整,而 resets_countdown 内部会重新读一次
        // 时钟(晚于本行捕获的 now)。若恰好跨毫秒边界,floor 会少 1 分钟
        // (实测并行负载下「4天22时」偶发成「4天21时」)。缓冲把边界推到
        // 秒级,消除该非确定。
        let at = |mins: u64| (now + mins * 60_000 + 1_000).to_string();
        assert_eq!(
            resets_countdown(&at(4 * 1440 + 22 * 60)).as_deref(),
            Some("4天22时")
        );
        assert_eq!(
            resets_countdown(&at(3 * 60 + 12)).as_deref(),
            Some("3时12分")
        );
        assert_eq!(resets_countdown(&at(45)).as_deref(), Some("45分"));
        // 已过 / 非时间戳 → 缺席(不显示倒计时)
        let past = (now - 60 * 60_000).to_string();
        assert_eq!(resets_countdown(&past), None);
        assert_eq!(resets_countdown("soon"), None);
    }
}
