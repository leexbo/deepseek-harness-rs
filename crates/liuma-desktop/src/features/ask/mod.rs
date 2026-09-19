//! 问答卡、计划审批卡与沙箱升级审批卡切片:
//! 模型 `ask_user_question` 抛出的问题集(pager 一题一答)、待审计划的
//! 批准/拒绝与沙箱升级审批(一次两钮),均在消息流底部栈挂卡;应答经
//! host.respond 回填,工具结果作为同一 tool-call 的 tool/result。
//! pending_ask/pending_plan/pending_approval 属 StoreState 会话镜像
//! (reducer 侧),本切片只消费其值、经本域应答。

mod approval;
mod ask_question;
mod plan_review;
pub(crate) mod store;

pub(crate) use approval::render as render_approval;
pub(crate) use ask_question::render as render_question;
pub(crate) use plan_review::render as render_plan;
pub(crate) use store::AskStore;
