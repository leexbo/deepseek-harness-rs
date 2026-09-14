//! 附件功能的状态与行为切片:草稿态/吸入/lightbox/拒收 toast。
//!
//! 承载 [`AttachmentsStore`](AppStore 的 `attachments` 字段)与该域的
//! `impl AppStore` 扩展块;类型 [`DraftImage`]/[`DraftFile`]/
//! [`AttachmentToast`] 与图片辅助函数随功能归此。跨功能调用面仅
//! [`AttachmentToast`] 与 [`attachment_error_text`](shell 发送路径的
//! 拒收文案单源)。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use dsh_attachment::{ImageAttachmentLimits, ImageMediaType};
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

/// 一条待发送草稿文件(源 ComposerFileAttachment 对应物;直传源路径,
/// 发送时宿主流式落盘,不读字节进内存)
#[derive(Clone)]
pub struct DraftFile {
    /// 草稿态临时 id(移除定位)
    pub id: String,
    /// 源路径(进程内直传宿主)
    pub path: PathBuf,
    /// 显示名(末分量)
    pub name: String,
    /// 字节数(卡 meta 行显示)
    pub size: u64,
}

/// 草稿态临时 id(nanos 十六进制;图片/文件通道共用形状)
fn new_draft_id() -> String {
    format!(
        "draft-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
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
        "COMMAND_FILES_UNSUPPORTED" => "命令不接受文件附件,请先移除文件".to_string(),
        "INVALID_FILE_NAME" | "INVALID_FILE_SOURCE" => "文件附件无效,请重新添加后再试".to_string(),
        _ => "图片发送失败,请重新添加图片后再试".to_string(),
    }
}

/// 附件大小文本(源 imageSizeText:字节 → 10MB / 2.5MB)
pub(crate) fn image_size_text(bytes: u64) -> String {
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

/// 一条待发送草稿附件(单一有序列表,照源 ComposerAttachments:渲染序
/// 与发送序 = 插入序;图片/文件不分校)
#[derive(Clone)]
pub(crate) enum DraftAttachment {
    /// 图片(字节管线,发送前不落盘)
    Image(DraftImage),
    /// 文件(源路径直传,发送时宿主落盘)
    File(DraftFile),
}

/// 附件功能切片状态(草稿轨/解码缓存/lightbox/拒收 toast)。
/// 作为 [`AppStore::attachments`] 单字段组合入根;默认空。
#[derive(Default)]
pub(crate) struct AttachmentsStore {
    /// 待发送草稿附件(插入序;发送前不落盘,失败保留)
    pub drafts: Vec<DraftAttachment>,
    /// 会话日志已引用附件 → 解码图缓存(历史消息渲染)
    pub image_cache: HashMap<String, Arc<gpui_kit::Image>>,
    /// Lightbox 打开的图(attachmentId/草稿 id)_Arc<Image>;根级渲染
    pub lightbox: Option<(String, Arc<gpui_kit::Image>)>,
    /// 附件通告(拒收 toast;Some = 展示)
    pub attachment_toast: Option<AttachmentToast>,
    /// 草稿轨滚动句柄(store 持有 = 滚动位置跨帧保留;箭头翻页用)
    pub scroll_handle: gpui_kit::ScrollHandle,
    /// 轨道视口宽(px;canvas paint 期捕获,点击翻页步长的分子)
    pub rail_viewport_w: std::cell::Cell<f32>,
    /// 两端箭头当前可见性。canvas paint 期由最新滚动几何推导(照源
    /// AttachmentRail updateEdges:1px 容差),变化才 notify;构造期读
    /// 此值有至多一帧滞后,由该 notify 驱动收敛帧
    pub rail_edges: std::cell::Cell<(bool, bool)>,
    /// 上帧草稿张数;None = 轨道未挂载(照源 countRef:首挂载不跳尾,
    /// 只有「已有轨上新增」才滚到末尾露出)
    pub rail_mount_count: std::cell::Cell<Option<usize>>,
    /// 滚动偏移的弹簧目标(offset 空间:0=轨头,负=已右滚)。弹簧元素
    /// 每帧把它写进滚动句柄,是轨道滚动位置的**唯一写主**;点击翻页/
    /// 滚轮/新增露尾都只改这里
    pub rail_scroll_target: std::cell::Cell<f32>,
    /// 弹簧元素换代序号:新增露尾时 +1,弹簧元素 id 随之更换 ⇒ 状态
    /// 重建、新弹簧**从目标起步**(源版露尾即瞬时赋值;若复用旧弹簧
    /// 则需依赖后续动画帧,而泵没有任何保证——绘制期状态变更不触发
    /// 下一帧,滚动条时代的老坑)
    pub rail_seq: std::cell::Cell<usize>,
}

impl AttachmentsStore {
    /// 弹簧目标重定位:变换后 clamp 到 [-max_offset, 0](offset 空间;
    /// max_offset 取上一帧绘制值,越界残差由 paint 期 clamp 兜底)
    pub(crate) fn retarget(&self, f: impl FnOnce(f32) -> f32) {
        let max = self.scroll_handle.max_offset().x.as_f32();
        self.rail_scroll_target
            .set(f(self.rail_scroll_target.get()).clamp(-max, 0.));
    }
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
    fn draft_image_count(&self) -> usize {
        self.attachments
            .drafts
            .iter()
            .filter(|d| matches!(d, DraftAttachment::Image(_)))
            .count()
    }

    /// 草稿图片总字节(超限检查)
    fn draft_image_bytes(&self) -> u64 {
        self.attachments
            .drafts
            .iter()
            .filter_map(|d| match d {
                DraftAttachment::Image(im) => Some(im.bytes.len() as u64),
                DraftAttachment::File(_) => None,
            })
            .sum()
    }

    /// 图片字节批量准入(源 intakeImages 序:格式 → 数量 → 单张压缩 →
    /// 总量;任一失败整批拒,返回 toast 文案)。合格产物按原序返回,
    /// **不落草稿**——由调用方按各自插入序组装。
    fn admit_image_bytes(
        &self,
        files: &[Vec<u8>],
    ) -> Result<Vec<(Vec<u8>, ImageMediaType)>, String> {
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
                return Err(self.attachment_error_text("UNSUPPORTED_IMAGE_TYPE"));
            };
            parsed.push((bytes.clone(), mime));
        }
        // 数量
        if self.draft_image_count() + parsed.len() > limits.max_images_per_message {
            return Err(self.attachment_error_text("TOO_MANY_IMAGES"));
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
                return Err(self.attachment_error_text("IMAGE_TOO_LARGE"));
            }
            compressed.push((bytes.clone(), media_type));
        }
        // 总字节
        let total =
            self.draft_image_bytes() + compressed.iter().map(|(b, _)| b.len() as u64).sum::<u64>();
        if total > limits.max_message_image_bytes {
            return Err(self.attachment_error_text("IMAGES_TOO_LARGE"));
        }
        Ok(compressed)
    }

    /// 粘贴图片 intake(剪贴板路径):整批准入后追加到草稿列表末尾
    /// (插入序 = 粘贴序)。返回 false 表示整批被拒(已设 toast)。
    pub fn intake_images(&mut self, files: &[Vec<u8>]) -> bool {
        let compressed = match self.admit_image_bytes(files) {
            Ok(v) => v,
            Err(text) => {
                self.attachments.attachment_toast = Some(AttachmentToast { text });
                return false;
            }
        };
        for (bytes, media_type) in compressed {
            let Some(image) = Self::decode_image(&bytes, media_type) else {
                continue;
            };
            self.attachments
                .drafts
                .push(DraftAttachment::Image(DraftImage {
                    id: new_draft_id(),
                    bytes,
                    image,
                    media_type: media_type.as_str().to_string(),
                    name: None,
                }));
        }
        true
    }

    /// 路径 intake(文件对话框 / 拖拽共用):按文件头嗅探分流,组装保持
    /// 路径序(照源单一有序列表)。图片子集整批准入——任一超限整批
    /// 不入轨(文件也不入,源 intakeFiles 同语义);文件直传源路径,
    /// 不读字节进内存,尺寸取元数据。
    pub fn intake_dropped_paths(&mut self, paths: &[std::path::PathBuf]) {
        enum Plan {
            Image(Vec<u8>),
            File(DraftFile),
        }
        let mut plan: Vec<Plan> = Vec::new();
        for p in paths {
            // 嗅探只读头部 32 字节(PNG/JPEG/WebP/GIF 魔数足够判定);
            // 嗅探为图但后续读取/解码失败 → 图片管线内拒收 toast
            let is_image = std::fs::File::open(p).ok().and_then(|mut f| {
                use std::io::Read as _;
                let mut head = [0u8; 32];
                let n = f.read(&mut head).ok()?;
                image::guess_format(&head[..n]).ok().map(|_| ())
            });
            match is_image {
                Some(()) => {
                    if let Ok(bytes) = std::fs::read(p) {
                        plan.push(Plan::Image(bytes));
                    }
                }
                None => {
                    let Ok(meta) = std::fs::metadata(p) else {
                        continue;
                    };
                    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    plan.push(Plan::File(DraftFile {
                        id: new_draft_id(),
                        path: p.clone(),
                        name: name.to_string(),
                        size: meta.len(),
                    }));
                }
            }
        }
        let image_bytes: Vec<Vec<u8>> = plan
            .iter()
            .filter_map(|p| match p {
                Plan::Image(bytes) => Some(bytes.clone()),
                Plan::File(_) => None,
            })
            .collect();
        let admitted = match self.admit_image_bytes(&image_bytes) {
            Ok(v) => v,
            Err(text) => {
                self.attachments.attachment_toast = Some(AttachmentToast { text });
                return;
            }
        };
        // 按计划序组装:图片按准入结果序回填(序与 plan 中图片出现序一致)
        let mut next_image = 0usize;
        for p in plan {
            match p {
                Plan::Image(_) => {
                    let (bytes, media_type) = &admitted[next_image];
                    next_image += 1;
                    let Some(image) = Self::decode_image(bytes, *media_type) else {
                        continue;
                    };
                    self.attachments
                        .drafts
                        .push(DraftAttachment::Image(DraftImage {
                            id: new_draft_id(),
                            bytes: bytes.clone(),
                            image,
                            media_type: media_type.as_str().to_string(),
                            name: None,
                        }));
                }
                Plan::File(f) => self.attachments.drafts.push(DraftAttachment::File(f)),
            }
        }
    }

    /// 移除一条草稿附件(id 定位;图片/文件同槽)
    pub fn remove_draft(&mut self, id: &str, cx: &mut Context<Self>) {
        self.attachments.drafts.retain(|d| match d {
            DraftAttachment::Image(im) => im.id != id,
            DraftAttachment::File(f) => f.id != id,
        });
        cx.notify();
    }

    /// 打开 Lightbox(草稿 id 或 attachmentId;key 双映射)
    pub fn open_lightbox(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(img) = self
            .attachments
            .drafts
            .iter()
            .find_map(|d| match d {
                DraftAttachment::Image(im) if im.id == key => Some(im.image.clone()),
                _ => None,
            })
            .or_else(|| self.attachments.image_cache.get(key).cloned())
        {
            self.attachments.lightbox = Some((key.to_string(), img));
            cx.notify();
        }
    }

    /// 把图片压到合规(准入拒绝外的增强路径):
    /// ①单边超限 → thumbnail 缩到上限(保持比例);
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
            "图片宽高不能超过 8192px,请缩小后重试"
        );
        assert_eq!(
            image_reject_text("IMAGE_TOO_MANY_PIXELS", &limits),
            "图片分辨率过大,请压缩后重试"
        );
        assert_eq!(
            image_reject_text("IMAGE_TOO_LARGE", &limits),
            "单张图片不能超过 20MB"
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
