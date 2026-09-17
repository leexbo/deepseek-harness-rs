//! 文件类型分类与文档预览渲染器注册表(纯函数,无状态)。
//!
//! 两个职责:
//! - **类型视觉**:文件名 → 单色图标 + 家族染色。gpui 的 SVG 渲染是
//!   alpha-mask 单色(`paint_svg` → `MonochromeSprite`),源项目的彩色
//!   渐变图标无法照搬;分类表(精确名/前缀/扩展名)照源移植,彩色以
//!   家族染色近似。色值见 [`crate::kits::theme::FILE_TYPE_TINT`]。
//! - **预览渲染器注册表**:文件名 → 候选渲染器序列(照源注册表:
//!   text→markdown→image→pdf→code 注册序,最长后缀优先,平长按注册
//!   序)。HTML 视觉渲染待 webview 拍板,`.html/.htm` 由 code 覆盖。

use gpui_kit::component::{Icon, IconName};

use crate::kits::icons::{DshIcon, fixed};

// ── 类型分类 ─────────────────────────────────────────────────

/// 文件类型家族(染色与图标_glyph_ 的粒度;非语言全集)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileClass {
    /// markdown(md/markdown/mdx;readme/changelog/contributing 裸名)
    Markdown,
    /// 位图/矢量图片(含 svg)
    Image,
    /// pdf
    Pdf,
    /// html/xhtml
    Html,
    /// css/scss/less
    Css,
    /// rust
    Rust,
    /// git 元数据(.gitignore 等)
    Git,
    /// json 系
    Json,
    /// 结构化配置(toml/yaml/ini;dockerfile/makefile)
    Config,
    /// env 系(.env/.env.*)
    Env,
    /// 锁文件(*.lock)
    Lock,
    /// shell 脚本
    Shell,
    /// python
    Python,
    /// js/ts 系
    JsTs,
    /// 其余代码语言(go/c/java/cs/kt/swift/php/rb/sql/lua…)
    Code,
    /// 压缩包
    Archive,
    /// 视频
    Video,
    /// 音频
    Audio,
    /// word 文档
    Word,
    /// excel 表格
    Excel,
    /// ppt 幻灯
    Ppt,
    /// 字体
    Font,
    /// 纯文本(txt/log)
    Text,
    /// 未分类
    Other,
}

/// 文件名小写(分类只看小写形态;`\` 归一为 `/` 后取 basename)
fn file_name_of(name: &str) -> String {
    let normalized = name.replace('\\', "/");
    let base = normalized.rsplit('/').next().unwrap_or_default();
    base.to_lowercase()
}

/// 精确文件名 → 家族(源 NAME_TYPES:readme/changelog/contributing;
/// git 元数据;makefile)
fn class_by_exact_name(name: &str) -> Option<FileClass> {
    let stem = name.strip_suffix(".txt").unwrap_or(name);
    match stem {
        "readme" | "changelog" | "contributing" => Some(FileClass::Markdown),
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".gitconfig" | ".gitkeep" => {
            Some(FileClass::Git)
        }
        ".env" => Some(FileClass::Env),
        "makefile" | "gnumakefile" => Some(FileClass::Config),
        _ => None,
    }
}

/// 前缀规则(源 FILE_NAME_PREFIX_TYPES:`.env.` / `dockerfile.`)
fn class_by_prefix(name: &str) -> Option<FileClass> {
    if name.starts_with(".env.") {
        Some(FileClass::Env)
    } else if name.starts_with("dockerfile.") {
        Some(FileClass::Config)
    } else {
        None
    }
}

fn class_by_extension(name: &str) -> FileClass {
    // 取「最后一个点后」的扩展(`.gitignore` 这类点文件已在精确名表
    // 命中;此处兜底 Other)
    let Some(dot) = name.rfind('.') else {
        return FileClass::Other;
    };
    match &name[dot + 1..] {
        "md" | "markdown" | "mdx" => FileClass::Markdown,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg" => FileClass::Image,
        "pdf" => FileClass::Pdf,
        "html" | "htm" | "xhtml" => FileClass::Html,
        "css" | "scss" | "less" => FileClass::Css,
        "rs" => FileClass::Rust,
        "json" | "jsonc" | "jsonl" | "ndjson" => FileClass::Json,
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "properties" => FileClass::Config,
        "lock" => FileClass::Lock,
        "sh" | "bash" | "zsh" | "fish" => FileClass::Shell,
        "py" | "pyw" | "pyi" => FileClass::Python,
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => FileClass::JsTs,
        "zip" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "tar" | "jar" => {
            FileClass::Archive
        }
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "flv" | "wmv" | "m4v" => FileClass::Video,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "wma" | "opus" => FileClass::Audio,
        "doc" | "docx" | "odt" | "pages" => FileClass::Word,
        "xls" | "xlsx" | "ods" | "numbers" => FileClass::Excel,
        "ppt" | "pptx" | "odp" => FileClass::Ppt,
        "ttf" | "otf" | "woff" | "woff2" | "eot" => FileClass::Font,
        "txt" | "log" => FileClass::Text,
        // 源 code 表其余语言 → 通用代码家族
        "go" | "java" | "c" | "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" | "cs" | "kt"
        | "kts" | "swift" | "php" | "rb" | "rake" | "gemspec" | "sql" | "lua" | "xml" | "xsd"
        | "xsl" | "xslt" => FileClass::Code,
        _ => FileClass::Other,
    }
}

/// 文件名 → 家族(精确名 → 前缀 → 扩展名,照源分类链)
pub fn file_class(name: &str) -> FileClass {
    let lower = file_name_of(name);
    class_by_exact_name(&lower)
        .or_else(|| class_by_prefix(&lower))
        .unwrap_or_else(|| class_by_extension(&lower))
}

/// 家族 → 定尺寸单色图标(字形;染色经调用点 `text_color(FILE_TYPE_TINT)`)
pub fn class_icon(class: FileClass, size: f32) -> Icon {
    use FileClass as F;
    match class {
        F::Markdown | F::Text => fixed(IconName::FileText, size),
        F::Image => fixed(DshIcon::FileImage, size),
        F::Pdf | F::Config | F::Word | F::Font | F::Other => fixed(DshIcon::FileType, size),
        F::Html | F::Css | F::Python | F::JsTs | F::Code => fixed(DshIcon::FileCode, size),
        F::Rust => fixed(DshIcon::FileCog, size),
        F::Git => fixed(DshIcon::FileDiff, size),
        F::Json => fixed(DshIcon::FileBraces, size),
        F::Env => fixed(DshIcon::FileKey, size),
        F::Lock => fixed(DshIcon::FileLock, size),
        F::Shell => fixed(DshIcon::FileTerminal, size),
        F::Archive => fixed(DshIcon::FileArchive, size),
        F::Video => fixed(DshIcon::FileVideoCamera, size),
        F::Audio => fixed(DshIcon::FileVolume, size),
        F::Excel => fixed(DshIcon::FileSpreadsheet, size),
        F::Ppt => fixed(DshIcon::FileChartColumn, size),
    }
}

// ── 预览渲染器注册表(照源)──────────────────────────────────

/// 文档预览渲染器(照源内建集;HTML 视觉渲染未实装)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DocRenderer {
    /// 纯文本(无扩展名声明,永不自动命中,仅菜单兜底)
    Text,
    /// Markdown
    Markdown,
    /// 图片(bytes-complete)
    Image,
    /// PDF(bytes-complete)
    Pdf,
    /// 代码(源 languages 全表)
    Code,
}

/// 渲染器内容装载方式(照源 DocumentLoadMode)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadMode {
    /// 行页分页读(5000 行/页)
    TextPages,
    /// 一次整读字节(bytes-complete)
    BytesComplete,
}

/// 注册序(平长后缀的决胜序,照源 apply 顺序:text→markdown→image→pdf→code)
const REGISTRY: [DocRenderer; 5] = [
    DocRenderer::Text,
    DocRenderer::Markdown,
    DocRenderer::Image,
    DocRenderer::Pdf,
    DocRenderer::Code,
];

impl DocRenderer {
    /// 稳定 id(桶记忆/测试 selector 用)
    pub fn id(self) -> &'static str {
        match self {
            DocRenderer::Text => "text",
            DocRenderer::Markdown => "markdown",
            DocRenderer::Image => "image",
            DocRenderer::Pdf => "pdf",
            DocRenderer::Code => "code",
        }
    }

    /// 菜单名(照源 zh locale 逐字)
    pub fn title(self) -> &'static str {
        match self {
            DocRenderer::Text => "纯文本",
            DocRenderer::Markdown => "Markdown",
            DocRenderer::Image => "图片",
            DocRenderer::Pdf => "PDF",
            DocRenderer::Code => "代码",
        }
    }

    /// 内容装载方式
    pub fn load_mode(self) -> LoadMode {
        match self {
            DocRenderer::Text | DocRenderer::Markdown | DocRenderer::Code => LoadMode::TextPages,
            DocRenderer::Image | DocRenderer::Pdf => LoadMode::BytesComplete,
        }
    }

    /// 是否消费换行开关(照源 wrap 声明)
    pub fn wrap(self) -> bool {
        matches!(self, DocRenderer::Text | DocRenderer::Code)
    }

    /// 声明的扩展名(无点、小写;text 无声明)
    fn extensions(self) -> &'static [&'static str] {
        match self {
            DocRenderer::Text => &[],
            DocRenderer::Markdown => &["md", "markdown"],
            DocRenderer::Image => &["png", "jpg", "jpeg", "gif", "webp"],
            DocRenderer::Pdf => &["pdf"],
            // 源 languages.ts 全表(照抄;svg 追加——gpui 解码面无 svg,
            // 按源码文本可读降级)
            DocRenderer::Code => &[
                "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "sh", "bash", "zsh", "json",
                "jsonc", "jsonl", "ndjson", "py", "pyw", "pyi", "rb", "rake", "gemspec", "go",
                "rs", "java", "c", "h", "cc", "cpp", "cxx", "hh", "hpp", "hxx", "cs", "kt", "kts",
                "swift", "php", "yaml", "yml", "toml", "ini", "md", "markdown", "mdx", "html",
                "htm", "xhtml", "css", "scss", "less", "sql", "xml", "xsd", "xsl", "xslt", "lua",
                "svg",
            ],
        }
    }
}

/// 源 UNVIEWABLE_BINARY_EXTENSIONS(浏览器/解码面不可呈现的二进制;
/// 空候选且命中此表 = 整体 unsupported 空态,不读取)
const UNVIEWABLE_EXTENSIONS: &[&str] = &[
    // 视频
    "mp4", "mov", "avi", "mkv", "webm", "flv", "wmv", "m4v", // 音频
    "mp3", "wav", "flac", "ogg", "m4a", "aac", "wma", "opus", // 压缩包
    "zip", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "tar", "jar", // office
    "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "pages", "numbers",
    // 二进制交付物
    "exe", "dll", "so", "dylib", "bin", "o", "class", "pyc", "wasm", // 字体
    "ttf", "otf", "woff", "woff2", "eot", // 磁盘镜像/数据文件
    "dmg", "iso", "img", "sqlite", "db",
    // 图片但解码面不支持(gpui ImageFormat 仅 png/jpeg/gif/webp)
    "psd", "ai", "sketch", "tiff", "tif", "heic", "heif", "avif", "bmp", "ico",
];

/// 文件名是否以 `.{ext}` 结尾;命中返回后缀长度(`a.markdown` 对
/// markdown = 9,含点;compound 后缀只取单段,与源 matchedSuffixLength 一致)
fn suffix_len(name: &str, ext: &str) -> Option<usize> {
    let suffix = format!(".{ext}");
    name.ends_with(&suffix).then_some(suffix.len())
}

/// 该后缀是否被任一渲染器声明为二进制(照源 binaryExtensions:
/// image 除 svg 外全部 + pdf)。命中则候选不再追加纯文本兜底
fn binary_document(name: &str) -> bool {
    const BINARY: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "pdf"];
    BINARY.iter().any(|ext| suffix_len(name, ext).is_some())
}

/// 是否落在不可预览表(空候选时的判据)
fn unviewable(name: &str) -> bool {
    UNVIEWABLE_EXTENSIONS
        .iter()
        .any(|ext| suffix_len(name, ext).is_some())
}

/// 文件名 → 预览渲染器候选序列。空 = 该格式整体不可预览
/// (unsupported 空态:不读取、无菜单)。
///
/// 序 = 默认渲染器在前,末位可含纯文本兜底;菜单在候选数 > 1 时显示。
/// 规则照源:扩展命中按「最长后缀优先,平长按注册序」排序。
pub fn doc_candidates(name: &str) -> Vec<DocRenderer> {
    let lower = file_name_of(name);
    let mut matched: Vec<(usize, DocRenderer)> = REGISTRY
        .iter()
        .filter_map(|renderer| {
            if *renderer == DocRenderer::Text {
                return None; // text 无扩展名声明,仅兜底
            }
            let best = renderer
                .extensions()
                .iter()
                .filter_map(|ext| suffix_len(&lower, ext))
                .max()?;
            Some((best, *renderer))
        })
        .collect();
    // 最长后缀优先;平长保持 REGISTRY 序(sort_by 稳定,收集序即注册序)
    matched.sort_by_key(|(len, _)| std::cmp::Reverse(*len));
    let mut candidates: Vec<DocRenderer> = matched.into_iter().map(|(_, r)| r).collect();
    if candidates.is_empty() {
        if unviewable(&lower) {
            return Vec::new();
        }
        // 无命中且非不可预览 → 纯文本兜底
        return vec![DocRenderer::Text];
    }
    if !binary_document(&lower) {
        candidates.push(DocRenderer::Text);
    }
    candidates
}

/// 默认渲染器(候选首位)
pub fn default_renderer(name: &str) -> Option<DocRenderer> {
    doc_candidates(name).first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 分类链 ─────────────────────────────────────────────

    #[test]
    fn file_class_exact_name_prefix_ext_chain() {
        assert_eq!(file_class("README"), FileClass::Markdown);
        assert_eq!(file_class("changelog.txt"), FileClass::Markdown);
        assert_eq!(file_class(".gitignore"), FileClass::Git);
        assert_eq!(file_class(".env"), FileClass::Env);
        assert_eq!(file_class(".env.production"), FileClass::Env);
        assert_eq!(file_class("dockerfile.dev"), FileClass::Config);
        assert_eq!(file_class("Makefile"), FileClass::Config);
        // 路径形态与大小写归一
        assert_eq!(file_class("src\\Main.RS"), FileClass::Rust);
        assert_eq!(file_class("a/b/c/notes.MD"), FileClass::Markdown);
        // 扩展名表
        assert_eq!(file_class("Cargo.toml"), FileClass::Config);
        assert_eq!(file_class("logo.SVG"), FileClass::Image);
        assert_eq!(file_class("lib.rs"), FileClass::Rust);
        assert_eq!(file_class("main.py"), FileClass::Python);
        assert_eq!(file_class("index.ts"), FileClass::JsTs);
        assert_eq!(file_class("go.mod"), FileClass::Other);
        assert_eq!(file_class("noext"), FileClass::Other);
    }

    #[test]
    fn class_icon_all_variants_covered() {
        // 全家族图标可构造(编译期穷尽 + 运行时冒烟)
        const ALL: [FileClass; 24] = [
            FileClass::Markdown,
            FileClass::Image,
            FileClass::Pdf,
            FileClass::Html,
            FileClass::Css,
            FileClass::Rust,
            FileClass::Git,
            FileClass::Json,
            FileClass::Config,
            FileClass::Env,
            FileClass::Lock,
            FileClass::Shell,
            FileClass::Python,
            FileClass::JsTs,
            FileClass::Code,
            FileClass::Archive,
            FileClass::Video,
            FileClass::Audio,
            FileClass::Word,
            FileClass::Excel,
            FileClass::Ppt,
            FileClass::Font,
            FileClass::Text,
            FileClass::Other,
        ];
        for class in ALL {
            let _ = class_icon(class, 14.);
        }
    }

    // ── 渲染器注册表 ───────────────────────────────────────

    #[test]
    fn doc_candidates_longest_suffix_and_tie_order() {
        // code 表也含 markdown(源 languages 全表)→ `.markdown` 平长 9,
        // 注册序 markdown 在 code 前
        assert_eq!(
            doc_candidates("a.markdown"),
            vec![DocRenderer::Markdown, DocRenderer::Code, DocRenderer::Text]
        );
        // .md 平长(2) → 注册序 markdown 在 code 前
        assert_eq!(
            doc_candidates("readme.md"),
            vec![DocRenderer::Markdown, DocRenderer::Code, DocRenderer::Text]
        );
        // .png 命中 image 且 binary → 不追加纯文本
        assert_eq!(doc_candidates("pic.png"), vec![DocRenderer::Image]);
        // .svg 命中 code(解码面无 svg;源码文本可读)
        assert_eq!(
            doc_candidates("icon.svg"),
            vec![DocRenderer::Code, DocRenderer::Text]
        );
        // bmp/ico = 解码面外 → 不可预览空候选
        assert!(doc_candidates("logo.bmp").is_empty());
        assert!(doc_candidates("favicon.ico").is_empty());
        // .pdf 命中 pdf 且 binary → 单候选
        assert_eq!(doc_candidates("doc.pdf"), vec![DocRenderer::Pdf]);
        // 纯代码
        assert_eq!(
            doc_candidates("main.rs"),
            vec![DocRenderer::Code, DocRenderer::Text]
        );
        // 未知扩展且非不可预览 → 纯文本兜底
        assert_eq!(doc_candidates("data.xyz"), vec![DocRenderer::Text]);
        // 无扩展
        assert_eq!(doc_candidates("LICENSE"), vec![DocRenderer::Text]);
    }

    #[test]
    fn doc_candidates_unviewable_binary_table() {
        for name in [
            "movie.mp4",
            "song.mp3",
            "bundle.zip",
            "doc.docx",
            "app.dmg",
            "lib.so",
            "f.ttf",
            "photo.heic",
            "art.psd",
        ] {
            assert!(doc_candidates(name).is_empty(), "{name} 应为不可预览空候选");
        }
    }

    #[test]
    fn default_renderer_is_first_candidate() {
        assert_eq!(default_renderer("a.md"), Some(DocRenderer::Markdown));
        assert_eq!(default_renderer("a.go"), Some(DocRenderer::Code));
        assert_eq!(default_renderer("a.exe"), None);
    }

    #[test]
    fn renderer_metadata_matches_source() {
        assert_eq!(DocRenderer::Text.id(), "text");
        assert_eq!(DocRenderer::Code.title(), "代码");
        assert_eq!(DocRenderer::Image.load_mode(), LoadMode::BytesComplete);
        assert_eq!(DocRenderer::Markdown.load_mode(), LoadMode::TextPages);
        assert!(DocRenderer::Code.wrap());
        assert!(!DocRenderer::Markdown.wrap());
    }
}
