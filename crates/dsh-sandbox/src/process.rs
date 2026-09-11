//! 进程 host functions:spawn / child-wait / child-kill(SIGTERM→grace→SIGKILL)。
//!
//! 语义:
//! - 子进程入独立进程组(Unix `process_group(0)`)——kill-tree 即组信号,
//!   孙进程(除非自己换组)一并覆盖;
//! - 终止顺序:SIGTERM(组)→ 等待 grace → SIGKILL(组),绝不留僵尸;
//! - 沙箱链由 [`crate::sandbox`] 在 spawn 路径强制(探测失败即拒绝)。
//!
//! WIT 对应:`dsh:host/process`(spawn/child-wait/child-kill/kill-tree)。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

use crate::sandbox::{self, Confined, Rung, SandboxPolicy};

/// 进程错误
#[derive(Debug, Error)]
pub enum ProcessError {
    /// spawn 失败
    #[error("spawn failed: {0}")]
    Spawn(String),
    /// 沙箱拒绝(fail-closed)
    #[error("sandbox refused: {0}")]
    Sandbox(#[from] sandbox::SandboxError),
    /// 等待失败
    #[error("wait failed: {0}")]
    Wait(String),
}

/// 退出状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    /// 退出码(被信号杀死时为 None)
    pub code: Option<i32>,
    /// 终止信号(Unix)
    pub signal: Option<i32>,
}

impl ExitStatus {
    /// 是否成功(code == 0)
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// spawn 选项
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    /// 工作目录(缺省继承宿主)
    pub cwd: Option<PathBuf>,
    /// 追加环境变量
    pub env: HashMap<String, String>,
    /// 沙箱策略;Required 时探测不到可用 rung 即拒绝(fail-closed)
    pub sandbox: Option<SandboxPolicy>,
    /// 写入子进程 stdin 的字节(Some = piped,写完即关;None = null)。
    /// hooks 桥等需要序列化载荷喂 stdin 的调用方使用。
    pub stdin: Option<Vec<u8>>,
}

/// 子进程句柄(独立进程组)
pub struct Child {
    inner: tokio::process::Child,
    /// 进程组 id(Unix = pgid;非 Unix 平台组语义缺失,仅杀直接子进程)
    pgid: Option<i32>,
    /// stderr 收集缓冲(spawn 后即起 drain 任务读至 EOF;修复「stderr
    /// piped 但无人读 → 子进程大 stderr 堵塞」泄漏,同时供 [`Child::wait_classified`])
    stderr_buf: std::sync::Arc<tokio::sync::Mutex<String>>,
    /// drain 任务句柄([`Child::stderr_text`] 先 await 其收尾——child exit
    /// 与 stderr EOF 存在竞争,否则分类/落盘可能缺尾行)
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    /// 本 spawn 的沙箱包装元数据(None = 无沙箱);分类见 [`ExitClass`]
    confined: Option<Confined>,
}

/// spawn:经沙箱链包装后启动,Unix 下入独立进程组。
pub async fn spawn(cmd: &str, args: &[String], opts: &SpawnOptions) -> Result<Child, ProcessError> {
    // 沙箱链(fail-closed):bwrap/seatbelt 重写 argv;landlock 装 pre_exec;
    // 探测不到 rung 且策略要求沙箱 → 拒绝
    #[allow(unused_variables)] // 仅 Linux(landlock pre_exec)使用
    let (program, argv, policy_for_preexec, confined): (
        String,
        Vec<String>,
        Option<SandboxPolicy>,
        Option<Confined>,
    ) = match &opts.sandbox {
        Some(policy) => {
            #[cfg(unix)]
            {
                let probed = sandbox::probe().ok_or(sandbox::SandboxError::NoRunner)?;
                match &probed.rung {
                    Rung::Landlock => (
                        cmd.to_string(),
                        args.to_vec(),
                        Some(policy.clone()),
                        Some(Confined::landlock(cmd, args, probed.enforcement)),
                    ),
                    _ => {
                        let confined = sandbox::wrap_argv(policy, cmd, args, &probed)?;
                        (
                            confined.program.clone(),
                            confined.argv.clone(),
                            None,
                            Some(confined),
                        )
                    }
                }
            }
            #[cfg(not(unix))]
            {
                sandbox::prepare_fallback(policy)?;
                (cmd.to_string(), args.to_vec(), None, None)
            }
        }
        None => (cmd.to_string(), args.to_vec(), None, None),
    };

    let mut command = tokio::process::Command::new(program);
    command.args(&argv);
    if let Some(cwd) = &opts.cwd {
        command.current_dir(cwd);
    }
    for (k, v) in &opts.env {
        command.env(k, v);
    }
    if opts.stdin.is_some() {
        command.stdin(std::process::Stdio::piped());
    } else {
        command.stdin(std::process::Stdio::null());
    }
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    #[cfg(unix)]
    {
        // 独立进程组:kill-tree = 组信号(process_group 是 CommandExt 方法)
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            // landlock rung:exec 前自限制(继承 landlock-run 的 self-restrict-then-exec)
            if let Some(policy) = &policy_for_preexec {
                sandbox::landlock_pre_exec(&mut command, policy)?;
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // tokio Command 原生 process_group(std CommandExt 无需导入)
            command.process_group(0);
        }
    }

    let mut inner = command
        .spawn()
        .map_err(|e| ProcessError::Spawn(format!("{cmd}: {e}")))?;
    let pgid = inner.id().map(|pid| pid as i32);
    // stdin 载荷:写入后立即关闭管道(子进程读到 EOF;write_all 在
    // spawn 返回的任务上下文里完成,大载荷由 OS 管道缓冲 + await 背压兜底)
    if let Some(payload) = &opts.stdin {
        use tokio::io::AsyncWriteExt;
        if let Some(mut stdin) = inner.stdin.take() {
            stdin
                .write_all(payload)
                .await
                .map_err(|e| ProcessError::Spawn(format!("{cmd}: stdin write: {e}")))?;
            stdin
                .shutdown()
                .await
                .map_err(|e| ProcessError::Spawn(format!("{cmd}: stdin close: {e}")))?;
        }
    }
    let (stderr_buf, stderr_task) = drain_stderr(&mut inner);
    Ok(Child {
        inner,
        pgid,
        stderr_buf,
        stderr_task,
        confined,
    })
}

/// 起 tokio 任务把子进程 stderr 读至 EOF 存入共享缓冲(读入即消费,
/// 否则子进程 stderr 无消费者时会写满管道而阻塞)
fn drain_stderr(
    inner: &mut tokio::process::Child,
) -> (
    std::sync::Arc<tokio::sync::Mutex<String>>,
    Option<tokio::task::JoinHandle<()>>,
) {
    use tokio::io::AsyncReadExt;
    let buf = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let Some(mut stderr) = inner.stderr.take() else {
        return (buf, None);
    };
    let target = buf.clone();
    let task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        if stderr.read_to_end(&mut bytes).await.is_ok() {
            *target.lock().await = String::from_utf8_lossy(&bytes).into_owned();
        }
    });
    (buf, Some(task))
}

impl Child {
    /// pid
    pub fn pid(&self) -> Option<u32> {
        self.inner.id()
    }

    /// 读全部 stdout(到 EOF;配合 wait 使用)
    pub async fn stdout(&mut self) -> Result<Vec<u8>, ProcessError> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        if let Some(mut out) = self.inner.stdout.take() {
            out.read_to_end(&mut buf)
                .await
                .map_err(|e| ProcessError::Wait(e.to_string()))?;
        }
        Ok(buf)
    }

    /// 等待退出
    pub async fn wait(&mut self) -> Result<ExitStatus, ProcessError> {
        let status = self
            .inner
            .wait()
            .await
            .map_err(|e| ProcessError::Wait(e.to_string()))?;
        Ok(to_status(status))
    }

    /// 已收集的 stderr(UTF-8 宽容解码):await drain 任务收尾后取完整文本
    pub async fn stderr_text(&mut self) -> String {
        if let Some(task) = self.stderr_task.take() {
            let _ = task.await;
        }
        self.stderr_buf.lock().await.clone()
    }

    /// 等待退出并按沙箱元数据分类(见 [`ExitClass`])
    ///
    /// 分类序(`denialSignatures` + `runnerFailureRules`):
    /// runner 失败(命令未执行)→ 拒绝(执行了但被内核拦)→ 常规退出。
    pub async fn wait_classified(&mut self) -> ExitClass {
        let status = match self.wait().await {
            Ok(status) => status,
            Err(e) => {
                return ExitClass::RunnerFailed {
                    code: None,
                    detail: e.to_string(),
                };
            }
        };
        let stderr = self.stderr_text().await;
        let Some(confined) = &self.confined else {
            return ExitClass::Ran(status);
        };
        classify_exit(status, &stderr, confined)
    }

    /// 带超时等待(取消路径用)
    pub async fn wait_timeout(&mut self, grace: Duration) -> Option<ExitStatus> {
        match tokio::time::timeout(grace, self.inner.wait()).await {
            Ok(Ok(status)) => Some(to_status(status)),
            _ => None,
        }
    }

    /// 终止:SIGTERM(组)→ grace → SIGKILL(组)。
    ///
    /// 顺序语义:SIGTERM(组)先、grace 内退出即温和成功。
    pub async fn kill_with_grace(&mut self, grace: Duration) -> Result<ExitStatus, ProcessError> {
        self.signal_group(libc::SIGTERM)?;
        if let Some(status) = self.wait_timeout(grace).await {
            return Ok(status);
        }
        self.signal_group(libc::SIGKILL)?;
        self.wait().await
    }

    /// 进程树级终止(独立进程组下与 kill_with_grace 同路径)
    pub async fn kill_tree(&mut self, grace: Duration) -> Result<ExitStatus, ProcessError> {
        self.kill_with_grace(grace).await
    }

    #[cfg(unix)]
    fn signal_group(&self, sig: i32) -> Result<(), ProcessError> {
        if let Some(pgid) = self.pgid {
            // 负 pid = 进程组
            let rc = unsafe { libc::kill(-pgid, sig) };
            if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return Err(ProcessError::Wait(format!(
                    "killpg({pgid}, {sig}): {}",
                    std::io::Error::last_os_error()
                )));
            }
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn signal_group(&self, _sig: i32) -> Result<(), ProcessError> {
        // 非 Unix:仅杀直接子进程(Windows Job Objects 后置)
        let _ = self.inner.kill();
        Ok(())
    }

    /// 进程组信号句柄(与 [`Child`] 分离):后台任务的 stop 发信号
    /// 不必等持有 Child 的一方读完 stdout
    pub fn group_killer(&self) -> GroupKiller {
        GroupKiller { pgid: self.pgid }
    }
}

/// 进程组信号句柄(pgid 的快照;克隆自由)
#[derive(Clone, Debug)]
pub struct GroupKiller {
    /// 进程组 id(Unix;None = 无法组信号)
    pgid: Option<i32>,
}

impl GroupKiller {
    /// 组信号(仅 Unix 有进程组语义;非 Unix 不支持分离终止)
    #[cfg(unix)]
    pub fn signal(&self, sig: i32) -> Result<(), ProcessError> {
        if let Some(pgid) = self.pgid {
            let rc = unsafe { libc::kill(-pgid, sig) };
            if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return Err(ProcessError::Wait(format!(
                    "killpg({pgid}, {sig}): {}",
                    std::io::Error::last_os_error()
                )));
            }
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn signal(&self, _sig: i32) -> Result<(), ProcessError> {
        Err(ProcessError::Wait(
            "detached group kill requires unix process groups".into(),
        ))
    }

    /// SIGTERM → grace → SIGKILL;**不等待回收**——回收与状态收尾
    /// 由持有 Child 的一方(watcher)负责
    pub async fn kill_detached(&self, grace: Duration) -> bool {
        if self.signal(libc::SIGTERM).is_err() {
            return false;
        }
        tokio::time::sleep(grace).await;
        let _ = self.signal(libc::SIGKILL);
        true
    }
}

/// 退出分类(沙箱语境):命令 / 运行器 / 拒绝的判别
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitClass {
    /// 命令执行并退出(含被拒绝后的常规失败;退出码由调用方解读)
    Ran(ExitStatus),
    /// 沙箱 runner 失败:命令从未执行(bwrap/seatbelt 自身崩溃、拒绝 profile)
    RunnerFailed {
        /// 退出码(等待失败时为 None)
        code: Option<i32>,
        /// 命中的致命行原文
        detail: String,
    },
    /// 沙箱拒绝:命令执行了但某文件效果被内核拦(dialect 命中的 stderr 行)
    Denied {
        /// 退出状态
        status: ExitStatus,
        /// 命中的拒绝行原文
        line: String,
    },
}

/// 按 runner 失败规则 → 拒绝方言 → 常规排序分类
/// (「runner failure means the command never ran, while denial means
/// confinement worked and blocked it」;匹配为大小写不敏感后缀判定,
/// 即 `RunnerFailureRule`/`DENIAL_SIGNATURES` 语义)
pub fn classify_exit(status: ExitStatus, stderr: &str, confined: &Confined) -> ExitClass {
    let code = status.code;
    // 1) runner 失败:退出码门 + fatal 签名(经 informational 行排除后);
    //    仅非零码参与(信号致死视为命令已执行,归常规分类)
    if let Some(c) = code
        && c != 0
    {
        for rule in confined.runner_failure_rules {
            if let Some(ok) = rule.allowed_exit_codes
                && !ok.contains(&c)
            {
                continue;
            }
            for line in stderr.lines() {
                let l = line.trim();
                if rule
                    .informational_lines
                    .iter()
                    .any(|info| l.eq_ignore_ascii_case(info))
                {
                    continue;
                }
                if rule
                    .fatal_signatures
                    .iter()
                    .any(|s| l.to_lowercase().contains(&s.to_lowercase()))
                {
                    return ExitClass::RunnerFailed {
                        code,
                        detail: l.to_string(),
                    };
                }
            }
        }
    }
    // 2) 拒绝方言
    for line in stderr.lines() {
        let l = line.trim();
        if confined
            .denial_dialect
            .iter()
            .any(|d| l.to_lowercase().contains(d))
        {
            return ExitClass::Denied {
                status,
                line: l.to_string(),
            };
        }
    }
    // 3) 常规退出
    ExitClass::Ran(status)
}

#[cfg(unix)]
pub(crate) fn to_status(status: std::process::ExitStatus) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus {
        code: status.code(),
        signal: status.signal(),
    }
}

#[cfg(not(unix))]
pub(crate) fn to_status(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus {
        code: status.code(),
        signal: None,
    }
}
