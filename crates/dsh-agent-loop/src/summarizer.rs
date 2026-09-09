//! 折叠摘要端口:engine 触发历史折叠时的一次性出网调用。
//!
//! 该调用**不是会话面请求**:其 messages 是待折叠前缀而非会话派生,
//! 不经闸门的 derive-and-compare 比对。持久化走另一条同样完整的链:
//! engine 在调用前后落 `audit/call`(operation=compaction,归因当前
//! user/message)与 `compaction/summary` 事件(载荷即记录)——重放读
//! 记录、不重调(确定性重放的前提)。

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::RequestHeader;

/// 一次性摘要调用端口(宿主实现;engine 持有的 transport 须同时实现)
pub trait Summarizer {
    /// 以会话 header 的模型配置发起摘要请求,返回摘要文本。
    /// 实现自行替换 system 与 tools(一次性请求不携带工具目录)。
    fn summarize<'a>(
        &'a mut self,
        header: &'a RequestHeader,
        messages: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}
