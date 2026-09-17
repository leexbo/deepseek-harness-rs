//! 文件树纯函数层:单层目录列举 + 排序 collator + 越界/错误语义。
//! 照源 workspace-files list 契约:无忽略规则(点开头混排、.git 照
//! 列)、单层返回、超 `max_entries` 截断置标志;symlink/非常规条目
//! 归 Other(不可导航/不可打开)。

use std::cmp::Ordering;
use std::path::Path;

/// 单层最大条目数(照源 maxEntries;超出截断 + 标志)
pub const MAX_ENTRIES: usize = 2000;

/// 目录条目类型(照源:'file' | 'directory' | 'other')
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    /// 普通文件
    File,
    /// 目录
    Directory,
    /// symlink/socket/设备等(std `DirEntry::file_type` 不跟随符号链接,
    /// 链接即 Other——与源 readdir withFileTypes 的 isDirectory/isFile
    /// 判定一致,防经 symlink 逃逸工作区)
    Other,
}

/// 一条目录项
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DirEntryRow {
    /// 条目名(单段)
    pub name: String,
    /// 类型
    pub kind: EntryKind,
}

/// 单层列举结果
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Listing {
    /// 已排序条目(目录在前,组内 collator 序)
    pub entries: Vec<DirEntryRow>,
    /// 超上限被截断
    pub truncated: bool,
}

/// 列举错误(文案映射见 views 的 failure_line)
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ListError {
    /// 目录不存在
    NotFound,
    /// 路径不是目录
    NotDirectory,
    /// 目录在工作区之外
    OutsideWorkspace,
    /// 其余 IO 失败(带消息)
    Unavailable(String),
}

/// 数字感知、大小写不敏感 collator(源 Intl.Collator numeric,
/// sensitivity base 近似:数字段按数值比较,其余字符 case-fold;
/// 全等回落逐字节序保证确定性)
pub fn collate(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (Some(x), Some(y)) => {
                let (xd, yd) = (x.is_ascii_digit(), y.is_ascii_digit());
                if xd && yd {
                    // 数字段:去前导零后先比位数(位数大 = 数值大),再逐字符
                    let xa: String = consume_digits(&mut ai);
                    let ya: String = consume_digits(&mut bi);
                    let ord = xa.len().cmp(&ya.len()).then_with(|| xa.cmp(&ya));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    ai.next();
                    bi.next();
                    let ord = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
            }
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => return a.as_bytes().cmp(b.as_bytes()),
        }
    }
}

fn consume_digits<I: Iterator<Item = char>>(iter: &mut std::iter::Peekable<I>) -> String {
    let mut digits = String::new();
    while let Some(c) = iter.peek().copied() {
        if !c.is_ascii_digit() {
            break;
        }
        digits.push(c);
        iter.next();
    }
    digits.trim_start_matches('0').to_string()
}

/// 排序:目录在前,组内 collator(照源 orderEntries)
pub fn order_entries(entries: &mut [DirEntryRow]) {
    entries.sort_by(|a, b| {
        let dir_first = match (a.kind, b.kind) {
            (EntryKind::Directory, EntryKind::Directory)
            | (EntryKind::File, EntryKind::File)
            | (EntryKind::Other, EntryKind::Other) => Ordering::Equal,
            (EntryKind::Directory, _) => Ordering::Less,
            (_, EntryKind::Directory) => Ordering::Greater,
            // 文件与 other 同组(源只分「目录 / 其余」两组)
            _ => Ordering::Equal,
        };
        dir_first.then_with(|| collate(&a.name, &b.name))
    });
}

/// 单层列举。`root` = 工作区根,`dir` = 目标目录绝对路径
/// (必须位在 root 内,经 canonicalize 校验——防构造路径逃逸)。
/// 不存在/非目录/越界各自成错;读取成功后按序返回并截断
pub fn list_dir(root: &Path, dir: &Path, max_entries: usize) -> Result<Listing, ListError> {
    let root_canon = std::fs::canonicalize(root)
        .map_err(|e| ListError::Unavailable(format!("{}: {e}", root.display())))?;
    let dir_canon = std::fs::canonicalize(dir).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ListError::NotFound
        } else {
            ListError::Unavailable(format!("{}: {err}", dir.display()))
        }
    })?;
    if !dir_canon.starts_with(&root_canon) {
        return Err(ListError::OutsideWorkspace);
    }
    if !dir_canon.is_dir() {
        return Err(ListError::NotDirectory);
    }
    let read = std::fs::read_dir(&dir_canon)
        .map_err(|e| ListError::Unavailable(format!("{}: {e}", dir.display())))?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in read {
        let entry = entry.map_err(|e| ListError::Unavailable(format!("{}: {e}", dir.display())))?;
        if entries.len() == max_entries {
            truncated = true;
            break;
        }
        // file_type 不跟随 symlink:链接/套接字/设备 → Other
        let kind = match entry.file_type() {
            Ok(t) if t.is_dir() => EntryKind::Directory,
            Ok(t) if t.is_file() => EntryKind::File,
            _ => EntryKind::Other,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        entries.push(DirEntryRow { name, kind });
    }
    order_entries(&mut entries);
    Ok(Listing { entries, truncated })
}

/// 树行错误文案(照源 zh locale 逐字)
pub fn failure_line(err: &ListError) -> String {
    match err {
        ListError::NotFound => "这个目录不在了。可能已被移动或删除。".to_string(),
        ListError::OutsideWorkspace => "这个目录在工作区之外，侧栏不会读取它。".to_string(),
        ListError::NotDirectory => "这不是一个目录。".to_string(),
        ListError::Unavailable(msg) => format!("读取失败：{msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-files-face-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    #[test]
    fn collate_numeric_and_casefold() {
        assert_eq!(collate("file2", "file10"), Ordering::Less);
        assert_eq!(collate("file10", "file9"), Ordering::Greater);
        assert_eq!(collate("B.md", "a.md"), Ordering::Greater);
        assert_eq!(collate("a.md", "B.md"), Ordering::Less);
        // 数字段相等(a007 ≡ a7)后落字节序兜底(确定性),a007 < a7
        assert_eq!(collate("a007", "a7"), Ordering::Less);
        let ord = collate("a1b", "a1c");
        assert_eq!(ord, Ordering::Less);
    }

    #[test]
    fn order_entries_directories_first_dotfiles_mixed() {
        let mut rows = vec![
            DirEntryRow {
                name: "zeta.md".into(),
                kind: EntryKind::File,
            },
            DirEntryRow {
                name: "src".into(),
                kind: EntryKind::Directory,
            },
            DirEntryRow {
                name: ".git".into(),
                kind: EntryKind::Directory,
            },
            DirEntryRow {
                name: "file10".into(),
                kind: EntryKind::File,
            },
            DirEntryRow {
                name: "File2".into(),
                kind: EntryKind::File,
            },
            DirEntryRow {
                name: "link".into(),
                kind: EntryKind::Other,
            },
        ];
        order_entries(&mut rows);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        // 数字感知:File2(<file10 折叠同前缀,2 < 10)
        assert_eq!(
            names,
            vec![".git", "src", "File2", "file10", "link", "zeta.md"]
        );
    }

    #[test]
    fn list_dir_orders_and_types() {
        let root = temp_root("order");
        fs::create_dir_all(root.join("beta")).expect("建子目录");
        fs::write(root.join("alpha.txt"), "x").expect("写文件");
        std::os::unix::fs::symlink(root.join("alpha.txt"), root.join("slink")).expect("建 symlink");
        let listing = list_dir(&root, &root, MAX_ENTRIES).expect("列举应成功");
        let names: Vec<(&str, EntryKind)> = listing
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.kind))
            .collect();
        assert_eq!(
            names,
            vec![
                ("beta", EntryKind::Directory),
                ("alpha.txt", EntryKind::File),
                ("slink", EntryKind::Other),
            ]
        );
        assert!(!listing.truncated);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_dir_truncation_flag() {
        let root = temp_root("trunc");
        for i in 0..5 {
            fs::write(root.join(format!("f{i}.txt")), "x").expect("写文件");
        }
        let listing = list_dir(&root, &root, 3).expect("列举应成功");
        assert_eq!(listing.entries.len(), 3);
        assert!(listing.truncated);
        // 恰好等于上限:不截断
        let exact = list_dir(&root, &root, 5).expect("列举应成功");
        assert_eq!(exact.entries.len(), 5);
        assert!(!exact.truncated);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_dir_error_kinds() {
        let root = temp_root("errors");
        fs::create_dir_all(root.join("d")).expect("建子目录");
        // 不存在
        assert_eq!(
            list_dir(&root, &root.join("gone"), 10),
            Err(ListError::NotFound)
        );
        // 非目录
        fs::write(root.join("f.txt"), "x").expect("写文件");
        assert_eq!(
            list_dir(&root, &root.join("f.txt"), 10),
            Err(ListError::NotDirectory)
        );
        // 越界:指向 root 之外(symlink 到外层再 canonicalize 即脱离 root)
        let outside = temp_root("outside");
        let link = root.join("escape");
        std::os::unix::fs::symlink(&outside, &link).expect("建外链");
        assert_eq!(list_dir(&root, &link, 10), Err(ListError::OutsideWorkspace));
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn failure_line_copies_source_copy_verbatim() {
        assert_eq!(
            failure_line(&ListError::NotFound),
            "这个目录不在了。可能已被移动或删除。"
        );
        assert_eq!(
            failure_line(&ListError::Unavailable("boom".into())),
            "读取失败：boom"
        );
    }
}
