//! 聊天消息流功能切片的 store 域:composer 输入/清空与下拉开态、消息列
//! 虚拟化与钉底跟随、工具卡/think/上下文/搜索组折叠、TodoDock 开态、
//! @ 补全探测与候选导航、消息复制反馈、发送/取消/命令执行。视图同目录
//! (chat_pane/composer/terminal/toolcard/todo_dock/context_meter);纯投影
//! 与 UI 纯算法见 chat.rs / reference.rs。

use crate::features::chat::QueuePlacement;
use gpui_kit::component::input::InputState;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui_kit::{AppContext as _, Context, Entity, Window};
use gpui_kit::component::RopeExt;
use gpui_kit::component::input::TextareaState;

use super::projection::{ChatNode, ChatState, RowSlot, build_row_slots};
use crate::features::attachments::AttachmentToast;
use crate::shell::store::AppStore;

/// Mermaid 图查看器打开态(`None` = 关闭;打开动作经内嵌图点击)。
/// 查看器为**纯图**(只有角落关闭钮 + Esc/遮罩/ctrl+滚轮缩放/拖拽
/// 平移)——所有控件(图表/代码、复制、下载、放大)都落在内嵌卡片
/// 上,见 [`MermaidCard`]。`zoom` 打开时置 `0.0` 哨兵 = 「自适应待算」
/// —— 渲染侧按视口与自然尺寸算 contain 倍数并回写;`viewport` 同由
/// 渲染侧首帧回写。位图走视口裁剪光栅(kits::mermaid::raster_viewport):
/// 内存 O(视口) 与图尺寸/放大倍数无关。
#[derive(Debug, Clone)]
pub struct MermaidViewer {
    /// 闭合围栏内容(图源码)
    pub source: Arc<str>,
    /// 显示倍数(相对自然逻辑尺寸;0.0 = 未定,渲染侧算 fit)
    pub zoom: f32,
    /// 平移原点(视口左上在整图×zoom 坐标系中的位置;拖拽更新,
    /// 双向钳制 [0, 整图−视口],整图小于视口时恒 0 = fit 全览居中)
    pub pan: (f32, f32),
    /// 视口逻辑尺寸(渲染侧每帧回写;0.0 = 尚未布局,任务侧跳过渲染)
    pub viewport: (f32, f32),
    /// 拖拽中上次指针位(按下置位、移动消费、**任意位置抬起**清位;
    /// 仅 Some↔None 迁移通知重绘——位置更新由 pan 通知携带)
    pub drag_last: Option<(f32, f32)>,
    /// 开图占位光栅 = 卡片正在显示的图(同 RenderImage,atlas 已有,
    /// 零额外纹理):后台首档光栅就位前过渡显示,消除开图空白/主线程
    /// 同步渲染冻 UI(首开 usvg 解析 + 系统字体加载可达秒级)。
    /// `placeholder_zoom` = 该光栅的渲染倍数(地图式过渡数学的基准)。
    pub placeholder: Option<Arc<gpui_kit::RenderImage>>,
    pub placeholder_zoom: f32,
    /// 首档即时渲染已排(开图后槽空时 kick 一次 0 延迟后台任务;
    /// 只 kick 一次防失败重试死循环,后续走 300ms 防抖)
    pub kicked: bool,
    /// 缩放意图代号(打开/每次缩放、拖拽结束取新值;防抖任务凭它
    /// 判定自己是否仍代表最新意图——过期任务的结果直接丢弃)
    pub zoom_gen: u64,
}

/// 查看器缩放重光栅的防抖窗口(对齐 zed `MERMAID_ZOOM_DEBOUNCE`):
/// 滚轮/触控板连续缩放期间只拉伸旧档,静止后一次后台重光栅。
const MERMAID_RERASTER_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// zoom_gen 单调源(进程级)
fn next_mermaid_gen() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static GEN: AtomicU64 = AtomicU64::new(0);
    GEN.fetch_add(1, Ordering::Relaxed)
}

/// 单张 mermaid 卡片的内联控件态(图上方工具条:图表/代码、复制、
/// 下载、放大——「控件在卡片、放大只放大图片」的归属)。
/// `None` = 收藏/轨迹等无控件调用点;消息流经 per-card map 挂靠。
#[derive(Debug, Clone, Default)]
pub struct MermaidCard {
    /// true = 代码模式(卡片工具条切换;查看器纯图态同受此开关)
    pub show_code: bool,
    /// 「复制」按钮反馈窗内(按钮变绿色「已复制」,超时由 detach 定时
    /// 任务清位——反馈锚定在动作发生处,不靠异步通知)
    pub copied: bool,
}

/// 命令行待发送态(命令菜单点选带参命令;发送时拼接 /name + 任务描述)
#[derive(Debug, Clone, PartialEq)]
pub struct PendingCommand {
    /// 命令名(不含 /)
    pub name: String,
}

/// composer 底排下拉(互斥单开;根级外点全关)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerMenu {
    /// 关
    None,
    /// + 指令菜单(/plan 等)
    Commands,
    /// 权限选择
    Permission,
    /// 模型 + 推理等级
    Model,
    /// 上下文占用详情(圆环点击)
    Context,
}

/// @ 引用补全状态:探测 hit + 候选(文件/会话分组)+ 高亮。
#[derive(Debug, Clone)]
pub struct AtCompletion {
    /// 待替换 token + span(置候选查询与选中替换)
    pub hit: super::reference::AtHit,
    /// 文件候选(目录下钻的路径)
    pub files: Vec<super::reference::FileCandidate>,
    /// 会话候选
    pub sessions: Vec<super::reference::SessionCandidate>,
    /// 高亮索引(扁平文件+会话;键盘导航)
    pub highlight: usize,
}

/// 上下文占用快照(session_stats 的 context* 键;ContextMeter 数据源)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextOccupancy {
    /// 已用 token(最新请求 prompt 侧采样,折叠/换模型后回落)
    pub used: u64,
    /// 窗口容量(1M)
    pub window: u64,
    /// 占比 0..1
    pub percent: f64,
    /// 系统提示段(启发式构成)
    pub system: u64,
    /// 工具定义段
    pub tools: u64,
    /// 会话消息段
    pub messages: u64,
}
/// 模型菜单的级联子菜单(一级行 → 右侧子卡)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerSubmenu {
    /// 模型选择(按 provider 分组;推理强度维持平铺不做子菜单)
    Models,
}

/// 聊天消息流功能切片状态(composer/消息列/折叠/补全/复制反馈)。
#[allow(missing_docs)]
pub(crate) struct ChatStore {
    /// composer 输入态(多行,Enter 发送/Shift+Enter 换行)
    pub composer_input: Option<Entity<TextareaState>>,
    /// 发送后待清空(GPUI 订阅回调无窗口句柄,set_value 需在渲染期执行)
    pub pending_composer_clear: bool,
    /// 展开的工具行 key(call:<id>)
    pub expanded_tools: HashSet<String>,
    /// 工具卡头尾展开钮(call key;read/search/diff 卡共用)
    pub card_expanded: HashSet<String>,
    /// 搜索组折叠(键 = `{callKey}:{path}`;search 卡 matches 形态)
    pub search_collapsed: HashSet<String>,
    /// 展开的 Think 行 key(a:<turn>:<step>)
    pub open_reasoning: HashSet<String>,
    /// 展开的上下文注入行 key(ctx:<seq>)
    pub open_context: HashSet<String>,
    /// 展开的轮过程组(键 = 段收口节点 key,如 turn-end:11)。**空集 =
    /// 全收**:历史会话载入即收、turn/end 收口出现即收(「默认收 + 手动
    /// 展开例外」天然实现自动收拢,无需事件 hook);点组头展开后持久,
    /// 不再被自动收回。内存态,不持久化
    pub open_turns: HashSet<String>,
    /// 导航轨 hover 点(锚点所属行槽下标;None = 未悬停)。摘要卡显隐
    /// 与定位由渲染层按它比对;悬停移出清空
    pub nav_hover: Option<usize>,
    /// 导航轨轨道线窗口纵向 span(paint 捕获的 (顶 y, 底 y);canvas 是
    /// 块级 auto 高(高度 0),单点只能取 y——顶/底各捕一个,高 = 底−顶。
    /// 拖拽换算指针 → 轨道坐标与比例定位用)
    pub nav_track: Option<(f32, f32)>,
    /// 导航捕获的合并 notify 定时任务(paint 期捕获不能直接 notify:
    /// 滚动探索时每帧都有新行首次入捕获,逐个 notify = 每帧双次全量
    /// 重渲染(可见项含 markdown 重建)= 滚动卡顿;合并为 ≤每 250ms
    /// 一帧额外渲染,滚动本身的连续帧已足够把点带出来)
    nav_reflow: Option<gpui_kit::Task<()>>,
    /// 渲染行槽缓存(每帧随 sync_chat_list 重建:O(n) 纯扫描,直接
    /// 重建避免缓存失效面——测试/旁路直接改 chats 不经 version bump;
    /// 列表行数以此为准,非 nodes 原始数)
    pub(crate) row_slots: Vec<RowSlot>,
    /// TodoDock 展开
    pub todo_open: bool,
    /// 消息列虚拟化状态(gpui 内建 list:逐项测高缓存 + Bottom 对齐;
    /// logical_scroll_top 为 None 时自动钉底跟随)
    pub chat_list: gpui_kit::ListState,
    /// 滚动条轨道高(content-card 渲染期 canvas 捕获,上一帧值):
    /// 组件库 thumb 映射把轨道高当滚动视口,轨道挂全列时差值段成盲区
    /// (拖不到底)——FullTrackHandle 以 (轨道高−列表视口高) 补偿
    /// content_size,thumb 满行程映射回列表真实滚动域
    pub track_h: f32,
    /// 输入卡总高(composer 根渲染期 canvas 捕获,上一帧值):
    /// composer 下拉菜单锚卡的 bottom 依它定位——菜单须整体悬在输入卡
    /// 上方(底缘=卡顶上方 2px),卡高随命令行/输入行数变化,固定
    /// bottom 会叠进输入框(卡越高叠越深)
    pub composer_h: f32,
    /// 上次同步进 ListState 的条数(splice 增量通知的记账)
    chat_list_count: usize,
    /// ListState 当前归属会话(失配 → reset 重建缓存与滚动位)
    chat_list_session: Option<String>,
    /// ListState 最近一次落地的列宽(渲染侧写回;宽变 → settle 后全量重测)
    list_col_w: Option<f32>,
    /// 列宽 settle 定时任务(每次宽变重启;tick 后重臂 measure_all 全量测高;
    /// gpui Task 无 cancel——过期任务经 col_w_gen 守卫丢弃)
    col_w_reflow: Option<gpui_kit::Task<()>>,
    /// 列宽重排代次(每次调度 +1;settle 比对当前代,过期任务直接返回)
    col_w_gen: u64,
    pub pinned: bool,
    /// 消息流版本号(每次当前会话事件 +1;渲染侧比对驱动滚动)
    pub chat_version: u64,
    /// 渲染侧已消费的消息流版本(render 回写)
    pub rendered_version: u64,
    /// composer 底排下拉开态(互斥)
    pub composer_menu: ComposerMenu,
    /// 权限 chip 的窗口 bounds(渲染期 canvas 捕获,上一帧值):权限
    /// 下拉卡**根级渲染**的锚——卡底缘贴 chip 顶上方 5px、左对齐
    /// (根级原因见 permission_card)
    pub perm_chip_bounds: Option<gpui_kit::Bounds<gpui_kit::Pixels>>,
    /// 模型菜单级联子菜单(一级行点开的右侧子卡;None = 全收)
    pub composer_submenu: Option<ComposerSubmenu>,
    /// 队列条带折叠态(多条时计数头收起;单条恒直显)
    pub queue_dock_collapsed: bool,
    /// 行内编辑中的队列条目 id(None = 无)
    pub queue_editing: Option<String>,
    /// 队列行内编辑输入(进入编辑态惰建并预填原文)
    pub queue_edit_input: Option<Entity<InputState>>,
    /// 命令行待发送态(命令菜单点选带参命令;/命令以品牌色命令行呈现
    /// 与输入文字区分——gpui-component Textarea 无文本分段染色能力,
    /// 框内染色不可达,故命令与参数分呈现:命令行可移除,输入框只写
    /// 参数,发送时拼接 /name args 走既有文本路径)
    pub pending_command: Option<PendingCommand>,
    /// 计划归档卡展开态(键 = plan:<seq>;缺席 = 折叠)
    pub open_plans: HashSet<String>,
    /// 计划 chip hover 态(激活时 hover 才把图标换成 ⓧ 取消态;
    /// ⓧ 不常显)
    pub plan_chip_hovered: bool,
    /// composer 当前已应用 placeholder(渲染期同步比对基线,见
    /// sync_composer_placeholder)
    pub composer_placeholder: &'static str,
    /// 展开的 LLM 重试行 key(retry:<seq>;缺席 = 折叠)
    pub open_retries: HashSet<String>,
    /// 重试倒计时节拍(有未来截止的等待行才保活;见 sync_retry_tick)
    pub(crate) retry_tick: Option<gpui_kit::Task<()>>,
    /// @ 引用补全探测(Some = 弹补全菜单,@ 触发与 / 菜单共存但互斥展示)
    pub at_completion: Option<AtCompletion>,
    /// @ 补全打开时的 Enter 标志(渲染层消费:选中高亮项而非发送)
    pub enter_at_completion: bool,
    /// 最近复制成功的消息 key(图标 Copy→Check 反馈;定时清除)
    pub copied_key: Option<String>,
    /// Mermaid 图查看器(打开态;None = 关闭)
    pub mermaid_viewer: Option<MermaidViewer>,
    /// 查看器缩放的防抖重光栅任务(替换 = 取消旧 timer;见
    /// `schedule_mermaid_reraster`)。滚轮连续缩放期间不重光栅(旧档
    /// 拉伸显示),静止 300ms 后后台渲染一次——同步逐帧重光栅既卡 UI
    /// 又令 atlas 纹理线性堆积(7G 内存泄漏的另一半根因)。
    pub mermaid_reraster: Option<gpui_kit::Task<()>>,
    /// 单张 mermaid 卡片的控件态(键 = 消息 markdown 节点 id `{key}-md-...`)。
    /// 卡片自绘工具条(图表/代码、±缩放、下载、放大)由此挂靠;查看器
    /// 纯图态读同一张卡的状态。
    pub mermaid_cards: HashMap<String, MermaidCard>,
}

impl Default for ChatStore {
    fn default() -> Self {
        Self {
            composer_h: 0.,
            plan_chip_hovered: false,
            composer_placeholder: "输入消息,Enter 发送 / Shift+Enter 换行",
            composer_input: None,
            pending_composer_clear: false,
            expanded_tools: HashSet::new(),
            card_expanded: HashSet::new(),
            search_collapsed: HashSet::new(),
            open_reasoning: HashSet::new(),
            open_context: HashSet::new(),
            open_turns: HashSet::new(),
            nav_hover: None,
            nav_track: None,
            nav_reflow: None,
            row_slots: Vec::new(),
            todo_open: false,
            chat_list: gpui_kit::ListState::new(0, gpui_kit::ListAlignment::Bottom, gpui_kit::px(600.)),
            track_h: 0.,
            chat_list_count: 0,
            chat_list_session: None,
            list_col_w: None,
            col_w_reflow: None,
            col_w_gen: 0,
            pinned: true,
            chat_version: 0,
            rendered_version: 0,
            composer_menu: ComposerMenu::None,
            perm_chip_bounds: None,
            composer_submenu: None,
            queue_dock_collapsed: true,
            queue_editing: None,
            queue_edit_input: None,
            pending_command: None,
            open_plans: HashSet::new(),
            open_retries: HashSet::new(),
            retry_tick: None,
            at_completion: None,
            enter_at_completion: false,
            copied_key: None,
            mermaid_viewer: None,
            mermaid_reraster: None,
            mermaid_cards: HashMap::new(),
        }
    }
}

impl AppStore {
    /// 渲染期清空 composer(见 pending_composer_clear 注释)
    pub fn flush_composer_clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.chat.pending_composer_clear {
            return;
        }
        self.chat.pending_composer_clear = false;
        if let Some(input) = &self.chat.composer_input {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
    }

    /// 渲染期同步 composer placeholder:计划模式=「描述你的任务以生成计划」
    /// (逐字文案),标准态=常规文案。
    /// 模式翻转的回调(乐观/回声/切会话)均无窗口句柄,与
    /// flush_composer_clear 同理走渲染期回写;已应用值记
    /// composer_placeholder,不同才 set(无通知环)
    pub fn sync_composer_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let want: &'static str = if self.current_chat().is_some_and(|c| c.plan_mode) {
            "描述你的任务以生成计划"
        } else {
            "输入消息,Enter 发送 / Shift+Enter 换行"
        };
        if self.chat.composer_placeholder == want {
            return;
        }
        if let Some(input) = &self.chat.composer_input {
            input.update(cx, |s, cx| s.set_placeholder(want, window, cx));
            self.chat.composer_placeholder = want;
        }
    }

    /// 打开 mermaid 查看器(卡片「放大」按钮/双击图入口)。zoom 置 0.0
    /// 哨兵:渲染侧首次绘制按视口与自然尺寸算 fit 回写。`placeholder` =
    /// 卡片正在显示的光栅(缓存命中零渲染):开图首档光栅走**后台**,
    /// 就位前拉伸显示占位 —— 主线程绝不同步 usvg 解析(首开含系统字体
    /// 加载可达秒级,曾致开图冻 UI)。打开时关掉一切下拉开合。
    pub fn open_mermaid_viewer(
        &mut self,
        source: Arc<str>,
        placeholder: Option<(Arc<gpui_kit::RenderImage>, f32)>,
        cx: &mut Context<Self>,
    ) {
        let (placeholder, placeholder_zoom) = match placeholder {
            Some((img, z)) => (Some(img), z),
            None => (None, 1.0),
        };
        self.chat.mermaid_viewer = Some(MermaidViewer {
            source,
            zoom: 0.0,
            pan: (0.0, 0.0),
            viewport: (0.0, 0.0),
            drag_last: None,
            placeholder,
            placeholder_zoom,
            kicked: false,
            zoom_gen: next_mermaid_gen(),
        });
        // 打开即取新 gen:让上一会话遗留的未决防抖任务全部过期
        self.close_all_menus(cx);
        cx.notify();
    }

    /// 渲染侧 fit 回写(哨兵 0.0 → 算得 contain 倍数):整图恰好入视口,
    /// pan 归零;zoom 落定同样走防抖重光栅(视口档需要一档清晰光栅)。
    pub fn set_mermaid_viewer_fit(&mut self, fit: f32, cx: &mut Context<Self>) {
        if let Some(v) = self.chat.mermaid_viewer.as_mut()
            && v.zoom <= 0.0
        {
            v.zoom = fit;
            v.pan = (0.0, 0.0);
            v.zoom_gen = next_mermaid_gen();
            self.schedule_mermaid_reraster(cx, MERMAID_RERASTER_DEBOUNCE);
        }
    }

    /// 渲染侧视口尺寸回写(每帧;变更才重排)。视口影响裁剪区域与 pan
    /// 钳制范围 → 变更即需要一档新视口光栅(窗口缩放后)。
    pub fn set_mermaid_viewer_viewport(&mut self, w: f32, h: f32, cx: &mut Context<Self>) {
        if let Some(v) = self.chat.mermaid_viewer.as_mut()
            && ((v.viewport.0 - w).abs() > 0.5 || (v.viewport.1 - h).abs() > 0.5)
        {
            v.viewport = (w, h);
            v.zoom_gen = next_mermaid_gen();
            self.schedule_mermaid_reraster(cx, MERMAID_RERASTER_DEBOUNCE);
        }
    }

    /// 开图首档即时渲染(渲染侧槽空时 kick 一次,0 延迟):后台线程渲染
    /// 视口档,主线程只显示占位图。只 kick 一次(失败重试会每帧循环,
    /// 后续缩放/平移自然走 300ms 防抖重排)。
    pub fn kick_mermaid_reraster(&mut self, cx: &mut Context<Self>) {
        if let Some(v) = self.chat.mermaid_viewer.as_mut()
            && !v.kicked
        {
            v.kicked = true;
            v.zoom_gen = next_mermaid_gen();
            self.schedule_mermaid_reraster(cx, std::time::Duration::ZERO);
        }
    }

    /// 关闭查看器(遮罩/Esc 同源):取消未决防抖任务并立即回收单槽
    /// 视口光栅的 atlas 纹理——事件处理器内调用,下一帧起无绘制方,
    /// 回收安全(不回收则残留至进程退出)。占位图归卡片缓存,不在此回收。
    pub fn close_mermaid_viewer(&mut self, cx: &mut Context<Self>) {
        self.chat.mermaid_viewer = None;
        self.chat.mermaid_reraster = None;
        crate::kits::mermaid::drain_pending_drops(cx);
        if let Some(img) = crate::kits::mermaid::viewer_evict() {
            cx.drop_image(img, None);
        }
        cx.notify();
    }

    /// 查看器缩放(×`step`;0.25 倍步进传 0.8,1.25 传 1.25)。
    /// 哨兵 0.0(fit 未算)时按 1.0 起算;clamp [0.25, 8]。**锚定缩放**:
    /// `anchor` = 视口坐标锚点(滚轮传光标位、`None` = 视口中心),
    /// 缩放前后锚点指向同一图点 `pan' = (pan+a)·z'/z − a`(需自然尺寸,
    /// 首帧后必命中缓存),再钳回有效范围。每次滚轮都重排防抖任务
    /// (替换 = 取消旧 timer)——连续缩放静止后重光栅一次;期间查看器
    /// 拉伸显示旧档(见 `viewer_display`)。
    pub fn set_mermaid_zoom(
        &mut self,
        step: f32,
        anchor: Option<(f32, f32)>,
        cx: &mut Context<Self>,
    ) {
        if let Some(v) = self.chat.mermaid_viewer.as_mut() {
            let z = if v.zoom <= 0.0 { 1.0 } else { v.zoom };
            let z_new = (z * step).clamp(0.25, 8.0);
            let a = anchor
                .map(|(x, y)| (x.clamp(0.0, v.viewport.0), y.clamp(0.0, v.viewport.1)))
                .unwrap_or((v.viewport.0 / 2.0, v.viewport.1 / 2.0));
            let nat = crate::kits::mermaid::natural_size(&v.source);
            let fig = nat.map(|(w, h)| (w * z_new, h * z_new));
            v.pan = match fig {
                Some((fw, fh)) if v.viewport.0 > 0.0 && v.viewport.1 > 0.0 => (
                    ((v.pan.0 + a.0) * z_new / z - a.0).clamp(0.0, (fw - v.viewport.0).max(0.0)),
                    ((v.pan.1 + a.1) * z_new / z - a.1).clamp(0.0, (fh - v.viewport.1).max(0.0)),
                ),
                _ => v.pan, // 尚未布局/无自然尺寸:pan 维持,任务侧会跳过
            };
            v.zoom = z_new;
            v.zoom_gen = next_mermaid_gen();
        }
        self.schedule_mermaid_reraster(cx, MERMAID_RERASTER_DEBOUNCE);
        cx.notify();
    }

    /// 查看器拖拽平移(视口像素增量;拖拽 move 高频调用)。pan 反向减
    /// 增量(拖右 → 图右移 = 原点左移),双向钳制;**只 pan + notify,
    /// 不排任务不动 gen**(60Hz 拖拽逐次 spawn 任务纯浪费;期间 stale
    /// 平移偏移补偿跟手,松手由 [`set_mermaid_drag`]`(None)` 排最终档)。
    pub fn pan_mermaid_viewer(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
        if let Some(v) = self.chat.mermaid_viewer.as_mut() {
            let nat = crate::kits::mermaid::natural_size(&v.source);
            let fig = nat.map(|(w, h)| (w * v.zoom, h * v.zoom));
            let next = match fig {
                Some((fw, fh)) if v.viewport.0 > 0.0 && v.viewport.1 > 0.0 => (
                    (v.pan.0 - dx).clamp(0.0, (fw - v.viewport.0).max(0.0)),
                    (v.pan.1 - dy).clamp(0.0, (fh - v.viewport.1).max(0.0)),
                ),
                _ => v.pan,
            };
            v.pan = next;
        }
        cx.notify();
    }

    /// 拖拽指针位更新:`Some` = 按下/移动中(**仅 Some↔None 迁移才
    /// notify**——位置变化的重绘由 pan 携带);`None` = 松手(任意位置,
    /// 含画布外——根层抬起兜底),此时若有平移则取新 gen + 排防抖任务
    /// 渲染最终档。事件侧读**活状态**(store.read),勿依赖渲染快照。
    pub fn set_mermaid_drag(&mut self, at: Option<(f32, f32)>, cx: &mut Context<Self>) {
        let Some(v) = self.chat.mermaid_viewer.as_mut() else {
            return;
        };
        let was = v.drag_last.is_some();
        if was == at.is_some() {
            v.drag_last = at; // Some→Some 位置更新零通知 / None→None no-op
            return;
        }
        if at.is_none() {
            v.zoom_gen = next_mermaid_gen(); // 松手:补拖拽期未排的最终档
        }
        v.drag_last = at;
        if at.is_none() {
            self.schedule_mermaid_reraster(cx, MERMAID_RERASTER_DEBOUNCE);
        }
        cx.notify();
    }

    /// 防抖后台视口重光栅:静止 `delay` 后(滚轮 300ms / 开图首档 0),
    /// 主线程校验意图(查看器仍在、gen 未变、已布局、单槽无精确档)→
    /// 后台线程 usvg+resvg 只光栅化视口可见区域(≤ 视口尺寸,与图大小/
    /// 放大倍数无关)→ 主线程写单槽(`viewer_put`;被替换旧图
    /// `drop_image` 回收)→ notify 重绘清晰档。渲染在后台(UI 不卡),
    /// 期间占位图/旧档平移/拉伸显示(不空窗)。
    fn schedule_mermaid_reraster(&mut self, cx: &mut Context<Self>, delay: std::time::Duration) {
        let Some(viewer) = self.chat.mermaid_viewer.clone() else {
            return;
        };
        let (intent, source) = (viewer.zoom_gen, viewer.source);
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            // 字段收尾的硬约束:**清字段 = drop 本任务句柄 = 在下一个
            // await 点取消自己**。校验通过后还要跨两个 await(后台渲染、
            // 入槽更新),中途清字段会让光栅永远到不了入槽(查看器永久
            // 占位/旧档,滚轮「延迟」的根源之一)。因此字段只在无后续
            // await 的终态清,且清前复验 gen —— 防误杀已接管字段的新任务。
            fn clear_if_mine(st: &mut AppStore, intent: u64) {
                if st
                    .chat
                    .mermaid_viewer
                    .as_ref()
                    .is_some_and(|v| v.zoom_gen == intent)
                {
                    st.chat.mermaid_reraster = None;
                }
            }
            // 校验意图:查看器仍在、gen 未变(gen 变 = 新意图已接管字段)
            let current = this
                .update(cx, |st, _| {
                    let v = st.chat.mermaid_viewer.clone()?;
                    (v.zoom_gen == intent).then_some(v)
                })
                .ok()
                .flatten();
            let Some(v) = current else { return };
            // 视口未布局(渲染路径首帧回写后会重排任务)或单槽已是精确档
            //(开图 kick 已入槽)→ 无事可做,终态清位
            if v.viewport.0 <= 0.0
                || v.viewport.1 <= 0.0
                || crate::kits::mermaid::viewer_upto_date(&v.source, v.zoom, v.pan, v.viewport)
            {
                this.update(cx, |st, _| clear_if_mine(st, intent)).ok();
                return;
            }
            let src = v.source.clone();
            let (zoom, pan, viewport) = (v.zoom, v.pan, v.viewport);
            let rendered = cx
                .background_spawn(async move {
                    crate::kits::mermaid::raster_viewport(&src, zoom, pan, viewport)
                })
                .await;
            let Ok(raster) = rendered else {
                this.update(cx, |st, _| clear_if_mine(st, intent)).ok();
                return;
            };
            this.update(cx, |st, cx| {
                // 复验:后台渲染期间意图又变 → 丢弃本次结果(字段归新任务)
                let fresh = st
                    .chat
                    .mermaid_viewer
                    .as_ref()
                    .map(|v| v.zoom_gen == intent);
                if fresh != Some(true) {
                    return;
                }
                crate::kits::mermaid::drain_pending_drops(cx);
                if let Some(old) = crate::kits::mermaid::viewer_put(&source, &raster) {
                    cx.drop_image(old, None);
                }
                clear_if_mine(st, intent); // 终态清位(闭包执行中 drop 自身无碍)
                cx.notify();
            })
            .ok();
        });
        self.chat.mermaid_reraster = Some(task); // 旧任务被 drop → 未醒 timer 取消
    }

    /// 卡片「放大」:打开查看器(`placeholder` = 卡片在档光栅,开图占位)。
    pub fn open_mermaid_enlarged(
        &mut self,
        source: Arc<str>,
        placeholder: Option<(Arc<gpui_kit::RenderImage>, f32)>,
        cx: &mut Context<Self>,
    ) {
        self.open_mermaid_viewer(source, placeholder, cx);
    }

    /// 卡片「图表/代码」切换(卡片工具条;查看器纯图态同受此开关)
    pub fn toggle_mermaid_code(&mut self, card_key: &str, cx: &mut Context<Self>) {
        if let Some(card) = self.chat.mermaid_cards.get_mut(card_key) {
            card.show_code = !card.show_code;
        } else {
            self.chat.mermaid_cards.insert(
                card_key.to_string(),
                MermaidCard {
                    show_code: true,
                    ..Default::default()
                },
            );
        }
        // 图↔代码高度突变 → 列表项需重测,否则项高不刷新/图片溢出
        self.remeasure_chat_list(cx);
    }

    /// 卡片「复制」:源码进剪贴板 + **按钮级即时反馈**(按钮变绿色
    /// 「已复制」,`MERMAID_COPY_FEEDBACK` 后复原)。反馈窗用 detach
    /// 定时任务清位:不占可替换任务字段(连点两张卡各自的复原互不
    /// 取消),任务尾只做一次性清位 + notify。
    pub fn copy_mermaid_source(
        &mut self,
        card_key: &str,
        source: &Arc<str>,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(source.to_string()));
        self.chat
            .mermaid_cards
            .entry(card_key.to_string())
            .or_default()
            .copied = true;
        cx.spawn({
            let key = card_key.to_string();
            async move |this, cx| {
                cx.background_executor()
                    .timer(crate::kits::mermaid::MERMAID_COPY_FEEDBACK)
                    .await;
                this.update(cx, |st, cx| {
                    if let Some(card) = st.chat.mermaid_cards.get_mut(&key) {
                        card.copied = false;
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    /// 消息列是否钉在底部(24px 阈值,对齐 web FOLLOW_THRESHOLD)。
    /// ListState 语义:Bottom 对齐下 logical_scroll_top 为 None 即自动
    /// 钉底(item_ix = 末项);用户上滚后 Some(具体项),距底以已测
    /// 高度合计推导——近尾(≤24px)仍视为钉底,流式跟底不轻易断
    pub fn at_bottom(&self) -> bool {
        let list = &self.chat.chat_list;
        if list.logical_scroll_top().item_ix >= list.item_count() {
            return true; // None/默认 = 钉底跟随态
        }
        let max = f32::from(list.max_offset_for_scrollbar().y);
        let cur = f32::from(-list.scroll_px_offset_for_scrollbar().y);
        max - cur <= 24.
    }

    /// 消息列与行槽对齐:切会话 → reset(清高度缓存 + 滚动位);
    /// 追加 → splice 增量计数(新项 Unmeasured,入视口才测高);
    /// 回缩(组折叠:end 自动收)→ 锚定 reset 保守重建。reset 后经
    /// schedule settle 全量重测(见 remeasure_chat_list)。渲染期调用
    /// (flush 伴生)。行数以 row_slots(派生行槽)为准,非 nodes 原始数
    pub fn sync_chat_list(&mut self, cx: &mut Context<Self>) {
        self.ensure_row_slots();
        let sid = self.state.current_id.clone();
        // 列表行数 = 行槽 + 流尾插队气泡(伪行;session/queue 帧驱动增减)
        let steering = self
            .current_chat()
            .map(|c| {
                c.queue
                    .iter()
                    .filter(|e| e.placement == QueuePlacement::Steering)
                    .count()
            })
            .unwrap_or(0);
        let count = self.chat.row_slots.len() + steering;
        if self.chat.chat_list_session.as_deref() != sid.as_deref() {
            self.chat.chat_list.reset(count);
            self.chat.chat_list_session = sid;
            self.chat.chat_list_count = count;
            self.schedule_chat_remeasure(cx);
            return;
        }
        if count == self.chat.chat_list_count {
            return;
        }
        if count > self.chat.chat_list_count {
            let at = self.chat.chat_list_count;
            self.chat.chat_list.splice(at..at, count - at);
        } else {
            let anchor = self.viewport_anchor();
            self.reset_chat_list_anchored(anchor, cx);
        }
        self.chat.chat_list_count = count;
    }

    /// 重建行槽缓存(纯扫描直接重建,无失效追踪)
    pub(crate) fn ensure_row_slots(&mut self) {
        let nodes = self
            .current_chat()
            .map(|c| c.nodes.as_slice())
            .unwrap_or(&[]);
        self.chat.row_slots = build_row_slots(nodes, &self.chat.open_turns);
    }

    /// 视口锚:非钉底时取视口顶槽的稳定 key 与项内偏移(重建后据此恢复);
    /// 钉底(None)走 Bottom 对齐自然跟随,无需锚定
    fn viewport_anchor(&self) -> Option<(String, f32)> {
        if self.at_bottom() {
            return None;
        }
        let list = &self.chat.chat_list;
        let top = list.logical_scroll_top();
        let nodes = self.current_nodes();
        let key = match self.chat.row_slots.get(top.item_ix) {
            Some(RowSlot::Node(n) | RowSlot::GroupMember(n)) => {
                nodes.get(*n).map(|nd| nd.key().to_string())
            }
            Some(RowSlot::Group { turn_key, .. } | RowSlot::GroupOpen { turn_key, .. }) => {
                Some(turn_key.clone())
            }
            None => None,
        };
        key.map(|k| (k, f32::from(top.offset_in_item)))
    }

    /// 锚定 key → 当前行号:先精确匹配(节点 key / 组 turn_key);未中
    /// 说明该节点被折叠吞并 → 落到覆盖它的组头行
    fn slot_index_of_anchor(&self, key: &str) -> Option<usize> {
        let nodes = self.current_nodes();
        let slot_key = |s: &RowSlot| -> Option<String> {
            match s {
                RowSlot::Node(n) | RowSlot::GroupMember(n) => {
                    nodes.get(*n).map(|nd| nd.key().to_string())
                }
                RowSlot::Group { turn_key, .. } | RowSlot::GroupOpen { turn_key, .. } => {
                    Some(turn_key.clone())
                }
            }
        };
        if let Some(ix) = self
            .chat
            .row_slots
            .iter()
            .position(|s| slot_key(s).as_deref() == Some(key))
        {
            return Some(ix);
        }
        let node_ix = nodes.iter().position(|n| n.key() == key)?;
        self.chat.row_slots.iter().position(|s| {
            matches!(s, RowSlot::Group { first, last, .. } if *first <= node_ix && node_ix <= *last)
        })
    }

    /// 折叠回缩后的列表重建:reset(清测高缓存)+ 滚动位显式化 +
    /// settle 全量重测。中部行数变化不能走 splice(splice 记账假设
    /// 尾部追加,测高缓存按行号存放会整体错位)。
    /// 滚动位:钉底 → 显式 scroll_to 末项之外(reset 后的滚动位语义
    /// 不可依赖,悬挂的旧位会让 Bottom 跟随失效出现底部空隙);
    /// 非钉底 → 锚定项**顶对齐**(offset 归零:锚定项常因收拢变身份,
    /// 保留旧项内偏移会把视口推出内容之外)
    fn reset_chat_list_anchored(&mut self, anchor: Option<(String, f32)>, cx: &mut Context<Self>) {
        let count = self.chat.row_slots.len();
        self.chat.chat_list.reset(count);
        self.chat.chat_list_count = count;
        match anchor
            .as_ref()
            .and_then(|(key, _)| self.slot_index_of_anchor(key))
        {
            Some(ix) => self.chat.chat_list.scroll_to(gpui_kit::ListOffset {
                item_ix: ix,
                offset_in_item: gpui_kit::px(0.),
            }),
            None => self.chat.chat_list.scroll_to(gpui_kit::ListOffset {
                item_ix: usize::MAX,
                offset_in_item: gpui_kit::px(0.),
            }),
        }
        self.schedule_chat_remeasure(cx);
    }

    /// 轮过程组展开/收拢(组头点击)
    pub fn toggle_turn_group(&mut self, key: &str, cx: &mut Context<Self>) {
        // 锚须在行槽重建前读(旧槽位)
        let anchor = self.viewport_anchor();
        if !self.chat.open_turns.insert(key.to_string()) {
            self.chat.open_turns.remove(key);
        }
        self.ensure_row_slots();
        self.reset_chat_list_anchored(anchor, cx);
        cx.notify();
    }

    /// 导航轨 hover 点更新(None = 移出)
    pub fn set_nav_hover(&mut self, at: Option<usize>, cx: &mut Context<Self>) {
        if self.chat.nav_hover != at {
            self.chat.nav_hover = at;
            cx.notify();
        }
    }

    /// 导航跳转:目标槽**顶对齐** + 解锁钉底(jump 走显式滚动位,Bottom
    /// 跟随不再甩回底部;回落钉底由用户点回底钮或滚到底触发)
    pub fn jump_to_nav(&mut self, slot_ix: usize, cx: &mut Context<Self>) {
        if slot_ix >= self.chat.row_slots.len() {
            return;
        }
        self.chat.pinned = false;
        self.chat.nav_hover = None;
        self.chat.chat_list.scroll_to(gpui_kit::ListOffset {
            item_ix: slot_ix,
            offset_in_item: gpui_kit::px(0.),
        });
        cx.notify();
    }

    /// 导航捕获的合并 notify(见 nav_reflow 字段注释;已有任务挂起则
    /// 直接复用,窗口内任意多次捕获只换来一次额外渲染)
    fn schedule_nav_notify(&mut self, cx: &mut Context<Self>) {
        if self.chat.nav_reflow.is_some() {
            return;
        }
        self.chat.nav_reflow = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            this.update(cx, |s, cx| {
                s.chat.nav_reflow = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// 轨道线纵向 span 捕获(canvas prepaint;eps 门控)
    pub fn note_nav_track(&mut self, top: f32, bottom: f32, cx: &mut Context<Self>) {
        if self.chat.nav_track != Some((top, bottom)) {
            self.chat.nav_track = Some((top, bottom));
            self.schedule_nav_notify(cx);
        }
    }

    /// 滚动条轨道高捕获(渲染期 canvas,见 ChatState.track_h 注释)。
    /// 变化才 notify:canvas 每帧 paint 都跑,无守卫即每帧重渲染死循环。
    pub fn note_track_h(&mut self, h: f32, cx: &mut Context<Self>) {
        if (self.chat.track_h - h).abs() > 0.5 {
            self.chat.track_h = h;
            cx.notify();
        }
    }

    /// 输入卡总高捕获(渲染期 canvas,见 ChatState.composer_h 注释;
    /// 同款变化守卫)
    pub fn note_composer_h(&mut self, h: f32, cx: &mut Context<Self>) {
        if (self.chat.composer_h - h).abs() > 0.5 {
            self.chat.composer_h = h;
            cx.notify();
        }
    }

    /// 列宽变化通知(渲染期写回;见 shell/mod.rs 回写块):
    /// gpui list 宽变失效后只重测可视带,`scroll_max` 依 items.summary()
    /// 总高推得——未测项 0 高 → 滚动被 clamp 在几百 px 内打转(Bottom
    /// 对齐下滚出带区即甩回钉底)。settle 250ms 后重臂 measure_all:
    /// 下次 prepaint 全量测高(layout_all_items),总高精确 → clamp/滚动条
    /// 全部恢复;拖拽期间每帧宽变不触发,避免全量重测拖垮 resize 帧。
    /// 侧栏拖宽同路径(列宽 = viewport − 侧栏)。
    pub fn sync_chat_list_width(&mut self, col_w: gpui_kit::Pixels, cx: &mut Context<Self>) {
        let w = f32::from(col_w);
        if self.chat.list_col_w == Some(w) {
            return;
        }
        self.chat.list_col_w = Some(w);
        self.schedule_chat_remeasure(cx);
    }

    /// 宽变 reset 分支同调:reset 清缓存后同样只有带区被测高(大会话
    /// 切换后滚动卡死),经同一 settle 全量重测修复。
    fn schedule_chat_remeasure(&mut self, cx: &mut Context<Self>) {
        // gpui Task 无 cancel:旧任务保留,但代次不匹配即丢弃(只重绑
        // 任务句柄,不复用——句柄对旧任务仍存活,settle 后照常结束)
        self.chat.col_w_gen = self.chat.col_w_gen.wrapping_add(1);
        let generation = self.chat.col_w_gen;
        self.chat.col_w_reflow = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            this.update(cx, |s, cx| {
                if s.chat.col_w_gen == generation {
                    s.chat.col_w_reflow = None;
                    s.remeasure_chat_list(cx);
                }
            })
            .ok();
        }));
    }

    /// 重臂 measure-all latch 并 notify:宽变失效(或 reset)已把项清为
    /// Unmeasured,下一帧 prepaint 走 layout_all_items 全量测高(latency
    /// 一帧),此后 items.summary() 总高精确,滚动 clamp 恢复。
    pub fn remeasure_chat_list(&mut self, cx: &mut Context<Self>) {
        // measure_all 消费 self 但只是共享 Rc 的 clone——作用是在
        // RefCell 内重臂 measuring_behavior 的 Measure(false) latch;
        // 下一 prepaint 消费后自复位为已测(不重复全量)
        self.chat.chat_list = self.chat.chat_list.clone().measure_all();
        cx.notify();
    }

    /// 发送(queue 模式;命令类 `/plan` 不做乐观 running——宿主短路
    /// 无 turn 结束帧复位,乐观会永久卡「停止」)
    pub fn send(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(id) = self.state.current_id.clone() else {
            return;
        };
        let mut text_owned = text.to_string();
        // 命令行态:命令 + 参数拼接为 /name args,走既有文本路径(命令
        // 短路、命令拒图片、命令不乐观 running 等语义全在下游)
        if let Some(cmd) = self.chat.pending_command.take() {
            let arg = text_owned.trim();
            text_owned = if arg.is_empty() {
                format!("/{}", cmd.name)
            } else {
                format!("/{} {}", cmd.name, arg)
            };
            cx.notify();
        }
        // `/plan [on|off|任务]`:切计划模式。裸 /plan 与 /plan on = 进入;
        // /plan off = 退出;其余余文 = 进入计划模式并把余文作为首条任务
        // 消息发送(参考图流程:composer 内 /plan 前缀 + 任务描述)。
        // 模式切换不产生聊天区通告(状态由 chip 体现)
        if let Some(rest) = text_owned.strip_prefix("/plan") {
            let rest = rest.trim();
            let host = self.bridge.host().clone();
            let sid = id.clone();
            let (cmd, task) = match rest {
                "" | "on" => ("/plan".to_string(), None),
                "off" => ("/plan off".to_string(), None),
                other => ("/plan".to_string(), Some(other.to_string())),
            };
            let rx = self.bridge.call(async move {
                let _ = host.execute_command(&sid, &cmd).await;
            });
            drop(rx);
            match task {
                None => {
                    cx.notify();
                    return;
                }
                Some(t) => text_owned = t,
            }
        }
        let text = text_owned.as_str();
        let is_command = text.trim().starts_with('/');
        if !is_command {
            self.state.running_by_id.insert(id.clone(), true);
            // 用户消息强制钉底(web:新内容 toBottom)
            self.chat.pinned = true;
        }
        let host = self.bridge.host().clone();
        // 图片在前文本在后(源 serializeImages 序:content = [...images, ...text])
        use base64::Engine as _;
        let mut content: Vec<serde_json::Value> = self
            .attachments
            .draft_images
            .iter()
            .map(|d| {
                serde_json::json!({
                    "type": "image",
                    "mediaType": d.media_type,
                    "data": base64::engine::general_purpose::STANDARD.encode(&d.bytes),
                    "name": d.name,
                })
            })
            .collect();
        if !text.is_empty() {
            content.push(serde_json::json!({ "type": "text", "text": text }));
        }
        // 命令 claim 拒绝图片:源 command.imagesUnsupported(整批拒绝,草稿与文本保留)
        let is_cmd = is_command;
        let drafts = std::mem::take(&mut self.attachments.draft_images);
        if is_cmd && !drafts.is_empty() {
            self.attachments.attachment_toast = Some(AttachmentToast {
                text: format!(
                    "/{} 不接受图片附件,请先移除图片",
                    text.split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .trim_start_matches('/')
                ),
            });
            self.attachments.draft_images = drafts;
            cx.notify();
            return;
        }
        let sid = id.clone();
        let text_owned = text.to_string();
        // 运行中 Enter 行为按偏好(queue = 排队 / steer = 转向;
        // 源 ui-conversation EnterBehaviorRow 对应物);空闲/命令恒排队
        let running = self.state.running_by_id.get(&id).copied().unwrap_or(false);
        let mode = if !is_command && running {
            self.bridge.host().busy_enter()
        } else {
            "queue".to_string()
        };
        let rx = self.bridge.call(async move {
            // @session 引用 — 解析 mention,读被引会话快照,追加快照
            // user message(直接消息 + 不可信快照)
            let content = content;
            let refs = super::reference::parse_session_references(&text_owned);
            // 4a 完整溯源模型:@session 引用 → 读被引会话快照,作为独立
            // user/message 注入(带 source.kind=session-reference),而非
            // 追加进用户 content。驱动并入注入数组交引擎,在用户消息后落档。
            let mut contexts = Vec::new();
            if !refs.is_empty() {
                for (label, ref_sid) in refs.iter().take(super::reference::MAX_REFERENCES) {
                    if ref_sid == &sid {
                        continue; // 自引用跳过(源 SELF_REFERENCE)
                    }
                    if let Ok(history) = host.history(ref_sid, None, 1024).await {
                        let events: Vec<serde_json::Value> = history
                            .events
                            .into_iter()
                            .map(|e| {
                                let translated = e.event.clone();
                                serde_json::json!({
                                    "role": translated.data["role"],
                                    "content": translated.data["content"],
                                })
                            })
                            .collect();
                        let snap = super::reference::session_snapshot(
                            &[super::reference::SnapshotSession {
                                session_id: ref_sid.clone(),
                                label: label.clone(),
                                retained: 0,
                                original: 0,
                            }],
                            &events,
                        );
                        if snap.contains("sessionId") {
                            // id 由 host 侧驱动补 v7;此处仅提供内容与 source 染色
                            contexts.push(serde_json::json!({
                                "content": snap,
                                "source": {
                                    "kind": "session-reference",
                                    "form": "recall",
                                    "references": [{
                                        "sessionId": ref_sid,
                                        "label": label,
                                    }],
                                },
                            }));
                        }
                    }
                }
            }
            host.prompt_with_contexts(&sid, &content, &mode, contexts)
                .await
        });
        let store = cx.entity().clone();
        let sid = id;
        cx.spawn(async move |_this, cx| {
            let rpc = rx.await;
            // fail-loud:prompt 被拒(会话 worker 退出等)此前完全静默,
            // UI 只表现为「发送无反应」
            if let Ok(Err(e)) = &rpc {
                eprintln!("[dsh-desktop] prompt 被拒: {} ({})", e.message, e.code);
            }
            match rpc {
                // 成功:草稿图片已提交,清空(失败保留——源 sendFailed 语义)
                Ok(Ok(_)) => {
                    store.update(cx, |s, cx| {
                        s.attachments.draft_images.clear();
                        cx.notify();
                    });
                }
                Ok(Err(e)) => {
                    // 附件准入被拒(attachment-error):源 image.sendFailed ——
                    // 展示 reason 中文映射,清空失败草稿 + 复位 running
                    // (图片未进 turn,无 turn/end;不复位发送钮会永卡红色停止态)
                    if e.code == "attachment-error" {
                        let reason = e.details["reason"].as_str().unwrap_or_default();
                        store.update(cx, |s, cx| {
                            s.attachments.draft_images.clear();
                            if !is_command {
                                s.state.running_by_id.insert(sid.clone(), false);
                            }
                            s.attachments.attachment_toast = Some(AttachmentToast {
                                text: s.attachment_error_text(reason),
                            });
                            cx.notify();
                        });
                    } else if !is_command {
                        store.update(cx, |s, cx| {
                            s.state.running_by_id.insert(sid.clone(), false);
                            cx.notify();
                        });
                    }
                }
                Err(_) => {}
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
        cx.notify();
    }

    pub fn cancel_current(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.state.current_id.clone() {
            // 子代理会话的 turn 跑在父 worker 的驻留任务里,自身令牌
            // 取消无效——分流到宿主打断(与 interrupt_agent 同信号)。
            // 在看谁就停谁。
            let is_subagent = self.state.sessions.iter().any(|s| {
                s.session_id == id && s.origin.as_deref() == Some("subagent")
            });
            if is_subagent {
                self.bridge.host().interrupt_subagent(&id);
            } else {
                self.bridge.host().cancel_session(&id);
            }
        }
        cx.notify();
    }

    /// 当前会话上下文占用(圆环/详情面板;首个 LLM 请求前 None)
    pub fn context_occupancy(&self) -> Option<ContextOccupancy> {
        let id = self.state.current_id.as_deref()?;
        let stats = self.stats_by_id.get(id)?;
        let used = stats["contextUsed"].as_u64()?;
        if used == 0 {
            return None;
        }
        let window = stats["contextWindow"].as_u64().unwrap_or(1_000_000);
        let b = &stats["contextBreakdown"];
        Some(ContextOccupancy {
            used,
            window,
            percent: (used as f64 / window as f64).clamp(0.0, 1.0),
            system: b["systemTokens"].as_u64().unwrap_or(0),
            tools: b["toolsTokens"].as_u64().unwrap_or(0),
            messages: b["messageTokens"].as_u64().unwrap_or(0),
        })
    }

    /// composer 下拉开关(互斥;同菜单再点 = 关)
    /// 设置命令行待发送态(命令菜单点选;composer 渲染命令行)
    pub fn set_pending_command(&mut self, name: &str, cx: &mut Context<Self>) {
        self.chat.pending_command = Some(PendingCommand {
            name: name.to_string(),
        });
        cx.notify();
    }

    /// 移除命令行(× 钮;输入框参数保留为普通文本)
    pub fn clear_pending_command(&mut self, cx: &mut Context<Self>) {
        self.chat.pending_command = None;
        cx.notify();
    }

    /// 队列条带折叠切换
    pub fn toggle_queue_dock_collapse(&mut self, cx: &mut Context<Self>) {
        self.chat.queue_dock_collapsed = !self.chat.queue_dock_collapsed;
        cx.notify();
    }

    /// 进入队列条目行内编辑(预填原文;仅纯文本条目可编辑)
    pub fn queue_begin_edit(
        &mut self,
        session_id: &str,
        item_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self
            .state
            .chats
            .get(session_id)
            .and_then(|c| c.queue.iter().find(|e| e.id == item_id))
            .and_then(|e| e.text.clone())
            .unwrap_or_default();
        if self.chat.queue_edit_input.is_none() {
            self.chat.queue_edit_input = Some(cx.new(|cx| InputState::new(window, cx)));
        }
        if let Some(input) = &self.chat.queue_edit_input {
            input.update(cx, |s, cx| s.set_value(&text, window, cx));
        }
        self.chat.queue_editing = Some(item_id.to_string());
        cx.notify();
    }

    /// 取消行内编辑
    pub fn queue_cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.chat.queue_editing = None;
        cx.notify();
    }

    /// 保存行内编辑(update_queue edit;帧回填新值)
    pub fn queue_save_edit(&mut self, session_id: &str, item_id: &str, cx: &mut Context<Self>) {
        let text = self
            .chat
            .queue_edit_input
            .as_ref()
            .map(|i| i.read(cx).value().trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            return;
        }
        self.queue_action(
            session_id,
            item_id,
            serde_json::json!({
                "kind": "edit",
                "content": [ { "type": "text", "text": text } ]
            }),
            cx,
        );
        self.chat.queue_editing = None;
        cx.notify();
    }

    /// 队列条目变更(steer 立即投递 / remove 移除 / edit 编辑);
    /// 变更后 session/queue 帧自动广播,UI 无需手动刷。失败入聊天区
    /// 通告(队列是聊天流功能,此处走聊天区是正位)
    pub fn queue_action(
        &mut self,
        session_id: &str,
        item_id: &str,
        action: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let store = cx.entity().clone();
        let host = self.bridge.host().clone();
        let sid = session_id.to_string();
        let iid = item_id.to_string();
        let rx = self
            .bridge
            .call(async move { host.update_queue(&sid, &iid, &action).await });
        cx.spawn(async move |_this, cx| {
            let result = rx.await.unwrap_or_else(|e| {
                Err(dsh_core::proto::RpcError {
                    code: "cancelled".into(),
                    message: format!("{e}"),
                    details: serde_json::Value::Null,
                })
            });
            store.update(cx, |s, cx| {
                if let Err(e) = result {
                    s.push_local_notice(&format!("队列操作失败:{}", e.message), cx);
                }
            });
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    pub fn set_composer_menu(&mut self, menu: ComposerMenu, cx: &mut Context<Self>) {
        self.chat.composer_menu = if self.chat.composer_menu == menu {
            ComposerMenu::None
        } else {
            menu
        };
        cx.notify();
    }

    /// 产物行:打开文件(workspace-relative 按当前工作区根解析;系统 open)
    pub fn open_deliverable(&self, path: &str, cx: &mut Context<Self>) {
        let Some(root) = self.current_workspace_dir() else {
            return;
        };
        let p = std::path::Path::new(path);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        };
        cx.open_with_system(&abs);
    }

    /// @ 补全:从 composer_input 的 text/cursor 探测 hit 并构造候选。
    /// 在订阅 `InputEvent::Change` 时调用(每次文本/光标变化)。
    pub fn update_at_completion(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.chat.composer_input.clone() else {
            self.chat.at_completion = None;
            return;
        };
        let (text, caret) = input.update(cx, |s, _| (s.text().to_string(), s.cursor()));
        // 有激活 token 才弹菜单;否则关
        let Some(hit) = super::reference::at_hit(&text, caret) else {
            self.chat.at_completion = None;
            cx.notify();
            return;
        };
        // 文件候选(quoted 与非 quoted 均列文件;quoted 时不列会话)
        let files = self.file_candidates(&hit.token.query);
        let sessions = if hit.token.quoted {
            Vec::new()
        } else {
            self.session_candidates(&hit.token.query)
        };
        self.chat.at_completion = Some(AtCompletion {
            hit,
            files,
            sessions,
            highlight: 0,
        });
        cx.notify();
    }

    /// @file 候选:query 含 `/` → 目录下钻(实时列目录);否则 fuzzy 全盘。
    fn file_candidates(&self, query: &str) -> Vec<super::reference::FileCandidate> {
        let Some(root) = self.current_workspace_dir() else {
            return Vec::new();
        };
        if !query.contains('/') && !query.is_empty() {
            // 无斜杠的模糊查询:全盘扫描 + fuzzy 排名
            let cands = super::reference::scan_workspace(&root);
            return super::reference::rank_file_candidates(query, &cands);
        }
        // 目录下钻:query 指明目录前缀(如 `src/`),列该目录的下一层
        let (base_prefix, _) = match query.rsplit_once('/') {
            Some((d, _)) => (d, ()),
            None => ("", ()),
        };
        let dir_rel = if query.ends_with('/') {
            query.trim_end_matches('/').to_string()
        } else {
            base_prefix.to_string()
        };
        let dir = root.join(&dir_rel);
        let base = if dir_rel.is_empty() {
            String::new()
        } else {
            format!("{dir_rel}/")
        };
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let is_dir = e.path().is_dir();
                if is_dir && super::reference::EXCLUDED_DIRS.contains(&name.as_str()) {
                    continue;
                }
                let path = format!("{base}{name}");
                // query 指明完整文件路径前缀时,仅保留以 query 开头的
                if query.ends_with('/') || path.starts_with(query) {
                    out.push(super::reference::FileCandidate { path, is_dir });
                }
            }
        }
        out
    }

    /// @session 候选:list_sessions(排除自身),label=标题(projections.title)。
    fn session_candidates(&self, query: &str) -> Vec<super::reference::SessionCandidate> {
        let cid = self.state.current_id.clone();
        let default = self.default_workspace();
        let mut out = Vec::new();
        for s in self.state.sessions.iter() {
            if Some(&s.session_id) == cid.as_ref() {
                continue; // 排除自身
            }
            let ws = crate::shell::reducer::workspace_of(&s.session_id, &default);
            let label = s
                .projections
                .as_ref()
                .and_then(|p| p.values["title"].as_str())
                .filter(|t| !t.is_empty())
                .map(String::from)
                .unwrap_or_else(|| s.session_id.clone());
            out.push(super::reference::SessionCandidate {
                session_id: s.session_id.clone(),
                label,
                same_cwd: ws == default,
            });
        }
        super::reference::rank_session_candidates(query, None, &out)
    }

    /// 选中 @ 补全项:替换 token → set_value + 光标定位。
    /// `replacement` = 选中后插入的 mention 文本(如 `@main.rs` /
    /// `@[会话名](dsh-session:...)`)。光标置于 mention 之后。
    pub fn select_at_completion(
        &mut self,
        replacement: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(at) = self.chat.at_completion.clone() else {
            return;
        };
        let start = at.hit.start;
        let end = at.hit.caret;
        let Some(input) = self.chat.composer_input.clone() else {
            return;
        };
        let current = input.read(cx).text().to_string();
        // 替换 token[..start] 与 [end..) → mention + 尾随空格
        let before = &current[..start.min(current.len())];
        let after = &current[end.min(current.len())..];
        let new_text = format!("{before}{replacement} {after}");
        let new_cursor = (before.len() + replacement.len() + 1).min(new_text.len());
        input.update(cx, |s, cx| {
            s.set_value(new_text.clone(), window, cx);
            let pos = s.text().offset_to_position(new_cursor);
            s.set_cursor_position(pos, window, cx);
        });
        self.chat.at_completion = None;
        cx.notify();
    }

    /// 键盘导航(↑↓)
    pub fn navigate_at_completion(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(at) = self.chat.at_completion.as_mut() else {
            return;
        };
        let files = at.files.len();
        let sessions = at.sessions.len();
        let total = files + sessions;
        if total == 0 {
            return;
        }
        at.highlight = ((at.highlight as isize + delta).rem_euclid(total as isize)) as usize;
        cx.notify();
    }

    /// 完成 @ 补全:Enter 选中高亮项(有候选)或关菜单(无候选)。
    /// 渲染层 flush 调用(需 window;`enter_at_completion` 标志消费)。
    pub fn finish_at_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.chat.enter_at_completion {
            return;
        }
        self.chat.enter_at_completion = false;
        let Some(at) = self.chat.at_completion.clone() else {
            return;
        };
        let files = at.files.len();
        let highlight = at.highlight;
        if files > 0 && highlight < files {
            let path = at.files[highlight].path.clone();
            self.select_at_completion(&super::reference::file_mention(&path), window, cx);
        } else if highlight >= files && highlight - files < at.sessions.len() {
            let s = &at.sessions[highlight - files];
            let mention = super::reference::session_mention(&s.label, &s.session_id);
            self.select_at_completion(&mention, window, cx);
        } else {
            self.chat.at_completion = None;
            cx.notify();
        }
    }

    /// Esc 关 @ 补全
    pub fn cancel_at_completion(&mut self, cx: &mut Context<Self>) {
        if self.chat.at_completion.take().is_some() {
            cx.notify();
        }
    }

    /// 复制消息文本(剪贴板 + Copy→Check 反馈;1.2s 后清除——空闲期
    /// 定时器可能不唤醒主循环,Check 残留到下次交互,无害)
    pub fn copy_message(&mut self, key: &str, text: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text.to_string()));
        self.chat.copied_key = Some(key.to_string());
        let expect = key.to_string();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1200))
                .await;
            this.update(cx, |s, cx| {
                if s.chat.copied_key.as_deref() == Some(expect.as_str()) {
                    s.chat.copied_key = None;
                    cx.notify();
                }
            })?;
            Ok::<(), anyhow::Error>(())
        })
        .detach();
        cx.notify();
    }

    /// 工具行展开/折叠
    pub fn toggle_tool(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.chat.expanded_tools.insert(key.to_string()) {
            self.chat.expanded_tools.remove(key);
        }
        cx.notify();
    }

    /// 工具卡头尾展开钮(read/search/diff 卡内「… 其余 N 行」)
    pub fn toggle_card_expanded(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.chat.card_expanded.insert(key.to_string()) {
            self.chat.card_expanded.remove(key);
        }
        cx.notify();
    }

    /// 搜索卡文件组折叠(键 = `{callKey}:{path}`)
    pub fn toggle_search_group(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.chat.search_collapsed.insert(key.to_string()) {
            self.chat.search_collapsed.remove(key);
        }
        cx.notify();
    }

    /// Think 行展开/折叠
    pub fn toggle_reasoning(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.chat.open_reasoning.insert(key.to_string()) {
            self.chat.open_reasoning.remove(key);
        }
        cx.notify();
    }

    /// 上下文注入行展开/折叠
    pub fn toggle_context(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.chat.open_context.insert(key.to_string()) {
            self.chat.open_context.remove(key);
        }
        cx.notify();
    }

    /// LLM 重试行展开/折叠
    pub fn toggle_retry(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.chat.open_retries.insert(key.to_string()) {
            self.chat.open_retries.remove(key);
        }
        cx.notify();
    }

    /// 重试倒计时节拍(照 shell sync_run_tick 模式):存在未来截止的
    /// 等待行才保活 1s 循环。秒数在渲染期由截止时刻推算,timer 只负责
    /// 触发重绘;状态翻转(started/取消)自带 notify,节拍自会收敛退出
    pub(crate) fn sync_retry_tick(&mut self, cx: &mut Context<Self>) {
        let pending = self
            .current_chat()
            .is_some_and(|c| c.retry_deadlines.values().any(|d| *d > std::time::Instant::now()));
        if !pending {
            self.chat.retry_tick.take();
            return;
        }
        if self.chat.retry_tick.is_some() {
            return;
        }
        self.chat.retry_tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                // 实体已销毁或无等待行 → 退出循环(update 返回 Err)
                let gone = this
                    .update(cx, |s, cx| {
                        let pending = s.current_chat().is_some_and(|c| {
                            c.retry_deadlines.values().any(|d| *d > std::time::Instant::now())
                        });
                        cx.notify();
                        !pending
                    })
                    .unwrap_or(true);
                if gone {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// TodoDock 展开/折叠
    pub fn toggle_todo(&mut self, cx: &mut Context<Self>) {
        self.chat.todo_open = !self.chat.todo_open;
        cx.notify();
    }

    /// 执行 slash 命令(host 直接执行,非发模型)。
    /// `/export` 结果含 ZIP base64 → 落 ~/Downloads;其余命令把结果文本
    /// 呈现为本地通知行。
    pub fn execute_command(&mut self, line: &str, cx: &mut Context<Self>) {
        let Some(id) = self.state.current_id.clone() else {
            return;
        };
        let host = self.bridge.host().clone();
        let sid = id.clone();
        let sid_cmd = sid.clone(); // 闭包内 execute;sid 供 export 分支
        let line_o = line.to_string();
        let rx = self
            .bridge
            .call(async move { host.execute_command(&sid_cmd, &line_o).await });
        let store = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            let rpc = rx.await;
            match rpc {
                Ok(Ok(v)) => {
                    if v["kind"].as_str() == Some("export") {
                        // ZIP base64 → Downloads
                        use base64::Engine as _;
                        if let Some(data) = v["data"].as_str()
                            && let Ok(bytes) =
                                base64::engine::general_purpose::STANDARD.decode(data)
                        {
                            let safe = sid.replace('/', "-");
                            let home = std::env::var_os("HOME")
                                .map(std::path::PathBuf::from)
                                .unwrap_or_default();
                            let path = home
                                .join("Downloads")
                                .join(format!("dsh-session-{safe}.zip"));
                            if std::fs::write(&path, bytes).is_ok() {
                                store.update(cx, |s, cx| {
                                    s.push_local_notice(&format!("已导出:{}", path.display()), cx);
                                });
                            }
                        }
                    } else if v.get("mode").is_some() || v.get("accepted").is_some() {
                        // 模式/开关切换:状态由 chip 与界面体现,不进聊天区
                    } else {
                        let text = v
                            .get("model")
                            .and_then(|x| x.as_str())
                            .map(|m| format!("当前: {m}"))
                            .unwrap_or_else(|| "done".to_string());
                        store.update(cx, |s, cx| {
                            s.push_local_notice(&text, cx);
                        });
                    }
                }
                Ok(Err(e)) => {
                    store.update(cx, |s, cx| {
                        s.push_local_notice(&format!("命令失败:{}", e.message), cx);
                    });
                }
                Err(_) => {}
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 当前会话投影(无则 None)
    pub fn current_chat(&self) -> Option<&ChatState> {
        self.state
            .current_id
            .as_deref()
            .and_then(|id| self.state.chats.get(id))
    }

    /// 当前消息流节点(借用便捷)
    pub fn current_nodes(&self) -> &[ChatNode] {
        self.current_chat()
            .map(|c| c.nodes.as_slice())
            .unwrap_or(&[])
    }
}
