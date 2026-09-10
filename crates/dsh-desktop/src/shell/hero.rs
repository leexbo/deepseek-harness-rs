//! Hero 空态(web `Hero.tsx`):居中输入卡 + 工作区/模式 chip 行
//! (列宽由根布局按 metrics 策略给定)。空会话时由根布局挂载。
//! 两 chip 均为实装下拉:工作区 = 切换/添加(标题栏下拉同源行),
//! 模式 = preset 选择(describe presets)。

use gpui_kit::component::Icon;
use gpui_kit::component::IconName;
use gpui_kit::component::StyledExt;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::features::chat::composer;
use crate::kits::icons::{self, fixed};
use crate::kits::modals::{overlay_card, workspace_menu_rows};
use crate::kits::theme;
use crate::shell::store::{AppStore, HeroMenu};

/// Hero 整体(col_w:统一对话列宽,由根布局给定)
pub fn render(
    store: &Entity<AppStore>,
    col_w: gpui_kit::Pixels,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let st = store.read(cx);
    let ws = st
        .state
        .active_workspace
        .clone()
        .unwrap_or_else(|| st.default_workspace());
    let cfg = st.current_cfg_or_default();
    let preset_label = st.preset_label(&cfg.preset);
    let hero_menu = st.hero_menu;

    div()
        .relative()
        .flex()
        .min_h(px(0.))
        .flex_1()
        .flex_col()
        .items_center()
        .justify_center()
        // 列对齐容器同款槽 padding(左锚点槽/右滚动条槽)——hero 主卡
        // 与 composer/消息列同中心线
        .pl(px(
            crate::shell::metrics::H_PAD + crate::shell::metrics::NAV_GUTTER_W
        ))
        .pr(px(
            crate::shell::metrics::H_PAD + crate::shell::metrics::SCROLLBAR_GUTTER_W
        ))
        .pb(px(64.))
        .child(
            div()
                .v_flex()
                .relative()
                .w(col_w)
                .gap(px(12.))
                // 品牌鲸鱼 logo + 标语(logo.svg)
                .child(
                    div()
                        .v_flex()
                        .items_center()
                        .gap(px(10.))
                        .child(
                            div().flex().justify_center().child(
                                fixed(crate::kits::icons::DshIcon::Logo, 44.)
                                    .text_color(theme::LABEL()),
                            ),
                        )
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .text_color(theme::LABEL())
                                .child("探索未至之境"),
                        ),
                )
                // 首运行引导:凭据未配置时给出去设置的入口
                .when(st.settings.needs_onboarding, |el| {
                    let s = store.clone();
                    el.child(
                        div()
                            .id("hero-onboarding")
                            .flex()
                            .h(px(28.))
                            .items_center()
                            .gap(px(6.))
                            .px(px(10.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(theme::BORDER())
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(theme::WARN())
                            .hover(|s| s.bg(theme::LAYER()))
                            .child("尚未配置 API key——前往设置")
                            .on_click(move |_, _, cx| {
                                s.update(cx, |st, cx| st.toggle_settings(cx));
                            }),
                    )
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(hero_menu_slot(
                            hero_menu == HeroMenu::Workspace,
                            hero_chip(store, "hero-ws", &ws, fixed(IconName::FolderOpen, 14.)),
                        ))
                        .child(hero_menu_slot(
                            hero_menu == HeroMenu::Preset,
                            hero_chip(
                                store,
                                "hero-preset",
                                &preset_label,
                                fixed(icons::DshIcon::AgentPreset, 14.),
                            ),
                        )),
                )
                .child(composer::render(store, window, cx))
                // 菜单卡挂列尾(后于 composer,绘制在其上——挂 chip 行内
                // 会被 composer 盖住).以内容盒为锚,top 从内容盒顶到
                // chips 行底 + 6:logo(44)+gap(10)+标题(~21)+gap(12)
                // + chips(28)+6 ≈ 121(内容盒加 relative,不随居中漂移)
                .when(hero_menu != HeroMenu::None, |el| {
                    el.child(
                        div()
                            .absolute()
                            .top(px(121.))
                            .left_0()
                            .child(match hero_menu {
                                HeroMenu::Workspace => overlay_card(
                                    "hero-ws-card",
                                    320.,
                                    workspace_menu_rows(store, cx),
                                )
                                .into_any_element(),
                                HeroMenu::Preset => preset_card(store, cx).into_any_element(),
                                HeroMenu::None => div().into_any_element(),
                            }),
                    )
                }),
        )
}

/// hero chip 槽:开态豁免(mousedown stop_prop)+ 触发钮
fn hero_menu_slot(open: bool, trigger: gpui_kit::Stateful<gpui_kit::Div>) -> impl IntoElement {
    div()
        .when(open, |el| {
            el.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        })
        .child(trigger)
}

/// 模式(preset)卡:describe presets(id/name/description;当前勾选)
fn preset_card(store: &Entity<AppStore>, cx: &App) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (current, presets) = {
        let st = store.read(cx);
        (
            st.current_cfg_or_default().preset,
            st.state.host_info.presets.clone(),
        )
    };
    let mut rows: Vec<gpui_kit::AnyElement> = vec![];
    for (ix, p) in presets.iter().enumerate() {
        let Some(id) = p["id"].as_str() else { continue };
        let name = p["name"].as_str().unwrap_or(id);
        let desc = p["description"].as_str().unwrap_or_default();
        let s = store.clone();
        let v = id.to_string();
        let checked = id == current;
        let sel = format!("preset-item-{id}");
        rows.push(
            div()
                .id(("preset-item", ix))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(px(10.))
                .px(px(10.))
                .py(px(8.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::DOCK()))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .v_flex()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(13.))
                                .line_height(gpui_kit::relative(1.4))
                                .text_color(theme::LABEL())
                                .child(name.to_string()),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .line_height(gpui_kit::relative(1.3))
                                .text_color(theme::CAPTION())
                                .child(desc.to_string()),
                        ),
                )
                .when(checked, |el| {
                    el.child(fixed(IconName::Check, 14.).text_color(theme::LABEL()))
                })
                .debug_selector(move || sel.clone())
                .on_click(move |_, _, cx| {
                    let v = v.clone();
                    s.update(cx, |st, cx| st.set_session_preset(&v, cx));
                })
                .into_any_element(),
        );
    }
    overlay_card("hero-preset-card", 320., rows)
}

/// Hero 态 chip(工作区/模式触发钮;图标 + 文字,点击开下拉)
fn hero_chip(
    store: &Entity<AppStore>,
    id: &'static str,
    label: &str,
    icon: Icon,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let sel = id;
    let menu = if id == "hero-ws" {
        HeroMenu::Workspace
    } else {
        HeroMenu::Preset
    };
    let s = store.clone();
    let label = label.to_string();
    div()
        .id(id)
        .flex()
        .h(px(28.))
        .items_center()
        .gap(px(4.))
        .rounded(px(16.))
        .px(px(8.))
        .text_size(px(13.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(theme::LABEL())
        .cursor_pointer()
        // 素底 hover 才显灰(触发钮语言,同 composer chip)
        .hover(|s| s.bg(theme::DOCK()))
        .child(icon)
        .child(label)
        .child(fixed(IconName::ChevronDown, 12.).text_color(theme::CAPTION()))
        .debug_selector(move || sel.to_string())
        .on_click(move |_, _, cx| {
            let menu = menu;
            s.update(cx, |st, cx| st.set_hero_menu(menu, cx));
        })
}
