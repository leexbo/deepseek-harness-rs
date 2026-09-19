//! plan 模式协作状态(照源 packages/plan/plan-mode 包边界收拢)。
//!
//! plan 模式是逐 agent 的布尔协作状态:激活期间每个模型请求都携带一段
//! 部署方指引(plan:policy),`exit_plan_mode` 把完成的计划提交用户评审
//! ——**在 turn 内阻塞等待**,批准/拒绝/取消的结果作为同一 tool-call 的
//! tool/result 回传模型;`/plan off` 让用户直接离开。沙箱模式与审批策略
//! 独立执行,不读写 plan 状态(指引非安全边界)。
//!
//! 持久形态是 log-only 整值替换事件(`session/mode`、`plan/*`;声明与
//! 簿记在 liuma-session,防依赖环),resume/fork/compaction 靠折叠日志复原。
//! 工具目录跨模式不变(request-cache 稳定):进入/离开 plan 模式只改
//! 提示词段,不增删工具;`exit_plan_mode` 在两种模式下恒注册。
//!
//! 模块图:
//! - state:纯函数折叠(当前模式/待审计划/活跃计划)
//! - section:prompt 段(plan-mode 约束段 + active-plan 段)
//! - tool:exit_plan_mode 工具(校验 + 评审 port 直通)
//! - review:PlanReviewPort(宿主面:liuma-core 桌面/嵌入、Gateway、CLI)
//! - invariant:plan 族事件载荷形状校验

pub mod invariant;
pub mod review;
pub mod section;
pub mod state;
pub mod tool;

pub use review::{DISMISSED_REVIEW_ERROR, PlanReviewDecision, PlanReviewPort};
pub use section::{HeaderPlanSections, header_sections};
pub use state::{current_mode, mode_envelope, pending_plan, plan_envelope, plan_state};
pub use tool::PlanTool;
