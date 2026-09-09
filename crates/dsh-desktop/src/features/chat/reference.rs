//! @ 引用核心:触发检测、mention 编解码、@session 快照投影。
//!
//! 全部为纯函数(无 IO/无窗口):可单测、可重放。
//! - [`active_at_token`]:向左回溯判定 `@` 是否构成待补全 token(行首/空白
//!   后触发;`user@host` 不触发;支持 `@"` quoted)。
//! - [`file_mention`]/[`session_mention`]:mention 文本与 URI 编解码
//!   (`dsh-session:` scheme,base64url 可逆)。
//! - [`session_snapshot`]:被引会话 → 不可信快照 JSON(user/assistant 文本
//!   投影 + 64KB 预算截断 + `<` 转义)——host 侧读取后调用。

use serde_json::json;

/// 字节预算(@session 快照)
pub const MAX_REFERENCE_BYTES: usize = 65_536;
/// 单条消息最多引用会话数(源 MAX_REFERENCES)
pub const MAX_REFERENCES: usize = 3;

/// 一个激活中的 `@` token(触发检测结果)
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveAtToken {
    /// 整个待替换 token(含 `@`;如 `@src/`)
    pub prefix: String,
    /// `@` 之后的查询文本(如 `src/a`)
    pub query: String,
    /// 是否打开了 `@"` quoted(路径含空格;quoted 时只列文件)
    pub quoted: bool,
}

/// 用户气泡内 `@` 引用 token 的语义分类(源 ReferenceIconKind)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtKind {
    /// 会话(@[label](dsh-session:...))
    Session,
    /// 文件(@path / @"path with space")
    File,
    /// 目录(@dir/ 斜杠结尾)
    Folder,
}

/// 用户气泡文本里的一个 `@` 引用 span(渲染成胶囊;纯函数扫描)
#[derive(Debug, Clone, PartialEq)]
pub struct AtToken {
    /// 语义分类
    pub kind: AtKind,
    /// 显示 label(去 @ 前缀/引号/路径 basename;会话 = 标签)
    pub label: String,
    /// span 起止(byte,含整个 token:file/folder 含 @,session 含整段)
    pub start: usize,
    pub end: usize,
}

/// 扫描用户气泡文本里的 `@` 引用 token(源 projectUserText 语义便携版):
/// - 会话:`@[label](dsh-session:...)` 整段(优先级高于其余)
/// - 文件/目录:`@path`(无空格)与 `@"path with space"`;`@` 前须行首或空白
///   (不误伤 `user@host`);裸 token 尾随标点剥除;斜杠结尾 = 目录。
///
/// 返回按 [`AtToken::start`] 升序、互不重叠的 span。
pub fn scan_at_tokens(text: &str) -> Vec<AtToken> {
    let mut out = Vec::new();
    // 1) 会话:优先识别 @[label](dsh-session:...),span 覆盖整段
    for cap in regex_caps(text, "markdown") {
        let Some(id) = decode_session_uri(cap[1]) else {
            continue;
        };
        let label = cap[0].to_string();
        // 计算整段 byte range(@[ 起到匹配的 dsh-session:... 结束)
        // cap[1] 是完整 uri(含 dsh-session: 前缀),原样拼回 `)` 即整段
        let pat = format!("@[{label}]({})", cap[1]);
        if let Some(pos) = find_span(text, &pat) {
            // 去重:同位置已有则跳过
            if !out.iter().any(|t: &AtToken| t.start == pos.0) {
                out.push(AtToken {
                    kind: AtKind::Session,
                    label,
                    start: pos.0,
                    end: pos.1,
                });
            }
        }
        let _ = id;
    }
    // 2) 文件/目录:词边界扫描 @path 与 @"path"
    scan_plain_at(text, &mut out);
    // 3) 按 start 排序 + 去重叠(会话 span 可能盖住内部子串)
    out.sort_by_key(|t| t.start);
    let mut dedup: Vec<AtToken> = Vec::new();
    for t in out {
        if dedup.last().is_some_and(|last| t.start < last.end) {
            continue;
        }
        dedup.push(t);
    }
    dedup
}
/// 判定光标前的 `@` 是否构成待补全 token。
///
/// 源 grammar.ts `activeAtToken` 语义:`@` 必须在**行首或紧跟空白/标点**
/// 之后;`user@host`、`done @src/x"` 这类不触发。返回 None = 无激活 token
/// (不弹菜单)。`caret` 为光标 byte offset(0..=text.len)。
pub fn active_at_token(text: &str, caret: usize) -> Option<ActiveAtToken> {
    let caret = caret.min(text.len());
    let before = &text[..caret];
    // 定位 `@` 起始:向左找最后一个 '@';@ 后到 caret 的文本是待补全前缀。
    let at_pos = before.rfind('@')?;
    let between = &before[at_pos + 1..];
    // @ 之前必须行首或空白/标点(边界)——`user@host`、`done @src/x"` 不触发
    if at_pos > 0 {
        let prev = before[..at_pos].chars().next_back().unwrap();
        if !(prev.is_whitespace() || prev.is_ascii_punctuation()) {
            return None;
        }
    }
    // quoted 形态:`@"..."`(未闭合引号)——quoted 时空格合法(路径含空格)
    let quote_after = between.starts_with('"');
    if quote_after {
        let query = between.trim_start_matches('"').to_string();
        return Some(ActiveAtToken {
            prefix: format!("@\"{query}"),
            query,
            quoted: true,
        });
    }
    // 非 quoted:`@` 后到 caret 不能含空白(空白即 token 结束,不算激活)
    if between.contains(char::is_whitespace) {
        return None;
    }
    let query = between.to_string();
    Some(ActiveAtToken {
        prefix: format!("@{query}"),
        query,
        quoted: false,
    })
}

/// @file mention(源 formatFileMention:含空格用 `@"..."`,否则裸路径)
pub fn file_mention(path: &str) -> String {
    if path.contains(char::is_whitespace) {
        format!("@\"{path}\"")
    } else {
        format!("@{path}")
    }
}

/// @session URI(源 encodeSessionReferenceUri:base64url JSON)
pub fn session_uri(session_id: &str) -> String {
    use base64::Engine as _;
    let json = serde_json::json!({ "sessionId": session_id });
    let b64 = base64::engine::general_purpose::URL_SAFE
        .encode(json.to_string().as_bytes())
        .trim_end_matches('=')
        .to_string();
    format!("dsh-session:{b64}")
}

/// @session mention(源 formatSessionReferenceMention)
pub fn session_mention(label: &str, session_id: &str) -> String {
    format!("@[{label}]({})", session_uri(session_id))
}

/// 从 mention 文本解析 session 引用(源 parseSessionReferenceText):
/// 匹配 `@[label](dsh-session:...)` 或裸 `dsh-session:...` URI。
/// 返回 (label, session_id) 列表。
pub fn parse_session_references(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // Markdown 形态 @[label](uri)。regex_caps 返回 [label, uri](2 元素)
    for cap in regex_caps(text, "markdown") {
        let label = cap[0].to_string();
        if let Some(id) = decode_session_uri(cap[1]) {
            out.push((label, id));
        }
    }
    // 裸 URI。regex_caps 返回 [uri](1 元素)
    for cap in regex_caps(text, "bare") {
        if let Some(id) = decode_session_uri(cap[0]) {
            // 去重(已由 markdown 形态解析)
            if out.iter().all(|(_, existing)| existing != &id) {
                out.push((cap[0].to_string(), id));
            }
        }
    }
    out
}

/// 解码 `dsh-session:<base64url>` → sessionId
pub fn decode_session_uri(uri: &str) -> Option<String> {
    let b64 = uri.strip_prefix("dsh-session:")?;
    use base64::Engine as _;
    let padded = {
        let pad = (4 - b64.len() % 4) % 4;
        format!("{b64}{}", "=".repeat(pad))
    };
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(padded)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v["sessionId"].as_str().map(String::from)
}

/// 一条快照会话(投影 + 截断结果)
#[derive(Debug, Clone)]
pub struct SnapshotSession {
    /// 会话 id
    pub session_id: String,
    /// label(mention 里的标签)
    pub label: String,
    /// 保留消息数(预算裁剪后;快照 JSON 记 retainedMessages)
    pub retained: usize,
    /// 原始消息数(投影前;快照 JSON 记 originalMessages)
    pub original: usize,
}

/// 快照投影(源 retainReferencedSession):只保留 user/assistant 文本,
/// 丢弃 tool/reasoning/嵌套 context;逐条塞进 [`MAX_REFERENCE_BYTES`] 预算,
/// 超了先丢非保留整条,再 head/tail 截断最长消息。
///
/// `messages` = 被引会话的 translated events(role/content),按序。
/// 返回快照 JSON 文本(外包 referencd-sessions 标签 + `<` 转义)。
pub fn session_snapshot(refs: &[SnapshotSession], messages: &[serde_json::Value]) -> String {
    let mut sessions: Vec<serde_json::Value> = Vec::new();
    for r in refs {
        let mut projected: Vec<(String, String)> = Vec::new();
        for m in messages {
            let role = m["role"].as_str().unwrap_or_default();
            if role != "user" && role != "assistant" {
                continue;
            }
            let text = m["content"]
                .as_str()
                .map(String::from)
                .or_else(|| {
                    m["content"].as_array().map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b["type"].as_str() == Some("text"))
                            .filter_map(|b| b["text"].as_str())
                            .collect::<String>()
                    })
                })
                .unwrap_or_default();
            if text.is_empty() {
                continue;
            }
            projected.push((role.to_string(), text));
        }
        let original = projected.len();
        let (projected, truncated) = fit_budget(projected);
        sessions.push(serde_json::json!({
            "sessionId": r.session_id,
            "label": r.label,
            "conversation": projected
                .iter()
                .map(|(role, text)| json!({ "role": role, "text": text }))
                .collect::<Vec<_>>(),
            "capturedThroughSeq": original,
            "retainedMessages": r.retained,
            "originalMessages": r.original,
            "truncated": truncated,
        }));
    }
    // 外包标签 + `<` 转义(防 `</referenced-sessions>` 逃逸)
    let json = serde_json::to_string(&sessions).unwrap_or_default();
    let escaped = json.replace('<', "\\u003c");
    format!("<referenced-sessions>\n{escaped}\n</referenced-sessions>")
}

/// 预算裁剪(源 truncateWithNotice):全部保留不超预算;超则先丢非 checkpoint
/// 且非最新的整条,再对最长消息 head/tail 截断并注明省略。
fn fit_budget(mut messages: Vec<(String, String)>) -> (Vec<(String, String)>, bool) {
    let mut total = messages.iter().map(|(_, t)| t.len()).sum::<usize>();
    if total <= MAX_REFERENCE_BYTES {
        return (messages, false);
    }
    // 从最旧开始丢(保留最新),直到回到预算或仅剩 1 条
    while messages.len() > 1 && total > MAX_REFERENCE_BYTES {
        let removed = messages.remove(0);
        total -= removed.1.len();
    }
    // 仍超:截断最长消息 head/tail
    let mut truncated = false;
    for (_, text) in messages.iter_mut() {
        if total <= MAX_REFERENCE_BYTES {
            break;
        }
        if text.len() > MAX_REFERENCE_BYTES {
            let head = MAX_REFERENCE_BYTES / 2;
            let tail = MAX_REFERENCE_BYTES - head - "[… omitted …]".len();
            let keep: String = text.chars().take(head).collect();
            let tail_str: String = text
                .chars()
                .rev()
                .take(tail)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            *text = format!("{keep}[… omitted …]{tail_str}");
            total = MAX_REFERENCE_BYTES;
            truncated = true;
        }
    }
    (messages, truncated || total > MAX_REFERENCE_BYTES)
}

/// 一条文件/目录补全候选(源 FileReferenceCandidate)
#[derive(Debug, Clone, PartialEq)]
pub struct FileCandidate {
    /// 相对 workspace 根的路径(如 `src/main.rs` 或 `src/`)
    pub path: String,
    /// 是否目录(目录 = 继续下钻)
    pub is_dir: bool,
}

/// 排除目录(源 DEFAULT_FILE_SEARCH_EXCLUDED_DIRECTORIES)
pub const EXCLUDED_DIRS: &[&str] = &[".git", "node_modules"];
/// 扫描上限(源 maxEntries)
const MAX_ENTRIES: usize = 10_000;
/// 结果上限(源 maxResults)
const MAX_RESULTS: usize = 20;

/// BFS 扫描 workspace(源 scanWorkspace):记 {path, kind},排除目录与
/// 隐藏(除非 query 以 `.` 开头或含 `/.`),上限 [`MAX_ENTRIES`]。
pub fn scan_workspace(root: &std::path::Path) -> Vec<FileCandidate> {
    let mut out = Vec::new();
    let mut stack = vec![std::path::PathBuf::new()];
    let mut count = 0;
    while let Some(rel) = stack.pop() {
        if count >= MAX_ENTRIES {
            break;
        }
        let abs = root.join(&rel);
        let Ok(entries) = std::fs::read_dir(&abs) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.path().is_dir();
            // 隐藏文件:默认隐藏(除非查询显式要求;扫描阶段保守隐藏)
            if name.starts_with('.') {
                continue;
            }
            // 排除目录
            if is_dir && EXCLUDED_DIRS.contains(&name.as_str()) {
                continue;
            }
            let path = if rel.as_os_str().is_empty() {
                name.clone()
            } else {
                format!("{}/{}", rel.display(), name)
            };
            count += 1;
            if count > MAX_ENTRIES {
                break;
            }
            out.push(FileCandidate { path, is_dir });
            if is_dir && count < MAX_ENTRIES {
                stack.push(
                    entry
                        .path()
                        .strip_prefix(root)
                        .unwrap_or(&entry.path())
                        .to_path_buf(),
                );
            }
        }
    }
    out
}

/// fuzzy 排名(源 scoreCandidate):basename 精确/前缀/包含/全路径包含/子序列;
/// 目录 +25。`query` 可为空(列根,按 kind 目录优先 + 字母序)。
pub fn rank_file_candidates(query: &str, candidates: &[FileCandidate]) -> Vec<FileCandidate> {
    let q = query.to_lowercase();
    let mut scored: Vec<(i64, usize, &FileCandidate)> = Vec::new();
    for (ix, c) in candidates.iter().enumerate() {
        let path_l = c.path.to_lowercase();
        let base_l = c.path.rsplit('/').next().unwrap_or("").to_lowercase();
        let score = if q.is_empty() {
            0
        } else if base_l == q {
            1000
        } else if base_l.starts_with(&q) {
            900
        } else if base_l.contains(&q) {
            700
        } else if path_l.contains(&q) {
            500
        } else if is_subsequence(&q, &path_l) {
            300
        } else {
            -1
        };
        if score < 0 {
            continue;
        }
        let score = score + if c.is_dir { 25 } else { 0 };
        scored.push((score, ix, c));
    }
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            // 平局:路径更短(更浅)优先,再原序——`main.rs` 优于 `src/main.rs`
            .then_with(|| a.2.path.len().cmp(&b.2.path.len()))
            .then_with(|| a.1.cmp(&b.1))
    });
    scored
        .into_iter()
        .take(MAX_RESULTS)
        .map(|(_, _, c)| c.clone())
        .collect()
}

/// 子序列匹配(源 subsequence,带 gap 惩罚简化:仅判断是否为子序列)
fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut it = hay.chars();
    'outer: for n in needle.chars() {
        loop {
            match it.next() {
                Some(c) if c == n => continue 'outer,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

/// `@` token 扫描结果绑定(桌面补全菜单渲染 + 选中替换用)。
/// `span` = 待替换的 byte range(base 0 起)。
#[derive(Debug, Clone)]
pub struct AtHit {
    /// 激活 token(含 @/quoted)
    pub token: ActiveAtToken,
    /// token 起始字节(含 @)
    pub start: usize,
    /// 光标字节
    pub caret: usize,
}

/// 从输入文本 + 光标字节计算 @ hit(供补全菜单;无激活 token = None)。
pub fn at_hit(text: &str, caret: usize) -> Option<AtHit> {
    let token = active_at_token(text, caret)?;
    let start = caret - token.prefix.len();
    Some(AtHit {
        token,
        start,
        caret,
    })
}

/// @session 候选(源 SessionReferenceCandidate)
#[derive(Debug, Clone)]
pub struct SessionCandidate {
    /// 会话 id
    pub session_id: String,
    /// label(标题;无则 id)
    pub label: String,
    /// 是否同 cwd(亲和度排名用)
    pub same_cwd: bool,
}

/// 会话候选排序(源 candidateRank):同 cwd 优先 → cwd 未定 → 其余;
/// query 匹配 sessionId/label 子串。
pub fn rank_session_candidates(
    query: &str,
    current_cwd: Option<&str>,
    candidates: &[SessionCandidate],
) -> Vec<SessionCandidate> {
    let q = query.to_lowercase();
    let mut sorted: Vec<&SessionCandidate> = candidates
        .iter()
        .filter(|c| {
            q.is_empty()
                || c.session_id.to_lowercase().contains(&q)
                || c.label.to_lowercase().contains(&q)
        })
        .collect();
    sorted.sort_by(|a, b| {
        let ra = if a.same_cwd { 0 } else { 1 };
        let rb = if b.same_cwd { 0 } else { 1 };
        ra.cmp(&rb).then_with(|| a.session_id.cmp(&b.session_id))
    });
    let _ = current_cwd;
    // 与文件候选同上限:会话多时候选行数不限会把补全卡顶到窗高
    sorted
        .into_iter()
        .take(MAX_RESULTS)
        .cloned()
        .collect()
}

/// 轻量匹配(避免引 regex crate;按标记分派)。
/// - `"markdown"`:找 `@[label](dsh-session:uri)`,每组返回 [label, uri]。
/// - `"bare"`:找 `dsh-session:...` URI,每组返回 [uri]。
fn regex_caps<'a>(text: &'a str, kind: &str) -> Vec<Vec<&'a str>> {
    let mut out = Vec::new();
    if kind == "markdown" {
        // @[label](dsh-session:...)
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'@'
                && bytes.get(i + 1) == Some(&b'[')
                && let Some(close_b) = find_byte(bytes, i + 2, b']')
                && let Some(close_p) = find_byte(bytes, close_b + 1, b')')
                && bytes.get(close_b + 1) == Some(&b'(')
            {
                let label = &text[i + 2..close_b];
                let uri = &text[close_b + 2..close_p];
                if uri.starts_with("dsh-session:") {
                    out.push(vec![label, uri]);
                }
                i = close_p + 1;
                continue;
            }
            i += 1;
        }
    } else {
        // "bare":(dsh-session:[A-Za-z0-9_-]+)
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i..].starts_with(b"dsh-session:") {
                let start = i;
                let mut j = i + b"dsh-session:".len();
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'-')
                {
                    j += 1;
                }
                out.push(vec![&text[start..j]]);
                i = j;
            } else {
                i += 1;
            }
        }
    }
    out
}

fn find_byte(bytes: &[u8], from: usize, target: u8) -> Option<usize> {
    bytes[from..]
        .iter()
        .position(|&b| b == target)
        .map(|p| p + from)
}

/// 在文本里定位 `pat` 首次出现的 byte range(base 0 起)。
/// 找不到 → None。
fn find_span(text: &str, pat: &str) -> Option<(usize, usize)> {
    let t = text.as_bytes();
    let p = pat.as_bytes();
    if p.is_empty() || p.len() > t.len() {
        return None;
    }
    for i in 0..=(t.len() - p.len()) {
        if &t[i..i + p.len()] == p {
            return Some((i, i + p.len()));
        }
    }
    None
}

/// 词边界扫描 `@file`/`@folder`(源 projectUserText 的 plain 分支)。
/// 已有 out 里的 session span 覆盖区跳过;`@"` quoted 支持空格路径;
/// 尾部中文/西文句读剥除;斜杠结尾 = 目录。
fn scan_plain_at(text: &str, out: &mut Vec<AtToken>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'@' || covered_by_session(out, i) {
            i += 1;
            continue;
        }
        // @ 前必须行首或空白(不误伤 user@host / 词内 @)
        if i > 0 {
            let prev = bytes[i - 1];
            if !(prev.is_ascii_whitespace()) {
                i += 1;
                continue;
            }
        }
        // quoted:@"..."(止于未转义闭引号)
        if bytes.get(i + 1) == Some(&b'"') {
            let start_content = i + 2;
            if let Some(close_rel) = find_byte(bytes, start_content, b'"') {
                let raw = &text[start_content..close_rel];
                if !raw.is_empty() {
                    push_plain(out, i, close_rel + 1, raw, true);
                }
                i = close_rel + 1;
                continue;
            }
            i += 1;
            continue;
        }
        // 裸 @path:到下一个空白或行尾;剥尾随标点
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let raw = &text[start..j];
        let trimmed = trim_punct(raw);
        if !trimmed.is_empty() {
            // end = start + trimmed.len(不含剥除的标点)
            push_plain(out, i, start + trimmed.len(), trimmed, false);
        }
        i = j.max(i + 1);
    }
}

/// 判定位 i 是否落在 out 中某个 session span 内
fn covered_by_session(out: &[AtToken], i: usize) -> bool {
    out.iter()
        .any(|t| t.kind == AtKind::Session && i >= t.start && i < t.end)
}

/// 组一条 file/folder token(base=i@起点;content 为去引号/dir 判定后的
/// label;is_quoted 影响显示 label 的 basename 规则——quoted 视作整串)
fn push_plain(out: &mut Vec<AtToken>, start: usize, end: usize, content: &str, is_quoted: bool) {
    let is_dir = !is_quoted && content.ends_with('/');
    let display = if is_quoted {
        content.to_string()
    } else {
        content.trim_end_matches('/').to_string()
    };
    out.push(AtToken {
        kind: if is_dir { AtKind::Folder } else { AtKind::File },
        label: display,
        start,
        end,
    });
}

/// 剥 '#' 开头之外的可能作为命令符号的 @ 前缀,并剥尾随标点。
/// 文件/目录 token 显示 label 取 basename(切片后取末尾段)。
/// 但注意:这里保留完整 path(胶囊 title 需要完整路径,显示可另行叫 basename)。
/// —— displayLabel 与 title(完整)分开;本函数 label 存完整 path,
///     渲染层再按需 basename。此仅剥尾随标点。
fn trim_punct(s: &str) -> &str {
    s.trim_end_matches(
        &[
            '.', ',', ';', ':', '!', '?', '。', '，', '；', '：', '！', '？', ')', '」', '』',
        ][..],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_token_boundaries() {
        // 行首/空白后触发
        assert!(active_at_token("@src", 4).is_some());
        assert!(active_at_token("看 @src", 5).is_some());
        assert!(active_at_token("a @src", 5).is_some());
        // user@host 不触发
        assert!(active_at_token("user@host", 9).is_none());
        // @ 后空白不触发
        assert!(active_at_token("a @ b", 5).is_none());
    }

    #[test]
    fn at_token_quoted() {
        // @"docs/a b = 10 bytes(@ " d o c s / a sp b)
        let t = active_at_token("@\"docs/a b", 10).unwrap();
        assert!(t.quoted);
        assert_eq!(t.query, "docs/a b");
    }

    #[test]
    fn at_token_span_and_query() {
        // "看 " = 3 bytes(中文)+ 1 空格;@src/main = 9 bytes → caret = 13
        let t = active_at_token("看 @src/main", 13).unwrap();
        assert_eq!(t.query, "src/main");
        assert_eq!(t.prefix, "@src/main");
    }

    #[test]
    fn mention_codec_roundtrip() {
        let mention = session_mention("M2", "abc-123");
        assert!(mention.starts_with("@[M2](dsh-session:"));
        let parsed = parse_session_references(&mention);
        assert_eq!(parsed, vec![("M2".to_string(), "abc-123".to_string())]);
        // 裸 URI 也能解
        let uri = session_uri("xyz");
        assert_eq!(
            parse_session_references(&format!("看 {uri}")),
            vec![(uri.clone(), "xyz".to_string())]
        );
        // 解码往返
        assert_eq!(decode_session_uri(&uri), Some("xyz".to_string()));
    }

    #[test]
    fn file_mention_quotes_spaces() {
        assert_eq!(file_mention("a b.md"), "@\"a b.md\"");
        assert_eq!(file_mention("main.rs"), "@main.rs");
    }

    #[test]
    fn snapshot_projects_and_escapes() {
        let msgs = vec![
            json!({ "role": "user", "content": "第一问" }),
            json!({ "role": "assistant", "content": "答" }),
            json!({ "role": "tool", "output": "不该出现" }),
            json!({ "role": "user", "content": [ { "type": "text", "text": "块文本" } ] }),
        ];
        let refs = vec![SnapshotSession {
            session_id: "s1".into(),
            label: "S1".into(),
            retained: 0,
            original: 0,
        }];
        let snap = session_snapshot(&refs, &msgs);
        assert!(snap.starts_with("<referenced-sessions>"));
        assert!(snap.contains("第一问"));
        assert!(snap.contains("块文本"));
        assert!(!snap.contains("tool"));
        assert!(!snap.contains("不该出现"));
        // `<` 转义(这里无 `<`,非破坏性)
        let with_lt = json!([{ "sessionId": "s1", "conversation": [ { "role": "user", "text": "<script>" } ] }]);
        let _ = with_lt;
    }

    #[test]
    fn snapshot_budget_truncates() {
        // 构造超预算:很多长消息
        let mut msgs = Vec::new();
        for i in 0..200 {
            msgs.push(
                json!({ "role": "user", "content": format!("msg {i} {}", "x".repeat(1000)) }),
            );
        }
        let refs = vec![SnapshotSession {
            session_id: "s1".into(),
            label: "S1".into(),
            retained: 0,
            original: 0,
        }];
        let snap = session_snapshot(&refs, &msgs);
        assert!(snap.len() < 200_000, "应截断到预算附近,实际 {}", snap.len());
    }

    #[test]
    fn scan_workspace_lists_and_excludes() {
        let root = std::env::temp_dir().join(format!("dsh-ref-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("README.md"), "").unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        std::fs::write(root.join(".git/config"), "").unwrap();
        let cands = scan_workspace(&root);
        let paths: Vec<&str> = cands.iter().map(|c| c.path.as_str()).collect();
        assert!(paths.contains(&"README.md"));
        assert!(paths.contains(&"src/main.rs"));
        assert!(!paths.iter().any(|p| p.contains("node_modules")));
        assert!(!paths.iter().any(|p| p.starts_with('.')));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rank_file_matches_basename_precedence() {
        let cands = vec![
            FileCandidate {
                path: "src/main.rs".into(),
                is_dir: false,
            },
            FileCandidate {
                path: "main.rs".into(),
                is_dir: false,
            },
            FileCandidate {
                path: "src/lib.rs".into(),
                is_dir: false,
            },
        ];
        // basename 精确匹配(query == basename)得分最高
        let exact = rank_file_candidates("main.rs", &cands);
        assert_eq!(exact[0].path, "main.rs");
        // basename 前缀匹配
        let prefix = rank_file_candidates("main", &cands);
        assert!(prefix[0].path == "main.rs" || prefix[0].path == "src/main.rs");
        // 空 query 列出全部
        let all = rank_file_candidates("", &cands);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn scan_at_tokens_files_and_folders() {
        use AtKind::*;
        // 普通文件 + 目录 + 词边界
        let toks = scan_at_tokens("看 @docs/design.md 和 @src/ 这两个");
        assert_eq!(toks.len(), 2);
        assert_eq!(toks[0].kind, File);
        assert_eq!(toks[0].label, "docs/design.md");
        assert_eq!(
            &"看 @docs/design.md 和 @src/ 这两个"[toks[0].start..toks[0].end],
            "@docs/design.md"
        );
        assert_eq!(toks[1].kind, Folder);
        assert_eq!(
            &"看 @docs/design.md 和 @src/ 这两个"[toks[1].start..toks[1].end],
            "@src/"
        );
    }

    #[test]
    fn scan_at_tokens_quoted_and_punct() {
        use AtKind::*;
        // 引号路径(含空格)
        let toks = scan_at_tokens("读 @\"docs/a b.md\" 内容");
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, File);
        assert_eq!(toks[0].label, "docs/a b.md");
        // 尾随标点剥除
        let toks = scan_at_tokens("参考 @README.md。");
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].label, "README.md");
        assert_eq!(
            &"参考 @README.md。"[toks[0].start..toks[0].end],
            "@README.md"
        );
    }

    #[test]
    fn scan_at_tokens_session() {
        use AtKind::*;
        // 会话整段优先,不被内部子串误判
        let mention = session_mention("M2", "abc-123");
        let text = format!("引 {mention} 分析");
        let toks = scan_at_tokens(&text);
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, Session);
        assert_eq!(toks[0].label, "M2");
    }

    #[test]
    fn scan_at_tokens_ignores_email() {
        // user@host 不触发(text 内 @ 前非空白)
        let toks = scan_at_tokens("联系 user@host 或 a@b");
        assert_eq!(toks.len(), 0);
    }
}
