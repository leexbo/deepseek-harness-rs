//! SKILL.md 解析:手写栅栏扫描 + serde_norway(源 skill-filesystem
//! parseSkillFile 同构:首行恰 `---`、逐行找闭合、CRLF 兼容)。
//!
//! 失败 warn-and-skip:解析错误以 Err 文案上行,调用方按文件粒度打日志
//! 跳过,单文件坏不影响其余技能。正文(闭合栅栏之后)仅 trim,无截断
//! ——技能是可信本地内容。

use serde_norway::Value as Yaml;

/// 解析后的 SKILL.md(frontmatter 字段 + 正文)
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSkill {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    /// 任意 object,原样透传、不进模型面(源 metadata 语义)
    pub metadata: Option<serde_json::Value>,
    pub disable_model_invocation: bool,
    pub user_invocable: bool,
    /// 闭合 `---` 之后的全部内容(trim,无截断)
    pub body: String,
}

/// 解析一段 SKILL.md 源文本(错误文案逐字照源)
pub fn parse_skill_source(source: &str) -> Result<ParsedSkill, String> {
    let lines: Vec<&str> = source.split('\n').collect();
    if lines.first().copied().unwrap_or("").trim_end_matches('\r') != "---" {
        return Err("missing YAML frontmatter".into());
    }
    let close_idx = lines[1..]
        .iter()
        .position(|l| l.trim_end_matches('\r') == "---")
        .map(|i| i + 1)
        .ok_or_else(|| "missing YAML frontmatter".to_string())?;
    let front_text = lines[1..close_idx].join("\n");
    let body = lines[close_idx + 1..].join("\n").trim().to_string();

    let yaml: Yaml = serde_norway::from_str(&front_text)
        .map_err(|e| format!("invalid YAML frontmatter: {e}"))?;
    let Yaml::Mapping(map) = yaml else {
        return Err("invalid YAML frontmatter: frontmatter must be a mapping".into());
    };

    // 旧驼峰键直接拒绝(源 rejectLegacyInvocationKey 逐字)
    for legacy in ["disableModelInvocation", "userInvocable"] {
        if map.contains_key(Yaml::String(legacy.into())) {
            return Err(format!(
                "frontmatter field \"{legacy}\" is unsupported; use \"{}\"",
                match legacy {
                    "disableModelInvocation" => "disable-model-invocation",
                    _ => "user-invocable",
                }
            ));
        }
    }

    let get_str = |key: &str| -> Option<Option<String>> {
        match map.get(Yaml::String(key.into())) {
            Some(Yaml::String(s)) => Some(Some(s.clone())),
            Some(Yaml::Null) | None => Some(None),
            _ => None, // 类型错
        }
    };
    let name = get_str("name")
        .ok_or_else(|| "invalid YAML frontmatter: field \"name\" must be a string".to_string())?
        .ok_or_else(|| "frontmatter requires name and description".to_string())?;
    let description = get_str("description")
        .ok_or_else(|| {
            "invalid YAML frontmatter: field \"description\" must be a string".to_string()
        })?
        .filter(|d| !d.trim().is_empty())
        .ok_or_else(|| "frontmatter requires name and description".to_string())?;
    if !is_skill_name(&name) {
        return Err(format!("invalid skill name \"{name}\""));
    }
    let when_to_use = get_str("whenToUse").ok_or_else(|| {
        "invalid YAML frontmatter: field \"whenToUse\" must be a string".to_string()
    })?;
    let metadata = match map.get(Yaml::String("metadata".into())) {
        None | Some(Yaml::Null) => None,
        Some(v) => Some(
            serde_json::to_value(v)
                .map_err(|e| format!("invalid YAML frontmatter: metadata: {e}"))?,
        ),
    };

    let disable_model_invocation = invocation_bool(&map, "disable-model-invocation", false)?;
    let user_invocable = invocation_bool(&map, "user-invocable", true)?;

    Ok(ParsedSkill {
        name,
        description,
        when_to_use,
        metadata,
        disable_model_invocation,
        user_invocable,
        body,
    })
}

/// frontmatter 布尔:true/false/yes/no/on/off/1/0(大小写不敏感,源
/// frontmatterBoolean 词表);缺席 = 缺省,其他类型 = 错
fn invocation_bool(map: &serde_norway::Mapping, key: &str, default: bool) -> Result<bool, String> {
    match map.get(Yaml::String(key.into())) {
        None | Some(Yaml::Null) => Ok(default),
        Some(v) => bool_of_yaml(v).ok_or_else(|| {
            format!("invalid invocation frontmatter: field \"{key}\" must be a boolean")
        }),
    }
}

fn bool_of_yaml(v: &Yaml) -> Option<bool> {
    match v {
        Yaml::Bool(b) => Some(*b),
        Yaml::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        Yaml::Number(n) => match n.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// 技能名语法:`^[a-z0-9]+(?:-[a-z0-9]+)*$`(源 SKILL_NAME;无 regex 依赖)
pub fn is_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && name.split('-').all(|part| !part.is_empty())
}

impl ParsedSkill {
    /// 模型可调用 = 未显式禁用(源 isModelInvocable)
    pub fn model_invocable(&self) -> bool {
        !self.disable_model_invocation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "---\nname: my-skill\ndescription: Does a thing\n---\n\nBody here.\n";

    #[test]
    fn parses_valid_skill() {
        let p = parse_skill_source(VALID).unwrap();
        assert_eq!(p.name, "my-skill");
        assert_eq!(p.description, "Does a thing");
        assert_eq!(p.body, "Body here.");
        assert!(p.model_invocable());
        assert!(p.user_invocable);
        assert!(p.when_to_use.is_none());
        assert!(p.metadata.is_none());
    }

    #[test]
    fn optional_fields_and_defaults() {
        let src = "---\nname: a\ndescription: b\nwhenToUse: When c\nmetadata:\n  owner: x\ndisable-model-invocation: yes\nuser-invocable: no\n---\nbody";
        let p = parse_skill_source(src).unwrap();
        assert_eq!(p.when_to_use.as_deref(), Some("When c"));
        assert!(!p.model_invocable());
        assert!(!p.user_invocable);
        assert_eq!(p.metadata.as_ref().unwrap()["owner"], "x");
    }

    #[test]
    fn bool_spellings() {
        for (raw, want) in [
            ("true", true),
            ("TRUE", true),
            ("yes", true),
            ("On", true),
            ("1", true),
            ("false", false),
            ("No", false),
            ("OFF", false),
            ("0", false),
        ] {
            let src = format!("---\nname: a\ndescription: b\nuser-invocable: {raw}\n---\n");
            assert_eq!(
                parse_skill_source(&src).unwrap().user_invocable,
                want,
                "{raw}"
            );
        }
    }

    #[test]
    fn crlf_fences() {
        let src = "---\r\nname: a\r\ndescription: b\r\n---\r\nbody line\r\n";
        let p = parse_skill_source(src).unwrap();
        assert_eq!(p.name, "a");
        assert_eq!(p.body, "body line");
    }

    #[test]
    fn missing_or_unclosed_frontmatter() {
        assert_eq!(
            parse_skill_source("just text").unwrap_err(),
            "missing YAML frontmatter"
        );
        assert_eq!(
            parse_skill_source("---\nname: a\n").unwrap_err(),
            "missing YAML frontmatter"
        );
    }

    #[test]
    fn requires_name_and_description() {
        let no_desc = parse_skill_source("---\nname: a\n---\nbody").unwrap_err();
        assert_eq!(no_desc, "frontmatter requires name and description");
        let no_name = parse_skill_source("---\ndescription: d\n---\nbody").unwrap_err();
        assert_eq!(no_name, "frontmatter requires name and description");
        let empty_desc =
            parse_skill_source("---\nname: a\ndescription: \"   \"\n---\n").unwrap_err();
        assert_eq!(empty_desc, "frontmatter requires name and description");
    }

    #[test]
    fn invalid_names_rejected() {
        for bad in [
            "UPPER",
            "has space",
            "-lead",
            "trail-",
            "double--dash",
            "a_b",
        ] {
            let src = format!("---\nname: {bad}\ndescription: d\n---\n");
            assert_eq!(
                parse_skill_source(&src).unwrap_err(),
                format!("invalid skill name \"{bad}\""),
                "{bad}"
            );
        }
        assert!(is_skill_name("a"));
        assert!(is_skill_name("my-skill-2"));
    }

    #[test]
    fn legacy_camel_case_rejected() {
        let src = "---\nname: a\ndescription: d\ndisableModelInvocation: true\n---\n";
        assert_eq!(
            parse_skill_source(src).unwrap_err(),
            "frontmatter field \"disableModelInvocation\" is unsupported; use \"disable-model-invocation\""
        );
        let src = "---\nname: a\ndescription: d\nuserInvocable: true\n---\n";
        assert_eq!(
            parse_skill_source(src).unwrap_err(),
            "frontmatter field \"userInvocable\" is unsupported; use \"user-invocable\""
        );
    }

    #[test]
    fn invalid_yaml_and_types() {
        assert!(
            parse_skill_source("---\nname: [unclosed\n---\n")
                .unwrap_err()
                .starts_with("invalid YAML frontmatter")
        );
        let src = "---\nname: a\ndescription: d\nuser-invocable: maybe\n---\n";
        assert_eq!(
            parse_skill_source(src).unwrap_err(),
            "invalid invocation frontmatter: field \"user-invocable\" must be a boolean"
        );
    }
}
