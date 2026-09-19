//! 终端卡(bash
//! 工具调用的展开体——提示符横幅(状态点 + cwd 标签 + 逐行
//! `$ command` + 失败退出 pill + 复制钮)+ ANSI 彩色输出区(等宽、
//! 不软换行保列对齐、内部双向滚动)。失败语义:非零退出是
//! **结果数据**(success=true + exitCode 透出),信号终止才计失败。
//!
//! ANSI 解析(务实移植):OSC/非 CSI 转义与惰性
//! 控制符清除;SGR 状态跨行折叠(换行不重置);基本 16 色映射主题
//! token(黑/白→LABEL、亮黑→弱化、红/绿/黄→DANGER/SUCCESS/WARN、
//! 蓝→BRAND),256 色/真彩直渲;`\r`/退格/擦行(`ESC[K`)按终端列
//! 缓冲重放(CJK 宽字符双列);行内 tab 展开到 8 列制表位。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use gpui_kit::component::StyledExt;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, Rgba, StatefulInteractiveElement,
    Styled, div, px,
};

use super::projection::ToolState;
use crate::kits::cache::MemoCache;
use crate::kits::theme;
use crate::shell::store::AppStore;

// ── ANSI 模型 ─────────────────────────────────────────────────

/// 一个输出 span(style 为 None → 裸文本,不加包装)
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AnsiSpan {
    pub text: String,
    pub style: Option<SpanStyle>,
}

/// SGR 态解析后的行内样式
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct SpanStyle {
    pub color: Option<Rgba>,
    pub bg: Option<Rgba>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
}

pub(crate) type AnsiLine = Vec<AnsiSpan>;

/// SGR 图形状态(跨行折叠;newline 不重置)
#[derive(Debug, Clone, PartialEq, Default)]
struct Sgr {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Color {
    /// 基本 16 色(索引 0..16)
    Basic(u8),
    /// 256 色盘索引
    Indexed(u8),
    /// 真彩
    Rgb(u8, u8, u8),
}

impl Sgr {
    /// 解析为渲染样式(全默认态 → None:裸文本不加包装)
    fn style(&self) -> Option<SpanStyle> {
        let style = SpanStyle {
            color: self.fg.map(|c| resolve(c, true)),
            bg: self.bg.map(|c| resolve(c, false)),
            bold: self.bold,
            dim: self.dim,
            italic: self.italic,
            underline: self.underline,
            strike: self.strike,
        };
        (style != SpanStyle::default()).then_some(style)
    }
}

/// VGA 基本色表(anser 同款:暗色终端的 16 基色)
const VGA: [[u8; 3]; 16] = [
    [0, 0, 0],
    [187, 0, 0],
    [0, 187, 0],
    [187, 187, 0],
    [0, 0, 187],
    [187, 0, 187],
    [0, 187, 187],
    [255, 255, 255],
    [85, 85, 85],
    [255, 85, 85],
    [0, 255, 0],
    [255, 255, 85],
    [85, 85, 255],
    [255, 85, 255],
    [85, 255, 255],
    [255, 255, 255],
];

/// 基本色的主题 token 映射:黑/白
/// 收敛主标签(黑字在暗底不可读),亮黑取弱化标签;红/绿/黄/蓝落到
/// 状态色;品红/青无 token 对应,走字面色
fn resolve(c: Color, is_fg: bool) -> Rgba {
    let [r, g, b] = match c {
        Color::Rgb(r, g, b) => [r, g, b],
        Color::Indexed(n) => color_256(n),
        Color::Basic(i) => {
            if is_fg {
                match i {
                    0 | 7 => return theme::LABEL(),
                    8 => return theme::LABEL_3(),
                    1 | 9 => return theme::DANGER(),
                    2 | 10 => return theme::SUCCESS(),
                    3 | 11 => return theme::WARN(),
                    4 | 12 => return theme::BRAND(),
                    _ => VGA[i as usize],
                }
            } else {
                VGA[i as usize]
            }
        }
    };
    Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

/// xterm 256 色盘:16 基色 + 6³ 立方 + 24 灰阶
fn color_256(n: u8) -> [u8; 3] {
    match n {
        0..=15 => VGA[n as usize],
        16..=231 => {
            const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let i = n - 16;
            [
                STEPS[(i / 36) as usize],
                STEPS[((i / 6) % 6) as usize],
                STEPS[(i % 6) as usize],
            ]
        }
        232..=255 => {
            let g = 8 + (n - 232) * 10;
            [g, g, g]
        }
    }
}

/// 折叠一条 SGR 参数序列(params 如 "31"、"1;4"、"38;5;208")
fn fold_sgr(state: &mut Sgr, params: &[i64]) {
    let mut i = 0;
    while i < params.len() {
        let code = params[i];
        match code {
            0 => *state = Sgr::default(),
            1 => state.bold = true,
            2 => state.dim = true,
            3 => state.italic = true,
            4 => state.underline = true,
            9 => state.strike = true,
            21 | 22 => {
                state.bold = false;
                state.dim = false;
            }
            23 => state.italic = false,
            24 => state.underline = false,
            29 => state.strike = false,
            39 => state.fg = None,
            49 => state.bg = None,
            38 | 48 => {
                // 扩展色吃掉自己的参数:38;5;N / 38;2;R;G;B
                let kind = params.get(i + 1).copied().unwrap_or(-1);
                let (value, span): (Option<Color>, usize) = match kind {
                    5 => (
                        params
                            .get(i + 2)
                            .and_then(|n| u8::try_from(*n).ok())
                            .map(Color::Indexed),
                        2,
                    ),
                    2 => (
                        params.get(i + 2).and_then(|r| {
                            params.get(i + 3).and_then(|g| {
                                params
                                    .get(i + 4)
                                    .map(|b| Color::Rgb(clamp_u8(*r), clamp_u8(*g), clamp_u8(*b)))
                            })
                        }),
                        4,
                    ),
                    _ => (None, 0),
                };
                let value = value.or(Some(Color::Basic(0)));
                if code == 38 {
                    state.fg = value;
                } else {
                    state.bg = value;
                }
                i += span;
            }
            30..=37 => state.fg = Some(Color::Basic((code - 30) as u8)),
            40..=47 => state.bg = Some(Color::Basic((code - 40) as u8)),
            90..=97 => state.fg = Some(Color::Basic((code - 90 + 8) as u8)),
            100..=107 => state.bg = Some(Color::Basic((code - 100 + 8) as u8)),
            _ => {}
        }
        i += 1;
    }
}

fn clamp_u8(v: i64) -> u8 {
    v.clamp(0, 255) as u8
}

// ── 清理与重放 ────────────────────────────────────────────────

/// 移除 OSC(窗口标题/超链接)、非 CSI 转义与惰性控制符;保留 CSI
/// 序列(色)与 `\n`(布局)、`\r`/`\x08`/tab(重放输入)
fn sanitize(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\x1b' {
            match chars.get(i + 1) {
                // OSC … BEL / ESC \:整段去
                Some(']') => {
                    i += 2;
                    while i < chars.len() && chars[i] != '\x07' && chars[i] != '\x1b' {
                        i += 1;
                    }
                    if i < chars.len() && chars[i] == '\x07' {
                        i += 1;
                    } else if chars[i..].starts_with(&['\x1b', '\\']) {
                        i += 2;
                    }
                    continue;
                }
                // CSI:原样保留(色/擦除)
                Some('[') => {
                    out.push(c);
                    i += 1;
                    continue;
                }
                // 其他转义(charset/reset 等):丢弃(可选中间字节)
                _ => {
                    i += 1;
                    while i < chars.len() && ('\x20'..='\x2f').contains(&chars[i]) {
                        i += 1;
                    }
                    if i < chars.len() && ('\x30'..='\x7e').contains(&chars[i]) {
                        i += 1;
                    }
                    continue;
                }
            }
        }
        // 惰性控制符(NUL/BEL/VT/FF/SO…/DEL):无显示意义;
        // \r(0x0D) 保留——重放输入
        if matches!(c,
            '\x00'..='\x07'
                | '\x0b' | '\x0c'
                | '\x0e'..='\x1a'
                | '\x1c'..='\x1f'
                | '\x7f')
        {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 该行是否需要列缓冲重放(回车/退格/擦行)
fn needs_replay(line: &str) -> bool {
    line.contains('\r') || line.contains('\x08') || has_erase_in_line(line)
}

/// `ESC[…K`(参数可含 `;` 与中间字节)
fn has_erase_in_line(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i + 1 < chars.len() {
        if chars[i] == '\x1b' && chars[i + 1] == '[' {
            let mut j = i + 2;
            while j < chars.len() && ('\x30'..='\x3f').contains(&chars[j]) {
                j += 1;
            }
            while j < chars.len() && ('\x20'..='\x2f').contains(&chars[j]) {
                j += 1;
            }
            if j < chars.len() && chars[j] == 'K' {
                return true;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    false
}

/// 终端列宽(CJK/全角/emoji 双列;文本呈现符号单列)
fn is_wide(c: char) -> bool {
    let u = c as u32;
    matches!(u,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD)
}

/// 列缓冲单元格(写入时的 SGR 态随格留存:重绘不波及未覆盖格)
#[derive(Debug, Clone, PartialEq)]
struct Cell {
    sgr: Sgr,
    ch: char,
    /// 宽字符双列的后半(占位,发射时输出空格)
    spacer: bool,
}

/// 重放一行的光标移动:`\r`/退格/擦行按终端语义画进列缓冲,再按格
/// 态发射 span(入口 SGR 态作行首默认)。返回行 spans + 行尾态。
fn replay_line(line: &str, entry: &Sgr) -> (AnsiLine, Sgr) {
    let chars: Vec<char> = line.chars().collect();
    let mut columns: Vec<Cell> = Vec::new();
    let mut cursor = 0usize;
    let mut sgr = entry.clone();
    let mut i = 0;

    let write = |columns: &mut Vec<Cell>, cursor: usize, sgr: &Sgr, ch: char| {
        // 覆盖宽字符任一半:另一半同时清空(终端不留半格)
        if let Some(cell) = columns.get(cursor)
            && cell.spacer
            && cursor > 0
        {
            columns[cursor - 1] = Cell {
                sgr: sgr.clone(),
                ch: ' ',
                spacer: false,
            };
        } else if let Some(cell) = columns.get(cursor)
            && is_wide(cell.ch)
            && columns.get(cursor + 1).is_some_and(|c| c.spacer)
        {
            columns[cursor + 1] = Cell {
                sgr: sgr.clone(),
                ch: ' ',
                spacer: false,
            };
        }
        if cursor >= columns.len() {
            columns.resize(
                cursor + 1,
                Cell {
                    sgr: Sgr::default(),
                    ch: ' ',
                    spacer: false,
                },
            );
        }
        columns[cursor] = Cell {
            sgr: sgr.clone(),
            ch,
            spacer: false,
        };
    };

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\r' => cursor = 0,
            '\x08' => cursor = cursor.saturating_sub(1),
            '\t' => {
                let stop = cursor + 8 - cursor % 8;
                while cursor < stop {
                    if cursor >= columns.len() {
                        columns.push(Cell {
                            sgr: sgr.clone(),
                            ch: ' ',
                            spacer: false,
                        });
                    } else if columns[cursor].ch == '\0' {
                        columns[cursor] = Cell {
                            sgr: sgr.clone(),
                            ch: ' ',
                            spacer: false,
                        };
                    }
                    cursor += 1;
                }
            }
            '\x1b' if chars.get(i + 1) == Some(&'[') => {
                // CSI:参数(0x30-3f)→ 中间(0x20-2f)→ final(0x40-7e)
                let mut j = i + 2;
                let params_start = j;
                while j < chars.len() && ('\x30'..='\x3f').contains(&chars[j]) {
                    j += 1;
                }
                let params_end = j;
                while j < chars.len() && ('\x20'..='\x2f').contains(&chars[j]) {
                    j += 1;
                }
                let final_byte = chars.get(j).copied();
                let params: String = chars[params_start..params_end].iter().collect();
                if let Some(fin) = final_byte {
                    match fin {
                        'K' => {
                            // 擦行:0=光标起至行尾,1=行首至光标(含),2=全行
                            let mode = params
                                .split(';')
                                .next()
                                .and_then(|m| m.parse::<i64>().ok())
                                .unwrap_or(0);
                            match mode {
                                1 => {
                                    for cell in columns.iter_mut().take(cursor + 1) {
                                        *cell = Cell {
                                            sgr: sgr.clone(),
                                            ch: ' ',
                                            spacer: false,
                                        };
                                    }
                                }
                                2 => columns.clear(),
                                _ => columns.truncate(cursor),
                            }
                        }
                        'm' => {
                            let parsed: Vec<i64> = if params.is_empty() {
                                vec![0]
                            } else {
                                params
                                    .split(';')
                                    .map(|p| p.parse::<i64>().unwrap_or(0))
                                    .collect()
                            };
                            fold_sgr(&mut sgr, &parsed);
                        }
                        _ => {}
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
            _ => {
                write(&mut columns, cursor, &sgr, c);
                cursor += 1;
                if is_wide(c) {
                    if cursor >= columns.len() {
                        columns.resize(
                            cursor + 1,
                            Cell {
                                sgr: Sgr::default(),
                                ch: ' ',
                                spacer: false,
                            },
                        );
                    }
                    columns[cursor] = Cell {
                        sgr: sgr.clone(),
                        ch: ' ',
                        spacer: true,
                    };
                    cursor += 1;
                }
            }
        }
        i += 1;
    }

    // 发射:同态连续格并为 span(spacer 出空格保列位)
    let mut spans: AnsiLine = Vec::new();
    let mut active: Option<(Sgr, String)> = None;
    for cell in &columns {
        let ch = if cell.spacer { ' ' } else { cell.ch };
        match &mut active {
            Some((s, buf)) if *s == cell.sgr => buf.push(ch),
            _ => {
                if let Some((s, buf)) = active.take()
                    && !buf.is_empty()
                {
                    spans.push(AnsiSpan {
                        text: buf,
                        style: s.style(),
                    });
                }
                active = Some((cell.sgr.clone(), ch.to_string()));
            }
        }
    }
    if let Some((s, buf)) = active.take()
        && !buf.is_empty()
    {
        spans.push(AnsiSpan {
            text: buf,
            style: s.style(),
        });
    }
    (spans, sgr)
}

/// 无重放行:线性扫 SGR 序列产 span(tab 展开),SGR 态跨行续携带
fn scan_line(line: &str, entry: &Sgr) -> (AnsiLine, Sgr) {
    let chars: Vec<char> = line.chars().collect();
    let mut sgr = entry.clone();
    let mut spans: AnsiLine = Vec::new();
    let mut buf = String::new();
    let mut buf_style = sgr.style();
    let mut column = 0usize;
    let mut i = 0;

    fn flush(spans: &mut AnsiLine, buf: &mut String, style: Option<SpanStyle>) {
        if !buf.is_empty() {
            spans.push(AnsiSpan {
                text: std::mem::take(buf),
                style,
            });
        } else {
            buf.clear();
        }
    }

    while i < chars.len() {
        let c = chars[i];
        if c == '\x1b' && chars.get(i + 1) == Some(&'[') {
            let mut j = i + 2;
            let params_start = j;
            while j < chars.len() && ('\x30'..='\x3f').contains(&chars[j]) {
                j += 1;
            }
            let params_end = j;
            while j < chars.len() && ('\x20'..='\x2f').contains(&chars[j]) {
                j += 1;
            }
            let final_byte = chars.get(j).copied();
            let params: String = chars[params_start..params_end].iter().collect();
            if final_byte == Some('m') {
                let parsed: Vec<i64> = if params.is_empty() {
                    vec![0]
                } else {
                    params
                        .split(';')
                        .map(|p| p.parse::<i64>().unwrap_or(0))
                        .collect()
                };
                flush(&mut spans, &mut buf, buf_style);
                fold_sgr(&mut sgr, &parsed);
                buf_style = sgr.style();
            }
            if final_byte.is_some() {
                j += 1;
            }
            i = j;
            continue;
        }
        if c == '\t' {
            let stop = column + 8 - column % 8;
            while column < stop {
                buf.push(' ');
                column += 1;
            }
            i += 1;
            continue;
        }
        buf.push(c);
        column += if is_wide(c) { 2 } else { 1 };
        i += 1;
    }
    flush(&mut spans, &mut buf, buf_style);
    (spans, sgr)
}

/// 解析输出为逐行 span(至少一行)。行尾换行终结符不算空行。
pub(crate) fn parse_ansi_lines(text: &str) -> Vec<AnsiLine> {
    let cleaned = sanitize(text);
    let mut lines: Vec<AnsiLine> = Vec::new();
    let mut state = Sgr::default();
    for raw in cleaned.split('\n') {
        // CRLF 的 \r 只是换行终结,不触发重放
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let (spans, next) = if needs_replay(line) {
            replay_line(line, &state)
        } else {
            scan_line(line, &state)
        };
        lines.push(spans);
        state = next;
    }
    if lines.len() > 1
        && lines
            .last()
            .is_some_and(|l| l.iter().all(|s| s.text.is_empty()))
    {
        lines.pop();
    }
    lines
}

// ── parse 缓存(同 markdown.rs 模式:展开的工具行每帧重绘,命中
//    则免重解析;key=call key,哈希守护;短输出不入缓存约束驻留)──

const CACHE_CAP: usize = 64;
/// 入缓存的最小输出长度(短输出解析廉价,不入驻留内存)
const CACHE_MIN_BYTES: usize = 512;

/// 域内自持解析缓存(见 kits::cache;key 撞车互不可见)
static CACHE: MemoCache<Vec<AnsiLine>> = MemoCache::new(CACHE_CAP, CACHE_MIN_BYTES);

fn parse_cached(key: &str, text: &str) -> Arc<Vec<AnsiLine>> {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    let hash = h.finish();
    if let Some(lines) = CACHE.get(key, hash) {
        return lines;
    }
    let lines = Arc::new(parse_ansi_lines(text));
    CACHE.put(key, hash, lines.clone(), text.len());
    lines
}

// ── 终端卡渲染 ────────────────────────────────────────────────

/// 失败判定:信号终止或非零退出;
/// 干净落定(码 0/无信号)不算失败
pub(crate) fn failed(signal: Option<&str>, exit_code: Option<i32>) -> bool {
    signal.is_some() || exit_code.is_some_and(|c| c != 0)
}

/// cwd 的提示符标签:路径尾段(无 cwd → `$`)
pub(crate) fn prompt_label(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return "$".into();
    }
    let segment = trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed);
    segment.to_string()
}

/// 终端卡整体(bash 展开体)。含**聊天
/// 渲染位覆写**(12px/18px 小号代码字体、l1 描边、
/// 输出区 224px 封顶)与组件本体(30px 左 gutter、光晕状态点、
/// 9/14/30 横幅内边距、l2 分隔线、Pill 退出态、行高 18)
#[allow(clippy::too_many_arguments)]
pub(crate) fn render(
    store: &Entity<AppStore>,
    cx: &App,
    ix: usize,
    key: &str,
    command: &str,
    cwd: Option<&str>,
    output: Option<&str>,
    state: ToolState,
    exit_code: Option<i32>,
    signal: Option<&str>,
) -> gpui_kit::AnyElement {
    let running = state == ToolState::Running;
    let is_failed = failed(signal, exit_code);
    // 状态点色(StateDot 语义):running=进行蓝,失败=红,干净=绿
    let dot_color = if running {
        theme::ONGOING()
    } else if is_failed {
        theme::DANGER()
    } else {
        theme::SUCCESS()
    };
    // 退出 pill:信号优先,其次非零码;干净落定无 pill
    let pill = if running {
        None
    } else if let Some(sig) = signal {
        Some(format!("信号 {sig}"))
    } else {
        exit_code.filter(|c| *c != 0).map(|c| format!("退出码 {c}"))
    };

    // 多行命令 = 每行一条提示行;尾随换行是终结符不是空命令
    let command_lines: Vec<&str> = {
        let body = command.strip_suffix('\n').unwrap_or(command);
        body.split('\n').collect()
    };

    let mut card = div()
        .id(("terminal-card", ix))
        .relative()
        .v_flex()
        // 聊天位缩进(margin 4 0 4 4;垂直由列 gap)
        .ml(px(4.))
        .rounded(px(12.))
        // l1 描边(聊天位覆写)+ 代码块表面 #1b1b1c
        .border_1()
        .border_color(theme::BORDER())
        .bg(theme::CODE())
        .overflow_hidden()
        // 小号代码字体(--dsw-font-markdown-code-block-small:12px/18px)
        .font_family("Menlo")
        .text_size(px(12.))
        .line_height(gpui_kit::relative(1.5))
        // 光晕状态点:浮在卡片 30px gutter 内(左 8;首行垂直中线 =
        // banner 上边距 9 + 行高 18/2 = 18,点 10px → top 13)
        .child(state_dot(("term-dot", ix), dot_color))
        // 横幅:提示行列(左让 gutter)+ pill/复制
        .child(
            div()
                .id(("term-banner", ix))
                .flex()
                .min_w(px(0.))
                .items_start()
                .gap(px(12.))
                .max_h(px(150.))
                .overflow_y_scroll()
                .pl(px(30.))
                .pr(px(14.))
                .py(px(9.))
                .when(!running, |el| {
                    el.border_b_1().border_color(theme::BORDER_2())
                })
                .child(
                    div().v_flex().min_w(px(0.)).flex_1().children(
                        command_lines
                            .iter()
                            .enumerate()
                            .map(|(li, line)| prompt_row(li, line, cwd))
                            .collect::<Vec<_>>(),
                    ),
                )
                .children(pill.map(status_pill))
                .children(
                    // 空输出(含仅转义/控制字节)不显示复制钮(以 trim 近似判空)
                    (!running && output.is_some_and(|o| !o.trim().is_empty()))
                        .then(|| copy_control(store, cx, ix, key, output.unwrap_or_default())),
                ),
        );

    if !running {
        card = card.child(output_area(ix, key, output));
    }
    card.into_any_element()
}

/// 光晕状态点(StateDot 同构):10px 同色 10% 光晕 + 6px 实心核
fn state_dot(id: impl Into<gpui_kit::ElementId>, color: Rgba) -> impl IntoElement {
    let halo = Rgba {
        a: color.a * 0.1,
        ..color
    };
    div()
        .id(id)
        .absolute()
        .left(px(8.))
        .top(px(13.))
        .size(px(10.))
        .rounded_full()
        .bg(halo)
        .child(
            div()
                .absolute()
                .top(px(2.))
                .left(px(2.))
                .size(px(6.))
                .rounded_full()
                .bg(color),
        )
}

/// 一条提示行:[cwd 标签|$] [command](命令 pre + 省略号截断;
/// 基线对齐;点在卡 gutter,行内无点槽)
fn prompt_row(li: usize, line: &str, cwd: Option<&str>) -> impl IntoElement {
    let label = if li == 0 {
        cwd.map(prompt_label).unwrap_or_else(|| "$".into())
    } else {
        "$".into()
    };
    div()
        .flex()
        .min_w(px(0.))
        .items_baseline()
        .gap(px(8.))
        .child(
            div()
                .flex_shrink_0()
                .text_color(theme::LABEL_3())
                .child(label),
        )
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .truncate()
                .text_color(theme::LABEL())
                .child(line.to_string()),
        )
}

/// 退出状态 pill(Pill 同构:行高等高、12px 圆角胶囊、layer-2 底、
/// 错误色文字)
fn status_pill(text: String) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .h(px(18.))
        .px(px(8.))
        .rounded(px(9.))
        .bg(theme::CARD())
        .text_size(px(12.))
        .text_color(theme::DANGER())
        .child(text)
}

/// 复制钮(复制原始输出,非渲染树;文案 复制/复制成功,
/// label-secondary 色,hover 提亮)
fn copy_control(
    store: &Entity<AppStore>,
    cx: &App,
    ix: usize,
    key: &str,
    output: &str,
) -> impl IntoElement {
    let copied = store.read(cx).chat.copied_key.as_deref() == Some(key);
    let s = store.clone();
    let (k, t) = (key.to_string(), output.to_string());
    div()
        .id(("term-copy", ix))
        .flex()
        .flex_shrink_0()
        .items_center()
        .h(px(18.))
        .cursor_pointer()
        // 13px 字号;行高随终端 18
        .text_size(px(13.))
        .text_color(theme::LABEL_2())
        .hover(|st| st.text_color(theme::LABEL()))
        .child(if copied { "复制成功" } else { "复制" })
        .on_click(move |_, _, cx| {
            let (k, t) = (k.clone(), t.clone());
            s.update(cx, |st, cx| st.copy_message(&k, &t, cx));
        })
}

/// 输出区:等宽不软换行(内层列无定宽 → MaxContent 单行测宽,外层
/// 横向滚动保列对齐)+ 垂直 224px 封顶内部滚(聊天位覆写
/// --dsl-terminal-output-max-height);空输出占位「无输出」
fn output_area(ix: usize, key: &str, output: Option<&str>) -> impl IntoElement {
    let Some(out) = output else {
        return empty_output().into_any_element();
    };
    let lines = parse_cached(key, out);
    if lines
        .iter()
        .all(|l| l.iter().all(|s| s.text.trim().is_empty()))
    {
        return empty_output().into_any_element();
    }
    div()
        .id(("term-out", ix))
        .max_h(px(224.))
        .overflow_scroll()
        // 左让 gutter,右 14,顶底 12(内边距 0 14 12 30)
        .pl(px(30.))
        .pr(px(14.))
        .py(px(12.))
        // 内层列无定宽:行按 MaxContent 单行测宽 → 不折行,超宽横向滚
        .child(
            div()
                .v_flex()
                .children(lines.iter().map(line_el).collect::<Vec<_>>()),
        )
        .into_any_element()
}

fn empty_output() -> impl IntoElement {
    div()
        .pl(px(30.))
        .pr(px(14.))
        .py(px(12.))
        .text_color(theme::LABEL_3())
        .child("无输出")
}

/// 单输出行(spans 横排;空行保最小行高维持行计数;非交互,无 id)。
/// 无 SGR 态的行用主标签色(输出基色 = label-primary)
fn line_el(line: &AnsiLine) -> impl IntoElement {
    let mut el = div()
        .flex()
        .min_h(px(18.))
        .line_height(gpui_kit::relative(1.5));
    for span in line {
        let mut s = div().text_color(
            span.style
                .as_ref()
                .and_then(|st| st.color)
                .unwrap_or(theme::LABEL()),
        );
        if let Some(st) = &span.style {
            if let Some(bg) = st.bg {
                s = s.bg(bg);
            }
            if st.bold {
                s = s.font_weight(gpui_kit::FontWeight::BOLD);
            }
            if st.dim {
                s = s.opacity(0.7);
            }
            if st.italic {
                s = s.italic();
            }
            if st.underline {
                s = s.underline();
            }
            if st.strike {
                s = s.line_through();
            }
        }
        el = el.child(s.child(span.text.clone()));
    }
    el
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[AnsiLine]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    #[test]
    fn plain_text_untouched() {
        assert_eq!(
            plain(&parse_ansi_lines("a\nb")),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn sgr_carries_across_lines_until_reset() {
        let lines = parse_ansi_lines("\x1b[31mred\nstill\x1b[0m plain");
        // 行 1:整行红
        assert_eq!(
            lines[0][0].style.as_ref().unwrap().color,
            Some(theme::DANGER())
        );
        // 行 2:红段 + 重置段(裸文本)
        assert_eq!(lines[1][0].text, "still");
        assert_eq!(
            lines[1][0].style.as_ref().unwrap().color,
            Some(theme::DANGER())
        );
        assert_eq!(lines[1][1].text, " plain");
        assert_eq!(lines[1][1].style, None);
    }

    #[test]
    fn osc_and_inert_controls_removed() {
        assert_eq!(
            plain(&parse_ansi_lines("\x1b]0;title\x07tex\x07t\x00")),
            vec!["text".to_string()]
        );
    }

    #[test]
    fn truecolor_and_256_resolve_literal() {
        let lines = parse_ansi_lines("\x1b[38;2;10;20;30ma\x1b[0m\x1b[38;5;196mb");
        assert_eq!(
            lines[0][0].style.as_ref().unwrap().color,
            Some(Rgba {
                r: 10. / 255.,
                g: 20. / 255.,
                b: 30. / 255.,
                a: 1.0
            })
        );
        // 196 = 立方 (5,0,0) → 255,0,0
        assert_eq!(
            lines[0][1]
                .style
                .as_ref()
                .unwrap()
                .color
                .map(|c| (c.r, c.g)),
            Some((1.0, 0.0))
        );
    }

    #[test]
    fn carriage_return_replays_columns() {
        // 重绘短于底帧:残帧尾巴保留(例:100%\rOK → OK0%)
        assert_eq!(
            plain(&parse_ansi_lines("100%\rOK")),
            vec!["OK0%".to_string()]
        );
        // 尾随退格只移光标不删格:abc\b → abc
        assert_eq!(
            plain(&parse_ansi_lines("abc\u{8}")),
            vec!["abc".to_string()]
        );
        // 擦行:ESB[K 截掉光标后残帧
        assert_eq!(
            plain(&parse_ansi_lines("spinner…\r\x1b[Kdone")),
            vec!["done".to_string()]
        );
    }

    #[test]
    fn wide_char_keeps_columns_on_redraw() {
        // 宽字符占两列:中\rA → A + 占位空格(列位不左移)
        assert_eq!(plain(&parse_ansi_lines("中\rA")), vec!["A ".to_string()]);
    }

    #[test]
    fn tabs_expand_to_eight_column_stops() {
        assert_eq!(
            plain(&parse_ansi_lines("a\tb")),
            vec!["a       b".to_string()]
        );
    }

    #[test]
    fn trailing_newline_terminator_is_not_a_blank_line() {
        assert_eq!(plain(&parse_ansi_lines("out\n")), vec!["out".to_string()]);
        // 真空行(双换行)保留
        assert_eq!(
            plain(&parse_ansi_lines("a\n\nb")),
            vec!["a".to_string(), String::new(), "b".to_string()]
        );
    }

    #[test]
    fn failed_and_prompt_label() {
        assert!(failed(Some("SIGTERM"), Some(0)));
        assert!(failed(None, Some(1)));
        assert!(!failed(None, Some(0)));
        assert!(!failed(None, None));
        assert_eq!(prompt_label("/w/some/dir/"), "dir");
        assert_eq!(prompt_label("/w"), "w");
        assert_eq!(prompt_label(""), "$");
    }

    #[test]
    fn parse_cache_hits_and_misses() {
        // 长输出(≥512B)入缓存:命中 = Arc 指针相等
        let long = "out-line\n".repeat(128);
        let a = parse_cached("t1", &long);
        let b = parse_cached("t1", &long);
        assert!(Arc::ptr_eq(&a, &b));
        let c = parse_cached("t1", "changed");
        assert!(!Arc::ptr_eq(&a, &c));
        // 短输出不入缓存(解析廉价,不驻留内存)
        let s1 = parse_cached("t2", "short");
        let s2 = parse_cached("t2", "short");
        assert!(!Arc::ptr_eq(&s1, &s2));
    }
}
