//! skill 子系统:发现 → registry(遮蔽/缓存)→ `skill` 工具 +
//! 持久 user 消息目录 + `/name` 用户手势。
//!
//! 语义蓝本为源项目 packages/skill(其本身对齐 `.agents/skills` 标准):
//! - 渐进披露:目录消息只有 name + description(≤500 字符);正文只在
//!   两条路径进入——`skill` 工具结果、或 `/name` 手势注入——且共用同一
//!   `<skill_content>` 渲染,模型在两条路径看到同一形态。
//! - 调用策略只有双面开关(modelInvocable × userInvocable),无工具白名单。
//! - 目录正文永不缓存;摘要缓存按 mtime/size 校验(热路径 stat 探测,
//!   照 AGENTS.md 惯例)。
//!
//! 注入通道与事件形态见 `catalog` 模块;引擎侧接线见 dsh-agent-loop 的
//! `set_skill_catalog_provider` / `set_skill_gesture_provider`。

pub mod catalog;
pub mod discovery;
pub mod frontmatter;

pub use catalog::{SkillCatalogState, gesture_payloads};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::SystemTime;

use serde_json::{Value, json};

use dsh_agent_loop::tools::{ToolCallRequest, ToolOutput, ToolPort};

use crate::discovery::{Candidate, SkillRoot, discover_root, skill_roots};
use crate::frontmatter::{ParsedSkill, parse_skill_source};

/// 目录 description 的归一化截断上限(源 catalogDescriptionMaxLength 默认)
pub const CATALOG_DESCRIPTION_MAX_LENGTH: usize = 500;

/// 一个可用技能的摘要(无正文;目录与 RPC 面形态)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    /// SKILL.md 绝对路径
    pub path: PathBuf,
    /// 模型面 resource base(目录型 = 技能文件夹;扁平型 = 所在根)
    pub base_dir: PathBuf,
    pub rank: u32,
    /// 模型可调用(未设 disable-model-invocation)
    pub model_invocable: bool,
    /// 用户可手势调用(未设 user-invocable: false)
    pub user_invocable: bool,
}

/// 完整技能定义(摘要 + 正文;`get` 每次重读,不缓存)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDefinition {
    pub summary: SkillSummary,
    pub body: String,
}

/// 摘要缓存条目:mtime/size 未变即复用解析结果(含「解析失败」的负缓存,
/// 坏文件每版本只 warn 一次,不随每步重扫刷屏)
#[derive(Clone)]
struct SummaryCache {
    size: u64,
    mtime: Option<SystemTime>,
    parsed: Option<ParsedSkill>,
}

/// 技能服务(宿主级共享;`list` 走摘要缓存,`get` 每次重读全文)
pub struct SkillService {
    /// 用户根的家目录(None = 真实 home;测试注入隔离)
    user_home: RwLock<Option<PathBuf>>,
    cache: Mutex<HashMap<PathBuf, SummaryCache>>,
}

impl Default for SkillService {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillService {
    pub fn new() -> Self {
        Self {
            user_home: RwLock::new(None),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 覆写用户根家目录(仅测试注入;生产恒真实 home)
    pub fn set_user_home(&self, home: Option<PathBuf>) {
        *self.user_home.write().expect("user_home 锁中毒") = home;
    }

    fn user_home(&self) -> PathBuf {
        self.user_home
            .read()
            .expect("user_home 锁中毒")
            .clone()
            .unwrap_or_else(|| std::env::home_dir().unwrap_or_else(|| PathBuf::from("/")))
    }

    fn roots(&self, cwd: &Path) -> Vec<SkillRoot> {
        skill_roots(cwd, &self.user_home())
    }

    /// 当前可见技能(rank first-wins 遮蔽:项目同名遮用户;按 name 排序)
    pub fn list(&self, cwd: &Path) -> Vec<SkillSummary> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for root in self.roots(cwd) {
            for cand in discover_root(&root) {
                let Some(summary) = self.summary_of(&cand) else {
                    continue;
                };
                if seen.insert(summary.name.clone()) {
                    out.push(summary);
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// 读完整定义(每次重读文件,正文永不缓存;名字失效/策略变化即时生效)。
    /// 未知名返回 None(调用方报 unknown);无关坏文件跳过不遮蔽。
    pub fn get(&self, cwd: &Path, name: &str) -> Option<SkillDefinition> {
        for root in self.roots(cwd) {
            for cand in discover_root(&root) {
                let Ok(text) = std::fs::read_to_string(&cand.path) else {
                    continue;
                };
                let Ok(parsed) = parse_skill_source(&text) else {
                    continue;
                };
                if parsed.name == name {
                    return Some(SkillDefinition {
                        summary: self.summary_from_parsed(&cand, parsed.clone()),
                        body: parsed.body,
                    });
                }
            }
        }
        None
    }

    /// 单候选摘要(mtime/size 缓存;解析失败缓存 None 并 warn 一次/每版本)
    fn summary_of(&self, cand: &Candidate) -> Option<SkillSummary> {
        let md = std::fs::metadata(&cand.path).ok()?;
        if !md.is_file() {
            return None;
        }
        let (size, mtime) = (md.len(), md.modified().ok());
        if let Ok(cache) = self.cache.lock()
            && let Some(hit) = cache.get(&cand.path)
            && hit.size == size
            && hit.mtime == mtime
        {
            return hit
                .parsed
                .as_ref()
                .map(|p| self.summary_from_parsed(cand, p.clone()));
        }
        let parsed = std::fs::read_to_string(&cand.path)
            .ok()
            .map(|text| match parse_skill_source(&text) {
                Ok(p) => Some(p),
                Err(e) => {
                    eprintln!(
                        "[dsh-skill] skill file {} ignored: {e}",
                        cand.path.display()
                    );
                    None
                }
            })
            .unwrap_or(None);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                cand.path.clone(),
                SummaryCache {
                    size,
                    mtime,
                    parsed: parsed.clone(),
                },
            );
        }
        parsed.map(|p| self.summary_from_parsed(cand, p))
    }

    fn summary_from_parsed(&self, cand: &Candidate, p: ParsedSkill) -> SkillSummary {
        let model_invocable = p.model_invocable();
        SkillSummary {
            name: p.name,
            description: p.description,
            when_to_use: p.when_to_use,
            path: cand.path.clone(),
            base_dir: cand.base_dir.clone(),
            rank: cand.rank,
            model_invocable,
            user_invocable: p.user_invocable,
        }
    }
}

// ── 渲染(逐字照源;模型可见文案,禁改写)────────────────────────

/// 目录 description 归一化:空白折叠 + trim + 500 字符截断(`...` 后缀)
pub fn catalog_description(value: &str, max_length: usize) -> String {
    let mut normalized = String::new();
    let mut prev_ws = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            prev_ws = true;
        } else {
            if prev_ws && !normalized.is_empty() {
                normalized.push(' ');
            }
            prev_ws = false;
            normalized.push(ch);
        }
    }
    let chars: Vec<char> = normalized.trim().chars().collect();
    if chars.len() <= max_length {
        chars.into_iter().collect()
    } else {
        let mut cut: String = chars[..max_length - 3].iter().collect();
        cut.push_str("...");
        cut
    }
}

/// escapeText(& < >;源同名函数逐字)
pub fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// escapeAttr(& " <;源同名函数逐字)
pub fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

/// 目录条目(name + 归一化 description)
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
}

pub fn catalog_entries(skills: &[SkillSummary]) -> Vec<CatalogEntry> {
    skills
        .iter()
        .map(|s| CatalogEntry {
            name: s.name.clone(),
            description: catalog_description(&s.description, CATALOG_DESCRIPTION_MAX_LENGTH),
        })
        .collect()
}

/// 首次发布的目录消息(源 renderCatalogMessage 逐字)
pub fn render_catalog_message(entries: &[CatalogEntry]) -> String {
    let mut lines: Vec<String> = vec![
        "<system-reminder>".into(),
        "A skill is a reusable set of task-specific instructions. The following skills are available in this session:".into(),
        String::new(),
        "<available_skills>".into(),
    ];
    lines.extend(render_catalog_entry_lines(entries));
    lines.extend([
        "</available_skills>".into(),
        String::new(),
        "If the user names a skill, or the task clearly matches a skill's description, call the `skill` tool with the exact skill name before taking task actions. Load all applicable skills, then follow their full instructions. This catalog contains summaries only; do not infer or follow a skill's instructions until it has been loaded.".into(),
        "A user may also invoke a skill directly; its <skill_content> block then appears in this conversation. Follow it, and do not call the `skill` tool again for that skill.".into(),
        "</system-reminder>".into(),
    ]);
    lines.join("\n")
}

/// 目录变化后的整条替换消息(源 renderCatalogUpdate 逐字;entries 可为空 = 墓碑)
pub fn render_catalog_update(entries: &[CatalogEntry]) -> String {
    let availability: [&str; 2] = if entries.is_empty() {
        [
            "No skills are currently available through the `skill` tool. Do not use names from earlier skill catalogs.",
            "A user may still invoke a skill directly; its <skill_content> block then appears in this conversation. Follow it, and do not call the `skill` tool for it.",
        ]
    } else {
        [
            "Use only names in this replacement catalog. If the user names a listed skill, or the task clearly matches its description, call the `skill` tool with the exact name before acting.",
            "A user may also invoke a skill directly; its <skill_content> block then appears in this conversation. Follow it, and do not call the `skill` tool again for that skill.",
        ]
    };
    let mut lines: Vec<String> = vec![
        "<system-reminder>".into(),
        "The available skill catalog changed. This complete catalog replaces every earlier available-skills list in this session:".into(),
        String::new(),
        "<available_skills>".into(),
    ];
    lines.extend(render_catalog_entry_lines(entries));
    lines.extend([
        "</available_skills>".into(),
        String::new(),
        availability[0].to_string(),
        availability[1].to_string(),
        "</system-reminder>".into(),
    ]);
    lines.join("\n")
}

fn render_catalog_entry_lines(entries: &[CatalogEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| format!("- `{}`: {}", e.name, escape_text(&e.description)))
        .collect()
}

/// 加载后技能正文(`<skill_content>`;源 renderSkillContent 逐字。
/// 工具结果与 `/name` 手势注入共用此形态)。`base_dir` 展示用绝对路径。
pub fn render_skill_content(name: &str, base_dir: &Path, body: &str) -> String {
    [
        format!("<skill_content name=\"{}\">", escape_attr(name)),
        "<skill_resources>".to_string(),
        format!(
            "Base directory for this skill: {}",
            escape_text(&base_dir.display().to_string())
        ),
        "Resolve relative paths mentioned by this skill against the base directory before using them. Load referenced resources only as needed.".to_string(),
        "</skill_resources>".to_string(),
        String::new(),
        "<skill_instructions>".to_string(),
        body.to_string(),
        "</skill_instructions>".to_string(),
        "</skill_content>".to_string(),
    ]
    .join("\n")
}

// ── 模型面 `skill` 工具 ──────────────────────────────────────

/// `skill` 工具:按名加载完整指令(结果 = `<skill_content>` 文本)。
/// 每会话一个(cwd = 会话工作区根);错误文案逐字照源。
pub struct SkillTool {
    service: std::sync::Arc<SkillService>,
    cwd: PathBuf,
}

impl SkillTool {
    pub fn new(service: std::sync::Arc<SkillService>, cwd: PathBuf) -> Self {
        Self { service, cwd }
    }
}

fn fail(output: String) -> ToolOutput {
    ToolOutput {
        output,
        success: false,
        ..Default::default()
    }
}

impl ToolPort for SkillTool {
    fn specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "skill",
                "description": "Load the full instructions for an available skill. Call this with the exact skill name from the session skill catalog before acting on a task that names or clearly matches that skill.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "The exact skill name from the available skills list.",
                        },
                    },
                    "required": ["name"],
                },
            },
        })]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        // OpenAI 兼容 wire 上 arguments 是 JSON 编码字符串;本地夹具是
        // 对象。两种形态都接受(dsh-tools 惯例)
        let args = if let Value::String(s) = &call.arguments {
            serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({}))
        } else {
            call.arguments.clone()
        };
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if !frontmatter::is_skill_name(name) {
            return fail(format!("invalid skill name \"{name}\""));
        }
        let Some(def) = self.service.get(&self.cwd, name) else {
            return fail(format!(
                "skill \"{name}\" is unknown or no longer available"
            ));
        };
        if !def.summary.model_invocable {
            return fail(format!(
                "skill \"{name}\" is not available for model invocation"
            ));
        }
        ToolOutput {
            output: render_skill_content(&def.summary.name, &def.summary.base_dir, &def.body),
            success: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 渲染 ─────────────────────────────────────────────────

    #[test]
    fn catalog_description_normalizes_and_truncates() {
        assert_eq!(catalog_description("  a \n\t b  ", 500), "a b");
        let long = "x".repeat(600);
        let cut = catalog_description(&long, 500);
        assert_eq!(cut.chars().count(), 500);
        assert!(cut.ends_with("..."));
        assert_eq!(catalog_description("abc", 3), "abc");
    }

    #[test]
    fn catalog_message_shape() {
        let entries = vec![CatalogEntry {
            name: "code-review".into(),
            description: "Reviews <code>".into(),
        }];
        let text = render_catalog_message(&entries);
        assert!(text.starts_with("<system-reminder>"));
        assert!(text.contains("- `code-review`: Reviews &lt;code&gt;"));
        assert!(text.ends_with("</system-reminder>"));
        // 转义只进渲染帧,不进 entries(源:published fact 不存转义)
        assert_eq!(entries[0].description, "Reviews <code>");
    }

    #[test]
    fn catalog_update_and_tombstone() {
        let entries = vec![CatalogEntry {
            name: "a".into(),
            description: "d".into(),
        }];
        let update = render_catalog_update(&entries);
        assert!(update.contains("The available skill catalog changed."));
        assert!(update.contains("Use only names in this replacement catalog."));
        let tomb = render_catalog_update(&[]);
        assert!(tomb.contains("No skills are currently available through the `skill` tool."));
        assert!(tomb.contains("do not call the `skill` tool for it."));
    }

    #[test]
    fn skill_content_block() {
        let text = render_skill_content(
            "my-skill",
            Path::new("/tmp/proj/.agents/skills/my-skill"),
            "STEP 1",
        );
        assert_eq!(
            text,
            "<skill_content name=\"my-skill\">\n<skill_resources>\nBase directory for this skill: /tmp/proj/.agents/skills/my-skill\nResolve relative paths mentioned by this skill against the base directory before using them. Load referenced resources only as needed.\n</skill_resources>\n\n<skill_instructions>\nSTEP 1\n</skill_instructions>\n</skill_content>"
        );
        let escaped = render_skill_content("a\"b", Path::new("/x"), "");
        assert!(escaped.starts_with("<skill_content name=\"a&quot;b\">"));
    }

    // ── SkillService ─────────────────────────────────────────

    struct TempHome(PathBuf);

    impl TempHome {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "dsh-skill-svc-{}-{}-{}",
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

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn skill_md(name: &str, desc: &str) -> String {
        format!("---\nname: {name}\ndescription: \"{desc}\"\n---\nbody of {name}\n")
    }

    fn service_with_home(home: &Path) -> std::sync::Arc<SkillService> {
        let svc = std::sync::Arc::new(SkillService::new());
        svc.set_user_home(Some(home.to_path_buf()));
        svc
    }

    #[test]
    fn project_shadows_user_and_sorts_by_name() {
        let home = TempHome::new("shadow");
        let ws = home.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        home.write(
            ".agents/skills/shared/SKILL.md",
            &skill_md("shared", "from-user"),
        );
        home.write(".agents/skills/zeta.md", &skill_md("zeta", "z"));
        home.write(
            "ws/.agents/skills/shared/SKILL.md",
            &skill_md("shared", "from-project"),
        );
        home.write("ws/.agents/skills/alpha.md", &skill_md("alpha", "a"));

        let svc = service_with_home(&home.0);
        let list = svc.list(&ws);
        let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "shared", "zeta"]);
        let shared = list.iter().find(|s| s.name == "shared").unwrap();
        assert_eq!(shared.description, "from-project");
        assert_eq!(shared.rank, 100);
        let zeta = list.iter().find(|s| s.name == "zeta").unwrap();
        assert_eq!(zeta.rank, 200);
    }

    #[test]
    fn get_reads_body_and_respects_policy() {
        let home = TempHome::new("get");
        let ws = home.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        home.write(
            ".agents/skills/user-only.md",
            "---\nname: user-only\ndescription: d\ndisable-model-invocation: true\n---\nSECRET-BODY",
        );
        let svc = service_with_home(&home.0);
        let list = svc.list(&ws);
        assert_eq!(list.len(), 1);
        assert!(!list[0].model_invocable);
        let def = svc.get(&ws, "user-only").unwrap();
        assert_eq!(def.body, "SECRET-BODY");
        assert_eq!(def.summary.base_dir, home.0.join(".agents/skills"));
        assert!(svc.get(&ws, "nope").is_none());
    }

    #[test]
    fn list_survives_broken_file() {
        let home = TempHome::new("broken");
        let ws = home.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        home.write(".agents/skills/bad.md", "not a skill");
        home.write(".agents/skills/good.md", &skill_md("good", "g"));
        let svc = service_with_home(&home.0);
        let list = svc.list(&ws);
        assert_eq!(
            list.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["good"]
        );
    }

    // ── SkillTool ────────────────────────────────────────────

    fn call(name: &str, args: Value) -> ToolCallRequest {
        ToolCallRequest {
            name: name.into(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn tool_loads_skill_content() {
        let home = TempHome::new("tool");
        let ws = home.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        home.write(
            "ws/.agents/skills/review/SKILL.md",
            &skill_md("review", "Reviews things"),
        );
        let svc = service_with_home(&home.0);
        let mut tool = SkillTool::new(svc, ws.clone());
        let spec = ToolPort::specs(&tool).pop().unwrap();
        assert_eq!(spec["function"]["name"], "skill");

        // 本地夹具:arguments 为对象
        let out = ToolPort::execute(&mut tool, &call("skill", json!({"name": "review"}))).await;
        assert!(out.success);
        assert!(out.output.contains("<skill_content name=\"review\">"));
        assert!(out.output.contains("body of review"));
        assert!(out.output.contains(&format!(
            "Base directory for this skill: {}",
            ws.join(".agents/skills/review").display()
        )));
    }

    #[tokio::test]
    async fn tool_accepts_json_string_arguments() {
        let home = TempHome::new("toolstr");
        let ws = home.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        home.write("ws/.agents/skills/str-arg.md", &skill_md("str-arg", "s"));
        let svc = service_with_home(&home.0);
        let mut tool = SkillTool::new(svc, ws);
        let out =
            ToolPort::execute(&mut tool, &call("skill", json!(r#"{"name":"str-arg"}"#))).await;
        assert!(out.success, "{}", out.output);
    }

    #[tokio::test]
    async fn tool_error_messages_verbatim() {
        let home = TempHome::new("toolerr");
        let ws = home.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        home.write(
            "ws/.agents/skills/disabled.md",
            "---\nname: disabled\ndescription: d\ndisable-model-invocation: true\n---\nbody",
        );
        let svc = service_with_home(&home.0);
        let mut tool = SkillTool::new(svc, ws);

        let out = ToolPort::execute(&mut tool, &call("skill", json!({"name": "Bad Name"}))).await;
        assert!(!out.success);
        assert_eq!(out.output, "invalid skill name \"Bad Name\"");

        let out = ToolPort::execute(&mut tool, &call("skill", json!({"name": "missing"}))).await;
        assert!(!out.success);
        assert_eq!(
            out.output,
            "skill \"missing\" is unknown or no longer available"
        );

        let out = ToolPort::execute(&mut tool, &call("skill", json!({"name": "disabled"}))).await;
        assert!(!out.success);
        assert_eq!(
            out.output,
            "skill \"disabled\" is not available for model invocation"
        );
    }
}
