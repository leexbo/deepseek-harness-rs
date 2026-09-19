//! 附件 UI:
//! 草稿缩略条([`draft_rail`],文件选择 + 粘贴 + 拖放;入口为输入卡
//! 底排独立附件钮,composer.rs)、历史消息图与文件卡渲染
//! ([`message_images`]/[`message_files`])、Lightbox([`lightbox`])与
//! 拖拽邀请蒙层([`drop_overlay`])。
//!
//! 遮罩层以元素树顺序(后绘制在上)叠于内容上。外部文件拖放由
//! gpui-pre 翻译为内部 `active_drag`(值 = `ExternalPaths` 真实路径),
//! 蒙层在 `has_active_drag()` 时渲染并作为落点。语义:草稿图直接预览
//! bytes,历史图经 `read_attachment` 异步解码缓存(`image_cache`);
//! 文件直传源路径,发送时宿主落盘。

use gpui_kit::ExternalPaths;
use gpui_kit::component::IconName;
use gpui_kit::component::StyledExt;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnimationExt as _, App, Entity, Image, InteractiveElement, IntoElement, ObjectFit,
    ParentElement, SharedString, SpringAnimation, SpringConfig, StatefulInteractiveElement, Styled,
    StyledImage, div, img, point, px, rgba,
};

use crate::features::attachments::store::{DraftAttachment, image_size_text};
use crate::kits::icons::{LiumaIcon, fixed};
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 草稿卡尺寸常量(翻页步长留卡参照与卡片渲染同源)
const RAIL_CARD_IMAGE_W: f32 = 64.;
const RAIL_CARD_FILE_W: f32 = 240.;
const RAIL_GAP: f32 = 10.;

/// 草稿附件条(源 AttachmentRail):无滚动条,溢出由两端悬浮圆形箭头
/// 翻页;单一有序列表,图片 64px 缩略 + 文件卡 240×64 / gap10 / 圆角 16 /
/// 移除钮;渲染序 = 插入序(照源)。翻页/滚轮走弹簧动画(源 smooth 滚动
/// 的 GPUI 对应物;速度跨目标保留,连续点击自然接管)。
pub fn draft_rail(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let st = store.read(cx);
    if st.attachments.drafts.is_empty() {
        // 照源 countRef:清空即忘挂载基数,下次首挂载不跳尾;弹簧目标
        // 一并归零(重挂载弹簧从目标起步,陈旧目标会把轨拽去旧位置)
        st.attachments.rail_mount_count.set(None);
        st.attachments.rail_scroll_target.set(0.);
        return div().into_any_element();
    }
    // 照源:新增附件落在轨尾 → 滚到末尾露出;首挂载不动。此刻
    // max_offset 还是上一帧的,只挂标记,由 canvas 观察哨在 paint 期
    // 用当帧新鲜 max 落目标
    let grew = st
        .attachments
        .rail_mount_count
        .get()
        .is_some_and(|n| st.attachments.drafts.len() > n);
    st.attachments
        .rail_mount_count
        .set(Some(st.attachments.drafts.len()));
    if grew {
        // 照源 el.scrollLeft = scrollWidth - clientWidth(瞬时露尾)。
        // 目标必须在 construct 期一次落定并**换代弹簧**:若只改目标复
        // 用旧弹簧,动画帧没有任何泵保证(绘制期状态变更不触发下一帧
        // ——滚动条时代同款坑),弹簧永远看不到新目标。卡宽全固定 ⇒
        // 内容宽构造期可知,max = 内容宽 - 视口宽
        let viewport = st.attachments.rail_viewport_w.get();
        let max = (rail_content_w(&st.attachments.drafts) - viewport).max(0.);
        st.attachments.rail_scroll_target.set(-max);
        st.attachments
            .rail_seq
            .set(st.attachments.rail_seq.get() + 1);
    }
    let (left_on, right_on) = st.attachments.rail_edges.get();
    let items = st.attachments.drafts.clone();
    let wheel_store = store.clone();
    let watch_store = store.clone();
    let watch_handle = st.attachments.scroll_handle.clone();
    let spring_handle = st.attachments.scroll_handle.clone();
    let spring_target = px(st.attachments.rail_scroll_target.get());
    div()
        .id("draft-rail")
        .debug_selector(|| "draft-rail".to_string())
        .v_flex()
        .flex_shrink_0()
        .w_full()
        .px(px(16.))
        .pt(px(10.))
        .pb(px(2.))
        .child(
            div()
                .relative()
                .child({
                    // 弹簧持有轨道滚动位置:每帧把当前位置写进句柄
                    // (paint 期 clamp 越界),静止时写的是目标值,无害
                    let row = div()
                        .id("draft-rail-scroll")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(RAIL_GAP))
                        .overflow_hidden()
                        .overflow_x_scroll()
                        .track_scroll(&st.attachments.scroll_handle)
                        .on_scroll_wheel(move |event, window, cx| {
                            // 照源 wheel 语义:轨道上滚轮独占消费(背后的
                            // 会话列表不得跟滚),纵向滚轮横移。gpui 的
                            // 内部滚动监听注册在前、本回调在后,Bubble 相
                            // 逆序派发 ⇒ 先于内部处理,stop_propagation
                            // 恰好只断它自己与祖先
                            let d = event.delta.pixel_delta(window.line_height());
                            let dx = d.x.as_f32();
                            let dy = d.y.as_f32();
                            let pan = if dx != 0. {
                                dx
                            } else {
                                dy.signum() * dy.abs().min(60.)
                            };
                            wheel_store.update(cx, |st, cx| {
                                st.attachments.retarget(|t| t - pan);
                                cx.notify();
                            });
                            cx.stop_propagation();
                        })
                        .children(items.iter().map(|d| {
                            match d {
                                DraftAttachment::Image(im) => {
                                    draft_card(store, &im.id, im.image.clone()).into_any_element()
                                }
                                DraftAttachment::File(f) => {
                                    draft_file_card(store, f.id.clone(), f.name.clone(), f.size)
                                        .into_any_element()
                                }
                            }
                        }));
                    row.with_spring(
                        ("draft-rail-spring", st.attachments.rail_seq.get()),
                        SpringAnimation::new(SpringConfig::new(170., 26., 1.))
                            .with_epsilon(0.5)
                            .to(spring_target),
                        move |row, value| {
                            // 弹簧逐帧驱动:当前位置写入句柄(paint 期 clamp)
                            spring_handle.set_offset(point(value, px(0.)));
                            row
                        },
                    )
                })
                // 两端翻页箭头(照源 .arrow):可见性读 rail_edges
                // (canvas 推导;至多一帧滞后,notify 收敛)
                .when(left_on, |el| {
                    el.child(rail_arrow(
                        store,
                        -1.,
                        "draft-rail-arrow-left",
                        IconName::ChevronLeft,
                        true,
                    ))
                })
                .when(right_on, |el| {
                    el.child(rail_arrow(
                        store,
                        1.,
                        "draft-rail-arrow-right",
                        IconName::ChevronRight,
                        false,
                    ))
                })
                .child(
                    // 几何观察哨:paint 期(滚动行已画完,max_offset 已
                    // 落盘)读最新滚动几何:推导箭头可见性(变化才 notify
                    // ——自驱动收敛,不依赖环境帧)、用当帧新鲜 max 落
                    // 「新增露尾」目标、把越界目标收回可滚域。absolute
                    // 层无 hitbox 不挡交互;视口宽顺带供翻页步长
                    div().absolute().inset_0().child(gpui_kit::canvas(
                        move |b, _, cx| {
                            let o = watch_handle.offset().x;
                            let m = watch_handle.max_offset().x;
                            let edges = (o < px(-1.), o > px(1.) - m);
                            let viewport_w = b.size.width.as_f32();
                            watch_store.update(cx, |st, cx| {
                                let a = &st.attachments;
                                a.rail_viewport_w.set(viewport_w);
                                if a.rail_scroll_target.get() < -m.as_f32() {
                                    // 收缩(移卡/缩窗)后的越界目标:静默收回
                                    // 可滚域(句柄每帧已被 paint clamp,仅
                                    // 修弹簧目标;下一次交互 retarget 自愈)
                                    a.rail_scroll_target.set(-m.as_f32());
                                }
                                if a.rail_edges.get() != edges {
                                    a.rail_edges.set(edges);
                                    cx.notify();
                                }
                            });
                        },
                        |_, _, _, _| {},
                    )),
                ),
        )
        .into_any_element()
}

/// 草稿轨内容宽(卡宽全固定 ⇒ 构造期可知;露尾目标的 max 分子,
/// 与卡片渲染同源,改卡宽必两处)
fn rail_content_w(drafts: &[DraftAttachment]) -> f32 {
    let cards: f32 = drafts
        .iter()
        .map(|d| match d {
            DraftAttachment::Image(_) => RAIL_CARD_IMAGE_W,
            DraftAttachment::File(_) => RAIL_CARD_FILE_W,
        })
        .sum();
    let n = drafts.len() as f32;
    if n == 0. {
        0.
    } else {
        cards + RAIL_GAP * (n - 1.)
    }
}

/// 轨道端箭头(照源 .arrow:圆钮、距缘 4px、垂直居中于卡行;按钮底 +
/// 细边 + 投影分层,一级标签色 chevron;hover 增亮。真机反馈源版
/// 24px 次级色不够显眼 → 28px + DOCK 底 + LABEL 字)。`dir` = 翻页
/// 方向(-1 左 / 1 右,照源 page())。
fn rail_arrow(
    store: &Entity<AppStore>,
    dir: f32,
    sel: &'static str,
    icon: IconName,
    align_left: bool,
) -> impl IntoElement {
    let page_store = store.clone();
    div()
        .id(SharedString::from(sel))
        .debug_selector(move || sel.to_string())
        .absolute()
        .top(px(18.))
        .when(align_left, |el| el.left(px(4.)))
        .when(!align_left, |el| el.right(px(4.)))
        .size(px(28.))
        .rounded_full()
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::DOCK())
        .shadow(vec![
            gpui_kit::BoxShadow::new(px(0.), px(2.), rgba(0x00000029).into()).blur_radius(px(8.)),
        ])
        .cursor_pointer()
        .hover(|s| s.bg(theme::LAYER()))
        .text_color(theme::LABEL())
        .flex()
        .items_center()
        .justify_center()
        .child(fixed(icon, 16.))
        .on_click(move |_, _, cx| {
            // 照源 page():一步 = 视口宽 - 一张缩略宽(留住一张可见卡
            // 作位置参照),下限 200 保窄轨可用;走弹簧目标,不直写句柄
            page_store.update(cx, |st, cx| {
                let viewport = st.attachments.rail_viewport_w.get();
                let step = (viewport - RAIL_CARD_IMAGE_W).max(200.);
                st.attachments.retarget(|t| t - dir * step);
                cx.notify();
            });
        })
}

/// 单张草稿卡(64×64,圆角 16,cover;右上移除钮,点击开 Lightbox)
fn draft_card(
    store: &Entity<AppStore>,
    id: &str,
    image: std::sync::Arc<Image>,
) -> impl IntoElement {
    let id_label = id.to_string();
    let open_store = store.clone();
    let id_open = id_label.clone();
    let remove_store = store.clone();
    let id_rm = id_label.clone();
    let id_sel = id_label.clone();
    div()
        .id(SharedString::from(format!("draft-img-{id}")))
        .debug_selector(move || format!("draft-img-{}", id_sel).to_string())
        .relative()
        .size(px(RAIL_CARD_IMAGE_W))
        .flex_shrink_0()
        .rounded(px(16.))
        .overflow_hidden()
        .bg(theme::BORDER())
        .cursor_pointer()
        .on_click(move |_, _, cx| {
            open_store.update(cx, |st, cx| st.open_lightbox(&id_open, cx));
        })
        .child(img(image).w_full().h_full().object_fit(ObjectFit::Cover))
        .child(
            div()
                .id(SharedString::from("draft-remove"))
                .debug_selector(|| "draft-remove".to_string())
                .absolute()
                .top(px(4.))
                .right(px(4.))
                .size(px(18.))
                .rounded_full()
                // 底 0.72 黑(源 contrast-fill);hover 增亮 + 手型光标
                // = 可点反馈(常显钮的交互语言)
                .bg(rgba(0x000000B8))
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x000000D9)))
                .text_color(gpui_kit::white())
                .flex()
                .items_center()
                .justify_center()
                .on_click(move |_, _, cx| {
                    remove_store.update(cx, |st, cx| st.remove_draft(&id_rm, cx));
                })
                .child(fixed(IconName::Close, 10.)),
        )
}

/// 文件类型徽章(源 FileTypeIcon 对应物):28×28 彩色方块 +
/// 白字分类 mark(W / X / PPT / PDF / MD / IMG / </> / ▶ / FILE)
fn file_kind_badge(name: &str) -> impl IntoElement {
    let kind = liuma_attachment::classify_file_name(name);
    let mark = match kind {
        liuma_attachment::FileKind::Word => "W",
        liuma_attachment::FileKind::Excel => "X",
        liuma_attachment::FileKind::Ppt => "PPT",
        liuma_attachment::FileKind::Pdf => "PDF",
        liuma_attachment::FileKind::Markdown => "MD",
        liuma_attachment::FileKind::Image => "IMG",
        liuma_attachment::FileKind::Video => "▶",
        liuma_attachment::FileKind::Html | liuma_attachment::FileKind::Code => "</>",
        liuma_attachment::FileKind::Other => "FILE",
    };
    let mark_size = if mark.chars().count() > 2 { 7. } else { 10. };
    div()
        .size(px(28.))
        .flex_shrink_0()
        .rounded(px(6.))
        .bg(theme::FILE_KIND_BADGE(kind))
        .text_color(gpui_kit::white())
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(mark_size))
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .child(mark.to_string())
}

/// 草稿文件卡(源 FileCard:240×64,gap10,padding 0 12,圆角 16,
/// 发丝线边框;28px 类型徽章 + 名称省略 + 「扩展名 大小」meta;
/// 右上移除钮)
fn draft_file_card(
    store: &Entity<AppStore>,
    id: String,
    name: String,
    size: u64,
) -> impl IntoElement {
    let remove_store = store.clone();
    let id_rm = id.clone();
    let id_sel = id.clone();
    div()
        .id(SharedString::from(format!("draft-file-{id}")))
        .debug_selector(move || format!("draft-file-{}", id_sel).to_string())
        .relative()
        .flex_shrink_0()
        .child(file_card_body(&name, size))
        .child(
            div()
                .id(SharedString::from("draft-file-remove"))
                .debug_selector(|| "draft-file-remove".to_string())
                .absolute()
                .top(px(4.))
                .right(px(4.))
                .size(px(18.))
                .rounded_full()
                // 底 0.72 黑(源 contrast-fill);hover 增亮 + 手型光标
                // = 可点反馈(常显钮的交互语言)
                .bg(rgba(0x000000B8))
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x000000D9)))
                .text_color(gpui_kit::white())
                .flex()
                .items_center()
                .justify_center()
                .on_click(move |_, _, cx| {
                    remove_store.update(cx, |st, cx| st.remove_draft(&id_rm, cx));
                })
                .child(fixed(IconName::Close, 10.)),
        )
}

/// 历史消息文件渲染(源 MessageItem 文件卡同族):240×64 卡,
/// 类型徽章 + 名称省略 + 「扩展名 大小」meta;无预览(源侧点击仅
/// title 提示,侧栏预览是独立能力)。
pub fn message_files(files: &[serde_json::Value]) -> impl IntoElement {
    let cards: Vec<gpui_kit::AnyElement> = files
        .iter()
        .filter_map(|b| {
            let a = &b["attachment"];
            let name = a["name"].as_str()?;
            let size = a["bytes"].as_u64()?;
            Some(file_card_body(name, size).into_any_element())
        })
        .collect();
    if cards.is_empty() {
        return div().into_any_element();
    }
    div()
        .id("message-files")
        .debug_selector(|| "message-files".to_string())
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(10.))
        .children(cards)
        .into_any_element()
}

/// 文件卡卡体(草稿卡/历史卡共用形态:240×64 / 28 徽章 / 名称+meta)
fn file_card_body(name: &str, size: u64) -> impl IntoElement {
    div()
        .flex()
        .w(px(RAIL_CARD_FILE_W))
        .h(px(RAIL_CARD_IMAGE_W))
        .flex_shrink_0()
        .items_center()
        .gap(px(RAIL_GAP))
        .px(px(12.))
        .rounded(px(16.))
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::CARD())
        .child(file_kind_badge(name))
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(theme::LABEL())
                        .truncate()
                        .child(name.to_string()),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::LABEL_2())
                        .truncate()
                        .child(format!(
                            "{} {}",
                            liuma_attachment::file_extension_label(name),
                            image_size_text(size)
                        )),
                ),
        )
}

/// 历史消息图渲染(源 MessageImage/ImageGallery):
/// 1 张 → single(长边 240,cover);≥2 张 → 全部 tile 64px。
pub fn message_images(
    store: &Entity<AppStore>,
    blocks: &[serde_json::Value],
    cx: &App,
) -> impl IntoElement {
    if blocks.is_empty() {
        return div().into_any_element();
    }
    let single = blocks.len() == 1;
    let mut items: Vec<(String, std::sync::Arc<Image>)> = Vec::new();
    for b in blocks {
        let Some(id) = b["attachment"]["attachmentId"].as_str() else {
            continue;
        };
        if let Some(imgd) = store.read(cx).attachments.image_cache.get(id).cloned() {
            items.push((id.to_string(), imgd));
        }
    }
    if items.is_empty() {
        return div().into_any_element();
    }
    let inner: gpui_kit::AnyElement = if single {
        let (id, imgd) = items.remove(0);
        let sel = id.clone();
        let open_store = store.clone();
        // 单图尺寸:内在宽高在长边 240 内等比、不放大(源 MessageImage
        // 规则)。必须显式定高——img 无约束时高度塌 0(实测 240×0 隐形)
        let (rw, rh) = intrinsic_dims(&blocks[0]);
        let scale = (240.0 / (rw.max(rh)).max(1) as f32).min(1.0);
        let w = rw as f32 * scale;
        let h = rh as f32 * scale;
        div()
            .id(SharedString::from(format!("msg-img-{id}")))
            .debug_selector(move || format!("msg-img-{sel}").to_string())
            .flex_shrink_0()
            .rounded(px(12.))
            .overflow_hidden()
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                open_store.update(cx, |st, cx| st.open_lightbox(&id, cx));
            })
            .child(img(imgd).w(px(w)).h(px(h)).object_fit(ObjectFit::Cover))
            .into_any_element()
    } else {
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(10.))
            .children(items.iter().map(|(id, imgd)| {
                let open_store = store.clone();
                let id = id.clone();
                div()
                    .id(SharedString::from(format!("msg-img-{id}")))
                    .flex_shrink_0()
                    .size(px(64.))
                    .rounded(px(12.))
                    .overflow_hidden()
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        open_store.update(cx, |st, cx| st.open_lightbox(&id, cx));
                    })
                    .child(img(imgd.clone()).object_fit(ObjectFit::Cover))
            }))
            .into_any_element()
    };
    inner
}

/// 从 image 块读内在宽高(缺失回退 160×160,避免 0 尺寸隐形)
fn intrinsic_dims(block: &serde_json::Value) -> (u32, u32) {
    let a = &block["attachment"];
    let w = a["width"].as_u64().filter(|w| *w > 0).unwrap_or(160) as u32;
    let h = a["height"].as_u64().filter(|h| *h > 0).unwrap_or(160) as u32;
    (w, h)
}

/// Lightbox(源 ImageLightbox):全屏原图预览 + 关闭钮。放在根层
/// (元素树末尾后绘制 → 叠于内容上;源为 body portal 同构)。
pub fn lightbox(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let st = store.read(cx);
    let Some((_key, image)) = st.attachments.lightbox.clone() else {
        return div().into_any_element();
    };
    let close_store = store.clone();
    let close_store2 = store.clone();
    div()
        .id("lightbox")
        .debug_selector(|| "lightbox".to_string())
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x000000d1))
        .on_mouse_down(gpui_kit::MouseButton::Left, move |_, _, cx| {
            close_store.update(cx, |st, cx| st.close_lightbox(cx))
        })
        .child(
            img(image)
                .max_w(px(1600.))
                .max_h(px(1000.))
                .object_fit(ObjectFit::Contain)
                .rounded(px(12.)),
        )
        .child(
            div()
                .id(SharedString::from("lightbox-close"))
                .debug_selector(|| "lightbox-close".to_string())
                .absolute()
                .top(px(20.))
                .right(px(20.))
                .size(px(36.))
                .rounded_full()
                .bg(rgba(0x000000B8))
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x000000D9)))
                .text_color(gpui_kit::white())
                .flex()
                .items_center()
                .justify_center()
                .on_click(move |_, _, cx| {
                    close_store2.update(cx, |st, cx| st.close_lightbox(cx));
                })
                .child(fixed(IconName::Close, 16.)),
        )
        .into_any_element()
}

/// 拖拽邀请蒙层(源 DropOverlay 对应物):外部文件拖入窗口期间
/// 全屏遮罩 + 居中邀请卡;蒙层自身即落点(Submit 时按路径 intake,
/// 图片/文件通道由文件头分流)。shell 根层在 `has_active_drag()` 时
/// 渲染——gpui-pre 把 OS 文件拖放翻译为内部 active_drag(Entered 携带
/// 真实路径,MouseMove 拖动,MouseUp 提交);本应用无内部拖拽生产者,
/// 两者等价。注意:该 map 只增不清(debug_bounds),缺席断言不可用。
pub fn drop_overlay(store: &Entity<AppStore>) -> impl IntoElement {
    let intake_store = store.clone();
    div()
        .id("drop-overlay")
        .debug_selector(|| "drop-overlay".to_string())
        .absolute()
        .inset_0()
        .occlude()
        .bg(rgba(0x000000a6))
        .flex()
        .items_center()
        .justify_center()
        .on_drop(move |paths: &ExternalPaths, _window, cx| {
            intake_store.update(cx, |st, _| st.intake_dropped_paths(paths.paths()));
        })
        .child(
            div()
                .id("drop-overlay-card")
                .debug_selector(|| "drop-overlay-card".to_string())
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(24.))
                .py(px(16.))
                .rounded(px(16.))
                .border_1()
                .border_color(theme::BORDER())
                .bg(if theme::is_dark() {
                    theme::LAYER()
                } else {
                    theme::CARD()
                })
                .text_color(theme::LABEL())
                .text_size(px(14.))
                .child(fixed(LiumaIcon::Paperclip, 16.))
                .child("松开以添加附件"),
        )
}
