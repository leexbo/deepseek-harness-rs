//! runner:command 钩子执行(源 hook-protocol/runner.ts 语义 +
//! dsh_sandbox 原语)。
//!
//! 执行器拥有进程控制:钩子经沙箱链跑(bash -c,与模型命令同一信任面,
//! 拍板 2),stdin 序列化载荷按方言带/不带尾换行,env 在擦洗之后合并,
//! 超时 per-hook timeoutSec(秒)> defaultTimeoutMs,取消令牌杀进程组。
//! 一切失败降级为受控结果:执行器拒绝 = 无退出码的非阻塞结果,绝不抛。

use std::path::Path;
use std::time::Duration;

use dsh_agent_loop::CancelToken;
use serde_json::Value;

use crate::codec::{HookOutput, parse_hook_output};
use crate::config::CommandHook;
use crate::events::{HookDialect, HookInvocation, hook_invoked_payload, hook_result_payload};

/// 参考默认 per-hook 超时(10 分钟;CC/Codex 对未设 timeout 的钩子)
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 600_000;

/// hook/result stderrSummary 默认上限
pub use crate::events::DEFAULT_STDERR_SUMMARY_MAX_CHARS;

/// 逐钩子落档回调(宿主注入;内部走 session.session_event 唯一写入口)。
/// turn 外运行(emit 点)实现方以 noop 收敛。
pub type HookEventSink = dyn Fn(&str, Value) + Send + Sync;

/// env 擦洗(照源 executor scrub + dsh-mcp scrub_env 同规则):
/// 键含敏感词(KEY|PASSWORD|SECRET|TOKEN)或 DSH_ 前缀的继承环境不进
/// 钩子进程;显式 env 之后合并(可覆盖)。
fn scrub_env() -> std::collections::HashMap<String, String> {
    let sensitive = |k: &str| {
        let upper = k.to_ascii_uppercase();
        upper.contains("KEY")
            || upper.contains("PASSWORD")
            || upper.contains("SECRET")
            || upper.contains("TOKEN")
            || k.starts_with("DSH_")
    };
    std::env::vars().filter(|(k, _)| !sensitive(k)).collect()
}

/// 单次 hook 运行结果(输出 + 墙钟时长,供 hook/result)
pub struct RunOutcome {
    pub output: HookOutput,
    pub duration_ms: i64,
}

/// 运行一个 command 钩子并解码输出(源 runHook)。
///
/// * `point`:触发事件名(判别门 expectedEventName,两方言都开启照源)
/// * `sink`:hook 事件落档回调(invoked 先于运行、result 后于运行;
///   `sink_turn` = None 表示 turn 外,不落记录,照源)
/// * `hook_env`:方言 env(CC 的 CLAUDE_PROJECT_DIR;Codex 无)
/// * `sandbox`:会话当前沙箱策略(照源 base 组合:钩子与模型命令同一
///   信任面;拍板 2)
#[allow(clippy::too_many_arguments)]
pub async fn run_hook(
    hook: &CommandHook,
    dialect: HookDialect,
    point: &str,
    payload: &Value,
    cwd: &Path,
    hook_env: &[(String, String)],
    sandbox: Option<dsh_sandbox::SandboxPolicy>,
    cancel: &CancelToken,
    sink: Option<(&HookEventSink, Option<u64>, Option<&HookInvocation>)>,
    default_timeout_ms: u64,
    stderr_summary_max_chars: usize,
    now_ms: impl Fn() -> i64,
) -> RunOutcome {
    let started = now_ms();
    let timeout = Duration::from_millis(
        hook.timeout_sec
            .map(|s| (s * 1000.0) as u64)
            .unwrap_or(default_timeout_ms),
    );
    let stdin = crate::payloads::serialize_stdin(dialect, payload);
    let (sink_fn, sink_turn, invocation) = match sink {
        Some((f, t, i)) => (Some(f), t, i),
        None => (None, None, None),
    };
    if let (Some(f), Some(_turn), Some(inv)) = (sink_fn, sink_turn, invocation) {
        f("hook/invoked", hook_invoked_payload(inv));
    }

    let mut env = scrub_env();
    for (k, v) in hook_env {
        env.insert(k.clone(), v.clone());
    }
    let opts = dsh_sandbox::SpawnOptions {
        cwd: Some(cwd.to_path_buf()),
        env,
        sandbox,
        stdin: Some(stdin),
    };
    let spawned =
        dsh_sandbox::spawn("/bin/bash", &["-c".into(), hook.command.clone()], &opts).await;

    // 超时 + 取消都是非阻塞收尾:杀进程组后按信号死(无退出码)解码
    let output = match spawned {
        Ok(mut child) => {
            let wait = async {
                let out = child.stdout().await.unwrap_or_default();
                let status = child.wait().await.ok();
                let err = child.stderr_text().await;
                (out, status, err)
            };
            let timed = async {
                tokio::time::sleep(timeout).await;
            };
            let cancelled = async {
                cancel.cancelled().await;
            };
            tokio::select! {
                (out, status, err) = wait => {
                    let exit_code = status.and_then(|s| s.code);
                    parse_hook_output(
                        exit_code,
                        &String::from_utf8_lossy(&out),
                        &err,
                        Some(point),
                    )
                }
                _ = timed => {
                    let _ = child.kill_with_grace(Duration::from_millis(500)).await;
                    parse_hook_output(None, "", "hook timed out", Some(point))
                }
                _ = cancelled => {
                    let _ = child.kill_with_grace(Duration::from_millis(500)).await;
                    parse_hook_output(None, "", "hook cancelled", Some(point))
                }
            }
        }
        Err(e) => {
            // 执行器拒绝(基础设施故障)= 非阻塞:无退出码,失败进 stderr 记录
            parse_hook_output(None, "", &e.to_string(), Some(point))
        }
    };

    let duration_ms = now_ms() - started;
    if let (Some(f), Some(turn), Some(inv)) = (sink_fn, sink_turn, invocation) {
        let _ = turn;
        f(
            "hook/result",
            hook_result_payload(
                turn,
                &inv.point,
                &inv.handler_id,
                &output,
                stderr_summary_max_chars,
                duration_ms,
            ),
        );
    }
    RunOutcome {
        output,
        duration_ms,
    }
}
