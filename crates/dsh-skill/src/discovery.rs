//! 技能发现:两根标准生态位置,rank 遮蔽(项目遮用户),每根一层扫描。
//!
//! 根(对齐跨工具 `.agents/skills` 约定,无品牌目录):
//! - 项目 `<projectRoot>/.agents/skills`(rank 100)
//! - 用户 `<home>/.agents/skills`(rank 200)
//!
//! 形态两种:目录 `<name>/SKILL.md` 与扁平文件 `<name>.md`;只扫一层
//! (不递归,照源)。项目根 = 向上找最近 `.git`,找不到回退 cwd
//! (与 dsh-host instructions 的发现语义同源)。`slot` 是文件系统定位名
//! (目录名/文件 stem),模型面身份以 frontmatter `name` 为准。

use std::path::{Path, PathBuf};

use crate::frontmatter::is_skill_name;

pub const SKILL_FILE: &str = "SKILL.md";

/// 一个扫描根(rank 越小越优先;同根内按 slot 名序,保证发现顺序确定)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRoot {
    pub path: PathBuf,
    pub rank: u32,
}

/// 一个发现条目(候选)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// 文件系统定位名(目录名 / 扁平文件 stem;排序键)
    pub slot: String,
    /// SKILL.md / 扁平 .md 的绝对路径
    pub path: PathBuf,
    /// 模型面 resource base(目录型 = 技能文件夹;扁平型 = 所在根目录)
    pub base_dir: PathBuf,
    pub rank: u32,
}

/// 从 `cwd` 逐级向上找第一个含 `.git` 的目录;找不到回退 `cwd`
/// (fail-soft;与 `dsh_host::instructions::find_project_root` 同语义,
/// 此处独立实现避免 skill 层反向依赖宿主 crate)
pub fn find_project_root(cwd: &Path) -> PathBuf {
    let mut current = cwd.to_path_buf();
    loop {
        if current.join(".git").symlink_metadata().is_ok() {
            return current;
        }
        match current.parent() {
            Some(p) if p != current => current = p.to_path_buf(),
            _ => return cwd.to_path_buf(),
        }
    }
}

/// 生产根组合(测试经 [`SkillRoot`] 直构)
pub fn skill_roots(cwd: &Path, user_home: &Path) -> Vec<SkillRoot> {
    let project_root = find_project_root(cwd);
    vec![
        SkillRoot {
            path: project_root.join(".agents").join("skills"),
            rank: 100,
        },
        SkillRoot {
            path: user_home.join(".agents").join("skills"),
            rank: 200,
        },
    ]
}

/// 扫一个根的一层条目(目录型 + 扁平 .md;IO 失败 = 空而非错)。
/// 输出按 slot 名升序。
pub fn discover_root(root: &SkillRoot) -> Vec<Candidate> {
    let Ok(entries) = std::fs::read_dir(&root.path) else {
        return Vec::new();
    };
    let mut slots: Vec<(String, Candidate)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let candidate = if ft.is_dir() {
            let slot = entry.file_name().to_string_lossy().to_string();
            let skill_file = path.join(SKILL_FILE);
            if !skill_file.is_file() {
                continue;
            }
            Candidate {
                slot,
                path: skill_file,
                base_dir: path,
                rank: root.rank,
            }
        } else if ft.is_file() {
            let slot = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if path.extension().and_then(|e| e.to_str()) != Some("md") || !is_skill_name(&slot) {
                continue;
            }
            let base_dir = path.parent().unwrap_or(&root.path).to_path_buf();
            Candidate {
                slot,
                path,
                base_dir,
                rank: root.rank,
            }
        } else {
            continue;
        };
        slots.push((candidate.slot.clone(), candidate));
    }
    slots.sort_by(|a, b| a.0.cmp(&b.0));
    slots.into_iter().map(|(_, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "dsh-skill-{}-{}-{}",
                tag,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, body: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const SKILL: &str = "---\nname: foo\ndescription: d\n---\nbody";

    #[test]
    fn discovers_directory_and_flat_forms() {
        let tmp = TempDir::new("discover");
        tmp.write("alpha/SKILL.md", SKILL);
        tmp.write("beta.md", SKILL);
        // 非 .md / 坏名扁平文件 / 无 SKILL.md 的目录都跳过
        tmp.write("gamma.txt", SKILL);
        tmp.write("Bad_Name.md", SKILL);
        tmp.write("empty-dir/other.txt", "x");

        let found = discover_root(&SkillRoot {
            path: tmp.0.clone(),
            rank: 100,
        });
        let slots: Vec<&str> = found.iter().map(|c| c.slot.as_str()).collect();
        assert_eq!(slots, vec!["alpha", "beta"]);
        // 目录型 base_dir = 技能文件夹;扁平型 = 所在根目录
        assert_eq!(found[0].base_dir, tmp.0.join("alpha"));
        assert_eq!(found[1].base_dir, tmp.0);
    }

    #[test]
    fn missing_root_is_empty_not_error() {
        let tmp = TempDir::new("missing");
        let found = discover_root(&SkillRoot {
            path: tmp.0.join("nope"),
            rank: 1,
        });
        assert!(found.is_empty());
    }

    #[test]
    fn root_composition_ranks() {
        let tmp = TempDir::new("roots");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let roots = skill_roots(&ws, &tmp.0);
        assert_eq!(roots[0].rank, 100);
        assert_eq!(roots[0].path, ws.join(".agents").join("skills"));
        assert_eq!(roots[1].rank, 200);
        assert_eq!(roots[1].path, tmp.0.join(".agents").join("skills"));
    }

    #[test]
    fn project_root_anchored_by_git_marker() {
        let tmp = TempDir::new("projroot");
        std::fs::create_dir(tmp.0.join(".git")).unwrap();
        let nested = tmp.0.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_project_root(&nested), tmp.0);
        // 无 .git 回退 cwd 自身
        let plain = TempDir::new("plain");
        let ws = plain.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        assert_eq!(find_project_root(&ws), ws);
    }
}
