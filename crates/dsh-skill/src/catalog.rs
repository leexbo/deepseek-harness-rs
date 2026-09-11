//! 会话目录与手势:digest 幂等的持久目录消息 + `/name` 手势注入载荷。
//!
//! 事件形态(源 tool-skill 同构):全部为 `user/message`,靠 `source.kind`
//! 区分——`skill-catalog`(form=catalog,首注/替换共载荷形,替换带
//! `update: true`,entries = [{name, description}])/ `skill-invocation`
//! (form=instructions,含 name)。`id` 由引擎补(缺省 v7),与 contexts
//! 注入同规。
//!
//! 目录幂等:digest 只对 entries 计算(JSON 逐条加界 + sha256,照源
//! digestCatalogEntries)——外框 `<system-reminder>` 文案不参与判定;
//! 无变化不重发;从未发布且目录为空不发;曾发布后删净发空墓碑。
//! 「模型只见一份目录」由 dsh-session derive_visible_messages 的
//! 「skill-catalog 保留最新一条」派生规则达成(源在 pre-step 决策里
//! 物理移除旧目录;日志只追加,这里以纯派生规则同一语义)。

use std::path::Path;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::frontmatter::is_skill_name;
use crate::{
    CatalogEntry, SkillDefinition, SkillService, catalog_entries, render_catalog_message,
    render_catalog_update, render_skill_content,
};

/// `/name` 手势正则的等价扫描:空白界定的 `/kebab` 词元(源
/// SKILL_GESTURE `/(^|\s)\/([a-z0-9]+(?:-[a-z0-9]+)*)(?=\s|$)/g`)。
/// 第二个 `/` 或任何非界字符断匹配——文件路径(`/usr/bin`)与分数
/// (`5/8`)不误伤;首见序去重由调用方跨消息合并。
pub fn scan_skill_gestures(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let at_boundary = i == 0 || chars[i - 1].is_whitespace();
        if at_boundary && chars[i] == '/' {
            let start = i + 1;
            let mut j = start;
            while j < chars.len()
                && (chars[j].is_ascii_lowercase() || chars[j].is_ascii_digit() || chars[j] == '-')
            {
                j += 1;
            }
            let candidate: String = chars[start..j].iter().collect();
            let followed_by_boundary = j == chars.len() || chars[j].is_whitespace();
            if followed_by_boundary && is_skill_name(&candidate) {
                if !names.contains(&candidate) {
                    names.push(candidate);
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    names
}

/// 本步用户面消息文本 → 手势注入载荷(排全部注入最后;源 pre-step
/// 手势监听器同构)。未知名与 user-invocable: false 保持普通散文
/// (查的是**加载后的定义**——真正产生注入的那次读取);去重按
/// 首见序跨全部文本。
pub fn gesture_payloads(service: &SkillService, cwd: &Path, texts: &[String]) -> Vec<Value> {
    let mut names: Vec<String> = Vec::new();
    for text in texts {
        for name in scan_skill_gestures(text) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
        .into_iter()
        .filter_map(|name| {
            let def: SkillDefinition = service.get(cwd, &name)?;
            if !def.summary.user_invocable {
                return None;
            }
            Some(json!({
                "content": render_skill_content(&def.summary.name, &def.summary.base_dir, &def.body),
                "source": {
                    "kind": "skill-invocation",
                    "name": name,
                    "form": "instructions",
                },
            }))
        })
        .collect()
}

/// 目录身份:entries 逐条 JSON 加界拼接后 sha256(照源
/// digestCatalogEntries——分隔符本身是合法 description 字符,只有
/// 引号加界才是精确边界)
pub fn digest_entries(entries: &[CatalogEntry]) -> String {
    let canonical: Vec<String> = entries
        .iter()
        .map(|e| {
            serde_json::to_string(&[e.name.as_str(), e.description.as_str()]).unwrap_or_default()
        })
        .collect();
    let hash = Sha256::digest(canonical.join("\n").as_bytes());
    hex::encode(hash)
}

/// 会话目录状态(published / last digest)。宿主每会话持有一份,
/// 引擎每步经 provider 调 [`Self::compose`],变化才返回载荷。
pub struct SkillCatalogState {
    published: bool,
    last_digest: Option<String>,
}

impl Default for SkillCatalogState {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillCatalogState {
    pub fn new() -> Self {
        Self {
            published: false,
            last_digest: None,
        }
    }

    /// 冷恢复:从日志事件流倒序找最近一条 skill-catalog,按其 entries
    /// 重算 digest(重开/重启后不重发现存目录)。owned 对(事件类型,
    /// 载荷)——调用方从锁内克隆,恢复只在 attach 跑一次。
    pub fn restore_from_log<I>(state: &mut Self, events: I)
    where
        I: IntoIterator,
        I::IntoIter: DoubleEndedIterator<Item = (String, Value)>,
    {
        for (ty, data) in events.into_iter().rev() {
            if ty != "user/message" {
                continue;
            }
            if data["source"]["kind"].as_str() != Some("skill-catalog") {
                continue;
            }
            if let Some(entries) = read_entries(&data["source"]["entries"]) {
                state.published = true;
                state.last_digest = Some(digest_entries(&entries));
                return;
            }
        }
    }

    /// 每步组合:返回注入载荷(user/message 全量,content + source)或
    /// None(无变化 / 从未发布且为空)。
    pub fn compose(&mut self, service: &SkillService, cwd: &Path) -> Option<Value> {
        let skills: Vec<_> = service
            .list(cwd)
            .into_iter()
            .filter(|s| s.model_invocable)
            .collect();
        let entries = catalog_entries(&skills);
        let digest = digest_entries(&entries);
        if self.last_digest.as_deref() == Some(digest.as_str()) {
            return None;
        }
        if !self.published && entries.is_empty() {
            return None;
        }
        let (text, is_update) = if self.published {
            (render_catalog_update(&entries), true)
        } else {
            (render_catalog_message(&entries), false)
        };
        self.published = true;
        self.last_digest = Some(digest);
        let mut source = json!({
            "kind": "skill-catalog",
            "form": "catalog",
            "entries": entries,
        });
        if is_update {
            source["update"] = json!(true);
        }
        Some(json!({ "content": text, "source": source }))
    }
}

/// 读一条目录 source 的 entries(非数组/字段缺失 = None,视作非本系统
/// 目录,冷恢复跳过——照源 readCatalogEntries 的防御姿态)
fn read_entries(value: &Value) -> Option<Vec<CatalogEntry>> {
    let arr = value.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let name = item["name"].as_str()?;
        if name.is_empty() {
            return None;
        }
        out.push(CatalogEntry {
            name: name.to_string(),
            description: item["description"].as_str()?.to_string(),
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::parse_skill_source;
    use std::path::PathBuf;

    fn skill_md(name: &str, desc: &str, extra: &str) -> String {
        format!("---\nname: {name}\ndescription: \"{desc}\"{extra}\n---\nbody of {name}\n")
    }

    fn svc_entry(src: &str) -> CatalogEntry {
        let p = parse_skill_source(src).unwrap();
        CatalogEntry {
            name: p.name,
            description: crate::catalog_description(&p.description, 500),
        }
    }

    struct TempHome(PathBuf);

    impl TempHome {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "dsh-skill-cat-{}-{}-{}",
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
        fn write(&self, rel: &str, body: &str) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn service_with_home(home: &Path) -> SkillService {
        let svc = SkillService::new();
        svc.set_user_home(Some(home.to_path_buf()));
        svc
    }

    #[test]
    fn gesture_scan_bounded_tokens_only() {
        assert_eq!(scan_skill_gestures("/review please"), vec!["review"]);
        assert_eq!(scan_skill_gestures("do /review now"), vec!["review"]);
        // 首见序去重
        assert_eq!(
            scan_skill_gestures("/b then /a then /a and /b"),
            vec!["b", "a"]
        );
        // 路径/分数不误伤
        assert!(scan_skill_gestures("see /usr/bin and 5/8").is_empty());
        // kebab;句尾标点非边界字符 → 断匹配(照源正则 (?=\s|$))
        assert_eq!(scan_skill_gestures("run /my-skill"), vec!["my-skill"]);
        assert!(scan_skill_gestures("run /my-skill.").is_empty());
        // 非法词元不产出
        assert!(scan_skill_gestures("/-bad /Bad /").is_empty());
    }

    #[test]
    fn gesture_skips_unknown_and_user_disabled() {
        let tmp = TempHome::new("gesture");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        tmp.write(".agents/skills/ok.md", &skill_md("ok", "d", ""));
        tmp.write(
            ".agents/skills/modelonly.md",
            &skill_md("modelonly", "d", "\nuser-invocable: false"),
        );
        let svc = service_with_home(&tmp.0);
        let payloads = gesture_payloads(&svc, &ws, &["do /ok and /missing and /modelonly".into()]);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["source"]["kind"], "skill-invocation");
        assert_eq!(payloads[0]["source"]["name"], "ok");
        assert_eq!(payloads[0]["source"]["form"], "instructions");
        let content = payloads[0]["content"].as_str().unwrap();
        assert!(content.contains("<skill_content name=\"ok\">"));
        assert!(content.contains("body of ok"));
    }

    #[test]
    fn digest_stable_and_entry_sensitive() {
        let e = svc_entry(&skill_md("a", "d", ""));
        let d1 = digest_entries(std::slice::from_ref(&e));
        let d2 = digest_entries(&[e]);
        assert_eq!(d1, d2);
        let d3 = digest_entries(&[svc_entry(&skill_md("a", "other", ""))]);
        assert_ne!(d1, d3);
        // 分隔符歧义:("a","b")+( "c|d") 与 ("a","b|c")+("d") 必不同
        let mk = |n: &str, d: &str| CatalogEntry {
            name: n.into(),
            description: d.into(),
        };
        assert_ne!(
            digest_entries(&[mk("a", "b"), mk("c|d", "x")]),
            digest_entries(&[mk("a", "b|c"), mk("d", "x")])
        );
    }

    #[test]
    fn compose_first_change_tombstone_none() {
        let tmp = TempHome::new("compose");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let svc = service_with_home(&tmp.0);
        let mut st = SkillCatalogState::new();

        // 空且从未发布 → 不发
        assert!(st.compose(&svc, &ws).is_none());

        // 首注
        tmp.write(".agents/skills/alpha.md", &skill_md("alpha", "a", ""));
        let first = st.compose(&svc, &ws).expect("首注");
        assert_eq!(first["source"]["kind"], "skill-catalog");
        assert_eq!(first["source"]["form"], "catalog");
        assert!(first["source"]["update"].is_null());
        assert!(
            first["content"]
                .as_str()
                .unwrap()
                .contains("available in this session")
        );
        assert_eq!(first["source"]["entries"][0]["name"], "alpha");

        // 无变化 → 不发
        assert!(st.compose(&svc, &ws).is_none());

        // 变化 → 整条替换 + update 标记
        tmp.write(".agents/skills/beta.md", &skill_md("beta", "b", ""));
        let second = st.compose(&svc, &ws).expect("替换");
        assert_eq!(second["source"]["update"], true);
        let text = second["content"].as_str().unwrap();
        assert!(text.contains("The available skill catalog changed."));
        assert!(text.contains("`beta`"));

        // 删净 → 空墓碑
        std::fs::remove_dir_all(tmp.0.join(".agents/skills")).unwrap();
        let tomb = st.compose(&svc, &ws).expect("墓碑");
        assert_eq!(tomb["source"]["entries"].as_array().unwrap().len(), 0);
        assert!(
            tomb["content"]
                .as_str()
                .unwrap()
                .contains("No skills are currently available")
        );
    }

    #[test]
    fn restore_from_log_suppresses_republish() {
        let tmp = TempHome::new("restore");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        tmp.write(".agents/skills/alpha.md", &skill_md("alpha", "a", ""));
        let svc = service_with_home(&tmp.0);
        let mut st = SkillCatalogState::new();
        let first = st.compose(&svc, &ws).expect("首注");

        // 模拟重启:新状态 + 日志倒序恢复(事件载荷 = compose 产出的
        // {content, source} 全量,与引擎落档的 user/message data 同形)
        let payload = first;
        let events = vec![("user/message".to_string(), payload)];
        let mut restored = SkillCatalogState::new();
        SkillCatalogState::restore_from_log(&mut restored, events);
        assert!(restored.compose(&svc, &ws).is_none(), "恢复后不重发");

        // 加了新技能 → 恢复后的状态照样发替换
        tmp.write(".agents/skills/beta.md", &skill_md("beta", "b", ""));
        let second = restored.compose(&svc, &ws).expect("替换");
        assert_eq!(second["source"]["update"], true);
    }

    #[test]
    fn restore_ignores_invalid_catalog_records() {
        let mut st = SkillCatalogState::new();
        let bad = vec![
            (
                "user/message".to_string(),
                json!({"source": {"kind": "skill-catalog"}}),
            ),
            ("turn/start".to_string(), json!({})),
        ];
        SkillCatalogState::restore_from_log(&mut st, bad);
        assert!(!st.published);
    }
}
