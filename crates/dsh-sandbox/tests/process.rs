//! 进程/沙箱集成测试(出口门)。
//!
//! - bash 工具在沙箱下执行成功(可写根内写文件 ✓、根外写被拒 ✓);
//! - SIGTERM→grace→SIGKILL(忽略 TERM 的进程被 KILL 兜底);
//! - fail-closed(策略要求沙箱但 rung 被禁用时拒绝执行);
//! - epoch 硬停(死循环 wasm 组件被 trap)属引擎测试,在 dsh-host/tests/engine.rs。

use std::sync::Mutex;
use std::time::Duration;

use dsh_sandbox::sandbox::{
    Confined, RunnerFailureRule, SandboxEnforcement, SandboxError, SandboxPolicy, probe,
};
use dsh_sandbox::{ExitClass, ExitStatus, SpawnOptions, classify_exit, spawn};

/// 涉及全局测试缝(set_disabled_for_tests)的沙箱测试串行化,
/// 防止 seam=true 时并行的其它沙箱测试 probe 到 None 而误判
static SANDBOX_TEST_MUTEX: Mutex<()> = Mutex::new(());

fn policy(workspace: &str) -> SandboxPolicy {
    SandboxPolicy::workspace_write(workspace)
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // 测试串行化意图明确
async fn bash_tool_runs_under_sandbox() {
    let _serial = SANDBOX_TEST_MUTEX.lock().unwrap();
    // 出口门:bash 工具在沙箱下执行成功(本机 macOS = seatbelt;Linux = bwrap/landlock)
    let Some(_) = probe() else {
        eprintln!("本机无沙箱 rung,跳过(此环境本应 fail-closed,见下一个用例)");
        return;
    };
    let workdir = std::env::temp_dir().join(format!("dsh-p3-sbx-{}", std::process::id()));
    std::fs::create_dir_all(&workdir).unwrap();
    let opts = SpawnOptions {
        cwd: Some(workdir.clone()),
        env: Default::default(),
        sandbox: Some(policy(workdir.to_str().unwrap())),
        stdin: None,
    };
    let mut child = spawn(
        "/bin/bash",
        &["-c".into(), "echo sandbox-ok && touch marker.txt".into()],
        &opts,
    )
    .await
    .expect("spawn under sandbox");
    let out = child.stdout().await.unwrap();
    let status = child.wait().await.unwrap();
    assert!(status.success(), "status: {status:?}");
    assert!(String::from_utf8_lossy(&out).contains("sandbox-ok"));
    assert!(workdir.join("marker.txt").exists(), "可写根内应可写");
}

/// workspace-write 根推导:workspace + `/tmp` +
/// 平台 temp_dir——沙箱下编译类工具在临时区落中间产物不被拦
/// (bwrap 临时区是隔离 tmpfs,macOS 是 canonicalize 后 subpath 写真目录)
#[tokio::test]
#[allow(clippy::await_holding_lock)] // 测试串行化意图明确
async fn workspace_write_allows_temp_artifacts() {
    let _serial = SANDBOX_TEST_MUTEX.lock().unwrap();
    let Some(_) = probe() else {
        eprintln!("本机无沙箱 rung,跳过(此环境本应 fail-closed,见 fail_closed 用例)");
        return;
    };
    let workdir = std::env::temp_dir().join(format!("dsh-p3-tmp-{}", std::process::id()));
    std::fs::create_dir_all(&workdir).unwrap();
    let tmp_file = std::env::temp_dir().join(format!("dsh-p3-tmpfile-{}.txt", std::process::id()));
    let opts = SpawnOptions {
        cwd: Some(workdir.clone()),
        env: Default::default(),
        sandbox: Some(policy(workdir.to_str().unwrap())),
        stdin: None,
    };
    let mut child = spawn(
        "/bin/bash",
        &[
            "-c".into(),
            format!("touch '{}' && touch marker.txt", tmp_file.display()),
        ],
        &opts,
    )
    .await
    .expect("spawn under sandbox");
    let status = child.wait().await.unwrap();
    assert!(
        status.success(),
        "临时区应可写(workspace-write 根推导);status: {status:?}"
    );
    assert!(
        workdir.join("marker.txt").exists(),
        "workspace 内写仍应成功"
    );
    let _ = std::fs::remove_file(&tmp_file);
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[allow(clippy::await_holding_lock)] // 测试串行化意图明确
async fn sandbox_denies_write_outside_writable_roots() {
    let _serial = SANDBOX_TEST_MUTEX.lock().unwrap();
    let workdir = std::env::temp_dir().join(format!("dsh-p3-deny-{}", std::process::id()));
    std::fs::create_dir_all(&workdir).unwrap();
    // 根外 = 白名单(/tmp、tmpdir、workdir)之外:macOS 只读系统卷
    let outside =
        std::path::PathBuf::from(format!("/System/dsh-p3-outside-{}", std::process::id()));
    let opts = SpawnOptions {
        cwd: Some(workdir.clone()),
        env: Default::default(),
        sandbox: Some(policy(workdir.to_str().unwrap())),
        stdin: None,
    };
    // 根外写 → seatbelt 拒绝(非零退出)
    let mut child = spawn(
        "/bin/bash",
        &["-c".into(), format!("touch {}", outside.display())],
        &opts,
    )
    .await
    .expect("spawn");
    let status = child.wait().await.unwrap();
    assert!(
        !status.success(),
        "根外写必须被沙箱拒绝(status: {status:?})"
    );
    assert!(!outside.exists());
}

/// 沙箱退出分类表驱动(denialSignatures + runnerFailureRules
/// 语义):判别序 = runner 失败(命令从未执行)→ 拒绝(内核拦截)→
/// 常规;覆盖码门 / informational 行排除 / 方言匹配 / 信号终止
#[test]
fn classify_exit_table_driven() {
    const EROFS_LINE: &str = "touch: cannot touch '/x': Read-only file system";
    const EPERM_LINE: &str = "touch: /System/x: Operation not permitted";
    const EACCES_LINE: &str = "touch: cannot touch '/x': Permission denied";
    const BWRAP_FATAL_LINE: &str = "bwrap: execvp: No such file or directory";
    const SEATBELT_FATAL_LINE: &str = "sandbox-exec: profile rejected";

    static BWRAP_RULES: &[RunnerFailureRule] = &[RunnerFailureRule {
        allowed_exit_codes: None,
        fatal_signatures: &["bwrap: "],
        informational_lines: &[],
    }];
    static SEATBELT_RULES: &[RunnerFailureRule] = &[RunnerFailureRule {
        allowed_exit_codes: None,
        fatal_signatures: &["sandbox-exec: "],
        informational_lines: &[],
    }];

    let mk = |dialect: &'static [&'static str], rules: &'static [RunnerFailureRule]| Confined {
        program: "runner".into(),
        argv: vec![],
        enforcement: SandboxEnforcement::Full,
        denial_dialect: dialect,
        runner_failure_rules: rules,
    };
    let bwrap = mk(&["read-only file system"], BWRAP_RULES);
    let seatbelt = mk(&["operation not permitted"], SEATBELT_RULES);
    let landlock = mk(&["permission denied"], &[]);
    let class = |code: Option<i32>, signal: Option<i32>, stderr: &str, c: &Confined| {
        classify_exit(ExitStatus { code, signal }, stderr, c)
    };

    // 拒绝方言:执行了但被内核拦
    assert!(
        matches!(class(Some(1), None, EROFS_LINE, &bwrap), ExitClass::Denied { line, .. } if line == EROFS_LINE)
    );
    assert!(matches!(
        class(Some(1), None, EPERM_LINE, &seatbelt),
        ExitClass::Denied { .. }
    ));
    assert!(matches!(
        class(Some(1), None, EACCES_LINE, &landlock),
        ExitClass::Denied { .. }
    ));

    // runner 失败:命令从未执行(fatal 签名命中)
    assert!(matches!(
        class(Some(1), None, BWRAP_FATAL_LINE, &bwrap),
        ExitClass::RunnerFailed { code: Some(1), .. }
    ));
    assert!(matches!(
        class(Some(1), None, SEATBELT_FATAL_LINE, &seatbelt),
        ExitClass::RunnerFailed { .. }
    ));

    // 判别序:runner 失败先于拒绝(命令都没跑,谈不上被拦)
    assert!(matches!(
        class(
            Some(1),
            None,
            &format!("{BWRAP_FATAL_LINE}\n{EROFS_LINE}"),
            &bwrap
        ),
        ExitClass::RunnerFailed { .. }
    ));

    // 码门:0 码即使带 fatal 也归常规(命令本身执行了);信号终止不经码门
    assert!(matches!(
        class(Some(0), None, BWRAP_FATAL_LINE, &bwrap),
        ExitClass::Ran(_)
    ));
    assert!(matches!(
        class(None, Some(9), "killed", &bwrap),
        ExitClass::Ran(_)
    ));

    // 非零无 fatal/无方言 → 常规(退出码是数据,不是失败)
    assert!(matches!(
        class(Some(42), None, "some command error", &bwrap),
        ExitClass::Ran(_)
    ));

    // allowed_exit_codes 门:专属码才认致命;非专属码同 fatal 不认
    static GATED_RULES: &[RunnerFailureRule] = &[RunnerFailureRule {
        allowed_exit_codes: Some(&[125]),
        fatal_signatures: &["fatal: "],
        informational_lines: &[],
    }];
    let gated = mk(&[], GATED_RULES);
    assert!(matches!(
        class(Some(125), None, "fatal: boom", &gated),
        ExitClass::RunnerFailed { .. }
    ));
    assert!(matches!(
        class(Some(1), None, "fatal: boom", &gated),
        ExitClass::Ran(_)
    ));

    // informational 行先剔除:仅信息行 → 常规;信息行 + 真致命 → 失败
    static INFO_RULES: &[RunnerFailureRule] = &[RunnerFailureRule {
        allowed_exit_codes: None,
        fatal_signatures: &["fatal: "],
        informational_lines: &["fatal: informational line"],
    }];
    let info = mk(&[], INFO_RULES);
    assert!(matches!(
        class(Some(1), None, "fatal: informational line", &info),
        ExitClass::Ran(_)
    ));
    assert!(matches!(
        class(
            Some(1),
            None,
            "fatal: informational line\nfatal: real",
            &info
        ),
        ExitClass::RunnerFailed { .. }
    ));
}

#[tokio::test]
async fn term_then_kill_with_grace() {
    // SIGTERM→grace→SIGKILL:单进程忙等且忽略 TERM → grace 过期 → SIGKILL 兜底
    let mut child = spawn(
        "/bin/bash",
        &["-c".into(), "trap '' TERM; while :; do :; done".into()],
        &SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let pid = child.pid().expect("pid");
    // 等待 trap 生效(spawn 后立即 TERM 会在 trap 安装前送达,死于 TERM 属正常路径)
    tokio::time::sleep(Duration::from_millis(300)).await;
    // 忽略 TERM:grace(300ms)内不退 → SIGKILL 兜底
    let status = child
        .kill_with_grace(Duration::from_millis(300))
        .await
        .unwrap();
    assert_eq!(
        status.signal,
        Some(9),
        "忽略 TERM 的进程必须被 SIGKILL 兜底 (pid {pid})"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // 测试串行化意图明确
async fn fail_closed_when_no_rung() {
    let _serial = SANDBOX_TEST_MUTEX.lock().unwrap();
    // fail-closed:策略要求沙箱但探测不到 rung → 拒绝执行,绝不静默降级
    // (set_disabled_for_tests 是 probe 的测试缝,模拟无 rung 环境)
    dsh_sandbox::sandbox::set_disabled_for_tests(true);
    let result = spawn(
        "/bin/echo",
        &["x".into()],
        &SpawnOptions {
            sandbox: Some(SandboxPolicy::read_only()),
            ..Default::default()
        },
    )
    .await;
    dsh_sandbox::sandbox::set_disabled_for_tests(false);
    assert!(
        matches!(
            result,
            Err(dsh_sandbox::ProcessError::Sandbox(SandboxError::NoRunner))
        ),
        "fail-closed 必须拒绝(得到 Ok,违反 fail-closed)"
    );
}

/// stdin 载荷:Some(bytes) = piped,写完即关;子进程读到 EOF 后消费。
/// hooks 桥依赖此原语喂序列化载荷(源 runHook stdin 语义)。
#[tokio::test]
#[allow(clippy::await_holding_lock)] // 测试串行化意图明确
async fn stdin_payload_reaches_child_and_closes() {
    let _serial = SANDBOX_TEST_MUTEX.lock().unwrap();
    // 无沙箱:原语本身与沙箱链正交
    let opts = SpawnOptions {
        stdin: Some(b"hook-payload-1".to_vec()),
        ..Default::default()
    };
    let mut child = spawn("/bin/bash", &["-c".into(), "cat".into()], &opts)
        .await
        .expect("spawn");
    let out = child.stdout().await.expect("stdout");
    let status = child.wait().await.expect("wait");
    let err = child.stderr_text().await;
    assert!(
        status.success(),
        "bash 应以 0 退出(得到 {status:?}) stderr={err} stdout={}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(
        String::from_utf8_lossy(&out),
        "hook-payload-1",
        "子进程未原样读回 stdin 载荷"
    );
}
