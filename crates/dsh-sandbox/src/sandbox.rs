//! OS 沙箱链:bwrap → Landlock / Seatbelt,fail-closed。
//!
//! 链语义:
//! - Linux:bwrap 可用则优先(命名空间重绘文件系统),否则 Landlock
//!   (`landlock` crate 经 pre_exec 自限制再 exec,无 C 交付物);
//! - macOS:Seatbelt(`sandbox-exec` SBPL);
//! - 探测不到可用 rung 且策略要求沙箱 → **拒绝执行**,绝不静默降级。
//!
//! 语义组织:
//! - 策略为 mode 三态(见 [`SandboxMode`]),可写根单源推导
//!   ([`SandboxPolicy::writable_roots`]),替代裸路径清单——装配层不再
//!   手拼根列表,各 rung 方言由同一推导喂给;
//! - 探测为**功能式**(真 profile 跑 `true`,非 which/`--version` 存在式),
//!   一次探测缓存为 rung;
//! - 每个 wrap 携带 enforcement 完整性声明 + 拒绝方言(dialect)表 +
//!   runner 失败识别规则(`enforcement`/`denialSignatures`/
//!   `runnerFailureRules`)。
//!
//! 执行世界能力束在此落最小版:目录视图(可写根)+ spawn 策略。
//! rung 形态二分:bwrap/seatbelt 经 [`wrap_argv`] 重建 argv;
//! landlock 经 [`landlock_pre_exec`] 在 exec 前自限制。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use thiserror::Error;

/// 沙箱错误
#[derive(Debug, Error, PartialEq)]
pub enum SandboxError {
    /// 策略要求沙箱但探测不到可用 rung(fail-closed)
    #[error("no sandbox runner available; refusing to run (fail-closed)")]
    NoRunner,
    /// rung 包装/应用失败
    #[error("sandbox error: {0}")]
    Other(String),
}

/// 探测到的可用 rung
#[derive(Debug, Clone, PartialEq)]
pub enum Rung {
    /// bubblewrap(路径)
    Bwrap(PathBuf),
    /// Landlock 内核 LSM(pre_exec 应用)
    Landlock,
    /// macOS Seatbelt(sandbox-exec 路径)
    Seatbelt(PathBuf),
}

/// 探测结果:rung + enforcement 完整性声明
///
/// `SandboxEnforcement` 语义:
/// `full` = 所选后端完整覆盖策略承诺的文件效果;`partial` = 有已知
/// 边界(声明而不回避,如 Windows WRITE_RESTRICTED 必须保留 Everyone)。
/// dsh 落地:当期 rung 集(bwrap/landlock/seatbelt)在各自策略承诺上
/// 均为 full(landlock 承诺子集在 ABI V1 即完整表达;若未来引入需新
/// ABI 位的能力,再引入 partial 声明)。
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    /// 链序选中的 rung
    pub rung: Rung,
    /// 该 rung 的 enforcement 完整性
    pub enforcement: SandboxEnforcement,
}

/// enforcement 完整性声明
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxEnforcement {
    /// 完整覆盖策略承诺
    Full,
    /// 部分覆盖(有已知边界;消费方不得视为绝对边界)
    Partial,
}

/// 执行世界策略模式(三态,与 dsh-core `permission.rs` 字符串命名一致)
///
/// `SandboxMode` 词汇;dsh 的
/// 装配语义:read-only 下 bash 工具不装配(files 走只读),
/// workspace-write 为默认,`full-access` 由权限切换触发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// 只读(无任何可写根)
    ReadOnly,
    /// 工作区可写(workspace 根 + 平台临时区)
    WorkspaceWrite,
    /// 全盘可写(**仍要求可用 rung**——不设「绕过沙箱」通道:
    /// fail-closed 优先)
    FullAccess,
}

/// 执行世界能力束(最小版):mode + 工作区根 + 可选读窄化
///
/// 可写根由 [`SandboxPolicy::writable_roots`] 单源推导
/// (workspace-write = workspace 根 + `/tmp` + `os.tmpdir()`),
/// 防止「write 工具能写 /tmp 但 bash 不能」一类方言漂移。
#[derive(Debug, Clone, PartialEq)]
pub struct SandboxPolicy {
    /// 策略模式
    pub mode: SandboxMode,
    /// 工作区根(workspace-write 下的可写边界;read-only/危险模式不消费)
    pub workspace_root: PathBuf,
    /// 允许读的目录(空 = 全系统可读)
    pub readable_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// 只读策略(无可写根)
    pub fn read_only() -> Self {
        Self {
            mode: SandboxMode::ReadOnly,
            workspace_root: PathBuf::new(),
            readable_roots: vec![],
        }
    }

    /// workspace-write 策略(工作区 + 平台临时区可写)
    pub fn workspace_write(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            mode: SandboxMode::WorkspaceWrite,
            workspace_root: workspace_root.into(),
            readable_roots: vec![],
        }
    }

    /// 全盘可写策略(仍要求可用 rung,见 [`SandboxMode::FullAccess`])
    pub fn full_access() -> Self {
        Self {
            mode: SandboxMode::FullAccess,
            workspace_root: PathBuf::new(),
            readable_roots: vec![],
        }
    }

    /// 本策略的可写根(单源推导,各 rung 方言共用)
    ///
    /// - ReadOnly → 空(仅允许 `/dev/null` 一类强制 sink);
    /// - WorkspaceWrite → `{workspace_root, "/tmp", env::temp_dir()}`
    ///   (canonicalize + 去重;macOS `/tmp` 即 `/private/tmp` 语义);
    /// - FullAccess → `["/"]`。
    pub fn writable_roots(&self) -> Vec<PathBuf> {
        match self.mode {
            SandboxMode::ReadOnly => vec![],
            SandboxMode::WorkspaceWrite => {
                let mut roots: Vec<PathBuf> = vec![
                    self.workspace_root.clone(),
                    PathBuf::from("/tmp"),
                    std::env::temp_dir(),
                ];
                // 与 wrap 路径共用 canonicalize(见 [`canonicalize`])
                for root in roots.iter_mut() {
                    *root = canonicalize(root);
                }
                roots.sort();
                roots.dedup();
                roots
            }
            SandboxMode::FullAccess => vec![PathBuf::from("/")],
        }
    }
}

/// 测试缝:置 true 时 [`probe`] 强制返回 None——在真实机器上验证 fail-closed 分支
static DISABLED_FOR_TESTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 探测缓存(设计文档 §7.8「探测一次缓存为 rung」;仅缓存成功结果,
/// 不可用时每次重试以便新装机/装包后可恢复)
static PROBED: OnceLock<ProbeResult> = OnceLock::new();

/// 设置测试缝(仅测试用:模拟无 rung 环境)
pub fn set_disabled_for_tests(disabled: bool) {
    DISABLED_FOR_TESTS.store(disabled, std::sync::atomic::Ordering::Relaxed);
}

/// 功能探测(链序:Linux bwrap→landlock;macOS seatbelt;其他平台 None)
///
/// **功能式**探测:
/// 以 read-only profile 真跑 `true`——`sandbox_init`/profile 被内核拒绝
/// 在此即失败,而非等到命令执行时;存在性探测(which/`--version`)会在
/// 内核拒绝 profile 时误报可用(fail-open 的隐藏路径)。
pub fn probe() -> Option<ProbeResult> {
    if DISABLED_FOR_TESTS.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    if let Some(probed) = PROBED.get() {
        return Some(probed.clone());
    }
    let probed = probe_uncached()?;
    let _ = PROBED.set(probed.clone());
    Some(probed)
}

fn probe_uncached() -> Option<ProbeResult> {
    #[cfg(target_os = "linux")]
    {
        if let Some(path) = which("bwrap") {
            if functional_probe_bwrap(&path) {
                return Some(ProbeResult {
                    rung: Rung::Bwrap(path),
                    enforcement: SandboxEnforcement::Full,
                });
            }
        }
        landlock::ABI::new()
            .ok()
            .filter(|abi| *abi >= landlock::ABI::V1)
            .map(|_| ProbeResult {
                rung: Rung::Landlock,
                enforcement: SandboxEnforcement::Full,
            })
    }
    #[cfg(target_os = "linux")]
    let _ = ();

    #[cfg(target_os = "macos")]
    {
        if let Some(path) = which("sandbox-exec")
            && functional_probe_seatbelt(&path)
        {
            return Some(ProbeResult {
                rung: Rung::Seatbelt(path),
                enforcement: SandboxEnforcement::Full,
            });
        }
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None // Windows ACL rung 后置;当前 fail-closed
    }
}

/// bwrap 功能探测:以 read-only profile 真跑 `true`
#[cfg(target_os = "linux")]
fn functional_probe_bwrap(path: &Path) -> bool {
    Command::new(path)
        .args(bwrap_args(&SandboxPolicy::read_only()))
        .arg("--")
        .arg("true")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Seatbelt 功能探测:以 read-only profile 真跑 `true`
/// (`sandbox-exec` 在 `sandbox_init` 拒绝 profile 时退出非零)
#[cfg(target_os = "macos")]
fn functional_probe_seatbelt(path: &Path) -> bool {
    Command::new(path)
        .args(seatbelt_args(&SandboxPolicy::read_only()))
        .arg("--")
        .arg("true")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn which(name: &str) -> Option<PathBuf> {
    let output = Command::new("which").arg(name).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!p.is_empty()).then(|| PathBuf::from(p))
}

/// 路径规范化(与可写根推导共用;macOS `/tmp` 等符号链接必须解析为
/// 真实路径,否则 profile 的 subpath 匹配不到任何东西)
fn canonicalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// bwrap profile 参数(与功能探测共用同一构造)
fn bwrap_args(policy: &SandboxPolicy) -> Vec<String> {
    let writable = policy.writable_roots();
    let mut argv: Vec<String> = vec![
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
    ];
    for r in &writable {
        if r == Path::new("/tmp") || *r == std::env::temp_dir() {
            // 临时区用隔离 tmpfs(`--tmpfs /tmp`):
            // 可写且与宿主 /tmp 隔离(往宿 /tmp 塞东西不在承诺内);
            // 若 workspace 根与临时区重叠,后续 `--bind` 按顺序覆盖
            argv.push("--tmpfs".into());
            argv.push(r.to_string_lossy().into_owned());
        } else {
            argv.push("--bind".into());
            argv.push(r.to_string_lossy().into_owned());
            argv.push(r.to_string_lossy().into_owned());
        }
    }
    argv
}

/// Seatbelt SBPL profile(与功能探测共用同一构造)
fn seatbelt_args(policy: &SandboxPolicy) -> Vec<String> {
    let writable = policy.writable_roots();
    // SBPL:默认拒绝;放行进程执行与读;写仅限 writable_roots
    // 注意 profile 顺序:SBP 规则名带 * 的是通配,子串独特的
    // 规则名(如 file-write* 匹配 write-data/write-unlink 等)
    // 要列在通用放行之后,本函数无需显式排序前缀
    let mut allow_write = String::new();
    for root in &writable {
        allow_write.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            root.to_string_lossy()
        ));
    }
    let profile = format!(
        // sysctl*:Rust std 启动为主线程装栈溢出保护页前要查栈边界
        // (sys/pal/unix/stack_overflow.rs),(deny default) 下被拒
        // → EINVAL → 全体 Rust 二进制崩「failed to allocate a
        // guard page」。sysctl 只读内核信息,放行无害
        "(version 1)(deny default)(allow process*)(allow file-read*)(allow sysctl*)(allow file-write* (literal \"/dev/null\"))\n{allow_write}"
    );
    vec!["-p".into(), profile]
}

/// runner 失败识别规则(见 [`RunnerFailureRule`]):区分「runner 失败(命令根本没执行)」
/// 与「拒绝(命令执行了但被内核拦)」。
///
/// 匹配序:先按 [`ExitClass`] 的 wait 后分类——若退出码命中
/// `allowed_exit_codes`(Some 时;None = 任意非零)且某 stderr 行
/// (经 `informational_lines` 精确行排除后)含 `fatal_signatures`,则为
/// runner 失败;否则按拒绝方言匹配;两者皆不中 → 常规退出。
#[derive(Debug, Clone, Copy)]
pub struct RunnerFailureRule {
    /// 该规则可匹配的非零退出码;None = 任意非零
    pub allowed_exit_codes: Option<&'static [i32]>,
    /// 单行致命诊断子串
    pub fatal_signatures: &'static [&'static str],
    /// 误报排除的行(精确行相等即剔除,在致命匹配前)
    pub informational_lines: &'static [&'static str],
}

/// argv 包装结果(`ConfinedArgv`)
#[derive(Debug, Clone)]
pub struct Confined {
    /// 包装后的程序(实际执行者:bwrap / sandbox-exec)
    pub program: String,
    /// 包装后的 argv(含 rung profile 参数与原始命令)
    pub argv: Vec<String>,
    /// 所选 rung 的 enforcement 完整性
    pub enforcement: SandboxEnforcement,
    /// 拒绝方言:被拒文件效果在本 rung 下产生的 stderr 子串
    /// (EROFS 之于 bwrap / EACCES 之于 landlock / EPERM 之于 seatbelt;
    /// 按所选后端匹配,不做跨后端并集——并集会声称某后端从不产生的拒绝)
    pub denial_dialect: &'static [&'static str],
    /// runner 失败识别规则(见 [`RunnerFailureRule`])
    pub runner_failure_rules: &'static [RunnerFailureRule],
}

impl Confined {
    /// landlock rung 的等价元数据(不经 argv 包装,program/argv 即原命令,
    /// 分类元数据(方言/成败规则)仍随身携带——进程内 EACCES 归为拒绝)
    pub fn landlock(program: &str, argv: &[String], enforcement: SandboxEnforcement) -> Self {
        Confined {
            program: program.to_string(),
            argv: argv.to_vec(),
            enforcement,
            denial_dialect: dialect_of(&Rung::Landlock),
            runner_failure_rules: runner_failure_of(&Rung::Landlock),
        }
    }
}

/// 拒绝方言表(`DENIAL_SIGNATURES`)
const DENIAL_DIALECT: &[(&str, &[&str])] = &[
    ("bwrap", &["read-only file system"]),
    ("landlock", &["permission denied"]),
    ("seatbelt", &["operation not permitted"]),
];

/// runner 失败规则表(`RUNNER_FAILURE_RULES`;dsh 无独立 launcher 形态
/// ——landlock 走 pre_exec 在 spawn 层报错,故该表仅覆盖 bwrap/seatbelt)
const RUNNER_FAILURE_RULES: &[(&str, &[RunnerFailureRule])] = &[
    (
        "bwrap",
        &[RunnerFailureRule {
            allowed_exit_codes: None,
            fatal_signatures: &["bwrap: "],
            informational_lines: &[],
        }],
    ),
    (
        "seatbelt",
        &[RunnerFailureRule {
            allowed_exit_codes: None,
            fatal_signatures: &["sandbox-exec: "],
            informational_lines: &[],
        }],
    ),
    ("landlock", &[]),
];

fn dialect_of(rung: &Rung) -> &'static [&'static str] {
    DENIAL_DIALECT
        .iter()
        .find(|(name, _)| rung_name(rung) == *name)
        .map(|(_, d)| *d)
        .unwrap_or(&[])
}

fn runner_failure_of(rung: &Rung) -> &'static [RunnerFailureRule] {
    RUNNER_FAILURE_RULES
        .iter()
        .find(|(name, _)| rung_name(rung) == *name)
        .map(|(_, r)| *r)
        .unwrap_or(&[])
}

fn rung_name(rung: &Rung) -> &'static str {
    match rung {
        Rung::Bwrap(_) => "bwrap",
        Rung::Landlock => "landlock",
        Rung::Seatbelt(_) => "seatbelt",
    }
}

/// argv 包装(bwrap / seatbelt rung):返回 [`Confined`] 供重建 Command。
///
/// Linux Landlock rung 不走 argv 包装(经 pre_exec),返回
/// [`SandboxError::Other`] 提示调用方走 landlock 路径。
/// `probed` 由调用方先经 [`probe`] 取得(避免 wrap 内重复探测)。
pub fn wrap_argv(
    policy: &SandboxPolicy,
    cmd: &str,
    args: &[String],
    probed: &ProbeResult,
) -> Result<Confined, SandboxError> {
    match &probed.rung {
        Rung::Bwrap(path) => {
            let mut argv = bwrap_args(policy);
            argv.push("--".into());
            argv.push(cmd.to_string());
            argv.extend(args.iter().cloned());
            Ok(Confined {
                program: path.to_string_lossy().into_owned(),
                argv,
                enforcement: probed.enforcement,
                denial_dialect: dialect_of(&probed.rung),
                runner_failure_rules: runner_failure_of(&probed.rung),
            })
        }
        Rung::Seatbelt(path) => {
            let mut argv = seatbelt_args(policy);
            argv.push(cmd.to_string());
            argv.extend(args.iter().cloned());
            Ok(Confined {
                program: path.to_string_lossy().into_owned(),
                argv,
                enforcement: probed.enforcement,
                denial_dialect: dialect_of(&probed.rung),
                runner_failure_rules: runner_failure_of(&probed.rung),
            })
        }
        Rung::Landlock => Err(SandboxError::Other(
            "landlock rung 走 pre_exec 路径,不包装 argv".into(),
        )),
    }
}

/// Landlock 自限制(exec 前在子进程内应用;继承 landlock-run 的 self-restrict-then-exec 语义)
#[cfg(target_os = "linux")]
pub fn landlock_pre_exec(
    command: &mut tokio::process::Command,
    policy: &SandboxPolicy,
) -> Result<(), SandboxError> {
    use std::os::unix::process::CommandExt;
    let rules = policy.clone();
    unsafe {
        command.pre_exec(move || apply_landlock(&rules).map_err(std::io::Error::other));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn apply_landlock(policy: &SandboxPolicy) -> Result<(), String> {
    use landlock::{
        AccessFs, Compatible, PathBeneath, PathFd, PathRulesetAttr, Ruleset, RulesetAttr,
        RulesetCreatedAttr,
    };
    let abi = landlock::ABI::new().map_err(|e| e.to_string())?;
    let mut ruleset = Ruleset::default()
        .set_compatibility(abi)
        .handle_fs(AccessFs::All)
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?;

    let add = |ruleset: &mut landlock::RulesetCreated, path: &std::path::Path, rights: AccessFs| {
        let fd = PathFd::new(path).map_err(|e| e.to_string())?;
        ruleset
            .add_rules(PathBeneath::new(fd, rights))
            .map_err(|e| e.to_string())
    };

    let read = AccessFs::from_read(AccessFs::ReadFile | AccessFs::ReadDir);
    let write = read | AccessFs::WriteFile;
    let _ = write;
    if policy.readable_roots.is_empty() {
        add(&mut ruleset, std::path::Path::new("/"), read)?;
    } else {
        for root in &policy.readable_roots {
            add(&mut ruleset, root, read)?;
        }
    }
    for root in &policy.writable_roots() {
        add(&mut ruleset, root, read | AccessFs::WriteFile)?;
    }
    ruleset.restrict_self().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_finds_rung_or_none() {
        #[cfg(target_os = "macos")]
        assert!(
            matches!(
                probe(),
                Some(ProbeResult {
                    rung: Rung::Seatbelt(_),
                    enforcement: SandboxEnforcement::Full
                })
            ),
            "macOS 应探测到 seatbelt(full)"
        );
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert_eq!(probe(), None);
    }

    #[test]
    fn writable_roots_by_mode() {
        let ws = std::env::temp_dir().join("dsh-ws-derive");
        assert!(SandboxPolicy::read_only().writable_roots().is_empty());
        let roots = SandboxPolicy::workspace_write(&ws).writable_roots();
        assert!(roots.contains(&ws), "workspace 根可写;got: {roots:?}");
        // /tmp 与 env temp_dir 均纳入(canonicalize 后可能同值,去重)
        let tmp = canonicalize(Path::new("/tmp"));
        assert!(roots.contains(&tmp), "/tmp 纳入可写根;got: {roots:?}");
        assert_eq!(
            SandboxPolicy::full_access().writable_roots(),
            vec![PathBuf::from("/")]
        );
        // 单调去重
        let mut deduped = roots.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(roots, deduped, "可写根必须已去重");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_argv_wraps() {
        let policy = SandboxPolicy::workspace_write("/tmp/dsh-sandbox-test");
        let probed = probe().expect("rung");
        let confined = wrap_argv(&policy, "touch", &["x".to_string()], &probed)
            .expect("bwrap/seatbelt rung 应包装成功");
        assert!(confined.enforcement == SandboxEnforcement::Full);
        assert!(confined.program.contains("sandbox-exec") || confined.program.contains("bwrap"));
        assert!(confined.argv.iter().any(|a| a.contains("(deny default)")));
        assert!(
            confined
                .argv
                .iter()
                .any(|a| a.contains("/tmp/dsh-sandbox-test"))
        );
        assert_eq!(confined.argv.last().unwrap(), "x");
        // guard page 回归:Rust std 启动查栈边界(sysctl)被 deny → EINVAL
        // →「failed to allocate a guard page」崩全体 Rust 二进制
        assert!(
            confined.argv.iter().any(|a| a.contains("(allow sysctl*)")),
            "SBPL 必须放行 sysctl,否则 Rust 工具链 guard page 崩溃"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bwrap_argv_wraps_when_available() {
        let Some(probed) = probe() else {
            return; // 本机无 rung:链回退 landlock(或 fail-closed 分支,已由另测覆盖)
        };
        if !matches!(probed.rung, Rung::Bwrap(_)) {
            return; // 本机无 bwrap:链回退 landlock
        }
        let policy = SandboxPolicy::workspace_write("/tmp");
        let confined = wrap_argv(&policy, "id", &[], &probed).expect("wrap");
        assert!(confined.program.contains("bwrap"));
        assert!(confined.argv.contains(&"--ro-bind".to_string()));
    }

    #[test]
    fn denial_dialect_and_runner_rules_static() {
        // 表驱动不动,证明三 rung 均有方言;landlock 无 runner 规则
        // (pre_exec 在 spawn 层报错,无 launcher 形态)
        for (name, rung) in [
            ("bwrap", Rung::Bwrap(PathBuf::from("x"))),
            ("landlock", Rung::Landlock),
            ("seatbelt", Rung::Seatbelt(PathBuf::from("x"))),
        ] {
            let dialect = dialect_of(&rung);
            assert!(!dialect.is_empty(), "{name} 必须有拒绝方言");
            let _ = runner_failure_of(&rung);
        }
        assert!(runner_failure_of(&Rung::Landlock).is_empty());
    }
}

/// 非 Unix 平台的策略校验(fail-closed 入口;Windows ACL 后置)
#[cfg(not(unix))]
pub fn prepare_fallback(_policy: &SandboxPolicy) -> Result<(), SandboxError> {
    Err(SandboxError::NoRunner)
}
