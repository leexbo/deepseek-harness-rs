//! markdown 渲染(布局安全版):pulldown-cmark 解析 → 块级 div 流,
//! 文本一律用**字符串 child**(div 内置 wrap;measured StyledText 在
//! flex/百分比宽链上按 MaxContent 单行测量 → 盒高单行、内容溢出
//! 重叠,已实测弃用;gpui-component TextView 批量创建存在异步丢
//! 文本,同样弃用)。
//!
//! 当前支持:标题/段落/代码块/有序无序列表/引用/分隔线/表格
//! (降级文本行)。行内粗斜体/链接/行内代码降级为纯文本——
//! 混排需等上游测量修复后再启。
//!
//! Mermaid:闭合的 ```` ```mermaid ```` 围栏(白名单图型)经
//! `kits/mermaid` 渲染为图;实现上 `Block::Code` 携带 `closed` 标志
//! (pulldown-cmark 对未闭合围栏也在 EOF 产出完整事件,闭合性按块源
//! 跨距判定,见 `fence_closed`),流式期未闭合显源码、闭合瞬间转图。
//!
//! parse 结果按节点 key 缓存(哈希守护):流式期间每 vsync 重绘,
//! 仅**文本变化的 chunk 帧**重解析;短文本(<512B,解析本就廉价)
//! 不入缓存以约束驻留内存。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use super::cache::MemoCache;

use gpui_kit::{InteractiveElement, StatefulInteractiveElement};
use gpui_kit::{IntoElement, ParentElement, Styled, div, px};

/// 渲染 markdown 文本(节点级唯一前缀作元素 id;同时作 parse 缓存键)。
/// mermaid 图不可点击(轨迹/计划页等无查看器场景)。
pub fn render(prefix: &str, text: &str) -> impl IntoElement {
    render_clickable(prefix, text, None)
}

/// 带 mermaid 卡片集上下文的渲染(聊天消息流用):cards = 动作钩子 +
/// 每卡片 key 状态快照,见 [`crate::kits::mermaid::MermaidCards`]。
/// `None` = 无控件(轨迹/计划页等)。
pub fn render_clickable(
    prefix: &str,
    text: &str,
    cards: Option<crate::kits::mermaid::MermaidCards>,
) -> impl IntoElement {
    let blocks = parse_cached(prefix, text);
    let mut children: Vec<gpui_kit::AnyElement> = Vec::with_capacity(blocks.len());
    for (ix, block) in blocks.iter().enumerate() {
        children.push(render_block(prefix, ix, block, cards.clone()).into_any_element());
    }
    div().children(children)
}

/// 流式 + 可点击(见 [`render_clickable`])。
pub fn render_streaming_clickable(
    prefix: &str,
    text: &str,
    cards: Option<crate::kits::mermaid::MermaidCards>,
) -> impl IntoElement {
    let blocks = parse_cached(prefix, text);
    let mut children: Vec<gpui_kit::AnyElement> = Vec::with_capacity(blocks.len());
    let last = blocks.len().saturating_sub(1);
    for (ix, block) in blocks.iter().enumerate() {
        if ix == last {
            let with_cursor = cursor_block(block);
            children.push(render_block(prefix, ix, &with_cursor, cards.clone()).into_any_element());
        } else {
            children.push(render_block(prefix, ix, block, cards.clone()).into_any_element());
        }
    }
    div().children(children)
}

/// 末块追加光标(仅文本承载块;克隆单块,不动缓存条目)
fn cursor_block(block: &Block) -> Block {
    let mut b = block.clone();
    match &mut b {
        Block::Heading(_, t) | Block::Paragraph(t) | Block::Item(_, t) | Block::Quote(t) => {
            t.push('▍')
        }
        // 闭合 mermaid 已渲染为图,▍ 无处安放 → 跳过;未闭合(源码态)
        // 与降级代码块维持原光标规则
        Block::Code(c, lang, closed) => {
            if !is_mermaid_block(c, lang.as_deref(), *closed) {
                c.push('▍');
            }
        }
        Block::Rule => {}
    }
    b
}

/// parse 缓存条目上限(消息数级;超限整体清空,简单防涨)
const CACHE_CAP: usize = 256;
/// 入缓存的最小文本长度(短文本解析本就廉价,不入驻留内存)
const CACHE_MIN_BYTES: usize = 512;

/// 域内自持解析缓存(见 kits::cache;key 撞车互不可见)
static CACHE: MemoCache<Vec<Block>> = MemoCache::new(CACHE_CAP, CACHE_MIN_BYTES);

/// 带缓存的解析:同 key 同文本(哈希)命中即复用。
fn parse_cached(key: &str, text: &str) -> Arc<Vec<Block>> {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    let hash = h.finish();
    if let Some(blocks) = CACHE.get(key, hash) {
        return blocks;
    }
    let blocks = Arc::new(parse(text));
    CACHE.put(key, hash, blocks.clone(), text.len());
    blocks
}

/// 块级模型
#[derive(Debug, Clone, PartialEq)]
enum Block {
    Heading(u8, String),
    Paragraph(String),
    /// 代码块(正文,围栏语言首词;无语言/缩进块为 None;`closed` =
    /// 围栏闭合态——pulldown-cmark 对未闭合围栏也在 EOF 产出完整
    /// CodeBlock 事件,闭合性须按块源文本判定,见 [`fence_closed`])
    Code(String, Option<String>, bool),
    Item(Option<u64>, String),
    Quote(String),
    Rule,
}

/// mermaid 渲染条件(分发与光标规则共用):闭合 + 语言名 mermaid
/// (大小写不敏感)+ 白名单图表类型。
fn is_mermaid_block(code: &str, lang: Option<&str>, closed: bool) -> bool {
    closed
        && lang.is_some_and(|l| l.eq_ignore_ascii_case("mermaid"))
        && crate::kits::mermaid::is_supported_diagram_type(code)
}

/// 块源码末非空行是否为合法闭围栏(CommonMark:同符号、缩进 ≤3、
/// 符号数 ≥ 开栏、仅尾随空白)。[`Parser`] 事件流对未闭合围栏与闭合
/// 围栏不可区分(EOF 均为完整 CodeBlock),故按块源跨距直接判定。
/// 仅对 Fenced 块有效(缩进块返回 false,调用侧由 lang=None 兜底)。
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

/// 解析(pure)
fn parse(text: &str) -> Vec<Block> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    // OffsetIter:每个事件携带源码跨距 —— CodeBlock 块源(含围栏)用
    // 于闭合性判定;跨距同时是 parse 缓存哈希的补充键源
    let mut parser = Parser::new_ext(text, options).into_offset_iter();
    let mut blocks: Vec<Block> = vec![];
    let mut buf = String::new();
    let mut pending: Option<Block> = None;
    let mut list_stack: Vec<Option<u64>> = vec![];

    fn flush(buf: &mut String, pending: &mut Option<Block>, blocks: &mut Vec<Block>) {
        if let Some(mut b) = pending.take() {
            let s = std::mem::take(buf);
            match &mut b {
                Block::Heading(_, t)
                | Block::Paragraph(t)
                | Block::Item(_, t)
                | Block::Quote(t) => *t = s,
                _ => {}
            }
            blocks.push(b);
        } else {
            buf.clear();
        }
    }

    while let Some((ev, range)) = parser.next() {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush(&mut buf, &mut pending, &mut blocks);
                    pending = Some(Block::Heading(level as u8, String::new()));
                }
                Tag::Paragraph => {
                    flush(&mut buf, &mut pending, &mut blocks);
                    pending = Some(Block::Paragraph(String::new()));
                }
                Tag::CodeBlock(kind) => {
                    flush(&mut buf, &mut pending, &mut blocks);
                    let lang = match &kind {
                        CodeBlockKind::Fenced(info) => {
                            info.split_whitespace().next().map(str::to_string)
                        }
                        CodeBlockKind::Indented => None,
                    };
                    let mut code = String::new();
                    for (ev, _) in parser.by_ref() {
                        match ev {
                            Event::Text(t) => code.push_str(&t),
                            Event::End(TagEnd::CodeBlock) => break,
                            _ => {}
                        }
                    }
                    blocks.push(Block::Code(code, lang, fence_closed(&text[range])));
                }
                Tag::List(start) => {
                    flush(&mut buf, &mut pending, &mut blocks);
                    list_stack.push(start);
                }
                Tag::Item => {
                    flush(&mut buf, &mut pending, &mut blocks);
                    let number = if let Some(slot) = list_stack.last_mut()
                        && slot.is_some()
                    {
                        let n = *slot;
                        *slot = Some(n.unwrap() + 1);
                        n
                    } else {
                        None
                    };
                    pending = Some(Block::Item(number, String::new()));
                }
                Tag::BlockQuote(_) => {
                    flush(&mut buf, &mut pending, &mut blocks);
                    pending = Some(Block::Quote(String::new()));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::Item | TagEnd::BlockQuote(_) => {
                    flush(&mut buf, &mut pending, &mut blocks)
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                }
                _ => {}
            },
            Event::Text(t) => buf.push_str(&t),
            Event::Code(t) => buf.push_str(&t),
            Event::SoftBreak | Event::HardBreak => buf.push(' '),
            Event::Rule => {
                flush(&mut buf, &mut pending, &mut blocks);
                blocks.push(Block::Rule);
            }
            _ => {}
        }
    }
    flush(&mut buf, &mut pending, &mut blocks);
    blocks
}

/// 单块(全部字符串 child:div 内置 wrap)
fn render_block(
    prefix: &str,
    ix: usize,
    block: &Block,
    cards: Option<crate::kits::mermaid::MermaidCards>,
) -> impl IntoElement {
    let id = |tag: &str| gpui_kit::SharedString::from(format!("{prefix}-md-{tag}-{ix}"));
    match block {
        Block::Heading(level, text) => {
            let size = match level {
                1 => 16.,
                2 => 15.,
                _ => 14.,
            };
            div()
                .id(id("h"))
                .mb(px(8.))
                .text_size(px(size))
                .font_weight(gpui_kit::FontWeight::BOLD)
                .text_color(crate::kits::theme::LABEL())
                .child(text.clone())
                .into_any_element()
        }
        Block::Paragraph(text) => div()
            .id(id("p"))
            .mb(px(8.))
            .text_size(px(14.))
            .text_color(crate::kits::theme::LABEL())
            .line_height(gpui_kit::relative(1.75))
            .child(text.clone())
            .into_any_element(),
        Block::Code(code, lang, closed) => {
            // mermaid 优先:syntect 无此语法,直接交给图渲染管线
            // (白名单闭合型;其余仍走下方代码高亮路径)
            if is_mermaid_block(code, lang.as_deref(), *closed) {
                return crate::kits::mermaid::diagram(prefix, ix, code.as_str().into(), cards);
            }
            // CRLF 防御:lines() 会吞 \r,行重组的字节范围将漂移 → 回退纯文本
            let body: gpui_kit::AnyElement = if code.contains('\r') {
                div().child(code.clone()).into_any_element()
            } else {
                crate::kits::highlight::highlight_window(
                    &format!("{prefix}-code"),
                    lang.as_deref(),
                    &code.lines().collect::<Vec<_>>(),
                )
                .map(|hl| {
                    // 行 spans → 全文绝对字节范围(Rgba→Hsla 为 HighlightStyle 的色型)
                    let mut ranges: Vec<(std::ops::Range<usize>, gpui_kit::HighlightStyle)> =
                        Vec::new();
                    let mut off = 0usize;
                    for line_spans in hl.iter() {
                        let mut cursor = off;
                        for s in line_spans {
                            let end = cursor + s.text.len();
                            if cursor < end {
                                ranges.push((
                                    cursor..end,
                                    gpui_kit::HighlightStyle {
                                        color: Some(s.color.into()),
                                        ..Default::default()
                                    },
                                ));
                            }
                            cursor = end;
                        }
                        off = cursor + 1; // 行间换行符占一字节
                    }
                    gpui_kit::StyledText::new(code.clone())
                        .with_highlights(ranges)
                        .into_any_element()
                })
                .unwrap_or_else(|| div().child(code.clone()).into_any_element())
            };
            div()
                .id(id("code"))
                .mb(px(8.))
                .max_h(px(400.))
                .overflow_y_scroll()
                .rounded(px(12.))
                .bg(crate::kits::theme::CODE())
                .p(px(10.))
                .font_family("Menlo")
                .text_size(px(13.))
                .text_color(crate::kits::theme::LABEL_2())
                .line_height(gpui_kit::relative(1.5))
                .child(body)
                .into_any_element()
        }
        Block::Item(number, text) => {
            let marker = match number {
                Some(n) => format!("{n}. "),
                None => "• ".into(),
            };
            div()
                .id(id("li"))
                .mb(px(4.))
                .flex()
                .text_size(px(14.))
                .text_color(crate::kits::theme::LABEL())
                .line_height(gpui_kit::relative(1.75))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(crate::kits::theme::LABEL_3())
                        .child(marker),
                )
                .child(div().min_w(px(0.)).flex_1().child(text.clone()))
                .into_any_element()
        }
        Block::Quote(text) => div()
            .id(id("q"))
            .mb(px(8.))
            .border_l_2()
            .border_color(crate::kits::theme::BORDER())
            .pl(px(10.))
            .text_size(px(14.))
            .text_color(crate::kits::theme::LABEL_3())
            .line_height(gpui_kit::relative(1.75))
            .child(text.clone())
            .into_any_element(),
        Block::Rule => div()
            .id(id("rule"))
            .mb(px(8.))
            .h(px(1.))
            .w_full()
            .bg(crate::kits::theme::BORDER())
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cache_hits_on_same_text_and_misses_on_change() {
        // 长文本(≥512B)入缓存:同 key 同文本 → 命中(Arc 指针相等)
        let long = "长内容段落。".repeat(128);
        let a = parse_cached("k1", &long);
        let b = parse_cached("k1", &long);
        assert!(std::sync::Arc::ptr_eq(&a, &b));
        // 文本变化 → 重解析(指针不同,内容更新)
        let long2 = format!("{long}追加");
        let c = parse_cached("k1", &long2);
        assert!(!std::sync::Arc::ptr_eq(&a, &c));
        assert_eq!(&*c, &parse(&long2));
        // 不同 key 互不影响
        let d = parse_cached("k2", &long);
        assert!(!std::sync::Arc::ptr_eq(&a, &d));
        // 短文本(<512B)不入缓存(解析廉价,不驻留内存)
        let s1 = parse_cached("k3", "短");
        let s2 = parse_cached("k3", "短");
        assert!(!std::sync::Arc::ptr_eq(&s1, &s2));
    }

    #[test]
    fn parses_common_subset() {
        let blocks = parse("# 标\n\n段 **粗** `c`。\n\n- 甲\n- 乙\n");
        assert_eq!(
            blocks,
            vec![
                Block::Heading(1, "标".into()),
                Block::Paragraph("段 粗 c。".into()),
                Block::Item(None, "甲".into()),
                Block::Item(None, "乙".into()),
            ]
        );
    }

    #[test]
    fn ordered_list_increments() {
        let blocks = parse("1. a\n2. b\n");
        assert_eq!(
            blocks,
            vec![
                Block::Item(Some(1), "a".into()),
                Block::Item(Some(2), "b".into()),
            ]
        );
    }

    #[test]
    fn code_block_and_rule() {
        let blocks = parse("t\n\n```\nx\n```\n\n---\n");
        assert_eq!(
            blocks,
            vec![
                Block::Paragraph("t".into()),
                Block::Code("x\n".into(), None, true),
                Block::Rule,
            ]
        );
        // 围栏语言首词捕获(infostring 多词取首词)
        let blocks = parse("```rust ignore\nfn a(){}\n```\n");
        assert_eq!(
            blocks,
            vec![Block::Code("fn a(){}\n".into(), Some("rust".into()), true)]
        );
    }

    /// 未闭合围栏的闭合性判定:EOF 也产出完整 CodeBlock,但 closed 为 false
    #[test]
    fn unclosed_fence_is_closed_false() {
        let blocks = parse("```mermaid\ngraph TD\n");
        assert_eq!(
            blocks,
            vec![Block::Code(
                "graph TD\n".into(),
                Some("mermaid".into()),
                false
            )]
        );
    }

    /// 闭合围栏判 true;波浪线围栏同判;缩进块非 Fenced(closed=false)
    #[test]
    fn closed_fence_is_closed_true() {
        let blocks = parse("```mermaid\ngraph TD\n    A-->B\n```\n");
        assert_eq!(
            blocks,
            vec![Block::Code(
                "graph TD\n    A-->B\n".into(),
                Some("mermaid".into()),
                true
            )]
        );
        let blocks = parse("~~~mermaid\ngraph TD\n~~~\n");
        assert_eq!(
            blocks[0],
            Block::Code("graph TD\n".into(), Some("mermaid".into()), true)
        );
        let blocks = parse("    let x = 1;\n");
        assert_eq!(
            blocks[0],
            Block::Code("let x = 1;\n".into(), None, false),
            "缩进块非围栏(closed=false)"
        );
    }

    /// 光标规则:闭合 mermaid(白名单型)不加 ▍;未闭合源码态与
    /// 降级代码块维持原样
    #[test]
    fn cursor_skips_closed_mermaid() {
        let closed = Block::Code("graph TD\n".into(), Some("mermaid".into()), true);
        assert_eq!(
            cursor_block(&closed),
            Block::Code("graph TD\n".into(), Some("mermaid".into()), true),
            "闭合 mermaid 不加 ▍"
        );
        let unclosed = Block::Code("graph TD\n".into(), Some("mermaid".into()), false);
        assert_eq!(
            cursor_block(&unclosed),
            Block::Code("graph TD\n▍".into(), Some("mermaid".into()), false),
            "未闭合仍按源码态加 ▍"
        );
        // 非白名单 mermaid 型(如 sankey-beta)闭合后仍按代码块处理
        let beta = Block::Code(
            "sankey-beta\n    A-->B\n".into(),
            Some("mermaid".into()),
            true,
        );
        assert_eq!(
            cursor_block(&beta),
            Block::Code(
                "sankey-beta\n    A-->B\n▍".into(),
                Some("mermaid".into()),
                true
            )
        );
    }
}
