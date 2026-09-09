//! 工具端口:agent-loop 拥有的 trait,宿主/工具层实现。
//!
//! 对应 WIT `dsh:tools`(tools/* 编排在 wasm 侧的形态);
//! native 阶段执行面经宿主 spawn+沙箱链(`dsh-tools::BashTool`)。

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::presentation::ToolView;

/// 一次工具调用请求(来自 assistant/message 的 tool_calls 项)
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRequest {
    /// 工具名
    pub name: String,
    /// 调用参数
    pub arguments: Value,
}

/// 工具执行输出
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolOutput {
    /// 工具输出文本(stdout/stderr 合并,由实现决定;模型派生消费面)
    pub output: String,
    /// 是否成功
    pub success: bool,
    /// 渲染意图(result 侧):工具在文本扁平化之前持有的类型化视图。
    /// None = 无结构化意图,UI 走通用 IN/OUT 卡
    pub view: Option<ToolView>,
}

/// 工具端口(engine 逐步调用;实现方持有执行世界/沙箱策略)
pub trait ToolPort {
    /// 工具声明(OpenAI function 形状;engine 注入请求 header 供模型选择)。
    /// 默认空 = 不向模型声明工具。
    fn specs(&self) -> Vec<Value> {
        Vec::new()
    }

    /// 执行一次工具调用
    fn execute(
        &mut self,
        call: &ToolCallRequest,
    ) -> impl std::future::Future<Output = ToolOutput> + Send;

    /// 随上次 [`execute`](ToolPort::execute) 产生的持久状态事件。
    ///
    /// 单边界规则:工具不直接写日志,只缓冲 (type, data);engine 在
    /// tool/result 落日志后依次取走追加——seq 分配与 sink 触发仍在
    /// 唯一写入口。默认空 = 无状态工具。
    fn take_state_events(&mut self) -> Vec<(String, Value)> {
        Vec::new()
    }

    /// call 侧渲染意图(运行中意图视图;如 file_edit 的 old/new diff)。
    /// engine 在 tool/call 事件落档时调用,视图随事件持久化。默认
    /// None = 无 call 侧意图(多数工具的意图只在 result 侧成形)
    fn present_call(&self, _call: &ToolCallRequest) -> Option<ToolView> {
        None
    }
}

/// 空工具集(无工具时注入;未知名调用返回失败结果而非中断 loop)
#[derive(Default)]
pub struct NoTools;

impl ToolPort for NoTools {
    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        ToolOutput {
            output: format!("unknown tool: {}", call.name),
            success: false,
            ..Default::default()
        }
    }
}

/// 装配期错误:两个工具声明了同一 function.name
#[derive(Debug, thiserror::Error)]
#[error("duplicate tool name: {0}")]
pub struct DuplicateToolNameError(pub String);

/// [`ToolPort`] 的对象安全形态(RPITIT trait 不能直接 dyn)。
///
/// 异构工具集合的元素形态;经 blanket impl,任意 `ToolPort + Send`
/// 的具体类型装箱后即可混装。
pub trait ToolPortObj: Send {
    /// 工具声明(同 [`ToolPort::specs`])
    fn specs(&self) -> Vec<Value>;

    /// 执行一次工具调用(boxed future)
    fn execute<'a>(
        &'a mut self,
        call: &'a ToolCallRequest,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>;

    /// 持久状态事件(同 [`ToolPort::take_state_events`];默认空)
    fn take_state_events(&mut self) -> Vec<(String, Value)> {
        Vec::new()
    }

    /// call 侧渲染意图(同 [`ToolPort::present_call`];默认 None)
    fn present_call(&self, _call: &ToolCallRequest) -> Option<ToolView> {
        None
    }
}

impl<T: ToolPort + Send> ToolPortObj for T {
    fn specs(&self) -> Vec<Value> {
        ToolPort::specs(self)
    }

    fn execute<'a>(
        &'a mut self,
        call: &'a ToolCallRequest,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(ToolPort::execute(self, call))
    }

    fn take_state_events(&mut self) -> Vec<(String, Value)> {
        ToolPort::take_state_events(self)
    }

    fn present_call(&self, call: &ToolCallRequest) -> Option<ToolView> {
        ToolPort::present_call(self, call)
    }
}

/// 工具集:多工具的名字分发层。
///
/// `specs()` 聚合各工具声明(engine 注入请求 header);执行按
/// function.name 派发到声明它的工具;名字冲突装配期拒绝;
/// 未知名返回失败结果而非中断 loop(NoTools 语义)。
pub struct ToolSet {
    tools: Vec<Box<dyn ToolPortObj>>,
}

impl std::fmt::Debug for ToolSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 元素是 trait 对象,只报告数量(测试断言 unwrap_err 需要 Debug)
        f.debug_struct("ToolSet")
            .field("tools", &self.tools.len())
            .finish()
    }
}

impl ToolSet {
    /// 构建工具集;两个工具声明同一 function.name 即装配失败(fail-fast)
    pub fn new(tools: Vec<Box<dyn ToolPortObj>>) -> Result<Self, DuplicateToolNameError> {
        let mut names = HashSet::new();
        for tool in &tools {
            for spec in tool.specs() {
                if let Some(name) = spec["function"]["name"].as_str() {
                    let fresh = names.insert(name.to_string());
                    if !fresh {
                        return Err(DuplicateToolNameError(name.to_string()));
                    }
                }
            }
        }
        Ok(Self { tools })
    }
}

impl ToolPort for ToolSet {
    fn specs(&self) -> Vec<Value> {
        self.tools.iter().flat_map(|t| t.specs()).collect()
    }

    /// 聚合各工具的状态事件(todo/write 等;engine 于 tool/result 后取走)
    fn take_state_events(&mut self) -> Vec<(String, Value)> {
        self.tools
            .iter_mut()
            .flat_map(|t| t.take_state_events())
            .collect()
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        let Some(tool) = self
            .tools
            .iter_mut()
            .find(|t| t.specs().iter().any(|s| s["function"]["name"] == call.name))
        else {
            return ToolOutput {
                output: format!("unknown tool: {}", call.name),
                success: false,
                ..Default::default()
            };
        };
        tool.execute(call).await
    }

    /// call 侧意图按名路由(同 execute 的分发规则)
    fn present_call(&self, call: &ToolCallRequest) -> Option<ToolView> {
        self.tools
            .iter()
            .find(|t| t.specs().iter().any(|s| s["function"]["name"] == call.name))
            .and_then(|t| t.present_call(call))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoTool {
        name: &'static str,
    }

    impl ToolPort for EchoTool {
        fn specs(&self) -> Vec<Value> {
            json!([{
                "type": "function",
                "function": { "name": self.name, "parameters": {} },
            }])
            .as_array()
            .cloned()
            .unwrap_or_default()
        }

        async fn execute(&mut self, _call: &ToolCallRequest) -> ToolOutput {
            ToolOutput {
                output: self.name.into(),
                success: true,
                ..Default::default()
            }
        }
    }

    fn call(name: &str) -> ToolCallRequest {
        ToolCallRequest {
            name: name.into(),
            arguments: json!({}),
        }
    }

    #[tokio::test]
    async fn aggregates_specs_and_dispatches_by_name() {
        let mut set = ToolSet::new(vec![
            Box::new(EchoTool { name: "alpha" }),
            Box::new(EchoTool { name: "beta" }),
        ])
        .expect("no conflict");
        let specs = ToolPort::specs(&set);
        let names: Vec<String> = specs
            .iter()
            .filter_map(|s| s["function"]["name"].as_str().map(String::from))
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
        // ToolSet 同时实现 ToolPort 与(经 blanket)ToolPortObj,
        // 显式消歧走 ToolPort 面
        assert_eq!(
            ToolPort::execute(&mut set, &call("beta")).await.output,
            "beta"
        );
        assert_eq!(
            ToolPort::execute(&mut set, &call("alpha")).await.output,
            "alpha"
        );
    }

    #[tokio::test]
    async fn unknown_name_fails_softly() {
        let mut set = ToolSet::new(vec![Box::new(EchoTool { name: "alpha" })]).unwrap();
        let out = ToolPort::execute(&mut set, &call("nope")).await;
        assert!(!out.success);
        assert_eq!(out.output, "unknown tool: nope");
    }

    #[test]
    fn duplicate_name_rejected_at_assembly() {
        let err = ToolSet::new(vec![
            Box::new(EchoTool { name: "alpha" }),
            Box::new(EchoTool { name: "alpha" }),
        ])
        .unwrap_err();
        assert_eq!(err.0, "alpha");
    }

    #[test]
    fn present_call_routes_by_name_and_defaults_none() {
        use crate::presentation::ToolView;
        // EchoTool 未覆写 present_call:默认 None
        let set = ToolSet::new(vec![Box::new(EchoTool { name: "alpha" })]).unwrap();
        assert!(ToolPort::present_call(&set, &call("alpha")).is_none());
        assert!(ToolPort::present_call(&set, &call("nope")).is_none());

        struct DiffTool;
        impl ToolPort for DiffTool {
            fn specs(&self) -> Vec<Value> {
                json!([{ "type": "function", "function": { "name": "file_edit", "parameters": {} } }])
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            }
            async fn execute(&mut self, _call: &ToolCallRequest) -> ToolOutput {
                ToolOutput::default()
            }
            fn present_call(&self, _call: &ToolCallRequest) -> Option<ToolView> {
                Some(ToolView::Diff {
                    diffs: vec![crate::presentation::FileDiff {
                        path: "a.txt".into(),
                        old_text: Some("old".into()),
                        new_text: "new".into(),
                    }],
                })
            }
        }
        let set = ToolSet::new(vec![Box::new(DiffTool)]).unwrap();
        assert!(matches!(
            ToolPort::present_call(&set, &call("file_edit")),
            Some(ToolView::Diff { .. })
        ));
    }
}
