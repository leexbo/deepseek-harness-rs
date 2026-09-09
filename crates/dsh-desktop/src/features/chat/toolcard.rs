//! 工具卡家族:渲染意图(ToolView wire 形态)的桌面窄化 + read/
//! search/diff 三张卡。
//!
//! 窄化是线界健壮性:视图跨事件线而来,非法/未知 `card` → None →
//! 通用 IN/OUT 卡(防御姿态)。UI 只 switch
//! `card`,从不 switch 工具名。
//!
//! 家族几何:rounded 12、bg `theme::CODE()`(#1b1b1c)、
//! 聊天位 `ml(4)`、Menlo 正文 13/22、`pre` 不软换行(内层列无定宽 →
//! MaxContent 单行测宽,外层横向滚动)、复制钮 label-secondary
//! 「复制→复制成功」同色、head/tail cap 8(4 头 + 4 尾 +「… 其余 N 行」
//! /「收起」)。read/search 横幅 bg `theme::CARD()`(bluish-850 banner
//! token);diff 无横幅(浮动复制钮)。

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, px,
};
use gpui_kit::component::StyledExt;

use super::projection::relativize;
use crate::kits::theme;
use crate::shell::store::AppStore;

// ── 窄化类型(防御性逐字段校验)────────────────────────────────

/// 终端卡详情(Terminal 视图)
pub(crate) struct TerminalDetail {
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<String>,
    pub(crate) cwd: Option<String>,
}

/// read 卡素材(Read 视图)
pub(crate) struct ReadCard {
    pub(crate) path: String,
    pub(crate) lines: Vec<(u64, String)>,
    pub(crate) total_lines: u64,
    pub(crate) lang: Option<String>,
}

/// 一个文件的分组匹配
pub(crate) struct FileGroup {
    pub(crate) path: String,
    pub(crate) matches: Vec<(u64, String)>,
}

/// search 卡素材(两形态)
pub(crate) enum SearchCard {
    Matches {
        files: Vec<FileGroup>,
        truncated: bool,
        total: u64,
    },
    Paths {
        paths: Vec<String>,
        truncated: bool,
        total: u64,
    },
}

/// 一条文件变更
pub(crate) struct DiffHunk {
    pub(crate) path: String,
    pub(crate) old_text: Option<String>,
    pub(crate) new_text: String,
}

/// diff 卡素材(Diff 视图;call 意图与 result 事实同构)
pub(crate) struct DiffCard {
    pub(crate) diffs: Vec<DiffHunk>,
}

/// 窄化后的卡视图(路由用)
pub(crate) enum CardView {
    Terminal(TerminalDetail),
    Read(ReadCard),
    Search(SearchCard),
    Diff(DiffCard),
}

/// 非法/未知视图 → None(通用 IN/OUT 卡);逐字段校验,任一不符即拒
pub(crate) fn narrow(view: &serde_json::Value) -> Option<CardView> {
    let card = view["card"].as_str()?;
    match card {
        "terminal" => Some(CardView::Terminal(TerminalDetail {
            exit_code: view["exitCode"].as_i64().map(|c| c as i32),
            signal: view["signal"].as_str().map(str::to_string),
            cwd: view["cwd"].as_str().map(str::to_string),
        })),
        "read" => Some(CardView::Read(ReadCard {
            path: view["path"].as_str()?.to_string(),
            lines: view["lines"]
                .as_array()?
                .iter()
                .map(|l| Some((l["number"].as_u64()?, l["text"].as_str()?.to_string())))
                .collect::<Option<Vec<_>>>()?,
            total_lines: view["totalLines"].as_u64()?,
            lang: view["lang"].as_str().map(str::to_string),
        })),
        "search" => {
            let truncated = view["truncated"].as_bool()?;
            let total = view["total"].as_u64()?;
            match view["shape"].as_str()? {
                // RS wire:searchMatches/searchPaths 变体名即形态
                "searchMatches" => Some(CardView::Search(SearchCard::Matches {
                    files: view["files"]
                        .as_array()?
                        .iter()
                        .map(|f| {
                            Some(FileGroup {
                                path: f["path"].as_str()?.to_string(),
                                matches: f["matches"]
                                    .as_array()?
                                    .iter()
                                    .map(|m| {
                                        Some((
                                            m["number"].as_u64()?,
                                            m["text"].as_str()?.to_string(),
                                        ))
                                    })
                                    .collect::<Option<Vec<_>>>()?,
                            })
                        })
                        .collect::<Option<Vec<_>>>()?,
                    truncated,
                    total,
                })),
                "searchPaths" => Some(CardView::Search(SearchCard::Paths {
                    paths: view["paths"]
                        .as_array()?
                        .iter()
                        .map(|p| Some(p.as_str()?.to_string()))
                        .collect::<Option<Vec<_>>>()?,
                    truncated,
                    total,
                })),
                _ => None,
            }
        }
        "diff" => {
            let diffs = view["diffs"]
                .as_array()?
                .iter()
                .map(|d| {
                    Some(DiffHunk {
                        path: d["path"].as_str()?.to_string(),
                        old_text: d["oldText"].as_str().map(str::to_string),
                        new_text: d["newText"].as_str()?.to_string(),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            // 空 diffs 无卡可画(diffs.length === 0 → null)
            if diffs.is_empty() {
                return None;
            }
            Some(CardView::Diff(DiffCard { diffs }))
        }
        _ => None,
    }
}

// ── 家族公共件 ────────────────────────────────────────────────

/// 头尾切分算术(源 head-tail-cap:head = ceil(cap/2),tail = 余数)
struct HeadTail {
    hidden: usize,
    capped: bool,
    head: usize,
    tail: usize,
}

fn head_tail(total: usize, cap: usize, expanded: bool) -> HeadTail {
    let hidden = total.saturating_sub(cap);
    HeadTail {
        hidden,
        capped: hidden > 0 && !expanded,
        head: cap.div_ceil(2),
        tail: cap - cap.div_ceil(2),
    }
}

// ── read 卡 ───────────────────────────────────────────────────

/// read 卡:横幅(path + 窗口计数 + lang + 复制)+ 行号槽正文。
/// 行号槽 48px 右对齐 LABEL_3,内容 LABEL,行高 22,横向滚动
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_read(
    store: &Entity<AppStore>,
    cx: &App,
    ix: usize,
    key: &str,
    card: &ReadCard,
    ws_root: Option<&str>,
) -> gpui_kit::AnyElement {
    let expanded = store.read(cx).chat.card_expanded.contains(key);
    let windowed = (card.lines.len() as u64) < card.total_lines;
    let raw = card
        .lines
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let ht = head_tail(card.lines.len(), CHAT_CARD_MAX_LINES, expanded);
    // 语法高亮(行级有状态,块缓存;lang 缺省/未知名 → None 纯等宽)
    let hl = crate::kits::highlight::highlight_window(
        &format!("{key}·read"),
        card.lang.as_deref(),
        &card
            .lines
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>(),
    );

    let mut body_rows: Vec<gpui_kit::AnyElement> = card
        .lines
        .iter()
        .enumerate()
        .take(if ht.capped { ht.head } else { usize::MAX })
        .map(|(i, (n, text))| read_line(*n, text, hl.as_ref().map(|h| h[i].as_slice())))
        .collect();
    // 展开/收起行在 hidden>0 时**恒显示**(源 ReadBlock.hidden>0 无条件渲染;
    // 展开后文案切「收起」,不随展开消失)。展开(capped=false)时按钮独立于
    // 头尾切片之外(hidden 仍准确),capped 时插在头尾之间。
    if ht.hidden > 0 {
        body_rows.push(read_expand_row(store, cx, ix, key, ht.hidden));
    }
    if ht.capped {
        body_rows.extend(
            card.lines[card.lines.len() - ht.tail..]
                .iter()
                .enumerate()
                .map(|(i, (n, text))| {
                    let ix0 = card.lines.len() - ht.tail + i;
                    read_line(*n, text, hl.as_ref().map(|h| h[ix0].as_slice()))
                }),
        );
    }

    div()
        .id(("read-card", ix))
        .relative()
        .v_flex()
        .ml(px(4.))
        .rounded(px(12.))
        .bg(theme::CODE())
        .overflow_hidden()
        .font_family("Menlo")
        .text_size(px(13.))
        .line_height(px(22.))
        // 横幅:label(相对化 + 省略号,12/18)+ [窗口计数 · lang · 复制]
        .child(
            div()
                .flex()
                .min_w(px(0.))
                .items_center()
                .gap(px(12.))
                .px(px(14.))
                .py(px(9.))
                .bg(theme::CARD())
                .child(
                    div()
                        .min_w(px(0.))
                        .flex_1()
                        .truncate()
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .text_color(theme::LABEL())
                        .child(relativize(ws_root, &card.path)),
                )
                .when(windowed, |el| {
                    el.child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(13.))
                            .line_height(px(18.))
                            .text_color(theme::LABEL_3())
                            .child(format!(
                                "显示 {} / {} 行",
                                card.lines.len(),
                                card.total_lines
                            )),
                    )
                })
                .children(card.lang.as_deref().map(|l| {
                    div()
                        .flex_shrink_0()
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .text_color(theme::LABEL_3())
                        .child(l.to_string())
                }))
                // 空窗口无复制钮(复制会以空串覆写剪贴板)
                .when(!card.lines.is_empty(), |el| {
                    el.child(copy_button(
                        store,
                        cx,
                        ("read-copy", ix),
                        &format!("{key}·read"),
                        &raw,
                    ))
                }),
        )
        // 正文:py 12、行号槽 48px(pr 14 右对齐)+ 内容;不软换行横向滚
        .child(
            div()
                .id(("read-body", ix))
                .overflow_scroll()
                .py(px(12.))
                .child(div().v_flex().children(body_rows)),
        )
        .into_any_element()
}

/// 一行读取窗口:行号槽(固定 48px 右对齐)+ 内容行(高亮 spans
/// 横排不折行;无高亮回退单色文本)
fn read_line(
    number: u64,
    text: &str,
    spans: Option<&[crate::kits::highlight::Span]>,
) -> gpui_kit::AnyElement {
    let content = match spans {
        Some(spans) if !spans.is_empty() => {
            // 横排 span(div 默认列向会竖排;pre 不折行 → 行超宽由外层横滚)
            let mut el = div().flex();
            for s in spans {
                el = el.child(div().text_color(s.color).child(s.text.clone()));
            }
            el
        }
        _ => div().text_color(theme::LABEL()).child(text.to_string()),
    };
    div()
        .flex()
        .min_h(px(22.))
        .line_height(px(22.))
        .child(
            div()
                .flex_shrink_0()
                .w(px(48.))
                .pr(px(14.))
                .text_right()
                .text_color(theme::LABEL_3())
                .child(number.to_string()),
        )
        .child(content)
        .into_any_element()
}

/// read 展开钮(让位行号槽:pl 48)
fn read_expand_row(
    store: &Entity<AppStore>,
    cx: &App,
    ix: usize,
    key: &str,
    hidden: usize,
) -> gpui_kit::AnyElement {
    let expanded = store.read(cx).chat.card_expanded.contains(key);
    let s = store.clone();
    let k = key.to_string();
    div()
        .id(("read-expand", ix))
        .debug_selector(move || format!("read-expand-{ix}"))
        .pl(px(48.))
        .min_h(px(22.))
        .line_height(px(22.))
        .cursor_pointer()
        .text_color(theme::LABEL_3())
        .hover(|st| st.text_color(theme::LABEL_2()))
        .child(if expanded {
            "收起".to_string()
        } else {
            format!("… 其余 {hidden} 行")
        })
        .on_click(move |_, _, cx| {
            let k = k.clone();
            s.update(cx, |st, cx| st.toggle_card_expanded(&k, cx));
        })
        .into_any_element()
}

// ── search 卡 ─────────────────────────────────────────────────

/// 展平行(头尾切片的统一单元:文件头/匹配行/路径行各占一行)
#[derive(Clone)]
enum SearchRow {
    File {
        path: String,
        count: usize,
        group: usize,
    },
    Match {
        number: u64,
        line: String,
        group: usize,
    },
    Path(String),
}

/// search 卡:header(计数摘要 + 复制)+ matches 分组(paths 列表)。
/// 截断时卡下 recovery footer 由调用方(render_search_footnote)渲染
pub(crate) fn render_search(
    store: &Entity<AppStore>,
    cx: &App,
    ix: usize,
    key: &str,
    card: &SearchCard,
) -> gpui_kit::AnyElement {
    let st = store.read(cx);
    let expanded = st.chat.card_expanded.contains(key);
    let prefix = format!("{key}:");
    let collapsed: std::collections::HashSet<String> = st
        .chat
        .search_collapsed
        .iter()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect();

    let (rows, summary, copy_text) = match card {
        SearchCard::Matches {
            files,
            truncated,
            total,
        } => {
            let shown: usize = files.iter().map(|f| f.matches.len()).sum();
            let count = if *truncated {
                format!("显示 {shown} / 共 {total}")
            } else {
                format!("{shown}")
            };
            let summary = format!("{count} 处匹配 · {} 个文件", files.len());
            let mut rows: Vec<SearchRow> = Vec::new();
            for (group, f) in files.iter().enumerate() {
                rows.push(SearchRow::File {
                    path: f.path.clone(),
                    count: f.matches.len(),
                    group,
                });
                if collapsed.contains(&format!("{key}:{}", f.path)) {
                    continue;
                }
                rows.extend(f.matches.iter().map(|(n, l)| SearchRow::Match {
                    number: *n,
                    line: l.clone(),
                    group,
                }));
            }
            let copy = files
                .iter()
                .map(|f| {
                    [f.path.clone()]
                        .into_iter()
                        .chain(f.matches.iter().map(|(n, l)| format!("{n}: {l}")))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            (rows, summary, copy)
        }
        SearchCard::Paths {
            paths,
            truncated,
            total,
        } => {
            let count = if *truncated {
                format!("显示 {} / 共 {total}", paths.len())
            } else {
                format!("{}", paths.len())
            };
            let summary = format!("{count} 个路径");
            let rows = paths.iter().map(|p| SearchRow::Path(p.clone())).collect();
            (rows, summary, paths.join("\n"))
        }
    };

    // 头尾切片 + tail 文件头修复:tail 首行是匹配且 head 无其文件头时
    // 补回(并从 tail 删一行抵消,hidden 保持精确)
    let empty = rows.is_empty();
    let ht = head_tail(rows.len(), CHAT_CARD_MAX_LINES, expanded);
    let (head, tail, tail_header): (Vec<SearchRow>, Vec<SearchRow>, Option<SearchRow>) = if ht
        .capped
    {
        let head: Vec<SearchRow> = rows[..ht.head].to_vec();
        let mut natural_tail: Vec<SearchRow> = rows[rows.len() - ht.tail..].to_vec();
        let restore_group = match natural_tail.first() {
            Some(SearchRow::Match { group, .. }) => Some(*group),
            _ => None,
        };
        let tail_header = restore_group.and_then(|g| {
            let head_has = head
                .iter()
                .any(|r| matches!(r, SearchRow::File { group, .. } if *group == g));
            if head_has {
                return None;
            }
            rows.iter().find_map(|r| match r {
                SearchRow::File { path, count, group } if *group == g => Some(SearchRow::File {
                    path: path.clone(),
                    count: *count,
                    group: *group,
                }),
                _ => None,
            })
        });
        if tail_header.is_some() {
            natural_tail.remove(0);
        }
        (head, natural_tail, tail_header)
    } else {
        (rows, Vec::new(), None)
    };

    let mut card_el = div()
        .id(("search-card", ix))
        .relative()
        .v_flex()
        .ml(px(4.))
        .rounded(px(12.))
        .bg(theme::CODE())
        .overflow_hidden()
        .font_family("Menlo")
        .text_size(px(13.))
        .line_height(px(22.))
        // header:摘要(13 LABEL_2 截断)+ 复制(空结果无)
        .child(
            div()
                .flex()
                .min_w(px(0.))
                .items_center()
                .gap(px(12.))
                .px(px(14.))
                .py(px(9.))
                .bg(theme::CARD())
                .child(
                    div()
                        .min_w(px(0.))
                        .flex_1()
                        .truncate()
                        .text_size(px(13.))
                        .line_height(px(18.))
                        .text_color(theme::LABEL_2())
                        .child(summary),
                )
                .when(!empty, |el| {
                    el.child(copy_button(
                        store,
                        cx,
                        ("search-copy", ix),
                        &format!("{key}·search"),
                        &copy_text,
                    ))
                }),
        );
    if empty {
        card_el = card_el.child(
            div()
                .px(px(14.))
                .py(px(12.))
                .text_color(theme::LABEL_3())
                .child("无结果"),
        );
    } else {
        card_el = card_el.child(
            div()
                .id(("search-body", ix))
                .overflow_scroll()
                // 源 .body padding: 8px 14px 12px 0(左 14 由行 pl 14 承担)
                .pt(px(8.))
                .pb(px(12.))
                .child(
                    div().v_flex().children(
                        head.iter()
                            .map(|r| search_row_el(store, ix, key, r))
                            .chain(std::iter::once(search_expand_row(
                                store, cx, ix, key, ht.hidden,
                            )))
                            .chain(tail_header.iter().map(|r| search_row_el(store, ix, key, r)))
                            .chain(tail.iter().map(|r| search_row_el(store, ix, key, r)))
                            .collect::<Vec<_>>(),
                    ),
                ),
        );
    }
    card_el.into_any_element()
}

/// 一条展平行(文件头可点击折叠组;匹配行 `N: ` 前缀 + 文本)
fn search_row_el(
    store: &Entity<AppStore>,
    ix: usize,
    key: &str,
    row: &SearchRow,
) -> gpui_kit::AnyElement {
    match row {
        SearchRow::File { path, count, group } => {
            let s = store.clone();
            let k = format!("{key}:{path}");
            div()
                .id(gpui_kit::ElementId::Name(
                    format!("search-file-{ix}-{group}").into(),
                ))
                .flex()
                .min_h(px(22.))
                .line_height(px(22.))
                .items_baseline()
                .gap(px(8.))
                .pl(px(14.))
                .cursor_pointer()
                .child(
                    div()
                        .min_w(px(0.))
                        .font_weight(gpui_kit::FontWeight::BOLD)
                        .text_color(theme::LABEL())
                        .child(path.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme::LABEL_3())
                        .child(count.to_string()),
                )
                .on_click(move |_, _, cx| {
                    let k = k.clone();
                    s.update(cx, |st, cx| st.toggle_search_group(&k, cx));
                })
                .into_any_element()
        }
        SearchRow::Match { number, line, .. } => div()
            .flex()
            .min_h(px(22.))
            .line_height(px(22.))
            .pl(px(14.))
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(theme::LABEL_3())
                    .child(format!("{number}: ")),
            )
            .child(div().text_color(theme::LABEL()).child(line.clone()))
            .into_any_element(),
        SearchRow::Path(path) => div()
            .flex()
            .min_h(px(22.))
            .line_height(px(22.))
            .pl(px(14.))
            .text_color(theme::LABEL())
            .child(path.clone())
            .into_any_element(),
    }
}

/// search 展开钮(pl 14,与行对齐)
fn search_expand_row(
    store: &Entity<AppStore>,
    cx: &App,
    ix: usize,
    key: &str,
    hidden: usize,
) -> gpui_kit::AnyElement {
    // 收敛行在 hidden>0 时恒显示(源 SearchBlock.hidden>0 无条件渲染),不随
    // capped(展开)消失;capped 仅决定展开前头尾是否截断,不影响按钮存在。
    if hidden == 0 {
        return div().into_any_element();
    }
    let expanded = store.read(cx).chat.card_expanded.contains(key);
    let s = store.clone();
    let k = key.to_string();
    div()
        .id(("search-expand", ix))
        .pl(px(14.))
        .min_h(px(22.))
        .line_height(px(22.))
        .cursor_pointer()
        .text_color(theme::LABEL_3())
        .hover(|st| st.text_color(theme::LABEL_2()))
        .child(if expanded {
            "收起".to_string()
        } else {
            format!("… 其余 {hidden} 行")
        })
        .on_click(move |_, _, cx| {
            let k = k.clone();
            s.update(cx, |st, cx| st.toggle_card_expanded(&k, cx));
        })
        .into_any_element()
}

/// 截断 recovery footer(卡下,tertiary 13px):截断尾注自原始输出提取
pub(crate) fn search_recovery_footer(output: Option<&str>) -> Option<String> {
    let note = output?.lines().find(|l| l.starts_with("(truncated"))?;
    Some(note.to_string())
}

// ── diff 卡 ───────────────────────────────────────────────────

/// 展平行(path 头/删行/增行/同文件间隙)
enum DiffRow {
    Path(String),
    Del(String),
    Add(String),
    Gap,
}

/// diff 卡:无横幅,path 头(粗体,pr 56 让位浮动钮)+ `-` 红 / `+` 绿
/// 正文 + `└ +A -R · N file(s)` footer + 浮动复制钮
pub(crate) fn render_diff(
    store: &Entity<AppStore>,
    cx: &App,
    ix: usize,
    key: &str,
    card: &DiffCard,
) -> gpui_kit::AnyElement {
    let expanded = store.read(cx).chat.card_expanded.contains(key);

    // 展平:同文件第二条 hunk 以 `⋯` 间隙开头(不重复路径头)
    let mut rows: Vec<DiffRow> = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut paths = std::collections::HashSet::new();
    let mut prev_path: Option<&str> = None;
    for d in &card.diffs {
        paths.insert(d.path.as_str());
        if prev_path != Some(d.path.as_str()) {
            rows.push(DiffRow::Path(d.path.clone()));
        } else {
            rows.push(DiffRow::Gap);
        }
        prev_path = Some(d.path.as_str());
        if let Some(old) = &d.old_text {
            for line in content_lines(old) {
                rows.push(DiffRow::Del(line.to_string()));
                removed += 1;
            }
        }
        for line in content_lines(&d.new_text) {
            rows.push(DiffRow::Add(line.to_string()));
            added += 1;
        }
    }
    if rows.is_empty() {
        return div().into_any_element();
    }

    let ht = head_tail(rows.len(), CHAT_CARD_MAX_LINES, expanded);
    let head_end = if ht.capped { ht.head } else { rows.len() };
    let copy_text = rows
        .iter()
        .map(|r| match r {
            DiffRow::Del(t) => format!("- {t}"),
            DiffRow::Add(t) => format!("+ {t}"),
            DiffRow::Path(t) => t.clone(),
            DiffRow::Gap => "⋯".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");

    let files = paths.len();
    div()
        .id(("diff-card", ix))
        .relative()
        .v_flex()
        .ml(px(4.))
        .rounded(px(12.))
        .bg(theme::CODE())
        .overflow_hidden()
        .font_family("Menlo")
        .text_size(px(13.))
        .line_height(px(22.))
        .child(
            div()
                .id(("diff-body", ix))
                .overflow_scroll()
                .px(px(14.))
                .py(px(12.))
                .child(
                    div().v_flex().children(
                        rows[..head_end]
                            .iter()
                            .map(diff_row_el)
                            .chain(std::iter::once(diff_expand_row(
                                store, cx, ix, key, ht.hidden,
                            )))
                            .chain(
                                rows[rows.len() - if ht.capped { ht.tail } else { 0 }..]
                                    .iter()
                                    .map(diff_row_el),
                            )
                            .collect::<Vec<_>>(),
                    ),
                ),
        )
        // footer:└ +A -R · N file(s)
        .child(
            div()
                .px(px(14.))
                .pb(px(12.))
                .text_color(theme::LABEL_3())
                .child(format!(
                    "└ +{added} -{removed} · {files} file{}",
                    if files == 1 { "" } else { "s" }
                )),
        )
        .when(!copy_text.is_empty(), |el| {
            el.child(
                div()
                    .absolute()
                    .top(px(8.))
                    .right(px(12.))
                    .child(copy_button(
                        store,
                        cx,
                        ("diff-copy", ix),
                        &format!("{key}·diff"),
                        &copy_text,
                    )),
            )
        })
        .into_any_element()
}

/// 一条 diff 行(Path 粗体 pr 56;Del 红 `+ Add 绿;Gap ⋯)
fn diff_row_el(row: &DiffRow) -> gpui_kit::AnyElement {
    let el = match row {
        DiffRow::Path(p) => div()
            .min_h(px(22.))
            .line_height(px(22.))
            .pr(px(56.))
            .font_weight(gpui_kit::FontWeight::BOLD)
            .text_color(theme::LABEL())
            .child(p.clone()),
        DiffRow::Del(t) => div()
            .flex()
            .min_h(px(22.))
            .line_height(px(22.))
            .text_color(theme::DANGER())
            .child(format!("- {t}")),
        DiffRow::Add(t) => div()
            .flex()
            .min_h(px(22.))
            .line_height(px(22.))
            .text_color(theme::SUCCESS())
            .child(format!("+ {t}")),
        DiffRow::Gap => div()
            .min_h(px(22.))
            .line_height(px(22.))
            .text_color(theme::LABEL_3())
            .child("⋯"),
    };
    el.into_any_element()
}

/// diff 展开钮(无缩进;源 .expand padding 0)
fn diff_expand_row(
    store: &Entity<AppStore>,
    cx: &App,
    ix: usize,
    key: &str,
    hidden: usize,
) -> gpui_kit::AnyElement {
    if hidden == 0 {
        return div().into_any_element();
    }
    let expanded = store.read(cx).chat.card_expanded.contains(key);
    let s = store.clone();
    let k = key.to_string();
    div()
        .id(("diff-expand", ix))
        .min_h(px(22.))
        .line_height(px(22.))
        .cursor_pointer()
        .text_color(theme::LABEL_3())
        .hover(|st| st.text_color(theme::LABEL_2()))
        .child(if expanded {
            "收起".to_string()
        } else {
            format!("… 其余 {hidden} 行")
        })
        .on_click(move |_, _, cx| {
            let k = k.clone();
            s.update(cx, |st, cx| st.toggle_card_expanded(&k, cx));
        })
        .into_any_element()
}

/// 变更侧文本 → 内容行(空文本零行;单一尾换行是终结符不是空行)
fn content_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    body.split('\n').collect()
}

// ── 公共复制钮(源 copyButton:label-secondary,13px,文案切换)──

fn copy_button(
    store: &Entity<AppStore>,
    cx: &App,
    id: impl Into<gpui_kit::ElementId>,
    key: &str,
    text: &str,
) -> impl IntoElement {
    let copied = store.read(cx).chat.copied_key.as_deref() == Some(key);
    let s = store.clone();
    let (k, t) = (key.to_string(), text.to_string());
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .cursor_pointer()
        .text_size(px(13.))
        .line_height(px(18.))
        .text_color(theme::LABEL_2())
        .hover(|st| st.text_color(theme::LABEL()))
        .child(if copied { "复制成功" } else { "复制" })
        .on_click(move |_, _, cx| {
            let (k, t) = (k.clone(), t.clone());
            s.update(cx, |st, cx| st.copy_message(&k, &t, cx));
        })
}

/// 聊天位卡体头尾上限(CHAT_*_MAX_LINES = 原语默认 16 的一半)
pub(crate) const CHAT_CARD_MAX_LINES: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    // ── 窄化:合法/非法样本 ────────────────────────────────────

    #[test]
    fn narrow_valid_views() {
        let v = narrow(&serde_json::json!({
            "card": "read", "path": "a.rs", "offset": 1,
            "lines": [ { "number": 1, "text": "x" } ],
            "totalLines": 9, "lang": "rust",
        }))
        .expect("合法 read");
        match v {
            CardView::Read(r) => {
                assert_eq!(r.path, "a.rs");
                assert_eq!(r.total_lines, 9);
                assert_eq!(r.lines, vec![(1, "x".to_string())]);
            }
            _ => panic!(),
        }

        match narrow(&serde_json::json!({
            "card": "search", "shape": "searchMatches", "truncated": true, "total": 10,
            "files": [ { "path": "a.rs", "matches": [ { "number": 2, "text": "m" } ] } ],
        }))
        .expect("合法 matches")
        {
            CardView::Search(SearchCard::Matches {
                files,
                truncated,
                total,
            }) => {
                assert!(truncated);
                assert_eq!(total, 10);
                assert_eq!(files[0].matches, vec![(2, "m".to_string())]);
            }
            _ => panic!(),
        }

        assert!(matches!(
            narrow(&serde_json::json!({
                "card": "terminal", "exitCode": 3, "signal": null, "cwd": "/w",
            }))
            .unwrap(),
            CardView::Terminal(_)
        ));
        assert!(matches!(
            narrow(&serde_json::json!({
                "card": "diff",
                "diffs": [ { "path": "a", "oldText": null, "newText": "n" } ],
            }))
            .unwrap(),
            CardView::Diff(_)
        ));
    }

    /// 非法/未知视图 → None(线界健壮性:字段缺失、类型错、未知 card)
    #[test]
    fn narrow_rejects_invalid_views() {
        for bad in [
            serde_json::json!({}),                 // 无 card
            serde_json::json!({ "card": "web" }),  // 未知卡(词汇表预留位)
            serde_json::json!({ "card": "read" }), // 字段缺失
            serde_json::json!({ "card": "read", "path": 3, "offset": 1,
                "lines": [], "totalLines": 1 }), // 类型错
            serde_json::json!({ "card": "search", "shape": "weird",
                "truncated": false, "total": 0 }), // 未知 shape
            serde_json::json!({ "card": "diff", "diffs": [] }), // 空 diffs
        ] {
            assert!(narrow(&bad).is_none(), "应拒绝: {bad}");
        }
    }

    // ── 纯逻辑单元 ────────────────────────────────────────────

    #[test]
    fn head_tail_split() {
        let ht = head_tail(20, 8, false);
        assert_eq!((ht.hidden, ht.head, ht.tail), (12, 4, 4));
        assert!(ht.capped);
        let ht = head_tail(8, 8, false);
        assert_eq!(ht.hidden, 0);
        assert!(!ht.capped);
        // 展开不切(capped=false),但 hidden 仍非零——收起按钮在展开后
        // 仍应渲染(源 ReadBlock/SearchBlock/DiffBlock 按钮条件 = hidden>0)。
        let ht = head_tail(20, 8, true);
        assert!(!ht.capped);
        assert_eq!(ht.hidden, 12, "展开后 hidden 仍准确(收起按钮据此恒渲染)");
    }

    #[test]
    fn content_lines_terminator_rule() {
        assert!(content_lines("").is_empty());
        assert_eq!(content_lines("a\n"), vec!["a"]);
        assert_eq!(content_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(content_lines("a\n\n"), vec!["a", ""]); // 内部空行保留
    }
}
