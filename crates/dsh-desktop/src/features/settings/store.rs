//! 设置功能的 store 域:设置页路由/快照/onboarding、provider 编辑器
//! (增删改/凭据/探测)、通用区偏好下拉(preset/权限/语言/busy-enter)
//! 与全权确认。视图见 features::settings::views。

use std::collections::{HashMap, HashSet};

use gpui_kit::component::IndexPath;
use gpui_kit::component::input::{EditorState, InputEvent, InputState};
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::{AppContext, Context, Entity, Window};

use crate::shell::store::AppStore;

/// 从端点获取模型的弹层态(候选清单 + 逐项勾选)
pub(crate) struct ModelFetch {
    /// 端点返回的候选模型
    pub candidates: Vec<String>,
    /// 与 candidates 等长的勾选态(默认全勾)
    pub picked: Vec<bool>,
}

/// 设置导航区(设置模式 = 侧栏切换为设置菜单,内容区显示对应页;
/// 插件/MCP/技能待实装后进菜单——入口迁移优于新增,不做空占位)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsNav {
    /// 通用
    General,
    /// 模型与 Provider
    Models,
    /// MCP Servers
    Mcp,
    /// Hooks(Claude Code / Codex 桥)
    Hooks,
    /// 关于
    About,
}

/// 通用区偏好下拉菜单种类(根级坐标锚定,同行菜单模式)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefMenuKind {
    /// Agent 预设
    Preset,
    /// 权限
    Permission,
    /// 语言
    Language,
    /// 繁忙时 Enter 键行为
    BusyEnter,
}

/// 设置功能切片状态(页路由/快照/onboarding/provider 编辑器与删除确认/偏好下拉)。
pub(crate) struct SettingsStore {
    /// 设置页开(独立页路由:右列内容整体切换)
    pub settings_open: bool,
    /// 设置页快档(settings_view 数据;打开与每次变更后刷新)
    pub settings_snapshot: serde_json::Value,
    /// 首运行引导态(未 onboarded 且默认 provider 凭据缺席)
    pub needs_onboarding: bool,
    /// 设置页 provider 表单三输入(挂窗后建;同 id 提交 = 更新)
    pub set_form_id: Option<Entity<InputState>>,
    /// provider 表单 base_url 输入
    pub set_form_url: Option<Entity<InputState>>,
    /// provider 表单默认模型输入
    pub set_form_model: Option<Entity<InputState>>,
    /// provider 表单方言选择(chips 三选一)
    pub set_form_dialect: String,
    /// 设置页导航(两栏壳:左 nav + 单区内容)
    pub settings_nav: SettingsNav,
    /// MCP 分区详情页(None = 列表页;Some = 详情:新增/编辑/JSON 导入)
    pub mcp_detail: Option<McpDetailState>,
    /// Hooks 分区详情页(None = 列表页;Some = 新增/编辑表单)
    pub hooks_detail: Option<HooksDetailState>,
    /// MCP server 最近连接状态(server id → (status, error);mcp/status 帧维护)
    pub mcp_status_by_id: HashMap<String, (String, String)>,
    /// 行内编辑中的 provider id(编辑卡在行卡内展开)
    pub editing_provider: Option<String>,
    /// 添加卡开态
    pub adding_provider: bool,
    /// 首运行 setup 卡已手动关闭的 provider(本会话内回退普通行)
    pub dismissed_setup: HashSet<String>,
    /// 编辑卡 API key 输入(write-only;应用时随条目存 settings)
    pub key_input: Option<Entity<InputState>>,
    /// provider 表单显示名输入
    pub set_form_name: Option<Entity<InputState>>,
    /// provider 表单模型清单草稿(表单内增删,应用时落盘)
    pub set_form_models: Vec<String>,
    /// 手动添加模型的单行输入
    pub set_form_model_input: Option<Entity<InputState>>,
    /// 计费端点启用(表单开关)
    pub set_form_billing_enabled: bool,
    /// 计费形态(balance / usage)
    pub set_form_billing_kind: String,
    /// 计费 URL 输入
    pub set_form_billing_url: Option<Entity<InputState>>,
    /// 计费 JSON 路径输入:余额金额 / 货币
    pub set_form_path_balance: Option<Entity<InputState>>,
    pub set_form_path_currency: Option<Entity<InputState>>,
    /// 计费 JSON 路径输入:5小时 / 7天 / 重置
    pub set_form_path_5h: Option<Entity<InputState>>,
    pub set_form_path_7d: Option<Entity<InputState>>,
    pub set_form_path_resets: Option<Entity<InputState>>,
    /// 从端点获取模型的弹层(Some = 开):候选 + 逐项勾选态
    pub model_fetch: Option<ModelFetch>,
    /// 模型拉取进行中(弹层内「获取中」态)
    pub model_fetch_loading: bool,
    /// 计费刷新中的 provider(刷新钮禁用态)
    pub billing_refreshing: Option<String>,
    /// 额度自动刷新进行中(静默路径单飞;与手动 billing_refreshing 互斥跳过)
    pub billing_auto_running: bool,
    /// 额度自动刷新上次尝试时刻(turn/end 防抖基线;失败也记,防抖不追打)
    pub billing_auto_last: Option<std::time::Instant>,
    /// 保存通告(源 savedNotice:应用成功后一行 success 文案)
    pub saved_provider_notice: Option<String>,
    /// 待确认删除的 provider
    pub delete_provider_target: Option<String>,
    /// 设置页内通告槽(单槽覆盖,不堆积;(成功?, 文案)):计费查询、
    /// 保存失败等设置动作的反馈——**不走聊天区** push_local_notice。
    /// 4s 自动清除(notice_seq 守卫防误清新通告)
    pub settings_notice: Option<(bool, String)>,
    /// 通告代次(每次置新通告 +1;清除任务比对丢弃过期)
    pub settings_notice_seq: u64,
    /// 保存通告的清除任务同款瞬态(「已保存 X」不常驻)
    /// 通用区偏好下拉(gpui-component Select;挂窗后建,Confirm 落盘)
    pub preset_select: Option<Entity<SelectState<Vec<gpui_kit::SharedString>>>>,
    /// 权限下拉
    pub permission_select: Option<Entity<SelectState<Vec<gpui_kit::SharedString>>>>,
    /// 语言下拉
    pub language_select: Option<Entity<SelectState<Vec<gpui_kit::SharedString>>>>,
    /// 繁忙时 Enter 键行为下拉
    pub busy_enter_select: Option<Entity<SelectState<Vec<gpui_kit::SharedString>>>>,
    /// 权限选 full-access 的风险确认(源 RiskConfirmation 对应物)。
    /// Some 记录确认来源:设置页默认预设 / composer 会话权限——确认后
    /// 各自落不同的目标(默认预设落盘 / 会话 set_permission)
    pub full_access_confirm: Option<FullAccessAsk>,
}

/// full-access 风险确认的来源(确认动作随来源分流)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullAccessAsk {
    /// 设置页「默认权限预设」选完全权限:确认 = 落盘默认预设
    Default,
    /// composer 权限菜单选完全权限:确认 = 会话内 set_permission
    Session,
}

impl Default for SettingsStore {
    fn default() -> Self {
        Self {
            settings_open: false,
            settings_snapshot: serde_json::Value::Null,
            needs_onboarding: false,
            set_form_id: None,
            set_form_url: None,
            set_form_model: None,
            set_form_dialect: "openai-chat".into(),
            settings_nav: SettingsNav::Models,
            mcp_detail: None,
            hooks_detail: None,
            mcp_status_by_id: HashMap::new(),
            editing_provider: None,
            adding_provider: false,
            dismissed_setup: HashSet::new(),
            key_input: None,
            set_form_name: None,
            set_form_models: Vec::new(),
            set_form_model_input: None,
            set_form_billing_enabled: false,
            set_form_billing_kind: "balance".into(),
            set_form_billing_url: None,
            set_form_path_balance: None,
            set_form_path_currency: None,
            set_form_path_5h: None,
            set_form_path_7d: None,
            set_form_path_resets: None,
            model_fetch: None,
            model_fetch_loading: false,
            billing_refreshing: None,
            billing_auto_running: false,
            billing_auto_last: None,
            saved_provider_notice: None,
            delete_provider_target: None,
            settings_notice: None,
            settings_notice_seq: 0,
            preset_select: None,
            permission_select: None,
            language_select: None,
            busy_enter_select: None,
            full_access_confirm: None,
        }
    }
}

impl AppStore {
    /// 设置页 provider 表单三输入懒建(Enter 提交;同 id = 更新;挂窗态由
    /// store::attach_window_state 调用)
    pub(crate) fn ensure_provider_form_inputs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 设置页 provider 表单三输入(Enter 提交;同 id = 更新)
        if self.settings.set_form_id.is_none() {
            self.settings.set_form_id =
                Some(cx.new(|cx| InputState::new(window, cx).placeholder("小写英文/连字符")));
        }
        if self.settings.set_form_url.is_none() {
            self.settings.set_form_url =
                Some(cx.new(|cx| {
                    InputState::new(window, cx).placeholder("https://api.example.com/v1")
                }));
        }
        if self.settings.set_form_model.is_none() {
            self.settings.set_form_model =
                Some(cx.new(|cx| InputState::new(window, cx).placeholder("可选")));
        }
        if self.settings.set_form_name.is_none() {
            self.settings.set_form_name = Some(
                cx.new(|cx| InputState::new(window, cx).placeholder("给这个 Provider 起个名字")),
            );
        }
        if self.settings.set_form_model_input.is_none() {
            self.settings.set_form_model_input =
                Some(cx.new(|cx| InputState::new(window, cx).placeholder("模型 id")));
        }
        if self.settings.set_form_billing_url.is_none() {
            self.settings.set_form_billing_url =
                Some(cx.new(|cx| InputState::new(window, cx).placeholder("https://…")));
        }
        for (slot, ph) in [
            (
                &mut self.settings.set_form_path_balance,
                "余额金额路径,如 balance_infos.0.total_balance",
            ),
            (
                &mut self.settings.set_form_path_currency,
                "货币路径,如 balance_infos.0.currency",
            ),
            (
                &mut self.settings.set_form_path_5h,
                "5小时用量路径,如 five_hour.utilization",
            ),
            (
                &mut self.settings.set_form_path_7d,
                "7天用量路径,如 seven_day.utilization",
            ),
            (
                &mut self.settings.set_form_path_resets,
                "重置时间路径,如 resets_in",
            ),
        ] {
            if slot.is_none() {
                *slot = Some(cx.new(|cx| InputState::new(window, cx).placeholder(ph)));
            }
        }
        self.ensure_pref_selects(window, cx);
        for input in [
            &self.settings.set_form_id,
            &self.settings.set_form_url,
            &self.settings.set_form_model,
        ]
        .into_iter()
        .flatten()
        {
            cx.subscribe(input, |this, _i, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.apply_provider_editor(cx);
                }
            })
            .detach();
        }
    }

    /// 通用区偏好下拉构建(gpui-component Select;Confirm → 按 label 映射
    /// id 落盘。full-access 经风险确认,取消时回滚显示)
    pub(crate) fn ensure_pref_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.settings.settings_snapshot.clone();
        let preset_options: Vec<(String, String)> = snapshot["presetOptions"]
            .as_array()
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
            .unwrap_or_default();
        let permission_options: Vec<(String, String)> = snapshot["permissionOptions"]
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
        let language_options = vec![("zh".to_string(), "中文".to_string())];
        let busy_options = vec![
            ("queue".to_string(), "排队发送".to_string()),
            ("steer".to_string(), "插话发送".to_string()),
        ];
        self.settings.preset_select = Some(Self::build_pref_select(
            preset_options,
            snapshot["defaultPreset"].as_str().unwrap_or("standard"),
            PrefMenuKind::Preset,
            window,
            cx,
        ));
        self.settings.permission_select = Some(Self::build_pref_select(
            permission_options,
            snapshot["defaultPermission"]
                .as_str()
                .unwrap_or("workspace-write"),
            PrefMenuKind::Permission,
            window,
            cx,
        ));
        self.settings.language_select = Some(Self::build_pref_select(
            language_options,
            snapshot["language"].as_str().unwrap_or("zh"),
            PrefMenuKind::Language,
            window,
            cx,
        ));
        self.settings.busy_enter_select = Some(Self::build_pref_select(
            busy_options,
            snapshot["busyEnter"].as_str().unwrap_or("queue"),
            PrefMenuKind::BusyEnter,
            window,
            cx,
        ));
    }

    /// 单个偏好 Select 构建(labels + 当前项 + Confirm 落盘订阅)
    fn build_pref_select(
        options: Vec<(String, String)>,
        current: &str,
        kind: PrefMenuKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SelectState<Vec<gpui_kit::SharedString>>> {
        let labels: Vec<gpui_kit::SharedString> = options
            .iter()
            .map(|(_, l)| gpui_kit::SharedString::from(l.clone()))
            .collect();
        let index = options
            .iter()
            .position(|(id, _)| id == current)
            .map(|ix| IndexPath::default().row(ix));
        let state = cx.new(|cx| SelectState::new(labels, index, window, cx));
        cx.subscribe(
            &state,
            move |this, _s, event: &SelectEvent<Vec<gpui_kit::SharedString>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = event {
                    let label_s = label.to_string();
                    if let Some((id, _)) = options.iter().find(|(_, l)| *l == label_s) {
                        let id = id.clone();
                        match kind {
                            PrefMenuKind::Preset => this.set_default_preset(&id, cx),
                            PrefMenuKind::Permission => this.set_default_permission(&id, cx),
                            PrefMenuKind::Language => this.set_language(&id, cx),
                            PrefMenuKind::BusyEnter => this.set_busy_enter(&id, cx),
                        }
                    }
                }
            },
        )
        .detach();
        state
    }
    /// 设置页开关(独立页路由;打开时刷新快照与 onboarding 态)
    pub fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.settings.settings_open = !self.settings.settings_open;
        if self.settings.settings_open {
            self.settings.settings_snapshot = self.bridge.host().settings_view();
            self.recalc_onboarding();
        }
        cx.notify();
    }

    /// onboarding 重估:凭据已就绪但未标记 → 顺手完成引导
    pub fn recalc_onboarding(&mut self) {
        let onboarded = self.settings.settings_snapshot["onboarded"]
            .as_bool()
            .unwrap_or(false);
        let default_provider = self.settings.settings_snapshot["defaultProvider"]
            .as_str()
            .unwrap_or("deepseek")
            .to_string();
        self.settings.needs_onboarding =
            !onboarded && !self.bridge.host().credential_status(&default_provider);
        if !self.settings.needs_onboarding && !onboarded {
            let _ = self.bridge.host().set_onboarded();
        }
    }

    /// 切换默认 preset(通用区 Agent 预设行;落盘)
    pub fn set_default_preset(&mut self, id: &str, cx: &mut Context<Self>) {
        match self.bridge.host().set_default_preset(id) {
            Ok(()) => {
                self.settings_refresh(cx);
            }
            Err(e) => {
                self.set_settings_notice(false, format!("保存失败:{}", e.message), cx);
            }
        }
    }

    /// 切换默认权限预设(通用区权限行;full-access 先经风险确认)。
    /// 落盘为工作区默认权限预设,新会话 pin 时沿用。
    pub fn set_default_permission(&mut self, id: &str, cx: &mut Context<Self>) {
        if id == "full-access" {
            self.settings.full_access_confirm = Some(FullAccessAsk::Default);
            cx.notify();
            return;
        }
        match self.bridge.host().set_default_permission_preset(id) {
            Ok(()) => {
                self.settings_refresh(cx);
            }
            Err(e) => {
                self.set_settings_notice(false, format!("保存失败:{}", e.message), cx);
            }
        }
    }

    /// 确认 full-access(风险确认后按来源分流:默认预设落盘 /
    /// composer 会话权限切换)
    pub fn confirm_full_access(&mut self, cx: &mut Context<Self>) {
        let ask = self.settings.full_access_confirm.take();
        if ask == Some(FullAccessAsk::Session) {
            self.set_session_permission("full-access", cx);
            return;
        }
        if let Err(e) = self
            .bridge
            .host()
            .set_default_permission_preset("full-access")
        {
            self.push_local_notice(&format!("保存失败:{}", e.message), cx);
            return;
        }
        self.settings_refresh(cx);
    }

    /// 取消 full-access 确认(设置页来源时 Select 显示回滚到实际值;
    /// composer 来源无 Select,仅关弹窗)
    pub fn cancel_full_access(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.full_access_confirm = None;
        let actual = self.settings.settings_snapshot["defaultPermission"]
            .as_str()
            .unwrap_or("workspace-write");
        let label = match actual {
            "read-only" => "仅可查看",
            "workspace-write" => "工作区内修改",
            "full-access" => "完全权限",
            other => other,
        };
        if let Some(select) = &self.settings.permission_select {
            let v = gpui_kit::SharedString::from(label.to_string());
            select.update(cx, |s, cx| s.set_selected_value(&v, window, cx));
        }
        cx.notify();
    }

    /// 切换界面语言偏好(落盘;RS 现仅 zh)
    pub fn set_language(&mut self, id: &str, cx: &mut Context<Self>) {
        match self.bridge.host().set_language(id) {
            Ok(()) => {
                self.settings_refresh(cx);
            }
            Err(e) => {
                self.set_settings_notice(false, format!("保存失败:{}", e.message), cx);
            }
        }
    }

    /// 切换外观偏好(light / dark / system;落盘)。返回是否成功——
    /// 主题切换由调用方在持 window 的点击闭包里做(theme::apply)。
    pub fn set_appearance(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        match self.bridge.host().set_appearance(id) {
            Ok(()) => {
                self.settings_refresh(cx);
                true
            }
            Err(e) => {
                self.set_settings_notice(false, format!("保存失败:{}", e.message), cx);
                false
            }
        }
    }

    /// 切换繁忙时 Enter 键行为偏好(通用区行;落盘)
    pub fn set_busy_enter(&mut self, behavior: &str, cx: &mut Context<Self>) {
        match self.bridge.host().set_busy_enter(behavior) {
            Ok(()) => {
                self.settings_refresh(cx);
            }
            Err(e) => {
                self.set_settings_notice(false, format!("保存失败:{}", e.message), cx);
            }
        }
    }

    /// 设置快照重拉 + onboarding 重估
    pub fn settings_refresh(&mut self, cx: &mut Context<Self>) {
        self.settings.settings_snapshot = self.bridge.host().settings_view();
        self.recalc_onboarding();
        cx.notify();
    }

    /// 切换设置页导航区(源两栏壳:左 nav + 单区内容)
    pub fn set_settings_nav(&mut self, nav: SettingsNav, cx: &mut Context<Self>) {
        self.settings.settings_nav = nav;
        cx.notify();
    }

    /// 该 provider 是否处于首运行 setup 姿态:尚无可服务 provider 且
    /// 默认 provider 未配置凭据(源 needsSetup:setup 卡即其在页面上
    /// 的存在形式,直到用户关闭)
    fn provider_needs_setup(&self, id: &str) -> bool {
        let rows = self.settings.settings_snapshot["providers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let any_ready = rows
            .iter()
            .any(|p| p["credentialReady"].as_bool().unwrap_or(false));
        let me_ready = rows
            .iter()
            .find(|p| p["id"].as_str() == Some(id))
            .and_then(|p| p["credentialReady"].as_bool())
            .unwrap_or(true);
        !any_ready
            && !me_ready
            && self.settings.settings_snapshot["defaultProvider"].as_str() == Some(id)
    }

    /// 渲染期 setup 姿态判定(需 setup 且未被手动关闭)
    pub fn provider_setup_posture(&self, id: &str) -> bool {
        self.provider_needs_setup(id) && !self.settings.dismissed_setup.contains(id)
    }

    /// 打开行内编辑卡(源:编辑卡在行卡内展开;预填自定义字段,key 清空)
    pub fn open_provider_editor(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.fill_provider_form(Some(id), window, cx);
        self.ensure_key_input(window, cx);
        self.settings.editing_provider = Some(id.to_string());
        self.settings.adding_provider = false;
        self.settings.saved_provider_notice = None;
        cx.notify();
    }

    /// 打开添加卡(空表单)
    pub fn open_provider_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.fill_provider_form(None, window, cx);
        self.ensure_key_input(window, cx);
        self.settings.editing_provider = None;
        self.settings.adding_provider = true;
        self.settings.saved_provider_notice = None;
        cx.notify();
    }

    /// API key 输入惰建(编辑卡主字段;write-only)
    fn ensure_key_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.key_input.is_none() {
            self.settings.key_input =
                Some(cx.new(|cx| {
                    InputState::new(window, cx).placeholder("sk-…(留空 = 保持既有凭据)")
                }));
        }
        if let Some(input) = &self.settings.key_input {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
    }

    /// 关闭编辑/添加卡(setup 姿态的关闭 = dismiss,本会话回退普通行;
    /// setup 卡渲染不经 editing_provider,故须显式携带目标 id)
    pub fn close_provider_editor(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.settings.editing_provider.as_deref() == Some(id) {
            self.settings.editing_provider = None;
        }
        if !id.is_empty() && self.provider_needs_setup(id) {
            self.settings.dismissed_setup.insert(id.to_string());
        }
        self.settings.adding_provider = false;
        cx.notify();
    }

    /// 应用编辑/添加卡:key 非空随条目直存 settings(provider `api_key`);
    /// 留空 = 保留已存值。成功 → 保存通告 + 静默探测 + 刷新 + 关卡
    pub fn apply_provider_editor(&mut self, cx: &mut Context<Self>) {
        let id = if let Some(id) = &self.settings.editing_provider {
            id.clone()
        } else if self.settings.adding_provider {
            self.settings
                .set_form_id
                .as_ref()
                .map(|i| i.read(cx).value().trim().to_string())
                .unwrap_or_default()
        } else {
            return;
        };
        if id.is_empty() {
            self.push_local_notice("provider id 不可为空", cx);
            return;
        }
        let key = self
            .settings
            .key_input
            .as_ref()
            .map(|i| i.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let existing = self.settings.settings_snapshot["providers"]
            .as_array()
            .and_then(|ps| {
                ps.iter()
                    .find(|p| p["id"].as_str() == Some(id.as_str()))
                    .cloned()
            });
        let existing_ref = existing
            .as_ref()
            .and_then(|p| p["credential_ref"].as_str().map(String::from));
        // 输入留空 = 不改已存 key(None = 保留,write-only 语义;
        // 视图不回明文,不存在「从快照回填」)
        let api_key = (!key.is_empty()).then_some(key);
        let model = self
            .settings
            .set_form_model
            .as_ref()
            .map(|i| i.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let entry = dsh_core::settings::ProviderEntry {
            id: id.clone(),
            base_url: self
                .settings
                .set_form_url
                .as_ref()
                .map(|i| i.read(cx).value().trim().to_string())
                .unwrap_or_default(),
            dialect: self.settings.set_form_dialect.clone(),
            credential_ref: existing_ref,
            api_key,
            default_model: if model.is_empty() { None } else { Some(model) },
            display_name: self
                .settings
                .set_form_name
                .as_ref()
                .map(|i| i.read(cx).value().trim().to_string())
                .filter(|v| !v.is_empty()),
            models: self.settings.set_form_models.clone(),
            billing: self.form_billing_config(cx),
            billing_cache: self.settings.settings_snapshot["providers"]
                .as_array()
                .and_then(|ps| {
                    ps.iter()
                        .find(|p| p["id"].as_str() == Some(id.as_str()))
                        .and_then(|p| serde_json::from_value(p["billing_cache"].clone()).ok())
                }),
        };
        match self.bridge.host().upsert_provider(entry) {
            Ok(()) => {
                self.settings.editing_provider = None;
                self.settings.adding_provider = false;
                self.settings.saved_provider_notice = Some(id.clone());
                let saved = id.clone();
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(4000))
                        .await;
                    this.update(cx, |s, cx| {
                        if s.settings.saved_provider_notice.as_deref() == Some(saved.as_str()) {
                            s.settings.saved_provider_notice = None;
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
                self.settings_refresh(cx);
                self.probe_models_quietly(&id, cx);
            }
            Err(e) => {
                self.set_settings_notice(false, format!("保存失败:{}", e.message), cx);
            }
        }
    }

    /// 表单计费区 → BillingConfig(未启用 = None;URL 空 = None)
    fn form_billing_config(&self, cx: &Context<Self>) -> Option<dsh_core::settings::BillingConfig> {
        if !self.settings.set_form_billing_enabled {
            return None;
        }
        let val = |slot: &Option<Entity<InputState>>| -> String {
            slot.as_ref()
                .map(|i| i.read(cx).value().trim().to_string())
                .unwrap_or_default()
        };
        let url = val(&self.settings.set_form_billing_url);
        if url.is_empty() {
            return None;
        }
        let opt_path = |slot: &Option<Entity<InputState>>| -> Option<String> {
            let v = val(slot);
            (!v.is_empty()).then_some(v)
        };
        Some(dsh_core::settings::BillingConfig {
            kind: if self.settings.set_form_billing_kind == "usage" {
                dsh_core::settings::BillingKind::Usage
            } else {
                dsh_core::settings::BillingKind::Balance
            },
            url,
            paths: dsh_core::settings::BillingPaths {
                balance: opt_path(&self.settings.set_form_path_balance),
                currency: opt_path(&self.settings.set_form_path_currency),
                usage_5h: opt_path(&self.settings.set_form_path_5h),
                usage_7d: opt_path(&self.settings.set_form_path_7d),
                resets: opt_path(&self.settings.set_form_path_resets),
            },
        })
    }

    /// 设置页内通告(单槽覆盖;ok = 绿色成功 / 否则红色失败)。
    /// **4s 自动清除**——反馈的持久形态在数据本身(计费行/卡片),
    /// 通告只是瞬态提示,不留常驻
    pub(crate) fn set_settings_notice(
        &mut self,
        ok: bool,
        msg: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.settings.settings_notice_seq = self.settings.settings_notice_seq.wrapping_add(1);
        let seq = self.settings.settings_notice_seq;
        self.settings.settings_notice = Some((ok, msg.into()));
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(4000))
                .await;
            this.update(cx, |s, cx| {
                if s.settings.settings_notice_seq == seq {
                    s.settings.settings_notice = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// 计费端点启用开关
    pub fn toggle_billing_enabled(&mut self, cx: &mut Context<Self>) {
        self.settings.set_form_billing_enabled = !self.settings.set_form_billing_enabled;
        cx.notify();
    }

    /// 计费形态切换(balance / usage)
    pub fn set_billing_kind(&mut self, kind: &str, cx: &mut Context<Self>) {
        self.settings.set_form_billing_kind = kind.to_string();
        cx.notify();
    }

    /// 手动添加模型(输入非空且不重复才入草稿;成功清输入)
    pub fn add_model_manual(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.settings.set_form_model_input else {
            return;
        };
        let v = input.read(cx).value().trim().to_string();
        if v.is_empty() || self.settings.set_form_models.contains(&v) {
            return;
        }
        self.settings.set_form_models.push(v);
        if let Some(input) = &self.settings.set_form_model_input {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        cx.notify();
    }

    /// 移除草稿清单中的模型
    pub fn remove_form_model(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.settings.set_form_models.len() {
            self.settings.set_form_models.remove(ix);
            cx.notify();
        }
    }

    /// 从端点拉取可用模型:base_url/方言取表单;key = 表单值(空 = 交给
    /// host 按既有凭据链解析,仅已保存 provider 生效)。结果进弹层多选
    pub fn open_fetch_models(&mut self, provider_id: &str, cx: &mut Context<Self>) {
        let base_url = self
            .settings
            .set_form_url
            .as_ref()
            .map(|i| i.read(cx).value().trim().to_string())
            .unwrap_or_default();
        if base_url.is_empty() {
            self.set_settings_notice(false, "先填写 Base URL 再获取模型", cx);
            return;
        }
        let dialect = self.settings.set_form_dialect.clone();
        let key = self
            .settings
            .key_input
            .as_ref()
            .map(|i| i.read(cx).value().trim().to_string())
            .filter(|v| !v.is_empty());
        let pid = provider_id.to_string();
        self.settings.model_fetch_loading = true;
        self.settings.model_fetch = Some(ModelFetch {
            candidates: Vec::new(),
            picked: Vec::new(),
        });
        cx.notify();
        let store = cx.entity().clone();
        let host = self.bridge.host().clone();
        let rx = self.bridge.call(async move {
            host.discover_models(base_url, dialect, key, Some(pid.clone()))
                .await
        });
        cx.spawn(async move |_this, cx| {
            let candidates = rx.await.unwrap_or_default();
            store.update(cx, |s, _cx| {
                if let Some(mf) = &mut s.settings.model_fetch {
                    mf.candidates = candidates.clone();
                    mf.picked = vec![true; candidates.len()];
                }
                s.settings.model_fetch_loading = false;
            });
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 勾选/取消候选模型
    pub fn toggle_fetch_pick(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(mf) = &mut self.settings.model_fetch
            && ix < mf.picked.len()
        {
            mf.picked[ix] = !mf.picked[ix];
            cx.notify();
        }
    }

    /// 采纳勾选候选入草稿清单(去重),收弹层
    pub fn adopt_fetched_models(&mut self, cx: &mut Context<Self>) {
        if let Some(mf) = self.settings.model_fetch.take() {
            for (c, pick) in mf.candidates.iter().zip(&mf.picked) {
                if *pick && !self.settings.set_form_models.contains(c) {
                    self.settings.set_form_models.push(c.clone());
                }
            }
        }
        cx.notify();
    }

    /// 关闭获取弹层
    pub fn close_fetch_modal(&mut self, cx: &mut Context<Self>) {
        self.settings.model_fetch = None;
        self.settings.model_fetch_loading = false;
        cx.notify();
    }

    /// 额度自动刷新触发(turn/end;60s 防抖——用量刚消耗完就近补一查,
    /// 猝发 turn 不追打)
    pub fn auto_refresh_billing(&mut self, cx: &mut Context<Self>) {
        self.refresh_billing_auto_inner(false, cx);
    }

    /// 额度静默自动刷新(自定时机:启动一次 + 5min 节拍 force + turn/end
    /// 防抖)。只刷 default provider(徽标唯一数据源);未配计费端点整轮
    /// 跳过。静默纪律:不落设置页通告、不点亮手动刷新钮 spinner;手动
    /// (billing_refreshing)进行中跳过本轮。失败不提示,记尝试时刻防抖
    fn refresh_billing_auto_inner(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.settings.billing_refreshing.is_some() || self.settings.billing_auto_running {
            return;
        }
        if !force
            && self
                .settings
                .billing_auto_last
                .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(60))
        {
            return;
        }
        let snap = &self.settings.settings_snapshot;
        let Some(pid) = snap["defaultProvider"].as_str().map(str::to_string) else {
            return;
        };
        let configured = snap["providers"].as_array().is_some_and(|ps| {
            ps.iter()
                .any(|p| p["id"].as_str() == Some(pid.as_str()) && p["billing"].is_object())
        });
        if !configured {
            return;
        }
        self.settings.billing_auto_running = true;
        self.settings.billing_auto_last = Some(std::time::Instant::now());
        cx.notify();
        let store = cx.entity().clone();
        let host = self.bridge.host().clone();
        let rx = self
            .bridge
            .call(async move { host.fetch_billing(&pid).await });
        cx.spawn(async move |_this, cx| {
            let result = rx.await.unwrap_or_else(|e| Err(format!("{e}")));
            store.update(cx, |s, cx| {
                s.settings.billing_auto_running = false;
                if result.is_ok() {
                    s.settings_refresh(cx);
                }
            });
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 额度自动刷新节拍(挂窗一次):启动即查一次,此后每 5min 一轮
    pub fn start_billing_tick(&mut self, cx: &mut Context<Self>) {
        if self.billing_tick.is_some() {
            return;
        }
        self.refresh_billing_auto_inner(true, cx);
        self.billing_tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(300))
                    .await;
                let _ = this.update(cx, |s, cx| s.refresh_billing_auto_inner(true, cx));
            }
        }));
    }

    /// 立即刷新计费。use_form = 编辑器内按钮(**表单当前值**试查,未
    /// 应用也能刷;成功写回该 provider 的 billing_cache);false = 卡片
    /// 刷新钮走已保存配置。结果均刷新快照
    pub fn refresh_billing_now(
        &mut self,
        provider_id: &str,
        use_form: bool,
        cx: &mut Context<Self>,
    ) {
        if self.settings.billing_refreshing.is_some() {
            return;
        }
        let pid = provider_id.to_string();
        let cfg = if use_form {
            self.form_billing_config(cx)
        } else {
            None
        };
        let form_key = self
            .settings
            .key_input
            .as_ref()
            .map(|i| i.read(cx).value().trim().to_string())
            .filter(|v| !v.is_empty());
        let dialect = self.settings.set_form_dialect.clone();
        self.settings.billing_refreshing = Some(pid.clone());
        cx.notify();
        let store = cx.entity().clone();
        let host = self.bridge.host().clone();
        let rx = self.bridge.call(async move {
            match cfg {
                Some(cfg) => {
                    let snap = host
                        .test_billing(cfg, dialect, form_key, Some(pid.clone()))
                        .await?;
                    host.set_billing_cache(&pid, snap).map_err(|e| e.message)?;
                    Ok(())
                }
                None => host.fetch_billing(&pid).await,
            }
        });
        cx.spawn(async move |_this, cx| {
            let result = rx.await.unwrap_or_else(|e| Err(format!("{e}")));
            store.update(cx, |s, cx| {
                s.settings.billing_refreshing = None;
                match result {
                    Ok(()) => {
                        s.set_settings_notice(true, "计费已更新", cx);
                        s.settings_refresh(cx);
                    }
                    Err(msg) => s.set_settings_notice(false, format!("计费查询失败:{msg}"), cx),
                }
            });
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 静默后台探测 provider 模型清单(upsert 清缓存后无自动重探路径;
    /// 应用编辑卡后触发,结果随下次设置刷新可见,失败不提示)
    fn probe_models_quietly(&mut self, provider_id: &str, cx: &mut Context<Self>) {
        let store = cx.entity().clone();
        let host = self.bridge.host().clone();
        let pid = provider_id.to_string();
        let rx = self
            .bridge
            .call(async move { host.refresh_models(&pid).await });
        cx.spawn(async move |_this, cx| {
            let _ = rx.await;
            store.update(cx, |s, cx| s.settings_refresh(cx));
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 打开 provider 删除确认(源:移除先经确认模态)
    pub fn ask_delete_provider(&mut self, id: &str, cx: &mut Context<Self>) {
        self.settings.saved_provider_notice = None;
        self.settings.delete_provider_target = Some(id.to_string());
        cx.notify();
    }

    /// 取消 provider 删除
    pub fn cancel_delete_provider(&mut self, cx: &mut Context<Self>) {
        self.settings.delete_provider_target = None;
        cx.notify();
    }

    /// 确认删除 provider(凭据记录是用户资产,不随删)
    pub fn confirm_delete_provider(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.settings.delete_provider_target.take() else {
            return;
        };
        if self.settings.editing_provider.as_deref() == Some(id.as_str()) {
            self.settings.editing_provider = None;
        }
        match self.bridge.host().remove_provider(&id) {
            Ok(()) => {
                self.settings.saved_provider_notice = None;
                self.settings_refresh(cx);
            }
            Err(e) => self.push_local_notice(&format!("删除失败:{}", e.message), cx),
        }
    }

    /// 表单预填(编辑态取快照字段;添加态清空 + 方言回默认)
    fn fill_provider_form(
        &mut self,
        id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = id.and_then(|id| {
            self.settings.settings_snapshot["providers"]
                .as_array()
                .and_then(|ps| {
                    ps.iter().find(|p| p["id"].as_str() == Some(id)).map(|p| {
                        (
                            p["base_url"].as_str().unwrap_or_default().to_string(),
                            p["dialect"].as_str().unwrap_or("openai-chat").to_string(),
                            p["default_model"].as_str().unwrap_or_default().to_string(),
                            p["display_name"].as_str().unwrap_or_default().to_string(),
                            p["models"].as_array().cloned().unwrap_or_default(),
                            p["billing"].clone(),
                        )
                    })
                })
        });
        let (url, dialect, model, name, models, billing) = entry.unwrap_or_else(|| {
            (
                String::new(),
                "openai-chat".into(),
                String::new(),
                String::new(),
                Vec::new(),
                serde_json::Value::Null,
            )
        });
        self.settings.set_form_dialect = dialect;
        self.settings.set_form_models = models
            .iter()
            .filter_map(|m| m.as_str().map(String::from))
            .collect();
        // 计费表单回填(快照字段 = ProviderEntry serde 直出)
        let billing_kind = billing["kind"].as_str().unwrap_or("balance").to_string();
        let billing_url = billing["url"].as_str().unwrap_or_default().to_string();
        let paths = |k: &str| billing["paths"][k].as_str().unwrap_or_default().to_string();
        self.settings.set_form_billing_enabled = billing.is_object();
        self.settings.set_form_billing_kind = billing_kind;
        let put = |slot: &mut Option<Entity<InputState>>,
                   v: String,
                   window: &mut Window,
                   cx: &mut Context<Self>| {
            if let Some(input) = slot {
                input.update(cx, |s, cx| s.set_value(&v, window, cx));
            }
        };
        let id_value = id.unwrap_or_default().to_string();
        put(&mut self.settings.set_form_id, id_value, window, cx);
        put(&mut self.settings.set_form_url, url, window, cx);
        put(&mut self.settings.set_form_model, model, window, cx);
        put(&mut self.settings.set_form_name, name, window, cx);
        put(
            &mut self.settings.set_form_model_input,
            String::new(),
            window,
            cx,
        );
        put(
            &mut self.settings.set_form_billing_url,
            billing_url,
            window,
            cx,
        );
        put(
            &mut self.settings.set_form_path_balance,
            paths("balance"),
            window,
            cx,
        );
        put(
            &mut self.settings.set_form_path_currency,
            paths("currency"),
            window,
            cx,
        );
        put(
            &mut self.settings.set_form_path_5h,
            paths("usage_5h"),
            window,
            cx,
        );
        put(
            &mut self.settings.set_form_path_7d,
            paths("usage_7d"),
            window,
            cx,
        );
        put(
            &mut self.settings.set_form_path_resets,
            paths("resets"),
            window,
            cx,
        );
    }
}

/// MCP 详情页状态(新增 / 编辑 / JSON 导入三态共用载体)
#[derive(Clone)]
pub struct McpDetailState {
    /// 编辑中的 server id(None = 新增;编辑态 id 锁定)
    pub editing: Option<String>,
    /// 页签:表单 / JSON 粘贴
    pub mode: McpDetailMode,
    /// 启用开关表单值
    pub form_enabled: bool,
    /// 表单:id(仅新增可输入;编辑态身份锁定)
    pub form_id: Option<Entity<InputState>>,
    pub form_command: Option<Entity<InputState>>,
    pub form_cwd: Option<Entity<InputState>>,
    /// 参数(每参数一条;空格分隔单行会吞含空格的参数)
    pub form_args: Vec<Entity<InputState>>,
    /// 单次调用超时 MS(空 = 默认 60000)
    pub form_timeout: Option<Entity<InputState>>,
    /// 动态环境变量键值对列表
    pub form_env: Vec<(Entity<InputState>, Entity<InputState>)>,
    /// 传输形态(true = streamable-http,false = stdio)
    pub form_http: bool,
    /// http endpoint URL
    pub form_url: Option<Entity<InputState>>,
    /// http 附加请求头键值对列表(原样透传,如 Authorization)
    pub form_headers: Vec<(Entity<InputState>, Entity<InputState>)>,
    /// JSON 粘贴区输入
    pub json_input: Option<Entity<EditorState>>,
    /// JSON 页签预填文本(编辑模式 = 当前配置;新建 = None 显示占位示例)
    pub json_draft: Option<String>,
    /// JSON 实时解析预览(None = 未解析;Err = 错误文案)
    pub json_preview: Option<Result<Vec<dsh_core::settings::McpServerEntry>, String>>,
}

/// Hooks 分区表单态(新增/编辑共享;M4.2)
#[derive(Clone)]
pub struct HooksDetailState {
    /// 编辑中的桥 id(None = 新增;编辑态 id 锁定)
    pub editing: Option<String>,
    /// 启用开关表单值
    pub form_enabled: bool,
    /// 方言选择(claude-code / codex)
    pub form_dialect: String,
    /// config 路径输入
    pub form_config_path: Option<Entity<InputState>>,
    /// pluginRoot 输入(CC;可选)
    pub form_plugin_root: Option<Entity<InputState>>,
    /// projectDir 输入(CC;可选,缺省 = 会话工作区)
    pub form_project_dir: Option<Entity<InputState>>,
    /// 缺省超时 MS 输入(空 = 600000)
    pub form_timeout: Option<Entity<InputState>>,
}

/// 详情页页签
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpDetailMode {
    /// 表单(逐字段)
    Form,
    /// JSON 粘贴导入
    Json,
}

/// ── MCP Servers 设置分区(列表 ↔ 详情页)─────────────────────────
impl AppStore {
    /// 打开详情页(新增模式:id 可输入;JSON 页签不激活)
    pub fn open_mcp_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form_id = Some(cx.new(|cx| InputState::new(window, cx).placeholder("server 名")));
        let form_command =
            Some(cx.new(|cx| InputState::new(window, cx).placeholder("启动命令(如 npx)")));
        let form_cwd = Some(cx.new(|cx| InputState::new(window, cx).placeholder("工作目录(可选)")));
        let form_timeout = Some(cx.new(|cx| InputState::new(window, cx).placeholder("60000")));
        self.settings.mcp_detail = Some(McpDetailState {
            editing: None,
            mode: McpDetailMode::Form,
            form_enabled: true,
            form_id,
            form_command,
            form_cwd,
            form_args: Vec::new(),
            form_timeout,
            form_env: Vec::new(),
            form_http: false,
            form_url: Some(
                cx.new(|cx| InputState::new(window, cx).placeholder("https://host/mcp")),
            ),
            form_headers: Vec::new(),
            json_input: None,
            json_draft: None,
            json_preview: None,
        });
        cx.notify();
    }

    /// 打开详情页(编辑模式:按 id 从快照预填;id 锁定只读)
    pub fn open_mcp_edit(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.settings.settings_snapshot["mcpServers"]
            .as_array()
            .and_then(|list| list.iter().find(|e| e["id"] == *id).cloned())
            .and_then(|v| serde_json::from_value::<dsh_core::settings::McpServerEntry>(v).ok())
        else {
            return;
        };
        let form_id = Some(cx.new(|cx| InputState::new(window, cx).placeholder("server 名")));
        let form_command = Some(cx.new(|cx| InputState::new(window, cx)));
        form_command.as_ref().unwrap().update(cx, |s, cx| {
            s.set_value(entry.command.clone(), window, cx);
        });
        let form_cwd = Some(cx.new(|cx| InputState::new(window, cx).placeholder("工作目录(可选)")));
        if let Some(cwd) = &entry.cwd {
            form_cwd.as_ref().unwrap().update(cx, |s, cx| {
                s.set_value(cwd.clone(), window, cx);
            });
        }
        let mut form_args = Vec::new();
        for a in &entry.args {
            let input = cx.new(|cx| InputState::new(window, cx));
            input.update(cx, |s, cx| s.set_value(a.clone(), window, cx));
            form_args.push(input);
        }
        let form_timeout = Some(cx.new(|cx| InputState::new(window, cx).placeholder("60000")));
        if let Some(ms) = entry.tool_call_timeout_ms {
            form_timeout.as_ref().unwrap().update(cx, |s, cx| {
                s.set_value(ms.to_string(), window, cx);
            });
        }
        let mut form_env = Vec::new();
        for (k, v) in &entry.env {
            let k_in = cx.new(|cx| InputState::new(window, cx));
            k_in.update(cx, |s, cx| s.set_value(k.clone(), window, cx));
            let v_in = cx.new(|cx| InputState::new(window, cx));
            v_in.update(cx, |s, cx| s.set_value(v.clone(), window, cx));
            form_env.push((k_in, v_in));
        }
        let form_http = entry.is_http();
        let form_url =
            Some(cx.new(|cx| InputState::new(window, cx).placeholder("https://host/mcp")));
        if let Some(url) = &entry.url {
            form_url.as_ref().unwrap().update(cx, |s, cx| {
                s.set_value(url.clone(), window, cx);
            });
        }
        let mut form_headers = Vec::new();
        for (k, v) in &entry.headers {
            let k_in = cx.new(|cx| InputState::new(window, cx));
            k_in.update(cx, |s, cx| s.set_value(k.clone(), window, cx));
            let v_in = cx.new(|cx| InputState::new(window, cx));
            v_in.update(cx, |s, cx| s.set_value(v.clone(), window, cx));
            form_headers.push((k_in, v_in));
        }
        // JSON 页签预填:当前配置回显为 mcpServers 形态(去 id/enabled——
        // 编辑态身份在标题锁定,启停在表单)
        let mut body = serde_json::to_value(&entry).unwrap_or(serde_json::json!({}));
        if let Some(map) = body.as_object_mut() {
            map.remove("id");
            map.remove("enabled");
        }
        let json_draft = serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": { entry.id.clone(): body }
        }))
        .ok();
        self.settings.mcp_detail = Some(McpDetailState {
            editing: Some(id.to_string()),
            mode: McpDetailMode::Form,
            form_enabled: entry.enabled,
            form_id,
            form_command,
            form_cwd,
            form_args,
            form_timeout,
            form_env,
            form_http,
            form_url,
            form_headers,
            json_input: None,
            json_draft,
            json_preview: None,
        });
        cx.notify();
    }

    /// 返回列表页(弃草稿)
    pub fn close_mcp_detail(&mut self, cx: &mut Context<Self>) {
        self.settings.mcp_detail = None;
        cx.notify();
    }

    /// 添加一条参数输入
    pub fn add_mcp_arg(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = self.settings.mcp_detail.as_mut() else {
            return;
        };
        detail
            .form_args
            .push(cx.new(|cx| InputState::new(window, cx)));
        cx.notify();
    }

    /// 移除一条参数输入
    pub fn remove_mcp_arg(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(detail) = self.settings.mcp_detail.as_mut() else {
            return;
        };
        if ix < detail.form_args.len() {
            detail.form_args.remove(ix);
            cx.notify();
        }
    }

    /// 添加一组环境变量键值输入
    pub fn add_mcp_env(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = self.settings.mcp_detail.as_mut() else {
            return;
        };
        let k = cx.new(|cx| InputState::new(window, cx).placeholder("KEY"));
        let v = cx.new(|cx| InputState::new(window, cx).placeholder("VALUE"));
        detail.form_env.push((k, v));
        cx.notify();
    }

    /// 移除一组环境变量键值输入
    pub fn remove_mcp_env(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(detail) = self.settings.mcp_detail.as_mut() else {
            return;
        };
        if ix < detail.form_env.len() {
            detail.form_env.remove(ix);
            cx.notify();
        }
    }

    /// 切换传输形态(stdio ↔ streamable-http;字段集随形态显隐)
    pub fn toggle_mcp_transport(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.settings.mcp_detail.as_mut() else {
            return;
        };
        detail.form_http = !detail.form_http;
        cx.notify();
    }

    /// 添加一组 http 请求头键值输入
    pub fn add_mcp_header(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = self.settings.mcp_detail.as_mut() else {
            return;
        };
        let k = cx.new(|cx| InputState::new(window, cx).placeholder("Header"));
        let v = cx.new(|cx| InputState::new(window, cx).placeholder("VALUE"));
        detail.form_headers.push((k, v));
        cx.notify();
    }

    /// 移除一组 http 请求头键值输入
    pub fn remove_mcp_header(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(detail) = self.settings.mcp_detail.as_mut() else {
            return;
        };
        if ix < detail.form_headers.len() {
            detail.form_headers.remove(ix);
            cx.notify();
        }
    }

    /// 翻转详情页启用开关
    pub fn toggle_mcp_form_enabled(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.settings.mcp_detail.as_mut() else {
            return;
        };
        detail.form_enabled = !detail.form_enabled;
        cx.notify();
    }

    /// 切换详情页页签(表单 ↔ JSON;切到 JSON 时懒建粘贴区)
    pub fn switch_mcp_mode(
        &mut self,
        mode: McpDetailMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.settings.mcp_detail.as_mut() else {
            return;
        };
        detail.mode = mode;
        if mode == McpDetailMode::Json && detail.json_input.is_none() {
            // 文档标准用法:创建时 default_value 预填(编辑模式 = 当前
            // 配置);新增模式无草稿 → placeholder 显示示例
            let draft = detail.json_draft.clone();
            let input = cx.new(|cx| {
                let state = EditorState::new(window, cx).language("json");
                match draft {
                    Some(text) => state.default_value(text),
                    None => state.placeholder(
                        "{\n  \"mcpServers\": {\n    \"filesystem\": {\n      \"command\": \"npx\",\n      \"args\": [\"-y\", \"@modelcontextprotocol/server-filesystem\", \"~/dir\"]\n    },\n    \"remote\": {\n      \"transport\": \"http\",\n      \"url\": \"https://host/mcp\",\n      \"headers\": { \"Authorization\": \"Bearer <token>\" }\n    }\n  }\n}",
                    ),
                }
            });
            cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    let text = input.read(cx).value().to_string();
                    let preview = dsh_core::settings::parse_mcp_servers_json(&text);
                    let Some(d) = this.settings.mcp_detail.as_mut() else {
                        return;
                    };
                    d.json_preview = Some(preview);
                }
                cx.notify();
            })
            .detach();
            detail.json_input = Some(input);
        }
        if mode == McpDetailMode::Form {
            detail.json_input = None;
            detail.json_preview = None;
        }
        cx.notify();
    }

    /// 导入 JSON(逐条 upsert;任一非法整体拒绝,错误进页内通告)
    pub fn import_mcp_json(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.settings.mcp_detail.as_ref() else {
            return;
        };
        let Some(input) = &detail.json_input else {
            return;
        };
        let text = input.read(cx).value().to_string();
        match self.bridge.host().import_mcp_servers_json(&text) {
            Ok(n) => {
                self.settings.mcp_detail = None;
                self.settings.settings_notice = Some((true, format!("已导入 {n} 个 MCP server")));
                self.settings_refresh(cx);
            }
            Err(e) => {
                self.settings.settings_notice = Some((false, e.message));
                cx.notify();
            }
        }
    }

    /// 统一保存:表单态提交字段,JSON 态解析导入(同一个保存动作)
    pub fn save_mcp_detail(&mut self, cx: &mut Context<Self>) {
        let mode = self
            .settings
            .mcp_detail
            .as_ref()
            .map(|d| d.mode)
            .unwrap_or(McpDetailMode::Form);
        match mode {
            McpDetailMode::Form => self.submit_mcp_server(cx),
            McpDetailMode::Json => self.import_mcp_json(cx),
        }
    }

    /// 编辑态卸载:删条目并返回列表
    pub fn uninstall_mcp_detail(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .settings
            .mcp_detail
            .as_ref()
            .and_then(|d| d.editing.clone())
        else {
            return;
        };
        if self.bridge.host().remove_mcp_server(&id).is_ok() {
            self.settings.mcp_detail = None;
            self.settings_refresh(cx);
        }
        cx.notify();
    }

    /// 提交 MCP server(新增 = 新 id upsert;编辑 = 同 id 覆盖)。
    /// http 形态:url 必填 + headers 键值对(键空跳过);stdio 形态:
    /// command 必填 + args/env;超时两态共用(空 = 60000)
    pub fn submit_mcp_server(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.settings.mcp_detail.as_ref() else {
            return;
        };
        let id = match &detail.editing {
            Some(id) => id.clone(),
            None => detail
                .form_id
                .as_ref()
                .map(|i| i.read(cx).value().trim().to_string())
                .unwrap_or_default(),
        };
        if id.is_empty() {
            self.push_mcp_form_notice("id 不能为空", cx);
            return;
        }
        let timeout = match detail
            .form_timeout
            .as_ref()
            .map(|i| i.read(cx).value().trim().to_string())
        {
            Some(t) if t.is_empty() => None,
            Some(t) => match t.parse::<u64>() {
                Ok(ms) => Some(ms),
                Err(_) => {
                    self.push_mcp_form_notice("超时 MS 须为非负整数", cx);
                    return;
                }
            },
            None => None,
        };
        let (command, args, env, cwd, url, headers) = if detail.form_http {
            let url = detail
                .form_url
                .as_ref()
                .map(|i| i.read(cx).value().trim().to_string())
                .unwrap_or_default();
            if url.is_empty() {
                self.push_mcp_form_notice("http 传输需要 url", cx);
                return;
            }
            let mut header_map = std::collections::BTreeMap::new();
            for (k, v) in &detail.form_headers {
                let key = k.read(cx).value().trim().to_string();
                if key.is_empty() {
                    continue;
                }
                header_map.insert(key, v.read(cx).value().to_string());
            }
            (
                String::new(),
                Vec::new(),
                std::collections::BTreeMap::new(),
                None,
                Some(url),
                header_map,
            )
        } else {
            let Some(cmd_in) = &detail.form_command else {
                return;
            };
            let command = cmd_in.read(cx).value().trim().to_string();
            if command.is_empty() {
                self.push_mcp_form_notice("command 不能为空", cx);
                return;
            }
            let args: Vec<String> = detail
                .form_args
                .iter()
                .map(|i| i.read(cx).value().trim().to_string())
                .filter(|v| !v.is_empty())
                .collect();
            let mut env = std::collections::BTreeMap::new();
            for (k, v) in &detail.form_env {
                let key = k.read(cx).value().trim().to_string();
                if key.is_empty() {
                    continue;
                }
                env.insert(key, v.read(cx).value().to_string());
            }
            let cwd = detail
                .form_cwd
                .as_ref()
                .map(|i| i.read(cx).value().trim().to_string())
                .filter(|c| !c.is_empty());
            (
                command,
                args,
                env,
                cwd,
                None,
                std::collections::BTreeMap::new(),
            )
        };
        let entry = dsh_core::settings::McpServerEntry {
            id,
            enabled: detail.form_enabled,
            command,
            args,
            env,
            cwd,
            tool_call_timeout_ms: timeout,
            url,
            headers,
        };
        match self.bridge.host().upsert_mcp_server(entry) {
            Ok(()) => {
                self.settings.mcp_detail = None;
                self.settings_refresh(cx);
            }
            Err(e) => {
                self.settings.settings_notice = Some((false, e.message));
                cx.notify();
            }
        }
    }

    fn push_mcp_form_notice(&mut self, msg: &str, cx: &mut Context<Self>) {
        if let Some(detail) = self.settings.mcp_detail.as_mut() {
            detail.json_preview = Some(Err(msg.to_string()));
        }
        cx.notify();
    }

    // ── Hooks(Claude Code / Codex 桥;M4.2)──

    /// 打开 Hooks 详情页(新增模式)
    pub fn open_hooks_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.hooks_detail = Some(HooksDetailState {
            editing: None,
            form_enabled: true,
            form_dialect: "claude-code".to_string(),
            form_config_path: Some(
                cx.new(|cx| InputState::new(window, cx).placeholder("hooks.json 路径")),
            ),
            form_plugin_root: Some(cx.new(|cx| {
                InputState::new(window, cx).placeholder("可选;替换 ${CLAUDE_PLUGIN_ROOT}")
            })),
            form_project_dir: Some(
                cx.new(|cx| InputState::new(window, cx).placeholder("可选;缺省 = 会话工作区")),
            ),
            form_timeout: Some(cx.new(|cx| InputState::new(window, cx).placeholder("600000"))),
        });
        cx.notify();
    }

    /// 打开 Hooks 详情页(编辑模式:按 id 预填)
    pub fn open_hooks_edit(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.settings.settings_snapshot["hookBridges"]
            .as_array()
            .and_then(|list| list.iter().find(|e| e["id"] == *id).cloned())
            .and_then(|v| serde_json::from_value::<dsh_core::settings::HookBridgeEntry>(v).ok())
        else {
            return;
        };
        let state = HooksDetailState {
            editing: Some(entry.id.clone()),
            form_enabled: entry.enabled,
            form_dialect: entry.dialect.clone(),
            form_config_path: Some(cx.new(|cx| InputState::new(window, cx))),
            form_plugin_root: Some(cx.new(|cx| InputState::new(window, cx))),
            form_project_dir: Some(cx.new(|cx| InputState::new(window, cx))),
            form_timeout: Some(cx.new(|cx| InputState::new(window, cx))),
        };
        let Some(d) = self.settings.hooks_detail.as_mut() else {
            return;
        };
        *d = state;
        let Some(d) = self.settings.hooks_detail.as_ref() else {
            return;
        };
        if let Some(inp) = &d.form_config_path {
            inp.update(cx, |s2, cx| {
                s2.set_value(entry.config_path.clone(), window, cx)
            });
        }
        if let (Some(inp), Some(v)) = (&d.form_plugin_root, &entry.plugin_root) {
            inp.update(cx, |s2, cx| s2.set_value(v.clone(), window, cx));
        }
        if let (Some(inp), Some(v)) = (&d.form_project_dir, &entry.project_dir) {
            inp.update(cx, |s2, cx| s2.set_value(v.clone(), window, cx));
        }
        if let (Some(inp), Some(v)) = (&d.form_timeout, entry.default_timeout_ms) {
            inp.update(cx, |s2, cx| s2.set_value(v.to_string(), window, cx));
        }
        cx.notify();
    }

    /// 关闭 Hooks 详情页(回列表)
    pub fn close_hooks_detail(&mut self, cx: &mut Context<Self>) {
        self.settings.hooks_detail = None;
        cx.notify();
    }

    /// 翻转 Hooks 方言(表单)
    pub fn set_hooks_dialect(&mut self, dialect: &str, cx: &mut Context<Self>) {
        if let Some(d) = self.settings.hooks_detail.as_mut() {
            d.form_dialect = dialect.to_string();
        }
        cx.notify();
    }

    /// 翻转 Hooks 启用开关(表单)
    pub fn toggle_hooks_form_enabled(&mut self, cx: &mut Context<Self>) {
        if let Some(d) = self.settings.hooks_detail.as_mut() {
            d.form_enabled = !d.form_enabled;
        }
        cx.notify();
    }

    /// 保存 Hooks 表单(upsert;变更 = 下次 attach 生效)
    pub fn save_hooks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.settings.hooks_detail.clone() else {
            return;
        };
        let read =
            |inp: &Option<Entity<InputState>>, window: &mut Window, cx: &mut Context<Self>| {
                let _ = window;
                inp.as_ref()
                    .map(|e| e.read(cx).value().trim().to_string())
                    .unwrap_or_default()
            };
        let config_path = read(&d.form_config_path, window, cx);
        let plugin_root = read(&d.form_plugin_root, window, cx);
        let project_dir = read(&d.form_project_dir, window, cx);
        let timeout_raw = read(&d.form_timeout, window, cx);
        let id = d.editing.clone().unwrap_or_else(|| {
            format!(
                "hooks-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_micros() as u64)
                    .unwrap_or(0)
            )
        });
        let entry = dsh_core::settings::HookBridgeEntry {
            id,
            dialect: d.form_dialect.clone(),
            config_path,
            enabled: d.form_enabled,
            plugin_root: (!plugin_root.is_empty()).then_some(plugin_root),
            project_dir: (!project_dir.is_empty()).then_some(project_dir),
            default_timeout_ms: timeout_raw.parse::<u64>().ok(),
            stderr_summary_max_chars: None,
        };
        match self.bridge.host().upsert_hook_bridge(entry) {
            Ok(()) => {
                self.settings.hooks_detail = None;
                self.settings_refresh(cx);
            }
            Err(e) => {
                self.settings.settings_notice = Some((false, e.message));
            }
        }
        cx.notify();
    }

    /// 启停 hooks 桥(enabled 翻转)
    pub fn toggle_hook_bridge(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(mut entry) = self.settings.settings_snapshot["hookBridges"]
            .as_array()
            .and_then(|list| list.iter().find(|e| e["id"] == *id).cloned())
            .and_then(|v| serde_json::from_value::<dsh_core::settings::HookBridgeEntry>(v).ok())
        else {
            return;
        };
        entry.enabled = !entry.enabled;
        if self.bridge.host().upsert_hook_bridge(entry).is_ok() {
            self.settings_refresh(cx);
        }
        cx.notify();
    }

    /// 卸载 hooks 桥
    pub fn remove_hook_bridge(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.bridge.host().remove_hook_bridge(id).is_ok() {
            self.settings_refresh(cx);
        }
        cx.notify();
    }

    /// 启停 MCP server(enabled 翻转,upsert 落盘)
    pub fn toggle_mcp_server(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(mut entry) = self.settings.settings_snapshot["mcpServers"]
            .as_array()
            .and_then(|list| list.iter().find(|e| e["id"] == *id).cloned())
            .and_then(|v| serde_json::from_value::<dsh_core::settings::McpServerEntry>(v).ok())
        else {
            return;
        };
        entry.enabled = !entry.enabled;
        if self.bridge.host().upsert_mcp_server(entry).is_ok() {
            self.settings_refresh(cx);
        }
        cx.notify();
    }

    /// 卸载 MCP server(删除注册条目;端口池同步停机,工具面即时收敛)
    pub fn remove_mcp_server(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.bridge.host().remove_mcp_server(id).is_ok() {
            self.settings_refresh(cx);
        }
        cx.notify();
    }
}
