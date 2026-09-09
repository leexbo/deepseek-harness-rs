//! 图片附件词汇。
//!
//! 附件是**内容寻址不可变对象**:`sha256:<64hex>` 为 id,字节存宿主侧
//! 对象存储(dsh-host),会话日志只携带引用([`ImageAttachmentRef`])。
//! 引用出现在 `user/message` 的块数组内容里(`{type:"image", attachment}`
//! 块;图前文后),纯文本消息的内容保持顶层字符串(既有事实面)。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 光栅图媒体类型(白名单即全集;源 ImageMediaType)。
/// wire 形状 = MIME 串(`image/png` 等),非变体名。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageMediaType {
    /// PNG
    Png,
    /// JPEG
    Jpeg,
    /// WebP
    Webp,
    /// GIF
    Gif,
}

impl Serialize for ImageMediaType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ImageMediaType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        ImageMediaType::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("未知图片媒体类型:{s}")))
    }
}

impl ImageMediaType {
    /// MIME 串(wire/存储面)
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
        }
    }

    /// MIME 串解析(白名单外返回 None)
    pub fn parse(mime: &str) -> Option<Self> {
        match mime {
            "image/png" => Some(Self::Png),
            "image/jpeg" | "image/jpg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            "image/gif" => Some(Self::Gif),
            _ => None,
        }
    }

    /// 媒体类型对应的文件扩展名(ZIP 导出 media/ 条目路径用)
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Gif => "gif",
        }
    }
}

/// 一张已持久化图片的不可变引用(源 ImageAttachmentRef;camelCase wire)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachmentRef {
    /// 对象 id(`sha256:<64hex>`)
    pub attachment_id: String,
    /// 编码媒体类型(从存储字节验证)
    pub media_type: ImageMediaType,
    /// 精确编码字节数
    pub bytes: u64,
    /// 内在编码宽(px)
    pub width: u32,
    /// 内在编码高(px)
    pub height: u32,
    /// 显示名(已剥离路径;可缺省)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ImageAttachmentRef {
    /// 序列化为 user/message 块数组里的 image 块
    pub fn to_block(&self) -> Value {
        serde_json::to_value(self)
            .ok()
            .map(|attachment| serde_json::json!({ "type": "image", "attachment": attachment }))
            .unwrap_or(Value::Null)
    }

    /// 从事件数据里的 image 块还原引用(形状不符返回 None)
    pub fn from_block(block: &Value) -> Option<Self> {
        if block["type"].as_str() != Some("image") {
            return None;
        }
        let a = &block["attachment"];
        Some(Self {
            attachment_id: a["attachmentId"].as_str()?.to_string(),
            media_type: ImageMediaType::parse(a["mediaType"].as_str()?)?,
            bytes: a["bytes"].as_u64()?,
            width: a["width"].as_u64()? as u32,
            height: a["height"].as_u64()? as u32,
            name: a["name"].as_str().map(String::from),
        })
    }
}

/// 图片准入限制(源 ImageAttachmentLimits;attachment-local 默认值)
#[derive(Debug, Clone, PartialEq)]
pub struct ImageAttachmentLimits {
    /// 单张字节上限(3.5MB)
    pub max_image_bytes: u64,
    /// 单条消息张数上限
    pub max_images_per_message: usize,
    /// 单条消息图片总字节上限
    pub max_message_image_bytes: u64,
    /// 解码像素上限
    pub max_image_pixels: u64,
    /// 单边像素上限
    pub max_image_dimension: u32,
}

impl Default for ImageAttachmentLimits {
    fn default() -> Self {
        Self {
            max_image_bytes: 3_670_016, // 3.5MB
            max_images_per_message: 20,
            max_message_image_bytes: 100 * 1024 * 1024,
            max_image_pixels: 40_000_000,
            max_image_dimension: 4096,
        }
    }
}

/// 图片准入错误码(wire `details.reason`;源 ImageAdmissionErrorCode 全集)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageAdmissionError {
    /// 超单条消息张数
    TooManyImages,
    /// 超单条消息总字节
    ImagesTooLarge,
    /// 白名单外媒体类型
    UnsupportedImageType,
    /// 字节无法解码为图片
    InvalidImage,
    /// 声明类型与实际解码格式不符
    ImageTypeMismatch,
    /// 超单张字节
    ImageTooLarge,
    /// 超解码像素
    ImageTooManyPixels,
    /// 超单边像素
    ImageDimensionTooLarge,
}

impl ImageAdmissionError {
    /// wire reason 串
    pub fn reason(&self) -> &'static str {
        match self {
            Self::TooManyImages => "TOO_MANY_IMAGES",
            Self::ImagesTooLarge => "IMAGES_TOO_LARGE",
            Self::UnsupportedImageType => "UNSUPPORTED_IMAGE_TYPE",
            Self::InvalidImage => "INVALID_IMAGE",
            Self::ImageTypeMismatch => "IMAGE_TYPE_MISMATCH",
            Self::ImageTooLarge => "IMAGE_TOO_LARGE",
            Self::ImageTooManyPixels => "IMAGE_TOO_MANY_PIXELS",
            Self::ImageDimensionTooLarge => "IMAGE_DIMENSION_TOO_LARGE",
        }
    }
}

impl std::fmt::Display for ImageAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "image admission rejected: {}", self.reason())
    }
}

impl std::error::Error for ImageAdmissionError {}

/// user/message 内容里的 image 块(形状宽容:非块数组返回空)
pub fn image_blocks(content: &Value) -> Vec<Value> {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"].as_str() == Some("image"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// 内容里的文本部分(纯字符串直通;块数组拼 text 块)
pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"].as_str() == Some("text"))
            .map(|b| b["text"].as_str().unwrap_or_default())
            .collect::<String>(),
        _ => String::new(),
    }
}

/// 组装 user/message 内容:无图 = 纯字符串(既有事实面);
/// 有图 = 块数组,图前文后,空文本省略 text 块(源序)
pub fn message_content(text: &str, images: &[ImageAttachmentRef]) -> Value {
    if images.is_empty() {
        return Value::String(text.to_string());
    }
    let mut blocks: Vec<Value> = images.iter().map(|r| r.to_block()).collect();
    if !text.is_empty() {
        blocks.push(serde_json::json!({ "type": "text", "text": text }));
    }
    Value::Array(blocks)
}

/// `agent/inbox/spliced` inserted 条目:`{id, content}`,有图附 `images`
/// (refs 数组)、有来源染色附 `source`(重放方与桌面队列帧共用形状)
pub fn splice_item(
    id: String,
    text: String,
    images: &[ImageAttachmentRef],
    source: Option<&serde_json::Value>,
) -> Value {
    let mut item = serde_json::json!({ "id": id, "content": text });
    if !images.is_empty() {
        item["images"] = Value::Array(
            images
                .iter()
                .filter_map(|r| serde_json::to_value(r).ok())
                .collect(),
        );
    }
    if let Some(source) = source {
        item["source"] = source.clone();
    }
    item
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r#ref() -> ImageAttachmentRef {
        ImageAttachmentRef {
            attachment_id: "sha256:aa".into(),
            media_type: ImageMediaType::Png,
            bytes: 4,
            width: 2,
            height: 2,
            name: Some("shot.png".into()),
        }
    }

    #[test]
    fn media_type_roundtrip() {
        for t in [
            ImageMediaType::Png,
            ImageMediaType::Jpeg,
            ImageMediaType::Webp,
            ImageMediaType::Gif,
        ] {
            assert_eq!(ImageMediaType::parse(t.as_str()), Some(t));
        }
        assert_eq!(ImageMediaType::parse("image/bmp"), None);
        assert_eq!(
            ImageMediaType::parse("image/jpg"),
            Some(ImageMediaType::Jpeg)
        );
    }

    #[test]
    fn block_roundtrip_keeps_camel_case() {
        let block = r#ref().to_block();
        assert_eq!(block["type"], "image");
        assert_eq!(block["attachment"]["attachmentId"], "sha256:aa");
        assert_eq!(block["attachment"]["mediaType"], "image/png");
        let back = ImageAttachmentRef::from_block(&block).unwrap();
        assert_eq!(back, r#ref());
    }

    #[test]
    fn message_content_text_only_stays_string() {
        assert_eq!(message_content("hi", &[]), Value::String("hi".into()));
    }

    #[test]
    fn message_content_images_first_text_last() {
        let c = message_content("hi", &[r#ref()]);
        let arr = c.as_array().unwrap();
        assert_eq!(arr[0]["type"], "image");
        assert_eq!(arr[1]["type"], "text");
        assert_eq!(arr.len(), 2);
        // 纯图片:无 text 块
        let c = message_content("", &[r#ref()]);
        assert_eq!(c.as_array().unwrap().len(), 1);
    }

    #[test]
    fn content_helpers_read_both_shapes() {
        assert_eq!(content_text(&Value::String("a".into())), "a");
        let blocks = serde_json::json!([{ "type": "image", "attachment": {} }, { "type": "text", "text": "b" }]);
        assert_eq!(content_text(&blocks), "b");
        assert_eq!(image_blocks(&blocks).len(), 1);
        assert_eq!(
            image_blocks(&Value::String("a".into())),
            Vec::<Value>::new()
        );
    }
}
