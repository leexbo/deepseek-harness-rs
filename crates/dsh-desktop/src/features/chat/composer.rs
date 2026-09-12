//! 输入卡(web `Composer.tsx`):统一列宽(由调用方列容器给定),
//! 圆角 22,bg-card。多行输入(Enter 发送/Shift+Enter 换行);running
//! 时发送钮变停止。底排:+ 命令菜单(模式选择)/ Plan chip(计划
//! 模式激活时)/ 权限下拉 / 模型·思考等级下拉。

use gpui_kit::component::Icon;
use gpui_kit::component::IconName;
use gpui_kit::component::StyledExt;
use gpui_kit::component::input::Textarea;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use super::store::ComposerMenu;
use crate::features::attachments;
use crate::kits::icons::{self, DshIcon, fixed};
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 输入卡整体
pub fn render(store: &Entity<AppStore>, window: &mut Window, cx: &mut App) -> impl IntoElement {
    let st = store.read(cx);
    let running = st
        .state
        .current_id
        .as_deref()
        .map(|id| st.is_running(id))
        .unwrap_or(false);
    let has_at = st.chat.at_completion.is_some();
    div()
        .w_full()
        .v_flex()
        .relative()
        .rounded(px(22.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::CARD())
        // 卡片浮起感:深盘底深主要靠色阶,阴影近乎不可见(无害);
        // 浅盘卡转白后靠阴影与白画布分层
        .shadow(vec![
            gpui_kit::BoxShadow::new(
                gpui_kit::px(0.),
                gpui_kit::px(2.),
                gpui_kit::rgba(0x00000014).into(),
            )
            .blur_radius(gpui_kit::px(10.)),
        ])
        .child(attachments::draft_rail(store, cx))
        // 输入卡总高/宽捕获(渲染期 paint;composer 下拉锚卡的定位分子
        // 与命令/技能菜单卡的「跟输入卡同宽」宽度来源,见
        // ChatState.composer_h / composer_w)。absolute 层不占 flex 位、
        // 无 hitbox 不挡交互;变化守卫在 note_composer_size(防每帧
        // notify 死循环)
        .child({
            let cap = store.clone();
            div()
                .absolute()
                .inset_0()
                // canvas 默认 0 高,要尺寸须显式铺满容器
                .child(
                    gpui_kit::canvas(
                        move |b, _, cx| {
                            let size = b.size;
                            cap.update(cx, |st, cx| {
                                st.note_composer_size(size.width.as_f32(), size.height.as_f32(), cx)
                            });
                        },
                        |_, _, _, _| {},
                    )
                    .size_full(),
                )
        })
        // 命令行(命令菜单点选带参命令;/命令品牌色与输入文字区分,
        // × 移除;发送时拼接 /name args)
        .children(
            st.chat
                .pending_command
                .as_ref()
                .map(|c| command_line(store, &c.name, cx)),
        )
        .children(st.chat.composer_input.as_ref().map(|e| {
            let input = div()
                .id("composer-scroll")
                // 行数封顶:8 行 × 21px(14px 字号 1.5 行距)+ 上下内边距;
                // AutoGrow 的 flex_grow 在无约束容器会持续撑高,这里兜底
                .max_h(px(184.))
                .overflow_y_scroll()
                .px(px(16.))
                .pt(px(12.))
                .pb(px(4.))
                // textarea 形态由卡片承担:Input 自身去边框去背景
                .child(
                    Textarea::new(e)
                        .appearance(false)
                        .text_size(px(14.))
                        .line_height(gpui_kit::relative(1.5)),
                );
            // 测试钩子:输入区 bounds 可经 debug_bounds 检索(点击聚焦路径)
            #[cfg(test)]
            let input = div()
                .debug_selector(|| "composer-hit".to_string())
                .child(input);
            input
        }))
        .child(bottom_row(store, running, window, cx))
        // @ 引用补全菜单(锚在输入卡上缘;后绘制在上层)
        .children(has_at.then(|| at_completion_anchor(store, cx)))
        // @ 补全键盘导航:capture 阶段拦截 ↑↓/Enter/Esc(有补全时),
        // stop_propagation 阻止 Input 光标移动/发送
        .capture_key_down({
            let s = store.clone();
            move |ev: &gpui_kit::KeyDownEvent, _window, cx| {
                if s.read(cx).chat.at_completion.is_none() {
                    return;
                }
                let key = ev.keystroke.key.as_str();
                match key {
                    "arrowup" => {
                        cx.stop_propagation();
                        s.update(cx, |st, cx| st.navigate_at_completion(-1, cx));
                    }
                    "arrowdown" => {
                        cx.stop_propagation();
                        s.update(cx, |st, cx| st.navigate_at_completion(1, cx));
                    }
                    "escape" => {
                        cx.stop_propagation();
                        s.update(cx, |st, cx| st.cancel_at_completion(cx));
                    }
                    "enter" => {
                        cx.stop_propagation();
                        s.update(cx, |st, cx| {
                            st.chat.enter_at_completion = true;
                            cx.notify();
                        });
                    }
                    _ => {}
                }
            }
        })
        .debug_selector(|| "composer-card".to_string())
}

/// 命令行(输入卡内、输入框上缘):`/name` 品牌色 + 参数 hint 灰字 +
/// × 移除钮。命令与输入文字的区分载体——命令是结构化前缀不是正文,
/// 发送时与输入框文本拼接(/name args)走既有文本路径
fn command_line(store: &Entity<AppStore>, name: &str, _cx: &App) -> gpui_kit::AnyElement {
    let s = store.clone();
    let mut row = div()
        .flex()
        .min_w(px(0.))
        .items_center()
        .gap(px(6.))
        .px(px(16.))
        .pt(px(10.))
        .child(
            div()
                .flex()
                .items_center()
                .h(px(24.))
                .px(px(8.))
                .rounded(px(6.))
                .bg(theme::ONGOING().opacity(0.14))
                .text_size(px(13.))
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .text_color(theme::BRAND())
                .child(format!("/{name}")),
        );
    row = row.child(
        div()
            .id("composer-command-clear")
            .debug_selector(|| "composer-command-clear".to_string())
            .flex()
            .items_center()
            .justify_center()
            .size(px(20.))
            .rounded(px(10.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::LAYER()))
            .text_color(theme::CAPTION())
            .child(fixed(IconName::Close, 12.))
            .on_click(move |_, _, cx| {
                s.update(cx, |st, cx| st.clear_pending_command(cx));
            }),
    );
    div()
        .id("composer-command-line")
        .debug_selector(|| "composer-command-line".to_string())
        .flex()
        .min_w(px(0.))
        .items_center()
        .child(row)
        .into_any_element()
}

/// @ 补全菜单锚点(输入卡上缘 absolute;供键盘/点击选中)
fn at_completion_anchor(store: &Entity<AppStore>, cx: &App) -> gpui_kit::AnyElement {
    let st = store.read(cx);
    let Some(at) = st.chat.at_completion.clone() else {
        return div().into_any_element();
    };
    div()
        .id("at-completion-anchor")
        .debug_selector(|| "at-completion-anchor".to_string())
        .absolute()
        .left(px(16.))
        .right(px(16.))
        // 底缘 = 输入卡顶上方 2px(锚卡挂在卡根,bottom=卡高+2;固定值
        // 会随卡高变化叠进输入框/悬空过高)
        .bottom(px(composer_h(&st.chat) + 2.))
        .child(at_completion_card(store, at))
        .into_any_element()
}

/// 锚卡 bottom 值(卡根参照系):输入卡总高 + 2px 间隙(渲染期捕获
/// 的上一帧卡高;首帧 0 时不弹正位,下一帧校准——与 track_h 同模式)
fn composer_h(chat: &crate::features::chat::ChatStore) -> f32 {
    chat.composer_h + 2.
}

/// 底排(左:+ 指令菜单 / Plan chip / 权限;右:模型·等级 / 上下文
/// 圆环 / 发送——对齐 web Composer 布局)
fn bottom_row(
    store: &Entity<AppStore>,
    running: bool,
    _window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let st = store.read(cx);
    let cfg = st.current_cfg_or_default();
    let plan_mode = st.current_chat().map(|c| c.plan_mode).unwrap_or(false);
    let menu = st.chat.composer_menu;
    let occupancy = st.context_occupancy();
    let cmds = st.bridge.host().command_list();
    // 卡顶锚的 bottom(须在可变借用前读;见 menu_slot 注释)
    let anchor_bottom = st.chat.composer_h - 7.;

    let cmd_trigger = round_button("composer-cmd", fixed(IconName::Plus, 14.)).on_click({
        let s = store.clone();
        move |_, _, cx| {
            s.update(cx, |st, cx| {
                st.set_composer_menu(ComposerMenu::Commands, cx)
            });
        }
    });
    let perm_trigger = chip(
        "chip-perm",
        permission_label(&cfg.permission),
        icons::permission_icon(&cfg.permission),
    )
    .on_click({
        let s = store.clone();
        move |_, _, cx| {
            s.update(cx, |st, cx| {
                st.set_composer_menu(ComposerMenu::Permission, cx)
            });
        }
    });
    let model_label = format!(
        "{} · {}",
        cfg.model,
        effort_label(cfg.effort.as_deref().unwrap_or("high"))
    );
    let model_trigger = chip("chip-model", &model_label, icons::model_icon(&cfg.model)).on_click({
        let s = store.clone();
        move |_, _, cx| {
            s.update(cx, |st, cx| st.set_composer_menu(ComposerMenu::Model, cx));
        }
    });

    div()
        .flex()
        .h(px(42.))
        .items_center()
        .gap(px(8.))
        .px(px(10.))
        .pb(px(8.))
        .child(menu_slot(
            cmd_trigger,
            (menu == ComposerMenu::Commands)
                .then(|| {
                    commands_card(
                        store,
                        cmds.clone(),
                        &st.chat.skill_entries,
                        st.chat.composer_w,
                    )
                }),
            anchor_bottom,
        ))
        // 「+」与模式 chips 之间的细竖线分组
        .child(
            div()
                .w(px(1.))
                .h(px(16.))
                .flex_shrink_0()
                .bg(theme::BORDER()),
        )
        // 权限 chip:卡片**根级渲染**(shell/mod.rs,同 +/行/工作区菜单;
        // 内联浮层叠进输入卡子树会透视,根级无此问题)。
        // 这里只放 chip + 渲染期 bounds 捕获(根级锚定的定位分子)
        .child(div().relative().flex_shrink_0().child(perm_trigger).child(
            div().absolute().inset_0().child({
                let cap = store.clone();
                gpui_kit::canvas(
                    move |b, _, cx| {
                        cap.update(cx, |st, _| st.chat.perm_chip_bounds = Some(b));
                    },
                    |_, _, _, _| {},
                )
                .size_full()
            }),
        ))
        // 计划模式 chip(仅激活态渲染,退出即整个消失;进入唯一入口=
        // 命令菜单「plan」行)
        .children(plan_mode.then(|| plan_chip(store, st.chat.plan_chip_hovered)))
        .child(div().flex_1())
        .child(menu_slot(
            model_trigger,
            (menu == ComposerMenu::Model).then(|| model_card(store, cx)),
            anchor_bottom,
        ))
        .children(occupancy.map(|o| {
            menu_slot(
                context_button(store, o),
                (menu == ComposerMenu::Context).then(|| context_card(store, cx)),
                anchor_bottom,
            )
        }))
        .child(send_or_stop(store, running))
}

/// 下拉槽:relative 锚 + 开态豁免 + 开态锚卡(向上弹,仅开时渲染;
/// 底缘 = 输入卡顶上方 2px,卡高随命令行/输入行数变化自适应)。
/// 权限下拉不走此槽(根级渲染,见 bottom_row 注释)。occlude 阻断
/// 命中向卡后方穿透。豁免 = wrapper 与锚卡都挂 mousedown
/// stop_propagation:根级外点关闭按 hitbox 树派发,锚卡几何上超出
/// wrapper 矩形,豁免必须各自持有——漏挂锚卡则点菜单行先触发关闭
/// 重绘,行元素消失,on_click(按下+抬起成对)永不完成
fn menu_slot(
    trigger: gpui_kit::Stateful<gpui_kit::Div>,
    card: Option<gpui_kit::AnyElement>,
    anchor_bottom: f32,
) -> impl IntoElement {
    div()
        .relative()
        .flex_shrink_0()
        .when(card.is_some(), |el| {
            el.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        })
        .child(trigger)
        .when_some(card, |el, card| {
            el.child(
                div()
                    .id("composer-menu-anchor")
                    .absolute()
                    .left_0()
                    .bottom(px(anchor_bottom))
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(card),
            )
        })
}

/// @ 引用补全菜单:文件/Session 分组候选,高亮 +
/// 点击选中插入 mention(替换 @ token)。
fn at_completion_card(
    store: &Entity<AppStore>,
    at: super::store::AtCompletion,
) -> gpui_kit::AnyElement {
    let mut rows: Vec<gpui_kit::AnyElement> = vec![];
    if !at.files.is_empty() {
        rows.push(section_label("文件与文件夹").into_any_element());
        for (i, f) in at.files.iter().enumerate() {
            let idx = i; // 高亮索引:文件占 0..files.len
            let s = store.clone();
            let label = f.path.clone();
            let path_c = f.path.clone();
            let is_dir = f.is_dir;
            rows.push(
                at_row(
                    &label,
                    idx,
                    idx == at.highlight,
                    fixed(
                        if is_dir {
                            IconName::Folder
                        } else {
                            IconName::File
                        },
                        14.,
                    ),
                    move |window, cx| {
                        let replacement = super::reference::file_mention(&path_c);
                        s.update(cx, |st, cx| {
                            st.select_at_completion(&replacement, window, cx);
                        });
                        let _ = cx;
                    },
                )
                .into_any_element(),
            );
        }
    }
    if !at.sessions.is_empty() {
        if !rows.is_empty() {
            rows.push(menu_separator().into_any_element());
        }
        rows.push(section_label("Session 对话").into_any_element());
        let file_count = at.files.len();
        for (i, s) in at.sessions.iter().enumerate() {
            let idx = file_count + i;
            let st2 = store.clone();
            let label = s.label.clone();
            let sid = s.session_id.clone();
            let label_c = label.clone();
            let sid_c = sid.clone();
            rows.push(
                at_row(
                    &label,
                    idx,
                    idx == at.highlight,
                    fixed(DshIcon::MessageSquare, 14.),
                    move |window, cx| {
                        let mention = super::reference::session_mention(&label_c, &sid_c);
                        st2.update(cx, |st, cx| {
                            st.select_at_completion(&mention, window, cx);
                        });
                        let _ = cx;
                    },
                )
                .into_any_element(),
            );
        }
    }
    menu_card(rows, None)
}

/// @ 补全单行(图标 + label;默认素底,高亮/hover 灰底)
fn at_row(
    label: &str,
    _idx: usize,
    highlighted: bool,
    icon: Icon,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let sel = format!("at-row-{label}");
    div()
        .id(gpui_kit::SharedString::from(sel.clone()))
        .flex()
        .h(px(30.))
        .flex_shrink_0()
        .items_center()
        .gap(px(8.))
        .rounded(px(6.))
        .px(px(8.))
        .cursor_pointer()
        .when(highlighted, |el| el.bg(theme::DOCK()))
        .hover(|s| s.bg(theme::DOCK()))
        .text_size(px(13.))
        .text_color(theme::LABEL_2())
        .child(icon)
        // 单行截断:长会话标题/深路径换行会溢出定高行框叠绘到后续行
        // (实测报障);min_w(0) 压住 taffy 文本 min-content 撑行。
        // -text 钩子供测试断言「单行」(换行时该层高度 > 行高)
        .child({
            let text_sel = format!("{sel}-text");
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .debug_selector(move || text_sel.clone())
                .child(label.to_string())
        })
        // 测试钩子:行 bounds 按文案检索
        .debug_selector(move || sel.clone())
        .on_click(move |_, window, cx| on_click(window, cx))
}

/// 计划模式 chip(仅激活态渲染,退出即整个消失;进入唯一入口=命令菜单
/// 「plan」行即点即执行)。形态定稿:图标 + 「计划」,
/// 默认 ListChecks、hover 换 ⓧ 取消态(ⓧ 不常显);点击恒发 standard
/// (绝对方向,见 store::apply_plan_mode),连发幂等收敛
fn plan_chip(store: &Entity<AppStore>, hovered: bool) -> impl IntoElement {
    let s = store.clone();
    let h = store.clone();
    div()
        .id("chip-plan")
        .flex()
        .h(px(24.))
        .flex_shrink_0()
        .items_center()
        .gap(px(4.))
        .rounded(px(12.))
        .px(px(8.))
        .text_size(px(12.))
        .text_color(theme::WARN())
        .bg(theme::LAYER())
        .cursor_pointer()
        .hover(|s| s.bg(theme::DOCK()))
        .on_hover(move |hovered: &bool, _, cx| {
            h.update(cx, |st, cx| {
                st.chat.plan_chip_hovered = *hovered;
                cx.notify();
            });
        })
        .child(if hovered {
            fixed(IconName::CircleX, 14.)
        } else {
            fixed(DshIcon::ListChecks, 14.)
        })
        .child("计划")
        // 测试钩子(release 空操作)
        .debug_selector(|| "chip-plan".to_string())
        .on_click(move |_, _, cx| {
            s.update(cx, |st, cx| st.exit_plan_mode(cx));
        })
}

/// 命令菜单(「+」触发):斜杠指令列表(从 host 注册表拉取,动态化),
/// 选中即 host 执行(非发模型)。首节「添加」
/// = 图片附件入口(附件不占弹窗旁的独立钮),命令节空则略;
/// 「技能」节 = session_skills(user-invocable,菜单打开时拉取),
/// 点击落草稿 chip(发送拼 /name args,host 手势注入接管)。
fn commands_card(
    store: &Entity<AppStore>,
    cmds: Vec<dsh_core::registry::CommandDescriptor>,
    skills: &[super::store::SkillEntry],
    composer_w: f32,
) -> gpui_kit::AnyElement {
    let mut rows: Vec<gpui_kit::AnyElement> = vec![section_label("添加").into_any_element()];
    rows.push(
        menu_row(
            "attach-item",
            fixed(DshIcon::Paperclip, 14.),
            "图片附件",
            false,
            {
                let s = store.clone();
                move |_, window, cx| {
                    // 先收菜单再开文件对话框(菜单不复选)
                    s.update(cx, |st, cx| st.close_all_menus(cx));
                    let rx = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
                        files: true,
                        directories: false,
                        multiple: true,
                        prompt: Some("选择图片".into()),
                    });
                    // Fn 闭包不能 move 出捕获:异步块内用克隆体
                    let s2 = s.clone();
                    window
                        .spawn(cx, async move |cx| {
                            let paths = rx.await.ok().and_then(|r| r.ok()).flatten();
                            if let Some(paths) = paths {
                                cx.update(move |_, app| {
                                    s2.update(app, |st, _| st.intake_dropped_paths(&paths));
                                })
                                .ok();
                            }
                        })
                        .detach();
                }
            },
        )
        .into_any_element(),
    );
    if !cmds.is_empty() {
        rows.push(section_label("命令").into_any_element());
        for cmd in cmds {
            let s = store.clone();
            let name = cmd.name.to_string();
            let desc = cmd.description.to_string();
            let name_static: &'static str = Box::leak(name.clone().into_boxed_str());
            // 命令行呈现(命令与输入文字区分;输入框写任务描述,发送时
            // 拼接 /name + 文本);无参命令 = 保持既有立即执行
            let has_hint = cmd.hint.is_some();
            rows.push(
                command_row(name_static, desc, move |cx| {
                    let s = s.clone();
                    s.update(cx, move |st, cx| {
                        if has_hint {
                            st.set_pending_command(name_static, cx);
                        } else {
                            st.execute_command(name_static, cx);
                        }
                        st.close_all_menus(cx);
                    });
                })
                .into_any_element(),
            )
        }
    }
    if !skills.is_empty() {
        rows.push(section_label("技能").into_any_element());
        for sk in skills {
            let s = store.clone();
            let name = sk.name.clone();
            let desc = if sk.model_invocable {
                sk.description.clone()
            } else {
                format!("仅用户 · {}", sk.description)
            };
            let chip_name: &'static str = Box::leak(name.clone().into_boxed_str());
            rows.push(
                command_row(chip_name, desc, move |cx| {
                    let s = s.clone();
                    s.update(cx, move |st, cx| {
                        // 技能恒走草稿 chip(参数在输入框;不立即执行)——
                        // 发送拼 /name args,host 侧手势识别接管
                        st.set_pending_command(chip_name, cx);
                        st.close_all_menus(cx);
                    });
                })
                .into_any_element(),
            );
        }
    }
    // 跟输入卡同宽(拍板):无宽度约束时 taffy 文本按 max-content
    // 定宽,长描述把卡撑到超窗、行内 truncate 永不生效。触发钮距卡左
    // 缘 10px(bottom_row 内边距),减 10 对齐卡右缘;首帧捕获 0 →
    // 不约束,下一帧校准(同 composer_h 锚定模式)
    menu_card(rows, (composer_w > 0.).then(|| composer_w - 10.))
}

/// 指令行(源 CommandMenu 行:命令名黑 semibold + 描述灰同行;
/// 大行高、hover 灰底圆角,非勾选语义)
fn command_row(
    cmd: &'static str,
    desc: String,
    on_click: impl Fn(&mut App) + 'static,
) -> impl IntoElement {
    let sel = cmd.to_string();
    div()
        .id(cmd)
        .flex()
        .h(px(40.))
        .flex_shrink_0()
        .items_center()
        .gap(px(12.))
        .rounded(px(8.))
        .px(px(12.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::DOCK()))
        .child(
            div()
                .text_size(px(14.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(theme::LABEL())
                .child(cmd.to_string()),
        )
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .truncate()
                .text_size(px(13.))
                .text_color(theme::LABEL_2())
                .child(desc),
        )
        // 测试钩子:行 bounds 按命令文本检索(release 空操作)
        .debug_selector(move || sel.clone())
        .on_click(move |_, _, cx| on_click(cx))
}

/// 上下文占用圆环钮(环 + 百分比;触发详情卡)
fn context_button(
    store: &Entity<AppStore>,
    o: super::store::ContextOccupancy,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let s = store.clone();
    let percent = (o.percent * 100.0).round() as u64;
    div()
        .id("context-ring")
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(4.))
        .h(px(24.))
        .px(px(6.))
        .cursor_pointer()
        .rounded_full()
        .hover(|s| s.bg(theme::DOCK()))
        .child(super::context_meter::ring(o.percent, 14.))
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme::LABEL_3())
                .child(format!("{percent}%")),
        )
        .debug_selector(|| "context-ring".to_string())
        .on_click(move |_, _, cx| {
            s.update(cx, |st, cx| st.set_composer_menu(ComposerMenu::Context, cx));
        })
}

/// 上下文占用详情卡:占比 + 构成三段条(系统提示/工具定义/会话消息)
fn context_card(store: &Entity<AppStore>, cx: &App) -> gpui_kit::AnyElement {
    use super::context_meter::{breakdown_rows, fmt_tok, ring};
    let Some(o) = store.read(cx).context_occupancy() else {
        return div().into_any_element();
    };
    let percent = (o.percent * 100.0).round() as u64;
    let total = o.system + o.tools + o.messages;
    let mut children: Vec<gpui_kit::AnyElement> = vec![
        div()
            .id("context-ring-open")
            // 测试钩子(release 空操作)
            .debug_selector(|| "context-ring-open".to_string())
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .pt(px(6.))
            .child(ring(o.percent, 22.))
            .child(
                div()
                    .v_flex()
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme::LABEL())
                            .child(format!("上下文已用 {percent}%")),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::CAPTION())
                            .child(format!("{} / {} tok", fmt_tok(o.used), fmt_tok(o.window))),
                    ),
            )
            .into_any_element(),
    ];
    // 构成分段条(段宽 = 总占比 × 段内占比,像素定宽免 flex 摊派)
    let bar_w: f64 = 224.0;
    children.push(
        div()
            .id("context-bar")
            .flex()
            .h(px(6.))
            .mx(px(8.))
            .my(px(6.))
            .w(px(bar_w as f32))
            .rounded(px(3.))
            .overflow_hidden()
            .bg(theme::BORDER())
            .children(breakdown_rows(&o).iter().filter_map(|(_, color, v)| {
                let frac = if total > 0 {
                    o.percent * (*v as f64 / total as f64)
                } else {
                    0.0
                };
                (frac > 0.0).then(|| {
                    div()
                        .h_full()
                        .flex_shrink_0()
                        .w(px((frac * bar_w) as f32))
                        .bg(*color)
                })
            }))
            .into_any_element(),
    );
    for (label, color, v) in breakdown_rows(&o) {
        let share = if total > 0 {
            v as f64 / total as f64
        } else {
            0.0
        };
        children.push(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .pb(px(2.))
                .text_size(px(12.))
                .text_color(theme::LABEL_2())
                .child(div().size(px(8.)).rounded_full().bg(color))
                .child(label)
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child(format!("{} · {:.0}%", fmt_tok(v), share * 100.0)),
                )
                .into_any_element(),
        );
    }
    menu_card(children, None)
}

/// 权限下拉卡(选项来自 describe permissions;当前值勾选)。
/// **根级渲染**(shell/mod.rs):内联浮层叠进输入卡子树会透视,
/// 照 +/行/工作区菜单的根级模式挂出
pub(crate) fn permission_card(store: &Entity<AppStore>, cx: &App) -> gpui_kit::AnyElement {
    let st = store.read(cx);
    let current = st.current_cfg_or_default().permission;
    let permissions = st.state.host_info.permissions.clone();
    let rows = permissions
        .iter()
        .enumerate()
        .map(|(ix, p)| {
            let s = store.clone();
            let v = p.clone();
            let checked = *p == current;
            menu_row(
                ("perm-item", ix),
                icons::permission_icon(p),
                permission_label(p),
                checked,
                move |_, _, cx| {
                    let v = v.clone();
                    s.update(cx, |st, cx| {
                        if v == "full-access" {
                            // 风险确认弹窗,确认后才真正切换
                            st.ask_full_access_session(cx);
                        } else {
                            st.set_session_permission(&v, cx);
                        }
                    });
                },
            )
            .into_any_element()
        })
        .collect();
    menu_card(rows, None)
}

/// 模型 + 推理等级下拉(模型表空则略模型区;等级 low/high/max)
fn model_card(store: &Entity<AppStore>, cx: &App) -> gpui_kit::AnyElement {
    let st = store.read(cx);
    let cfg = st.current_cfg_or_default();
    let efforts = st.state.host_info.efforts.clone();
    let current_model = cfg.model.clone();
    let current_effort = cfg.effort.clone().unwrap_or_else(|| "high".into());
    // provider 分组(设置快照注册表;清单 = 用户圈定优先、探测缓存回退)
    let default_pid = st.settings.settings_snapshot["defaultProvider"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut groups: Vec<(String, String, Vec<String>, bool)> = Vec::new();
    for p in st.settings.settings_snapshot["providers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let pid = p["id"].as_str().unwrap_or_default().to_string();
        if pid.is_empty() {
            continue;
        }
        let name = p["display_name"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or(&pid)
            .to_string();
        let models = st.bridge.host().models_for(&pid);
        groups.push((pid.clone(), name, models, pid == default_pid));
    }
    // 当前 provider 名(模型入口行右侧展示):含当前模型的组,回退默认组
    let current_provider = groups
        .iter()
        .find(|(_, _, ms, _)| ms.contains(&current_model))
        .or_else(|| groups.iter().find(|(_, _, _, d)| *d))
        .map(|(_, n, ..)| n.clone())
        .unwrap_or_default();
    let s_model_row = store.clone();
    // ── 一级卡:模型入口行 + 推理强度平铺(不改)──
    let mut rows: Vec<gpui_kit::AnyElement> = vec![];
    rows.push(section_label("模型").into_any_element());
    rows.push(
        div()
            .id("model-entry")
            .debug_selector(|| "model-entry".to_string())
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(30.))
            .px(px(12.))
            .rounded(px(8.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::DOCK()))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme::LABEL())
                    .child("模型"),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::CAPTION())
                    .truncate()
                    .max_w(px(120.))
                    .child(current_model.clone()),
            )
            .when(!current_provider.is_empty(), |el| {
                el.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::CAPTION())
                        .truncate()
                        .max_w(px(80.))
                        .child(current_provider.clone()),
                )
            })
            .child(fixed(IconName::ChevronRight, 12.).text_color(theme::CAPTION()))
            .on_click(move |_, _, cx| {
                s_model_row.update(cx, |st, cx| {
                    let next = match st.chat.composer_submenu {
                        Some(crate::features::chat::ComposerSubmenu::Models) => None,
                        _ => Some(crate::features::chat::ComposerSubmenu::Models),
                    };
                    st.set_composer_submenu(next, cx);
                });
            })
            .into_any_element(),
    );
    rows.push(menu_separator().into_any_element());
    rows.push(section_label("推理等级").into_any_element());
    for (ix, e) in efforts.iter().enumerate() {
        let s = store.clone();
        let v = e.clone();
        let checked = *e == current_effort;
        rows.push(
            menu_row(
                ("effort-item", ix),
                fixed(DshIcon::Brain, 14.),
                &effort_label(e),
                checked,
                move |_, _, cx| {
                    let v = v.clone();
                    s.update(cx, |st, cx| st.set_session_effort(&v, cx));
                },
            )
            .into_any_element(),
        );
    }
    // ── 级联子卡(模型选择;挂主卡左侧,右缘窗口放不下右侧)──
    let sub = st.chat.composer_submenu;
    let main = menu_card(rows, None);
    let mut wrap = div().relative().child(main);
    if sub == Some(crate::features::chat::ComposerSubmenu::Models) {
        let mut sub_rows: Vec<gpui_kit::AnyElement> = vec![];
        sub_rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .py(px(4.))
                .text_size(px(11.))
                .text_color(theme::CAPTION())
                .child("模型")
                .into_any_element(),
        );
        for (pid, name, models, _d) in &groups {
            if models.is_empty() {
                continue;
            }
            sub_rows.push(
                div()
                    .px(px(8.))
                    .py(px(3.))
                    .text_size(px(11.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(theme::LABEL_3())
                    .child(name.clone())
                    .into_any_element(),
            );
            for (ix, m) in models.iter().enumerate() {
                let s = store.clone();
                let v = m.clone();
                let gpid = pid.clone();
                sub_rows.push(
                    menu_row(
                        gpui_kit::ElementId::Name(gpui_kit::SharedString::from(format!(
                            "submenu-model-{pid}-{ix}"
                        ))),
                        icons::model_icon(m),
                        m,
                        *m == current_model,
                        move |_, _, cx| {
                            let v = v.clone();
                            let gpid = gpid.clone();
                            s.update(cx, |st, cx| {
                                st.set_session_provider_model(&gpid, &v, cx);
                                st.close_composer_menu(cx);
                            });
                        },
                    )
                    .into_any_element(),
                );
            }
        }
        if sub_rows.len() == 1 {
            sub_rows.push(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .text_size(px(12.))
                    .text_color(theme::CAPTION())
                    .child("各 Provider 尚未配置模型。")
                    .into_any_element(),
            );
        }
        wrap = wrap.child(
            div()
                .id("model-submenu")
                .debug_selector(|| "model-submenu".to_string())
                .absolute()
                .right(px(228.))
                .bottom(px(0.))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .id("model-submenu-list")
                        .min_w(px(200.))
                        .max_h(px(300.))
                        .overflow_y_scroll()
                        .rounded(px(12.))
                        .border_1()
                        .border_color(theme::BORDER())
                        .bg(if theme::is_dark() {
                            theme::LAYER()
                        } else {
                            theme::CARD()
                        })
                        .shadow_md()
                        .p(px(4.))
                        .children(sub_rows),
                ),
        );
    }
    wrap.into_any_element()
}

/// 下拉卡容器(纵向堆叠 + 高度封顶滚动;block 形态,不加 .flex——
/// flex 容器 + overflow scroll 在 taffy 中 min-content 泄内容高)。
/// 浮层面浅色取白(白底;灰 LAYER 是画布 hover 语言)、
/// 深色取 LAYER(浮层比画布亮一阶)
/// 菜单卡(分区行列表浮层)。max_w 传入时宽度收敛——taffy 无约束
/// 文本按 max-content 定宽,长内容行必须外部给约束,行内 truncate
/// 才会生效(at 补全由锚定层 left/right 定宽,传 None 即可)
fn menu_card(
    children: Vec<gpui_kit::AnyElement>,
    max_w: Option<f32>,
) -> gpui_kit::AnyElement {
    div()
        .id("composer-menu-card")
        .debug_selector(|| "composer-menu-card".to_string())
        .min_w(px(220.))
        .when_some(max_w, |el, w| el.max_w(px(w)))
        .max_h(px(300.))
        .overflow_y_scroll()
        .rounded(px(12.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(if theme::is_dark() {
            theme::LAYER()
        } else {
            theme::CARD()
        })
        .shadow_md()
        .p(px(4.))
        .children(children)
        .into_any_element()
}

/// 菜单行(图标 + 文案 + 弹性占位 + 勾选;字一级色、
/// 勾前景色非品牌蓝、大行高)
fn menu_row(
    id: impl Into<gpui_kit::ElementId>,
    icon: Icon,
    label: &str,
    checked: bool,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut App) + 'static,
) -> impl IntoElement {
    let sel = label.to_string();
    div()
        .id(id)
        .flex()
        .h(px(36.))
        .flex_shrink_0()
        .items_center()
        .gap(px(10.))
        .rounded(px(8.))
        .px(px(12.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::DOCK()))
        .text_size(px(14.))
        .text_color(theme::LABEL())
        .child(icon)
        .child(label.to_string())
        .child(div().flex_1())
        .when(checked, |el| {
            el.child(fixed(IconName::Check, 14.).text_color(theme::LABEL()))
        })
        // 测试钩子:行 bounds 按文案检索(release 恒等空操作)
        .debug_selector(move || sel.clone())
        .on_click(move |ev, window, cx| on_click(ev, window, cx))
}

/// 分区小标题(左缩进与菜单行 px12 对齐)
fn section_label(text: &str) -> impl IntoElement {
    div()
        .px(px(12.))
        .py(px(4.))
        .text_size(px(11.))
        .text_color(theme::CAPTION())
        .child(text.to_string())
}

/// 分区分隔线
fn menu_separator() -> impl IntoElement {
    div().h(px(1.)).my(px(2.)).bg(theme::BORDER())
}

/// 发送/停止圆钮(running → 停止方块)
fn send_or_stop(store: &Entity<AppStore>, running: bool) -> impl IntoElement {
    let s = store.clone();
    // 停止态用自绘小方块(确定性优于字形;发送态箭头图标随文字色)
    let (id, bg, child) = if running {
        (
            "stop",
            theme::DANGER(),
            div()
                .size(px(10.))
                .rounded(px(2.))
                .bg(gpui_kit::rgba(0xFFFFFFFF))
                .into_any_element(),
        )
    } else {
        (
            "send",
            theme::BRAND(),
            fixed(IconName::ArrowUp, 16.).into_any_element(),
        )
    };
    div()
        .id(id)
        .flex()
        .size(px(34.))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(bg)
        .cursor_pointer()
        .hover(|s| s.opacity(0.85))
        // 填充面(品牌蓝/危险红)上的符号双盘恒白——浅色 LABEL 是近黑,
        // 蓝底黑箭头对比脏
        .text_color(gpui_kit::rgba(0xFFFFFFFF))
        .child(child)
        .on_click(move |_, _, cx| {
            s.update(cx, |st, cx| {
                if running {
                    st.cancel_current(cx);
                } else {
                    // 与 Enter 订阅同一条路径:读值发送 + 延迟清空
                    let text = st
                        .chat
                        .composer_input
                        .as_ref()
                        .map(|e| e.read(cx).value().trim().to_string())
                        .unwrap_or_default();
                    if !text.is_empty() {
                        st.send(&text, cx);
                        st.chat.pending_composer_clear = true;
                    }
                }
            });
        })
}

/// 34px 圆钮(+ 命令菜单触发)。素底,hover 才显灰底(按钮
/// 语言:常驻底色=选中态,触发钮默认透明)
fn round_button(id: &'static str, icon: Icon) -> gpui_kit::Stateful<gpui_kit::Div> {
    let sel = id;
    div()
        .id(id)
        .flex()
        .size(px(34.))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_color(theme::LABEL_2())
        .cursor_pointer()
        .hover(|s| s.bg(theme::DOCK()).text_color(theme::LABEL()))
        .child(icon)
        // 测试钩子:触发钮 bounds 按 id 检索(release 空操作)
        .debug_selector(move || sel.to_string())
}

/// 药丸 chip(权限/模型触发;图标经 icons 映射)。素底,hover 才显
/// 灰底(常驻底=灰蒙蒙)
fn chip(id: &'static str, label: &str, icon: Icon) -> gpui_kit::Stateful<gpui_kit::Div> {
    let sel = id;
    div()
        .id(id)
        .flex()
        .h(px(24.))
        .flex_shrink_0()
        .items_center()
        .gap(px(4.))
        .rounded(px(12.))
        .px(px(10.))
        .text_size(px(12.))
        .text_color(theme::LABEL_2())
        .cursor_pointer()
        .hover(|s| s.bg(theme::DOCK()).text_color(theme::LABEL()))
        .child(icon)
        .child(label.to_string())
        // 测试钩子:触发钮 bounds 按 id 检索(release 空操作)
        .debug_selector(move || sel.to_string())
}

/// 权限中文文案(statusbar
/// 徽标共用):仅可查看 / 工作区内修改 / 完全权限
pub(crate) fn permission_label(mode: &str) -> &'static str {
    match mode {
        "read-only" => "仅可查看",
        "full-access" => "完全权限",
        _ => "工作区内修改",
    }
}

/// 思考等级展示文案(low/high/max → Low/High/Max;statusbar 共用)
pub(crate) fn effort_label(effort: &str) -> String {
    match effort {
        "low" => "Low".into(),
        "high" => "High".into(),
        "max" => "Max".into(),
        other => other.to_string(),
    }
}
