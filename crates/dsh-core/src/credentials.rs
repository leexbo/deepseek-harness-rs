//! 凭据 seam(含 macOS Keychain)。
//!
//! 引用-记录二分:设置(`settings.json` 的 provider `credential_ref`)只存
//! 引用,明文只存在于运行时提供方(环境变量 / 工作区 `.env` / 钥匙串)。
//! 解析四级链:
//!
//! 1. 显式注入(启动参数已解析的 key,最高优先);
//! 2. 显式引用(`env:NAME` / `dotenv:NAME` / `keychain:SERVICE/ACCOUNT`)
//!    ——用户意志,失败即缺席,**不**静默回落默认链;
//! 3. 默认环境变量名(`{PROVIDER 大写}_API_KEY`,如 deepseek →
//!    `DEEPSEEK_API_KEY`,与历史行为一致):env > 工作区 `.env`;
//! 4. 钥匙串默认槽(service `dsh` / account provider id)。
//!
//! 钥匙串经 [`KeychainPort`] 端口抽象:生产为 macOS Security framework
//! (`security-framework` crate),非 macOS 平台返回不支持(该级缺席,
//! 链上继续);测试注入内存实现。无后端错误不 panic、不阻断——凭据
//! 缺席的失败留给装配层拒绝。

use std::path::Path;

use crate::settings::ProviderEntry;

/// 钥匙串端口(测试注入;生产见 [`OsKeychain`])
pub trait KeychainPort: Send + Sync {
    /// 读通用密码记录(不存在/平台不支持 → Err)
    fn get(&self, service: &str, account: &str) -> anyhow::Result<String>;
    /// 写入(已存在即覆盖)
    fn set(&self, service: &str, account: &str, secret: &str) -> anyhow::Result<()>;
    /// 删除(不存在视为成功——幂等)
    fn delete(&self, service: &str, account: &str) -> anyhow::Result<()>;
}

/// 生产钥匙串端口:macOS Security framework 的 generic password;
/// 非 macOS 平台三级操作全部「平台不支持」。
pub struct OsKeychain;

#[cfg(target_os = "macos")]
impl KeychainPort for OsKeychain {
    fn get(&self, service: &str, account: &str) -> anyhow::Result<String> {
        let bytes = security_framework::passwords::get_generic_password(service, account)?;
        String::from_utf8(bytes).map_err(|e| anyhow::anyhow!("钥匙串记录非 UTF-8:{e}"))
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> anyhow::Result<()> {
        security_framework::passwords::set_generic_password(service, account, secret.as_bytes())
            .map_err(Into::into)
    }

    fn delete(&self, service: &str, account: &str) -> anyhow::Result<()> {
        security_framework::passwords::delete_generic_password(service, account).map_err(Into::into)
    }
}

#[cfg(not(target_os = "macos"))]
impl KeychainPort for OsKeychain {
    fn get(&self, _service: &str, _account: &str) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("钥匙串仅 macOS 支持"))
    }

    fn set(&self, _service: &str, _account: &str, _secret: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("钥匙串仅 macOS 支持"))
    }

    fn delete(&self, _service: &str, _account: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("钥匙串仅 macOS 支持"))
    }
}

/// 凭据落盘目标(录入面;`AppHost::store_credential` 消费)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStore {
    /// 钥匙串默认槽(service `dsh` / account provider id)
    Keychain,
    /// 工作区 `.env` 的默认变量名行
    Dotenv,
}

/// 凭据引用(设置文件里的 `credential_ref` 字面量解析结果)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialRef {
    /// 环境变量名
    Env(String),
    /// 工作区 `.env` 键名
    Dotenv(String),
    /// 钥匙串槽位
    Keychain {
        /// service 名
        service: String,
        /// account 名
        account: String,
    },
}

/// 解析引用字面量:`env:NAME` / `dotenv:NAME` / `keychain:SERVICE/ACCOUNT`
pub fn parse_credential_ref(s: &str) -> Option<CredentialRef> {
    let (kind, rest) = s.split_once(':')?;
    match kind {
        "env" if !rest.is_empty() => Some(CredentialRef::Env(rest.into())),
        "dotenv" if !rest.is_empty() => Some(CredentialRef::Dotenv(rest.into())),
        "keychain" => {
            let (service, account) = rest.split_once('/')?;
            if service.is_empty() || account.is_empty() {
                return None;
            }
            Some(CredentialRef::Keychain {
                service: service.into(),
                account: account.into(),
            })
        }
        _ => None,
    }
}

/// provider 默认环境变量名(deepseek → DEEPSEEK_API_KEY;其余 →
/// `{ID 大写,连字符转下划线}_API_KEY`——与既有 DEEPSEEK_API_KEY 约定一致)
pub fn default_env_name(provider_id: &str) -> String {
    format!("{}_API_KEY", provider_id.to_uppercase().replace('-', "_"))
}

/// 钥匙串默认槽的 service 名
pub const KEYCHAIN_SERVICE: &str = "dsh";

/// 四级链解析(返回 None = 各级均缺席;不报错——装配层决定缺席语义)
pub fn resolve_credential(
    explicit: Option<&str>,
    provider: &ProviderEntry,
    workspace: &Path,
    keychain: &dyn KeychainPort,
) -> Option<String> {
    // 1. 显式注入
    if let Some(k) = explicit
        && !k.is_empty()
    {
        return Some(k.to_string());
    }
    // 2. 显式引用(用户意志:失败即缺席,不回落)
    if let Some(refstr) = &provider.credential_ref
        && let Some(parsed) = parse_credential_ref(refstr)
    {
        return match parsed {
            CredentialRef::Env(name) => std::env::var(&name).ok().filter(|v| !v.is_empty()),
            CredentialRef::Dotenv(name) => dsh_app::dotenv_key_at(workspace, &name),
            CredentialRef::Keychain { service, account } => {
                match keychain.get(&service, &account) {
                    Ok(secret) => Some(secret),
                    Err(e) => {
                        eprintln!("[dsh-core] 钥匙串读取 {service}/{account} 失败:{e}");
                        None
                    }
                }
            }
        };
    }
    // 3. 默认环境变量名:env > 工作区 .env
    let env_name = default_env_name(&provider.id);
    if let Some(v) = std::env::var(&env_name).ok().filter(|v| !v.is_empty()) {
        return Some(v);
    }
    if let Some(v) = dsh_app::dotenv_key_at(workspace, &env_name) {
        return Some(v);
    }
    // 4. 钥匙串默认槽(记录不存在/平台不支持:安静缺席,诊断在设置页呈现)
    keychain.get(KEYCHAIN_SERVICE, &provider.id).ok()
}

/// 内存钥匙串(测试/演示注入)
#[derive(Default)]
pub struct InMemoryKeychain {
    entries: std::sync::Mutex<std::collections::HashMap<(String, String), String>>,
}

impl InMemoryKeychain {
    /// 空实例
    pub fn new() -> Self {
        Self::default()
    }
}

impl KeychainPort for InMemoryKeychain {
    fn get(&self, service: &str, account: &str) -> anyhow::Result<String> {
        self.entries
            .lock()
            .expect("内存钥匙串锁中毒(测试 bug)")
            .get(&(service.into(), account.into()))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("记录不存在"))
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> anyhow::Result<()> {
        self.entries
            .lock()
            .expect("内存钥匙串锁中毒(测试 bug)")
            .insert((service.into(), account.into()), secret.into());
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> anyhow::Result<()> {
        self.entries
            .lock()
            .expect("内存钥匙串锁中毒(测试 bug)")
            .remove(&(service.into(), account.into()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试 provider(默认链环境变量名 = {ID 大写}_API_KEY;
    /// 各用例用不同 id,避免并行测试互踩进程级环境变量)
    fn provider_named(id: &str, cred_ref: Option<&str>) -> ProviderEntry {
        ProviderEntry {
            id: id.into(),
            base_url: format!("https://{id}.example/v1"),
            dialect: "openai-chat".into(),
            credential_ref: cred_ref.map(String::from),
            default_model: None,
            ..crate::settings::builtin_provider()
        }
    }

    fn ws() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("dsh-cred-{}", uuid::Uuid::new_v4().simple()))
    }

    /// 引用字面量解析:三种合法形态 + 非法形态
    #[test]
    fn parse_refs() {
        assert_eq!(
            parse_credential_ref("env:ACME_API_KEY"),
            Some(CredentialRef::Env("ACME_API_KEY".into()))
        );
        assert_eq!(
            parse_credential_ref("dotenv:ACME_API_KEY"),
            Some(CredentialRef::Dotenv("ACME_API_KEY".into()))
        );
        assert_eq!(
            parse_credential_ref("keychain:dsh/acme"),
            Some(CredentialRef::Keychain {
                service: "dsh".into(),
                account: "acme".into()
            })
        );
        for bad in [
            "",
            "env:",
            "keychain:noslash",
            "keychain:/acct",
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

    /// 四级链:显式注入最高优先
    #[test]
    fn explicit_wins() {
        let p = provider_named("dshwin", None);
        let var = default_env_name("dshwin");
        unsafe { std::env::set_var(&var, "from-env") };
        let kc = InMemoryKeychain::new();
        kc.set("dsh", "dshwin", "from-keychain").unwrap();
        assert_eq!(
            resolve_credential(Some("explicit"), &p, &ws(), &kc).as_deref(),
            Some("explicit")
        );
        unsafe { std::env::remove_var(&var) };
    }

    /// 默认链次序:env > 工作区 .env > 钥匙串默认槽
    #[test]
    fn default_chain_order() {
        let dir = ws();
        std::fs::create_dir_all(&dir).unwrap();
        let p = provider_named("dshtest", None);
        let var = default_env_name("dshtest");
        let kc = InMemoryKeychain::new();

        unsafe { std::env::remove_var(&var) };
        std::fs::write(dir.join(".env"), format!("{var}=from-dotenv\n")).unwrap();
        kc.set("dsh", "dshtest", "from-keychain").unwrap();
        assert_eq!(
            resolve_credential(None, &p, &dir, &kc).as_deref(),
            Some("from-dotenv")
        );

        unsafe { std::env::set_var(&var, "from-env") };
        assert_eq!(
            resolve_credential(None, &p, &dir, &kc).as_deref(),
            Some("from-env")
        );

        unsafe { std::env::remove_var(&var) };
        std::fs::remove_file(dir.join(".env")).unwrap();
        assert_eq!(
            resolve_credential(None, &p, &dir, &kc).as_deref(),
            Some("from-keychain")
        );

        // 全部缺席 → None(不报错)
        let empty = InMemoryKeychain::new();
        assert_eq!(resolve_credential(None, &p, &dir, &empty), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 显式引用是用户意志:钥匙串引用失败即缺席,不回落默认链
    #[test]
    fn explicit_ref_does_not_fallthrough() {
        let p = provider_named("dshref", Some("keychain:dsh/dshref"));
        let var = default_env_name("dshref");
        unsafe { std::env::set_var(&var, "from-default-env") };
        let empty = InMemoryKeychain::new();
        assert_eq!(resolve_credential(None, &p, &ws(), &empty), None);
        unsafe { std::env::remove_var(&var) };
    }

    /// keychain: 引用命中默认链之前的槽位
    #[test]
    fn keychain_ref_resolves() {
        let p = provider_named("dshkc", Some("keychain:custom/slot"));
        let kc = InMemoryKeychain::new();
        kc.set("custom", "slot", "secret").unwrap();
        assert_eq!(
            resolve_credential(None, &p, &ws(), &kc).as_deref(),
            Some("secret")
        );
    }

    /// env:/dotenv: 引用解析
    #[test]
    fn env_and_dotenv_refs() {
        let var = "DSH_TEST_REF_VAR";
        unsafe { std::env::set_var(var, "v1") };
        assert_eq!(
            resolve_credential(
                None,
                &provider_named("dshenv", Some("env:DSH_TEST_REF_VAR")),
                &ws(),
                &InMemoryKeychain::new()
            )
            .as_deref(),
            Some("v1")
        );
        unsafe { std::env::remove_var(var) };

        let dir = ws();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "DSH_TEST_REF_DOT=v2\n").unwrap();
        assert_eq!(
            resolve_credential(
                None,
                &provider_named("dshdot", Some("dotenv:DSH_TEST_REF_DOT")),
                &dir,
                &InMemoryKeychain::new()
            )
            .as_deref(),
            Some("v2")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
