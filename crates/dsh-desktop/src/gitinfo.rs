//! git 分支探测(标题栏工作区下拉 / StatusBar 徽标用):纯文件读
//! `.git/HEAD`,不 spawn 进程。分支是**瞬态 UI 态**——模型可经 bash
//! 切换分支,故不落 session 日志(single boundary rule),由 store 在
//! 启动/工作区变更/切工作区/开菜单与 running 轮询时刷新。

use std::path::{Path, PathBuf};

/// 读工作区根的当前分支名。非 repo / 读取失败 → None;detached HEAD
/// → 7 位短 sha。支持 `.git` 目录与 `.git` 文件(worktree/submodule
/// 的 `gitdir:` 指针,相对路径按工作区根拼接)两种形态。
pub fn branch_of(root: &Path) -> Option<String> {
    let dotgit = root.join(".git");
    let head_path: PathBuf = if dotgit.is_dir() {
        dotgit.join("HEAD")
    } else {
        // worktree/submodule:.git 是一文字指针
        let pointer = std::fs::read_to_string(&dotgit).ok()?;
        let target = pointer.trim().strip_prefix("gitdir:")?.trim();
        let target = Path::new(target);
        if target.is_absolute() {
            target.to_path_buf()
        } else {
            root.join(target)
        }
        .join("HEAD")
    };
    let head = std::fs::read_to_string(&head_path).ok()?;
    parse_head(&head)
}

/// HEAD 内容解析:`ref: refs/heads/X` → X;40 位 hex(detached)→ 前 7 位
fn parse_head(head: &str) -> Option<String> {
    let head = head.trim();
    if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        let branch = branch.trim();
        (!branch.is_empty()).then(|| branch.to_string())
    } else if head.len() == 40 && head.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(head[..7].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsh-gitinfo-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("夹具目录");
        dir
    }

    #[test]
    fn normal_branch_ref() {
        let dir = fixture("ref");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(branch_of(&dir).as_deref(), Some("feature/x"));
    }

    #[test]
    fn detached_head_short_sha() {
        let dir = fixture("detached");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(
            dir.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(branch_of(&dir).as_deref(), Some("0123456"));
    }

    #[test]
    fn gitdir_pointer_file() {
        let dir = fixture("pointer");
        let real = dir.join("real-git");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        // 相对路径指针
        fs::write(dir.join(".git"), "gitdir: real-git\n").unwrap();
        assert_eq!(branch_of(&dir).as_deref(), Some("main"));
        // 绝对路径指针
        let dir2 = fixture("pointer-abs");
        fs::create_dir_all(&dir2).unwrap();
        fs::write(dir2.join(".git"), format!("gitdir: {}", real.display())).unwrap();
        assert_eq!(branch_of(&dir2).as_deref(), Some("main"));
    }

    #[test]
    fn non_repo_and_malformed() {
        let dir = fixture("bare");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(branch_of(&dir), None);
        // HEAD 内容异常
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/HEAD"), "garbage").unwrap();
        assert_eq!(branch_of(&dir), None);
    }
}
