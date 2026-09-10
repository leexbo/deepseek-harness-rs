//! AppStore:应用级多会话状态 + 宿主桥 + handler。
//! App 是唯一状态收口;UI 组件只读渲染 + 调 handler。

use std::collections::HashMap;

use dsh_core::proto::ServerRequest;
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::{AppContext, Context, Window};

use crate::features::ask::AskStore;
use crate::features::attachments::AttachmentsStore;
use crate::features::chat::{ChatNode, ChatStore, ComposerMenu};
use crate::features::feedback::FeedbackStore;
use crate::features::search::SearchStore;
use crate::features::sessions::SessionsStore;
use crate::features::settings::SettingsStore;
use crate::features::subagents::SubagentsStore;
use crate::features::trajectory::TrajectoryStore;
use crate::kits::theme;
use crate::shell::host::HostBridge;
use crate::shell::panel::PanelTab;
use crate::shell::reducer::{self, Effect, StoreState};

/// hero 空态 chip 下拉(工作区/模式;互斥)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeroMenu {
    /// 关
    None,
    /// 工作区选择
    Workspace,
    /// 模式(preset)选择
    Preset,
}

/// 会话级配置缓存(打开会话时拉取,设置成功后回写)
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCfg {
    /// 模型(override > 宿主默认)
    pub model: String,
    /// 权限(read-only / workspace-write / full-access)
    pub permission: String,
    /// 思考等级(low / high / max;宿主恒 Some,默认 high)
    pub effort: Option<String>,
    /// 模式/preset(standard / minimal / 工作区自定义)
    pub preset: String,
}

/// 应用状态根。
pub struct AppStore {
    /// 图片附件功能切片状态(草稿轨/解码缓存/lightbox/拒收 toast;
    /// 域与行为见 features::attachments)
    pub attachments: AttachmentsStore,
    /// 问答/计划审批功能切片状态(问答卡交互态 + 自定义输入;
    /// 域与行为见 features::ask)
    pub ask: AskStore,
    /// 消息反馈功能切片状态(赞/踩/备注 + 弹窗;域与行为见
    /// features::feedback)
    pub feedback: FeedbackStore,

    /// 可纯化状态(帧 reducer 的作用域)
    pub state: StoreState,
    /// 宿主桥(含 runtime 保活;UI handler 直调)
    pub bridge: HostBridge,
    /// 会话 → 统计(状态栏/上下文占用)
    pub stats_by_id: HashMap<String, serde_json::Value>,
    /// 运行时长秒刷定时器:仅当前会话 running 时在场;drop = 取消。
    /// 存句柄以便会话停止/切换时主动停止,避免空闲期悬空轮询。
    pub run_tick: Option<gpui_kit::Task<()>>,
    /// 子代理菜单秒刷定时器(lineage 菜单开且有运行中子代理时在场)
    pub lineage_tick: Option<gpui_kit::Task<()>>,
    /// 额度自动刷新 5min 节拍(挂窗一次常驻;触发面见 start_billing_tick)
    pub billing_tick: Option<gpui_kit::Task<()>>,
    /// 系统外观观察者订阅(窗口挂载时注册一次,drop = 退订)
    pub appearance_sub: Option<gpui_kit::Subscription>,
    /// 全库检索功能切片状态(侧栏搜索输入/命中面板/跳转定位;域与行为见
    /// features::search)
    pub search: SearchStore,
    /// 聊天消息流功能切片状态(composer 输入/清空、消息列虚拟化与钉底跟随、
    /// 工具卡/think/上下文/搜索组折叠、TodoDock 开态、composer 下拉开态、
    /// @ 补全探测与 Enter 标志、复制反馈;域与行为见 features::chat)
    pub chat: ChatStore,
    /// 设置功能切片状态(页路由/快照/onboarding/provider 编辑器与删除
    /// 确认/偏好下拉;域与行为见 features::settings)
    pub settings: SettingsStore,
    /// 侧栏折叠(56px rail)
    pub sidebar_collapsed: bool,
    /// 右侧面板开态(右栏;手动开关)
    pub panel_open: bool,
    /// 右侧面板宽(下限 PANEL_MIN,上限随侧栏形态协商;默认 = 下限)
    pub panel_px: f32,
    /// 面板拖宽锚点(光标 x + 起始宽 + 视口宽)
    pub panel_resize_anchor: Option<(f32, f32, f32)>,
    /// 面板开着的标签页(同类去重,序 = 打开序;标签壳形态)
    pub panel_tabs: Vec<PanelTab>,
    /// 面板激活标签(None = 空态快捷菜单,面板不自动收)
    pub panel_active_tab: Option<PanelTab>,
    /// 面板「+」菜单锚点坐标(root 级渲染;None = 关)
    pub panel_plus_menu_at: Option<gpui_kit::Point<gpui_kit::Pixels>>,
    /// 侧栏展开宽(用户可拖宽;clamp 到 [SIDEBAR_MIN, SIDEBAR_MAX],
    /// 独立于折叠——折叠不写 0,重开仍用此宽)
    pub sidebar_px: f32,
    /// 侧栏拖宽锚点(当次拖拽:光标 x + 起始宽);None = 未拖拽
    pub sidebar_resize_anchor: Option<(f32, f32)>,
    /// 会话与工作区树功能切片状态(行/组/工作区菜单开态与坐标、
    /// 重命名/删除目标、工作区路径/标题/分支表、折叠组;域与行为
    /// 见 features::sessions)
    pub sessions: SessionsStore,
    /// hero 空态 chip 下拉开态(互斥)
    pub hero_menu: HeroMenu,
    /// 会话 → 配置缓存(模型/权限/思考等级/模式)
    pub session_cfg_by_id: HashMap<String, SessionCfg>,
    /// 子代理血缘功能切片状态(后代目录开态;域与行为见
    /// features::subagents)
    pub subagents: SubagentsStore,
    /// 轨迹功能切片状态(台账/检查器/时间线/turns-calls 折叠;
    /// 域与行为见 features::trajectory)
    pub trajectory: TrajectoryStore,
    /// 本地通告序号(Notice key 去重用)
    local_notice_seq: u64,
}

impl AppStore {
    /// 启动序列(describe + 清单 + 自动打开/新建;对齐 web 启动 effect)
    pub fn new(bridge: HostBridge, cx: &mut Context<Self>) -> Self {
        let host_info = bridge.describe();
        let mut sessions = bridge.host().list_sessions();
        let active_workspace = host_info.workspaces.first().cloned();
        let mut store = Self {
            state: StoreState {
                sessions: vec![],
                titles: HashMap::new(),
                current_id: None,
                running_by_id: HashMap::new(),
                running_since_by_id: HashMap::new(),
                host_info,
                active_workspace,
                chats: HashMap::new(),
                jobs_by_id: HashMap::new(),
                pending_plan: None,
                pending_ask: None,
                pending_approval: None,
            },
            bridge,
            stats_by_id: HashMap::new(),
            run_tick: None,
            lineage_tick: None,
            billing_tick: None,
            attachments: AttachmentsStore::default(),
            ask: AskStore::default(),
            feedback: FeedbackStore::default(),
            search: SearchStore::default(),
            chat: ChatStore::default(),
            settings: SettingsStore::default(),
            sidebar_collapsed: false,
            sidebar_px: crate::shell::metrics::SIDEBAR_W,
            panel_open: false,
            panel_px: crate::shell::metrics::PANEL_W,
            panel_resize_anchor: None,
            appearance_sub: None,
            panel_tabs: Vec::new(),
            panel_active_tab: None,
            panel_plus_menu_at: None,
            sidebar_resize_anchor: None,
            sessions: SessionsStore::default(),
            hero_menu: HeroMenu::None,
            session_cfg_by_id: HashMap::new(),
            subagents: SubagentsStore::default(),
            trajectory: TrajectoryStore::default(),
            local_notice_seq: 0,
        };
        if sessions.is_empty() {
            // 空仓:自动建会话再刷新(web 同款)
            let ws = store.non_default_workspace();
            store.bridge.host().create_session(None, None, ws);
            sessions = store.bridge.host().list_sessions();
        }
        store.state.sessions = sessions;
        // 工作区表 + 分支基线(标题栏下拉/StatusBar 徽标)
        store.refresh_workspaces();
        // onboarding 基线(未引导且凭据缺席 → hero 引导条)
        store.settings.settings_snapshot = store.bridge.host().settings_view();
        store.recalc_onboarding();
        // 启动即完整打开首个会话(history 折叠 + 统计),不再等点击
        if let Some(first) = store.state.sessions.first().map(|s| s.session_id.clone()) {
            store.open_session(&first, cx);
        }
        store
    }

    /// 挂窗态(输入框需要 Window;在 WorkspaceView 构造时调用)
    pub fn attach_window_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 系统外观观察者:System 档时系统切浅/深 → 主题实时同步
        // (非 System 档回调即短路;显式档强制 NSApp 外观触发的回调同样
        // 短路,不会成环)。订阅存 self,drop = 退订。
        if self.appearance_sub.is_none() {
            self.appearance_sub = Some(window.observe_window_appearance(|window, cx| {
                if theme::current_appearance() == theme::Appearance::System {
                    theme::apply(theme::Appearance::System, Some(window), cx);
                }
            }));
        }
        self.ensure_search_input(window, cx);
        if self.trajectory.trajectory_search.is_none() {
            self.trajectory.trajectory_search =
                Some(cx.new(|cx| InputState::new(window, cx).placeholder("搜索")));
        }
        self.ensure_provider_form_inputs(window, cx);
        self.ensure_pref_selects(window, cx);
        // 额度自动刷新节拍(挂窗一次):启动即查 + 每 5min 一轮
        self.start_billing_tick(cx);
        if self.chat.composer_input.is_none() {
            let composer = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(1, 8)
                    .placeholder("输入消息,Enter 发送 / Shift+Enter 换行")
            });
            // Enter 发送(shift=true = Shift+Enter 换行,交给默认行为);
            // 多行模式 Enter 已默认插入换行 → 取值后剥尾随换行,置位延迟清空。
            // @ 补全打开时:Enter 选中高亮项而非发送(源菜单 arbitrate)。
            cx.subscribe(&composer, |this, input, event: &InputEvent, cx| {
                match event {
                    InputEvent::PressEnter { shift: false, .. } => {
                        if this.chat.at_completion.is_some() {
                            // 选中由渲染层处理(需要 window);这里标记一次
                            this.chat.enter_at_completion = true;
                            cx.notify();
                            return;
                        }
                        let raw = input.read(cx).value().to_string();
                        let text = raw.trim_end_matches('\n').trim().to_string();
                        if !text.is_empty() {
                            this.send(&text, cx);
                            this.chat.pending_composer_clear = true;
                        }
                    }
                    InputEvent::Change => {
                        // @ 触发探测:每次文本/光标变化重算候选
                        this.update_at_completion(cx);
                    }
                    _ => {}
                }
            })
            .detach();
            self.chat.composer_input = Some(composer);
        }
    }

    // ── 帧应用 ─────────────────────────────────────────────────

    /// 应用一帧(reducer 纯函数 + 副作用执行;当前会话的 chat 事件
    /// 先按现滚动位置评估 pinned 再计数,渲染侧据此跟随滚底)。
    /// chat 事件逐帧直通 notify:GPUI 自身按 vsync 合帧(Zed 同款
    /// 用法);曾试 background-timer 合帧,空闲主循环不被唤醒导致
    /// 重绘信号卡死(表现为流式期间完全无输出),已回退。
    pub fn apply_frame(&mut self, frame: ServerRequest, cx: &mut Context<Self>) {
        // MCP 连接状态(宿主级端口池三态 + 停机):维护设置页状态表;
        // 失败落设置页通告(连接与具体会话无关,不进聊天区)
        if frame.method.as_str() == "mcp/status" {
            if let Some(server) = frame.payload["server"].as_str() {
                let status = frame.payload["status"].as_str().unwrap_or_default();
                let error = frame.payload["error"].as_str().unwrap_or_default();
                self.settings
                    .mcp_status_by_id
                    .insert(server.to_string(), (status.to_string(), error.to_string()));
                if status == "failed" {
                    self.set_settings_notice(
                        false,
                        format!("MCP server「{server}」连接失败:{error}"),
                        cx,
                    );
                }
            }
            return;
        }
        let touches_current_chat = frame.method.as_str() == "session/event"
            && frame.payload["sessionId"]
                .as_str()
                .is_some_and(|id| Some(id) == self.state.current_id.as_deref());
        if touches_current_chat {
            self.chat.pinned = self.at_bottom();
        }
        // 轨迹直播(回合粒度):当前会话 turn 结束且轨迹面板可见 → 重拉
        // (桌面全量重折叠上 tokio,回合粒度足够)
        let trajectory_live = touches_current_chat
            && self.trajectory_visible()
            && frame.payload["event"]["type"].as_str() == Some("turn/end");
        // 额度就近补查:turn 消耗完 → default provider 静默刷新(60s 防抖)
        let billing_due =
            touches_current_chat && frame.payload["event"]["type"].as_str() == Some("turn/end");
        // 会话事件含 image 块 → 异步拉取历史图缓存(经 read_attachment)
        if touches_current_chat {
            let sid = self.state.current_id.clone();
            let ev = &frame.payload["event"];
            let mut collect = |blocks: &[serde_json::Value]| {
                for b in blocks {
                    if let Some(aid) = b["attachment"]["attachmentId"].as_str()
                        && let Some(sid) = sid.clone()
                    {
                        self.ensure_image_loaded(&sid, aid, cx);
                    }
                }
            };
            if let Some(arr) = ev["data"]["content"].as_array() {
                collect(arr);
            }
            if let Some(arr) = frame.payload["items"].as_array() {
                for item in arr {
                    if let Some(content) = item["message"]["content"].as_array() {
                        collect(content);
                    }
                }
            }
        }
        // 权限回声权威校准:driver 落档 sandbox/mode → cfg 缓存(乐观
        // 更新的校准源;set_permission 成功回调的 refresh 读落档前旧值,
        // 竞态见 set_session_permission 注释)
        if touches_current_chat
            && frame.payload["event"]["type"] == "sandbox/mode"
            && let Some(mode) = frame.payload["event"]["data"]["mode"].as_str()
        {
            self.apply_permission_echo(mode, cx);
        }
        let effects = reducer::apply_frame(&mut self.state, frame);
        for effect in effects {
            self.run_effect(effect, cx);
        }
        if touches_current_chat {
            self.chat.chat_version += 1;
        }
        if trajectory_live {
            self.refresh_trajectory(cx);
        }
        if billing_due {
            self.auto_refresh_billing(cx);
        }
        self.sync_run_tick(cx);
        self.sync_lineage_tick(cx);
        cx.notify();
    }

    /// 打断运行中的子代理(任务面板行直呼;信号经宿主注册表,
    /// 与 interrupt_agent 工具同语义)
    pub fn interrupt_subagent(&mut self, child_id: &str, cx: &mut Context<Self>) {
        self.bridge.host().interrupt_subagent(child_id);
        cx.notify();
    }

    /// 子代理秒刷:当前查看视角有运行中子代理 → 1s 节拍,
    /// 驱动任务条与血缘菜单的行内计时(任务条常驻显示运行态,不再
    /// 依赖菜单开态);无运行中子代理即停(照 sync_run_tick 手法)。
    pub(crate) fn sync_lineage_tick(&mut self, cx: &mut Context<Self>) {
        let ticking = self
            .state
            .current_id
            .as_deref()
            .is_some_and(|id| self.running_subagent_count(&self.subagent_anchor_of(id)) > 0);
        if ticking && self.lineage_tick.is_none() {
            self.lineage_tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                    let gone = this
                        .update(cx, |s, cx| {
                            let alive = s.state.current_id.as_deref().is_some_and(|id| {
                                s.running_subagent_count(&s.subagent_anchor_of(id)) > 0
                            });
                            if alive {
                                cx.notify();
                            }
                            !alive
                        })
                        .unwrap_or(true);
                    if gone {
                        break;
                    }
                }
            }));
            cx.notify();
        } else if !ticking {
            self.lineage_tick.take();
        }
    }

    /// 运行时长秒刷:仅当前会话 running 时启动 1s 轮询节拍,会话结束/
    /// 切换即停。用于驱动标题栏「标题 + 时长」与转写区「深入探索中…」
    /// 的实时时钟(真实时序须定时器逐秒重算,不能靠帧动画)。
    pub(crate) fn sync_run_tick(&mut self, cx: &mut Context<Self>) {
        let running = self
            .state
            .current_id
            .as_deref()
            .is_some_and(|id| self.state.running_by_id.get(id).copied().unwrap_or(false));
        if running && self.run_tick.is_none() {
            self.run_tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                    // 实体已销毁或节拍已停 → 退出循环(update 返回 Err)
                    let gone = this
                        .update(cx, |s, cx| {
                            let alive = s.state.current_id.as_deref().is_some_and(|id| {
                                s.state.running_by_id.get(id).copied().unwrap_or(false)
                            });
                            if alive {
                                cx.notify();
                            }
                            !alive
                        })
                        .unwrap_or(true);
                    if gone {
                        break;
                    }
                }
            }));
            cx.notify();
        } else if !running {
            // 会话停止/切换:停掉旧节拍,让 run_tick 复位为 None
            self.run_tick.take();
        }
    }

    fn run_effect(&mut self, effect: Effect, cx: &mut Context<Self>) {
        match effect {
            Effect::Sessions => self.refresh_list(),
            Effect::HostInfo => {
                self.state.host_info = self.bridge.describe();
                // 工作区增减连带路径/分支表
                self.refresh_workspaces();
            }
            Effect::Stats(id) => {
                // 状态边沿**同步**拉取:session/stats 推送的丢帧自愈
                // 安全网(tokio broadcast 滞后丢帧;单会话解析与同帧
                // list_sessions 全量扫描同量级)
                self.refresh_stats_sync(&id);
                // 模型可能刚经 bash 切过分支
                self.refresh_branches();
            }
            Effect::StatsUpsert(id, stats) => {
                // 事件驱动实时统计(宿主落档点增量聚合的推送)
                self.stats_by_id.insert(id, stats);
                // 顺带刷分支:模型可经 bash 切分支(非准静态),HEAD
                // 微秒级单文件读,搭统计事件车足够新
                self.refresh_branches();
                cx.notify();
            }
        }
    }

    /// 重拉会话清单
    pub fn refresh_list(&mut self) {
        self.state.sessions = self.bridge.host().list_sessions();
    }

    /// 同步拉会话统计(打开会话/状态边沿;保证帧处理即落表)
    pub(crate) fn refresh_stats_sync(&mut self, id: &str) {
        if let Ok(v) = self.bridge.host().session_stats(id) {
            self.stats_by_id.insert(id.to_string(), v);
        }
    }

    // ── handlers ───────────────────────────────────────────────

    // ── 会话配置(composer 下拉)───────────────────────────────

    /// 拉/刷新会话配置缓存(同步四 getter;open_session 与设置成功后)
    pub fn refresh_session_cfg(&mut self, id: &str) {
        let host = self.bridge.host();
        self.session_cfg_by_id.insert(
            id.to_string(),
            SessionCfg {
                model: host.session_model(id),
                permission: host.session_permission(id),
                effort: host.session_effort(id),
                preset: host.session_preset(id),
            },
        );
    }

    /// 当前会话配置展示值(缓存缺失时回落宿主默认形态)
    pub fn current_cfg_or_default(&self) -> SessionCfg {
        self.state
            .current_id
            .as_deref()
            .and_then(|id| self.session_cfg_by_id.get(id))
            .cloned()
            .unwrap_or_else(|| SessionCfg {
                model: self
                    .state
                    .host_info
                    .model
                    .clone()
                    .unwrap_or_else(|| "deepseek-chat".into()),
                permission: "workspace-write".into(),
                effort: Some("high".into()),
                preset: "standard".into(),
            })
    }

    /// preset id → 展示名(describe presets 表;缺失回落 id)
    pub fn preset_label(&self, id: &str) -> String {
        self.state
            .host_info
            .presets
            .iter()
            .find(|p| p["id"].as_str() == Some(id))
            .and_then(|p| p["name"].as_str())
            .unwrap_or(id)
            .to_string()
    }

    /// 外点全关(composer 下拉 + hero chip 下拉 + 行内 ⋯ + 标题栏
    /// 工作区下拉 + 面板「+」菜单;开着的菜单区自带 mousedown
    /// stop_propagation 豁免,不会误伤自身交互)
    pub fn close_all_menus(&mut self, cx: &mut Context<Self>) {
        self.chat.composer_menu = ComposerMenu::None;
        self.hero_menu = HeroMenu::None;
        self.sessions.menu_open_session = None;
        self.sessions.menu_open_ws = None;
        self.sessions.workspace_menu_open = false;
        self.settings.full_access_confirm = None;
        self.panel_plus_menu_at = None;
        self.sync_lineage_tick(cx);
        cx.notify();
    }

    /// composer 权限菜单选「完全权限」:先收菜单,弹风险确认弹窗,
    /// 确认后才真正 set_session_permission(FullAccessAsk::Session)
    pub fn ask_full_access_session(&mut self, cx: &mut Context<Self>) {
        self.chat.composer_menu = ComposerMenu::None;
        self.hero_menu = HeroMenu::None;
        self.settings.full_access_confirm = Some(crate::features::settings::FullAccessAsk::Session);
        cx.notify();
    }

    /// 退出计划模式(激活态 Plan chip 点击;恒发 standard)
    pub fn exit_plan_mode(&mut self, cx: &mut Context<Self>) {
        self.apply_plan_mode(false, cx);
    }

    /// 计划模式设置(Plan chip 退出路径;直接走 set_mode RPC,非 /plan
    /// 文本——进入唯一入口是命令菜单「plan」行 send 拼 /plan,走 host
    /// 命令短路)。**方向一律绝对值,禁止读当前态做 toggle**:
    /// 真机日志取证:嵌套 on_click 在真机鼠标分发里连发
    /// (MouseUp Capture 阶段各自截存按下事件,stop_propagation 拦不住),
    /// 后发者读到先发者已翻转的乐观态即反向补发——真机日志
    /// standard/plan 严格交替 51 条、永远退不出。绝对方向幂等,连发
    /// 收敛同值。乐观更新:点击即刻翻转本地投影(回声帧丢/延迟时 UI
    /// 不至于「点了没反应」),失败回滚+通告
    fn apply_plan_mode(&mut self, target: bool, cx: &mut Context<Self>) {
        let Some(id) = self.state.current_id.clone() else {
            return;
        };
        let mode = if target { "plan" } else { "standard" };
        let prior = self.current_chat().map(|c| c.plan_mode).unwrap_or(!target);
        if let Some(chat) = self.state.chats.get_mut(&id) {
            chat.plan_mode = target;
        }
        cx.notify();
        let store = cx.entity().clone();
        let host = self.bridge.host().clone();
        let rollback_id = id.clone();
        let rx = self
            .bridge
            .call(async move { host.set_mode(&id, mode).await });
        cx.spawn(async move |_this, cx| {
            match rx.await {
                Ok(Err(e)) => store.update(cx, |s, cx| {
                    // 失败回滚乐观态,下次回声再校准
                    if let Some(chat) = s.state.chats.get_mut(&rollback_id) {
                        chat.plan_mode = prior;
                    }
                    s.push_local_notice(&format!("模式切换失败:{}", e.message), cx);
                }),
                Err(_) => store.update(cx, |s, cx| {
                    if let Some(chat) = s.state.chats.get_mut(&rollback_id) {
                        chat.plan_mode = prior;
                    }
                    s.push_local_notice("模式切换:通道失败", cx);
                }),
                _ => {}
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 切权限(idle-only;host 侧异步落 sandbox/mode 日志事件——可重放,
    /// 不再走 override/设置的旧存储)。**乐观更新**:RPC 只是异步入队
    /// 即返回,driver 落档前折叠磁盘日志读到的还是旧值
    /// (真机日志:6 击成对同值=首击宿主已切、成功回调 refresh 读旧把
    /// 标签刷回去,须补点)。就地写目标值,失败回滚+通告;回声
    /// 经 apply_permission_echo 权威校准(成功路径不再 refresh)
    pub fn set_session_permission(&mut self, permission: &str, cx: &mut Context<Self>) {
        let Some(id) = self.state.current_id.clone() else {
            return;
        };
        self.chat.composer_menu = ComposerMenu::None;
        self.hero_menu = HeroMenu::None;
        let v = permission.to_string();
        let prior = self
            .session_cfg_by_id
            .get(&id)
            .map(|c| c.permission.clone())
            .unwrap_or_else(|| self.bridge.host().session_permission(&id));
        if let Some(cfg) = self.session_cfg_by_id.get_mut(&id) {
            cfg.permission = v.clone();
        } else {
            let mut cfg = self.current_cfg_or_default();
            cfg.permission = v.clone();
            self.session_cfg_by_id.insert(id.clone(), cfg);
        }
        cx.notify();
        let store = cx.entity().clone();
        let host = self.bridge.host().clone();
        let call_id = id.clone();
        let rx = self
            .bridge
            .call(async move { host.set_permission(&call_id, &v).await });
        cx.spawn(async move |_this, cx| {
            match rx.await {
                Ok(Ok(())) => {} // 落档回声为准(apply_permission_echo 校准)
                Ok(Err(e)) => store.update(cx, |s, cx| {
                    s.set_cfg_permission(&id, &prior);
                    s.push_local_notice(&format!("切换失败:{}", e.message), cx);
                }),
                Err(_) => store.update(cx, |s, cx| {
                    s.set_cfg_permission(&id, &prior);
                    s.push_local_notice("权限切换:通道失败", cx);
                }),
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 写回权限 cfg 缓存(回声校准/失败回滚共用)
    fn set_cfg_permission(&mut self, id: &str, permission: &str) {
        if let Some(cfg) = self.session_cfg_by_id.get_mut(id) {
            cfg.permission = permission.to_string();
        }
    }

    /// sandbox/mode 落档回声 → cfg 缓存权威校准(driver 已实际生效;
    /// 首次出现则按当前形态补建缓存条目)
    fn apply_permission_echo(&mut self, mode: &str, cx: &mut Context<Self>) {
        let Some(id) = self.state.current_id.clone() else {
            return;
        };
        if let Some(cfg) = self.session_cfg_by_id.get_mut(&id) {
            if cfg.permission == mode {
                return;
            }
            cfg.permission = mode.to_string();
        } else {
            let mut cfg = self.current_cfg_or_default();
            if cfg.permission == mode {
                return;
            }
            cfg.permission = mode.to_string();
            self.session_cfg_by_id.insert(id, cfg);
        }
        cx.notify();
    }

    /// 切模型(校验宿主模型表;失败落通告)
    pub fn set_session_model(&mut self, model: &str, cx: &mut Context<Self>) {
        let v = model.to_string();
        self.mutate_session_cfg(cx, move |host, id| host.set_model(id, &v));
    }

    /// 模型二级菜单选型:先切工作区默认 provider(跨 provider 时;幂等),
    /// 再设模型——set_model 校验清单并 detach_if_idle,下一轮即走新
    /// provider + 模型
    pub fn set_session_provider_model(
        &mut self,
        provider_id: &str,
        model: &str,
        cx: &mut Context<Self>,
    ) {
        let host = self.bridge.host().clone();
        if let Some(ws) = self
            .state
            .active_workspace
            .clone()
            .or_else(|| host.workspace_names().first().cloned())
        {
            let _ = host.set_workspace_provider(&ws, provider_id);
        }
        self.set_session_model(model, cx);
    }

    /// 模型菜单级联子菜单切换(同子菜单再点 = 收;None = 全收)
    /// 发起 ask_user_question 问答(宿主发射 question/requested 帧后
    /// 阻塞等应答;发射即弃,帧回灌驱动问答卡。桌面端到端测试入口)
    #[cfg(test)]
    pub fn ask_questions_json(
        &mut self,
        session_id: &str,
        questions: Vec<serde_json::Value>,
        cx: &mut Context<Self>,
    ) {
        let host = self.bridge.host().clone();
        let sid = session_id.to_string();
        let rx = self.bridge.call(async move {
            let _ = host.ask_questions_json(&sid, questions).await;
        });
        // 应答接收端丢弃(帧已发射;测试只验证弹卡)
        drop(rx);
        cx.notify();
    }

    pub fn set_composer_submenu(
        &mut self,
        sub: Option<crate::features::chat::ComposerSubmenu>,
        cx: &mut Context<Self>,
    ) {
        self.chat.composer_submenu = sub;
        cx.notify();
    }

    /// 关闭模型菜单(选中后整体收起)
    pub fn close_composer_menu(&mut self, cx: &mut Context<Self>) {
        self.chat.composer_menu = ComposerMenu::None;
        self.chat.composer_submenu = None;
        cx.notify();
    }

    /// 切思考等级(low / high / max)
    pub fn set_session_effort(&mut self, effort: &str, cx: &mut Context<Self>) {
        let v = effort.to_string();
        self.mutate_session_cfg(cx, move |host, id| host.set_effort(id, &v));
    }

    /// 切模式/preset(standard / minimal / 工作区自定义;空闲时生效)
    pub fn set_session_preset(&mut self, preset: &str, cx: &mut Context<Self>) {
        let v = preset.to_string();
        self.mutate_session_cfg(cx, move |host, id| host.set_preset(id, &v));
    }

    /// hero chip 下拉开关(互斥;同菜单再点 = 关)
    pub fn set_hero_menu(&mut self, menu: HeroMenu, cx: &mut Context<Self>) {
        self.hero_menu = if self.hero_menu == menu {
            HeroMenu::None
        } else {
            menu
        };
        cx.notify();
    }

    /// 会话设置公共路径:成功回写缓存,失败本地通告;一律关菜单
    fn mutate_session_cfg(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&dsh_core::registry::AppHost, &str) -> Result<(), dsh_core::proto::RpcError>,
    ) {
        let Some(id) = self.state.current_id.clone() else {
            return;
        };
        if let Err(e) = f(self.bridge.host(), &id) {
            self.push_local_notice(&format!("切换失败:{}", e.message), cx);
        } else {
            self.refresh_session_cfg(&id);
        }
        self.chat.composer_menu = ComposerMenu::None;
        self.hero_menu = HeroMenu::None;
        cx.notify();
    }

    /// 本地通告行(设置失败等;不经 apply_frame,须自增版本驱动
    /// 渲染侧滚动跟随,且 key 须唯一——push_node 按 key 幂等)
    pub fn push_local_notice(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(id) = self.state.current_id.clone() else {
            return;
        };
        self.local_notice_seq += 1;
        let key = format!("notice-local-{}", self.local_notice_seq);
        self.state
            .chats
            .entry(id)
            .or_default()
            .push_node(ChatNode::Notice {
                key,
                text: text.to_string(),
            });
        self.chat.pinned = true;
        self.chat.chat_version += 1;
        cx.notify();
    }

    // ── 工作区整理 ──────────────────────────────────────────

    /// 侧栏折叠切换
    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        cx.notify();
    }

    // 侧栏拖宽(base + dx):锚点 = 光标 x + 起始宽,移动用增量加入
    // 起始宽(向右拖变宽)。宽独立于折叠:折叠不写 0,重开仍用自定宽。

    /// 侧栏拖宽开始(锚点 = 光标 x + 当时宽)
    pub fn sidebar_resize_begin(&mut self, cursor_x: f32, cx: &mut Context<Self>) {
        self.sidebar_resize_anchor = Some((cursor_x, self.sidebar_px));
        cx.notify();
    }

    /// 侧栏拖宽移动(clamp SIDEBAR_MIN..SIDEBAR_MAX)
    pub fn sidebar_resize_move(&mut self, cursor_x: f32, cx: &mut Context<Self>) {
        if let Some((anchor_x, anchor_w)) = self.sidebar_resize_anchor {
            self.sidebar_px =
                crate::shell::metrics::clamp_sidebar(anchor_w + (cursor_x - anchor_x));
            cx.notify();
        }
    }

    /// 侧栏拖宽结束
    pub fn sidebar_resize_end(&mut self, cx: &mut Context<Self>) {
        self.sidebar_resize_anchor = None;
        cx.notify();
    }

    // 右侧面板:手动开关 + 左缘拖宽(向左拖 = 增宽,与侧栏方向相反)。

    /// 面板开关
    pub fn toggle_panel(&mut self, cx: &mut Context<Self>) {
        self.panel_open = !self.panel_open;
        cx.notify();
    }

    // 面板标签页:壳开合与标签
    // 开合解耦——关最后一个标签回空态快捷菜单,面板不自动收;「+」
    // 与空态同份视图清单。

    /// 打开(或激活)面板标签:面板随手开,同类标签去重,激活之。
    /// 切入轨迹即重拉:缓存可能停留在会话空白期(会话在轨迹面板
    /// 打开/新建会话时拉到空台账,此后内容增长不构成「会话失配」),
    /// 无条件刷新;同会话重拉保留选中/折叠态(原主区切入轨迹语义)
    pub fn open_panel_tab(&mut self, tab: PanelTab, cx: &mut Context<Self>) {
        self.panel_open = true;
        if !self.panel_tabs.contains(&tab) {
            self.panel_tabs.push(tab);
        }
        self.panel_active_tab = Some(tab);
        self.panel_plus_menu_at = None;
        if tab == PanelTab::Trajectory {
            self.refresh_trajectory(cx);
        }
        cx.notify();
    }

    /// 关闭面板标签:关的是激活页则激活余下最后一张;无余 = 空态
    /// (panel_open 不动,面板保持开)
    pub fn close_panel_tab(&mut self, tab: PanelTab, cx: &mut Context<Self>) {
        self.panel_tabs.retain(|t| *t != tab);
        if self.panel_active_tab == Some(tab) {
            self.panel_active_tab = self.panel_tabs.last().copied();
        }
        cx.notify();
    }

    /// 激活已开标签(点标签条)。切入轨迹与 open_panel_tab 同语义:
    /// 在别的标签停留期间直播重拉不跑,切回时缓存可能陈旧 → 重拉
    pub fn activate_panel_tab(&mut self, tab: PanelTab, cx: &mut Context<Self>) {
        if self.panel_tabs.contains(&tab) {
            self.panel_active_tab = Some(tab);
            if tab == PanelTab::Trajectory {
                self.refresh_trajectory(cx);
            }
            cx.notify();
        }
    }

    /// 轨迹面板当前可见(激活标签 = 轨迹):直播重拉与切会话刷新的门控判据
    pub fn trajectory_visible(&self) -> bool {
        self.panel_active_tab == Some(PanelTab::Trajectory)
    }

    /// 面板「+」菜单开(坐标锚定,root 级渲染;同行 ⋯ 菜单模式)
    pub fn open_panel_plus_menu_at(
        &mut self,
        pos: gpui_kit::Point<gpui_kit::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.panel_plus_menu_at = Some(pos);
        cx.notify();
    }

    /// 面板拖宽开始(锚点 = 光标 x + 当时宽 + 视口宽)
    pub fn panel_resize_begin(&mut self, cursor_x: f32, viewport_w: f32, cx: &mut Context<Self>) {
        self.panel_resize_anchor = Some((cursor_x, self.panel_px, viewport_w));
        cx.notify();
    }

    /// 面板拖宽移动(向左拖 = 增宽)。上限无固定常量 = 视口 − 侧栏
    /// (当前形态)− 聊天最小宽:先压缩聊天区到最小宽;
    /// 越过当前形态上限自动收起左侧栏释放空间继续;反向缩窄不自动
    /// 展开(避免拖拽振荡)
    pub fn panel_resize_move(&mut self, cursor_x: f32, cx: &mut Context<Self>) {
        let Some((anchor_x, anchor_w, viewport_w)) = self.panel_resize_anchor else {
            return;
        };
        use crate::shell::metrics::panel_limit;
        let raw = anchor_w - (cursor_x - anchor_x);
        if raw > panel_limit(viewport_w, false, self.sidebar_px) && !self.sidebar_collapsed {
            self.sidebar_collapsed = true;
        }
        let limit = panel_limit(viewport_w, self.sidebar_collapsed, self.sidebar_px);
        self.panel_px = raw.clamp(crate::shell::metrics::PANEL_MIN, limit);
        cx.notify();
    }

    /// 面板拖宽结束
    pub fn panel_resize_end(&mut self, cx: &mut Context<Self>) {
        self.panel_resize_anchor = None;
        cx.notify();
    }

    // ── 派生读取(UI 用)────────────────────────────────────────

    /// 当前是否执行中(状态图优先,清单兜底)
    pub fn is_running(&self, id: &str) -> bool {
        self.state
            .running_by_id
            .get(id)
            .copied()
            .unwrap_or_else(|| {
                self.state
                    .sessions
                    .iter()
                    .find(|s| s.session_id == id)
                    .map(|s| s.running)
                    .unwrap_or(false)
            })
    }

    /// 本 turn 已运行时长(起始墙钟 → 现在;仅 running 且有记时起点时 Some)。
    /// 用于标题栏/转写区的实时「N 分钟」显示。
    pub fn run_elapsed(&self, id: &str) -> Option<std::time::Duration> {
        self.state
            .running_since_by_id
            .get(id)
            .map(|since| since.elapsed())
    }

    /// hero 空态:当前会话无节点且不执行中
    pub fn hero(&self) -> bool {
        match self.state.current_id.as_deref() {
            Some(id) => self.is_blank(id) && !self.is_running(id),
            None => true,
        }
    }
}
