//! 图片附件对象存储。
//!
//! 内容寻址不可变对象:`<root>/objects/<sha256 前2hex>/<sha256>`,
//! root = `~/.dshrs/attachments/v1`。写入去重 = 目标已存在即跳过
//! (同 id 必同字节);读取验证 digest。与源的差异:不做逐级 fsync
//! (桌面进程,O_EXCL/link 链简化为 tmp+rename,崩溃窗口只影响新附件)。

use std::path::PathBuf;

use sha2::{Digest, Sha256};

use dsh_session::attachments::{
    ImageAdmissionError, ImageAttachmentLimits, ImageAttachmentRef, ImageMediaType,
};

/// 一张待持久化图片(源 SaveImageAttachment)
#[derive(Debug, Clone)]
pub struct SaveImage {
    /// 编码字节
    pub data: Vec<u8>,
    /// 声明媒体类型
    pub media_type: ImageMediaType,
    /// 显示名(调用方已剥离路径;可缺省)
    pub name: Option<String>,
}

/// 存储层错误:准入类(用户可纠正,wire 带 reason)+ 存储故障类
#[derive(Debug, thiserror::Error)]
pub enum AttachmentStoreError {
    /// 准入拒绝(用户可纠正)
    #[error("{0}")]
    Admission(#[from] ImageAdmissionError),
    /// 引用 id 非法(非 sha256 形状)
    #[error("invalid attachment ref")]
    InvalidRef,
    /// 对象不存在
    #[error("attachment not found")]
    NotFound,
    /// 存储故障(读写失败/digest 不符)
    #[error("attachment io failure: {0}")]
    Io(String),
}

impl AttachmentStoreError {
    /// wire reason 串(准入类透传源码;故障类固定值)
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Admission(e) => e.reason(),
            Self::InvalidRef => "INVALID_ATTACHMENT_REF",
            Self::NotFound => "ATTACHMENT_NOT_FOUND",
            Self::Io(_) => "ATTACHMENT_READ_FAILED",
        }
    }
}

/// 校验后的图片元数据(解码产物)
struct ImageMeta {
    media_type: ImageMediaType,
    width: u32,
    height: u32,
}

/// 全解码校验:格式 ∈ 白名单、类型与声明一致、维度/像素在限内
/// (源 detectImage 语义:完整解码拒绝截断/损坏文件)。
fn detect_image(
    data: &[u8],
    declared: ImageMediaType,
    limits: &ImageAttachmentLimits,
) -> Result<ImageMeta, ImageAdmissionError> {
    let format = match image::guess_format(data) {
        Ok(f) => f,
        Err(_) => return Err(ImageAdmissionError::InvalidImage),
    };
    let media_type = match format {
        image::ImageFormat::Png => ImageMediaType::Png,
        image::ImageFormat::Jpeg => ImageMediaType::Jpeg,
        image::ImageFormat::WebP => ImageMediaType::Webp,
        image::ImageFormat::Gif => ImageMediaType::Gif,
        _ => return Err(ImageAdmissionError::UnsupportedImageType),
    };
    if media_type != declared {
        return Err(ImageAdmissionError::ImageTypeMismatch);
    }
    let reader = match image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
    {
        Some(dims) => dims,
        None => return Err(ImageAdmissionError::InvalidImage),
    };
    let (width, height) = reader;
    if u64::from(width) * u64::from(height) > limits.max_image_pixels {
        return Err(ImageAdmissionError::ImageTooManyPixels);
    }
    if width.max(height) > limits.max_image_dimension {
        return Err(ImageAdmissionError::ImageDimensionTooLarge);
    }
    // 强制完整解码:header 合法但截断的文件在此失败(同语义)
    if image::load_from_memory(data).is_err() {
        return Err(ImageAdmissionError::InvalidImage);
    }
    Ok(ImageMeta {
        media_type,
        width,
        height,
    })
}

/// 附件存储(线程安全:纯函数 + fs,无内部可变态)
#[derive(Debug, Clone)]
pub struct AttachmentStore {
    root: PathBuf,
    limits: ImageAttachmentLimits,
}

impl AttachmentStore {
    /// 以给定根构建(目录懒建)
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            limits: ImageAttachmentLimits::default(),
        }
    }

    /// 准入限制(客户端前置检查与错误文案共用)
    pub fn limits(&self) -> &ImageAttachmentLimits {
        &self.limits
    }

    /// 对象路径(id 模式校验 + 分桶)
    fn object_path(&self, id: &str) -> Result<PathBuf, AttachmentStoreError> {
        let hex = id
            .strip_prefix("sha256:")
            .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
            .ok_or(AttachmentStoreError::InvalidRef)?;
        Ok(self.root.join("objects").join(&hex[..2]).join(hex))
    }

    /// 批量准入(源 saveImages 序:张数 → 总量 → 逐张白名单/大小/解码;
    /// 全批先验证后写入——任一失败零落盘)。
    /// `existing_count`/`existing_bytes` = 草稿已有量(超限检查的权威面)。
    pub fn save_images(
        &self,
        inputs: &[SaveImage],
        existing_count: usize,
        existing_bytes: u64,
    ) -> Result<Vec<ImageAttachmentRef>, AttachmentStoreError> {
        if existing_count + inputs.len() > self.limits.max_images_per_message {
            return Err(ImageAdmissionError::TooManyImages.into());
        }
        let total = existing_bytes + inputs.iter().map(|i| i.data.len() as u64).sum::<u64>();
        if total > self.limits.max_message_image_bytes {
            return Err(ImageAdmissionError::ImagesTooLarge.into());
        }
        let mut metas = Vec::with_capacity(inputs.len());
        for input in inputs {
            if input.data.len() as u64 > self.limits.max_image_bytes {
                return Err(ImageAdmissionError::ImageTooLarge.into());
            }
            let meta = detect_image(&input.data, input.media_type, &self.limits)?;
            metas.push((input, meta));
        }
        let mut refs = Vec::with_capacity(inputs.len());
        for (input, meta) in metas {
            refs.push(self.write_object(input, &meta)?);
        }
        Ok(refs)
    }

    /// 单张落盘:sha256 → tmp 写入 → rename(已存在 = 同字节,跳过)
    fn write_object(
        &self,
        input: &SaveImage,
        meta: &ImageMeta,
    ) -> Result<ImageAttachmentRef, AttachmentStoreError> {
        let digest = Sha256::digest(&input.data);
        let attachment_id = format!("sha256:{}", hex::encode(digest));
        let target = self.object_path(&attachment_id)?;
        if !target.exists() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| AttachmentStoreError::Io(e.to_string()))?;
            }
            let tmp = target.with_extension("tmp");
            std::fs::write(&tmp, &input.data)
                .map_err(|e| AttachmentStoreError::Io(e.to_string()))?;
            // 同 id 必同字节:rename 覆盖(unix 原子)无损
            std::fs::rename(&tmp, &target).map_err(|e| AttachmentStoreError::Io(e.to_string()))?;
        }
        Ok(ImageAttachmentRef {
            attachment_id,
            media_type: meta.media_type,
            bytes: input.data.len() as u64,
            width: meta.width,
            height: meta.height,
            name: input.name.clone().filter(|n| !n.is_empty()),
        })
    }

    /// 读取对象字节(digest 验证;源 readImageFile 语义)
    pub fn read_image(&self, id: &str) -> Result<Vec<u8>, AttachmentStoreError> {
        let path = self.object_path(id)?;
        let data = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AttachmentStoreError::NotFound
            } else {
                AttachmentStoreError::Io(e.to_string())
            }
        })?;
        let digest = Sha256::digest(&data);
        if format!("sha256:{}", hex::encode(digest)) != id {
            return Err(AttachmentStoreError::Io("digest mismatch".into()));
        }
        Ok(data)
    }
}

/// dsh-llm 请求期图片字节来源(trait 注入;宿主实现)
impl dsh_llm::AttachmentSource for AttachmentStore {
    fn image_bytes(&self, id: &str) -> Option<Vec<u8>> {
        self.read_image(id).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1x1 PNG(image crate 编码产物——合法文件,全解码可过)
    fn png_1px() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(1, 1, image::Rgb([12, 34, 56]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    fn save() -> SaveImage {
        SaveImage {
            data: png_1px(),
            media_type: ImageMediaType::Png,
            name: Some("dot.png".into()),
        }
    }

    fn tmp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-attachments-{tag}-{}",
            std::process::id() * 1000 + (tag.len() as u32)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_and_read_roundtrip_content_addressed() {
        let root = tmp_root("rt");
        let store = AttachmentStore::new(&root);
        let refs = store.save_images(&[save()], 0, 0).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].width, 1);
        assert_eq!(refs[0].height, 1);
        assert_eq!(refs[0].bytes, save().data.len() as u64);
        assert_eq!(refs[0].name.as_deref(), Some("dot.png"));
        // 对象落位:objects/<2hex>/<sha256>
        let hex = refs[0].attachment_id.strip_prefix("sha256:").unwrap();
        assert!(root.join("objects").join(&hex[..2]).join(hex).exists());
        // 读回 = 原字节
        let expected = save().data;
        assert_eq!(store.read_image(&refs[0].attachment_id).unwrap(), expected);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_write_dedupes_without_error() {
        let root = tmp_root("dup");
        let store = AttachmentStore::new(&root);
        let a = store.save_images(&[save()], 0, 0).unwrap();
        let b = store
            .save_images(&[save()], 1, save().data.len() as u64)
            .unwrap();
        assert_eq!(a[0].attachment_id, b[0].attachment_id);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn count_and_total_limits_reject_batch_atomically() {
        let root = tmp_root("lim");
        let store = AttachmentStore::new(&root);
        let batch: Vec<SaveImage> = (0..21).map(|_| save()).collect();
        assert_eq!(
            store.save_images(&batch, 0, 0).unwrap_err().reason(),
            "TOO_MANY_IMAGES"
        );
        // 已有 19 张 + 新 2 张 = 21 → 拒
        assert_eq!(
            store
                .save_images(&[save(), save()], 19, 0)
                .unwrap_err()
                .reason(),
            "TOO_MANY_IMAGES"
        );
        // 零落盘:失败批不留对象
        assert!(!root.join("objects").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn type_mismatch_and_bad_id_rejected() {
        let root = tmp_root("bad");
        let store = AttachmentStore::new(&root);
        // 声明 jpeg 实为 png → IMAGE_TYPE_MISMATCH
        let mut wrong = save();
        wrong.media_type = ImageMediaType::Jpeg;
        assert_eq!(
            store.save_images(&[wrong], 0, 0).unwrap_err().reason(),
            "IMAGE_TYPE_MISMATCH"
        );
        // 非法 id 读取 → INVALID_ATTACHMENT_REF(不 panic)
        assert_eq!(
            store.read_image("not-an-id").unwrap_err().reason(),
            "INVALID_ATTACHMENT_REF"
        );
        // 合法形状但缺席 → ATTACHMENT_NOT_FOUND
        let absent = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            store.read_image(&absent).unwrap_err().reason(),
            "ATTACHMENT_NOT_FOUND"
        );
        // 截断 png → INVALID_IMAGE(全解码失败)
        let mut trunc = save();
        trunc.data.truncate(20);
        assert_eq!(
            store.save_images(&[trunc], 0, 0).unwrap_err().reason(),
            "INVALID_IMAGE"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
