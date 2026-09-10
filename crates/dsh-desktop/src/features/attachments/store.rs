//! 图片附件功能的状态与行为切片:草稿态/吸入/lightbox/拒收 toast。
//!
//! 承载 [`AttachmentsStore`](AppStore 的 `attachments` 字段)与该域的
//! `impl AppStore` 扩展块;类型 [`DraftImage`]/[`AttachmentToast`] 与
//! 图片辅助函数随功能归此。跨功能调用面仅 [`AttachmentToast`] 与
//! [`attachment_error_text`](shell 发送路径的拒收文案单源)。

use std::collections::HashMap;
use std::sync::Arc;

use dsh_core::attachments::{ImageAttachmentLimits, ImageMediaType};
use gpui_kit::Context;

use crate::shell::store::AppStore;

/// 一条待发送草稿图片(发送前 host 准入;bytes 供 base64 入 content)
#[derive(Clone)]
pub struct DraftImage {
    /// 草稿态临时 id(预览渲染 Key / 移除定位)
    pub id: String,
    /// 原始编码字节(白名单外已拒;此处必属 png/jpeg/webp/gif)
    pub bytes: Vec<u8>,
    /// 解码后的可渲染图
    pub image: Arc<gpui_kit::Image>,
    /// 媒体类型(MIME 串)
    pub media_type: String,
    /// 显示名(可空)
    pub name: Option<String>,
}

/// 附件拒收 toast(源 image-labels 文案)
#[derive(Clone)]
pub struct AttachmentToast {
    /// 正文
    pub text: String,
}

/// 图片拒收文案(单源 reason → zh;额度数值经 limits 展开,与源
/// image.sendFailed 模板对齐;映射不全回退通用失败文案)
pub(crate) fn image_reject_text(reason: &str, limits: &ImageAttachmentLimits) -> String {
    match reason {
        "TOO_MANY_IMAGES" => format!("一条消息最多添加 {} 张图片", limits.max_images_per_message),
        "IMAGES_TOO_LARGE" => format!(
            "图片总大小超过 {},请移除部分图片",
            image_size_text(limits.max_message_image_bytes)
        ),
        "UNSUPPORTED_IMAGE_TYPE" => "仅支持 PNG、JPG、WebP、GIF 格式的图片".to_string(),
        "IMAGE_TOO_LARGE" => format!(
            "单张图片不能超过 {}",
            image_size_text(limits.max_image_bytes)
        ),
        "IMAGE_TOO_MANY_PIXELS" => "图片分辨率过大,请压缩后重试".to_string(),
        "IMAGE_DIMENSION_TOO_LARGE" => format!(
            "图片宽高不能超过 {}px,请缩小后重试",
            limits.max_image_dimension
        ),
        "INVALID_IMAGE_BASE64" | "INVALID_IMAGE" | "IMAGE_TYPE_MISMATCH" => {
            "图片编码无效".to_string()
        }
        "MODEL_DOES_NOT_SUPPORT_IMAGES" => "当前模型不支持图片,请切换支持图片的模型".to_string(),
        _ => "图片发送失败,请重新添加图片后再试".to_string(),
    }
}

/// 附件大小文本(源 imageSizeText:字节 → 10MB / 2.5MB)
fn image_size_text(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const KB: u64 = 1024;
    if bytes >= MB {
        let mb = bytes as f64 / MB as f64;
        if mb.fract() < 0.05 {
            format!("{mb:.0}MB")
        } else if mb.fract() < 0.95 {
            format!("{mb:.1}MB")
        } else {
            format!("{mb:.0}MB")
        }
    } else if bytes >= KB {
        format!("{:.0}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

/// 按原格式编码(动态图 → 对应 ImageFormat;GIF 动图不经此路)
fn encode_image(
    img: &image::DynamicImage,
    media_type: ImageMediaType,
) -> image::ImageResult<Vec<u8>> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let format = match media_type {
        ImageMediaType::Png => image::ImageFormat::Png,
        ImageMediaType::Jpeg => image::ImageFormat::Jpeg,
        ImageMediaType::Webp => image::ImageFormat::WebP,
        ImageMediaType::Gif => image::ImageFormat::Gif,
    };
    img.write_to(&mut buf, format)?;
    Ok(buf.into_inner())
}

/// JPEG 质量编码(降级阶梯用;JpegEncoder::new_with_quality)
fn encode_jpeg_quality(img: &image::DynamicImage, quality: u8) -> image::ImageResult<Vec<u8>> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    img.write_with_encoder(enc)?;
    Ok(buf.into_inner())
}

/// 图片附件功能切片状态(草稿轨/解码缓存/lightbox/拒收 toast)。
/// 作为 [`AppStore::attachments`] 单字段组合入根;默认空。
#[derive(Default)]
pub(crate) struct AttachmentsStore {
    /// 待发送草稿图片(发送前不落盘,失败保留)
    pub draft_images: Vec<DraftImage>,
    /// 会话日志已引用附件 → 解码图缓存(历史消息渲染)
    pub image_cache: HashMap<String, Arc<gpui_kit::Image>>,
    /// Lightbox 打开的图(attachmentId/草稿 id)_Arc<Image>;根级渲染
    pub lightbox: Option<(String, Arc<gpui_kit::Image>)>,
    /// 附件通告(拒收 toast;Some = 展示)
    pub attachment_toast: Option<AttachmentToast>,
}

impl AppStore {
    /// 附件拒收文案(reason → zh;映射单源见 [`image_reject_text`])
    pub fn attachment_error_text(&self, reason: &str) -> String {
        image_reject_text(reason, self.bridge.host().image_limits())
    }

    // ── 图片附件(intake 前置检查 / 草稿态 / 拖拽)──────────────────

    /// 解码字节 → 可渲染图(mime 白名单由调用方保证)
    fn decode_image(bytes: &[u8], media_type: ImageMediaType) -> Option<Arc<gpui_kit::Image>> {
        let format = match media_type {
            ImageMediaType::Png => gpui_kit::ImageFormat::Png,
            ImageMediaType::Jpeg => gpui_kit::ImageFormat::Jpeg,
            ImageMediaType::Webp => gpui_kit::ImageFormat::Webp,
            ImageMediaType::Gif => gpui_kit::ImageFormat::Gif,
        };
        Some(Arc::new(gpui_kit::Image::from_bytes(
            format,
            bytes.to_vec(),
        )))
    }

    /// 草稿图片总数(超限检查)
    fn draft_count(&self) -> usize {
        self.attachments.draft_images.len()
    }

    /// 把图片压到合规(准入拒绝外的增强路径):
    /// ①单边 >4096px → thumbnail 缩到 4096(保持比例);
    /// ②编码后仍 >单张字节上限 → JPEG 质量阶梯降级。
    /// GIF 动图不压(压动图丢动画,保持现状——要么合规要么拒)。
    /// 返回压后的 bytes + media_type;无法压(编码失败/仍超限)返回 None(调用方回退拒收)。
    fn compress_to_limits(
        bytes: &[u8],
        media_type: ImageMediaType,
        limits: &ImageAttachmentLimits,
    ) -> Option<(Vec<u8>, ImageMediaType)> {
        if media_type == ImageMediaType::Gif {
            return None; // 动图不压
        }
        let dim = image::load_from_memory(bytes).ok()?;
        let mut img = dim;
        let max_dim = limits.max_image_dimension.max(1);
        let (w, h) = (img.width(), img.height());
        let over_dim = w.max(h) > max_dim;
        let over_bytes = bytes.len() as u64 > limits.max_image_bytes;
        if !over_dim && !over_bytes {
            // 已合规:原样返回(不重编码,保质量)
            return Some((bytes.to_vec(), media_type));
        }
        if over_dim {
            // 缩到单边 max_dim(保持比例)
            let (nw, nh) = if w >= h {
                (
                    max_dim,
                    (h as f64 * max_dim as f64 / w as f64).max(1.0) as u32,
                )
            } else {
                (
                    (w as f64 * max_dim as f64 / h as f64).max(1.0) as u32,
                    max_dim,
                )
            };
            img = img.resize(nw, nh, image::imageops::FilterType::Lanczos3);
        }
        // 编码回原格式(先试原格式,超字节再降 JPEG 质量)
        if let Ok(buf) = encode_image(&img, media_type)
            && buf.len() as u64 <= limits.max_image_bytes
        {
            return Some((buf, media_type));
        }
        // 仍超字节:JPEG 质量阶梯降级(85→70→55→40)
        for q in [85u8, 70, 55, 40] {
            if let Ok(buf) = encode_jpeg_quality(&img, q)
                && buf.len() as u64 <= limits.max_image_bytes
            {
                return Some((buf, ImageMediaType::Jpeg));
            }
        }
        None
    }

    /// 草稿图片总字节(超限检查)
    fn draft_bytes(&self) -> u64 {
        self.attachments
            .draft_images
            .iter()
            .map(|d| d.bytes.len() as u64)
            .sum()
    }

    /// intake 前置检查(源 intakeImages 序:格式 → 数量 → 单张 → 总量;
    /// 整批原子拒绝,进 toast 文案,零落盘)。
    /// 返回 false 表示整批被拒(已设 attachment_toast)。
    pub fn intake_images(&mut self, files: &[Vec<u8>]) -> bool {
        let limits = self.bridge.host().image_limits();
        // 全批先解码(格式/类型检查):任一非白名单 → 拒
        let mut parsed: Vec<(Vec<u8>, ImageMediaType)> = Vec::new();
        for bytes in files {
            // 用 image crate 猜格式(与宿主准入同白名单)
            let mime = image::guess_format(bytes).ok().and_then(|f| match f {
                image::ImageFormat::Png => Some(ImageMediaType::Png),
                image::ImageFormat::Jpeg => Some(ImageMediaType::Jpeg),
                image::ImageFormat::WebP => Some(ImageMediaType::Webp),
                image::ImageFormat::Gif => Some(ImageMediaType::Gif),
                _ => None,
            });
            let Some(mime) = mime else {
                self.attachments.attachment_toast = Some(AttachmentToast {
                    text: self.attachment_error_text("UNSUPPORTED_IMAGE_TYPE"),
                });
                return false;
            };
            parsed.push((bytes.clone(), mime));
        }
        // 数量
        if self.draft_count() + parsed.len() > limits.max_images_per_message {
            self.attachments.attachment_toast = Some(AttachmentToast {
                text: self.attachment_error_text("TOO_MANY_IMAGES"),
            });
            return false;
        }
        // 单张压到合规(超单边/字节 → 压缩;GIF 动图/压失败且仍超 → 拒)
        let mut compressed: Vec<(Vec<u8>, ImageMediaType)> = Vec::new();
        for (bytes, media_type) in parsed {
            let needs = bytes.len() as u64 > limits.max_image_bytes;
            if needs || media_type != ImageMediaType::Gif {
                // 尝试压缩(GIF 不动图;已合规的非 GIF 也过一遍——单边可能超)
                if let Some((cb, cm)) = Self::compress_to_limits(&bytes, media_type, limits) {
                    compressed.push((cb, cm));
                    continue;
                }
            }
            // 压缩失败/未压:看是否超单张字节(超则拒)
            if bytes.len() as u64 > limits.max_image_bytes {
                self.attachments.attachment_toast = Some(AttachmentToast {
                    text: self.attachment_error_text("IMAGE_TOO_LARGE"),
                });
                return false;
            }
            compressed.push((bytes.clone(), media_type));
        }
        // 总字节
        let total =
            self.draft_bytes() + compressed.iter().map(|(b, _)| b.len() as u64).sum::<u64>();
        if total > limits.max_message_image_bytes {
            self.attachments.attachment_toast = Some(AttachmentToast {
                text: self.attachment_error_text("IMAGES_TOO_LARGE"),
            });
            return false;
        }
        for (bytes, media_type) in compressed {
            let Some(image) = Self::decode_image(&bytes, media_type) else {
                continue;
            };
            self.attachments.draft_images.push(DraftImage {
                id: format!(
                    "draft-{:x}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                ),
                bytes,
                image,
                media_type: media_type.as_str().to_string(),
                name: None,
            });
        }
        true
    }

    /// 拖拽文件路径 intake(路径读字节;拒绝整批)
    pub fn intake_dropped_paths(&mut self, paths: &[std::path::PathBuf]) {
        let mut files = Vec::new();
        for p in paths {
            if let Ok(bytes) = std::fs::read(p) {
                files.push(bytes);
            }
        }
        if !files.is_empty() {
            self.intake_images(&files);
        }
    }

    /// 移除一张草稿图(id 定位)
    pub fn remove_draft_image(&mut self, id: &str, cx: &mut Context<Self>) {
        self.attachments.draft_images.retain(|d| d.id != id);
        cx.notify();
    }

    /// 打开 Lightbox(草稿 id 或 attachmentId;key 双映射)
    pub fn open_lightbox(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(img) = self
            .attachments
            .draft_images
            .iter()
            .find(|d| d.id == key)
            .map(|d| d.image.clone())
            .or_else(|| self.attachments.image_cache.get(key).cloned())
        {
            self.attachments.lightbox = Some((key.to_string(), img));
            cx.notify();
        }
    }

    pub fn close_lightbox(&mut self, cx: &mut Context<Self>) {
        self.attachments.lightbox = None;
        cx.notify();
    }

    /// 会话事件含 image 块时异步拉取历史图(读宿主 → base64 → 解码 → 缓存)
    pub fn ensure_image_loaded(&mut self, id: &str, attachment_id: &str, cx: &mut Context<Self>) {
        if self.attachments.image_cache.contains_key(attachment_id) {
            return;
        }
        let host = self.bridge.host().clone();
        let sid = id.to_string();
        let aid = attachment_id.to_string();
        let store = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            use base64::Engine as _;
            let rpc = host.read_attachment(&sid, &aid);
            let img = rpc.ok().and_then(|v| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(v["data"].as_str().unwrap_or_default())
                    .ok()?;
                let mime = ImageMediaType::parse(
                    v["attachment"]["mediaType"].as_str().unwrap_or_default(),
                )?;
                AppStore::decode_image(&bytes, mime)
            });
            let Some(img) = img else {
                return;
            };
            store.update(cx, |s, cx| {
                s.attachments.image_cache.insert(aid, img);
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 图片拒收文案(reason → zh;含维度/像素映射)
    #[test]
    fn image_reject_text_maps_all_reasons() {
        let limits = ImageAttachmentLimits::default();
        assert_eq!(
            image_reject_text("IMAGE_DIMENSION_TOO_LARGE", &limits),
            "图片宽高不能超过 4096px,请缩小后重试"
        );
        assert_eq!(
            image_reject_text("IMAGE_TOO_MANY_PIXELS", &limits),
            "图片分辨率过大,请压缩后重试"
        );
        assert_eq!(
            image_reject_text("IMAGE_TOO_LARGE", &limits),
            "单张图片不能超过 3.5MB"
        );
        assert_eq!(
            image_reject_text("TOO_MANY_IMAGES", &limits),
            "一条消息最多添加 20 张图片"
        );
        assert_eq!(
            image_reject_text("UNKNOWN", &limits),
            "图片发送失败,请重新添加图片后再试"
        );
    }
}
