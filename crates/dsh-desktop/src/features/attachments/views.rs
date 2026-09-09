//! 图片附件 UI:
//! 草稿缩略条([`draft_rail`],文件选择 + 粘贴;入口在命令菜单
//! 「添加 › 图片附件」行,composer.rs)、历史消息图渲染
//! ([`message_images`],single/tile)与 Lightbox([`lightbox`])。
//!
//! GPUI 0.2.2 无 z_index/backdrop_blur,文件 OS 拖放经 `active_drag`
//! (外部不可读)——故图片输入走**文件选择对话框**([`prompt_for_paths`])
//! 与**剪贴板粘贴**两个公开可行的入口;遮罩层以元素树顺序(后绘制在上)
//! 叠于内容上。语义:草稿图直接预览 bytes,历史图经 `read_attachment`
//! 异步解码缓存(`image_cache`)。

use gpui_kit::{
    App, Entity, Image, InteractiveElement, IntoElement, ObjectFit, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, StyledImage, div, img, px, rgba,
};
use gpui_kit::component::IconName;
use gpui_kit::component::StyledExt;

use crate::kits::icons::fixed;
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 草稿缩略条(源 AttachmentRail):64px 卡 / gap10 / 圆角 16 / 移除钮。
/// 仅在有草稿时渲染;单卡点击开 Lightbox。
pub fn draft_rail(store: &Entity<AppStore>, cx: &App) -> impl IntoElement {
    let st = store.read(cx);
    if st.attachments.draft_images.is_empty() {
        return div().into_any_element();
    }
    let items = st.attachments.draft_images.clone();
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
                .id("draft-rail-scroll")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .overflow_x_scroll()
                .children(
                    items
                        .iter()
                        .map(|d| draft_card(store, cx, &d.id, d.image.clone())),
                ),
        )
        .into_any_element()
}

/// 单张草稿卡(64×64,圆角 16,cover;右上移除钮,点击开 Lightbox)
fn draft_card(
    store: &Entity<AppStore>,
    _cx: &App,
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
        .size(px(64.))
        .flex_shrink_0()
        .rounded(px(16.))
        .overflow_hidden()
        .bg(theme::BORDER())
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
                .bg(rgba(0x0000008c))
                .text_color(gpui_kit::white())
                .flex()
                .items_center()
                .justify_center()
                .on_click(move |_, _, cx| {
                    remove_store.update(cx, |st, cx| st.remove_draft_image(&id_rm, cx));
                })
                .child(fixed(IconName::Close, 10.)),
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
    let mut items: Vec<std::sync::Arc<Image>> = Vec::new();
    for b in blocks {
        let Some(id) = b["attachment"]["attachmentId"].as_str() else {
            continue;
        };
        if let Some(imgd) = store.read(cx).attachments.image_cache.get(id).cloned() {
            items.push(imgd);
        }
    }
    if items.is_empty() {
        return div().into_any_element();
    }
    let inner = if single {
        div()
            .flex_shrink_0()
            .max_w(px(240.))
            .rounded(px(12.))
            .overflow_hidden()
            .child(
                img(items[0].clone())
                    .w(px(240.))
                    .object_fit(ObjectFit::Cover),
            )
    } else {
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(10.))
            .children(items.iter().map(|imgd| {
                div()
                    .size(px(64.))
                    .rounded(px(12.))
                    .overflow_hidden()
                    .child(img(imgd.clone()).object_fit(ObjectFit::Cover))
            }))
    };
    inner.into_any_element()
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
                .bg(rgba(0x00000099))
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
