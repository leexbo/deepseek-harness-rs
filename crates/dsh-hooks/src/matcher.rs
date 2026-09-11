//! matcher:hook 点选择(源 hook-protocol/matcher.ts 逐字对齐)。
//!
//! 两模式(方言唯一差异轴):claude-code 下纯 `[A-Za-z0-9_|]+` 是
//! 字面量 alternation(`|` 分隔逐项精确等值,非子串),其余是无锚定
//! regex;codex 一律无锚定 regex。缺省/`''`/`'*'` = match-all 哨兵。
//! 无效正则运行时恒 false(绝不抛),解析期诊断串逐字照源。

use regex::Regex;

/// matcher 模式(源 MatcherMode):桥按方言选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatcherMode {
    /// Claude Code:word+pipe 字面量 alternation,其余 regex
    ClaudeCode,
    /// Codex:一律无锚定 regex
    Codex,
}

impl MatcherMode {
    /// 诊断串里的方言名(逐字:`invalid claude-code regex matcher "("`)
    pub fn name(self) -> &'static str {
        match self {
            MatcherMode::ClaudeCode => "claude-code",
            MatcherMode::Codex => "codex",
        }
    }
}

/// Claude 字面量判别:纯 word 字符 + `|`(源 CLAUDE_LITERAL)
fn is_claude_literal(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|')
}

/// match-all 哨兵:缺省 / `''` / `'*'`(源 isMatchAll)
fn is_match_all(matcher: Option<&str>) -> bool {
    match matcher {
        None => true,
        Some(m) => m.is_empty() || m == "*",
    }
}

/// 编译无锚定 regex;非法 pattern 返回 None(源 compileRegex)
fn compile_regex(pattern: &str) -> Option<Regex> {
    Regex::new(pattern).ok()
}

/// 解析期校验:合法返回 None,否则稳定诊断串(逐字照源
/// `invalid {mode} regex matcher {pattern:?}`;match-all 哨兵恒有效)。
pub fn matcher_diagnostic(matcher: Option<&str>, mode: MatcherMode) -> Option<String> {
    if is_match_all(matcher) {
        return None;
    }
    let pattern = matcher.unwrap_or_default();
    if mode == MatcherMode::ClaudeCode && is_claude_literal(pattern) {
        return None;
    }
    if compile_regex(pattern).is_none() {
        Some(format!(
            "invalid {} regex matcher {:?}",
            mode.name(),
            pattern
        ))
    } else {
        None
    }
}

/// 运行时匹配(源 matchesMatcher):claude 字面量 = `|` 分隔逐项精确
/// 等值;其余 = 无锚定 regex。无效 regex 返回 false 而非抛。
pub fn matches_matcher(matcher: Option<&str>, query: &str, mode: MatcherMode) -> bool {
    if is_match_all(matcher) {
        return true;
    }
    let pattern = matcher.unwrap_or_default();
    if mode == MatcherMode::ClaudeCode && is_claude_literal(pattern) {
        return pattern.split('|').any(|alt| alt == query);
    }
    compile_regex(pattern).is_some_and(|re| re.is_match(query))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_all_sentinels_in_both_modes() {
        for mode in [MatcherMode::ClaudeCode, MatcherMode::Codex] {
            assert!(matches_matcher(None, "anything", mode));
            assert!(matches_matcher(Some(""), "anything", mode));
            assert!(matches_matcher(Some("*"), "anything", mode));
            assert_eq!(matcher_diagnostic(None, mode), None);
            assert_eq!(matcher_diagnostic(Some(""), mode), None);
            assert_eq!(matcher_diagnostic(Some("*"), mode), None);
        }
    }

    #[test]
    fn claude_literal_is_exact_alternation_not_substring() {
        // 纯 word+pipe = 字面量:逐项精确等值
        assert!(matches_matcher(
            Some("Bash"),
            "Bash",
            MatcherMode::ClaudeCode
        ));
        assert!(!matches_matcher(
            Some("Bash"),
            "BashOutput",
            MatcherMode::ClaudeCode
        ));
        assert!(matches_matcher(
            Some("Edit|Write"),
            "Edit",
            MatcherMode::ClaudeCode
        ));
        assert!(matches_matcher(
            Some("Edit|Write"),
            "Write",
            MatcherMode::ClaudeCode
        ));
        assert!(!matches_matcher(
            Some("Edit|Write"),
            "EditFile",
            MatcherMode::ClaudeCode
        ));
    }

    #[test]
    fn claude_non_word_pattern_falls_through_to_regex() {
        // 含非 word 字符 = regex(无锚定)
        assert!(matches_matcher(
            Some("^git "),
            "git status",
            MatcherMode::ClaudeCode
        ));
        assert!(matches_matcher(
            Some("Edit.*"),
            "Editor",
            MatcherMode::ClaudeCode
        ));
    }

    #[test]
    fn codex_is_always_unanchored_regex() {
        // `Bash` 即 /Bash/:子串命中
        assert!(matches_matcher(
            Some("Bash"),
            "BashOutput",
            MatcherMode::Codex
        ));
        // regex 大小写敏感(照参考引擎):"Ed" 命中 "Editor"
        assert!(matches_matcher(Some("Ed"), "Editor", MatcherMode::Codex));
        assert!(!matches_matcher(Some("ed"), "Editor", MatcherMode::Codex));
        // alternation 与锚有效
        assert!(!matches_matcher(Some("a|b"), "c", MatcherMode::Codex));
        assert!(matches_matcher(Some("^a"), "ab", MatcherMode::Codex));
    }

    #[test]
    fn invalid_regex_is_non_match_not_panic() {
        assert!(!matches_matcher(Some("("), "x", MatcherMode::ClaudeCode));
        assert!(!matches_matcher(Some("("), "x", MatcherMode::Codex));
    }

    #[test]
    fn diagnostic_strings_match_source_verbatim() {
        assert_eq!(
            matcher_diagnostic(Some("("), MatcherMode::ClaudeCode).unwrap(),
            "invalid claude-code regex matcher \"(\""
        );
        assert_eq!(
            matcher_diagnostic(Some("["), MatcherMode::Codex).unwrap(),
            "invalid codex regex matcher \"[\""
        );
    }
}
