//! 问答卡与计划审批卡切片:
//! 模型 `ask_user_question` 抛出的问题集(pager 一题一答)与待审计划的
//! 批准/拒绝,均在消息流底部栈挂卡;应答经 host.respond 回填,工具结果
//! 作为同一 tool-call 的 tool/result。pending_ask/pending_plan 属
//! StoreState 会话镜像(reducer 侧),本切片只消费其值、经本域应答。

mod ask_question;
mod plan_review;
pub(crate) mod store;

pub(crate) use ask_question::render as render_question;
pub(crate) use plan_review::render as render_plan;
pub(crate) use store::AskStore;
