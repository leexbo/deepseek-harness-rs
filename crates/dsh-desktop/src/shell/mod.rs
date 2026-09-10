//! 根布局:侧栏 + 主列(顶栏 / 消息区 / 底部固定栈,
//! `flex h-full overflow-hidden` 结构);hero 态切换
//! (空会话 = 居中输入卡);设置为独立页(右列整体切换);
//! 轨迹页在右栏面板标签。

pub(crate) mod host;
pub(crate) mod metrics;
pub(crate) mod panel;
pub(crate) mod reducer;
pub(crate) mod store;

mod hero;
mod scroll;
mod statusbar;
mod topbar;

use crate::kits::modals::{
    attachment_toast_card, delete_confirm_modal, rename_modal, workspace_menu_card,
};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled, Window, div,
    px,
};
use gpui_kit::component::StyledExt;

use crate::features::ask;
use crate::features::attachments;
use crate::features::chat;
use crate::features::chat::ComposerMenu;
use crate::features::feedback;
use crate::features::sessions;
use crate::features::settings;
use crate::features::subagents;
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 应用全局键表(main.rs 启动与 layout_tests 装配共用同源,防漂移;
/// 处理器在 WorkspaceView 根 on_action 收口)
pub fn bind_global_keys(cx: &mut gpui_kit::App) {
    cx.bind_keys([gpui_kit::KeyBinding::new(
        "shift-cmd-p",
        panel::OpenPanelPlan,
        None,
    )]);
}

/// 工作区根视图(状态收口在 AppStore)
pub struct WorkspaceView {
    store: Entity<AppStore>,
    /// 渲染次数(测试断言重绘节奏:真流式 = 每 chunk 一画)
    #[cfg(test)]
    pub(crate) render_count: usize,
}

impl WorkspaceView {
    /// 构造(搜索输入等挂窗态已在 main 的 attach_window_state 完成)
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        // 状态 → 视图重绘链(真流式的根基):AppStore 每次 notify
        // (帧泵逐 chunk / 轮询 / 操作)必须显式观察转发——gpui 的
        // Entity::notify 只达 observers,render 期读取不构成订阅;
        // 缺此链时窗口只在 OS 事件(鼠标/闪烁)时重绘,chunk 静默
        // 改状态后一次性画出全文 =「假流式」
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        // ⇧⌘P = 开面板计划标签。App 级 on_action(元素级 on_action 须在
        // 焦点路径上,启动未聚焦时收不到;全局监听在 bubble 末尾,无焦点
        // 也送达;Context 有同名低阶方法,须全限定)。键表见
        // shell::bind_global_keys
        let hotkey_store = store.clone();
        gpui_kit::App::on_action(cx, move |_: &panel::OpenPanelPlan, cx: &mut gpui_kit::App| {
            hotkey_store.update(cx, |st, cx| {
                st.open_panel_tab(panel::PanelTab::Plan, cx)
            });
        });
        Self {
            store,
            #[cfg(test)]
            render_count: 0,
        }
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.render_count += 1;
        }
        if std::env::var_os("DSH_PROBE").is_some() {
            use std::sync::atomic::{AtomicU64, Ordering};
            static T0: AtomicU64 = AtomicU64::new(0);
            static N: AtomicU64 = AtomicU64::new(0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if T0.load(Ordering::Relaxed) == 0 {
                T0.store(now, Ordering::Relaxed);
                eprintln!("[t3] render #1 (基准)");
            } else {
                let n = N.fetch_add(1, Ordering::Relaxed) + 2;
                eprintln!("[t3] render #{n} +{}ms", now - T0.load(Ordering::Relaxed));
            }
        }
        // 渲染期窗口态回写:composer 延迟清空 + 消息列虚拟化对齐
        // (splice/reset 记账)+ 跟随滚底 + 轨迹滚动冲洗(prepend
        // 锚定/跟随滚底)(订阅/帧泵回调无窗口句柄或无滚动时机;
        // 见 store 字段注释)
        self.store.update(cx, |s, cx| {
            s.flush_composer_clear(window, cx);
            s.sync_composer_placeholder(window, cx);
            s.flush_trajectory_scroll();
            s.sync_chat_list(cx);
            s.sync_retry_tick(cx);
            // 列宽变化通知(宽变失效 → settle 全量重测;见 chat/store 注释)
            s.sync_chat_list_width(
                crate::shell::metrics::window_chat_col_w(
                    window,
                    s.sidebar_collapsed,
                    s.sidebar_px,
                    s.panel_open,
                    s.panel_px,
                ),
                cx,
            );
            // @ 补全 Enter 选中(渲染期消费标志;需 window)
            s.finish_at_completion(window, cx);
            if s.chat.chat_version != s.chat.rendered_version {
                if s.chat.pinned {
                    s.chat.chat_list.scroll_to(gpui_kit::ListOffset {
                        item_ix: usize::MAX,
                        offset_in_item: px(0.),
                    });
                }
                s.chat.rendered_version = s.chat.chat_version;
            }
        });
        let st = self.store.read(cx);
        let hero = st.hero();
        let sidebar_collapsed = st.sidebar_collapsed;
        // 对话列宽(消息列/composer/hero 统一;见 metrics 策略;侧栏拖宽后
        // 随 sidebar_px 收窄内容区)
        let col_w = crate::shell::metrics::window_chat_col_w(
            window,
            sidebar_collapsed,
            st.sidebar_px,
            st.panel_open,
            st.panel_px,
        );
        // 任一菜单开 → 根级 mousedown 全关(bubble 相,命中树祖先皆达)。
        // 开着的菜单区自带 stop_propagation 豁免(composer 菜单槽/⋯ 钮/
        // 菜单卡/工作区下拉),豁免与关闭同挂 mousedown,不会先关再被
        // toggle 重开
        let any_menu_open = st.chat.composer_menu != ComposerMenu::None
            || st.hero_menu != crate::shell::store::HeroMenu::None
            || st.sessions.menu_open_session.is_some()
            || st.sessions.menu_open_ws.is_some()
            || st.sessions.workspace_menu_open
            || st.panel_plus_menu_at.is_some();
        let settings_open = st.settings.settings_open;
        div()
            .relative()
            // 分离式侧栏(方案 B):左右两列——左列侧栏卡满高(顶到窗口
            // 顶 8px 边距,macOS 交通灯浮于卡上,卡内顶部留拖拽让位);
            // 右列 = 标题行 + 内容卡 + 状态栏。标题栏/状态栏不再压缩
            // 侧栏空间,只占内容区域
            .flex()
            .size_full()
            .overflow_hidden()
            .bg(theme::BASE())
            .text_color(theme::LABEL())
            .when(any_menu_open, |el| {
                el.on_mouse_down(gpui_kit::MouseButton::Left, {
                    let store = self.store.clone();
                    move |_, _, cx| {
                        store.update(cx, |st, cx| st.close_all_menus(cx));
                    }
                })
            })
            // Esc = 关查看器(捕获相:查看器开启时拦截吞掉;查看器关闭
            // 时无其他 Esc 语义,放行)
            .capture_key_down({
                let store = self.store.clone();
                move |ev: &gpui_kit::KeyDownEvent, _window, cx| {
                    if ev.keystroke.key.as_str() == "escape"
                        && store.read(cx).chat.mermaid_viewer.is_some()
                    {
                        store.update(cx, |st, cx| st.close_mermaid_viewer(cx));
                        cx.stop_propagation();
                    }
                }
            })
            // 左列:全高扁平面板(贴窗口边,无卡)
            .child(sessions::render(&self.store, cx))
            // 右列:设置页开 = 整列接管(顶栏/内容/状态栏全部让位,
            // 设置页自带拖拽头——工作区 chrome 不压在设置页上);
            // 闭 = 标题行 → 内容卡 → 状态栏
            .child(if settings_open {
                settings::render(&self.store, cx).into_any_element()
            } else {
                div()
                    .v_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .h_full()
                    // 自绘标题栏(拖拽/双击缩放由 TitleBar 提供;
                    // pl 覆盖 = 交通灯已移左列侧栏上,右列无让位;
                    // h 覆盖 = 组件库默认 34,与右栏面板头 40 同高对齐);
                    // 无底部分割线(靠留白分层)
                    .child(
                        gpui_kit::component::TitleBar::new()
                            .pl(px(8.))
                            .h(px(40.))
                            .child(topbar::title_bar_row(&self.store, window, cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .flex_1()
                            .flex_col()
                            // 内容区扁平(去胶囊卡):直接落 BASE 画布,满幅
                            // 到窗口右缘;bubble/工具行/composer 作为浮层
                            // 组件与画布形成层次
                            .overflow_hidden()
                            // 滚动条挂点:组件库默认滚动条盖满本列(全高,
                            // 到窗底;挂在 chat_pane 内只到聊天框上边,
                            // 不可取)
                            .relative()
                            // 测试钩子:布局回归断言双栏分离(release 空操作)
                            .debug_selector(|| "content-card".to_string())
                            .when(hero, |el| {
                                el.child(hero::render(&self.store, col_w, window, cx))
                            })
                            .when(!hero, |el| {
                                el.child(
                                    chat::chat_pane::render(&self.store, window, cx)
                                        .into_any_element(),
                                )
                                .child(
                                    // 底部栈:统一列宽容器(与消息列同宽同中心线)
                                    div()
                                        .v_flex()
                                        .flex_shrink_0()
                                        // 列对齐容器同款槽 padding(左锚点槽/
                                        // 右滚动条槽,与列表容器同中心线)
                                        .pl(px(
                                            crate::shell::metrics::H_PAD
                                                + crate::shell::metrics::NAV_GUTTER_W,
                                        ))
                                        .pr(px(
                                            crate::shell::metrics::H_PAD
                                                + crate::shell::metrics::SCROLLBAR_GUTTER_W,
                                        ))
                                        .pt(px(4.))
                                        .pb(px(8.))
                                        .child(
                                            div()
                                                .v_flex()
                                                .mx_auto()
                                                .w(col_w)
                                                .gap(px(8.))
                                                .child(chat::queue_dock::render(
                                                    &self.store,
                                                    window,
                                                    cx,
                                                ))
                                                .children(ask::render_plan(
                                                    &self.store,
                                                    window,
                                                    cx,
                                                ))
                                                .children(ask::render_approval(&self.store, cx))
                                                .children(ask::render_question(
                                                    &self.store,
                                                    window,
                                                    cx,
                                                ))
                                                // todo_dock = 纯状态展示(非阻塞交互,
                                                // 不同于审批/提问)
                                                .children(chat::todo_dock::render(
                                                    &self.store,
                                                    cx,
                                                ))
                                                // 子代理任务条(聊天框上方常驻
                                                // chips:运行中子代理一键切换查看)
                                                .when_some(
                                                    subagents::task_bar(&self.store, cx),
                                                    |el, bar| el.child(bar),
                                                )
                                                .child(chat::composer::render(
                                                    &self.store,
                                                    window,
                                                    cx,
                                                )),
                                        ),
                                )
                                // 组件库默认滚动条(滚动时浮现、闲置淡出;拖
                                // 曳走 ListState 协作 API):元素填满父容器 =
                                // 盖满整列到窗底(挂 chat_pane 内只到聊天框
                                // 上边,不成全高)。
                                // 组件库把轨道自身高当滚动视口,全高轨道的
                                // 差值段(底部栈/状态栏)成 thumb
                                // 盲区(「不能拉到底部」)
                                // → FullTrackHandle 以 (轨道高−视口高) 补偿
                                // content_size,满行程映射回列表真实滚动域
                                .child({
                                    let st = self.store.read(cx);
                                    let extra = st.chat.track_h
                                        - f32::from(st.chat.chat_list.viewport_bounds().size.height);
                                    let handle =
                                        scroll::FullTrackHandle::new(&st.chat.chat_list, gpui_kit::px(extra));
                                    // viewport_from_layout:轨道钉元素布局
                                    // bounds(全列)——默认走 handle 的列表
                                    // 视口,轨道会缩在列表段;全列轨道 +
                                    // handle 的 content_size
                                    // 补偿 = thumb 满行程映射回列表真实滚动域
                                    gpui_kit::component::scroll::Scrollbar::new(&handle)
                                        .viewport_from_layout()
                                })
                                // 轨道高捕获(渲染期 paint;差值补偿的分子,
                                // 见 shell/scroll.rs)。absolute 层不占 flex
                                // 位、无 hitbox 不挡交互;变化守卫在
                                // note_track_h(防每帧 notify 死循环)
                                .child({
                                    let cap = self.store.clone();
                                    div()
                                        .absolute()
                                        .inset_0()
                                        // canvas 默认 0 高(nav_track 靠两个
                                        // 0 高 canvas 的 origin 夹边界);
                                        // 此处要 size,须显式铺满容器
                                        .child(
                                            gpui_kit::canvas(
                                                move |b, _, cx| {
                                                    let h = b.size.height.as_f32();
                                                    cap.update(cx, |st, cx| st.note_track_h(h, cx));
                                                },
                                                |_, _, _, _| {},
                                            )
                                            .size_full(),
                                        )
                                })
                            }),
                    )
                    // 状态栏(仅右列 26px:统计 + 权限/模型/分支徽标;
                    // 侧栏下方不再被压)
                    .child(statusbar::render(&self.store, cx))
                    .into_any_element()
            })
            // 右侧面板(右栏;settings 整列接管时面板内部自行
            // 返回空,收起时同)
            .when_some(
                (!self.store.read(cx).settings.settings_open)
                    .then(|| panel::render(&self.store, window, cx)),
                |el, panel| el.child(panel),
            )
            // 标题栏工作区下拉卡(root 级:TitleBar 是首个子,后续兄弟
            // 绘制在其上,卡放 TitleBar 内会被主内容盖住)
            .when(self.store.read(cx).sessions.workspace_menu_open, |el| {
                el.child(workspace_menu_card(&self.store, cx))
            })
            // 会话行 ⋯ 菜单(root 级:行内 absolute 会被侧栏卡
            // overflow_hidden 裁剪 + 被后绘的内容卡遮挡)
            .when(
                self.store.read(cx).sessions.menu_open_session.is_some(),
                |el| {
                    let card = {
                        let st = self.store.read(cx);
                        st.sessions
                            .menu_open_session
                            .as_deref()
                            .zip(st.sessions.row_menu_pos)
                            .map(|(id, pos)| sessions::row_menu_card(&self.store, id, pos))
                    };
                    el.children(card)
                },
            )
            // 工作区分组头 ⋯ 菜单(同上:root 级定位渲染)
            .when(self.store.read(cx).sessions.menu_open_ws.is_some(), |el| {
                let card = {
                    let st = self.store.read(cx);
                    st.sessions
                        .menu_open_ws
                        .as_deref()
                        .zip(st.sessions.ws_menu_pos)
                        .map(|(ws, pos)| sessions::ws_menu_card(&self.store, cx, ws, pos))
                };
                el.children(card)
            })
            // 面板「+」菜单(root 级定位渲染,同 row/ws 菜单;徽标文案
            // 与面板空态同源:键表生成)
            .when(
                self.store.read(cx).panel_plus_menu_at.is_some(),
                |el| {
                    let card = self
                        .store
                        .read(cx)
                        .panel_plus_menu_at
                        .map(|pos| {
                            let shortcut =
                                window.keystroke_text_for(&panel::OpenPanelPlan);
                            panel::plus_menu_card(&self.store, pos, shortcut)
                        });
                    el.children(card)
                },
            )
            // composer 权限下拉(根级渲染,同 +/行/工作区菜单模式:
            // 内联浮层叠进输入卡子树会透视;其余
            // composer 下拉不叠卡体,维持内联)。锚 = 渲染期捕获的
            // chip bounds(见 ChatState.perm_chip_bounds):卡底缘贴
            // chip 顶上方 5px、左缘对齐。settings 整列接管时
            // 不渲染(composer 已让位,bounds 为陈旧值)
            .when(
                self.store.read(cx).chat.composer_menu == ComposerMenu::Permission
                    && !self.store.read(cx).settings.settings_open,
                |el| {
                    let card = {
                        let st = self.store.read(cx);
                        st.chat.perm_chip_bounds.map(|b| {
                            let card = chat::composer::permission_card(&self.store, cx);
                            let vh = f32::from(window.viewport_size().height);
                            div()
                                .id("composer-perm-menu")
                                .absolute()
                                .left(px(f32::from(b.origin.x)))
                                .bottom(px(vh - f32::from(b.origin.y) + 5.))
                                // 阻断命中穿透 + 外点关闭豁免(同 + 菜单卡)
                                .occlude()
                                .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation()
                                })
                                .child(card)
                        })
                    };
                    el.children(card)
                },
            )
            .when(
                self.store.read(cx).sessions.rename_target.is_some()
                    || self.store.read(cx).sessions.rename_ws_target.is_some(),
                |el| el.child(rename_modal(&self.store, cx)),
            )
            .when(self.store.read(cx).sessions.delete_target.is_some(), |el| {
                el.child(delete_confirm_modal(&self.store, cx))
            })
            .when(
                self.store
                    .read(cx)
                    .settings
                    .delete_provider_target
                    .is_some(),
                |el| el.child(settings::provider_delete_modal(&self.store, cx)),
            )
            .when(self.store.read(cx).settings.model_fetch.is_some(), |el| {
                el.child(settings::provider_models_fetch_modal(&self.store, cx))
            })
            .when(
                self.store.read(cx).settings.full_access_confirm.is_some(),
                |el| el.child(settings::full_access_modal(&self.store, cx)),
            )
            // 图片 Lightbox 与附件拒收 toast(root 级树序末尾,
            // 后绘制在上;Lightbox 遮罩叠于全部内容)
            .when(self.store.read(cx).attachments.lightbox.is_some(), |el| {
                el.child(attachments::lightbox(&self.store, cx))
            })
            // Mermaid 查看器(根级;与 lightbox 同构。置于 toast 前——
            // 下载完成通知须浮于查看器遮罩之上)
            .when(self.store.read(cx).chat.mermaid_viewer.is_some(), |el| {
                el.child(chat::mermaid_viewer::mermaid_viewer(
                    &self.store,
                    window,
                    cx,
                ))
            })
            // 消息反馈备注弹窗(根级;open_note → 输入框+保存/取消)
            .when(
                self.store.read(cx).feedback.feedback_note_editor.is_some(),
                |el| el.children(feedback::render_note_editor(&self.store, cx)),
            )
            .when(
                self.store.read(cx).attachments.attachment_toast.is_some(),
                |el| el.child(attachment_toast_card(&self.store, cx)),
            )
    }
}

/// 真实整树布局回归:AppHost(fake,临时根)→ AppStore → 注入长会话
/// 节点(用户/助手/工具/收尾混合)→ WorkspaceView 渲染,断言相邻
/// 节点 bounds 垂直不交叠。
#[cfg(test)]
mod layout_tests;
