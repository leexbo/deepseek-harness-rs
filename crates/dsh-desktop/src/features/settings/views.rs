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

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, px,
};
use gpui_kit::component::IconName;
use gpui_kit::component::InteractiveElementExt as _;
use gpui_kit::component::Sizable;
use gpui_kit::component::StyledExt;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::{Select, SelectState};

use crate::features::settings::SettingsNav;
use crate::kits::icons::{DshIcon, fixed};
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
        .bg(theme::BASE())
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
                // 区容器宽度(源 section max-width 720)
                .child(div().v_flex().w(px(720.)).child(
                    match store.read(cx).settings.settings_nav {
                        SettingsNav::Models => models_section(store, cx).into_any_element(),
                        SettingsNav::General => general_section(store, cx).into_any_element(),
                        SettingsNav::About => about_section(store, cx).into_any_element(),
                    },
                )),
        )
}

/// 区说明行(源 .intro:13/tertiary)
fn intro_line(text: impl Into<String>) -> impl IntoElement {
    div()
        .text_size(px(14.))
        .text_color(theme::LABEL_3())
        .child(text.into())
}

/// 区标题(源 .title 16/500)
fn section_title(text: &str) -> impl IntoElement {
    div()
        .text_size(px(16.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .child(text.to_string())
}

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
                .text_color(if *ok { theme::SUCCESS() } else { theme::DANGER() })
                .child(format!("{} {msg}", if *ok { "✓" } else { "⚠" }))
        }));
    let providers: Vec<serde_json::Value> = st.settings.settings_snapshot["providers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    // 行卡列表(源 .rows:与标题块之间 extra 12 空气,gap 8)
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

/// 保存通告行(源 .savedNotice:12/success)
fn saved_notice(name: &str) -> impl IntoElement {
    div()
        .id("provider-saved-notice")
        .text_size(px(12.))
        .text_color(theme::SUCCESS())
        .child(format!("已保存 {name}"))
}

/// 单个 provider 卡片(参考图1 形态):圆标 avatar + 名称 + URL 链接行;
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
        .border_color(if is_default {
            theme::BRAND()
        } else {
            theme::BORDER()
        })
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
                .child(fixed(DshIcon::Clock, 11.))
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
                    .child(div().text_color(theme::LABEL_3()).child(currency.to_string()))
                    .into_any_element(),
            )
        }
        Some("usage") => {
            let row = row.when_some(cache["pct_5h"].as_u64(), |el, v| {
                el.child("5小时:").child(
                    div()
                        .text_color(theme::SUCCESS())
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .child(format!("{v}%")),
                )
            });
            let row = row.when_some(cache["pct_7d"].as_u64(), |el, v| {
                el.child("7天:").child(
                    div()
                        .text_color(theme::SUCCESS())
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .child(format!("{v}%")),
                )
            });
            let row = row.when_some(cache["resets"].as_str(), |el, v| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(2.))
                        .text_color(theme::LABEL_3())
                        .child(fixed(DshIcon::Clock, 11.))
                        .child(v.to_string()),
                )
            });
            Some(row.into_any_element())
        }
        _ => None,
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

/// 凭据状态圆点(源 .credentialDot:8px 实心,success/error)
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

/// 行头「编辑」钮(源 .secondaryButton dense:28h r14 边框胶囊;再点收起)
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

/// 行头「移除」钮(源 .dangerButton dense:28h 胶囊,危险色文字)
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

/// 首运行 setup 卡(源 .setupCard:填充模块 = 该 provider 在页面上的
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

/// 添加块(源 .addBlock):开态 = 填充模块卡(id 输入 + 编辑器);
/// 闭态 = dashed 添加钮(源 44h r12,「空位」语义)
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
    let s = store.clone();
    div()
        .id("provider-add")
        .debug_selector(|| "provider-add".to_string())
        .flex()
        .h(px(44.))
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
        .child("添加 Provider")
        .on_click(move |_, window, cx| {
            s.update(cx, |st, cx| st.open_provider_add(window, cx));
        })
        .into_any_element()
}

/// 编辑卡(参考图2 形态):名称 / Base URL / API Key / API 格式 /
/// 模型列表(端点拉取多选 + 手动添加)/ 计费端点(开关 + 类型 + URL
/// + JSON 路径);页脚右对齐 取消/应用。setup/添加卡共用(无标题行)
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
    div()
        .id(editor_id)
        .debug_selector(move || editor_sel.clone())
        .v_flex()
        .gap(px(14.))
        .rounded(px(12.))
        .when(!setup && !id.is_empty(), |el| {
            el.bg(theme::SIDEBAR()).p(px(14.)).pr(px(16.))
        })
        // 标题行(行内编辑态;setup/添加卡无)
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
        // 图2 字段序:名称 / Base URL / API Key / API 格式
        .child(input_row(
            "名称",
            &st.settings.set_form_name,
            "field-name".into(),
        ))
        .child(input_row(
            "Base URL",
            &st.settings.set_form_url,
            "field-url".into(),
        ))
        .child(
            div()
                .v_flex()
                .gap(px(6.))
                .child(field_label(if setup {
                    "API Key(必填)"
                } else {
                    "API Key"
                }))
                .children(
                    st.settings
                        .key_input
                        .as_ref()
                        .map(|e| div().h(px(34.)).child(Input::new(e).small())),
                ),
        )
        .child(
            div()
                .v_flex()
                .gap(px(6.))
                .child(field_label("API 格式"))
                .child(dialect_chips(store, &st.settings.set_form_dialect)),
        )
        // 模型列表(图2:空态虚线框;行删除;端点拉取 + 手动添加)
        .child(
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
                                .map(|(ix, m)| model_draft_row(store, ix, m)),
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
                ),
        )
        // 计费端点(开关 + 形态 + URL + JSON 路径)
        .child(
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
                                    if st.settings.billing_refreshing.as_deref() == Some(id) {
                                        theme::ONGOING()
                                    } else {
                                        theme::LABEL_2()
                                    },
                                )
                                .hover(|s| s.bg(theme::DOCK()))
                                .child(if st.settings.billing_refreshing.as_deref() == Some(id) {
                                    "刷新中…"
                                } else {
                                    "立即刷新"
                                })
                                .on_click(move |_, _, cx| {
                                    let pid = billing_pid.clone();
                                    s_kind.update(cx, |st, cx| {
                                        st.refresh_billing_now(&pid, true, cx);
                                    });
                                }),
                        )
                    })
                }),
        )
        // 页脚:右对齐 取消/应用(源 .editorActions 胶囊钮)
        .child(
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
        )
}

/// 草稿模型行(id + 移除 x)
fn model_draft_row(store: &Entity<AppStore>, ix: usize, model: &str) -> impl IntoElement {
    let s = store.clone();
    div()
        .id(sid("model-draft", &ix.to_string()))
        .debug_selector(|| format!("model-draft-{ix}"))
        .flex()
        .items_center()
        .justify_between()
        .h(px(30.))
        .px(px(10.))
        .rounded(px(8.))
        .bg(theme::SIDEBAR())
        .text_size(px(13.))
        .text_color(theme::LABEL_2())
        .child(model.to_string())
        .child(
            div()
                .id(sid("model-draft-remove", &ix.to_string()))
                .flex()
                .size(px(20.))
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(theme::CAPTION())
                .hover(|s| s.bg(theme::DOCK()).text_color(theme::DANGER()))
                .child(fixed(IconName::Close, 12.))
                .on_click(move |_, _, cx| {
                    s.update(cx, |st, cx| st.remove_form_model(ix, cx));
                }),
        )
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
        .border_color(if active { theme::BRAND() } else { theme::BORDER() })
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(if active { theme::LABEL() } else { theme::LABEL_3() })
        .hover(|s| s.bg(theme::DOCK()))
        .child(label.to_string())
        .on_click(move |_, _, cx| {
            let k = kind.to_string();
            s.update(cx, |st, cx| st.set_billing_kind(&k, cx));
        })
}

/// 字段标签(源 .fieldLabel:12/500 secondary)
fn field_label(text: &str) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(theme::LABEL_2())
        .child(text.to_string())
}

/// 方言三选 chips(点击换选;标签 = 描述性协议名,值 = 方言串)
fn dialect_chips(store: &Entity<AppStore>, selected: &str) -> impl IntoElement {
    let choices = [
        ("openai-chat", "OpenAI Chat Completions"),
        ("anthropic", "Anthropic Messages (v1/messages)"),
        ("openai-responses", "OpenAI Responses"),
    ];
    let mut row = div().flex().flex_wrap().items_center().gap(px(4.));
    for (d, label) in choices {
        let s = store.clone();
        let active = d == selected;
        row = row.child(
            div()
                .id(sid("dialect-chip", d))
                .flex()
                .h(px(28.))
                .items_center()
                .px(px(8.))
                .rounded(px(14.))
                .cursor_pointer()
                .text_size(px(12.))
                .when(active, |el| {
                    el.bg(theme::DOCK())
                        .text_color(theme::LABEL())
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                })
                .when(!active, |el| {
                    el.text_color(theme::LABEL_3()).hover(|s| s.bg(theme::DOCK()))
                })
                .child(label)
                .on_click(move |_, _, cx| {
                    s.update(cx, |st, cx| {
                        st.settings.set_form_dialect = d.to_string();
                        cx.notify();
                    });
                }),
        );
    }
    row
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
        .child(intro_line(
            "dsh-desktop —— deepseek-harness 的 Rust 桌面重写。",
        ))
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
        // 源序:Agent 预设 / 权限 / 语言 / 外观 / 繁忙时 Enter 键行为
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

/// 选择器行(源 AgentPresetRow / PermissionRow / LanguageRow /
/// EnterBehaviorRow:左 title+desc,右 Select 下拉[gpui-component])
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

/// 外观组(源 AppearanceRow:标题 + cube 行;cube = 图标上文字下,
/// r16,选中 = 模块填充 + 描边)
fn appearance_group(store: &Entity<AppStore>, current: &str) -> impl IntoElement {
    let cubes: [(&str, &str, gpui_kit::component::Icon); 3] = [
        ("light", "浅色", fixed(IconName::Sun, 20.)),
        ("dark", "深色", fixed(IconName::Moon, 20.)),
        ("system", "跟随系统", fixed(DshIcon::Monitor, 20.)),
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
                .text_color(if active { theme::LABEL() } else { theme::LABEL_2() })
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

/// Provider 删除确认模态(shell/mod.rs 根级渲染;源 deleteDialog)
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
                        .border_color(if picked { theme::BRAND() } else { theme::BORDER() })
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
                .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
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
                .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(format!("移除 {id}")),
                )
                .child(caption_line(
                    "移除后工作区引用回落内置默认;凭据记录(钥匙串/.env)保留。",
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
    let basic: [(SettingsNav, &str, gpui_kit::component::Icon); 2] = [
        (SettingsNav::General, "常规", fixed(IconName::Settings, 15.)),
        (SettingsNav::Models, "模型设置", fixed(DshIcon::Gauge, 15.)),
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

/// 分组导航组头(源无分类,目标形态对齐参考图:小号说明字)
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
            el.hover(|s| s.bg(theme::LAYER())).text_color(theme::LABEL_3())
        })
        .text_size(px(13.))
        .text_color(if active { theme::LABEL() } else { theme::LABEL_3() })
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
