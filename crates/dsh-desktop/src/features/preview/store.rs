//! 文档预览功能切片状态与行为。
//!
//! 桶 = 预览中的文件(rel 路径键,随 tab 关闭即焚——照源内存态)。
//! text-pages 模式:页 = [`face::TextPage`](5000 行/页,追加式 pages
//! 表 + 行缓存);bytes-complete 模式:整档字节 + 图片解码产物。
//! 变更检测 = 1s stat 轮询(仅存在预览 tab 时),observed ≠ 装载版本
//! → 提示条,只提示不自动重载。渲染器手选 per-tab 内存;跨 load_mode
//! 切换清内容重读,同 mode 切换保留内容(照源)。

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::{Context, ListAlignment, ListState, px};

use super::face::{self, ReadError, TextPage};
use crate::kits::filetype::{self, DocRenderer, LoadMode};
use crate::shell::panel::{PanelTab, PreviewTab};
use crate::shell::store::AppStore;

/// 预览切片状态
#[derive(Default)]
pub struct PreviewStore {
    /// 预览桶(rel 路径 → 状态)
    pub buckets: HashMap<PathBuf, PreviewBucket>,
    /// 「打开方式」菜单(根级渲染;键 = 目标文件 rel 路径,值 = 锚点)
    pub menu: Option<(PathBuf, gpui_kit::Point<gpui_kit::Pixels>)>,
}

/// 单文件预览桶
pub struct PreviewBucket {
    /// 该格式整体不可预览(空候选:不读取、无菜单)
    pub unsupported: bool,
    /// 手选渲染器(None = 自动默认;per-tab 内存,照源)
    pub renderer: Option<DocRenderer>,
    /// 已装载内容的模式(切换渲染器跨 mode 时据此清内容)
    pub loaded_mode: Option<LoadMode>,
    /// 换行开关(默认开;仅 wrap 渲染器消费)
    pub wrap: bool,
    /// 已载页(offset → 页;text-pages)
    pub pages: BTreeMap<u32, TextPage>,
    /// 已载行缓存(页到达时重建;code 行列与计数的数据源)
    pub lines: Vec<String>,
    /// 已读到最后一行
    pub eof: bool,
    /// 装载进行中
    pub loading: bool,
    /// 当前装载失败文案(有内容时显示为页尾状态行)
    pub failure: Option<String>,
    /// 整档字节(bytes-complete)
    pub complete: Option<Arc<Vec<u8>>>,
    /// 图片解码产物(image 渲染器)
    pub image: Option<Arc<gpui_kit::Image>>,
    /// 图片内在尺寸(装载时解码;宽高比定容器高)
    pub image_dims: Option<(u32, u32)>,
    /// PDF 各页渲染尺寸(未栅格化先取;布局占位/宽高比)
    pub pdf_dims: Option<Arc<Vec<(f32, f32)>>>,
    /// PDF 已栅格化页(页号 → RenderImage;渐进回填)
    pub pdf_pages: HashMap<usize, Arc<gpui_kit::RenderImage>>,
    /// PDF 级失败文案(密码/解析失败;整面空态)
    pub pdf_failed: Option<String>,
    /// 装载时版本 token
    pub version: Option<(u64, u64)>,
    /// 轮询观察到的版本 token
    pub observed: Option<(u64, u64)>,
    /// stat 失败(文件没了;提示条占位,照源元数据失败面)
    pub meta_failed: bool,
    /// 待跳行(行导航;装载覆盖后消费)
    pub nav_line: Option<u32>,
    /// 已高亮行(下次导航/重载清除)
    pub highlight_line: Option<u32>,
    /// 装载代号(防过期回包二次应用)
    pub load_id: u64,
    /// code 行列状态(桶持久,滚动跨切换保留)
    pub code_list: ListState,
    /// code 行列计数(与 lines 对齐)
    pub code_list_count: usize,
}

impl PreviewBucket {
    fn new() -> Self {
        Self {
            unsupported: false,
            renderer: None,
            loaded_mode: None,
            wrap: true,
            pages: BTreeMap::new(),
            lines: Vec::new(),
            eof: false,
            loading: false,
            failure: None,
            complete: None,
            image: None,
            image_dims: None,
            pdf_dims: None,
            pdf_pages: HashMap::new(),
            pdf_failed: None,
            version: None,
            observed: None,
            meta_failed: false,
            nav_line: None,
            highlight_line: None,
            load_id: 0,
            code_list: ListState::new(0, ListAlignment::Top, px(600.)),
            code_list_count: 0,
        }
    }

    /// 当前生效渲染器(手选优先,回落自动默认)
    pub fn current_renderer(&self, name: &str) -> Option<DocRenderer> {
        self.renderer.or_else(|| filetype::default_renderer(name))
    }

    /// 变更提示条判据(照源三方版本对比收敛为:已装载 + 已观察 + 不一致)
    pub fn changed(&self) -> bool {
        match (self.version, self.observed) {
            (Some(current), Some(observed)) => current != observed,
            _ => false,
        }
    }

    /// 已载全部文本(拼接页;text/markdown 体数据源)
    pub fn joined_text(&self) -> String {
        self.pages
            .values()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("")
    }

    /// 下一页 offset(末页 offset + 行数;空页至少 +1 防死循环)
    fn next_offset(&self) -> u32 {
        self.pages
            .last_key_value()
            .map(|(offset, page)| offset + page.lines.max(1))
            .unwrap_or(1)
    }

    /// 页到达后重建行缓存并对齐 code 行列(追加 splice,重载 reset)
    fn rebuild_lines(&mut self, reset: bool) {
        let mut lines = Vec::new();
        for page in self.pages.values() {
            if page.lines == 0 {
                continue;
            }
            let text = page.text.strip_suffix('\n').unwrap_or(&page.text);
            for line in text.split('\n') {
                lines.push(line.to_string());
            }
        }
        let count = lines.len();
        self.lines = lines;
        if reset || count < self.code_list_count {
            self.code_list.reset(count);
        } else if count > self.code_list_count {
            let at = self.code_list_count;
            self.code_list.splice(at..at, count - at);
        }
        self.code_list_count = count;
    }

    /// 清装载内容(渲染器跨 mode 切换 / 重载)
    fn clear_content(&mut self) {
        self.pages.clear();
        self.lines.clear();
        self.eof = false;
        self.failure = None;
        self.complete = None;
        self.image = None;
        self.image_dims = None;
        self.pdf_dims = None;
        self.pdf_pages.clear();
        self.pdf_failed = None;
        self.highlight_line = None;
        self.loaded_mode = None;
        self.rebuild_lines(true);
    }
}

/// 文件名(rel 路径 → 小写 basename)
fn file_name_of(rel: &std::path::Path) -> String {
    rel.file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

impl AppStore {
    /// 当前生效渲染器(桶 + 文件名)
    fn preview_renderer_of(&self, rel: &std::path::Path) -> Option<DocRenderer> {
        self.preview
            .buckets
            .get(rel)
            .and_then(|b| b.current_renderer(&file_name_of(rel)))
    }

    /// 打开文件预览 tab(树行点击入口):同路径 reveal 不重开(带行则
    /// 更新导航参数),否则新开;桶惰性建立并首发装载
    pub fn open_file_preview(&mut self, abs_id: &str, line: Option<u32>, cx: &mut Context<Self>) {
        let Some(root) = self.current_workspace_dir() else {
            return;
        };
        let rel = PathBuf::from(abs_id)
            .strip_prefix(&root)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(abs_id));
        let existing = self
            .panel_tabs
            .iter()
            .position(|t| matches!(t, PanelTab::Preview(p) if p.path == rel));
        if let Some(ix) = existing {
            if line.is_some()
                && let Some(PanelTab::Preview(p)) = self.panel_tabs.get_mut(ix)
            {
                p.line = line;
            }
        } else {
            self.panel_tabs.push(PanelTab::Preview(PreviewTab {
                path: rel.clone(),
                line,
            }));
        }
        self.panel_open = true;
        self.panel_active_tab = Some(PanelTab::Preview(PreviewTab {
            path: rel.clone(),
            line,
        }));
        self.panel_plus_menu_at = None;
        self.preview_ensure_bucket(&rel, line, cx);
        self.preview_poll_start(cx);
        cx.notify();
    }

    /// 桶惰性建立 + 首发装载(不可预览格式只立 unsupported 标记)
    pub(crate) fn preview_ensure_bucket(
        &mut self,
        rel: &PathBuf,
        line: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        if !self.preview.buckets.contains_key(rel) {
            let mut bucket = PreviewBucket::new();
            if filetype::doc_candidates(&file_name_of(rel)).is_empty() {
                bucket.unsupported = true;
            }
            bucket.nav_line = line;
            self.preview.buckets.insert(rel.to_path_buf(), bucket);
        } else if line.is_some()
            && let Some(bucket) = self.preview.buckets.get_mut(rel)
        {
            bucket.nav_line = line; // 重开带行 → 更新导航目标
        }
        let (need_load, nav_ready) = {
            let Some(bucket) = self.preview.buckets.get(rel) else {
                return;
            };
            let need_load = bucket.loaded_mode.is_none()
                && !bucket.unsupported
                && !bucket.loading
                && bucket.failure.is_none();
            // 行导航就绪:目标已被已载行覆盖,或已 eof(越界目标钳到尾)
            let nav_ready = bucket
                .nav_line
                .is_some_and(|t| t as usize <= bucket.lines.len() || bucket.eof);
            (need_load, nav_ready)
        };
        if need_load {
            self.preview_start_load(rel, cx);
        } else if nav_ready {
            self.preview_consume_nav(rel);
        }
    }

    /// 按当前渲染器 mode 发起装载(页 1 或整档字节)
    fn preview_start_load(&mut self, rel: &PathBuf, cx: &mut Context<Self>) {
        let Some(renderer) = self.preview_renderer_of(rel) else {
            return;
        };
        match renderer.load_mode() {
            LoadMode::TextPages => self.preview_load_page(rel, 1, cx),
            LoadMode::BytesComplete => self.preview_load_bytes(rel, cx),
        }
    }

    /// 装载一页文本(offset 起,页大小 = MAX_LINES)
    fn preview_load_page(&mut self, rel: &PathBuf, offset: u32, cx: &mut Context<Self>) {
        let Some(root) = self.current_workspace_dir() else {
            return;
        };
        let Some(bucket) = self.preview.buckets.get_mut(rel) else {
            return;
        };
        if bucket.loading {
            return;
        }
        bucket.loading = true;
        bucket.failure = None;
        bucket.load_id += 1;
        bucket.loaded_mode = Some(LoadMode::TextPages);
        let load_id = bucket.load_id;
        cx.notify();
        let abs = root.join(rel);
        let rx = self.bridge.call(async move {
            tokio::task::spawn_blocking(move || face::read_text_page(&abs, offset, face::MAX_LINES))
                .await
                .map_err(|e| ReadError::Unavailable(format!("{e}")))
                .and_then(|r| r)
        });
        let rel_done = rel.clone();
        let store = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            let Ok(result) = rx.await else {
                return Ok::<(), anyhow::Error>(());
            };
            store.update(cx, |s, cx| {
                // 过期回包判定与 stat 先行(短借用),桶的可变借用后置
                let applies = s
                    .preview
                    .buckets
                    .get(&rel_done)
                    .is_some_and(|b| b.load_id == load_id);
                if !applies {
                    return;
                }
                let abs = s.current_workspace_dir().map(|r| r.join(&rel_done));
                let stat_version = abs.as_deref().and_then(face::stat_version);
                let Some(bucket) = s.preview.buckets.get_mut(&rel_done) else {
                    return;
                };
                bucket.loading = false;
                // 行导航与续页的决策量(桶借用结束前取出)
                let mut nav_target = None;
                let mut next_page = None;
                match result {
                    Ok(page) => {
                        bucket.eof = page.eof || page.lines == 0;
                        bucket.pages.insert(page.offset, page);
                        bucket.rebuild_lines(false);
                        bucket.version = stat_version;
                        bucket.observed = bucket.version;
                        bucket.meta_failed = bucket.version.is_none();
                        if let Some(target) = bucket.nav_line {
                            if target as usize <= bucket.lines.len() || bucket.eof {
                                nav_target = Some(target);
                            } else if !bucket.eof {
                                next_page = Some(bucket.next_offset());
                            }
                        }
                    }
                    Err(err) => {
                        bucket.failure = Some(face::failure_line(&err));
                    }
                }
                if nav_target.is_some() {
                    s.preview_consume_nav(&rel_done);
                }
                if let Some(offset) = next_page {
                    s.preview_load_page(&rel_done, offset, cx);
                }
                cx.notify();
            });
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 整档装载(bytes-complete;image 顺带解码)
    fn preview_load_bytes(&mut self, rel: &PathBuf, cx: &mut Context<Self>) {
        let Some(root) = self.current_workspace_dir() else {
            return;
        };
        let decode_image = self.preview_renderer_of(rel) == Some(DocRenderer::Image);
        let Some(bucket) = self.preview.buckets.get_mut(rel) else {
            return;
        };
        if bucket.loading {
            return;
        }
        bucket.loading = true;
        bucket.failure = None;
        bucket.load_id += 1;
        let load_id = bucket.load_id;
        bucket.loaded_mode = Some(LoadMode::BytesComplete);
        cx.notify();
        let abs = root.join(rel);
        let rx = self.bridge.call(async move {
            tokio::task::spawn_blocking(move || {
                let bytes = face::read_all_bytes(&abs)?;
                // image 渲染器顺带解内在尺寸(后台线程,大图不卡 UI)
                let dims = if decode_image {
                    image::load_from_memory(&bytes)
                        .ok()
                        .map(|d| (d.width(), d.height()))
                } else {
                    None
                };
                Ok::<_, ReadError>((bytes, dims))
            })
            .await
            .map_err(|e| ReadError::Unavailable(format!("{e}")))
            .and_then(|r| r)
        });
        let rel_done = rel.clone();
        let store = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            let Ok(result) = rx.await else {
                return Ok::<(), anyhow::Error>(());
            };
            let rel_done = rel_done.clone();
            store.update(cx, |s, cx| {
                let applies = s
                    .preview
                    .buckets
                    .get(&rel_done)
                    .is_some_and(|b| b.load_id == load_id);
                if !applies {
                    return;
                }
                let abs = s.current_workspace_dir().map(|r| r.join(&rel_done));
                let stat_version = abs.as_deref().and_then(face::stat_version);
                let Some(bucket) = s.preview.buckets.get_mut(&rel_done) else {
                    return;
                };
                bucket.loading = false;
                match result {
                    Ok((bytes, dims)) => {
                        bucket.complete = Some(Arc::new(bytes));
                        bucket.image_dims = dims;
                        bucket.version = stat_version;
                        bucket.observed = bucket.version;
                        bucket.meta_failed = bucket.version.is_none();
                    }
                    Err(err) => {
                        bucket.failure = Some(face::failure_line(&err));
                    }
                }
                let decode_now = decode_image;
                let as_pdf = s.preview_renderer_of(&rel_done) == Some(DocRenderer::Pdf);
                if decode_now {
                    s.preview_decode_image(&rel_done);
                }
                if as_pdf {
                    s.preview_start_pdf(&rel_done, cx);
                }
                cx.notify();
            });
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 解码图片产物(仅 image 渲染器;失败 = 「无法显示这张图片」)
    fn preview_decode_image(&mut self, rel: &std::path::Path) {
        let Some(bucket) = self.preview.buckets.get_mut(rel) else {
            return;
        };
        let Some(bytes) = bucket.complete.clone() else {
            return;
        };
        let format = match file_name_of(rel).rsplit('.').next() {
            Some("png") => Some(gpui_kit::ImageFormat::Png),
            Some("jpg" | "jpeg") => Some(gpui_kit::ImageFormat::Jpeg),
            Some("gif") => Some(gpui_kit::ImageFormat::Gif),
            Some("webp") => Some(gpui_kit::ImageFormat::Webp),
            _ => None,
        };
        bucket.image =
            format.map(|f| Arc::new(gpui_kit::Image::from_bytes(f, bytes.as_ref().clone())));
        if bucket.image.is_none() {
            bucket.failure = Some("无法显示这张图片".to_string());
        }
    }

    /// PDF:解析各页尺寸(未栅格化),成功后驱动渐进栅格化;失败/
    /// 密码映射为整面空态文案(照源)
    fn preview_start_pdf(&mut self, rel: &PathBuf, cx: &mut Context<Self>) {
        let Some(bytes) = self
            .preview
            .buckets
            .get(rel)
            .and_then(|b| b.complete.clone())
        else {
            return;
        };
        let load_id = self
            .preview
            .buckets
            .get(rel)
            .map(|b| b.load_id)
            .unwrap_or_default();
        let rel = rel.clone();
        let store = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            let task_bytes = bytes;
            let dims = cx
                .background_executor()
                .spawn(async move { super::pdf::open_page_dims(&task_bytes) })
                .await;
            store.update(cx, |s, cx| {
                let Some(bucket) = s.preview.buckets.get_mut(&rel) else {
                    return;
                };
                if bucket.load_id != load_id {
                    return;
                }
                match dims {
                    Ok(dims) => {
                        bucket.pdf_dims = Some(Arc::new(dims));
                        s.preview_render_pdf_pages(&rel, cx);
                    }
                    Err(super::pdf::PdfError::Password) => {
                        bucket.pdf_failed = Some("此 PDF 需要密码，暂不支持预览".to_string());
                    }
                    Err(super::pdf::PdfError::Invalid(msg)) => {
                        bucket.pdf_failed = Some(format!("无法显示 PDF：{msg}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// PDF 页渐进栅格化(逐页后台渲染回填;装载代换即中止;已渲染
    /// 页跳过——刷新钮重载后 pdf_pages 已清,自然全量重绘)
    fn preview_render_pdf_pages(&mut self, rel: &PathBuf, cx: &mut Context<Self>) {
        let Some(bucket) = self.preview.buckets.get(rel) else {
            return;
        };
        let Some(bytes) = bucket.complete.clone() else {
            return;
        };
        let Some(dims) = bucket.pdf_dims.clone() else {
            return;
        };
        let load_id = bucket.load_id;
        let rel = rel.clone();
        let store = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            for ix in 0..dims.len() {
                let need = store.update(cx, |s, _| {
                    s.preview
                        .buckets
                        .get(&rel)
                        .filter(|b| b.load_id == load_id)
                        .is_some_and(|b| !b.pdf_pages.contains_key(&ix))
                });
                if !need {
                    if store.update(cx, |s, _| {
                        s.preview
                            .buckets
                            .get(&rel)
                            .is_some_and(|b| b.load_id != load_id)
                    }) {
                        break; // 装载代已换:中止
                    }
                    continue; // 页已就绪:跳过
                }
                let page_bytes = bytes.clone();
                let rendered = cx
                    .background_executor()
                    .spawn(async move { super::pdf::render_page(&page_bytes, ix) })
                    .await;
                store.update(cx, |s, cx| {
                    let Some(bucket) = s.preview.buckets.get_mut(&rel) else {
                        return;
                    };
                    if bucket.load_id != load_id {
                        return;
                    }
                    match rendered {
                        Ok(page) => {
                            // 直通 RGBA → RenderImage(尺寸不符 = 产线异常,记失败)
                            if let Some(buffer) =
                                image::RgbaImage::from_raw(page.width, page.height, page.rgba)
                            {
                                bucket.pdf_pages.insert(
                                    ix,
                                    Arc::new(gpui_kit::RenderImage::new(vec![image::Frame::new(
                                        buffer,
                                    )])),
                                );
                            } else {
                                bucket.pdf_failed = Some("无法显示 PDF：位图尺寸不符".to_string());
                            }
                        }
                        Err(super::pdf::PdfError::Password) => {
                            bucket.pdf_failed = Some("此 PDF 需要密码，暂不支持预览".to_string());
                        }
                        Err(super::pdf::PdfError::Invalid(msg)) => {
                            bucket.pdf_failed = Some(format!("无法显示 PDF：{msg}"));
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 消费行导航:滚动到目标行并一次性高亮
    fn preview_consume_nav(&mut self, rel: &std::path::Path) {
        let Some(bucket) = self.preview.buckets.get_mut(rel) else {
            return;
        };
        if let Some(line) = bucket.nav_line.take() {
            let ix = line.saturating_sub(1) as usize;
            bucket.code_list.scroll_to_reveal_item(ix);
            bucket.highlight_line = Some(line);
        }
    }

    /// 「加载更多」(text-pages;eof/loading 中不发)
    pub fn preview_load_more(&mut self, rel: &PathBuf, cx: &mut Context<Self>) {
        let Some(bucket) = self.preview.buckets.get(rel) else {
            return;
        };
        if bucket.eof || bucket.loading {
            return;
        }
        let next = bucket.next_offset();
        self.preview_load_page(rel, next, cx);
    }

    /// 重新读取(刷新钮 / 变更条「重新载入」):按当前 mode 清内容重读
    pub fn preview_reload(&mut self, rel: &PathBuf, cx: &mut Context<Self>) {
        let unsupported = self.preview.buckets.get(rel).is_some_and(|b| b.unsupported);
        if unsupported {
            return;
        }
        {
            let Some(bucket) = self.preview.buckets.get_mut(rel) else {
                return;
            };
            bucket.load_id += 1; // 使在途回包过期
            bucket.loading = false;
            bucket.clear_content();
        }
        self.preview_start_load(rel, cx);
    }

    /// 手选渲染器(「打开方式」菜单):跨 mode 清内容重读,同 mode 保留
    pub fn preview_select_renderer(
        &mut self,
        rel: &PathBuf,
        renderer: DocRenderer,
        cx: &mut Context<Self>,
    ) {
        self.preview.menu = None;
        let mode_change = self
            .preview
            .buckets
            .get(rel)
            .and_then(|b| b.loaded_mode)
            .is_some_and(|m| m != renderer.load_mode());
        let idle = self
            .preview
            .buckets
            .get(rel)
            .is_some_and(|b| b.loaded_mode.is_none() && !b.loading);
        {
            let Some(bucket) = self.preview.buckets.get_mut(rel) else {
                return;
            };
            bucket.renderer = Some(renderer);
            if mode_change {
                bucket.load_id += 1;
                bucket.loading = false;
                bucket.clear_content();
            }
        }
        if mode_change || idle {
            self.preview_start_load(rel, cx);
        }
        cx.notify();
    }

    /// 换行开关
    pub fn preview_toggle_wrap(&mut self, rel: &PathBuf, cx: &mut Context<Self>) {
        if let Some(bucket) = self.preview.buckets.get_mut(rel) {
            bucket.wrap = !bucket.wrap;
        }
        cx.notify();
    }

    /// 「打开方式」菜单开(锚点坐标,根级渲染)
    pub fn preview_open_menu_at(
        &mut self,
        rel: PathBuf,
        pos: gpui_kit::Point<gpui_kit::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.preview.menu = Some((rel, pos));
        cx.notify();
    }

    /// 关 tab 即焚桶(照源内存态)
    pub fn preview_forget(&mut self, rel: &std::path::Path) {
        self.preview.buckets.remove(rel);
        if matches!(self.preview.menu, Some((ref target, _)) if target == rel) {
            self.preview.menu = None;
        }
    }

    /// 变更轮询(1s;仅存在预览 tab 时跑;stat 失败 → 元数据失败面)。
    /// 任务在最后一个预览 tab 关闭后自然退出(轮内发现无 tab 即 break)
    pub fn preview_poll_start(&mut self, cx: &mut Context<Self>) {
        if self.preview_poll.is_some() {
            return;
        }
        let store = cx.entity().clone();
        self.preview_poll = Some(cx.spawn(async move |_this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let mut changed = false;
                let alive = store.update(cx, |s, cx| {
                    let targets: Vec<PathBuf> = s
                        .panel_tabs
                        .iter()
                        .filter_map(|t| match t {
                            PanelTab::Preview(p) => Some(p.path.clone()),
                            _ => None,
                        })
                        .collect();
                    if targets.is_empty() {
                        return false;
                    }
                    let root = s.current_workspace_dir();
                    for rel in targets {
                        let Some(bucket) = s.preview.buckets.get_mut(&rel) else {
                            continue;
                        };
                        if bucket.unsupported {
                            continue;
                        }
                        let abs = root.as_ref().map(|r| r.join(&rel));
                        let observed = abs.as_deref().and_then(face::stat_version);
                        if observed != bucket.observed {
                            bucket.observed = observed;
                            bucket.meta_failed = observed.is_none();
                            changed = true;
                        }
                    }
                    if changed {
                        cx.notify();
                    }
                    true
                });
                if !alive {
                    break;
                }
            }
        }));
    }
}
