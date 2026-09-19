//! 文档预览视图:头行(路径/打开方式/换行/刷新)+ 变更提示条 +
//! 渲染器体(text/code/markdown/image)+ 页脚(加载更多/失败重试)。
//!
//! text 与 code 共用桶内 [`gpui_kit::ListState`](行虚拟化;code 带行号
//! 与高亮,text 可拖选)。wrap 关闭时行按 MaxContent 单行展示、超宽
//! 裁剪(gpui 列表虚拟化无横向滚动面;源为横向滚动,已列入披露
//! 偏差)。markdown 复用 kits::markdown。

use std::sync::Arc;

use std::path::PathBuf;

use gpui_kit::base::SelectableText;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{IconName, Sizable as _, StyledExt};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, ClickEvent, Entity, HighlightStyle, InteractiveElement, IntoElement, ObjectFit,
    ParentElement, StatefulInteractiveElement, Styled, StyledImage, StyledText, Window, div, img,
    px,
};

use super::store::PreviewBucket;
use crate::kits::filetype::{self, DocRenderer};
use crate::kits::icons::{LiumaIcon, fixed};
use crate::kits::theme;
use crate::shell::panel::PreviewTab;
use crate::shell::scroll::FullTrackHandle;
use crate::shell::store::AppStore;

/// 预览文档的拖选 order 基址(与 CHAT/PANEL 分区互不相交,见
/// kits::markdown 同名常量注释)
const PREVIEW_ORDER_BASE: u64 = 1 << 60;

/// 预览视图(面板正文;tab = 目标文件)
pub fn render(
    store: &Entity<AppStore>,
    tab: &PreviewTab,
    _window: &mut Window,
    cx: &mut App,
) -> gpui_kit::AnyElement {
    let rel = tab.path.clone();
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let abs_display = store
        .read(cx)
        .current_workspace_dir()
        .map(|root| root.join(&rel).display().to_string())
        .unwrap_or_else(|| rel.display().to_string());
    let Some(snap) = preview_snap(store, &rel, &name, cx) else {
        // 桶未建立(装载起步前的首帧)
        return div()
            .v_flex()
            .size_full()
            .min_h(px(0.))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(preview_loading()),
            )
            .into_any_element();
    };
    let mut col = div().v_flex().size_full().min_h(px(0.));
    col = col
        .child(preview_header(store, &rel, &abs_display, &snap))
        .when(snap.changed || snap.meta_failed, |this| {
            this.child(preview_changed_bar(store, &rel, snap.meta_failed))
        })
        .child(preview_body(store, &rel, &name, &snap, cx));
    col.into_any_element()
}

/// 桶快照(单次读取;行数据与滚动经 store 实时读)
fn preview_snap(
    store: &Entity<AppStore>,
    rel: &std::path::Path,
    name: &str,
    cx: &App,
) -> Option<PreviewSnap> {
    let st = store.read(cx);
    let b = st.preview.buckets.get(rel)?;
    Some(PreviewSnap {
        candidates: filetype::doc_candidates(name),
        renderer: b.current_renderer(name),
        wrap: b.wrap,
        loading: b.loading,
        failure: b.failure.clone(),
        has_content: !b.pages.is_empty() || b.complete.is_some(),
        unsupported: b.unsupported,
        changed: b.changed(),
        meta_failed: b.meta_failed,
        eof: b.eof,
        image: b.image.clone(),
        image_dims: b.image_dims,
    })
}

struct PreviewSnap {
    candidates: Vec<DocRenderer>,
    renderer: Option<DocRenderer>,
    wrap: bool,
    loading: bool,
    failure: Option<String>,
    has_content: bool,
    unsupported: bool,
    changed: bool,
    meta_failed: bool,
    eof: bool,
    image: Option<Arc<gpui_kit::Image>>,
    image_dims: Option<(u32, u32)>,
}

/// 装载中行(spinner + 文案)
fn preview_loading() -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .text_size(px(12.))
        .text_color(theme::CAPTION())
        .child(Spinner::new().small())
        .child("正在读取…")
}

/// 头行:完整路径(目录段灰 + basename 主色)+ 渲染器菜单(候选 > 1)
/// + 换行开关(仅 wrap 渲染器)+ 刷新钮
fn preview_header(
    store: &Entity<AppStore>,
    rel: &std::path::Path,
    abs_display: &str,
    snap: &PreviewSnap,
) -> impl IntoElement {
    let rel = rel.to_path_buf();
    let (prefix, last) = match abs_display.rsplit_once('/') {
        Some((head, tail)) if !tail.is_empty() => (format!("{head}/"), tail.to_string()),
        _ => (String::new(), abs_display.to_string()),
    };
    let s_reload = store.clone();
    let mut header = div()
        .id("preview-header")
        .debug_selector(|| "preview-header".to_string())
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(4.))
        .h(px(36.))
        .pl(px(12.))
        .pr(px(8.))
        .border_b_1()
        .border_color(theme::BORDER())
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_baseline()
                .text_size(px(12.))
                .child(div().truncate().text_color(theme::CAPTION()).child(prefix))
                .child(div().truncate().text_color(theme::LABEL_2()).child(last)),
        );
    // 渲染器菜单(候选 > 1 才显示;钮文案 = 当前渲染器名)
    if snap.candidates.len() > 1 {
        let s_menu = store.clone();
        let menu_rel = rel.clone();
        let title = snap.renderer.map(DocRenderer::title).unwrap_or("预览");
        header = header.child(
            div()
                .id("preview-renderer-menu")
                .debug_selector(|| "preview-renderer-menu".to_string())
                .flex()
                .items_center()
                .gap(px(4.))
                .h(px(24.))
                .px(px(8.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(theme::LABEL_2())
                .hover(|s| s.bg(theme::DOCK()).text_color(theme::LABEL()))
                .child(title)
                .child(fixed(IconName::ChevronDown, 11.))
                .on_click(move |ev: &ClickEvent, _, cx| {
                    let pos = match ev {
                        ClickEvent::Mouse(m) => m.down.position,
                        _ => gpui_kit::Point::default(),
                    };
                    s_menu.update(cx, |st, cx| {
                        st.preview_open_menu_at(menu_rel.clone(), pos, cx)
                    });
                }),
        );
    }
    // 换行开关(仅 wrap 渲染器;色示状态)
    if snap.renderer.is_some_and(DocRenderer::wrap) {
        let s_wrap = store.clone();
        let wrap_rel = rel.clone();
        let wrap_on = snap.wrap;
        header = header.child(
            div()
                .id("preview-wrap")
                .debug_selector(|| "preview-wrap".to_string())
                .flex()
                .size(px(24.))
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(if wrap_on {
                    theme::LABEL()
                } else {
                    theme::CAPTION()
                })
                .hover(|s| s.bg(theme::DOCK()))
                .child(fixed(LiumaIcon::TextWrap, 13.))
                .on_click(move |_, _, cx| {
                    s_wrap.update(cx, |st, cx| st.preview_toggle_wrap(&wrap_rel, cx));
                }),
        );
    }
    // 刷新
    header.child(
        div()
            .id("preview-refresh")
            .debug_selector(|| "preview-refresh".to_string())
            .flex()
            .size(px(24.))
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .text_color(theme::CAPTION())
            .hover(|s| s.bg(theme::DOCK()).text_color(theme::LABEL()))
            .child(fixed(LiumaIcon::RefreshCw, 13.))
            .on_click(move |_, _, cx| {
                s_reload.update(cx, |st, cx| st.preview_reload(&rel, cx));
            }),
    )
}

/// 变更/元数据失败提示条(照源:只提示不自动重载,占 header 下同一位)
fn preview_changed_bar(
    store: &Entity<AppStore>,
    rel: &std::path::Path,
    meta_failed: bool,
) -> impl IntoElement {
    let rel = rel.to_path_buf();
    let s_reload = store.clone();
    let text = if meta_failed {
        "文件不存在，可能已被移动或删除"
    } else {
        "文件已更新，当前显示为旧内容"
    };
    div()
        .id("preview-changed-bar")
        .debug_selector(|| "preview-changed-bar".to_string())
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_between()
        .h(px(30.))
        .px(px(12.))
        .bg(theme::LAYER())
        .border_b_1()
        .border_color(theme::BORDER())
        .text_size(px(12.))
        .text_color(theme::LABEL_2())
        .child(text)
        .child(
            div()
                .id("preview-changed-reload")
                .debug_selector(|| "preview-changed-reload".to_string())
                .flex()
                .items_center()
                .h(px(22.))
                .px(px(8.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(theme::BRAND())
                .hover(|s| s.bg(theme::DOCK()))
                .child("重新载入")
                .on_click(move |_, _, cx| {
                    s_reload.update(cx, |st, cx| st.preview_reload(&rel, cx));
                }),
        )
}

/// 正文分发
fn preview_body(
    store: &Entity<AppStore>,
    rel: &PathBuf,
    name: &str,
    snap: &PreviewSnap,
    cx: &mut App,
) -> gpui_kit::AnyElement {
    // 不可预览空态(不读取、无菜单)
    if snap.unsupported {
        return preview_empty_state("preview-unsupported", "该格式文件暂时无法预览", None);
    }
    // 无内容时的整面状态(loading / 失败)
    if !snap.has_content {
        if snap.loading {
            return div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .child(preview_loading())
                .into_any_element();
        }
        if let Some(failure) = snap.failure.clone() {
            let s_retry = store.clone();
            let retry_rel = rel.clone();
            return preview_empty_state(
                "preview-failure",
                &failure,
                Some((
                    "重试",
                    Box::new(move |cx: &mut App| {
                        s_retry.update(cx, |st, cx| st.preview_reload(&retry_rel, cx));
                    }),
                )),
            );
        }
    }
    let renderer = snap.renderer;
    let body = match renderer {
        Some(DocRenderer::Text) | Some(DocRenderer::Code) => {
            preview_lines_body(store, rel, name, snap, cx)
        }
        Some(DocRenderer::Markdown) => {
            let text = store
                .read(cx)
                .preview
                .buckets
                .get(rel)
                .map(PreviewBucket::joined_text)
                .unwrap_or_default();
            div()
                .id("preview-markdown-body")
                .debug_selector(|| "preview-markdown-body".to_string())
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .p(px(14.))
                .child(div().text_size(px(13.)).text_color(theme::LABEL_2()).child(
                    crate::kits::markdown_tv::tv_static("preview-markdown", &text),
                ))
                .into_any_element()
        }
        Some(DocRenderer::Image) => {
            if let Some(image) = snap.image.clone() {
                let dims = snap.image_dims;
                div()
                    .id("preview-image-body")
                    .debug_selector(|| "preview-image-body".to_string())
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .p(px(12.))
                    .child(
                        div()
                            .mx_auto()
                            .w_full()
                            .when_some(dims, |this, (w, h)| {
                                this.aspect_ratio(if h > 0 { w as f32 / h as f32 } else { 1. })
                            })
                            .child(img(image).size_full().object_fit(ObjectFit::Contain)),
                    )
                    .into_any_element()
            } else if snap.loading {
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(preview_loading())
                    .into_any_element()
            } else {
                preview_empty_state(
                    "preview-failure",
                    snap.failure.as_deref().unwrap_or("无法显示这张图片"),
                    None,
                )
            }
        }
        Some(DocRenderer::Pdf) => {
            let (dims, pages, pdf_failed) = {
                let st = store.read(cx);
                match st.preview.buckets.get(rel) {
                    Some(b) => (
                        b.pdf_dims.clone(),
                        b.pdf_dims.as_ref().map(|dims| {
                            (0..dims.len())
                                .map(|ix| b.pdf_pages.get(&ix).cloned())
                                .collect::<Vec<_>>()
                        }),
                        b.pdf_failed.clone(),
                    ),
                    None => (None, None, None),
                }
            };
            if let Some(failed) = pdf_failed {
                preview_empty_state("preview-failure", &failed, None)
            } else if let Some(dims) = dims
                && let Some(pages) = pages
            {
                // 页槽:宽高比占位(尺寸未栅格化先知),未就绪页居中
                //「正在绘制页面…」;栅格渐进回填(store 后台任务)
                let slots: Vec<gpui_kit::AnyElement> = dims
                    .iter()
                    .zip(pages)
                    .enumerate()
                    .map(|(ix, (dim, image))| {
                        let (w, h) = *dim;
                        let sel = format!("preview-pdf-page-{ix}");
                        let label = format!("PDF 第 {} 页：正在绘制页面…", ix + 1);
                        let body = match image {
                            Some(render) => img(gpui_kit::ImageSource::Render(render))
                                .size_full()
                                .into_any_element(),
                            None => div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(12.))
                                .text_color(theme::CAPTION())
                                .child(label)
                                .into_any_element(),
                        };
                        div()
                            .id(gpui_kit::SharedString::from(format!(
                                "preview-pdf-page-{ix}"
                            )))
                            .debug_selector(move || sel.clone())
                            .w_full()
                            .aspect_ratio(if h > 0. { w / h } else { 1. })
                            .flex_shrink_0()
                            .overflow_hidden()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(theme::BORDER())
                            .child(body)
                            .into_any_element()
                    })
                    .collect();
                div()
                    .id("preview-pdf-body")
                    .debug_selector(|| "preview-pdf-body".to_string())
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .p(px(12.))
                    .v_flex()
                    .gap(px(12.))
                    .children(slots)
                    .into_any_element()
            } else if snap.loading {
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(preview_loading())
                    .into_any_element()
            } else {
                preview_empty_state(
                    "preview-failure",
                    snap.failure.as_deref().unwrap_or("无法显示这份 PDF"),
                    None,
                )
            }
        }
        _ => div().flex_1().min_h(px(0.)).into_any_element(),
    };
    // 页尾:text-pages 未 eof → 「加载更多」;有内容时的失败 = 页尾状态行
    let paged = matches!(
        renderer,
        Some(DocRenderer::Text) | Some(DocRenderer::Code) | Some(DocRenderer::Markdown)
    );
    if paged && !snap.eof {
        let s_more = store.clone();
        let more_rel = rel.clone();
        let loading = snap.loading;
        return div()
            .v_flex()
            .flex_1()
            .min_h(px(0.))
            .child(body)
            .child(
                div()
                    .id("preview-load-more")
                    .debug_selector(|| "preview-load-more".to_string())
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(6.))
                    .h(px(36.))
                    .flex_shrink_0()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(theme::LABEL_2())
                    .hover(|s| s.bg(theme::DOCK()))
                    .when(loading, |this| this.child(Spinner::new().small()))
                    .child("加载更多")
                    .on_click(move |_, _, cx| {
                        s_more.update(cx, |st, cx| st.preview_load_more(&more_rel, cx));
                    }),
            )
            .into_any_element();
    }
    if paged && let Some(failure) = snap.failure.clone() {
        let s_retry = store.clone();
        let retry_rel = rel.clone();
        return div()
            .v_flex()
            .flex_1()
            .min_h(px(0.))
            .child(body)
            .child(
                div()
                    .id("preview-tail-failure")
                    .debug_selector(|| "preview-tail-failure".to_string())
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(10.))
                    .h(px(36.))
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .text_color(theme::CAPTION())
                    .child(failure)
                    .child(
                        div()
                            .id("preview-tail-retry")
                            .cursor_pointer()
                            .text_color(theme::BRAND())
                            .hover(|s| s.bg(theme::DOCK()))
                            .rounded(px(6.))
                            .px(px(8.))
                            .child("重试")
                            .on_click(move |_, _, cx| {
                                s_retry.update(cx, |st, cx| st.preview_reload(&retry_rel, cx));
                            }),
                    ),
            )
            .into_any_element();
    }
    body.into_any_element()
}

/// 居中空态(灰化大图标 + 文案;可选重试钮)
type RetryAction = (&'static str, Box<dyn Fn(&mut App)>);

fn preview_empty_state(
    selector: &str,
    text: &str,
    retry: Option<RetryAction>,
) -> gpui_kit::AnyElement {
    let mut el = div()
        .debug_selector(move || selector.to_string())
        .flex_1()
        .min_h(px(0.))
        .v_flex()
        .items_center()
        .justify_center()
        .gap(px(10.))
        .child(fixed(IconName::File, 36.).text_color(theme::CAPTION()))
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme::CAPTION())
                .child(text.to_string()),
        );
    if let Some((label, on_retry)) = retry {
        let sel = selector.to_string();
        el = el.child(
            div()
                .id(gpui_kit::SharedString::from(format!("{sel}-retry")))
                .debug_selector(move || format!("{sel}-retry"))
                .flex()
                .items_center()
                .h(px(26.))
                .px(px(12.))
                .rounded(px(6.))
                .cursor_pointer()
                .border_1()
                .border_color(theme::BORDER())
                .text_size(px(12.))
                .text_color(theme::LABEL_2())
                .hover(|s| s.bg(theme::DOCK()))
                .child(label)
                .on_click(move |_, _, cx| on_retry(cx)),
        );
    }
    el.into_any_element()
}

/// text/code 行体(桶内 ListState 虚拟化;行实时读 store;code 高亮
/// 整窗一次算好逐行取)
fn preview_lines_body(
    store: &Entity<AppStore>,
    rel: &PathBuf,
    name: &str,
    snap: &PreviewSnap,
    cx: &mut App,
) -> gpui_kit::AnyElement {
    let code = snap.renderer == Some(DocRenderer::Code);
    // spans/lines 都取 Arc(每帧 O(1) 拷贝);code 高亮在后台任务算好
    // 落桶(store::preview_maybe_kick_highlight),未就绪行按纯色渲染
    let (list_state, spans, highlight_line) = {
        let st = store.read(cx);
        match st.preview.buckets.get(rel) {
            Some(b) => (b.code_list.clone(), b.spans.clone(), b.highlight_line),
            None => return div().flex_1().min_h(px(0.)).into_any_element(),
        }
    };
    let _ = name;
    let row_store = store.clone();
    let row_rel = rel.clone();
    let rows = gpui_kit::list(list_state.clone(), move |ix, _window, cx| {
        let (line, wrap, highlighted) = {
            let st = row_store.read(cx);
            match st.preview.buckets.get(row_rel.as_path()) {
                Some(b) => (
                    b.lines.get(ix).cloned().unwrap_or_default(),
                    b.wrap,
                    b.highlight_line == Some(ix as u32 + 1),
                ),
                None => (String::new(), true, false),
            }
        };
        if code {
            let spans_line = spans.as_ref().and_then(|s| s.get(ix)).map(|line_spans| {
                line_spans
                    .iter()
                    .filter_map(|s| {
                        let start = s.offset;
                        let end = start + s.text.len();
                        (start < end).then(|| {
                            (
                                start..end,
                                HighlightStyle {
                                    color: Some(s.color.into()),
                                    ..Default::default()
                                },
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            });
            let mut row = div()
                .id(("preview-code-line", ix))
                .debug_selector(move || format!("preview-code-line-{ix}"))
                .flex()
                .items_baseline()
                .pl(px(8.))
                .pr(px(12.))
                .font_family("Menlo")
                .text_size(px(12.5))
                .line_height(gpui_kit::relative(1.6))
                .text_color(theme::LABEL_2())
                .when(highlighted, |this| this.bg(theme::LAYER()))
                .child(
                    div()
                        .w(px(40.))
                        .flex_shrink_0()
                        .text_right()
                        .pr(px(8.))
                        .text_size(px(11.))
                        .text_color(theme::CAPTION())
                        .child(format!("{}", ix + 1)),
                );
            if wrap {
                row = row.child(
                    div().min_w(px(0.)).flex_1().child(match spans_line {
                        Some(ranges) => StyledText::new(line)
                            .with_highlights(ranges)
                            .into_any_element(),
                        None => div().child(line).into_any_element(),
                    }),
                );
            } else {
                row = row.child(
                    div()
                        .min_w(px(0.))
                        .flex_1()
                        .truncate()
                        .child(match spans_line {
                            Some(ranges) => StyledText::new(line)
                                .with_highlights(ranges)
                                .into_any_element(),
                            None => div().child(line).into_any_element(),
                        }),
                );
            }
            row.into_any_element()
        } else {
            // 纯文本行:可拖选(域基址 + 行序;无行号,照源)
            let sel = SelectableText::new(("preview-text-line", ix), line)
                .document_order(PREVIEW_ORDER_BASE + ix as u64);
            div()
                .id(("preview-text-row", ix))
                .debug_selector(move || format!("preview-text-row-{ix}"))
                .flex()
                .px(px(14.))
                .py(px(1.))
                .text_size(px(13.))
                .line_height(gpui_kit::relative(1.6))
                .text_color(theme::LABEL_2())
                .when(highlighted, |this| this.bg(theme::LAYER()))
                .child(if wrap {
                    div().min_w(px(0.)).flex_1().child(sel).into_any_element()
                } else {
                    div()
                        .min_w(px(0.))
                        .flex_1()
                        .truncate()
                        .child(sel)
                        .into_any_element()
                })
                .into_any_element()
        }
    });
    let _ = highlight_line;
    div()
        .debug_selector(|| "preview-lines-body".to_string())
        .relative()
        .flex_1()
        .min_h(px(0.))
        .vertical_scrollbar(&FullTrackHandle::new(&list_state, px(0.)))
        // list 元素须显式尺寸(裸挂塌 0 高 → 可见范围空、行闭包不调用;
        // chat 列同款 h_full().w_full())
        .child(rows.h_full().w_full())
        .into_any_element()
}

/// 「打开方式」菜单卡(root 级定位渲染,同面板「+」菜单:occlude +
/// 整卡 mousedown 豁免 + 外点关闭);当前生效项打勾
pub fn renderer_menu_card(
    store: &Entity<AppStore>,
    rel: &PathBuf,
    pos: gpui_kit::Point<gpui_kit::Pixels>,
    cx: &App,
) -> impl IntoElement {
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let current = store
        .read(cx)
        .preview
        .buckets
        .get(rel)
        .and_then(|b| b.current_renderer(&name));
    let candidates = filetype::doc_candidates(&name);
    let items: Vec<gpui_kit::AnyElement> = candidates
        .iter()
        .copied()
        .map(|renderer| {
            let s = store.clone();
            let item_rel = rel.clone();
            let selected = current == Some(renderer);
            let sel_renderer = renderer;
            div()
                .id(gpui_kit::SharedString::from(format!(
                    "preview-renderer-item-{}",
                    renderer.id()
                )))
                .debug_selector(move || format!("preview-renderer-item-{}", sel_renderer.id()))
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(26.))
                .px(px(8.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(theme::LABEL_2())
                .hover(|st| st.bg(theme::DOCK()))
                .child(div().w(px(14.)).child(if selected {
                    fixed(IconName::Check, 12.)
                } else {
                    fixed(IconName::Check, 12.).opacity(0.)
                }))
                .child(div().text_size(px(12.)).child(renderer.title()))
                .on_click(move |_, _, cx| {
                    s.update(cx, |st, cx| {
                        st.preview_select_renderer(&item_rel, sel_renderer, cx)
                    });
                })
                .into_any_element()
        })
        .collect();
    div()
        .id("preview-renderer-menu-card")
        .absolute()
        .occlude()
        .debug_selector(|| "preview-renderer-menu-card".to_string())
        .top(pos.y + px(12.))
        .left(pos.x)
        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .v_flex()
        .w(px(140.))
        .gap(px(2.))
        .rounded(px(10.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::LAYER())
        .p(px(4.))
        .shadow_md()
        .children(items)
}
