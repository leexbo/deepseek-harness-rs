//! 语法高亮(syntect + 自写 VSCode Dark+/Light+ 近似主题,随主题盘)。
//!
//! 服务 read 卡正文与 markdown 代码块(shiki 式
//! 行级高亮;diff 卡/终端卡本就不高亮)。
//!
//! - 懒加载:SyntaxSet + Theme 经 OnceLock,首次调用 ~几十 ms,启动不阻塞;
//! - lang → syntax:`find_syntax_by_token`(语言名/扩展名均认),未知名回退
//!   纯文本(None);
//! - 行级有状态:`HighlightLines` 逐行喂入,跨行语法上下文正确(多行字符串/
//!   块注释不丢色)——等效整窗 tokenize;
//! - 主题:程序化构造 ~20 scope 的 Dark+/Light+ 近似(色值与轨迹
//!   json_tokens 同族,字面取 VSCode 调色板;按 theme::is_dark() 取盘);
//! - 块级缓存:key + 内容哈希,512B 入驻门槛 + 上限 128(与 markdown/terminal
//!   parse 缓存同模式)——重绘零重算,流式代码块跟随内容变化只重算当前块。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use gpui_kit::Rgba;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ScopeSelectors, Theme, ThemeItem, ThemeSettings};
use syntect::parsing::SyntaxSet;

use super::cache::MemoCache;

/// 一个高亮 span(纯色;MVP 不做粗斜体)
pub(crate) struct Span {
    pub color: Rgba,
    pub text: String,
}

// ── 引擎(懒加载单例)─────────────────────────────────────────

struct Engine {
    set: SyntaxSet,
    /// 双盘语法主题(下标 = theme::is_dark() as usize;浅盘 = Light+)
    themes: [Theme; 2],
}

fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| Engine {
        set: SyntaxSet::load_defaults_newlines(),
        themes: [plus_theme(false), plus_theme(true)],
    })
}

// ── 自写主题(VSCode Dark+ / Light+ 近似;与轨迹 json_tokens 同族)──

/// VSCode 调色板(字面值;json_tokens 的 #CE9178/#B5CEA8 即出 Dark+)
mod palette {
    use gpui_kit::Rgba;

    pub const FG: Rgba = rgb(0xD4D4D4); // 默认前景
    pub const COMMENT: Rgba = rgb(0x6A9955);
    pub const STRING: Rgba = rgb(0xCE9178);
    pub const NUMBER: Rgba = rgb(0xB5CEA8);
    pub const KEYWORD: Rgba = rgb(0xC586C0); // 控制流/导入类关键字
    pub const KEYWORD_TYPE: Rgba = rgb(0x569CD6); // 类型/存储/语言常量
    pub const FUNCTION: Rgba = rgb(0xDCDCAA);
    pub const TYPE: Rgba = rgb(0x4EC9B0); // 类/接口/内建类型名
    pub const VARIABLE: Rgba = rgb(0x9CDCFE); // 参数/属性
    pub const ESCAPE: Rgba = rgb(0xD7BA7D);

    const fn rgb(hex: u32) -> Rgba {
        Rgba {
            r: ((hex >> 16) & 0xFF) as f32 / 255.0,
            g: ((hex >> 8) & 0xFF) as f32 / 255.0,
            b: (hex & 0xFF) as f32 / 255.0,
            a: 1.0,
        }
    }
}

/// VSCode Light+ 调色板(浅盘代码块;与 Dark+ 同槽位一一对应)
mod light_palette {
    use gpui_kit::Rgba;

    pub const FG: Rgba = rgb(0x3B3B3B);
    pub const COMMENT: Rgba = rgb(0x008000);
    pub const STRING: Rgba = rgb(0xA31515);
    pub const NUMBER: Rgba = rgb(0x098658);
    pub const KEYWORD: Rgba = rgb(0xAF00DB);
    pub const KEYWORD_TYPE: Rgba = rgb(0x0000FF);
    pub const FUNCTION: Rgba = rgb(0x795E26);
    pub const TYPE: Rgba = rgb(0x267F99);
    pub const VARIABLE: Rgba = rgb(0x001080);
    pub const ESCAPE: Rgba = rgb(0xEE0000);

    const fn rgb(hex: u32) -> Rgba {
        Rgba {
            r: ((hex >> 16) & 0xFF) as f32 / 255.0,
            g: ((hex >> 8) & 0xFF) as f32 / 255.0,
            b: (hex & 0xFF) as f32 / 255.0,
            a: 1.0,
        }
    }
}

fn to_sy_color(c: Rgba) -> syntect::highlighting::Color {
    syntect::highlighting::Color {
        r: (c.r * 255.) as u8,
        g: (c.g * 255.) as u8,
        b: (c.b * 255.) as u8,
        a: (c.a * 255.) as u8,
    }
}

/// scope 选择器(解析失败即构造主题失败——字面量常量表,不容错)
fn sel(s: &str) -> ScopeSelectors {
    s.parse()
        .unwrap_or_else(|e| panic!("非法 scope 选择器 {s}: {e}"))
}

fn plus_theme(dark: bool) -> Theme {
    use syntect::highlighting::StyleModifier;
    let item = |scope: &str, color: Rgba| ThemeItem {
        scope: sel(scope),
        style: StyleModifier {
            foreground: Some(to_sy_color(color)),
            background: None,
            font_style: Some(FontStyle::empty()),
        },
    };
    let (fg, comment, string, number, keyword, keyword_type, function, ty, variable, escape) =
        if dark {
            (
                palette::FG,
                palette::COMMENT,
                palette::STRING,
                palette::NUMBER,
                palette::KEYWORD,
                palette::KEYWORD_TYPE,
                palette::FUNCTION,
                palette::TYPE,
                palette::VARIABLE,
                palette::ESCAPE,
            )
        } else {
            (
                light_palette::FG,
                light_palette::COMMENT,
                light_palette::STRING,
                light_palette::NUMBER,
                light_palette::KEYWORD,
                light_palette::KEYWORD_TYPE,
                light_palette::FUNCTION,
                light_palette::TYPE,
                light_palette::VARIABLE,
                light_palette::ESCAPE,
            )
        };
    // 泛化在前、特化在后(syntect 按 scope 匹配度取最优)
    let items = vec![
        item("comment", comment),
        item("punctuation.definition.comment", comment),
        item("string", string),
        item("punctuation.definition.string", string),
        item("constant.character.escape", escape),
        item("constant.numeric", number),
        item("constant.language", keyword_type),
        item("keyword", keyword),
        item("storage", keyword_type),
        item("entity.name.function", function),
        item("support.function", function),
        item("entity.name.type", ty),
        item("support.type", ty),
        item("support.class", ty),
        item("entity.name.tag", keyword_type),
        item("entity.other.attribute-name", variable),
        item("variable.language", keyword_type),
        item("variable.other", variable),
        item("support.variable.property", variable),
    ];
    Theme {
        name: Some(if dark {
            "dsh-dark-plus".into()
        } else {
            "dsh-light-plus".into()
        }),
        author: Some("dsh-desktop 自写(VSCode Dark+/Light+ 近似)".into()),
        scopes: items,
        settings: ThemeSettings {
            foreground: Some(to_sy_color(fg)),
            ..Default::default()
        },
    }
}

// ── 高亮管线 ─────────────────────────────────────────────────

/// 常见语言名归一/近似(默认语法集的缺口:无 TypeScript/toml——
/// TS 按 JS 近似高亮,toml 回退纯文本;后续如需精确再补语法包)
fn alias(lang: &str) -> String {
    match lang.to_ascii_lowercase().as_str() {
        "typescript" | "ts" | "tsx" | "jsx" => "javascript".into(),
        "golang" => "go".into(),
        "shell" | "shellscript" | "zsh" => "bash".into(),
        other => other.to_string(),
    }
}

/// 高亮窗口:逐行 spans(与输入行一一对应,空行为空 vec)。
/// lang 未知名/缺省 → None(调用方回退纯文本)。
fn highlight(lang: &str, lines: &[&str]) -> Option<Vec<Vec<Span>>> {
    let eng = engine();
    let lang = alias(lang);
    let syntax = eng.set.find_syntax_by_token(&lang)?;
    let mut hl = HighlightLines::new(syntax, &eng.themes[crate::kits::theme::is_dark() as usize]);
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        // newline 加载语法集要求行尾带 \n;结果范围按需裁掉
        let fed = format!("{line}\n");
        let ranges = hl.highlight_line(&fed, &eng.set).ok()?;
        let mut spans: Vec<Span> = Vec::new();
        for (style, text) in ranges {
            if text.is_empty() {
                continue;
            }
            spans.push(Span {
                color: rgba_of(style.foreground),
                text: text.to_string(),
            });
        }
        // 行尾终结符贴在末 span 上:剥掉,避免 h_flex 里出现空白占位
        if let Some(last) = spans.last_mut() {
            while last.text.ends_with('\n') {
                last.text.pop();
            }
            if last.text.is_empty() {
                spans.pop();
            }
        }
        out.push(spans);
    }
    Some(out)
}

fn rgba_of(c: syntect::highlighting::Color) -> Rgba {
    Rgba {
        r: c.r as f32 / 255.0,
        g: c.g as f32 / 255.0,
        b: c.b as f32 / 255.0,
        a: c.a as f32 / 255.0,
    }
}

// ── 块级缓存(同 markdown/terminal parse 缓存模式)─────────────

const CACHE_CAP: usize = 128;
/// 入缓存的最小文本长度(短块高亮本就廉价,不驻留留内存)
const CACHE_MIN_BYTES: usize = 512;

/// 域内自持高亮缓存(见 kits::cache;key 撞车互不可见)
static CACHE: MemoCache<Vec<Vec<Span>>> = MemoCache::new(CACHE_CAP, CACHE_MIN_BYTES);

/// 带缓存的高亮窗口:key 需调用方稳定(read 卡 = call key;markdown 块 =
/// 节点 key)。命中条件 = 同 key + 同 lang + 内容哈希一致。
pub(crate) fn highlight_window(
    key: &str,
    lang: Option<&str>,
    lines: &[&str],
) -> Option<Arc<Vec<Vec<Span>>>> {
    let lang = lang?;
    let bytes: usize = lines.iter().map(|l| l.len()).sum();
    let hash = {
        let mut h = DefaultHasher::new();
        lang.hash(&mut h);
        for l in lines {
            l.hash(&mut h);
        }
        h.finish()
    };
    // 盘随主题切:缓存 key 必须带盘,否则换盘后吃到旧色
    let mode_tag = if crate::kits::theme::is_dark() {
        "dark"
    } else {
        "light"
    };
    let cache_key = format!("{mode_tag}·{key}·{lang}");
    if let Some(spans) = CACHE.get(&cache_key, hash) {
        return Some(spans);
    }
    let spans = Arc::new(highlight(lang, lines)?);
    CACHE.put(&cache_key, hash, spans.clone(), bytes);
    Some(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors_of(spans: &[Span]) -> Vec<Rgba> {
        spans.iter().map(|s| s.color).collect()
    }

    /// 语言映射:常见名/扩展名命中(TS 无语法按 JS 近似),未知名回退 None
    #[test]
    fn lang_mapping() {        let lines = ["fn main() {}"];
        assert!(highlight_window("t", Some("rust"), &lines).is_some());
        assert!(highlight_window("t", Some("rs"), &lines).is_some());
        assert!(highlight_window("t", Some("py"), &lines).is_some());
        assert!(highlight_window("t", Some("typescript"), &lines).is_some());
        assert!(highlight_window("t", Some("no-such-lang"), &lines).is_none());
        assert!(highlight_window("t", None, &lines).is_none());
    }

    /// Light+ 主题构造:浅盘字面值就位(浅色代码块的分发面)
    #[test]
    fn light_plus_theme_built() {
        let t = plus_theme(false);
        assert_eq!(t.name.as_deref(), Some("dsh-light-plus"));
        assert_eq!(
            t.settings.foreground.expect("前景"),
            to_sy_color(light_palette::FG)
        );
    }

    /// Dark+ 基本类色:注释绿、字符串橙、数字浅绿、let 蓝
    #[test]
    fn dark_plus_basic_classes() {
        let lines = ["// 注释", "let s = \"str\";", "let n = 42;"];
        let spans = highlight("rust", &lines).expect("rust 高亮");
        assert_eq!(spans.len(), 3);
        // 注释行可被 syntect 拆多段(标点+内容),但整行皆绿
        assert!(
            colors_of(&spans[0]).iter().all(|c| *c == palette::COMMENT),
            "{:?}",
            colors_of(&spans[0])
        );
        let line2 = colors_of(&spans[1]);
        assert!(
            line2.contains(&palette::KEYWORD_TYPE),
            "let 应为蓝:{line2:?}"
        );
        assert!(line2.contains(&palette::STRING), "字符串应为橙:{line2:?}");
        let line3 = colors_of(&spans[2]);
        assert!(line3.contains(&palette::NUMBER), "数字应为浅绿:{line3:?}");
    }

    /// 行级有状态:块注释/多行字符串跨行不丢色
    #[test]
    fn multiline_context_carries() {
        let lines = ["/* 起", "跨行注释仍绿", "收 */", "let x = 1;"];
        let spans = highlight("rust", &lines).expect("rust");
        assert_eq!(
            colors_of(&spans[1]),
            vec![palette::COMMENT],
            "注释续行应保持绿"
        );
        // 终止行后恢复正常着色(含关键字蓝)
        let after = colors_of(&spans[3]);
        assert!(after.contains(&palette::KEYWORD_TYPE) || after.contains(&palette::NUMBER));
    }

    /// 空行为空 vec;行尾 \n 剥净(spans 文本无换行符)
    #[test]
    fn empty_lines_and_newline_stripped() {
        let lines = ["let a = 1;", "", "let b = 2;"];
        let spans = highlight("rust", &lines).expect("rust");
        assert_eq!(spans.len(), 3);
        assert!(spans[1].is_empty());
        for line in &spans {
            for s in line {
                assert!(!s.text.contains('\n'), "span 不应含换行:{:?}", s.text);
            }
        }
    }

    /// 缓存命中(Arc 指针相等);短块不驻留
    #[test]
    fn cache_hits_and_threshold() {
        let long: Vec<String> = (0..40)
            .map(|i| format!("let v{i} = {i}; // 注释行"))
            .collect();
        let refs: Vec<&str> = long.iter().map(|s| s.as_str()).collect();
        let a = highlight_window("k1", Some("rust"), &refs).expect("hl");
        let b = highlight_window("k1", Some("rust"), &refs).expect("hl");
        assert!(Arc::ptr_eq(&a, &b));
        // 内容变化 → 重算
        let mut changed = long.clone();
        changed[0] = "let changed = 0;".into();
        let refs2: Vec<&str> = changed.iter().map(|s| s.as_str()).collect();
        let c = highlight_window("k1", Some("rust"), &refs2).expect("hl");
        assert!(!Arc::ptr_eq(&a, &c));
        // 短块不入缓存(两次调用不同 Arc)
        let s1 = highlight_window("k2", Some("rust"), &["let x = 1;"]).expect("hl");
        let s2 = highlight_window("k2", Some("rust"), &["let x = 1;"]).expect("hl");
        assert!(!Arc::ptr_eq(&s1, &s2));
    }
}
