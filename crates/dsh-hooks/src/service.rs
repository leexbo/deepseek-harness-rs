//! service:HookService(配置加载 + 逐点运行 + 最严格合并)与
//! HookPortImpl(dsh-agent-loop HookPort 实现)。
//!
//! 桥定位照源:**兼容适配器**。配置读不到/解析不了 ⇒ 不注册任何钩子
//! (warn 由宿主记);UserPromptSubmit/Stop 忽略 matcher;PreToolUse
//! matcher 主语 = 工具名;SessionStart matcher 主语 = 会话 source。
//! hook 对经宿主 sink 落档(turn 外不落,照源)。

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dsh_agent_loop::CancelToken;
use dsh_agent_loop::hooks::{
    HookPort, PostToolVerdict, PreStepVerdict, PreToolVerdict, StopVerdict,
};
use dsh_agent_loop::tools::ToolCallRequest;
use serde_json::Value;

use crate::codec::HookOutput;
use crate::config::{BridgeDialect, HookConfig};
use crate::events::HookDialect;
use crate::merge::{MergedDecision, MergedHookOutcome, merge_hook_outputs};

/// 落档回调(宿主注入):`(type, data)` 唯一写入口。
pub type HookSink = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// 单桥运行时(解析后配置 + 方言元数据)
pub struct Bridge {
    pub dialect: BridgeDialect,
    pub config: HookConfig,
    /// CC:CLAUDE_PROJECT_DIR 值(显式配置 > 会话工作区);Codex 无
    pub project_dir: Option<String>,
    pub default_timeout_ms: u64,
    pub stderr_summary_max_chars: usize,
    /// plugin 名(mislabel guard 染色用)
    pub plugin: &'static str,
}

impl Bridge {
    pub fn new(
        dialect: BridgeDialect,
        config: HookConfig,
        project_dir: Option<String>,
        default_timeout_ms: u64,
        stderr_summary_max_chars: usize,
    ) -> Self {
        Self {
            dialect,
            config,
            project_dir,
            default_timeout_ms,
            stderr_summary_max_chars,
            plugin: match dialect {
                BridgeDialect::ClaudeCode => "hooks-claude-code",
                BridgeDialect::Codex => "hooks-codex",
            },
        }
    }
}

/// hooks 运行时:多桥共享(进程级配置,所有会话共用解析结果)。
pub struct HookService {
    bridges: Vec<Bridge>,
    /// 每桥 handler 计数(handlerId 单调;跨会话单调与源一致)
    counters: Vec<AtomicU64>,
    cancel: CancelToken,
}

impl HookService {
    /// 单桥构造(宿主装配辅助;超时/摘要缺省照源)
    pub fn bridge(
        dialect: crate::config::BridgeDialect,
        config: HookConfig,
        project_dir: Option<String>,
        default_timeout_ms: Option<u64>,
        stderr_summary_max_chars: Option<usize>,
    ) -> Bridge {
        Bridge::new(
            dialect,
            config,
            project_dir,
            default_timeout_ms.unwrap_or(crate::runner::DEFAULT_HOOK_TIMEOUT_MS),
            stderr_summary_max_chars.unwrap_or(crate::events::DEFAULT_STDERR_SUMMARY_MAX_CHARS),
        )
    }

    /// 装配:已解析的桥列表(空 = 无钩子;宿主据此不挂 HookPort)
    pub fn new(bridges: Vec<Bridge>, cancel: CancelToken) -> Self {
        let counters = bridges.iter().map(|_| AtomicU64::new(0)).collect();
        Self {
            bridges,
            counters,
            cancel,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bridges.is_empty()
    }

    /// SessionStart(detached emit 点;照源:不落 hook 对——turn 外
    /// 运行,noop sink)。plain-stdout-as-context 为 Codex 专属
    /// (SessionStart/UserPromptSubmit;exit 0 非 `{` 开头 stdout)。
    pub async fn run_session_start(
        &self,
        session_id: &str,
        cwd: &Path,
        source: &str,
        sandbox: Option<dsh_sandbox::SandboxPolicy>,
    ) -> MergedHookOutcome {
        self.run_point_plain_stdout(
            "SessionStart",
            source,
            session_id,
            cwd,
            &|dialect| {
                crate::payloads::session_start(
                    dialect,
                    session_id,
                    &cwd.display().to_string(),
                    source,
                    "",
                )
            },
            None,
            sandbox,
            now_ms,
        )
        .await
    }

    pub fn cancel(&self) -> &CancelToken {
        &self.cancel
    }

    /// 逐桥运行一个 hook 点(串行按配置序,最严格合并)。
    ///
    /// * `point`:事件名;`match_query`:matcher 主语(空 = 该点忽略
    ///   matcher);`payload_builder`:按方言构造载荷(惰性——只对配置了
    ///   该点的桥构造)
    /// * `sink` + `turn`:hook 对落档;`turn = None` = turn 外不落
    #[allow(clippy::too_many_arguments)]
    pub async fn run_point(
        &self,
        point: &str,
        match_query: &str,
        session_id: &str,
        cwd: &Path,
        payload_builder: &(dyn Fn(HookDialect) -> Value + Send + Sync),
        sink: Option<(&HookSink, Option<u64>)>,
        sandbox: Option<dsh_sandbox::SandboxPolicy>,
        now_ms: impl Fn() -> i64 + Send + Sync + Copy,
    ) -> MergedHookOutcome {
        self.run_point_inner(
            point,
            match_query,
            session_id,
            cwd,
            payload_builder,
            sink,
            sandbox,
            false,
            now_ms,
        )
        .await
    }

    /// 带方言后处理变体(Codex plain-stdout-as-context;SessionStart/
    /// UserPromptSubmit 专属)
    #[allow(clippy::too_many_arguments)]
    pub async fn run_point_plain_stdout(
        &self,
        point: &str,
        match_query: &str,
        session_id: &str,
        cwd: &Path,
        payload_builder: &(dyn Fn(HookDialect) -> Value + Send + Sync),
        sink: Option<(&HookSink, Option<u64>)>,
        sandbox: Option<dsh_sandbox::SandboxPolicy>,
        now_ms: impl Fn() -> i64 + Send + Sync + Copy,
    ) -> MergedHookOutcome {
        self.run_point_inner(
            point,
            match_query,
            session_id,
            cwd,
            payload_builder,
            sink,
            sandbox,
            true,
            now_ms,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_point_inner(
        &self,
        point: &str,
        match_query: &str,
        _session_id: &str,
        cwd: &Path,
        payload_builder: &(dyn Fn(HookDialect) -> Value + Send + Sync),
        sink: Option<(&HookSink, Option<u64>)>,
        sandbox: Option<dsh_sandbox::SandboxPolicy>,
        plain_stdout_as_context: bool,
        now_ms: impl Fn() -> i64 + Send + Sync + Copy,
    ) -> MergedHookOutcome {
        let mut outputs: Vec<HookOutput> = Vec::new();
        for (idx, bridge) in self.bridges.iter().enumerate() {
            let Some(groups) = bridge.config.get(point) else {
                continue;
            };
            let dialect = bridge.dialect.dialect();
            let payload = payload_builder(dialect);
            // 方言 env(CC:CLAUDE_PROJECT_DIR;Codex 无)
            let hook_env: Vec<(String, String)> = match bridge.dialect {
                BridgeDialect::ClaudeCode => bridge
                    .project_dir
                    .as_ref()
                    .map(|d| vec![("CLAUDE_PROJECT_DIR".to_string(), d.clone())])
                    .unwrap_or_default(),
                BridgeDialect::Codex => Vec::new(),
            };
            for group in groups {
                // matcher 过滤(该点无主语时配置已丢 matcher = match-all)
                if !crate::matcher::matches_matcher(
                    group.matcher.as_deref(),
                    match_query,
                    bridge.dialect.matcher_mode(),
                ) {
                    continue;
                }
                for hook in &group.hooks {
                    let n = self.counters[idx].fetch_add(1, Ordering::Relaxed) + 1;
                    let handler_id = format!(
                        "{}:{}:{}",
                        match bridge.dialect {
                            BridgeDialect::ClaudeCode => "claude-code",
                            BridgeDialect::Codex => "codex",
                        },
                        point,
                        n
                    );
                    let invocation = crate::events::HookInvocation {
                        turn: sink.and_then(|(_, t)| t).unwrap_or(0),
                        point: point.to_string(),
                        dialect,
                        handler_id,
                        matcher: group.matcher.clone(),
                    };
                    let sink_fn = sink.map(|(f, _)| f);
                    let sink_turn = sink.and_then(|(_, t)| t);
                    let out = crate::runner::run_hook(
                        hook,
                        dialect,
                        point,
                        &payload,
                        cwd,
                        &hook_env,
                        // 沙箱策略:会话当前模式(拍板 2;宿主装配时定)
                        sandbox.clone(),
                        &self.cancel,
                        sink_fn.map(|f| {
                            (
                                f.as_ref(),
                                sink_turn,
                                Some(&invocation) as Option<&crate::events::HookInvocation>,
                            )
                        }),
                        bridge.default_timeout_ms,
                        bridge.stderr_summary_max_chars,
                        now_ms,
                    )
                    .await;
                    // updatedInput / systemMessage:照源 warn + 忽略
                    if out.output.updated_input.is_some() {
                        eprintln!(
                            "hooks: {} {} hook requested updatedInput, which is not yet honored (ignored)",
                            bridge.plugin, point
                        );
                    }
                    if let Some(m) = &out.output.system_message {
                        eprintln!(
                            "hooks: {} {} hook emitted a systemMessage, which is not yet surfaced (ignored): {m}",
                            bridge.plugin, point
                        );
                    }
                    // Codex plain-stdout-as-context(源 plainStdoutAsContext;
                    // SessionStart/UserPromptSubmit 专属,由调用方经
                    // run_point_plain_stdout 开启)
                    let mut output = out.output;
                    if plain_stdout_as_context
                        && dialect == HookDialect::Codex
                        && point != "PreToolUse"
                        && point != "PostToolUse"
                        && output.exit_code == Some(0)
                        && output.additional_context.is_none()
                        && !output.stdout.is_empty()
                        && !output.stdout.starts_with('{')
                    {
                        output.additional_context = Some(output.stdout.clone());
                    }
                    outputs.push(output);
                }
            }
        }
        merge_hook_outputs(&outputs)
    }
}

/// 工具级审批结果(对齐 dsh-tools::ApprovalOutcome 词汇)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalOutcome {
    AllowedOnce,
    Rejected,
    Cancelled,
    Unavailable,
}

/// 宿主审批面(拍板 3 的工具级审批;None = ask fail-closed deny)。
/// 入参:`(tool_name, args_summary, reason)`。
pub type ToolApprovalFn = Arc<
    dyn Fn(
            String,
            String,
            String,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolApprovalOutcome> + Send>>
        + Send
        + Sync,
>;

/// HookPort 实现:引擎四调用点 → HookService 逐点运行 + 决策映射
/// (源两桥 index.ts 的 per-event wiring)。
pub struct HookPortImpl {
    pub service: Arc<HookService>,
    pub session_id: String,
    pub workspace: PathBuf,
    pub sink: HookSink,
    /// Codex 载荷 model 字段(照源桥配置,缺省 '')
    pub model: String,
    /// 工具级审批面(拍板 3;None = ask fail-closed deny)
    pub approval: Option<ToolApprovalFn>,
    /// 会话当前沙箱策略(钩子进程执行世界;拍板 2)
    pub sandbox: Option<dsh_sandbox::SandboxPolicy>,
}

impl HookPortImpl {
    fn sink_tuple(&self, turn: u64) -> Option<(&HookSink, Option<u64>)> {
        Some((&self.sink, Some(turn)))
    }
}

impl HookPort for HookPortImpl {
    async fn on_prompt_submit(&self, prompt: &str, turn: u64) -> PreStepVerdict {
        let merged = self
            .service
            .run_point(
                "UserPromptSubmit",
                "",
                &self.session_id,
                &self.workspace,
                &|dialect| {
                    crate::payloads::prompt_submit(
                        dialect,
                        &self.session_id,
                        &self.workspace.display().to_string(),
                        prompt,
                        &self.model,
                        turn,
                    )
                },
                self.sink_tuple(turn),
                self.sandbox.clone(),
                now_ms,
            )
            .await;
        match merged.decision {
            MergedDecision::Deny => PreStepVerdict::Reject,
            // ask 在此点无意义(照源不映射);上下文折叠由引擎 contexts
            // 之外处理——源把 additionalContext 并入 enter 的 messages。
            // RS 形态:服务返回后由宿主 prompt_with_contexts 承载;此处
            // 引擎拦截点只消费裁决,上下文由 service 落档为染色行。
            _ => PreStepVerdict::Proceed,
        }
    }

    async fn pre_tool(&self, call: &ToolCallRequest, turn: u64) -> PreToolVerdict {
        let tool_use_id = String::new();
        let merged = self
            .service
            .run_point(
                "PreToolUse",
                &call.name,
                &self.session_id,
                &self.workspace,
                &|dialect| {
                    crate::payloads::pre_tool_use(
                        dialect,
                        &self.session_id,
                        &self.workspace.display().to_string(),
                        &call.name,
                        &tool_use_id,
                        &call.arguments,
                        &self.model,
                        turn,
                    )
                },
                self.sink_tuple(turn),
                self.sandbox.clone(),
                now_ms,
            )
            .await;
        match merged.decision {
            MergedDecision::Deny => PreToolVerdict::Deny {
                reason: merged
                    .reason
                    .unwrap_or_else(|| "blocked by PreToolUse hook".to_string()),
            },
            // ask ⇒ 真实权限路径(拍板 3):allowed-once 放行;拒绝/取消/
            // 无通道各自 fail-closed deny(照源 reason 可分辨)。无审批面
            // = fail-closed:reason 含 "needs approval"(源测试锁定文案)。
            MergedDecision::Ask => {
                let reason = merged.reason.clone().unwrap_or_default();
                match &self.approval {
                    Some(appr) => {
                        let args_summary = abbreviated_args(&call.arguments);
                        let fut = appr(call.name.clone(), args_summary, reason.clone());
                        match fut.await {
                            ToolApprovalOutcome::AllowedOnce => PreToolVerdict::Proceed,
                            ToolApprovalOutcome::Rejected => PreToolVerdict::Deny {
                                reason: if reason.is_empty() {
                                    format!("tool \"{}\" requires approval (rejected)", call.name)
                                } else {
                                    reason
                                },
                            },
                            ToolApprovalOutcome::Cancelled => PreToolVerdict::Deny {
                                reason: format!("tool \"{}\" approval cancelled", call.name),
                            },
                            ToolApprovalOutcome::Unavailable => PreToolVerdict::Deny {
                                reason: format!(
                                    "tool \"{}\" needs approval (no approval channel)",
                                    call.name
                                ),
                            },
                        }
                    }
                    None => PreToolVerdict::Deny {
                        reason: format!("tool \"{}\" needs approval", call.name),
                    },
                }
            }
            _ => PreToolVerdict::Proceed,
        }
    }

    async fn post_tool(
        &self,
        call: &ToolCallRequest,
        output: &dsh_agent_loop::tools::ToolOutput,
        turn: u64,
    ) -> PostToolVerdict {
        let tool_use_id = String::new();
        let merged = self
            .service
            .run_point(
                "PostToolUse",
                &call.name,
                &self.session_id,
                &self.workspace,
                &|dialect| {
                    crate::payloads::post_tool_use(
                        dialect,
                        &self.session_id,
                        &self.workspace.display().to_string(),
                        &call.name,
                        &tool_use_id,
                        &call.arguments,
                        &output.output,
                        &self.model,
                        turn,
                    )
                },
                self.sink_tuple(turn),
                self.sandbox.clone(),
                now_ms,
            )
            .await;
        match merged.decision {
            MergedDecision::Deny => PostToolVerdict::Block {
                feedback: merged
                    .reason
                    .unwrap_or_else(|| "blocked by PostToolUse hook".to_string()),
            },
            _ => match merged.additional_context.first() {
                Some(text) => PostToolVerdict::Inject { text: text.clone() },
                None => PostToolVerdict::Pass,
            },
        }
    }

    async fn on_stop(&self, turn: u64) -> StopVerdict {
        let merged = self
            .service
            .run_point(
                "Stop",
                "",
                &self.session_id,
                &self.workspace,
                &|dialect| {
                    crate::payloads::stop(
                        dialect,
                        &self.session_id,
                        &self.workspace.display().to_string(),
                        &self.model,
                        turn,
                    )
                },
                self.sink_tuple(turn),
                self.sandbox.clone(),
                now_ms,
            )
            .await;
        match merged.decision {
            MergedDecision::Deny => StopVerdict::Continue {
                reason: merged
                    .reason
                    .unwrap_or_else(|| "continue: blocked by Stop hook".to_string()),
            },
            _ => StopVerdict::Pass,
        }
    }
}

/// 参数摘要(审批卡与审计面用;截断防日志膨胀)
fn abbreviated_args(args: &Value) -> String {
    let s = args.to_string();
    if s.chars().count() > 200 {
        let cut: String = s.chars().take(200).collect();
        format!("{cut}…")
    } else {
        s
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
