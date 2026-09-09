//! 工作流工具:子代理原语上的薄编排。
//!
//! - `workflow`:顺序步骤链——每步一个子代理任务,上一步结果作为
//!   下一步的上下文;任一步失败即止。
//! - `ralph`:循环执行者——同一目标反复交给子代理执行,子代理回复
//!   `RALPH_DONE` 即收敛,上限轮数兜底(防不收敛循环)。
//!
//! 无新内核:两者只是 SubagentTool 的调用编排,能力束窄化、独立
//! 子日志、取消传播全部继承。

use dsh_agent_loop::{ToolCallRequest, ToolOutput, ToolPort};
use serde_json::{Value, json};

use crate::subagent::SubagentTool;

/// ralph 收敛标记(子代理回复含此串即结束循环)
pub const RALPH_DONE: &str = "RALPH_DONE";

/// workflow 工具:顺序步骤链
pub struct WorkflowTool<T> {
    /// 编排用的子代理(独立传输,注册表与顶层共享)
    pub subagent: SubagentTool<T>,
}

impl<T> WorkflowTool<T>
where
    T: dsh_agent_loop::LlmTransport + dsh_agent_loop::Summarizer + Send + 'static,
{
    /// 以子代理实例构建
    pub fn new(subagent: SubagentTool<T>) -> Self {
        Self { subagent }
    }

    /// 组一步的任务文本(首步原样;后续步骤携带上一步结果作链式上下文)
    fn task_for(&self, step: &str, prev: &str) -> String {
        if prev.is_empty() {
            step.to_string()
        } else {
            format!("{step}\n\nResult from the previous step:\n{prev}")
        }
    }
}

impl<T> ToolPort for WorkflowTool<T>
where
    T: dsh_agent_loop::LlmTransport + dsh_agent_loop::Summarizer + Send + 'static,
{
    fn specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "workflow",
                "description": "Run a sequence of steps, each executed by an isolated subagent; each step receives the previous step's result as context. Use for multi-stage tasks with clear handoffs.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "steps": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Ordered step descriptions (2-6 recommended)"
                        }
                    },
                    "required": ["steps"],
                },
            },
        })]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        if call.name != "workflow" {
            return ToolOutput {
                output: format!("unknown tool: {}", call.name),
                success: false,
                ..Default::default()
            };
        }
        let fail = |msg: String| ToolOutput {
            output: msg,
            success: false,
            ..Default::default()
        };
        let arguments: Value = if let Some(s) = call.arguments.as_str() {
            serde_json::from_str(s).unwrap_or(json!({}))
        } else {
            call.arguments.clone()
        };
        let Some(steps) = arguments["steps"].as_array() else {
            return fail("workflow requires arguments.steps (array of string)".into());
        };
        if steps.is_empty() {
            return fail("workflow requires at least one step".into());
        }
        let steps: Vec<String> = steps
            .iter()
            .filter_map(|s| s.as_str().map(String::from))
            .collect();
        if steps.len() != arguments["steps"].as_array().map(Vec::len).unwrap_or(0) {
            return fail("workflow steps must all be strings".into());
        }

        // 每步同步等结果(编排语义 = 前台;不经模型面,直接走子代理
        // 前台入口——不受后台默认影响)
        let mut prev = String::new();
        let mut report = String::new();
        for (i, step) in steps.iter().enumerate() {
            let out = self.subagent.run_foreground(&self.task_for(step, &prev)).await;
            if !out.success {
                return ToolOutput {
                    output: format!(
                        "workflow aborted at step {} ({step}): {}",
                        i + 1,
                        out.output
                    ),
                    success: false,
                    ..Default::default()
                };
            }
            report.push_str(&format!("== step {} ==\n{}\n\n", i + 1, out.output));
            prev = out.output;
        }
        ToolOutput {
            output: report.trim_end().to_string(),
            success: true,
            ..Default::default()
        }
    }
}

/// ralph 工具:循环执行者
pub struct RalphTool<T> {
    /// 编排用的子代理(独立传输,注册表与顶层共享)
    pub subagent: SubagentTool<T>,
    /// 轮数上限(防不收敛循环)
    pub max_rounds: usize,
}

impl<T> RalphTool<T>
where
    T: dsh_agent_loop::LlmTransport + dsh_agent_loop::Summarizer + Send + 'static,
{
    /// 以子代理实例构建(默认上限 8 轮)
    pub fn new(subagent: SubagentTool<T>) -> Self {
        Self {
            subagent,
            max_rounds: 8,
        }
    }
}

impl<T> ToolPort for RalphTool<T>
where
    T: dsh_agent_loop::LlmTransport + dsh_agent_loop::Summarizer + Send + 'static,
{
    fn specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "ralph",
                "description": "Loop a persistent goal against subagents: each round a fresh subagent works on the goal with the previous round's result as context. The subagent replies RALPH_DONE alone when the goal is complete.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "The persistent goal description" },
                        "max_rounds": { "type": "integer", "description": "Round cap (default 8)" }
                    },
                    "required": ["task"],
                },
            },
        })]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        if call.name != "ralph" {
            return ToolOutput {
                output: format!("unknown tool: {}", call.name),
                success: false,
                ..Default::default()
            };
        }
        let fail = |msg: String| ToolOutput {
            output: msg,
            success: false,
            ..Default::default()
        };
        let arguments: Value = if let Some(s) = call.arguments.as_str() {
            serde_json::from_str(s).unwrap_or(json!({}))
        } else {
            call.arguments.clone()
        };
        let Some(task) = arguments["task"].as_str() else {
            return fail("ralph requires arguments.task (string)".into());
        };
        let max_rounds = arguments["max_rounds"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(self.max_rounds)
            .min(self.max_rounds);

        let instruction = format!(
            "{task}\n\nThis is round {{ROUND}} of a persistent loop. Work toward the goal using \
the previous round's result as context. When the goal is fully achieved, reply with \
exactly {RALPH_DONE} and nothing else.\n\nPrevious round result:\n{{PREV}}"
        );
        let mut prev = String::new();
        let mut report = String::new();
        let mut converged = false;
        for round in 1..=max_rounds {
            let prompt = instruction
                .replace("{ROUND}", &round.to_string())
                .replace("{PREV}", &prev);
            // 每轮同步等结果(编排语义 = 前台,直接走子代理前台入口)
            let out = self.subagent.run_foreground(&prompt).await;
            if !out.success {
                return ToolOutput {
                    output: format!("ralph aborted at round {round}: {}", out.output),
                    success: false,
                    ..Default::default()
                };
            }
            if out.output.contains(RALPH_DONE) {
                converged = true;
                report.push_str(&format!(
                    "== round {round} (converged) ==\n{}\n",
                    out.output
                ));
                break;
            }
            report.push_str(&format!("== round {round} ==\n{}\n\n", out.output));
            prev = out.output;
        }
        if !converged {
            report.push_str(&format!("(no convergence in {max_rounds} rounds)"));
        }
        ToolOutput {
            output: report.trim_end().to_string(),
            success: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent_loop::LlmEvent;
    use dsh_llm::FakeProvider;
    use std::sync::Arc;

    /// 脚本组工厂:每次构建子代理传输时从队列弹一组脚本(FakeProvider
    /// 不 Clone;一个子代理 = 一次 factory 调用 = 一个 provider 携一组响应)
    fn scripted_factory(scripts: &[&[&str]]) -> crate::subagent::TransportFactory<FakeProvider> {
        let queue: Vec<Vec<Vec<LlmEvent>>> = scripts
            .iter()
            .map(|group| {
                group
                    .iter()
                    .map(|text| vec![LlmEvent::AssistantMessage(json!({ "content": text }))])
                    .collect()
            })
            .collect();
        let queue = std::sync::Arc::new(std::sync::Mutex::new(queue));
        Arc::new(move || {
            let mut q = queue.lock().unwrap();
            let group = if q.is_empty() { Vec::new() } else { q.remove(0) };
            let mut p = FakeProvider::new();
            for response in group {
                p.then(response);
            }
            Ok(p)
        })
    }

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dsh-wf-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn workflow_chains_steps() {
        // 两步顺序执行;第二步任务文本含第一步结果(链式上下文)
        let sub = SubagentTool::new(
            dir("chain"),
            scripted_factory(&[&["step one result: 42"], &["step two consumed 42"]]),
            "m".into(),
        );
        let mut wf = WorkflowTool::new(sub);
        let out = ToolPort::execute(
            &mut wf,
            &ToolCallRequest {
                name: "workflow".into(),
                arguments: json!({ "steps": ["compute the answer", "use the answer"] }),
            },
        )
        .await;
        assert!(out.success, "{}", out.output);
        assert!(out.output.contains("step one result: 42"));
        assert!(out.output.contains("step two consumed 42"));

        // 链式上下文:第二个子会话日志的 user/message 含第一步结果
        let registry = wf.subagent.registry.lock().unwrap().clone();
        assert_eq!(registry.len(), 2, "每步一个子代理");
        let second = std::fs::read_to_string(&registry[1].session_path).unwrap();
        assert!(
            second.contains("step one result: 42"),
            "第二步任务必须携带第一步结果"
        );
    }

    #[tokio::test]
    async fn ralph_converges_on_marker() {
        // 每轮一个 fresh 子代理(工厂语义:一次 factory 调用 = 一个子
        // 代理携一组脚本;ralph 两轮 → 两组)
        let sub = SubagentTool::new(
            dir("ralph"),
            scripted_factory(&[&["round 1: halfway"], &["RALPH_DONE"]]),
            "m".into(),
        );
        let mut ralph = RalphTool::new(sub);
        let out = ToolPort::execute(
            &mut ralph,
            &ToolCallRequest {
                name: "ralph".into(),
                arguments: json!({ "task": "finish the thing" }),
            },
        )
        .await;
        assert!(out.success, "{}", out.output);
        assert!(out.output.contains("halfway"));
        assert!(out.output.contains("converged"));
    }
}
