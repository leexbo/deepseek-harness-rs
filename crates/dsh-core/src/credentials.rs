//! 凭据解析。
//!
//! 凭证明文存放于设置文件(`settings.yaml` 的 provider `api_key`
//! 字段);解析链(高到低):
//!
//! 1. 显式注入(启动参数已解析的 key,最高优先);
//! 2. provider 条目 `api_key`(设置文件直存,设置页录入即写此处);
//! 3. 显式引用(`env:NAME`)——用户意志:引用可解析但环境变量缺席
//!    即缺席,**不**静默回落;字面量不可解析则跳过该级继续;
//! 4. 默认环境变量名(`{PROVIDER 大写}_API_KEY`,如 deepseek →
//!    `DEEPSEEK_API_KEY`)。
//!
//! 各级缺席返回 None(不报错)——凭据缺席的失败留给装配层拒绝。

use crate::settings::ProviderEntry;

/// 凭据引用(设置文件里的 `credential_ref` 字面量解析结果)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialRef {
    /// 环境变量名
    Env(String),
}

/// 解析引用字面量:`env:NAME`
pub fn parse_credential_ref(s: &str) -> Option<CredentialRef> {
    let (kind, rest) = s.split_once(':')?;
    match kind {
        "env" if !rest.is_empty() => Some(CredentialRef::Env(rest.into())),
        _ => None,
    }
}

/// provider 默认环境变量名(deepseek → DEEPSEEK_API_KEY;其余 →
/// `{ID 大写,连字符转下划线}_API_KEY`——与既有 DEEPSEEK_API_KEY 约定一致)
pub fn default_env_name(provider_id: &str) -> String {
    format!("{}_API_KEY", provider_id.to_uppercase().replace('-', "_"))
}

/// 凭据链解析(显式注入 > 设置文件 `api_key` > `env:` 引用 > 默认环境变量名)
pub fn resolve_credential(explicit: Option<&str>, provider: &ProviderEntry) -> Option<String> {
    // 1. 显式注入
    if let Some(k) = explicit
        && !k.is_empty()
    {
        return Some(k.to_string());
    }
    // 2. 设置文件直存
    if let Some(k) = &provider.api_key
        && !k.is_empty()
    {
        return Some(k.clone());
    }
    // 3. 显式引用(可解析而变量缺席 = 用户意志缺席,不回落)
    if let Some(refstr) = &provider.credential_ref
        && let Some(CredentialRef::Env(name)) = parse_credential_ref(refstr)
    {
        return std::env::var(&name).ok().filter(|v| !v.is_empty());
    }
    // 4. 默认环境变量名
    let env_name = default_env_name(&provider.id);
    std::env::var(&env_name).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试 provider(默认链环境变量名 = {ID 大写}_API_KEY;
    /// 各用例用不同 id,避免并行测试互踩进程级环境变量)
    fn provider_named(id: &str, cred_ref: Option<&str>, api_key: Option<&str>) -> ProviderEntry {
        ProviderEntry {
            id: id.into(),
            base_url: format!("https://{id}.example/v1"),
            dialect: "openai-chat".into(),
            credential_ref: cred_ref.map(String::from),
            api_key: api_key.map(String::from),
            default_model: None,
            ..crate::settings::builtin_provider()
        }
    }

    /// 引用字面量解析:合法与非法形态
    #[test]
    fn parse_refs() {
        assert_eq!(
            parse_credential_ref("env:ACME_API_KEY"),
            Some(CredentialRef::Env("ACME_API_KEY".into()))
        );
        for bad in [
            "",
            "env:",
            "keychain:dsh/acme",
            "dotenv:X",
            "plain",
            "file:/etc/x",
        ] {
            assert_eq!(parse_credential_ref(bad), None, "{bad} 应不可解析");
        }
    }

    /// 默认环境变量名映射
    #[test]
    fn env_names() {
        assert_eq!(default_env_name("deepseek"), "DEEPSEEK_API_KEY");
        assert_eq!(default_env_name("my-prov"), "MY_PROV_API_KEY");
    }

    /// 显式注入最高优先
    #[test]
    fn explicit_wins() {
        let p = provider_named("dshwin", None, Some("from-settings"));
        assert_eq!(
            resolve_credential(Some("explicit"), &p).as_deref(),
            Some("explicit")
        );
    }

    /// 设置文件 api_key 次之;空串视同缺席
    #[test]
    fn settings_key_beats_ref() {
        let p = provider_named("dshset", Some("env:DSH_SET_REF_VAR"), Some("from-settings"));
        assert_eq!(
            resolve_credential(None, &p).as_deref(),
            Some("from-settings")
        );
        let empty = provider_named("dshset2", Some("env:DSH_SET_REF_VAR"), Some(""));
        unsafe { std::env::set_var("DSH_SET_REF_VAR", "from-ref") };
        assert_eq!(
            resolve_credential(None, &empty).as_deref(),
            Some("from-ref")
        );
        unsafe { std::env::remove_var("DSH_SET_REF_VAR") };
    }

    /// env: 引用可解析而变量缺席 = 缺席,不回落默认链;不可解析字面量跳过该级
    #[test]
    fn ref_semantics() {
        let var = default_env_name("dshref");
        unsafe { std::env::set_var(&var, "from-default") };
        let dangling = provider_named("dshref", Some("env:DSH_REF_MISSING_VAR"), None);
        assert_eq!(resolve_credential(None, &dangling), None);
        let unparseable = provider_named("dshref", Some("keychain:dsh/x"), None);
        assert_eq!(
            resolve_credential(None, &unparseable).as_deref(),
            Some("from-default")
        );
        unsafe { std::env::remove_var(&var) };
    }

    /// 默认环境变量名兜底
    #[test]
    fn default_env_fallback() {
        let p = provider_named("dshdef", None, None);
        let var = default_env_name("dshdef");
        unsafe { std::env::set_var(&var, "from-env") };
        assert_eq!(resolve_credential(None, &p).as_deref(), Some("from-env"));
        unsafe { std::env::remove_var(&var) };
        assert_eq!(resolve_credential(None, &p), None);
    }
}
