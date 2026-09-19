//! 会话标题规范化与确定性回退。
//!
//! 规范化剥离终端控制码/方向性控制符、把空白塌缩为单空格,再做
//! UTF-8 字节截断(不劈开码点)。用 [`char`] 迭代 + 控制字符白名单
//! 实现——Rust `char` 遍历天然按码点切分,不会劈开 emoji/汉字。

/// 去控制码 + 空白归一,产出单行标题文本(空串若清洗后为空)。
///
/// 剥 OSC/CSI/ESC 序列、C0/C1 控制符、双向与
/// 隐形控制符、BOM,最后 `\s+`→单空格并 trim。码点扫描实现
/// (Rust `char` 遍历天然不劈码点)。
pub fn clean_title_text(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            // OSC `ESC ]` 或 `0x9D`:跳到 BEL/`ESC \`/串尾
            '\u{001B}' if chars.get(i + 1) == Some(&']') => {
                i = skip_until(&chars, i + 1, |x| x == '\u{0007}' || x == '\u{001B}');
                continue;
            }
            '\u{009D}' => {
                i = skip_until(&chars, i + 1, |x| x == '\u{0007}' || x == '\u{001B}');
                continue;
            }
            // CSI `ESC [` 或 `0x9B`:参数字节 `0x30-0x3F` + 中间字节
            // `0x20-0x2F` + 一个终结字节 `0x40-0x7E`
            '\u{001B}' if chars.get(i + 1) == Some(&'[') => {
                i = skip_csi(&chars, i + 2);
                continue;
            }
            '\u{009B}' => {
                i = skip_csi(&chars, i + 1);
                continue;
            }
            // 其余 2 字节 ESC 序列 `ESC @-_`
            '\u{001B}' => {
                i += if chars
                    .get(i + 1)
                    .is_some_and(|n| matches!(n, '\u{0040}'..='\u{005F}'))
                {
                    2
                } else {
                    1 // 孤立 ESC:剥掉自身
                };
                continue;
            }
            _ if is_control_or_directional(c) => {
                i += 1;
                continue;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 从 `start` 起扫描到满足 `halt` 的字节为止(消费该字节),返回新索引。
fn skip_until(chars: &[char], mut i: usize, halt: impl Fn(char) -> bool) -> usize {
    while i < chars.len() {
        if halt(chars[i]) {
            return i + 1;
        }
        i += 1;
    }
    i
}

/// 跳过一条 CSI 序列:@`start` 起先吃参数字节(`0x30-0x3F`)与中间字节
/// (`0x20-0x2F`),再吃一个终结字节(`0x40-0x7E`);无终结字节则吃到底。
fn skip_csi(chars: &[char], mut i: usize) -> usize {
    let n = chars.len();
    while i < n && matches!(chars[i], '\u{0030}'..='\u{003F}') {
        i += 1;
    }
    while i < n && matches!(chars[i], '\u{0020}'..='\u{002F}') {
        i += 1;
    }
    if i < n && matches!(chars[i], '\u{0040}'..='\u{007E}') {
        i += 1;
    }
    i
}

/// 该 char 是否应剥离(源 CSS/ESC/控制/方向性正则在码点层面的并集)。
fn is_control_or_directional(c: char) -> bool {
    match c {
        // C0 控制(保留 \t \n \r 给空白归一)
        '\u{0000}'..='\u{0008}' | '\u{000B}' | '\u{000C}' | '\u{000E}'..='\u{001F}' => true,
        // DEL + C1
        '\u{007F}'..='\u{009F}' => true,
        // BOM
        '\u{FEFF}' => true,
        // 方向性/隐形控制(CS:S 分离的 \u{200B}..;源 DIRECTIONAL_CONTROL)
        '\u{200B}'
        | '\u{200E}'
        | '\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}' => true,
        _ => false,
    }
}

/// UTF-8 字节预算截断(不劈开码点;超出预算即截)。
///
/// 对应源 `truncateTitleUtf8`:逐码点累计 UTF-8 字节,超过 `max_bytes` 断。
pub fn truncate_title_utf8(input: &str, max_bytes: usize) -> String {
    debug_assert!(max_bytes > 0, "max_bytes 必须为正整数");
    let mut used = 0usize;
    let mut out = String::new();
    for c in input.chars() {
        let bytes = c.len_utf8();
        if used + bytes > max_bytes {
            break;
        }
        out.push(c);
        used += bytes;
    }
    out
}

/// 规范化一条被接受的标题并强制其 UTF-8 字节预算(末去尾空白)。
///
/// 对应源 `normalizeSessionTitle(input, maxBytes)`。
pub fn normalize_session_title(input: &str, max_bytes: usize) -> String {
    truncate_title_utf8(&clean_title_text(input), max_bytes)
        .trim_end()
        .to_string()
}

/// 确定性首条消息回退标题(末去尾空白)。
///
/// 对应源 `fallbackSessionTitle(input, maxWords, maxBytes)`:清洗 → 取
/// 前 `max_words` 个空白分隔词 → 按字节截断 → 去尾空白。
pub fn fallback_session_title(input: &str, max_words: usize, max_bytes: usize) -> String {
    let cleaned = clean_title_text(input);
    let words: Vec<&str> = cleaned
        .split(' ')
        .filter(|w| !w.is_empty())
        .take(max_words)
        .collect();
    truncate_title_utf8(&words.join(" "), max_bytes)
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_control_and_collapses_whitespace() {
        assert_eq!(
            clean_title_text("  fix\u{001B}[31m  red  \u{200E}  bug  "),
            "fix red bug"
        );
        assert_eq!(clean_title_text("\u{FEFF}hello  world\n"), "hello world");
        // 空白归一:换行/制表塌缩为单空格
        assert_eq!(clean_title_text("仅\n\t空白"), "仅 空白");
    }

    #[test]
    fn truncate_utf8_does_not_split_codepoint() {
        // 每个汉字 3 字节:预算 6 字节容纳 2 个汉字
        assert_eq!(truncate_title_utf8("你好世界", 6), "你好");
        // ASCII
        assert_eq!(truncate_title_utf8("abcdef", 4), "abcd");
    }

    #[test]
    fn normalize_title_trims_end() {
        assert_eq!(normalize_session_title("  a  b  ", 100), "a b");
    }

    #[test]
    fn fallback_takes_first_words_and_byte_caps() {
        assert_eq!(
            fallback_session_title("one two three four five six", 5, 100),
            "one two three four five"
        );
        assert_eq!(fallback_session_title("a b c d e", 3, 3), "a b");
        // 空输入 → 空标题
        assert_eq!(fallback_session_title("", 5, 40), "");
    }
}
