//! 文档预览纯函数层:读契约(照源 workspace-files read/readAll)。
//!
//! 上限语义照源:**超限报错,绝不静默截断**——单页行数 ≤ 5000、单页
//! 字节 ≤ 2MiB(`TooLarge`)、整档 ≤ 32MiB;页 = 整页文本字符串 +
//! 行数 + eof 标志(offset 越过文件尾 = 0 行 + eof)。NUL / 非 UTF-8
//! = `NotText`。版本 token = (mtime 纳秒, 长度),供变更提示条比对。

use std::io::{BufRead, BufReader};
use std::path::Path;

/// 默认且最大页行数(照源 maxLines)
pub const MAX_LINES: usize = 5000;
/// 单页字节上限(照源 maxBytes = 2 MiB)
pub const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// 整档字节上限(readAll;照源 maxFileBytes = 32 MiB)
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// 一页文本(行页;text = 本页各行以 `\n` 连接、末行无终止符)
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TextPage {
    /// 本页起始行(1-based)
    pub offset: u32,
    /// 本页文本
    pub text: String,
    /// 本页行数(offset 越过文件尾 = 0)
    pub lines: u32,
    /// 本页是否含最后一行
    pub eof: bool,
}

/// 读取错误(文案映射见 [`failure_line`])
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReadError {
    /// 文件不存在
    NotFound,
    /// 超过字节上限(带上限值)
    TooLarge {
        /// 触发的上限(human_bytes 格式化进文案)
        limit: u64,
    },
    /// 含 NUL 或非 UTF-8
    NotText,
    /// 不是普通文件
    NotRegularFile,
    /// 其余 IO 失败(带消息)
    Unavailable(String),
}

/// 文件元数据前置检查(NotFound / 非普通文件归一)
fn check_regular(abs: &Path) -> Result<std::fs::File, ReadError> {
    let meta = std::fs::metadata(abs).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ReadError::NotFound
        } else {
            ReadError::Unavailable(format!("{}: {err}", abs.display()))
        }
    })?;
    if !meta.is_file() {
        return Err(ReadError::NotRegularFile);
    }
    std::fs::File::open(abs).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ReadError::NotFound
        } else {
            ReadError::Unavailable(format!("{}: {err}", abs.display()))
        }
    })
}

/// 读一页文本(1-based `offset` 起行,`limit` 行;limit > MAX_LINES 按
/// 契约应报错,调用方不传超限值)。逐行流式读取,只读到页尾
pub fn read_text_page(abs: &Path, offset: u32, limit: usize) -> Result<TextPage, ReadError> {
    let file = check_regular(abs)?;
    let mut reader = BufReader::new(file);
    let mut skipped: u32 = 0;
    let mut taken: u32 = 0;
    let mut taken_bytes: u64 = 0;
    let mut buf: Vec<u8> = Vec::new();
    let mut eof = false;
    let mut has_nul = false;
    loop {
        if taken as usize >= limit {
            break;
        }
        let mut line: Vec<u8> = Vec::new();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| ReadError::Unavailable(format!("{}: {e}", abs.display())))?;
        if n == 0 {
            eof = true;
            break;
        }
        if skipped + 1 < offset {
            skipped += 1;
            continue;
        }
        taken_bytes += n as u64;
        if taken_bytes > MAX_BYTES {
            return Err(ReadError::TooLarge { limit: MAX_BYTES });
        }
        has_nul |= line.contains(&0);
        // 行尾 \r\n / \n 归一为 \n(拼接后整体去 CR;行中 CR 保留)
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            buf.extend_from_slice(&line);
            buf.push(b'\n');
        } else {
            buf.extend_from_slice(&line);
        }
        taken += 1;
    }
    if has_nul {
        return Err(ReadError::NotText);
    }
    let raw = String::from_utf8(buf).map_err(|_| ReadError::NotText)?;
    // 行间以 \n 连接、末行无终止符(照源);末行若换行终结则剥去
    let text = raw.strip_suffix('\n').unwrap_or(&raw).to_string();
    Ok(TextPage {
        offset,
        text,
        lines: taken,
        eof,
    })
}

/// 整档字节(readAll;超 32MiB 报 TooLarge)
pub fn read_all_bytes(abs: &Path) -> Result<Vec<u8>, ReadError> {
    let file = check_regular(abs)?;
    let meta = file
        .metadata()
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", abs.display())))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(ReadError::TooLarge {
            limit: MAX_FILE_BYTES,
        });
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    use std::io::Read as _;
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ReadError::Unavailable(format!("{}: {e}", abs.display())))?;
    Ok(bytes)
}

/// 版本 token((mtime 纳秒, 长度);stat 失败 = None,即元数据失败面)
pub fn stat_version(abs: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(abs).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    Some((mtime, meta.len()))
}

/// 人类可读字节(照源 humanBytes:≥1MiB 显 MB,≥1KiB 显 KB,均一位小数)
pub fn human_bytes(n: u64) -> String {
    const KIB: f64 = 1024.;
    const MIB: f64 = 1024. * 1024.;
    let v = n as f64;
    if v >= MIB {
        format!("{:.1} MB", v / MIB)
    } else if v >= KIB {
        format!("{:.1} KB", v / KIB)
    } else {
        format!("{n} B")
    }
}

/// 错误文案(照源 zh locale 逐字)
pub fn failure_line(err: &ReadError) -> String {
    match err {
        ReadError::NotFound => "文件不存在，可能已被移动或删除".to_string(),
        ReadError::TooLarge { limit } => {
            format!("单页内容超过 {} 上限，无法读取", human_bytes(*limit))
        }
        ReadError::NotText => "该格式文件暂时无法预览".to_string(),
        ReadError::NotRegularFile => "该路径不是普通文件，没有可显示的内容".to_string(),
        ReadError::Unavailable(msg) => format!("读取失败：{msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write as _;
    use std::path::PathBuf;

    fn temp_file(tag: &str, contents: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "liuma-preview-face-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let mut f = fs::File::create(&path).expect("建临时文件");
        f.write_all(contents).expect("写临时文件");
        path
    }

    fn cleanup(path: &Path) {
        fs::remove_file(path).ok();
    }

    #[test]
    fn read_text_page_first_page_and_offsets() {
        let path = temp_file("page", b"l1\nl2\nl3\n");
        let page = read_text_page(&path, 1, 2).expect("首页");
        assert_eq!(page.text, "l1\nl2");
        assert_eq!(page.lines, 2);
        assert!(!page.eof);
        let next = read_text_page(&path, 3, 2).expect("次页");
        assert_eq!(next.text, "l3");
        assert_eq!(next.lines, 1);
        assert!(next.eof);
        // 越过文件尾:0 行 + eof
        let beyond = read_text_page(&path, 9, 2).expect("越尾");
        assert_eq!(beyond.lines, 0);
        assert!(beyond.eof);
        cleanup(&path);
    }

    #[test]
    fn read_text_page_crlf_and_last_line_without_newline() {
        let path = temp_file("crlf", b"a\r\nb\r\nc");
        let page = read_text_page(&path, 1, 10).expect("整读");
        assert_eq!(page.text, "a\nb\nc");
        assert!(page.eof);
        cleanup(&path);
    }

    #[test]
    fn read_text_page_errors() {
        let gone = std::env::temp_dir().join("liuma-preview-face-gone-nope");
        assert_eq!(read_text_page(&gone, 1, 10), Err(ReadError::NotFound));
        let binary = temp_file("bin", b"abc\x00def");
        assert_eq!(read_text_page(&binary, 1, 10), Err(ReadError::NotText));
        cleanup(&binary);
        let dir = std::env::temp_dir();
        assert_eq!(read_text_page(&dir, 1, 10), Err(ReadError::NotRegularFile));
        // 非 UTF-8
        let utf = temp_file("utf", b"\xff\xfe broken");
        assert_eq!(read_text_page(&utf, 1, 10), Err(ReadError::NotText));
        cleanup(&utf);
    }

    #[test]
    fn read_text_page_byte_cap_errors_not_truncates() {
        // 3 行、每行 ~1MiB → 页字节超 2MiB 报 TooLarge(不静默截断)
        let big_line = "x".repeat(1024 * 1024);
        let contents = format!("{big_line}\n{big_line}\n{big_line}\n");
        let path = temp_file("big", contents.as_bytes());
        assert_eq!(
            read_text_page(&path, 1, 5000),
            Err(ReadError::TooLarge { limit: MAX_BYTES })
        );
        // 单行取 2 行 = 2MiB 出头 → 仍超;取 1 行 = 1MiB → 过
        assert_eq!(
            read_text_page(&path, 1, 2),
            Err(ReadError::TooLarge { limit: MAX_BYTES })
        );
        let one = read_text_page(&path, 1, 1).expect("一行页应过");
        assert_eq!(one.lines, 1);
        cleanup(&path);
    }

    #[test]
    fn read_all_bytes_cap() {
        let path = temp_file("all", b"hello");
        assert_eq!(read_all_bytes(&path).as_deref(), Ok(b"hello".as_slice()));
        cleanup(&path);
        let gone = std::env::temp_dir().join("liuma-preview-face-all-gone");
        assert_eq!(read_all_bytes(&gone), Err(ReadError::NotFound));
    }

    #[test]
    fn stat_version_changes_with_content() {
        let path = temp_file("stat", b"v1");
        let v1 = stat_version(&path).expect("stat");
        std::thread::sleep(std::time::Duration::from_millis(2));
        fs::write(&path, b"v2 longer").expect("改写");
        let v2 = stat_version(&path).expect("stat");
        assert_ne!(v1, v2);
        cleanup(&path);
        let gone = std::env::temp_dir().join("liuma-preview-face-stat-gone");
        assert_eq!(stat_version(&gone), None);
    }

    #[test]
    fn human_bytes_and_failure_lines() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(
            failure_line(&ReadError::TooLarge {
                limit: MAX_FILE_BYTES
            }),
            "单页内容超过 32.0 MB 上限，无法读取"
        );
        assert_eq!(
            failure_line(&ReadError::NotFound),
            "文件不存在，可能已被移动或删除"
        );
    }
}
