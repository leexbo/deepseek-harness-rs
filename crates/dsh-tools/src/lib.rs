//! tools 组件:工具 schema 注册与执行编排(native 阶段)。
//!
//! `BashTool` 把沙箱链接进 agent loop:工具执行 = 宿主 spawn
//! (fail-closed 沙箱、SIGTERM→grace→SIGKILL、独立进程组),
//! 输出经 tool/result 事件记录后进入下一轮请求派生(记录 ⟺ 可见)。
//! 执行世界能力束 = BashTool 携带的 cwd + SandboxPolicy,
//! 整体传递、可窄化(「执行世界」思想)。

use std::path::PathBuf;
use std::time::Duration;

use dsh_agent_loop::{CancelToken, ToolCallRequest, ToolOutput, ToolPort, ToolView};
use dsh_sandbox::process::ExitStatus;
use dsh_sandbox::pty::spawn_pty;
use dsh_sandbox::{ExitClass, SandboxMode, SandboxPolicy};
use dsh_sandbox::{SpawnOptions, spawn as spawn_child};
use serde_json::{Value, json};

pub mod ask_question;
pub mod file;
pub mod goal;
pub mod jobs;
pub mod plan;
pub mod session_query;
pub mod subagent;
pub mod todo;
pub mod workflow;

/// 会话权限模式动态源:工具执行时解析(源语义——日志 fold,权限
/// 事件落档即对下一次执行生效,无需重装配)。缺省 = 装配期静态策略
/// (CLI/测试装配)。
pub type ModeSource = std::sync::Arc<dyn Fn() -> SandboxMode + Send + Sync>;

pub use ask_question::{AskQuestionPort, AskQuestionTool, QuestionItem, QuestionOption};
pub use file::FileTools;
pub use goal::GoalTool;
pub use jobs::{JobRecord, JobTool, JobsRegistry, next_job_id};
pub use plan::PlanTool;
pub use subagent::{SubagentControlTool, SubagentRecord, SubagentRegistry, SubagentTool};
pub use todo::TodoWriteTool;
pub use workflow::{RALPH_DONE, RalphTool, WorkflowTool};

/// bash 工具:命令执行经沙箱链(pipes 或 PTY)
pub struct BashTool {
    /// 工作目录(同时是默认可写根)
    pub cwd: PathBuf,
    /// 沙箱策略(执行世界能力束;fail-closed)
    pub policy: SandboxPolicy,
    /// 终止宽限(SIGTERM→grace→SIGKILL 的 grace)
    pub grace: Duration,
    /// PTY 模式(需要终端语义的命令:isatty/彩色/行缓冲)
    pub pty: bool,
    /// 软取消令牌(执行中 select:取消即杀子进程并温和返回)
    pub cancel: CancelToken,
    /// 后台任务注册表(run_in_background 路径;None = 不支持后台)
    pub jobs: Option<JobsRegistry>,
    /// 会话权限模式动态源(execute 时解析;缺省 = 装配期静态 policy)
    pub mode_source: Option<ModeSource>,
}

impl BashTool {
    /// 以工作目录构建:workspace-write 策略(cwd + 平台临时区可写;
    /// 编译类工具在 /tmp 落中间产物
    /// 不会被拦,与 write/file 工具能力同源)
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        Self {
            policy: SandboxPolicy::workspace_write(cwd.clone()),
            cwd,
            grace: Duration::from_secs(5),
            pty: false,
            cancel: CancelToken::new(),
            jobs: None,
            mode_source: None,
        }
    }

    /// 覆写沙箱策略(访问模式:read-only = 无可写根;full-access = 全盘可写)
    pub fn with_policy(mut self, policy: SandboxPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// 挂动态模式源(会话日志 fold;权限切换落档即生效,空闲与运行中一致)
    pub fn with_mode_source(mut self, source: ModeSource) -> Self {
        self.mode_source = Some(source);
        self
    }

    /// 执行时解析策略:动态源优先,静态 policy 兜底。ReadOnly 仍走沙箱链
    /// (无可写根)——读命令可用,写被内核拦并带拒绝标记,优于装配期
    /// 「不装 bash」的粗粒度压制
    fn resolve_policy(&self) -> SandboxPolicy {
        match self.mode_source.as_ref().map(|f| f()) {
            Some(SandboxMode::FullAccess) => SandboxPolicy::full_access(),
            Some(SandboxMode::WorkspaceWrite) => SandboxPolicy::workspace_write(self.cwd.clone()),
            Some(SandboxMode::ReadOnly) => SandboxPolicy::read_only(),
            None => self.policy.clone(),
        }
    }

    /// 启用 PTY 模式(沙箱经 argv 包装;landlock-only 系统拒绝执行)
    pub fn with_pty(mut self) -> Self {
        self.pty = true;
        self
    }

    /// 设置软取消令牌(与会话令牌共享)
    pub fn with_cancel(mut self, token: CancelToken) -> Self {
        self.cancel = token;
        self
    }

    /// 注入后台任务注册表(启用 run_in_background;与 jobs 工具共享)
    pub fn with_jobs(mut self, registry: JobsRegistry) -> Self {
        self.jobs = Some(registry);
        self
    }

    /// 工具 schema(注册面;OpenAI wire 形状 `tools[]` 元素)
    pub fn spec() -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run a shell command in the sandboxed working directory.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The bash command to execute." },
                        "description": {
                            "type": "string",
                            "description": "Clear, concise description of what this command does in active voice, 5-10 words (shown in the UI). Examples: \"ls\" → \"List files in current directory\"; \"git status\" → \"Show working tree status\"; \"npm install\" → \"Install package dependencies\"."
                        },
                        "run_in_background": { "type": "boolean", "description": "Run detached; returns a job id immediately (manage via the jobs tool)" }
                    },
                    "required": ["command", "description"],
                },
            },
        })
    }

    /// PTY 路径:沙箱经 argv 包装;取消即 killer 组信号杀进程
    async fn execute_pty(&mut self, command: &str) -> ToolOutput {
        let policy = self.resolve_policy();
        let mut session = match spawn_pty(
            "/bin/bash",
            &["-c".into(), command.to_string()],
            Some(&self.cwd),
            Some(&policy),
        ) {
            Ok(s) => s,
            Err(e) => {
                return ToolOutput {
                    output: format!("pty spawn failed: {e}"),
                    success: false,
                    ..Default::default()
                };
            }
        };
        let output = tokio::select! {
            out = session.read_to_end() => out.unwrap_or_default(),
            _ = self.cancel.cancelled() => {
                session.kill();
                return ToolOutput { output: "cancelled".into(), success: false, ..Default::default() };
            }
        };
        let status = session.wait().await.unwrap_or(ExitStatus {
            code: None,
            signal: None,
        });
        let (success, exit_code, signal) = settle(status);
        ToolOutput {
            output: output.trim().to_string(),
            success,
            view: Some(terminal_view(exit_code, signal, Some(&self.cwd))),
        }
    }

    /// 后台执行:沙箱 spawn + 注册表登记 + watcher 任务(输出落盘、
    /// 状态收尾);立即返回 job id。后台任务不随父取消终止(它是
    /// 「后台」的全部意义),由 jobs 工具显式 stop。
    async fn execute_background(&mut self, command: &str) -> ToolOutput {
        let Some(registry) = self.jobs.clone() else {
            return ToolOutput {
                output: "background jobs not enabled in this assembly".into(),
                success: false,
                ..Default::default()
            };
        };
        let opts = SpawnOptions {
            cwd: Some(self.cwd.clone()),
            env: Default::default(),
            sandbox: Some(self.resolve_policy()),
        };
        let child = match spawn_child("/bin/bash", &["-c".into(), command.to_string()], &opts).await
        {
            Ok(child) => child,
            Err(e) => {
                return ToolOutput {
                    output: format!("spawn failed: {e}"),
                    success: false,
                    ..Default::default()
                };
            }
        };
        let id = next_job_id(&registry);
        let jobs_dir = self.cwd.join(".dsh/jobs");
        if let Err(e) = std::fs::create_dir_all(&jobs_dir) {
            return ToolOutput {
                output: format!("jobs dir create failed: {e}"),
                success: false,
                ..Default::default()
            };
        }
        let log_path = jobs_dir.join(format!("{id}.log"));
        let path_str = log_path.display().to_string();

        let killer = child.group_killer();
        registry
            .lock()
            .map(|mut r| {
                r.push(JobRecord {
                    id,
                    command: command.to_string(),
                    status: "running".into(),
                    log_path: log_path.clone(),
                    killer: Some(killer),
                    // 分离终止宽限:SIGTERM 后 1s 即 SIGKILL(不等前台 grace)
                    grace: Duration::from_secs(1),
                })
            })
            .ok();

        // watcher:stdout/stderr 收尾 → 落盘 → 状态收尾(kill 后也经此路径)。
        // stderr 一并落盘:沙箱拒绝/脚本错误文本不进 log 则排查无据
        let mut child = child;
        let watcher = async move {
            let out = child.stdout().await.unwrap_or_default();
            let status = child.wait().await.ok();
            let mut log = out;
            log.extend_from_slice(child.stderr_text().await.as_bytes());
            let _ = std::fs::write(&log_path, &log);
            let (status_label, exited_ok) = if let Some(s) = status {
                (if s.success() { "done" } else { "failed" }, s.success())
            } else {
                ("failed", false)
            };
            if let Ok(mut r) = registry.lock()
                && let Some(job) = r.iter_mut().find(|j| j.id == id)
            {
                // stop 抢先标记 stopped(watcher 只改 running,不覆盖)
                if job.status == "running" {
                    job.status = status_label.into();
                }
                job.killer = None;
            }
            exited_ok
        };
        tokio::spawn(watcher);

        ToolOutput {
            output: format!("started job {id} (log: {path_str}; jobs tool: list/read/stop)"),
            success: true,
            ..Default::default()
        }
    }
}

impl ToolPort for BashTool {
    /// 工具声明(engine 注入请求 header 供模型选择)
    fn specs(&self) -> Vec<Value> {
        vec![Self::spec()]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        if call.name != "bash" {
            return ToolOutput {
                output: format!("unknown tool: {}", call.name),
                success: false,
                ..Default::default()
            };
        }
        // OpenAI 兼容 wire 上 arguments 是 JSON 编码字符串;本地夹具是对象。
        // 两种形态都接受(字符串解析失败按空对象处理,由下方必填检查拒绝)
        let arguments: Value = if let Some(s) = call.arguments.as_str() {
            serde_json::from_str(s).unwrap_or(json!({}))
        } else {
            call.arguments.clone()
        };
        let Some(command) = arguments["command"].as_str() else {
            return ToolOutput {
                output: "bash tool requires arguments.command (string)".into(),
                success: false,
                ..Default::default()
            };
        };
        // description 必填(给用户看的一句意图说明,进 UI
        // 摘要;非空校验错误消息固定文案)
        let description_empty = arguments["description"]
            .as_str()
            .map(str::trim)
            .map(str::is_empty)
            .unwrap_or(true);
        if description_empty {
            return ToolOutput {
                output: "invalid description: expected a non-empty string".into(),
                success: false,
                ..Default::default()
            };
        }
        // 后台路径:spawn + 注册 + 立即返回 job id;
        // watcher 任务收尾状态并把输出落盘 .dsh/jobs/<id>.log
        if arguments["run_in_background"].as_bool().unwrap_or(false) {
            return self.execute_background(command).await;
        }
        if self.pty {
            return self.execute_pty(command).await;
        }
        let policy = self.resolve_policy();
        let opts = SpawnOptions {
            cwd: Some(self.cwd.clone()),
            env: Default::default(),
            sandbox: Some(policy.clone()),
        };
        // spawn 失败(含 fail-closed 沙箱拒绝)即工具失败,不中断 loop
        let child = match spawn_child("/bin/bash", &["-c".into(), command.to_string()], &opts).await
        {
            Ok(child) => child,
            Err(e) => {
                return ToolOutput {
                    output: format!("spawn failed: {e}"),
                    success: false,
                    ..Default::default()
                };
            }
        };
        let mut child = child;
        let output = tokio::select! {
            out = child.stdout() => String::from_utf8_lossy(&out.unwrap_or_default()).trim().to_string(),
            // 软取消:杀子进程(SIGTERM→grace→SIGKILL)后温和返回
            _ = self.cancel.cancelled() => {
                // 已取消,杀失败无补救手段(进程可能已退出)
                let _ = child.kill_with_grace(self.grace).await;
                return ToolOutput { output: "cancelled".into(), success: false, ..Default::default() };
            }
        };
        // 沙箱分类落定(denialSignatures + runnerFailureRules
        // 语义):runner 失败(命令从未执行)/ 拒绝(内核拦截了文件效果)/
        // 常规退出(success=true,退出码是数据、不是失败)
        let (success, exit_code, signal, rendered) = match child.wait_classified().await {
            ExitClass::Ran(status) => {
                let (s, c, sig) = settle(status);
                (s, c, sig, output)
            }
            ExitClass::RunnerFailed { code, detail } => (
                false,
                code,
                None,
                format!("sandbox runner 失败(命令未执行):\n{detail}"),
            ),
            ExitClass::Denied { status, .. } => {
                // 拒绝标记 + stderr 原文
                let stderr = child.stderr_text().await;
                (
                    false,
                    status.code,
                    None,
                    format!("{}\n{}", denial_marker(policy.mode), stderr.trim()),
                )
            }
        };
        ToolOutput {
            output: rendered,
            success,
            view: Some(terminal_view(exit_code, signal, Some(&self.cwd))),
        }
    }
}

/// 落定退出状态 → Terminal 渲染意图(前台/PTY 共用;后台启动与
/// 执行错误无退出状态,不产视图走通用卡)
fn terminal_view(
    exit_code: Option<i32>,
    signal: Option<String>,
    cwd: Option<&std::path::Path>,
) -> ToolView {
    ToolView::Terminal {
        exit_code,
        signal,
        cwd: cwd.map(|c| c.display().to_string()),
    }
}

/// 拒绝标记(`sandboxDenialMarker` 模式插值),
/// 模型据此识别「沙箱拦截而非命令逻辑错误」(模式名与 dsh-core
/// `permission.rs` 字符串一致)
fn denial_marker(mode: SandboxMode) -> String {
    let mode = match mode {
        SandboxMode::ReadOnly => "read-only",
        SandboxMode::WorkspaceWrite => "workspace-write",
        SandboxMode::FullAccess => "full-access",
    };
    format!("[sandbox: file access denied under {mode} mode]")
}

/// 退出状态 → (success, exit_code, signal):
/// - 有退出码(任意值):落定成功,码作为数据透出
/// - 信号终止:失败 + 信号名
/// - 状态不可知:失败,无线索
fn settle(status: ExitStatus) -> (bool, Option<i32>, Option<String>) {
    match (status.code, status.signal) {
        (Some(code), _) => (true, Some(code), None),
        (None, Some(sig)) => (false, None, Some(signal_name(sig))),
        (None, None) => (false, None, None),
    }
}

/// 信号编号 → 名(1–15 在 macOS/Linux 一致;其余回数值名,无损)
fn signal_name(sig: i32) -> String {
    let name = match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        11 => "SIGSEGV",
        12 => "SIGUSR2",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return format!("SIG{sig}"),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拒绝标记快照(模式名与
    /// dsh-core `permission.rs` 字符串一致,模型据此识别沙箱拦截)
    #[test]
    fn denial_marker_matches_mode_strings() {
        assert_eq!(
            denial_marker(SandboxMode::ReadOnly),
            "[sandbox: file access denied under read-only mode]"
        );
        assert_eq!(
            denial_marker(SandboxMode::WorkspaceWrite),
            "[sandbox: file access denied under workspace-write mode]"
        );
        assert_eq!(
            denial_marker(SandboxMode::FullAccess),
            "[sandbox: file access denied under full-access mode]"
        );
    }

    /// 落定快照:有码(任意值)即成功、信号终止失败、状态不可知失败
    #[test]
    fn settle_snapshot() {
        let s = |code, signal| ExitStatus { code, signal };
        assert_eq!(settle(s(Some(0), None)), (true, Some(0), None));
        assert_eq!(settle(s(Some(2), None)), (true, Some(2), None));
        assert_eq!(
            settle(s(None, Some(15))),
            (false, None, Some("SIGTERM".into()))
        );
        assert_eq!(settle(s(None, None)), (false, None, None));
    }

    /// description 必填:缺参/空白拒绝,错误消息固定文案;
    /// 合法调用不受影响
    #[tokio::test]
    async fn bash_requires_non_empty_description() {
        let dir = std::env::temp_dir().join(format!("dsh-bash-desc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut tool = BashTool::new(&dir);
        for args in [
            json!({ "command": "echo hi" }),
            json!({ "command": "echo hi", "description": "   " }),
        ] {
            let out = ToolPort::execute(
                &mut tool,
                &ToolCallRequest { name: "bash".into(), arguments: args },
            )
            .await;
            assert!(!out.success);
            assert_eq!(
                out.output,
                "invalid description: expected a non-empty string"
            );
        }
        let ok = ToolPort::execute(
            &mut tool,
            &ToolCallRequest {
                name: "bash".into(),
                arguments: json!({ "command": "echo hi", "description": "Echo greeting" }),
            },
        )
        .await;
        assert!(ok.success, "{}", ok.output);
    }

    /// 动态模式源:execute 时实时解析——read-only 下写 workspace 根被
    /// 内核拦(拒绝标记带模式名),翻转 workspace-write 后同一命令放行。
    /// 权限切换落档即生效的执行面基础
    #[tokio::test]
    async fn bash_mode_source_resolved_per_execute() {
        let dir = std::env::temp_dir().join(format!("dsh-bash-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mode = std::sync::Arc::new(std::sync::Mutex::new(SandboxMode::ReadOnly));
        let mode_for_tool = std::sync::Arc::clone(&mode);
        let mut tool =
            BashTool::new(&dir).with_mode_source(std::sync::Arc::new(move || {
                *mode_for_tool.lock().unwrap()
            }));
        let call = |cmd: String| ToolCallRequest {
            name: "bash".into(),
            arguments: json!({ "command": cmd, "description": "Probe write" }),
        };
        let denied =
            ToolPort::execute(&mut tool, &call(format!("touch {}/f", dir.display()))).await;
        assert!(!denied.success, "read-only 写应被拦:{:?}", denied.output);
        assert!(
            denied.output.contains("read-only mode"),
            "拒绝标记应带模式名:{:?}",
            denied.output
        );
        *mode.lock().unwrap() = SandboxMode::WorkspaceWrite;
        let ok = ToolPort::execute(&mut tool, &call(format!("touch {}/f", dir.display()))).await;
        assert!(ok.success, "workspace-write 写应放行:{:?}", ok.output);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 新构建路径:workspace-write 根推导(cwd + 平台临时区),而非裸 cwd
    #[test]
    fn bash_new_derives_workspace_write_policy() {
        let tool = BashTool::new("/tmp/example-ws");
        assert_eq!(tool.policy.mode, SandboxMode::WorkspaceWrite);
        assert!(
            tool.policy
                .writable_roots()
                .iter()
                .any(|r| r.ends_with("example-ws"))
        );
        // /tmp 与 temp_dir 在 macOS 上都是符号链接(→ /private/*),
        // 按 roots 推导同款 canonicalize 后比较
        let tmp =
            std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
        assert!(tool.policy.writable_roots().iter().any(|r| r == &tmp));
        assert_eq!(tool.cwd, PathBuf::from("/tmp/example-ws"));
    }
}
