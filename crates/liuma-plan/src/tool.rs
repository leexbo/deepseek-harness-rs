//! exit_plan_mode 工具:模型提交计划,**turn 内阻塞**等待用户评审。
//!
//! 仅 plan 模式可调用(读共享日志最近一条 session/mode 判定);计划须为
//! 以 `#` 标题开头的非空 markdown(`^#\\s+\\S`);评审经
//! [`PlanReviewPort`](crate::PlanReviewPort) 阻塞等待,批准/拒绝/关闭的
//! 结果作为同一 tool-call 的 tool/result 回传模型(拒绝留在 plan 模式,
//! 反馈随错误文本回传,模型修订重提)。port 缺席 = 无评审通道,失败并
//! 请模型让用户手动切模式。工具目录跨模式不变(request-cache
//! 稳定),plan 态的「禁止变更」由提示词段约束承担。

use std::sync::{Arc, Mutex};

use liuma_agent_loop::{ToolCallRequest, ToolOutput, ToolPort};
use liuma_session::EventLog;
use serde_json::{Value, json};

use crate::review::{PlanReviewDecision, PlanReviewPort};
use crate::state::current_mode;

/// 批准结果文本(逐字固定):批准即开工指令,实现自下一步开始。
pub const APPROVED_RESULT: &str =
    "Plan approved — plan mode exited; carry out the plan starting with your next step.";

/// 拒绝(有反馈)结果文本模板(逐字固定)。
pub const DECLINED_WITH_FEEDBACK: &str = "The user chose to keep planning; their feedback: ";

/// 拒绝(无反馈)结果文本(逐字固定)。
pub const DECLINED_NO_FEEDBACK: &str =
    "The user chose to keep planning; revise the plan and present again.";

/// plan 工具:exit_plan_mode
pub struct PlanTool {
    /// 共享会话日志(只读:当前模式)
    log: Arc<Mutex<EventLog>>,
    /// 评审端口(宿主面;缺席 = 无评审通道)
    port: Option<Arc<dyn PlanReviewPort>>,
    /// 归属会话 id(port 定向)
    session: String,
}

impl PlanTool {
    /// 以共享日志与评审端口构建(与 engine/闸门同一日志实例)。
    pub fn new(
        log: Arc<Mutex<EventLog>>,
        port: Option<Arc<dyn PlanReviewPort>>,
        session: &str,
    ) -> Self {
        Self {
            log,
            port,
            session: session.to_string(),
        }
    }

    /// 当前模式(锁失败按缺省 standard,不连坐工具执行)
    fn current_mode(&self) -> String {
        let Ok(log) = self.log.lock() else {
            return "standard".into();
        };
        current_mode(&log)
    }
}

/// 计划须以 `#` 标题开头(`^#\\s+\\S`:# + 至少一空白 + 非空白字符)。
fn has_heading(plan: &str) -> bool {
    let mut chars = plan.chars();
    if chars.next() != Some('#') {
        return false;
    }
    let mut saw_whitespace = false;
    for c in chars {
        if c.is_whitespace() {
            saw_whitespace = true;
        } else {
            return saw_whitespace;
        }
    }
    false
}

/// 把工具入参统一成对象:若为 JSON 字符串则解析,否则原样(模型对话方言
/// 把 tool 参数作为字符串下发)。
fn args_or_json_string(args: &Value) -> Value {
    if let Some(s) = args.as_str() {
        serde_json::from_str(s).unwrap_or_else(|_| json!({}))
    } else {
        args.clone()
    }
}

impl ToolPort for PlanTool {
    fn specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "exit_plan_mode",
                "description": "Use only in plan mode. Present your plan for the user's review and, on approval, leave plan mode. Send the COMPLETE plan as markdown, starting with a # heading that names it. The user may approve (carry out the plan from your next step) or keep planning — their feedback comes back in the tool result; revise and present again.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "plan": { "type": "string", "description": "The complete plan, as markdown, starting with a # heading that names it." }
                    },
                    "required": ["plan"],
                },
            },
        })]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        let fail = |msg: String| ToolOutput {
            output: msg,
            success: false,
            ..Default::default()
        };
        if call.name != "exit_plan_mode" {
            return fail(format!("unknown tool: {}", call.name));
        }
        if self.current_mode() != "plan" {
            return fail("exit_plan_mode is only available in plan mode".into());
        }
        let arguments = args_or_json_string(&call.arguments);
        let Some(plan) = arguments["plan"].as_str() else {
            return fail("exit_plan_mode requires arguments.plan (string)".into());
        };
        if !has_heading(plan) {
            return fail(
                "exit_plan_mode requires a non-empty markdown plan starting with a # heading"
                    .into(),
            );
        }
        let Some(port) = self.port.clone() else {
            return fail(
                "no plan review channel is available to review the plan; ask the user to switch the session mode instead"
                    .into(),
            );
        };
        let session = self.session.clone();
        match port.review(&session, plan).await {
            Ok(PlanReviewDecision::Approve) => ToolOutput {
                output: APPROVED_RESULT.into(),
                success: true,
                ..Default::default()
            },
            Ok(PlanReviewDecision::Decline { feedback }) => {
                match feedback.filter(|t| !t.trim().is_empty()) {
                    Some(fb) => fail(format!("{DECLINED_WITH_FEEDBACK}{fb}")),
                    None => fail(DECLINED_NO_FEEDBACK.into()),
                }
            }
            // Err 文案由 port 决定(关闭评审 = DISMISSED_REVIEW_ERROR;
            // 取消/中断走同义指引),原样回传模型
            Err(msg) => fail(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::DISMISSED_REVIEW_ERROR;
    use liuma_session::EventEnvelope;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn call(plan: &str) -> ToolCallRequest {
        ToolCallRequest {
            name: "exit_plan_mode".into(),
            arguments: json!({ "plan": plan }),
        }
    }

    fn log_with_mode(mode: Option<&str>) -> Arc<Mutex<EventLog>> {
        let log = Arc::new(Mutex::new(EventLog::new()));
        if let Some(m) = mode {
            log.lock()
                .unwrap()
                .append(EventEnvelope::new("session/mode", 0, json!({ "mode": m })))
                .unwrap();
        }
        log
    }

    /// 脚本化评审 port:按序吐出预定结果
    struct FakePort {
        results: Mutex<Vec<Result<PlanReviewDecision, String>>>,
        seen: Mutex<Vec<String>>,
        calls: AtomicUsize,
    }

    impl FakePort {
        fn new(results: Vec<Result<PlanReviewDecision, String>>) -> Arc<Self> {
            Arc::new(Self {
                results: Mutex::new(results),
                seen: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl PlanReviewPort for FakePort {
        fn review(
            &self,
            _session_id: &str,
            plan: &str,
        ) -> Pin<Box<dyn Future<Output = Result<PlanReviewDecision, String>> + Send>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(plan.to_string());
            let result = self
                .results
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err(DISMISSED_REVIEW_ERROR.into()));
            Box::pin(async move { result })
        }
    }

    #[test]
    fn heading_validation() {
        assert!(has_heading("# Title\nbody"));
        assert!(has_heading("#\tTitle"));
        assert!(!has_heading("Title\n# later"));
        assert!(!has_heading("#"));
        assert!(!has_heading("# "));
        assert!(!has_heading(""));
        assert!(!has_heading("## sub-only 不是一级标题"));
        // 注:`^#\s+\S` 只认单个 #;## 开头不匹配
        assert!(has_heading("# x"));
    }

    #[tokio::test]
    async fn rejects_outside_plan_mode() {
        let mut t = PlanTool::new(
            log_with_mode(None),
            Some(FakePort::new(vec![Ok(PlanReviewDecision::Approve)])),
            "s",
        );
        let out = ToolPort::execute(&mut t, &call("# p")).await;
        assert!(!out.success);
        assert!(out.output.contains("only available in plan mode"));

        let mut t = PlanTool::new(
            log_with_mode(Some("standard")),
            Some(FakePort::new(vec![Ok(PlanReviewDecision::Approve)])),
            "s",
        );
        let out = ToolPort::execute(&mut t, &call("# p")).await;
        assert!(!out.success);
    }

    #[tokio::test]
    async fn rejects_plan_without_heading() {
        let mut t = PlanTool::new(
            log_with_mode(Some("plan")),
            Some(FakePort::new(vec![Ok(PlanReviewDecision::Approve)])),
            "s",
        );
        let out = ToolPort::execute(&mut t, &call("no heading plan")).await;
        assert!(!out.success);
        assert_eq!(
            out.output,
            "exit_plan_mode requires a non-empty markdown plan starting with a # heading"
        );
    }

    #[tokio::test]
    async fn fails_without_review_channel() {
        let mut t = PlanTool::new(log_with_mode(Some("plan")), None, "s");
        let out = ToolPort::execute(&mut t, &call("# p")).await;
        assert!(!out.success);
        assert_eq!(
            out.output,
            "no plan review channel is available to review the plan; ask the user to switch the session mode instead"
        );
    }

    #[tokio::test]
    async fn approve_returns_carry_out_result() {
        let mut t = PlanTool::new(
            log_with_mode(Some("plan")),
            Some(FakePort::new(vec![Ok(PlanReviewDecision::Approve)])),
            "s",
        );
        let out = ToolPort::execute(&mut t, &call("# fix the bug\n1. read code")).await;
        assert!(out.success, "{}", out.output);
        assert_eq!(out.output, APPROVED_RESULT);
    }

    #[tokio::test]
    async fn decline_with_feedback_returns_error_carrying_feedback() {
        let mut t = PlanTool::new(
            log_with_mode(Some("plan")),
            Some(FakePort::new(vec![Ok(PlanReviewDecision::Decline {
                feedback: Some("use OAuth, not hand-rolled".into()),
            })])),
            "s",
        );
        let out = ToolPort::execute(&mut t, &call("# p")).await;
        assert!(!out.success);
        assert_eq!(
            out.output,
            "The user chose to keep planning; their feedback: use OAuth, not hand-rolled"
        );
        // 空白反馈视同无反馈
        let mut t = PlanTool::new(
            log_with_mode(Some("plan")),
            Some(FakePort::new(vec![Ok(PlanReviewDecision::Decline {
                feedback: Some("   ".into()),
            })])),
            "s",
        );
        let out = ToolPort::execute(&mut t, &call("# p")).await;
        assert_eq!(out.output, DECLINED_NO_FEEDBACK);
    }

    #[tokio::test]
    async fn dismissed_review_error_passes_through() {
        let mut t = PlanTool::new(
            log_with_mode(Some("plan")),
            Some(FakePort::new(vec![Err(DISMISSED_REVIEW_ERROR.into())])),
            "s",
        );
        let out = ToolPort::execute(&mut t, &call("# p")).await;
        assert!(!out.success);
        assert_eq!(out.output, DISMISSED_REVIEW_ERROR);
        assert!(out.output.contains("stay in plan mode"));
    }

    #[tokio::test]
    async fn port_receives_session_and_plan() {
        let port = FakePort::new(vec![Ok(PlanReviewDecision::Approve)]);
        let mut t = PlanTool::new(log_with_mode(Some("plan")), Some(port.clone()), "ws/stem");
        let out = ToolPort::execute(&mut t, &call("# plan body")).await;
        assert!(out.success);
        assert_eq!(port.seen.lock().unwrap().as_slice(), ["# plan body"]);
    }
}
