//! 执行原语(dsh-sandbox):OS 沙箱链与受控 spawn/PTY。
//!
//! 链语义:Linux bwrap → Landlock / macOS
//! Seatbelt,fail-closed(探测不到可用 rung 且策略要求沙箱 → 拒绝执行,
//! 绝不静默降级)。design.md §7.8 的「沙箱与进程」独立子系统在此落为
//! 独立 crate:替换沙箱后端(如容器策略)不触碰宿主核心。
//!
//! - [`sandbox`]:执行世界能力束(目录视图)+ rung 探测 + argv 包装 /
//!   landlock pre_exec 自限制;
//! - [`process`] — spawn(独立进程组、SIGTERM→grace→SIGKILL、kill-tree);
//! - [`pty`]:portable-pty 包装(需终端语义的工具;landlock-only 环境
//!   fail-closed 拒绝)。

#![deny(missing_docs)]

pub mod process;
pub mod pty;
pub mod sandbox;

pub use process::{
    Child, ExitClass, ExitStatus, GroupKiller, ProcessError, SpawnOptions, classify_exit, spawn,
};
pub use pty::{PtyError, PtySession, spawn_pty};
pub use sandbox::{
    Confined, ProbeResult, Rung, RunnerFailureRule, SandboxEnforcement, SandboxError, SandboxMode,
    SandboxPolicy, probe, wrap_argv,
};
