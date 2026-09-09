//! 文件三件套:file_read / file_edit / file_search。
//!
//! 能力束:读 = 全盘只读;写 = 构造时给定的根(与 BashTool 同一可写根,
//! 越界 fail-closed 拒绝)。直接 Rust IO,不经沙箱子进程——路径约束由
//! 本工具在进程内强制(读写都是显式路径,无 shell 注入面)。

use std::path::{Path, PathBuf};

use dsh_agent_loop::{
    FileDiff, FileMatches, ToolCallRequest, ToolOutput, ToolPort, ToolView, ViewLine,
};
use serde_json::{Value, json};

/// 文件工具集:读 / str_replace 编辑 / glob+grep 检索
pub struct FileTools {
    /// 相对路径解析基点,同时是唯一可写根
    pub root: PathBuf,
    /// 只读模式(file_edit 拒绝;访问模式 read-only;静态兜底)
    pub readonly: bool,
    /// 会话权限模式动态源(execute 时解析;缺省 = 静态 readonly)
    pub mode_source: Option<crate::ModeSource>,
    /// file_read 单文件字节上限(超出只读前 N 字节)
    pub max_read_bytes: u64,
    /// file_search 结果条数上限(超出截断并注明)
    pub max_results: usize,
    /// file_search 单文件内容扫描上限(更大的文件跳过)
    pub max_scan_bytes: u64,
    /// file_search 目录树访问条目上限(遍历熔断)
    pub max_walk_entries: usize,
}

impl FileTools {
    /// 以可写根构建(默认上限:读 2MB / 结果 200 条 / 扫描 1MB / 遍历 2 万条目)
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            readonly: false,
            mode_source: None,
            max_read_bytes: 2 * 1024 * 1024,
            max_results: 200,
            max_scan_bytes: 1024 * 1024,
            max_walk_entries: 20_000,
        }
    }

    /// 只读模式(file_edit 拒绝)
    pub fn with_readonly(mut self) -> Self {
        self.readonly = true;
        self
    }

    /// 挂动态模式源(会话日志 fold;权限切换落档即生效)
    pub fn with_mode_source(mut self, source: crate::ModeSource) -> Self {
        self.mode_source = Some(source);
        self
    }

    /// 执行时解析只读态:动态源优先,静态 readonly 兜底
    fn is_readonly(&self) -> bool {
        self.mode_source
            .as_ref()
            .map(|f| f() == dsh_sandbox::SandboxMode::ReadOnly)
            .unwrap_or(self.readonly)
    }

    /// 相对路径以 root 为基;绝对路径原样
    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        }
    }

    /// 写入前的可写根校验:目标父目录必须落在 root 内(fail-closed)
    fn writable_target(&self, path: &str) -> Result<PathBuf, String> {
        let target = self.resolve(path);
        let file_name = target
            .file_name()
            .ok_or_else(|| format!("invalid target path: {path}"))?
            .to_owned();
        let parent = target.parent().unwrap_or(Path::new("."));
        let canonical_parent = parent
            .canonicalize()
            .map_err(|e| format!("path not accessible: {path} ({e})"))?;
        let root = self
            .root
            .canonicalize()
            .map_err(|e| format!("workspace root not accessible ({e})"))?;
        if !canonical_parent.starts_with(&root) {
            return Err(format!(
                "write outside workspace root denied: {path} (root: {})",
                self.root.display()
            ));
        }
        Ok(canonical_parent.join(file_name))
    }

    /// file_read:行号输出;offset/limit(1 起)分页;超限注明。
    /// Read 视图与输出文本同循环构造(数据在手,零额外 IO)
    async fn file_read(&self, args: &Value) -> ToolOutput {
        let fail = |msg: String| ToolOutput {
            output: msg,
            success: false,
            ..Default::default()
        };
        let Some(path) = args["path"].as_str() else {
            return fail("file_read requires arguments.path (string)".into());
        };
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().unwrap_or(2000) as usize;

        let target = self.resolve(path);
        let bytes = match tokio::fs::read(&target).await {
            Ok(b) => b,
            Err(e) => return fail(format!("read failed: {path} ({e})")),
        };
        let read_capped = bytes.len() as u64 > self.max_read_bytes;
        let slice = if read_capped {
            &bytes[..self.max_read_bytes as usize]
        } else {
            &bytes[..]
        };
        let text = String::from_utf8_lossy(slice);
        let all_lines: Vec<&str> = text.lines().collect();
        let total = all_lines.len();
        let start = (offset - 1).min(total);
        let end = (start + limit).min(total);
        let mut out = String::new();
        let mut view_lines = Vec::with_capacity(end - start);
        for (i, line) in all_lines[start..end].iter().enumerate() {
            out.push_str(&format!("{}\t{}\n", start + i + 1, line));
            view_lines.push(ViewLine {
                number: (start + i + 1) as u64,
                text: (*line).to_string(),
            });
        }
        let shown = end - start;
        if shown < total || read_capped {
            out.push_str(&format!(
                "(showing lines {}..{} of {total}{})",
                start + 1,
                end,
                if read_capped {
                    ", file truncated at 2MB"
                } else {
                    ""
                },
            ));
        }
        ToolOutput {
            output: out.trim_end().to_string(),
            success: true,
            view: Some(ToolView::Read {
                path: path.to_string(),
                offset: (start + 1) as u64,
                lines: view_lines,
                total_lines: total as u64,
                lang: lang_for(path),
            }),
        }
    }

    /// file_edit:str_replace 语义——old_text 全文件唯一匹配才替换
    async fn file_edit(&self, args: &Value) -> ToolOutput {
        let fail = |msg: String| ToolOutput {
            output: msg,
            success: false,
            ..Default::default()
        };
        if self.is_readonly() {
            return fail("file_edit denied: session is in read-only mode".into());
        }
        let Some(path) = args["path"].as_str() else {
            return fail("file_edit requires arguments.path (string)".into());
        };
        let Some(old_text) = args["old_text"].as_str() else {
            return fail("file_edit requires arguments.old_text (string)".into());
        };
        let Some(new_text) = args["new_text"].as_str() else {
            return fail("file_edit requires arguments.new_text (string)".into());
        };

        let target = match self.writable_target(path) {
            Ok(t) => t,
            Err(e) => return fail(e),
        };
        let content = match tokio::fs::read_to_string(&target).await {
            Ok(c) => c,
            Err(e) => return fail(format!("read failed: {path} ({e})")),
        };
        // 非重叠匹配计数:唯一才允许替换(模型给的锚文本歧义即拒绝)
        let mut count = 0usize;
        let mut first: Option<usize> = None;
        let mut from = 0usize;
        while let Some(found) = content[from..].find(old_text) {
            if count == 0 {
                first = Some(from + found);
            }
            count += 1;
            from = from + found + old_text.len();
        }
        match count {
            0 => return fail(format!("old_text not found in {path}")),
            1 => {}
            n => {
                return fail(format!(
                    "old_text matches {n} times in {path}; it must match exactly once"
                ));
            }
        }
        // count == 1:重组替换
        let at = first.unwrap_or_default();
        let mut edited = String::with_capacity(content.len() - old_text.len() + new_text.len());
        edited.push_str(&content[..at]);
        edited.push_str(new_text);
        edited.push_str(&content[at + old_text.len()..]);
        if let Err(e) = tokio::fs::write(&target, &edited).await {
            return fail(format!("write failed: {path} ({e})"));
        }
        ToolOutput {
            output: format!("edited {path} (1 replacement)"),
            success: true,
            // 编辑成功后 old/new 即最终事实(result 侧与 call 侧意图同源;
            // RS 语义为全量意图 diff,无应用后 hunk 粒度)
            view: Some(edit_diff_view(path, old_text, new_text)),
        }
    }

    /// file_search:glob 匹配 + 内容子串检索(ripgrep 引擎:`ignore`
    /// 遍历尊重 .gitignore/隐藏文件,`grep-searcher` 做行搜索),上限截断。
    /// 结构化分组与截断/全量计数在文本扁平化**之前**成形(SearchMatches/
    /// SearchPaths 视图),文本与视图同源
    fn file_search(&self, args: &Value) -> ToolOutput {
        let fail = |msg: String| ToolOutput {
            output: msg,
            success: false,
            ..Default::default()
        };
        let base = match args["path"].as_str() {
            Some(p) => self.resolve(p),
            None => self.root.clone(),
        };
        let glob = args["glob"].as_str();
        let content = args["content"].as_str();
        if glob.is_none() && content.is_none() {
            return fail("file_search requires arguments.glob or arguments.content".into());
        }

        // glob 白名单(override:只有匹配的文件被遍历;`**` 支持跨目录)
        let overrides = glob.and_then(|g| {
            ignore::overrides::OverrideBuilder::new(&base)
                .add(g)
                .ok()
                .and_then(|b| b.build().ok())
        });
        // 内容检索:子串按字面匹配(正则元字符转义)
        let matcher = content.and_then(|needle| {
            grep_regex::RegexMatcherBuilder::new()
                .build(&regex_escape(needle))
                .ok()
        });

        // 结构化收集(保留页 ≤ max_results;total = 全量计数,不提前断流)
        let mut hits: Vec<SearchHit> = Vec::new();
        let mut paths: Vec<String> = Vec::new();
        let mut total: u64 = 0;
        let mut truncated = false;
        let mut builder = ignore::WalkBuilder::new(&base);
        builder.sort_by_file_path(|a, b| a.cmp(b));
        // .gitignore 在非 git 目录同样生效(独立 workspace 常见形态)
        builder.require_git(false);
        // glob 模式含隐藏段(如 **/.git/...)时放行隐藏遍历——
        // 否则 ignore crate 在目录级剪枝,显式 glob 也匹配不到隐藏路径
        let glob_has_hidden = glob.is_some_and(|g| {
            g.split('/')
                .any(|seg| seg.starts_with('.') && seg.len() > 1)
        });
        builder.hidden(!glob_has_hidden);
        if let Some(ov) = overrides {
            builder.overrides(ov);
        }
        let walker = builder.build();
        for (entries_visited, entry) in walker.enumerate() {
            if entries_visited > self.max_walk_entries {
                truncated = true;
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&base)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| entry.path().to_string_lossy().into_owned());

            match content {
                None => {
                    // glob-only:列出路径(全量计数,保留前 cap)
                    total += 1;
                    if paths.len() < self.max_results {
                        paths.push(rel);
                    }
                }
                Some(_) => {
                    let Some(m) = matcher.as_ref() else {
                        break; // 内容正则构建失败:转义后仍非法,空结果收尾
                    };
                    let Ok(meta) = entry.metadata() else {
                        continue;
                    };
                    if meta.len() > self.max_scan_bytes {
                        continue;
                    }
                    let mut sink = CollectSink {
                        rel: rel.clone(),
                        out: &mut hits,
                        total: &mut total,
                        cap: self.max_results,
                    };
                    let mut searcher = grep_searcher::SearcherBuilder::new()
                        .line_number(true)
                        .binary_detection(grep_searcher::BinaryDetection::quit(0))
                        .build();
                    let _ = searcher.search_path(m, entry.path(), &mut sink);
                }
            }
        }
        truncated |= total > (hits.len() + paths.len()) as u64;

        // 模型面文本与视图同源:文本自结构化保留页扁平化
        let (output, view) = if content.is_some() {
            let text = hits
                .iter()
                .map(|h| format!("{}:{}:{}", h.rel, h.line, h.text))
                .collect::<Vec<_>>()
                .join("\n");
            let mut out = text;
            if truncated {
                out.push_str(&format!(
                    "\n(truncated at {} results or walk cap {})",
                    hits.len(),
                    self.max_walk_entries
                ));
            }
            // 分组:首见文件序(walker 按路径排序,组内即输出序)
            let mut files: Vec<FileMatches> = Vec::new();
            for h in &hits {
                match files.last_mut() {
                    Some(g) if g.path == h.rel => g.matches.push(ViewLine {
                        number: h.line,
                        text: h.text.clone(),
                    }),
                    _ => files.push(FileMatches {
                        path: h.rel.clone(),
                        matches: vec![ViewLine {
                            number: h.line,
                            text: h.text.clone(),
                        }],
                    }),
                }
            }
            (
                out,
                ToolView::SearchMatches {
                    files,
                    truncated,
                    total,
                },
            )
        } else {
            let mut out = paths.join("\n");
            if truncated {
                out.push_str(&format!(
                    "\n(truncated at {} results or walk cap {})",
                    paths.len(),
                    self.max_walk_entries
                ));
            }
            (
                out,
                ToolView::SearchPaths {
                    paths,
                    truncated,
                    total,
                },
            )
        };
        ToolOutput {
            output,
            success: true,
            view: Some(view),
        }
    }
}

/// 一条内容匹配(结构化保留页;扁平化之前)
struct SearchHit {
    rel: String,
    line: u64,
    text: String,
}

/// 扩展名 → 语法高亮语言提示(read 视图;未知扩展 → None = 纯文本)
fn lang_for(path: &str) -> Option<String> {
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    let lang = match ext.as_str() {
        "rs" => "rust",
        "py" => "python",
        "ts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "json" => "json",
        "md" | "markdown" => "markdown",
        "css" => "css",
        "scss" => "scss",
        "html" => "html",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "sh" | "bash" | "zsh" => "shell",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "rb" => "ruby",
        "sql" => "sql",
        "xml" => "xml",
        other => other,
    };
    Some(lang.to_string())
}

/// file_edit 的意图 diff 视图(call/result 两侧同源)
fn edit_diff_view(path: &str, old_text: &str, new_text: &str) -> ToolView {
    ToolView::Diff {
        diffs: vec![FileDiff {
            path: path.to_string(),
            old_text: Some(old_text.to_string()),
            new_text: new_text.to_string(),
        }],
    }
}

/// 内容子串的正则转义(字面匹配;grep-regex 引擎)
fn regex_escape(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    for c in literal.chars() {
        if "\\.^$|?*+()[]{}#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// grep-searcher 收集 sink:全量计数 + 保留前 cap 条(文本 200 字符截断)。
/// 不再提前断流——search 视图的 `total` 语义 = 截断前全量匹配数
/// (RetainedItems.seen 同构语义;UI 据此显示「显示 X / 共 N …」)
struct CollectSink<'a> {
    rel: String,
    out: &'a mut Vec<SearchHit>,
    total: &'a mut u64,
    cap: usize,
}

impl grep_searcher::Sink for CollectSink<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        *self.total += 1;
        if self.out.len() < self.cap {
            let text = String::from_utf8_lossy(mat.bytes());
            let brief: String = text.trim().chars().take(200).collect();
            self.out.push(SearchHit {
                rel: self.rel.clone(),
                line: mat.line_number().unwrap_or(0),
                text: brief,
            });
        }
        Ok(true)
    }
}

/// OpenAI 兼容 wire 上 arguments 是 JSON 编码字符串;本地夹具是对象。
/// 两种形态都接受(与 BashTool 同一容忍策略)
fn parse_arguments(args: &Value) -> Value {
    if let Some(s) = args.as_str() {
        serde_json::from_str(s).unwrap_or(json!({}))
    } else {
        args.clone()
    }
}

impl ToolPort for FileTools {
    fn specs(&self) -> Vec<Value> {
        vec![
            json!({
                "type": "function",
                "function": {
                    "name": "file_read",
                    "description": "Read a text file with line numbers. Relative paths resolve against the workspace root. Default: first 2000 lines.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "File path (absolute or workspace-relative)" },
                            "offset": { "type": "integer", "description": "First line to return (1-based, default 1)" },
                            "limit": { "type": "integer", "description": "Max lines to return (default 2000)" }
                        },
                        "required": ["path"],
                    },
                },
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "file_edit",
                    "description": "Edit a file by exact string replacement. old_text must match exactly once in the file; zero or multiple matches are rejected. Writes are confined to the workspace root.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "File path (workspace-relative or absolute)" },
                            "old_text": { "type": "string", "description": "Exact text to replace (must be unique in the file)" },
                            "new_text": { "type": "string", "description": "Replacement text" }
                        },
                        "required": ["path", "old_text", "new_text"],
                    },
                },
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "file_search",
                    "description": "Search files by glob and/or content substring under a directory (default: workspace root). Respects .gitignore and skips hidden files (ripgrep engine). Returns matching paths (glob only) or path:line:text entries (content given). Binary and oversized files are skipped.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Base directory (default: workspace root)" },
                            "glob": { "type": "string", "description": "Glob pattern against the relative path (* and ** supported)" },
                            "content": { "type": "string", "description": "Substring to match within file contents" }
                        },
                    },
                },
            }),
        ]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        let arguments = parse_arguments(&call.arguments);
        match call.name.as_str() {
            "file_read" => self.file_read(&arguments).await,
            "file_edit" => self.file_edit(&arguments).await,
            "file_search" => self.file_search(&arguments),
            _ => ToolOutput {
                output: format!("unknown tool: {}", call.name),
                success: false,
                ..Default::default()
            },
        }
    }

    /// call 侧意图:file_edit 的 old/new diff(运行中即显,presentCall
    /// 意图 diff)。read/search 的意图只在 result 侧成形
    fn present_call(&self, call: &ToolCallRequest) -> Option<ToolView> {
        if call.name != "file_edit" {
            return None;
        }
        let args = parse_arguments(&call.arguments);
        let path = args["path"].as_str()?;
        let old_text = args["old_text"].as_str()?;
        let new_text = args["new_text"].as_str()?;
        Some(edit_diff_view(path, old_text, new_text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsh-file-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn read_lines_offset_limit_and_numbering() {
        let root = temp_root("read");
        std::fs::write(root.join("f.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let tools = FileTools::new(&root);
        let out = tools
            .file_read(&json!({ "path": "f.txt", "offset": 2, "limit": 2 }))
            .await;
        assert!(out.success);
        assert_eq!(out.output, "2\tl2\n3\tl3\n(showing lines 2..3 of 5)");
    }

    /// 视图与文本同源一致:同一 file_read 的 view 字段与 output 文本互相印证
    #[tokio::test]
    async fn read_view_consistent_with_text() {
        let root = temp_root("read-view");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn a() {}\nfn b() {}\nfn c() {}\n",
        )
        .unwrap();
        let tools = FileTools::new(&root);
        let out = tools
            .file_read(&json!({ "path": "src/main.rs", "offset": 2, "limit": 5 }))
            .await;
        assert!(out.success);
        let Some(dsh_agent_loop::ToolView::Read {
            path,
            offset,
            lines,
            total_lines,
            lang,
        }) = out.view
        else {
            panic!("read 视图应在场");
        };
        assert_eq!(path, "src/main.rs");
        assert_eq!(offset, 2);
        assert_eq!(total_lines, 3);
        assert_eq!(lang.as_deref(), Some("rust"));
        // 同源:文本行 = 视图行投影(行号 + 文本;排除窗口注记尾行)
        let text_lines: Vec<&str> = out
            .output
            .lines()
            .filter(|l| !l.starts_with("(showing"))
            .collect();
        let view_rows: Vec<String> = lines
            .iter()
            .map(|l| format!("{}\t{}", l.number, l.text))
            .collect();
        assert_eq!(text_lines, view_rows);
        // 窗口语义:窗口小于总数
        assert_eq!(lines.len(), 2);

        // 空窗口(offset 超尾):视图保留 offset、lines 空
        let beyond = tools
            .file_read(&json!({ "path": "src/main.rs", "offset": 99 }))
            .await;
        assert!(beyond.success);
        let Some(dsh_agent_loop::ToolView::Read { offset, lines, .. }) = beyond.view else {
            panic!("read 视图应在场");
        };
        assert_eq!(offset, 4);
        assert!(lines.is_empty());
    }

    /// file_edit 意图与结果视图:call 侧自 args、result 侧成功即事实
    #[tokio::test]
    async fn edit_present_call_and_result_view() {
        let root = temp_root("edit-view");
        std::fs::write(root.join("a.txt"), "alpha beta\n").unwrap();
        let mut tools = FileTools::new(&root);

        let call = ToolCallRequest {
            name: "file_edit".into(),
            arguments: json!({ "path": "a.txt", "old_text": "beta", "new_text": "BETA" }),
        };
        let intent = tools.present_call(&call).expect("call 侧意图 diff");
        assert!(matches!(&intent, dsh_agent_loop::ToolView::Diff { diffs }
            if diffs.len() == 1 && diffs[0].path == "a.txt"
                && diffs[0].old_text.as_deref() == Some("beta")
                && diffs[0].new_text == "BETA"));

        // read/search 无 call 侧意图(意图只在 result 侧成形)
        assert!(
            tools
                .present_call(&ToolCallRequest {
                    name: "file_read".into(),
                    arguments: json!({ "path": "a.txt" }),
                })
                .is_none()
        );

        let out = tools.execute(&call).await;
        assert!(out.success, "{}", out.output);
        assert_eq!(out.view, Some(intent), "result 侧 = 已应用事实(与意图同源)");

        // 编辑失败(多次匹配):无 diff 视图(错误走通用卡)
        std::fs::write(root.join("b.txt"), "x x x\n").unwrap();
        let failed = tools
            .execute(&ToolCallRequest {
                name: "file_edit".into(),
                arguments: json!({ "path": "b.txt", "old_text": "x", "new_text": "y" }),
            })
            .await;
        assert!(!failed.success);
        assert!(failed.view.is_none());
    }

    /// 搜索视图与文本同源:matches 分组/paths 列表 + 截断全量计数
    #[tokio::test]
    async fn search_views_consistent_with_text() {
        let root = temp_root("search-view");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "needle one\nneedle two\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "needle three\nother\n").unwrap();

        let tools = FileTools::new(&root);
        let out = tools.file_search(&json!({ "content": "needle" }));
        assert!(out.success);
        let Some(dsh_agent_loop::ToolView::SearchMatches {
            files,
            truncated,
            total,
        }) = out.view
        else {
            panic!("matches 视图应在场");
        };
        assert!(!truncated);
        assert_eq!(total, 3);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].matches.len(), 2);
        // 同源:文本行数 = 视图匹配数,行文一致
        let text_rows: Vec<&str> = out.output.lines().collect();
        let view_rows: Vec<String> = files
            .iter()
            .flat_map(|f| {
                f.matches
                    .iter()
                    .map(move |m| format!("{}:{}:{}", f.path, m.number, m.text))
            })
            .collect();
        assert_eq!(text_rows, view_rows);

        // glob-only:paths 形态
        let paths_out = tools.file_search(&json!({ "glob": "**/*.rs" }));
        assert!(paths_out.success);
        let Some(dsh_agent_loop::ToolView::SearchPaths {
            paths,
            truncated,
            total,
        }) = paths_out.view
        else {
            panic!("paths 视图应在场");
        };
        assert!(!truncated);
        assert_eq!(total, 2);
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(paths_out.output, "src/a.rs\nsrc/b.rs");
    }

    /// 截断:全量计数继续、保留页封顶,truncated 在场(不止于保留数)
    #[tokio::test]
    async fn search_truncation_counts_total() {
        let root = temp_root("search-cap");
        for i in 0..5 {
            std::fs::write(root.join(format!("f{i}.txt")), "needle\nneedle\n").unwrap();
        }
        let mut tools = FileTools::new(&root);
        tools.max_results = 3;
        let out = tools.file_search(&json!({ "content": "needle" }));
        assert!(out.success);
        let Some(dsh_agent_loop::ToolView::SearchMatches {
            files,
            truncated,
            total,
        }) = out.view
        else {
            panic!("matches 视图应在场");
        };
        assert!(truncated, "10 匹配 > 保留 3,应截断");
        assert_eq!(total, 10, "total = 截断前全量计数");
        assert_eq!(
            files.iter().map(|f| f.matches.len()).sum::<usize>(),
            3,
            "保留页 = 3"
        );
        // 文本同源:保留页行数 + 截断尾注
        assert!(out.output.contains("truncated at 3 results"));
        assert_eq!(
            out.output.lines().filter(|l| l.contains("needle")).count(),
            3
        );
    }

    #[tokio::test]
    async fn edit_requires_unique_match() {
        let root = temp_root("edit");
        std::fs::write(root.join("a.txt"), "alpha beta alpha\n").unwrap();
        let tools = FileTools::new(&root);

        let multi = tools
            .file_edit(&json!({ "path": "a.txt", "old_text": "alpha", "new_text": "x" }))
            .await;
        assert!(!multi.success);
        assert!(multi.output.contains("2 times"));

        let none = tools
            .file_edit(&json!({ "path": "a.txt", "old_text": "gamma", "new_text": "x" }))
            .await;
        assert!(!none.success);
        assert!(none.output.contains("not found"));

        let ok = tools
            .file_edit(&json!({ "path": "a.txt", "old_text": "beta", "new_text": "BETA" }))
            .await;
        assert!(ok.success);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "alpha BETA alpha\n"
        );
    }

    #[tokio::test]
    async fn edit_denies_write_outside_root() {
        let root = temp_root("edit-deny");
        let outside = std::env::temp_dir().join(format!(
            "dsh-file-outside-{}-{}.txt",
            std::process::id(),
            root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&outside, "x").unwrap();
        let tools = FileTools::new(&root);
        let out = tools
            .file_edit(&json!({
                "path": outside.display().to_string(),
                "old_text": "x", "new_text": "y"
            }))
            .await;
        assert!(!out.success);
        assert!(out.output.contains("denied"));
    }

    #[tokio::test]
    async fn edit_accepts_string_arguments_from_wire() {
        let root = temp_root("edit-wire");
        std::fs::write(root.join("a.txt"), "one two\n").unwrap();
        let mut tools = FileTools::new(&root);
        let call = ToolCallRequest {
            name: "file_edit".into(),
            arguments: json!(r#"{ "path": "a.txt", "old_text": "two", "new_text": "2" }"#),
        };
        let out = tools.execute(&call).await;
        assert!(out.success, "{}", out.output);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one 2\n"
        );
    }

    #[tokio::test]
    async fn search_glob_and_content() {
        let root = temp_root("search");
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    todo()\n}\n").unwrap();
        std::fs::write(root.join("src/nested/util.rs"), "// todo helper\n").unwrap();
        std::fs::write(root.join("notes.txt"), "nothing\n").unwrap();

        let tools = FileTools::new(&root);
        let glob_only = tools.file_search(&json!({ "glob": "**/*.rs" }));
        assert!(glob_only.success);
        let mut lines: Vec<&str> = glob_only.output.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, ["src/main.rs", "src/nested/util.rs"]);

        let with_content = tools.file_search(&json!({ "glob": "**/*.rs", "content": "todo" }));
        assert!(with_content.success);
        assert_eq!(
            with_content.output,
            "src/main.rs:2:todo()\nsrc/nested/util.rs:1:// todo helper"
        );

        let no_args = tools.file_search(&json!({}));
        assert!(!no_args.success);
    }

    #[test]
    fn search_glob_reaches_hidden_dirs_when_pattern_mentions_them() {
        let root = temp_root("search-hidden");
        std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        std::fs::write(root.join(".git/refs/heads/feature"), "").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn x() {}\n").unwrap();

        let tools = FileTools::new(&root);
        // 显式 .git 段:放行隐藏遍历,能匹配
        let hit = tools.file_search(&json!({ "glob": "**/.git/refs/heads/*" }));
        assert!(hit.success);
        assert_eq!(hit.output, ".git/refs/heads/feature");
        // 无隐藏段的 glob:仍跳过隐藏(默认行为不变)
        let plain = tools.file_search(&json!({ "glob": "**/*.rs" }));
        assert!(plain.success);
        assert_eq!(plain.output, "src/lib.rs");
    }

    #[test]
    fn search_respects_gitignore() {
        let root = temp_root("gitignore");
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::fs::write(root.join(".gitignore"), "vendor/\n*.log\n").unwrap();
        std::fs::write(root.join("vendor/secret.rs"), "needle here\n").unwrap();
        std::fs::write(root.join("debug.log"), "needle too\n").unwrap();
        std::fs::write(root.join("app.rs"), "needle kept\n").unwrap();

        let tools = FileTools::new(&root);
        let out = tools.file_search(&json!({ "content": "needle" }));
        assert!(out.success);
        assert_eq!(
            out.output, "app.rs:1:needle kept",
            "gitignore 条目不得被检索"
        );
    }

    #[tokio::test]
    async fn search_respects_result_cap() {
        let root = temp_root("cap");
        for i in 0..5 {
            std::fs::write(root.join(format!("f{i}.txt")), "needle\n").unwrap();
        }
        let mut tools = FileTools::new(&root);
        tools.max_results = 3;
        let out = tools.file_search(&json!({ "content": "needle" }));
        assert!(out.success);
        assert!(out.output.contains("truncated at 3 results"));
        assert_eq!(
            out.output.lines().filter(|l| l.contains("needle")).count(),
            3
        );
    }
}
