//! Mermaid 的 `MarkdownPlugin` 适配(gpui-kit 富文本管线的官方块级
//! 插件挂点)。
//!
//! parse 在**后台线程**:围栏闭合性(未闭合 = 流式中途,落回内置
//! 代码块显源码)+ 图型白名单;render 在 **UI 帧**:读 store 组装
//! 卡片集并复用 [`crate::kits::mermaid::diagram`]。卡片状态键 =
//! **源码 hash**(插件是全局渲染器,拿不到消息 prefix/块序,hash 键
//! 同时摆脱旧 `{prefix}-md-mermaid-{ix}` 隐式契约)。
//!
//! 装配:仅聊天正文视图挂本插件(渲染期 `TextView::plugin`,每帧
//! 构造经 `has_same_parser_configuration` 判定不触发重解析)。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::text::markdown_ast as mdast;
use gpui_kit::component::text::{MarkdownNode, MarkdownParseContext, MarkdownPlugin};
use gpui_kit::{App, Entity, IntoElement, Window};

use crate::kits::mermaid::{self, MermaidCards};
use crate::shell::store::AppStore;

/// mermaid 自定义节点数据(parse 产、render 消费)
pub(crate) struct MermaidData {
    /// 图源码
    pub source: Arc<str>,
    /// 卡片状态键(源码 hash hex;`mermaid_cards` 映射键 + 元素 id 基)
    pub card_key: String,
}

/// 卡片状态键(源码内容 hash,确定性;同图同键跨消息复用)
pub(crate) fn mermaid_card_key(source: &str) -> String {
    let mut h = DefaultHasher::new();
    source.hash(&mut h);
    format!("mmd-{:016x}", h.finish())
}

/// 聊天正文 TextView 的 mermaid 插件(store 注入:卡片状态快照与
/// 动作回调都落在 store 域)
pub(crate) struct MermaidTextViewPlugin {
    /// 应用状态(卡片状态/动作钩子的落点)
    pub store: Entity<AppStore>,
}

impl MarkdownPlugin for MermaidTextViewPlugin {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "mermaid"
    }

    fn parse(&self, node: &mdast::Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let mdast::Node::Code(code) = node else {
            return None;
        };
        if !code
            .lang
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case("mermaid"))
        {
            return None;
        }
        // 围栏闭合性:node_source 含围栏行的源段,末非空行须为合法
        // 闭栏(未闭合 = 流式中途,落回代码块显源码,闭合瞬间转图)
        let raw = cx.node_source(node)?;
        if !fence_closed(raw) {
            return None;
        }
        let source: Arc<str> = Arc::from(code.value.as_str());
        if !mermaid::is_supported_diagram_type(&source) {
            return None;
        }
        Some(MarkdownNode::new(
            "mermaid",
            MermaidData {
                card_key: mermaid_card_key(&source),
                source,
            },
        ))
    }

    fn render(&self, node: &MarkdownNode, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let data = node.data::<MermaidData>().expect("mermaid 节点数据应在");
        let cards = mermaid_cards_for(&self.store, cx);
        mermaid::diagram(&data.card_key, 0, data.source.clone(), Some(cards))
    }
}

/// 组装卡片集:per-card 状态快照(store 持久)+ 动作回调(捕获
/// store entity;语义照旧 build_mermaid_cards,键改源码 hash)
fn mermaid_cards_for(store: &Entity<AppStore>, cx: &App) -> MermaidCards {
    let s = store.clone();
    let states = s
        .read(cx)
        .chat
        .mermaid_cards
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                crate::kits::mermaid::MermaidCardState {
                    show_code: v.show_code,
                    copied: v.copied,
                },
            )
        })
        .collect();
    let callbacks = crate::kits::mermaid::MermaidCardCallbacks {
        toggle_code: {
            let store = s.clone();
            Arc::new(move |card_key, _w, cx| {
                store.update(cx, |st, cx| st.toggle_mermaid_code(card_key, cx));
            })
        },
        copy: {
            // 复制反馈在按钮本体(store 侧绿色「已复制」态切换),
            // 不用异步通知——反馈须锚定在动作发生处
            let store = s.clone();
            Arc::new(move |card_key, source, _w, cx| {
                store.update(cx, |st, cx| st.copy_mermaid_source(card_key, &source, cx));
            })
        },
        enlarge: {
            let store = s.clone();
            Arc::new(move |card_key, source, _w, cx| {
                // 开图占位 = 卡片在档光栅(缓存命中零渲染;卡片固定
                // 1.0 档,与内嵌预览共用缓存)——占位的渲染档上下文:
                // 全图、pan 0。查看器首档走后台渲染,就位前地图式过渡
                // 显示占位,主线程不同步 usvg 解析
                let placeholder = crate::kits::mermaid::raster_at_zoom(card_key, &source, cx, 1.0)
                    .ok()
                    .map(|img| (img, 1.0));
                store.update(cx, |st, cx| {
                    st.open_mermaid_enlarged(source, placeholder, cx)
                });
            })
        },
        download: {
            // 下载完成提示用通知(自动消失),而非插入消息流;导出为
            // 同步纯函数(固定 1.0 自然档);通知需 window,回调收之
            Arc::new(|_card_key, source, window, cx| {
                let msg = match crate::kits::mermaid::export_diagram_png(&source, 1.0, cx) {
                    Ok(p) => (
                        "已导出:".to_string() + &p.display().to_string(),
                        gpui_kit::component::notification::NotificationType::Success,
                    ),
                    Err(e) => (
                        format!("导出失败:{e}"),
                        gpui_kit::component::notification::NotificationType::Error,
                    ),
                };
                window.push_notification(
                    gpui_kit::component::notification::Notification::new()
                        .id::<MermaidDownloadNotice>()
                        .message(msg.0)
                        .with_type(msg.1),
                    cx,
                );
            })
        },
    };
    MermaidCards { states, callbacks }
}

/// 下载通知的稳定类型 id:同类型通知互相替换(连续导出不无限堆叠)
struct MermaidDownloadNotice;

/// 围栏闭合判定(语义照 kits/markdown.rs 的 fence_closed:末非空行
/// 须为合法闭栏——同符号、缩进 ≤3、长度 ≥ 开栏、仅尾随空白)
fn fence_closed(block_source: &str) -> bool {
    let mut last_nonblank = None;
    for line in block_source.lines() {
        if !line.trim().is_empty() {
            last_nonblank = Some(line);
        }
    }
    let Some(line) = last_nonblank else {
        return false;
    };
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    if indent > 3 || trimmed.len() < 2 {
        return false;
    }
    let Some(&ch) = trimmed.as_bytes().first() else {
        return false;
    };
    if !matches!(ch, b'`' | b'~') {
        return false;
    }
    let fence_len = trimmed.bytes().take_while(|&b| b == ch).count();
    if !trimmed[fence_len..]
        .chars()
        .all(|c| c.is_ascii_whitespace())
    {
        return false;
    }
    // 开栏长度取块首行(块源跨距从开栏起);闭栏须 ≥ 开栏
    let open_len = block_source
        .lines()
        .next()
        .map(|l| l.trim_start().bytes().take_while(|&b| b == ch).count())
        .unwrap_or(0);
    fence_len >= open_len
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 闭合性判定语义锁(流式中途显源码、闭合转图的门)
    #[test]
    fn fence_closed_matches_handrolled_semantics() {
        assert!(fence_closed("```mermaid\ngraph TD\nA-->B\n```"));
        assert!(!fence_closed("```mermaid\ngraph TD\nA-->B"));
        // 闭栏短于开栏 = 未闭合
        assert!(!fence_closed("````mermaid\ngraph TD\n```"));
        // 尾随空白允许;缩进 ≤3 允许
        assert!(fence_closed("```mermaid\ngraph TD\n```   "));
        assert!(fence_closed("```mermaid\nflowchart TD\n   ```"));
    }

    /// 卡片键 = 源码内容 hash(同图同键、异图异键)
    #[test]
    fn card_key_is_content_hash() {
        assert_eq!(
            mermaid_card_key("graph TD\nA-->B"),
            mermaid_card_key("graph TD\nA-->B")
        );
        assert_ne!(
            mermaid_card_key("graph TD\nA-->B"),
            mermaid_card_key("graph TD\nB-->A")
        );
    }
}
