//! 计划评审端口(turn 内阻塞评审的宿主面契约)。

use std::pin::Pin;

/// 评审终局决定(port 返回;工具映射为同一 tool-call 的 tool/result)。
#[derive(Debug, Clone, PartialEq)]
pub enum PlanReviewDecision {
    /// 批准:退出 plan 模式,模型自下一步开始实施。
    Approve,
    /// 拒绝:留在 plan 模式,反馈回传模型修订重提。
    Decline {
        /// 用户反馈(空/缺省 = 无文字反馈,仅「继续规划」)
        feedback: Option<String>,
    },
}

/// 评审被关闭/中断时 port 返回的错误文案(回传模型;照源 ASK_CANCELLED
/// 语义——用户拿回轮次,模型停在原地等消息)。
pub const DISMISSED_REVIEW_ERROR: &str = "The user dismissed the plan review to speak instead; stay in plan mode, stop here, and wait for their message.";

/// 计划评审端口(宿主注入;实现方:dsh-core AppHost 桌面/嵌入、Gateway、CLI)。
///
/// 阻塞评审(严格阻塞语义,同 [`dsh_tools`] 的 AskQuestionPort):落
/// `plan/submitted` + 广播 `question/requested` → await 用户应答 → 落终局
/// 事件(approved+mode / declined / cancelled)→ 返回决定。
/// `Err` = 评审被关闭/取消/中断,文案即回传模型的错误文本。
pub trait PlanReviewPort: Send + Sync {
    /// 阻塞评审一次计划提交。
    fn review(
        &self,
        session_id: &str,
        plan: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<PlanReviewDecision, String>> + Send>>;
}
