//! PTY spawn:portable-pty 包装 + 沙箱 argv 包装。
//!
//! 用途:需要终端语义的工具(isatty、彩色输出、行缓冲交互)。
//! codex 同款包法(portable-pty + 宿主侧进程管理)。
//!
//! 沙箱:portable-pty 的 CommandBuilder 不暴露 pre_exec,故沙箱只能经
//! argv 包装(bwrap / seatbelt rung)——与 pipes 路径同一探测链。
//! Landlock-only 系统(无法 argv 包装)按 fail-closed **拒绝** PTY 执行,
//! 不静默降级为无沙箱。
//!
//! 读取是阻塞 IO,经 `spawn_blocking` 桥接;master drop 即向会话送
//! SIGHUP(PTY 语义),配合 `kill` 的组信号。

use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use thiserror::Error;

use crate::sandbox::{Rung, SandboxPolicy, wrap_argv};

/// PTY 错误
#[derive(Debug, Error)]
pub enum PtyError {
    /// PTY 系统调用失败
    #[error("pty: {0}")]
    Io(String),
    /// 沙箱拒绝(landlock-only 系统无法 argv 包装,fail-closed)
    #[error("pty sandbox: {0}")]
    Sandbox(String),
}

/// PTY 会话:子进程 + master + killer 句柄。
///
/// `wait` 消费子进程句柄(退出后 killer 仍可用于补杀);
/// `kill` 走 killer 组信号。
pub struct PtySession {
    child: Option<Box<dyn Child + Send>>,
    master: Box<dyn MasterPty + Send>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    /// 已等待过的退出结果(重复 wait 幂等返回)
    exit: Option<crate::process::ExitStatus>,
}

/// 在 PTY 中 spawn(program + argv 已按沙箱策略包装)。
pub fn spawn_pty(
    program: &str,
    args: &[String],
    cwd: Option<&std::path::Path>,
    sandbox: Option<&SandboxPolicy>,
) -> Result<PtySession, PtyError> {
    // 沙箱 argv 包装(bwrap/seatbelt);landlock-only 返回 Err 即拒绝
    let (program, args) = match sandbox {
        Some(policy) => {
            let probed = crate::sandbox::probe().ok_or_else(|| {
                PtyError::Sandbox(
                    "fail-closed:PTY 执行需要 argv 包装沙箱(bwrap/seatbelt),本机无可用 rung".into(),
                )
            })?;
            if matches!(probed.rung, Rung::Landlock) {
                return Err(PtyError::Sandbox(
                    "fail-closed:PTY 执行需要 argv 包装沙箱(bwrap/seatbelt),本机仅 landlock(pre_exec 不可用于 PTY)".into(),
                ));
            }
            let confined = wrap_argv(policy, program, args, &probed).map_err(|e| {
                PtyError::Sandbox(format!(
                    "fail-closed:PTY 执行需要 argv 包装沙箱(bwrap/seatbelt),\
                     本机不可用({e})"
                ))
            })?;
            (confined.program, confined.argv)
        }
        None => (program.to_string(), args.to_vec()),
    };

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize::default())
        .map_err(|e| PtyError::Io(e.to_string()))?;
    let mut command = CommandBuilder::new(&program);
    command.args(&args);
    if let Some(dir) = cwd {
        command.cwd(dir);
    }
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| PtyError::Io(e.to_string()))?;
    let killer = child.clone_killer();
    // slave 必须 drop:子进程持有副本时 master 读不到 EOF
    drop(pair.slave);
    Ok(PtySession {
        child: Some(child),
        master: pair.master,
        killer: Some(killer),
        exit: None,
    })
}

impl PtySession {
    /// 读取全部输出直到 EOF(阻塞 IO 经 spawn_blocking;取消/超时由
    /// 调用方在 select 层处理并 kill)
    pub async fn read_to_end(&mut self) -> Result<String, PtyError> {
        let mut reader = self
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Io(e.to_string()))?;
        tokio::task::spawn_blocking(move || {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut reader, &mut buf).ok();
            String::from_utf8_lossy(&buf).into_owned()
        })
        .await
        .map_err(|e| PtyError::Io(e.to_string()))
    }

    /// 等待子进程退出(幂等;阻塞 wait 经 spawn_blocking)。
    /// 返回完整退出状态(退出码/信号;成功与否由调用方语义决定)
    pub async fn wait(&mut self) -> Result<crate::process::ExitStatus, PtyError> {
        if let Some(exit) = self.exit {
            return Ok(exit);
        }
        let Some(mut child) = self.child.take() else {
            // 句柄已失(重复 wait 后的异常路径):状态不可知,视为失败
            return Ok(crate::process::ExitStatus {
                code: None,
                signal: None,
            });
        };
        // portable-pty 的 ExitStatus 只携带 successful 布尔(无码/信号),
        // 成功即码 0,失败则状态不可知(码/信号留 None)
        let status = tokio::task::spawn_blocking(move || {
            child.wait().map(|s| crate::process::ExitStatus {
                code: if s.success() { Some(0) } else { None },
                signal: None,
            })
        })
        .await
        .map_err(|e| PtyError::Io(e.to_string()))?
        .map_err(|e| PtyError::Io(e.to_string()))?;
        self.exit = Some(status);
        Ok(status)
    }

    /// 杀死子进程(killer 组信号;已退出则无操作)
    pub fn kill(&mut self) {
        if let Some(mut killer) = self.killer.take() {
            killer.kill().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pty_runs_command_with_tty_semantics() {
        let dir = std::env::temp_dir().join(format!("dsh-pty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut session = spawn_pty(
            "/bin/bash",
            &["-c".into(), "test -t 1 && echo tty-ok".into()],
            Some(&dir),
            None,
        )
        .expect("pty spawn");
        let output = session.read_to_end().await.expect("read");
        let status = session.wait().await.expect("wait");
        assert!(status.success(), "exit 应成功");
        assert!(
            output.contains("tty-ok"),
            "PTY 下 stdout 是 tty;got: {output}"
        );
    }

    #[tokio::test]
    async fn pty_sandbox_denies_out_of_root_write_on_argv_rungs() {
        use crate::sandbox::SandboxPolicy;
        let dir = std::env::temp_dir().join(format!("dsh-pty-sbx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let outside = std::env::temp_dir().join(format!("dsh-pty-outside-{}", std::process::id()));
        // mode 语义下的只读策略:一切写(含 temp 下)均被拒——验证 PTY
        // 路径沙箱生效(argv 包装 rung)
        let policy = SandboxPolicy::read_only();
        let result = spawn_pty(
            "/bin/bash",
            &["-c".into(), format!("touch {}", outside.display())],
            Some(&dir),
            Some(&policy),
        );
        match result {
            Ok(mut session) => {
                let _ = session.read_to_end().await;
                let status = session.wait().await.unwrap_or(crate::process::ExitStatus {
                    code: None,
                    signal: None,
                });
                session.kill();
                assert!(!status.success(), "根外写必须失败(seatbelt/bwrap rung)");
                assert!(!outside.exists());
            }
            Err(PtyError::Sandbox(_)) => {
                // landlock-only 环境:fail-closed 拒绝,正确
            }
            Err(e) => panic!("unexpected: {e}"),
        }
    }
}
