//! Mermaid 图渲染(markdown ```mermaid 围栏 → SVG → gpui 光栅)。
//!
//! 引擎 merman `=0.8.0-alpha.5`(锁死防漂移;与 zed Cargo.lock 同版同
//! checksum,接口同 zed `crates/mermaid_render/src/render.rs` 形状。
//! 弃选 1jehuang/mermaid-rs-renderer:字体解析错误致文字不显示、
//! 类图/甘特图缺陷与 SVG 样式偏差;换引擎备选 mmdr(其 CLI 名)。
//! 引擎接触面隔离在本文件)。
//!
//! 管线:源码 → merman(站点配置 + vendored 文本测量)→ resvg_safe 管线
//! (foreignObject → 原生 SVG 文本、非法 CSS 清理、`!important` 剥离,
//! gpui/usvg 可直读)→ `parse_svg` → `render_parsed`(2x 设备光栅;
//! `img` element 按 render_size 布局 = 自然逻辑尺寸;内嵌图 `max_w_full`
//! 等比收窄至列宽——fit-width 自适应,窄图不放大,超宽溢出留滚动护栏;
//! 查看器按倍率重光栅走 [`raster_at_zoom`])。
//!
//! 缓存双层:SVG 失败态(hash 集合,防重复重算)与光栅结果(含失败态,
//! 防逐帧重跑 → fallback jank 循环)。光栅缓存**每 key 单条目**(缩放
//! 替换不累积)+ 按字节预算约束总量;被替换/被驱逐的图经
//! `App::drop_image` 回收 sprite atlas 纹理(gpui 的 atlas 按 ImageId
//! 持有纹理,不主动回收则每次重光栅都累积一块 GPU/进程内存——放大
//! 查看器几下滚轮即可涨数 GB)。**例外**:字节预算超限的整体清空不回
//! 收——同帧先渲染的其他卡片可能仍在绘制被清条目,显式回收会缺纹理;
//! 该路径仅在多图总量超 128MB 时触达,增长缓慢。

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use gpui_kit::{
    AnyElement, App, FontWeight, ImageCacheError, ImageSource, InteractiveElement, IntoElement,
    ParentElement, Rgba, SharedString, StatefulInteractiveElement, Styled, StyledImage, Window,
    div, img, px,
};
use gpui_kit::component::IconName;

use super::icons::{DshIcon, fixed};
use super::theme;
use super::theme::{palette_of, Palette};

/// 卡片控件态快照(消息流 per-card map;kits 无状态,不直接引用
/// AppStore——宿主注入状态与动作,本模块只按快照渲染)。
#[derive(Debug, Clone, Default)]
pub(crate) struct MermaidCardState {
    /// true = 代码模式
    pub show_code: bool,
    /// 「复制」反馈窗内(按钮绿色「已复制」;store 侧 detach 任务清位)
    pub copied: bool,
}

/// 「复制」按钮反馈窗(绿色「已复制」→ 复原;store 定时任务共用)
pub(crate) const MERMAID_COPY_FEEDBACK: std::time::Duration =
    std::time::Duration::from_millis(1600);

/// 卡片动作钩子(宿主经闭包注入;kits 只调不实现,保持无状态)。
/// 动作参数:卡片 key(`{prefix}-md-mermaid-{ix}`)、图源码。
/// 每条动作均为 `&'static` 式宿主回调(**统一收 `&mut Window`**:复制/下载
/// 等动作要在窗口上弹通知/取焦点,须能触达 window——gpui-component 的
/// `push_notification` 是 `WindowExt` 方法);type 别名拉平 clippy::type_complexity。
pub(crate) type ToggleCodeCb = Arc<dyn Fn(&str, &mut Window, &mut App) + Send + Sync>;
pub(crate) type CopyCb = Arc<dyn Fn(&str, Arc<str>, &mut Window, &mut App) + Send + Sync>;
pub(crate) type EnlargeCb = Arc<dyn Fn(&str, Arc<str>, &mut Window, &mut App) + Send + Sync>;
pub(crate) type DownloadCb = Arc<dyn Fn(&str, Arc<str>, &mut Window, &mut App) + Send + Sync>;

#[derive(Clone)]
pub(crate) struct MermaidCardCallbacks {
    /// 图表/代码切换
    pub toggle_code: ToggleCodeCb,
    /// 复制源码进剪贴板(参数 = 该卡图源码)
    pub copy: CopyCb,
    /// 放大(打开查看器;参数 = 该卡图源码)
    pub enlarge: EnlargeCb,
    /// 下载 PNG(参数 = 源码)
    pub download: DownloadCb,
}

impl MermaidCardCallbacks {
    /// 非交互调用点(轨迹/计划页等无查看器场景):全部空动作,卡片无工具条。
    #[allow(dead_code)]
    pub fn none() -> Self {
        Self {
            toggle_code: Arc::new(|_, _, _| {}),
            copy: Arc::new(|_, _, _, _| {}),
            enlarge: Arc::new(|_, _, _, _| {}),
            download: Arc::new(|_, _, _, _| {}),
        }
    }
}

/// 单条消息的 mermaid 卡片集:动作钩子 + 每卡片 key 的状态快照。
/// 宿主在 `assistant_block`(有 `cx`)读 store 组装后传入渲染链;`diagram`
/// 按自身 key 查快照——渲染链无需 `cx`。
#[derive(Clone)]
pub(crate) struct MermaidCards {
    pub states: std::collections::HashMap<String, MermaidCardState>,
    pub callbacks: MermaidCardCallbacks,
}

impl MermaidCards {
    /// 按卡片 key 解析出单张卡的挂载上下文(状态 + 动作)。
    pub fn ctx_for(&self, key: &str) -> MermaidCardCtx {
        MermaidCardCtx {
            state: self.states.get(key).cloned().unwrap_or_default(),
            callbacks: self.callbacks.clone(),
        }
    }
}

/// 单张卡片挂载上下文(状态快照 + 动作钩子)。
pub(crate) struct MermaidCardCtx {
    pub state: MermaidCardState,
    pub callbacks: MermaidCardCallbacks,
}

/// 源码长度上限(merman 渲染开销随游标规模增长;超限直接降级代码块,
/// 防长图/恶意图卡死消息流)
const MAX_SOURCE_BYTES: usize = 64 * 1024;

/// 卡片光栅缓存字节预算(超限整体清空,同 parse 缓存模式)。须大于
/// **单条**大图光栅的峰值(8192² 预乘 ≈ 268MB):预算低于单条时 put
/// 每次触发 clear_all → 自然重渲染 → 再 clear_all 的「预算阈值陷阱」,
/// 每轮泄漏一张纹理直至爆内存(放大大图即 7G 的第二轮根因;此前按
/// 条数封顶同样会累积)。512MB = 单条峰值 × 1.9,只兜异常场景——正常
/// 路径卡片走单条目替换、查看器走视口光栅(≤ 视口 ~ 60MB),远达不到。
const RASTER_CAP_BYTES: usize = 512 * 1024 * 1024;

/// 白名单:仅这些前缀渲染为图(merman 尚支持 beta 型,但未审样式,
/// 一律降级代码块防半成品观感)。若更新列表,同步本文件头注释。
const SUPPORTED_PREFIXES: &[&str] = &[
    "flowchart",
    "graph",
    "sequencediagram",
    "classdiagram",
    "statediagram",
    "statediagram-v2",
    "erdiagram",
    "gantt",
    "pie",
    "gitgraph",
    "mindmap",
    "timeline",
    "quadrantchart",
    "xychart-beta",
    "journey",
];

/// 源首词是否白名单图表类型(zed `is_supported_diagram_type` 语义:
/// 取首 token,大小写不敏感)
pub(crate) fn is_supported_diagram_type(source: &str) -> bool {
    let first_token = source
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '\n')
        .next()
        .unwrap_or("");
    SUPPORTED_PREFIXES
        .iter()
        .any(|prefix| first_token.eq_ignore_ascii_case(prefix))
}

/// 源码 → SVG 字符串(纯管线,无 gpui 依赖,可单元测试):
/// 限长 → 失败记忆 → 成功缓存 → `catch_unwind` → merman。任何失败 None
/// 并写入失败记忆(渲染失败也短路——否则查看器每帧首解重跑 merman)。
/// 成功缓存必需:查看器自然尺寸解析每帧调用,不缓存 = 每帧重跑 merman
/// 布局(拖拽/缩放期直接卡死);渲染路径(光栅 miss)同样受益。
pub(crate) fn svg_for(source: &str) -> Option<Arc<String>> {
    if source.len() > MAX_SOURCE_BYTES {
        return None;
    }
    let hash = hash_of(source);
    if FAILED_SOURCES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .contains(&hash)
    {
        return None;
    }
    if let Some(svg) = SVG_CACHE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&hash)
    {
        return Some(svg.clone());
    }
    let Some(svg) = catch_unwind(AssertUnwindSafe(|| render_svg(source)))
        .ok()
        .flatten()
    else {
        let mut failed = FAILED_SOURCES
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if failed.len() >= 64 {
            failed.clear();
        }
        failed.insert(hash);
        return None;
    };
    let svg = Arc::new(svg);
    let mut cache = SVG_CACHE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if cache.len() >= SVG_CACHE_CAP {
        cache.clear();
    }
    cache.insert(hash, svg.clone());
    Some(svg)
}

/// SVG 成功缓存(源哈希 → SVG 串;超限整体清空,同失败记忆模式)
static SVG_CACHE: LazyLock<Mutex<HashMap<u64, Arc<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const SVG_CACHE_CAP: usize = 32;

/// 图的自然逻辑尺寸:**免光栅**——直接解析 SVG 根元素 width/height
/// (merman 输出 px 数值;viewBox 兜底)。此前为拿这两个数字渲染一张
/// zoom=1 整图光栅,大图 = 8192² ≈ 268MB 纯浪费。
pub(crate) fn natural_size(source: &str) -> Option<(f32, f32)> {
    let hash = hash_of(source);
    if let Some(size) = NAT_SIZES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&hash)
    {
        return Some(*size);
    }
    let svg = svg_for(source)?;
    let size = parse_svg_natural_size(&svg)?;
    let mut sizes = NAT_SIZES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if sizes.len() >= 64 {
        sizes.clear();
    }
    sizes.insert(hash, size);
    Some(size)
}

static NAT_SIZES: LazyLock<Mutex<HashMap<u64, (f32, f32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn parse_svg_natural_size(svg: &str) -> Option<(f32, f32)> {
    // 根元素属性尺寸:仅当 width/height 都是**绝对 px 数值**才采用。
    // merman 恒输出 width="100%"(相对值)+ viewBox(真实布局尺寸):
    // 百分比必须整体弃用走 viewBox —— 逐字符截数字会把 "100%" 误读成
    // 100px,再混搭 viewBox 的高度 = 完全错误的纵横比(超宽图曾因此
    // 被量成 100×130)。
    let attr = |name: &str| -> Option<f32> {
        let pat = format!("{name}=\"");
        let i = svg.find(&pat)? + pat.len();
        let raw: String = svg[i..].chars().take_while(|c| *c != '"').collect();
        let num = raw.trim().trim_end_matches("px").trim();
        num.parse::<f32>()
            .ok()
            .filter(|n| *n > 0.0 && n.is_finite())
    };
    if let (Some(w), Some(h)) = (attr("width"), attr("height")) {
        return Some((w, h));
    }
    // viewBox="minX minY w h" 兜底(与根 style max-width 同值 = 布局真实尺寸)
    let i = svg.find("viewBox=\"")? + "viewBox=\"".len();
    let raw: String = svg[i..].chars().take_while(|c| *c != '"').collect();
    let nums: Vec<f32> = raw
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    if nums.len() == 4 && nums[2] > 0.0 && nums[3] > 0.0 {
        return Some((nums[2], nums[3]));
    }
    None
}

fn render_svg(source: &str) -> Option<String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let renderer = merman::svg::HeadlessRenderer::new()
        .with_site_config(dsh_mermaid_config())
        .with_vendored_text_measurer()
        .with_diagram_id(&format!("merman-{id}"));
    // resvg_safe:foreignObject 折叠/非法 CSS/无效属性清理(usvg 直读);
    // 再剥 merman 自带 !important,宿主注入样式优先级唯一
    // RootBackground:merman 把根元素写死 `background-color:white`
    // (themeVariables.background 不达根样式),usvg 直读该样式 →
    // 白底。改写为 BASE 合成色(不透明)。
    let pipeline = merman::svg::SvgPipeline::resvg_safe()
        .with_postprocessor(merman::svg::CssOverridePostprocessor::strip_existing_important())
        .with_postprocessor(merman::svg::RootBackgroundPostprocessor::new(hex(
            theme::CODE(),
        )));
    renderer
        .render_svg_with_pipeline_sync(source, &pipeline)
        .ok()
        .flatten()
}

// ── 主题映射(dsh 纯暗色板 → mermaid themeVariables)─────────────

/// 节点级元素:代码块 chrome + 惰性光栅图。调用方已滤过白名单;
/// 渲染失败时 img fallback 为纯代码文本(与代码块回退同款式)。
/// `cards` = 卡片集(动作钩子 + 每 key 状态快照);`None` 时不渲染
/// 工具条(轨迹/计划页等无查看器场景)。复制/放大/下载等动作全部落在
/// 卡片上——查看器是纯图,不把控件带进去。
pub(crate) fn diagram(
    prefix: &str,
    ix: usize,
    source: Arc<str>,
    cards: Option<MermaidCards>,
) -> AnyElement {
    let id = SharedString::from(format!("{prefix}-md-mermaid-{ix}"));
    let key = id.to_string();
    // 按自身 key 解析本卡上下文;无 cards → 无控件(轨迹/计划页)
    let ctx = cards.as_ref().map(|c| c.ctx_for(&key));
    let have_ctx = ctx.is_some();
    let show_code = ctx.as_ref().map(|c| c.state.show_code).unwrap_or(false);
    let copied = ctx.as_ref().map(|c| c.state.copied).unwrap_or(false);
    let callbacks = ctx.map(|c| c.callbacks);
    let src = source.clone();
    let fallback_src = source.clone();

    // 图表体:闭合 + 白名单已过;显示态(图表/代码)经卡片工具条切换。
    // `on_click` 只挂在 body 上——工具条按钮自处理点击,不冒泡到卡片。
    let body: AnyElement = if show_code {
        div()
            .id(format!("{key}-code"))
            .font_family("Menlo")
            .text_size(px(13.))
            .text_color(theme::LABEL_2())
            .line_height(gpui_kit::relative(1.5))
            .cursor_pointer()
            .on_click({
                let callbacks = callbacks.clone();
                let key = key.clone();
                move |_ev, w, cx| {
                    if let Some(cb) = &callbacks {
                        (cb.toggle_code)(&key, w, cx); // 代码态点击 → 回图表
                    }
                }
            })
            .child("```mermaid\n".to_string() + &source)
            .child("```")
            .into_any_element()
    } else {
        // 内嵌图沿用 max_w_full fit-width 自适应(天然按 render_size 逻辑
        // 宽布局,不模糊);光栅固定 1.0 档(与内嵌预览共用缓存键),
        // 超宽由外层 overflow_x_scroll 护栏承接。
        div()
            .id(format!("{key}-figure"))
            .debug_selector(|| format!("{prefix}-md-mermaid-figure-{ix}"))
            .cursor_pointer()
            .on_click({
                let callbacks = callbacks.clone();
                let key = key.clone();
                let source = source.clone();
                move |_ev, w, cx| {
                    if let Some(cb) = &callbacks {
                        (cb.enlarge)(&key, source.clone(), w, cx); // 图表态点击图 = 放大
                    }
                }
            })
            .child(
                img(ImageSource::Custom({
                    let key = key.clone();
                    let src = src.clone();
                    Arc::new(move |_window, cx| Some(raster_at_zoom(&key, &src, cx, 1.0)))
                }))
                // 显示宽 = render_size = 自然逻辑宽;仅超列宽时
                // max_w_full 收窄到列宽(横向护栏承接)——窄图保持
                // 固有宽,不撑爆卡片
                .max_w_full()
                .with_fallback(move || {
                    div()
                        .font_family("Menlo")
                        .text_size(px(13.))
                        .text_color(theme::LABEL_2())
                        .child(fallback_src.to_string())
                        .into_any_element()
                })
                .into_any_element(),
            )
            .into_any_element()
    };

    let el = div()
        .id(id)
        .debug_selector(|| format!("{prefix}-md-mermaid-{ix}"))
        .mb(px(8.))
        .w_full()
        // 只留横向护栏(放大超宽时可横滚);纵向交给消息列——卡片不
        // 抢纵向滚轮,否则与 ListState 滚动手势冲突、滚轮被吞成卡顿
        .overflow_x_scroll()
        .rounded(px(12.))
        .bg(theme::CODE())
        .p(px(10.))
        .child(if have_ctx {
            let cb = callbacks.as_ref().expect("have_ctx → callbacks");
            card_toolbar(&key, show_code, copied, cb, source.clone()).into_any_element()
        } else {
            div().into_any_element()
        })
        .child(body);

    el.into_any_element()
}

/// 卡片工具条(仅交互调用点):图表/代码分段 + 复制 + 下载 + 放大。
/// 全部动作经宿主注入的 callbacks(收图源码);kits 只按快照渲染。
/// 复制反馈 = 按钮本体态切换(✓ 已复制,绿色),非异步通知。
fn card_toolbar(
    key: &str,
    show_code: bool,
    copied: bool,
    callbacks: &MermaidCardCallbacks,
    source: Arc<str>,
) -> impl IntoElement {
    let key = key.to_string();
    let toggle_code = callbacks.toggle_code.clone();
    let copy_cb = callbacks.copy.clone();
    let enlarge_cb = callbacks.enlarge.clone();
    let download_cb = callbacks.download.clone();
    div()
        .debug_selector(|| format!("{key}-toolbar"))
        .flex()
        .items_center()
        .gap(px(2.))
        .mb(px(8.))
        .child(
            // 图表/代码分段(topbar segmented 样式;全自绘)
            div()
                .flex()
                .items_center()
                .gap(px(2.))
                .rounded_full()
                .bg(theme::LAYER())
                .p(px(2.))
                .child(segment_button(&key, "chart", "图表", !show_code, {
                    let key = key.clone();
                    let f = toggle_code.clone();
                    move |w, cx| f(&key, w, cx)
                }))
                .child(segment_button(&key, "code", "代码", show_code, {
                    let key = key.clone();
                    let f = toggle_code.clone();
                    move |w, cx| f(&key, w, cx)
                })),
        )
        .child(div().flex_1())
        .child({
            // 复制反馈 = 按钮本体态切换:✓ 已复制(SUCCESS 绿)窗口内;
            // 反馈态点击仍重复复制(重启反馈窗,剪贴板被覆盖后可再取)
            let (id, icon, label, color) = if copied {
                ("copy-done", IconName::Check, "已复制", theme::SUCCESS())
            } else {
                ("copy", IconName::Copy, "复制", theme::LABEL_2())
            };
            toolbar_label_button(&key, id, icon, label, color, {
                let key = key.clone();
                let source = source.clone();
                let f = copy_cb.clone();
                move |w, cx| f(&key, source.clone(), w, cx)
            })
        })
        .child(div().w(px(1.)).h(px(14.)).mx(px(6.)).bg(theme::BORDER_2()))
        .child(toolbar_label_button(
            &key,
            "download",
            DshIcon::Download,
            "下载",
            theme::LABEL_2(),
            {
                let key = key.clone();
                let source = source.clone();
                let f = download_cb.clone();
                move |w, cx| f(&key, source.clone(), w, cx)
            },
        ))
        .child(toolbar_label_button(
            &key,
            "enlarge",
            IconName::Maximize,
            "放大",
            theme::LABEL_2(),
            {
                let key = key.clone();
                let source = source.clone();
                let f = enlarge_cb.clone();
                move |w, cx| f(&key, source.clone(), w, cx)
            },
        ))
        .into_any_element()
}

/// 分段按钮(active = GLASS_BG 高亮,同 topbar segment)。`id` = ASCII
/// 选择器后缀(图表/代码),`label` = 显示文本(中文)。
fn segment_button(
    key: &str,
    id: &'static str,
    label: &'static str,
    active: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let key = key.to_string();
    let sel = format!("{key}-seg-{id}");
    let base = div()
        .id(sel.clone())
        .debug_selector(move || sel.clone())
        .flex()
        .h(px(22.))
        .items_center()
        .justify_center()
        .px(px(12.))
        .rounded_full()
        .cursor_pointer()
        .text_size(px(12.));
    let base = if active {
        base.bg(theme::GLASS_BG())
            .border_1()
            .border_color(theme::GLASS_BORDER())
            .text_color(theme::LABEL())
            .font_weight(FontWeight::MEDIUM)
    } else {
        base.text_color(theme::LABEL_3())
            .hover(|st| st.bg(theme::BORDER()))
    };
    base.on_click(move |_ev, window, cx| on_click(window, cx))
        .child(label.to_string())
}

/// 图标+文字按钮(复制/下载/放大);`color` = 文本色(复制反馈态传
/// SUCCESS 绿)
fn toolbar_label_button(
    key: &str,
    id: &'static str,
    icon: impl Into<gpui_kit::component::Icon>,
    label: &'static str,
    color: Rgba,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let key = key.to_string();
    let sel = format!("{key}-{id}");
    div()
        .id(sel.clone())
        .debug_selector(move || sel.clone())
        .h(px(26.))
        .flex()
        .items_center()
        .gap(px(5.))
        .px(px(8.))
        .rounded(px(8.))
        .cursor_pointer()
        .text_size(px(13.))
        .text_color(color)
        .hover(|st| st.bg(theme::LAYER()))
        .on_click(move |_ev, window, cx| on_click(window, cx))
        .child(fixed(icon, 14.))
        .child(label.to_string())
}

/// mermaid 站点配置(主题映射;键名是 mermaid 主题契约,
/// 色值取 kits::theme 唯一色板源,不另造常量)
fn dsh_mermaid_config() -> merman::MermaidConfig {
    merman::MermaidConfig::from_value(serde_json::json!({
        "theme": "base",
        "darkMode": !theme::is_dark(),
        "fontFamily": "system-ui, sans-serif",
        // htmlLabels 必须为 true:resvg_safe 管线把它折叠为原生 SVG 文本
        "htmlLabels": true,
        "flowchart": { "htmlLabels": true, "padding": 16 },
        "themeVariables": theme_variables(),
    }))
}

/// themeVariables 表(当前盘;纯数据,测试直查)
fn theme_variables() -> serde_json::Value {
    theme_variables_in(palette_of(theme::mode()))
}

/// themeVariables 表(纯函数化:给定盘生成,测试注入浅盘断言)
fn theme_variables_in(p: &Palette) -> serde_json::Value {
    let put = |vars: &mut serde_json::Map<String, serde_json::Value>, key: &str, c: Rgba| {
        vars.insert(key.into(), serde_json::json!(hex_in(c, p.base)));
    };
    let mut vars = serde_json::Map::new();
    put(&mut vars, "primaryColor", p.card);
    put(&mut vars, "primaryTextColor", p.label);
    put(&mut vars, "primaryBorderColor", p.border_2);
    put(&mut vars, "lineColor", p.border_2);
    put(&mut vars, "secondaryColor", p.layer);
    put(&mut vars, "secondaryTextColor", p.label);
    put(&mut vars, "tertiaryColor", p.dock);
    put(&mut vars, "tertiaryTextColor", p.label);
    put(&mut vars, "background", p.code);
    put(&mut vars, "mainBkg", p.card);
    put(&mut vars, "nodeBorder", p.border_2);
    put(&mut vars, "nodeTextColor", p.label);
    put(&mut vars, "clusterBkg", p.layer);
    put(&mut vars, "clusterBorder", p.border_2);
    put(&mut vars, "titleColor", p.label);
    put(&mut vars, "edgeLabelBackground", p.code);
    put(&mut vars, "textColor", p.label);
    put(&mut vars, "noteBkgColor", p.layer);
    put(&mut vars, "noteBorderColor", p.border_2);
    put(&mut vars, "noteTextColor", p.label);
    put(&mut vars, "actorBkg", p.dock);
    put(&mut vars, "actorBorder", p.border_2);
    put(&mut vars, "actorTextColor", p.label_2);
    put(&mut vars, "labelTextColor", p.label);
    put(&mut vars, "loopTextColor", p.label);
    put(&mut vars, "signalColor", p.label);
    put(&mut vars, "signalTextColor", p.label);
    put(&mut vars, "activationBkgColor", p.layer);
    put(&mut vars, "activationBorderColor", p.border_2);
    put(&mut vars, "classText", p.label);
    put(&mut vars, "labelColor", p.label_2);
    put(&mut vars, "attributeBackgroundColorOdd", p.code);
    put(&mut vars, "attributeBackgroundColorEven", p.card);
    put(&mut vars, "pieTitleTextColor", p.label);
    put(&mut vars, "pieSectionTextColor", p.label);
    put(&mut vars, "pieLegendTextColor", p.label);
    put(&mut vars, "pieStrokeColor", p.border_2);
    put(&mut vars, "pieOuterStrokeColor", p.border_2);
    put(&mut vars, "quadrant1Fill", p.card);
    put(&mut vars, "quadrant2Fill", p.card);
    put(&mut vars, "quadrant3Fill", p.card);
    put(&mut vars, "quadrant4Fill", p.card);
    put(&mut vars, "quadrant1TextFill", p.label);
    put(&mut vars, "quadrant2TextFill", p.label);
    put(&mut vars, "quadrant3TextFill", p.label);
    put(&mut vars, "quadrant4TextFill", p.label);
    put(&mut vars, "quadrantPointFill", p.border_2);
    put(&mut vars, "quadrantPointTextFill", p.label);
    put(&mut vars, "quadrantTitleFill", p.label);
    put(&mut vars, "quadrantXAxisTextFill", p.label);
    put(&mut vars, "quadrantYAxisTextFill", p.label);
    put(
        &mut vars,
        "quadrantExternalBorderStrokeFill",
        p.border_2,
    );
    put(
        &mut vars,
        "quadrantInternalBorderStrokeFill",
        p.border_2,
    );
    // gitGraph/pie 的 8 组系列色(与 zed 的 cScale*/pieN 同键;
    // dsh 无分支主题色,取调色板系列一色一轮;运行时取值随主题盘)
    let series = [
        p.brand,
        p.success,
        p.warn,
        p.danger,
        p.ongoing,
        p.label_3,
        p.caption,
        p.border_2,
    ];
    for (i, &c) in series.iter().enumerate() {
        put(&mut vars, &format!("cScale{i}"), c);
        put(&mut vars, &format!("cScaleLabel{i}"), p.label);
        put(&mut vars, &format!("pie{}", i + 1), c);
    }
    serde_json::Value::Object(vars)
}

/// 色值 → mermaid 期望的 `#RRGGBB`;带透明度的色(theme 的 BORDER 系)
/// 按 BASE 底色合成(散图无合成对象,取实体色避免偏白)
fn hex(c: Rgba) -> String {
    hex_in(c, theme::BASE())
}

/// 同上,按指定底色合成(theme_variables_in 纯函数化配套)
fn hex_in(c: Rgba, bg: Rgba) -> String {
    let a = c.a;
    let ch = |channel: f32, base: f32| {
        let v = channel * a + base * (1.0 - a);
        (v * 255.0) as u8
    };
    format!(
        "#{:02X}{:02X}{:02X}",
        ch(c.r, bg.r),
        ch(c.g, bg.g),
        ch(c.b, bg.b),
    )
}

fn hash_of(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

// ── 缓存 ─────────────────────────────────────────────────────

/// 已失败源内容哈希集合(失败态:防重复重算同一源;超限整体清空,
/// 清空后该源可重试——正确性无碍,仅可能短暂重算一次)
static FAILED_SOURCES: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// 光栅缓存条目:图或失败标记(失败同样驻留,防逐帧重跑)
type RasterEntry = Result<Arc<gpui_kit::RenderImage>, ()>;

/// 缩放档位量化步长。滚轮连续乘法会产生近乎唯一的瞬时 zoom,若按
/// `{key}@{zoom:.2}` 逐档缓存,每帧生成一条新光栅并累积 —— 高放大图
/// 单条可到 8192×8192×4 ≈ 268MB,几十帧滚轮即数 GB,触发 OOM。量化后
/// 相邻瞬时 zoom 吸附到同一档,配合下方「每 key 单条目替换」,内存
/// 稳定在一图一条。
const ZOOM_QUANTUM: f32 = 0.05;

/// 把瞬时 zoom 吸附到量化档(±ZOOM_QUANTUM/2 内同档)。乘法步进
/// (1.25/0.8)滚轮缩放都被此吸附,避免跨档抖动重光栅。
pub(crate) fn quantize_zoom(zoom: f32) -> f32 {
    (zoom / ZOOM_QUANTUM).round() * ZOOM_QUANTUM
}

/// 每 key 单条目光栅缓存。`map[key] = (量化档, 源哈希, 条目)`:
/// 同 key 新档**替换**旧档而非累积 —— 这是根治放大 OOM 的关键
/// (对齐 zed `CachedMermaidDiagram` 单条目语义:缩放换旧留一,不逐档堆积)。
/// 额外按 `/[/` 字节预算约束全缓存总量(见 [`RASTER_CAP_BYTES`])。
struct RasterCache {
    map: BTreeMap<String, (f32, u64, RasterEntry)>,
    /// 成功条目的累计帧字节(失败态字节为 0;清空或替换时同步扣减)
    bytes: usize,
}

static RASTER_CACHE: Mutex<RasterCache> = Mutex::new(RasterCache {
    map: BTreeMap::new(),
    bytes: 0,
});

/// 预算清空路径的延迟回收队列(帧中不 drop,见 [`RasterCache::clear_all`])
static PENDING_DROPS: LazyLock<Mutex<Vec<Arc<gpui_kit::RenderImage>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// 帧间回收积压纹理(下一个带 `App` 的触点调用:卡片光栅 / 防抖收尾 /
/// 查看器关闭)。
pub(crate) fn drain_pending_drops(cx: &mut App) {
    let queued: Vec<_> = std::mem::take(
        &mut *PENDING_DROPS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()),
    );
    for image in queued {
        cx.drop_image(image, None);
    }
}

impl RasterCache {
    fn bytes_of(entry: &RasterEntry) -> usize {
        match entry {
            Ok(img) => {
                let size = img.size(0);
                (size.width.0 as usize * size.height.0 as usize) * 4
            }
            Err(()) => 0,
        }
    }

    /// 整体清空并归零字节计数(超预算时)。被清的成功条目推入
    /// [`PENDING_DROPS`]:清空可发生在帧中,同帧先渲染的其他卡片可能
    /// 仍在绘制这些图,立即回收会缺纹理 —— 由下一个带 `App` 的触点在
    /// 帧间 drain(见 [`drain_pending_drops`])。
    fn clear_all(&mut self) {
        let dropped: Vec<_> = self
            .map
            .values()
            .filter_map(|(_, _, e)| e.clone().ok())
            .collect();
        PENDING_DROPS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .extend(dropped);
        self.map.clear();
        self.bytes = 0;
    }

    fn get(&self, key: &str, zoom_q: f32, hash: u64) -> Option<RasterEntry> {
        match self.map.get(key) {
            Some((z, h, e)) if *z == zoom_q && *h == hash => Some(e.clone()),
            _ => None,
        }
    }

    /// 插入条目,返回**同 key 被替换的旧图**(调用方经 `App::drop_image`
    /// 回收其 sprite atlas 纹理——同 key 替换后本帧起由新图上屏,旧图无
    /// 绘制方,回收安全)。预算清空路径驱逐的其他 key 条目**不**返回:
    /// 同帧先渲染的其他卡片可能仍在绘制它们,显式回收会缺纹理。
    fn put(
        &mut self,
        key: &str,
        zoom_q: f32,
        hash: u64,
        entry: RasterEntry,
    ) -> Option<Arc<gpui_kit::RenderImage>> {
        let new_bytes = Self::bytes_of(&entry);
        if self.bytes + new_bytes > RASTER_CAP_BYTES {
            self.clear_all();
        }
        // 同 key 直接覆盖:一图一档,缩放不产生第二格光栅
        let old = self.map.insert(key.to_string(), (zoom_q, hash, entry));
        self.bytes += new_bytes;
        old.and_then(|(_, _, old_entry)| {
            self.bytes = self.bytes.saturating_sub(Self::bytes_of(&old_entry));
            old_entry.ok()
        })
    }
}

/// 任意缩放光栅(**同步**路径;卡片内嵌预览=zoom 1 / ± 步进 / 下载
/// 用——点击级频率,同步可接受)。`img` 布局期同步调用(每次命中 ≈ 一
/// 次哈希查表,稳态帧零渲染);失败写缓存(Err 标记),后续帧直接引
/// fallback,**绝不返回 None**——img 把 None 当「加载中」走 200ms 空窗。
/// 缓存以 **基础 key** 为单条目,内部量化档位:同 key 新档替换旧档并
/// 回收其 atlas 纹理,一图任一时刻只驻留一条光栅(对齐 zed
/// `CachedMermaidDiagram` 单条目语义)。查看器滚轮缩放/拖拽**不走此路**
/// ——连续事件逐帧同步重光栅会卡死 UI 且纹理堆爆;走 store 的防抖后台
/// 视口光栅(见 [`viewer_display`] / store::schedule_mermaid_reraster)。
pub(crate) fn raster_at_zoom(
    key: &str,
    source: &str,
    cx: &mut App,
    zoom: f32,
) -> Result<Arc<gpui_kit::RenderImage>, ImageCacheError> {
    drain_pending_drops(cx);
    let zoom_q = quantize_zoom(zoom.max(0.25));
    let hash = hash_of(source);
    let entry = {
        let cache = RASTER_CACHE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        cache.get(key, zoom_q, hash)
    };
    if let Some(entry) = entry {
        return entry.map_err(|()| cache_error(key));
    }
    let image = raster_uncached_at(source, cx, zoom_q).map_err(|_| cache_error(key))?;
    let entry = Ok(image.clone());
    let evicted = RASTER_CACHE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .put(key, zoom_q, hash, entry);
    if let Some(old) = evicted {
        // 被替换旧图本帧起无绘制方 → 回收 sprite atlas 纹理(不回收则
        // per-ImageId 纹理随每次缩放累积,GPU/进程内存线性涨——即 7G 泄漏)
        cx.drop_image(old, None);
    }
    Ok(image)
}

// ── 查看器视口光栅(裁剪渲染 + 单槽缓存)──────────────────────
// 对齐 web 版的内存行为:web 持矢量(SVG DOM)任意缩放零像素开销;
// gpui 无彩色矢量路径(下版 svg() 仅单色遮罩),位图管线的最优逼近 =
// 只光栅化视口可见区域 —— 位图 ≤ 屏幕尺寸,与图多大/放大多少无关,
// 且任意 zoom 区域内全分辨率(非「钳分辨率+拉伸」)。

/// 视口光栅产物(渲染方 → 显示方/单槽的载荷)
pub(crate) struct ViewRaster {
    pub image: Arc<gpui_kit::RenderImage>,
    /// 渲染档位(量化 zoom;命中判定 + 显示缩放基准)
    pub zoom_q: f32,
    /// 渲染平移原点(量化 pan,视口 px;拖拽 stale 平移基准)
    pub pan_q: (f32, f32),
    /// 该光栅覆盖的逻辑区域(min(整图×zoom, 视口);显示尺寸基准)
    pub region: (f32, f32),
}

/// 显示侧的渲染档上下文(图是「什么状态」下渲染的):显示方据此做
/// 地图式过渡 —— 当前 zoom' ≠ 渲染 zoom 时按 `k = z'/z` 缩放整图、
/// `left = pan_r·k − pan'` 定位,滚轮/拖拽**即时视觉反馈**(旧位图
/// 缩放/平移),后台清晰档就位后无缝替换。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RenderedView {
    pub zoom_q: f32,
    pub pan_q: (f32, f32),
    pub region: (f32, f32),
}

/// 可见区域 = 整图与视口逐轴最小值(≥1px 防零尺寸)
pub(crate) fn region_of(fig: (f32, f32), viewport: (f32, f32)) -> (f32, f32) {
    (
        fig.0.min(viewport.0).max(1.0),
        fig.1.min(viewport.1).max(1.0),
    )
}

/// pan 量化(8px 步,拖拽期缓存键稳定)+ 钳制到 [0, 整图−视口]
/// (整图小于视口 → 0,fit 全览居中)
pub(crate) fn clamp_pan(pan: (f32, f32), fig: (f32, f32), viewport: (f32, f32)) -> (f32, f32) {
    let q = |v: f32| (v / 8.0).round() * 8.0;
    (
        q(pan.0).clamp(0.0, (fig.0 - viewport.0).max(0.0)),
        q(pan.1).clamp(0.0, (fig.1 - viewport.1).max(0.0)),
    )
}

/// 视口裁剪光栅(后台线程安全:纯函数,无 gpui App 依赖):
/// usvg 解析(系统字体)→ resvg 根变换 `scale ∘ translate(-pan)` 只把
/// 可见区域画进 ≤ 视口尺寸的 pixmap → 预乘 RGBA→BGRA(同 gpui 管线)
/// → `image::Frame` 裸构 `RenderImage`(repl/livekit 先例)。
pub(crate) fn raster_viewport(
    source: &str,
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
) -> anyhow::Result<ViewRaster> {
    let (nat_w, nat_h) =
        natural_size(source).ok_or_else(|| anyhow::anyhow!("svg_for 返回 None"))?;
    let zoom_q = quantize_zoom(zoom.max(0.25));
    let fig = (nat_w * zoom_q, nat_h * zoom_q);
    let region = region_of(fig, viewport);
    let pan_q = clamp_pan(pan, fig, viewport);
    // 两套单位:region/pan 是**显示逻辑 px**,SVG 文档单位 = 显示 ÷ zoom。
    // 位图 = region×SMOOTH(设备 2x,任意 zoom 全清晰且 ≤ 视口×2);
    // 根变换 scale = zoom×SMOOTH(svg 单位 → 设备 px),裁剪平移须除回
    // zoom 转到 svg 单位。错误地把 region×zoom×SMOOTH 当位图尺寸会得到
    // 视口×zoom×2(放大即数十 GB 分配失败)。
    let scale = zoom_q * gpui_kit::SMOOTH_SVG_SCALE_FACTOR;
    let pw = ((region.0 * gpui_kit::SMOOTH_SVG_SCALE_FACTOR).ceil() as u32).max(1);
    let ph = ((region.1 * gpui_kit::SMOOTH_SVG_SCALE_FACTOR).ceil() as u32).max(1);
    let tree = tree_for(source)?;
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(pw, ph).ok_or_else(|| anyhow::anyhow!("pixmap 分配失败"))?;
    // 设备px = (svg单位 − pan/zoom) × zoom×SMOOTH = (显示px − pan) × SMOOTH:
    // 先平移(裁剪可见区,svg 单位)后缩放
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale)
        .pre_translate(-pan_q.0 / zoom_q, -pan_q.1 / zoom_q);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let mut data = pixmap.take();
    for px in data.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let buffer =
        image::RgbaImage::from_raw(pw, ph, data).ok_or_else(|| anyhow::anyhow!("尺寸不符"))?;
    Ok(ViewRaster {
        image: Arc::new(gpui_kit::RenderImage::new(vec![image::Frame::new(buffer)])),
        zoom_q,
        pan_q,
        region,
    })
}

/// usvg 解析树进程级缓存(源哈希 → Tree;超限清空同 SVG_CACHE 模式)。
/// `Tree::from_data` 是渲染的大头(字体解析 + 全部文本转路径,大图秒级)
/// 且与 zoom/pan 无关 —— 解析一次后每次视口重光栅只剩 ≤ 视口区域的光栅
/// 化(数十 ms)。滚轮/拖拽延迟的主修复。Tree 全 Arc 字段,Send+Sync
/// 可跨线程共享。
fn tree_for(source: &str) -> anyhow::Result<Arc<usvg::Tree>> {
    let hash = hash_of(source);
    if let Some(tree) = TREE_CACHE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&hash)
    {
        return Ok(tree.clone());
    }
    let svg = svg_for(source).ok_or_else(|| anyhow::anyhow!("svg_for 返回 None"))?;
    let tree = Arc::new(usvg::Tree::from_data(
        svg.as_bytes(),
        viewport_usvg_options(),
    )?);
    let mut cache = TREE_CACHE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if cache.len() >= TREE_CACHE_CAP {
        cache.clear();
    }
    cache.insert(hash, tree.clone());
    Ok(tree)
}

static TREE_CACHE: LazyLock<Mutex<HashMap<u64, Arc<usvg::Tree>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const TREE_CACHE_CAP: usize = 8;

/// 查看器 usvg 装配(系统字体 + 通用族兜底)。merman 文本均带命名族
/// (trebuchet/verdana/…),两路渲染(卡片走 gpui 渲染器 / 查看器走
/// 自持 resvg)对命名族都由系统字体解析,一致性不受影响;仅无命名
/// 族的兜底字形可能略有差异。
fn viewport_usvg_options() -> &'static usvg::Options<'static> {
    static OPTS: LazyLock<usvg::Options<'static>> = LazyLock::new(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        db.set_sans_serif_family("Helvetica");
        db.set_serif_family("Times New Roman");
        db.set_monospace_family("Menlo");
        usvg::Options {
            fontdb: Arc::new(db),
            font_family: "sans-serif".to_owned(),
            ..Default::default()
        }
    });
    &OPTS
}

/// 单槽:一时刻一档(源哈希 + zoom_q + pan_q);新档替换旧档,旧图
/// 交调用方 drop_image —— 滚轮/拖拽全程内存恒定于 ≤ 视口大小一张位图。
struct ViewerSlot {
    hash: u64,
    zoom_q: f32,
    pan_q: (f32, f32),
    region: (f32, f32),
    image: Option<Arc<gpui_kit::RenderImage>>,
}
static VIEWER_SLOT: Mutex<Option<ViewerSlot>> = Mutex::new(None);

/// 单槽相关测试互斥(槽是全局静态;kits 单槽测试与 layout_tests 的
/// 防抖生命周期测试并发交叠会互相覆盖档位 → flaky)
#[cfg(test)]
pub(crate) static VIEWER_SLOT_TEST_LOCK: Mutex<()> = Mutex::new(());

/// 查看器显示载荷:(图, 该图的渲染档上下文)。显示方一律做地图式
/// 过渡:`k = z'/z_r` 缩放 + `left = pan_r·k − pan'` 定位 —— 精确命中
/// k=1 偏移 0;拖拽/滚轮期间旧档(或占位图)即时跟随,后台清晰档
/// 就位后无缝替换。
pub(crate) type ViewerDisplay = (Arc<gpui_kit::RenderImage>, RenderedView);

/// 查看器显示取图(**只读**,每帧调用零渲染):精确档命中 → 清晰图;
/// 未命中退同图旧档(缩放/平移过渡),防抖任务完成后换清晰档;跨图
/// 不 stale。
pub(crate) fn viewer_display(source: &str) -> Option<ViewerDisplay> {
    let hash = hash_of(source);
    let slot = VIEWER_SLOT
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let s = slot.as_ref()?;
    if s.hash != hash {
        return None; // 旧槽是别的图:不做跨图 stale
    }
    let image = s.image.clone()?;
    Some((
        image,
        RenderedView {
            zoom_q: s.zoom_q,
            pan_q: s.pan_q,
            region: s.region,
        },
    ))
}

/// 精确档已在(防抖任务前置检查,免无谓渲染)
pub(crate) fn viewer_upto_date(
    source: &str,
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
) -> bool {
    let Some((nat_w, nat_h)) = natural_size(source) else {
        return true; // 无尺寸 = 渲染必失败,视作最新避免重排循环
    };
    let hash = hash_of(source);
    let zoom_q = quantize_zoom(zoom.max(0.25));
    let pan_q = clamp_pan(pan, (nat_w * zoom_q, nat_h * zoom_q), viewport);
    let slot = VIEWER_SLOT
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    matches!(
        slot.as_ref(),
        Some(s) if s.hash == hash && s.zoom_q == zoom_q && s.pan_q == pan_q && s.image.is_some()
    )
}

/// 写入单槽(主线程,防抖收尾/开图首次同步渲染):同档幂等跳过;
/// 返回被替换旧图供 `drop_image`(回收 atlas 纹理)。
pub(crate) fn viewer_put(source: &str, raster: &ViewRaster) -> Option<Arc<gpui_kit::RenderImage>> {
    let hash = hash_of(source);
    let mut slot = VIEWER_SLOT
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(s) = slot.as_ref()
        && s.hash == hash
        && s.zoom_q == raster.zoom_q
        && s.pan_q == raster.pan_q
        && s.image.is_some()
    {
        return None;
    }
    slot.replace(ViewerSlot {
        hash,
        zoom_q: raster.zoom_q,
        pan_q: raster.pan_q,
        region: raster.region,
        image: Some(raster.image.clone()),
    })
    .and_then(|s| s.image)
}

/// 驱逐单槽(关闭查看器):返回图供 `drop_image`。
pub(crate) fn viewer_evict() -> Option<Arc<gpui_kit::RenderImage>> {
    VIEWER_SLOT
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take()
        .and_then(|s| s.image)
}

fn raster_uncached_at(
    source: &str,
    cx: &mut App,
    zoom: f32,
) -> anyhow::Result<Arc<gpui_kit::RenderImage>> {
    // svg_for 失败(短路)与 parse/render 失败统一为 anyhow→ImageCacheError
    let svg = svg_for(source).ok_or_else(|| anyhow::anyhow!("svg_for 返回 None"))?;
    let renderer = cx.svg_renderer();
    let parsed = renderer.parse_svg(svg.as_bytes())?;
    // ScaleFactor(z):设备光栅 = 自然×2×z(内部 SMOOTH 系数),img 按
    // render_size 布局 = 自然×z——清晰缩放,而非位图拉伸
    Ok(renderer.render_parsed(&parsed, zoom)?)
}

/// 失败态的错误载荷(内容对用户不可见——img fallback 替代显示)
fn cache_error(key: &str) -> ImageCacheError {
    ImageCacheError::Other(Arc::new(anyhow::anyhow!("mermaid 渲染失败:{key}")))
}

/// 导出当前缩放 PNG:`DSH_DOWNLOAD_DIR` 或 ~/Downloads(测试可覆盖);
/// 文件名进程内递增计数。返回落盘路径(宿主注入通知);失败返回 Err。
pub(crate) fn export_diagram_png(source: &str, zoom: f32, cx: &mut App) -> anyhow::Result<PathBuf> {
    fn download_dir() -> PathBuf {
        if let Some(d) = std::env::var_os("DSH_DOWNLOAD_DIR") {
            return PathBuf::from(d);
        }
        let home = std::env::var_os("HOME").unwrap_or_default();
        PathBuf::from(home).join("Downloads")
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let img = raster_at_zoom("mermaid-download", source, cx, zoom)?;
    let dir = download_dir();
    std::fs::create_dir_all(&dir)?;
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("dsh-mermaid-{n}.png"));
    save_png(&img, &path)?;
    Ok(path)
}

/// RenderImage(BGRA 预乘)→ PNG(直乘 RGBA;tiny-skia 光栅为顶行序)
fn save_png(img: &gpui_kit::RenderImage, path: &PathBuf) -> anyhow::Result<()> {
    let size = img.size(0);
    let w = size.width.0 as u32;
    let h = size.height.0 as u32;
    let bytes = img.as_bytes(0).ok_or_else(|| anyhow::anyhow!("帧缺失"))?;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    let unpremultiply = |p: u8, a: u8| {
        if a == 0 {
            0
        } else {
            ((u32::from(p) * 255 + u32::from(a) / 2) / u32::from(a)) as u8
        }
    };
    for px in bytes.as_chunks::<4>().0 {
        let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
        rgba.extend_from_slice(&[
            unpremultiply(r, a),
            unpremultiply(g, a),
            unpremultiply(b, a),
            a,
        ]);
    }
    let img = image::RgbaImage::from_raw(w, h, rgba).ok_or_else(|| anyhow::anyhow!("尺寸不符"))?;
    img.save(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOWCHART: &str = "flowchart LR\n    A-->B\n";

    #[test]
    fn flowchart_renders() {
        let svg = svg_for(FLOWCHART).expect("flowchart 渲染");
        assert!(svg.contains("<svg"), "应输出 SVG 头");
        assert!(!svg.contains("<foreignObject"), "resvg_safe 未生效");
        // 根背景:merman 默认写死 white,RootBackgroundPostprocessor 须
        // 改写为 CODE(卡片底;与节点底融合消除色差)
        assert!(
            svg.contains("background-color:#101012"),
            "根背景应改写为 CODE:{svg}"
        );
        assert!(!svg.contains("background-color:white"), "不应残存白底");
    }

    #[test]
    fn whitelist_types_render() {
        let samples: &[(&str, &str)] = &[
            ("flowchart", "flowchart LR\n    A-->B\n"),
            ("graph", "graph TD\n    A-->B\n"),
            (
                "sequenceDiagram",
                "sequenceDiagram\n    Alice->>Bob: Hello\n",
            ),
            ("classDiagram", "classDiagram\n    class BankAccount\n"),
            (
                "stateDiagram",
                "stateDiagram\n    [*] --> Still\n    Still --> [*]\n",
            ),
            ("stateDiagram-v2", "stateDiagram-v2\n    [*] --> Still\n"),
            (
                "erDiagram",
                "erDiagram\n    CUSTOMER ||--o{ ORDER : places\n",
            ),
            (
                "gantt",
                "gantt\n    title 计划\n    section A\n    任务: a1, 2024-01-01, 1d\n",
            ),
            (
                "pie",
                "pie title 食物\n    \"米饭\" : 386\n    \"面条\" : 85\n",
            ),
            ("gitGraph", "gitGraph\n    commit\n"),
            ("mindmap", "mindmap\n    root((中心))\n      支线\n"),
            (
                "timeline",
                "timeline\n    title 历史\n    2002 : 事件A\n    2004 : 事件B\n",
            ),
            (
                "quadrantChart",
                "quadrantChart\n    title 象限\n    x-axis 低 --> 高\n    y-axis 低 --> 高\n    quadrant-1 扩张\n    quadrant-2 推广\n    quadrant-3 观望\n    quadrant-4 重估\n    项目A: [0.3, 0.6]\n",
            ),
            (
                "xychart-beta",
                "xychart-beta\n    title \"销量\"\n    x-axis [一月, 二月, 三月]\n    y-axis \"(元)\"\n    line [500, 600, 700]\n",
            ),
            (
                "journey",
                "journey\n    title 行程\n    section 早晨\n      喝茶: 5: 我\n      出门: 3: 我\n",
            ),
        ];
        for (name, src) in samples {
            assert!(is_supported_diagram_type(src), "{name} 应过白名单");
            assert!(svg_for(src).is_some(), "{name} 渲染失败:\n{src}");
        }
    }

    #[test]
    fn cjk_labels_roundtrip() {
        let src = "flowchart LR\n    A[\"中文标签\"] --> B[\"节点\"]\n";
        let svg = svg_for(src).expect("CJK 渲染");
        assert!(svg.contains("中文标签"), "输出应含中文原文");
    }

    #[test]
    fn malformed_and_guards() {
        assert!(svg_for("flowchart LR\n    A--").is_none(), "畸形应失败");
        assert!(svg_for("classDiagram\n    class").is_none(), "截断应失败");
        let big = format!("flowchart LR\n    A-->{}\n", "B".repeat(65 * 1024));
        assert!(svg_for(&big).is_none(), "超长守卫应拦截");
        // 失败记忆:同一失败源二次仍 None(不重算)
        let bad = "flowchart LR\n    A--![1]";
        assert!(svg_for(bad).is_none());
        assert!(svg_for(bad).is_none());
        // 内容变化(哈希变化)→ 解除失败记忆,允许重试
        let bad2 = "flowchart LR\n    A--";
        assert!(svg_for(bad2).is_none());
    }

    #[test]
    fn whitelist_blocks_beta() {
        assert!(!is_supported_diagram_type("sankey-beta\n    A-->B\n"));
        assert!(!is_supported_diagram_type("kanban\n    A: B\n"));
        assert!(!is_supported_diagram_type(
            "sequence-diagram\n    A->>B: hi\n"
        ));
        assert!(!is_supported_diagram_type(""), "空源不过白名单");
    }

    #[test]
    fn theme_mapping_fields() {
        let vars = theme_variables();
        let get = |k: &str| vars[k].as_str().expect("应有字符串值").to_string();
        // 不透明色直传;BORDER_2(白 14%)按 BASE 合成 → 精确值由 hex 计算
        assert_eq!(get("background"), "#101012", "背景 = CODE(卡片底,消除色差)");
        assert_eq!(get("textColor"), "#FFFFFF", "主文本 = LABEL");
        assert_eq!(get("primaryColor"), "#232326", "节点面 = CARD");
        assert_eq!(get("lineColor"), "#343436", "边线 = BORDER_2 合成色");
        assert_eq!(get("nodeTextColor"), "#FFFFFF");
        assert!(vars.is_object());
        // 系列键齐(8 组 cScale*/pieN)
        for i in 0..8 {
            assert!(vars.get(format!("cScale{i}")).is_some(), "cScale{i}");
            assert!(vars.get(format!("pie{}", i + 1)).is_some(), "pie{}", i + 1);
        }
    }

    /// 浅盘映射(theme_variables_in 纯函数注入,不翻全局盘)
    #[test]
    fn theme_mapping_fields_light() {
        let vars = theme_variables_in(palette_of(gpui_kit::component::ThemeMode::Light));
        let get = |k: &str| vars[k].as_str().expect("应有字符串值").to_string();
        assert_eq!(get("background"), "#F7F7F9", "背景 = 浅盘 CODE");
        assert_eq!(get("textColor"), "#1D1D1F", "主文本 = 浅盘 LABEL");
        assert_eq!(get("primaryColor"), "#FFFFFF", "节点面 = 浅盘 CARD(白)");
        // BORDER_2(黑 16%)按白底合成
        assert_eq!(get("lineColor"), "#D6D6D6", "边线 = BORDER_2 合成色");
    }

    /// 光栅闭环:svg_for 之后的 parse_svg → render_parsed(usvg 字体解析
    /// + 2x 光栅)必须产出非空帧——SVG 字符串能渲染≠gpui 能光栅化
    /// (fontdb 字体解析/非法 CSS 清理失败都会在此暴露,失败即
    /// fallback 代码块)
    #[gpui_kit::test]
    fn raster_produces_valid_image(cx: &mut gpui_kit::TestAppContext) {
        let img = cx.update(|app| raster_at_zoom("raster-test", FLOWCHART, app, 1.0));
        let img = img.expect("光栅应成功(失败=usvg 解析/渲染错误)");
        assert_eq!(img.frame_count(), 1, "单帧光栅");
        assert!(img.size(0).width > gpui_kit::DevicePixels(0), "非零宽度");
        assert!(img.size(0).height > gpui_kit::DevicePixels(0), "非零高度");
    }

    /// 缩放光栅:zoom=2 应为自然尺寸 2 倍(查看器重光栅而非位图拉伸);
    /// 缓存键 `key@zoom` 与 zoom=1 条目互不干扰(变更源后两档独立刷新)
    #[gpui_kit::test]
    fn raster_at_zoom_scales_and_isolates(cx: &mut gpui_kit::TestAppContext) {
        let base = cx
            .update(|app| raster_at_zoom("zoom-test", FLOWCHART, app, 1.0))
            .expect("zoom=1 光栅");
        let z2 = cx
            .update(|app| raster_at_zoom("zoom-test", FLOWCHART, app, 2.0))
            .expect("zoom=2 光栅");
        let b = base.size(0);
        let z = z2.size(0);
        assert!(
            (z.width.0 as f64 / b.width.0 as f64 - 2.0).abs() < 0.05,
            "宽应为 2 倍:{} vs {}",
            z.width.0,
            b.width.0
        );
        assert!(
            (z.height.0 as f64 / b.height.0 as f64 - 2.0).abs() < 0.05,
            "高应为 2 倍"
        );
        // 缓存在档位间隔离:改源后 1x 重算、2x 保持旧档(此断言点仅在
        // 编译期保证键拼装不含歧义;实际隔离由 fit 测试的渲染闭环覆盖)

        // 量化吸附(根治放大 OOM 的核心):相邻连续瞬时 zoom 落到同一档,
        // 连续滚轮乘法不产生逐帧新档。纯函数,无共享静态 → 并行测试安全。
        // 档位乘 round()*step 有 ~1e-6 级浮点误差,用容差(1e-4)而非严格等。
        let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
        assert!(
            close(quantize_zoom(1.03), quantize_zoom(1.06)),
            "1.03/1.06 吸附同档"
        );
        assert!(close(quantize_zoom(1.03), 1.05), "1.03 → 1.05 档");
        assert!(close(quantize_zoom(2.0), 2.0), "整数档原样");
        assert!(close(quantize_zoom(0.26), 0.25), "下限原样");
        // 单条目替换 + 字节预算在 RasterCache::put(同 key insert 覆盖,
        // 超预算 clear_all)由数据结构保证,编译期已锁。
    }

    /// 查看器单槽(防抖收尾路径):同图新档**替换**旧档并返回旧图(供
    /// drop_image 回收 atlas 纹理——泄漏修复的行为锁);精确档命中、跨档
    /// stale、跨图不 stale、同档幂等、关闭驱逐。视口裁剪核心断言:荒谬
    /// 放大下位图仍 ≤ 视口×SMOOTH(内存 O(视口) 的回归锚点)。
    /// 与 layout_tests 的防抖生命周期测试共享槽静态 → 互斥锁串行。
    #[gpui_kit::test]
    fn viewer_slot_replaces_and_viewport_crops() {
        let _guard = VIEWER_SLOT_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let src = FLOWCHART;
        let other = "flowchart TD\n    X-->Y\n";
        let vp = (1000., 800.);

        // 荒谬放大(整图 ≫ 视口):位图必须 ≤ 视口×SMOOTH —— 与放大倍数
        // 无关(此前整图光栅 = 自然×zoom×2,放大 8 倍即数百 MB → 5G 泄漏)
        let huge = raster_viewport(src, 50.0, (50000., 50000.), vp).expect("荒谬放大视口光栅");
        let s = huge.image.size(0);
        assert!(
            s.width.0 <= vp.0 as i32 * 2 + 2 && s.height.0 <= vp.1 as i32 * 2 + 2,
            "视口裁剪:位图应 ≤ 视口×SMOOTH,实测 {}×{}",
            s.width.0,
            s.height.0
        );
        // pan 越界钳回上界(整图−视口)/下界 0,量化吸附
        let (nat_w, nat_h) = natural_size(src).expect("自然尺寸");
        let q = quantize_zoom(50.0);
        let close = |a: f32, b: f32| (a - b).abs() < 8.0 + 1e-3;
        assert!(close(huge.pan_q.0, nat_w * q - vp.0), "pan_x 应钳制上界");
        assert!(close(huge.pan_q.1, nat_h * q - vp.1), "pan_y 应钳制上界");

        // 首写无驱逐;精确档命中(载荷 = 渲染档上下文,含 region 基准)
        assert!(viewer_put(src, &huge).is_none(), "槽空首写无驱逐");
        let hit = viewer_display(src).expect("精确档命中");
        assert_eq!(hit.0.id, huge.image.id);
        assert_eq!(hit.1.zoom_q, huge.zoom_q, "档上下文带渲染 zoom");
        assert_eq!(hit.1.region, huge.region, "档上下文带渲染区域");

        // 换档替换:返回旧图(回收纹理);旧档显示退新档 stale
        let next = raster_viewport(src, 1.0, (0., 0.), vp).expect("fit 档");
        let evicted = viewer_put(src, &next);
        assert_eq!(
            evicted.map(|img| img.id),
            Some(huge.image.id),
            "同图新档应替换旧档并返回旧图"
        );
        let stale = viewer_display(src).expect("跨档 stale 过渡");
        assert_eq!(stale.0.id, next.image.id, "stale 显示最近档");
        assert_eq!(
            (stale.1.zoom_q, stale.1.pan_q, stale.1.region),
            (next.zoom_q, next.pan_q, next.region),
            "stale 载荷 = 旧档完整上下文(地图式过渡基准)"
        );
        assert!(
            !viewer_upto_date(src, 50.0, (50000., 50000.), vp),
            "旧档过期"
        );
        assert!(viewer_upto_date(src, 1.0, (0., 0.), vp), "新档最新");

        // 同档幂等:后到结果不覆盖不驱逐
        assert!(viewer_put(src, &next).is_none(), "同档已在不重复写");

        // 跨图不 stale(槽里是别的图 → None,不显示错误内容)
        assert!(viewer_display(other).is_none(), "跨图不做 stale");

        // 关闭驱逐:返回在档图;此后显示 None
        assert_eq!(
            viewer_evict().map(|img| img.id),
            Some(next.image.id),
            "驱逐返回在档图"
        );
        assert!(viewer_display(src).is_none(), "驱逐后无图");
    }

    /// 自适应缩放:超宽图在 400px 块级容器内应等比收窄(max_w_full 钳制
    /// + aspect_ratio 解析),不溢出、不横向压扁(用户反馈「没有自适应
    /// 缩放」的回归锚点)。量测 = 块容器 debug_bounds 高度:块级流中
    /// 容器高 = 子图实际布局高——钳制生效则 高≈400×宽高比(远小于
    /// 自然高),未生效则=自然高。
    #[gpui_kit::test]
    fn wide_image_scales_to_container(cx: &mut gpui_kit::TestAppContext) {
        let wide = "\
flowchart LR\n    N0[0] --> N1[1] --> N2[2] --> N3[3] --> N4[4] --> N5[5] --> N6[6] --> N7[7] --> N8[8] --> N9[9] --> N10[10] --> N11[11] --> N12[12] --> N13[13]
";
        // 自然比例(与绘制闭包同键,命中同一光栅)
        let natural = cx
            .update(|app| raster_at_zoom("fit-test", wide, app, 1.0))
            .expect("超宽图应可光栅");
        let nsize = natural.size(0);
        let ratio = nsize.height.0 as f64 / nsize.width.0 as f64;
        let nat_h = nsize.height.0 as f64;

        let window = cx.add_empty_window();
        let src: Arc<str> = wide.trim_end().into();
        window.draw(
            gpui_kit::point(px(0.), px(0.)),
            gpui_kit::size(px(400.), px(400.)),
            |_window, _cx| {
                let src = src.clone();
                div()
                    .id("fit-wrap")
                    .debug_selector(|| "fit-wrap".to_string())
                    .w_full()
                    .child(
                        img(ImageSource::Custom(Arc::new(move |_window, cx| {
                            Some(raster_at_zoom("fit-test", &src, cx, 1.0))
                        })))
                        .max_w_full(),
                    )
            },
        );

        let bounds = window.debug_bounds("fit-wrap").expect("容器 bounds 缺失");
        let w = bounds.size.width.as_f32() as f64;
        let h = bounds.size.height.as_f32() as f64;
        assert!((w - 400.0).abs() < 0.5, "容器宽应为 400:{w}");
        let got = h / w;
        assert!(
            (got - ratio).abs() < 0.02,
            "长宽比应保留(等比收窄):{got:.4} vs {ratio:.4}"
        );
        assert!(
            h < nat_h - 1.0,
            "容器高应显著小于自然高(钳制未生效?):{h} vs {nat_h}"
        );
    }
}
