//! 附件的请求期处理。
//!
//! 内部消息方言携带 `{type:"image"/"file", attachment:<ref>}` 块(持久
//! 引用,零字节);出网前经纯变换:①文件投影——文件从不原生上送,
//! 每个 file 块替换为路径句柄文本;
//! ②请求级图片 offload(超预算的最旧图替换为占位文本)——均为瞬态,
//! 不改持久消息;③adapter 方言翻译时经 [`AttachmentSource`] 读字节组
//! data URL / 解析文件路径。

use serde_json::Value;

/// 请求级图片字节预算(base64 估算口径;DEFAULT_MAX_REQUEST_IMAGE_BYTES)
pub const MAX_REQUEST_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// offload 占位文本(模型可据语义重读文件
/// 或请用户重附)
pub const OFFLOADED_IMAGE_TEXT: &str = "[image omitted to keep the request within its image limit; older images are omitted first. If this image is still needed, read its file again when a path is available; otherwise ask the user to attach it again.]";

/// 请求期图片字节来源(宿主注入;实现 = liuma-attachment `AttachmentStore`。
/// trait 随附件词汇收拢于 liuma-attachment,此处窄重导出保持既有路径)
pub use liuma_attachment::AttachmentSource;

/// 空来源(默认装配;图片一律降级占位,文件走无路径句柄)
pub struct NoAttachments;

impl AttachmentSource for NoAttachments {
    fn image_bytes(&self, _id: &str) -> Option<Vec<u8>> {
        None
    }
}

/// JSON 字符串字面量(含引号与转义)
fn json_string(value: &str) -> String {
    // 字符串序列化不会失败(&str 恒为合法输入)
    serde_json::to_string(value).expect("serde_json 字符串序列化不会失败")
}

/// 文件句柄文本(digest = id 去 `sha256:` 前缀
/// 的前 8 hex)。路径解析失败走 no-path 分支:公开承认读不到,
/// 禁止模型谎称已读。
pub fn file_handle_text(attachment: &Value, readonly_path: Option<&str>) -> String {
    let id = attachment["attachmentId"].as_str().unwrap_or_default();
    let digest: String = id
        .strip_prefix("sha256:")
        .unwrap_or(id)
        .chars()
        .take(8)
        .collect();
    let name = attachment["name"].as_str().unwrap_or_default();
    let bytes = attachment["bytes"].as_u64().unwrap_or(0);
    let identity = format!(
        "File {} ({bytes} bytes, sha256:{digest})",
        json_string(name)
    );
    match readonly_path {
        Some(path) => format!(
            "[{identity}: verbatim read-only copy saved at {}. Read that path with your file tools when its contents are needed; copy it to a writable location before modifying it. When delegating file work, include this saved path in the delegation prompt; only subagents sharing this execution environment can read it.]",
            json_string(path)
        ),
        None => format!(
            "[{identity} was uploaded, but the current execution environment cannot access a readable path. Report that limitation if its contents are needed; do not claim to have read it.]"
        ),
    }
}

/// 请求级文件投影:**文件从不原生上送**,
/// 每条路由收到的都是句柄文本——content 数组中每个 file 块原位替换为
/// text 块。瞬态变换(调用方在克隆上执行,不回写持久消息)。
pub fn project_files_to_text(messages: &mut Value, source: &dyn AttachmentSource) {
    let Some(items) = messages.as_array_mut() else {
        return;
    };
    for m in items.iter_mut() {
        let Some(slot) = m.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in slot.iter_mut() {
            if block["type"].as_str() != Some("file") {
                continue;
            }
            let attachment = block["attachment"].clone();
            let path = source.file_path(
                attachment["attachmentId"].as_str().unwrap_or_default(),
                attachment["name"].as_str().unwrap_or_default(),
            );
            *block = serde_json::json!({
                "type": "text",
                "text": file_handle_text(&attachment, path.as_deref()),
            });
        }
    }
}

/// base64 数据 URL(`data:<mediaType>;base64,<…>`)
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
/// **从最旧的图开始**替换为占位文本块。
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

/// 折叠摘要降级:全部图块 → 占位文本(纯文本面;
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

/// base64 体积估算(ceil(bytes / 3) × 4)
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

    fn file_block() -> Value {
        json!({ "type": "file", "attachment": {
            "attachmentId": "sha256:abcdef1234567890",
            "name": "功能清单.md",
            "bytes": 18_432,
        }})
    }

    /// 回归锁:file 块从不原生上送——投影后原位替换为句柄 text 块,
    /// 路径可解析走 saved-at 分支(逐字 bit-exact),图片块不受影响
    #[test]
    fn file_projection_replaces_blocks_with_handle_text() {
        struct PathSource;
        impl AttachmentSource for PathSource {
            fn image_bytes(&self, _id: &str) -> Option<Vec<u8>> {
                None
            }
            fn file_path(&self, _id: &str, _name: &str) -> Option<String> {
                Some("/attachments/files/ab/abcd/功能清单.md".into())
            }
        }
        let mut messages = json!([
            { "role": "user", "content": [image_block(10), file_block(), { "type": "text", "text": "总结一下" }] },
        ]);
        project_files_to_text(&mut messages, &PathSource);
        let blocks = messages[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "image", "图片块不参与文件投影");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(
            blocks[1]["text"],
            "[File \"功能清单.md\" (18432 bytes, sha256:abcdef12): verbatim read-only copy saved at \"/attachments/files/ab/abcd/功能清单.md\". Read that path with your file tools when its contents are needed; copy it to a writable location before modifying it. When delegating file work, include this saved path in the delegation prompt; only subagents sharing this execution environment can read it.]"
        );
        assert_eq!(blocks[2]["text"], "总结一下");
    }

    /// 回归锁:路径解析失败走 no-path 分支(公开承认读不到,
    /// 禁止谎称已读)
    #[test]
    fn file_projection_without_path_degrades_to_unavailable_handle() {
        let mut messages = json!([
            { "role": "user", "content": [file_block()] },
        ]);
        project_files_to_text(&mut messages, &NoAttachments);
        let blocks = messages[0]["content"].as_array().unwrap();
        assert_eq!(
            blocks[0]["text"],
            "[File \"功能清单.md\" (18432 bytes, sha256:abcdef12) was uploaded, but the current execution environment cannot access a readable path. Report that limitation if its contents are needed; do not claim to have read it.]"
        );
    }
}
