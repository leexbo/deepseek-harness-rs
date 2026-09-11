//! 图片附件的请求期处理。
//!
//! 内部消息方言携带 `{type:"image", attachment:<ref>}` 块(持久引用,
//! 零字节);出网前经两步纯变换:①请求级 offload(超预算的最旧图替换
//! 为占位文本——瞬态,不改持久消息);②adapter 方言翻译时经
//! [`AttachmentSource`] 读字节组 data URL(serialize.ts 语义)。

use serde_json::Value;

/// 请求级图片字节预算(base64 估算口径;DEFAULT_MAX_REQUEST_IMAGE_BYTES)
pub const MAX_REQUEST_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// offload 占位文本(源 OFFLOADED_IMAGE_TEXT 逐字;模型可据语义重读文件
/// 或请用户重附)
pub const OFFLOADED_IMAGE_TEXT: &str = "[image omitted to keep the request within its image limit; older images are omitted first. If this image is still needed, read its file again when a path is available; otherwise ask the user to attach it again.]";

/// 请求期图片字节来源(宿主注入;实现 = dsh-host AttachmentStore)
pub trait AttachmentSource: Send + Sync {
    /// 按附件 id 读编码字节;缺席 = None(翻译侧降级占位)
    fn image_bytes(&self, id: &str) -> Option<Vec<u8>>;
}

/// 空来源(默认装配;图片一律降级占位)
pub struct NoAttachments;

impl AttachmentSource for NoAttachments {
    fn image_bytes(&self, _id: &str) -> Option<Vec<u8>> {
        None
    }
}

/// base64 数据 URL(`data:<mediaType>;base64,<…>`;源 imagePart 形状)
pub fn image_data_url(block: &Value, source: &dyn AttachmentSource) -> Option<String> {
    let a = &block["attachment"];
    let id = a["attachmentId"].as_str()?;
    let media_type = a["mediaType"].as_str()?;
    let bytes = source.image_bytes(id)?;
    use base64::Engine as _;
    Some(format!(
        "data:{media_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// 请求级 offload:累计图片的 base64 估算体积(bytes × 4/3)超预算时,
/// **从最旧的图开始**替换为占位文本块(源 offloadRequestImages)。
/// 瞬态变换——调用方在克隆上执行,不回写持久消息。
/// 覆盖面:user/assistant 的 content 块数组 + tool 消息的 `images`
/// 引用数组(MCP 图片桥;哨兵 = 同款占位文本块,翻译侧直通)。
pub fn offload_request_images(messages: &mut Value, max_request_image_bytes: u64) {
    let Some(items) = messages.as_array_mut() else {
        return;
    };
    // 先累计全部图块的预算占用,再从最旧开始替换直到回到限内
    let mut total: u64 = 0;
    for m in items.iter() {
        for block in content_blocks(&m["content"]) {
            if is_image_block(block) {
                total += base64_size(block_bytes(block));
            }
        }
        for block in content_blocks(&m["images"]) {
            if is_image_block(block) {
                total += base64_size(block_bytes(block));
            }
        }
    }
    if total <= max_request_image_bytes {
        return;
    }
    'outer: for m in items.iter_mut() {
        for slot_key in ["content", "images"] {
            let Some(slot) = m.get_mut(slot_key).and_then(Value::as_array_mut) else {
                continue;
            };
            for block in slot.iter_mut() {
                if total <= max_request_image_bytes {
                    break 'outer;
                }
                if is_image_block(block) {
                    total = total.saturating_sub(base64_size(block_bytes(block)));
                    *block = serde_json::json!({ "type": "text", "text": OFFLOADED_IMAGE_TEXT });
                }
            }
        }
    }
}

/// 折叠摘要降级:全部图块 → 占位文本(源 compaction text-only 语义;
/// 摘要请求不该背着 base64 负载)。含 tool 消息的 `images` 引用数组。
pub fn strip_images_for_summary(messages: &Value) -> Value {
    let mut out = messages.clone();
    let Some(items) = out.as_array_mut() else {
        return out;
    };
    for m in items.iter_mut() {
        for slot_key in ["content", "images"] {
            let Some(slot) = m.get_mut(slot_key).and_then(Value::as_array_mut) else {
                continue;
            };
            for block in slot.iter_mut() {
                if is_image_block(block) {
                    *block = serde_json::json!({ "type": "text", "text": OFFLOADED_IMAGE_TEXT });
                }
            }
        }
    }
    out
}

fn is_image_block(block: &Value) -> bool {
    block["type"].as_str() == Some("image")
}

fn block_bytes(block: &Value) -> u64 {
    block["attachment"]["bytes"].as_u64().unwrap_or(0)
}

/// base64 体积估算(源口径:ceil(bytes / 3) × 4)
fn base64_size(bytes: u64) -> u64 {
    bytes.div_ceil(3) * 4
}

fn content_blocks(content: &Value) -> impl Iterator<Item = &Value> {
    content.as_array().into_iter().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixed(&'static [u8]);
    impl AttachmentSource for Fixed {
        fn image_bytes(&self, _id: &str) -> Option<Vec<u8>> {
            Some(self.0.to_vec())
        }
    }

    fn image_block(bytes: u64) -> Value {
        json!({
            "type": "image",
            "attachment": { "attachmentId": "sha256:x", "mediaType": "image/png", "bytes": bytes }
        })
    }

    #[test]
    fn data_url_carries_media_type_and_base64() {
        let url = image_data_url(&image_block(3), &Fixed(&[1, 2, 3])).unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        assert!(url.ends_with("AQID"));
    }

    #[test]
    fn unresolved_image_yields_none() {
        assert!(image_data_url(&image_block(3), &NoAttachments).is_none());
    }

    #[test]
    fn offload_replaces_oldest_first_until_within_budget() {
        // base64 估算:30B → 40,10B → 16,合计 56;预算 50 → 最旧一张(40)下车后
        // 余 16 ≤ 50,更晚的图保留
        let mut messages = json!([
            { "role": "user", "content": [image_block(30), { "type": "text", "text": "hi" }] },
            { "role": "assistant", "content": "ok" },
            { "role": "user", "content": [image_block(10)] },
        ]);
        offload_request_images(&mut messages, 50);
        let first = messages[0]["content"].as_array().unwrap();
        assert_eq!(first[0]["type"], "text");
        assert_eq!(first[0]["text"], OFFLOADED_IMAGE_TEXT);
        assert_eq!(first[1]["text"], "hi");
        // 40 下车后余 16 ≤ 50:更晚的图保留
        assert_eq!(messages[2]["content"][0]["type"], "image");
    }

    #[test]
    fn offload_under_budget_is_noop() {
        let mut messages = json!([{ "role": "user", "content": [image_block(10)] }]);
        offload_request_images(&mut messages, 100);
        assert_eq!(messages[0]["content"][0]["type"], "image");
    }

    #[test]
    fn strip_for_summary_replaces_every_image() {
        let messages = json!([{ "role": "user", "content": [image_block(1), { "type": "text", "text": "q" }] }]);
        let stripped = strip_images_for_summary(&messages);
        let blocks = stripped[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["text"], OFFLOADED_IMAGE_TEXT);
        assert_eq!(blocks[1]["text"], "q");
    }
}
