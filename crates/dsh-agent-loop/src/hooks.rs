//! HookPort:引擎拦截点 trait(源七扩展点的 RS 收敛形态;M4.2 拍板 1)。
//!
//! 源拦截点 → RS 四调用点:agent/pre-step → on_prompt_submit;
//! tools/pre-execute → pre_tool;tools/post-execute → post_tool;
//! agent/turn-stopping → on_stop。SessionStart / subagent:* 由宿主
//! detached 处理(不进引擎)。全部可选:None 时引擎零开销直通。
//!
//! 决策语义照源:PreToolUse deny ⇒ 工具不执行、isError 结果回灌;
//! PostToolUse block ⇒ 结果改写 + feedback;Stop continue ⇒ steer 强制
//! 续跑;UserPromptSubmit reject ⇒ turn 以 blocked 收尾、无 step。
//! hook/invoked·result 落档归实现方(经宿主 append 回调,唯一写入口)。

use serde_json::Value;

use crate::tools::ToolCallRequest;

/// UserPromptSubmit 裁决
#[derive(Debug, Clone, PartialEq)]
pub enum PreStepVerdict {
    /// 放行(进入正常消息组装)
    Proceed,
    /// 拒绝:turn 直接收尾,无 step(源 {kind:'reject'})
    Reject,
}

/// PreToolUse 裁决
#[derive(Debug, Clone, PartialEq)]
pub enum PreToolVerdict {
    Proceed,
    /// 工具不执行,reason 作为 isError 结果回灌模型
    Deny {
        reason: String,
    },
}

/// PostToolUse 裁决(结果已产出,决定如何落 tool/result)
#[derive(Debug, Clone, PartialEq)]
pub enum PostToolVerdict {
    Pass,
    /// 结果改写:output = feedback、success = false(源 block+feedback)
    Block {
        feedback: String,
    },
    /// 结果照落,其后追加一条注入上下文(源 context-only 委托折叠)
    Inject {
        text: String,
    },
}

/// Stop 裁决
#[derive(Debug, Clone, PartialEq)]
pub enum StopVerdict {
    Pass,
    /// 强制续跑:reason 压入引擎 steer 通道(源 agent.steer)
    Continue {
        reason: String,
    },
}

/// 引擎拦截点(源 waterfall listeners 的 RS 形态)。
///
/// 实现方(dsh-hooks HookPortImpl)负责 hook/invoked·result 落档与
/// 多钩子合并;引擎只消费最终裁决。`turn` 为引擎侧本 turn 序号
/// (自 1 起,与 hook/* 事件载荷的 turn 同源)。
pub trait HookPort: Send + Sync {
    /// UserPromptSubmit:turn/start 落档后、首条 user/message 前
    fn on_prompt_submit(
        &self,
        prompt: &str,
        turn: u64,
    ) -> impl std::future::Future<Output = PreStepVerdict> + Send;

    /// PreToolUse:tool/call 落档后、工具执行前(取消安全点之后)
    fn pre_tool(
        &self,
        call: &ToolCallRequest,
        turn: u64,
    ) -> impl std::future::Future<Output = PreToolVerdict> + Send;

    /// PostToolUse:工具执行后、tool/result 落档前
    fn post_tool(
        &self,
        call: &ToolCallRequest,
        output: &crate::tools::ToolOutput,
        turn: u64,
    ) -> impl std::future::Future<Output = PostToolVerdict> + Send;

    /// Stop:无 tool_calls break 后、turn/end 落档前
    fn on_stop(&self, turn: u64) -> impl std::future::Future<Output = StopVerdict> + Send;
}

/// HookPort 的对象安全形态(与 ToolPortObj 同理;引擎经 Box<dyn> 持有)
pub trait HookPortObj: Send + Sync {
    fn on_prompt_submit<'a>(
        &'a self,
        prompt: &'a str,
        turn: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PreStepVerdict> + Send + 'a>>;
    fn pre_tool<'a>(
        &'a self,
        call: &'a ToolCallRequest,
        turn: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PreToolVerdict> + Send + 'a>>;
    fn post_tool<'a>(
        &'a self,
        call: &'a ToolCallRequest,
        output: &'a crate::tools::ToolOutput,
        turn: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PostToolVerdict> + Send + 'a>>;
    fn on_stop<'a>(
        &'a self,
        turn: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = StopVerdict> + Send + 'a>>;
}

impl<T: HookPort> HookPortObj for T {
    fn on_prompt_submit<'a>(
        &'a self,
        prompt: &'a str,
        turn: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PreStepVerdict> + Send + 'a>> {
        Box::pin(HookPort::on_prompt_submit(self, prompt, turn))
    }
    fn pre_tool<'a>(
        &'a self,
        call: &'a ToolCallRequest,
        turn: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PreToolVerdict> + Send + 'a>> {
        Box::pin(HookPort::pre_tool(self, call, turn))
    }
    fn post_tool<'a>(
        &'a self,
        call: &'a ToolCallRequest,
        output: &'a crate::tools::ToolOutput,
        turn: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PostToolVerdict> + Send + 'a>> {
        Box::pin(HookPort::post_tool(self, call, output, turn))
    }
    fn on_stop<'a>(
        &'a self,
        turn: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = StopVerdict> + Send + 'a>> {
        Box::pin(HookPort::on_stop(self, turn))
    }
}

/// 注入上下文的 source 染色(源 mislabel guard:kind=plugin;
/// RS 面沿用 source.kind 字符串,宿主 translate 原样透传)
pub fn hook_context_source(dialect: &str) -> Value {
    serde_json::json!({ "kind": "plugin", "plugin": dialect })
}

#[cfg(test)]
mod tests {
    use crate::hooks::HookPortObj;
    use crate::hooks::{HookPort, PostToolVerdict, PreStepVerdict, PreToolVerdict, StopVerdict};
    use crate::tools::{ToolCallRequest, ToolOutput};
    use std::sync::{Arc, Mutex};

    /// 记录调用点的测试 HookPort(锁事件序断言)
    #[derive(Default)]
    struct RecordingHook {
        calls: Mutex<Vec<String>>,
        deny_tool: bool,
        reject_prompt: bool,
        continue_stop: bool,
    }

    impl RecordingHook {
        fn note(&self, s: &str) {
            self.calls.lock().unwrap().push(s.to_string());
        }
    }

    impl HookPort for RecordingHook {
        async fn on_prompt_submit(&self, _prompt: &str, turn: u64) -> PreStepVerdict {
            self.note(&format!("prompt-submit:{turn}"));
            if self.reject_prompt {
                PreStepVerdict::Reject
            } else {
                PreStepVerdict::Proceed
            }
        }
        async fn pre_tool(&self, call: &ToolCallRequest, turn: u64) -> PreToolVerdict {
            self.note(&format!("pre-tool:{}:{turn}", call.name));
            if self.deny_tool {
                PreToolVerdict::Deny {
                    reason: "policy says no".into(),
                }
            } else {
                PreToolVerdict::Proceed
            }
        }
        async fn post_tool(
            &self,
            _call: &ToolCallRequest,
            _output: &ToolOutput,
            turn: u64,
        ) -> PostToolVerdict {
            self.note(&format!("post-tool:{turn}"));
            PostToolVerdict::Pass
        }
        async fn on_stop(&self, turn: u64) -> StopVerdict {
            self.note(&format!("stop:{turn}"));
            if self.continue_stop {
                StopVerdict::Continue {
                    reason: "keep going".into(),
                }
            } else {
                StopVerdict::Pass
            }
        }
    }

    #[tokio::test]
    async fn hook_port_obj_dispatches_all_four_points() {
        let hook = Arc::new(RecordingHook {
            deny_tool: false,
            reject_prompt: false,
            continue_stop: false,
            calls: Mutex::new(Vec::new()),
        });
        let obj: Arc<dyn HookPortObj> = hook.clone();
        assert_eq!(
            HookPortObj::on_prompt_submit(&*obj, "hi", 1).await,
            PreStepVerdict::Proceed
        );
        let call = ToolCallRequest {
            name: "bash".into(),
            arguments: serde_json::json!({}),
        };
        assert_eq!(
            HookPortObj::pre_tool(&*obj, &call, 1).await,
            PreToolVerdict::Proceed
        );
        let out = ToolOutput::default();
        assert_eq!(
            HookPortObj::post_tool(&*obj, &call, &out, 1).await,
            PostToolVerdict::Pass
        );
        assert_eq!(HookPortObj::on_stop(&*obj, 1).await, StopVerdict::Pass);
        assert_eq!(
            *hook.calls.lock().unwrap(),
            vec![
                "prompt-submit:1".to_string(),
                "pre-tool:bash:1".to_string(),
                "post-tool:1".to_string(),
                "stop:1".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn deny_and_reject_verdicts_carry_reasons() {
        let hook = RecordingHook {
            deny_tool: true,
            reject_prompt: true,
            continue_stop: true,
            calls: Default::default(),
        };
        assert_eq!(
            HookPort::on_prompt_submit(&hook, "x", 2).await,
            PreStepVerdict::Reject
        );
        let call = ToolCallRequest {
            name: "rm".into(),
            arguments: serde_json::json!({}),
        };
        assert_eq!(
            HookPort::pre_tool(&hook, &call, 2).await,
            PreToolVerdict::Deny {
                reason: "policy says no".into()
            }
        );
        assert_eq!(
            HookPort::on_stop(&hook, 2).await,
            StopVerdict::Continue {
                reason: "keep going".into()
            }
        );
    }

    #[test]
    fn hook_context_source_labels_plugin() {
        let v = crate::hooks::hook_context_source("hooks-claude-code");
        assert_eq!(v["kind"], "plugin");
        assert_eq!(v["plugin"], "hooks-claude-code");
    }
}
