//! 后台任务:jobs 注册表 + jobs 工具。
//!
//! 单边界规则下的归属:后台任务 = 进程 + 输出文件(`.dsh/jobs/<id>.log`,
//! 文件即持久事实,不镜像入会话日志);`jobs` 工具读取时经正常 tool/result
//! 进入会话日志(读取路径就是普通工具调用)。注册表本身是瞬态活体
//! (进程终结即失活),不伪造持久事件。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dsh_agent_loop::{ToolCallRequest, ToolOutput, ToolPort};
use serde_json::{Value, json};

/// 单条后台任务记录(瞬态活体;输出文件才是持久事实)
pub struct JobRecord {
    /// 任务标识(单调递增)
    pub id: u64,
    /// 命令行
    pub command: String,
    /// 状态:running / done / failed / stopped
    pub status: String,
    /// 输出文件(落盘即事实)
    pub log_path: PathBuf,
    /// 进程组信号句柄(stop 用;与 Child 分离,不等 stdout 读)
    pub killer: Option<dsh_sandbox::GroupKiller>,
    /// 终止宽限
    pub grace: std::time::Duration,
}

/// jobs 注册表(执行工具与控制工具共享)
pub type JobsRegistry = Arc<Mutex<Vec<JobRecord>>>;

/// 下一个任务 id
pub fn next_job_id(registry: &JobsRegistry) -> u64 {
    registry
        .lock()
        .map(|r| r.iter().map(|j| j.id).max().unwrap_or(0) + 1)
        .unwrap_or(1)
}

/// jobs 工具:list / stop / read
pub struct JobTool {
    /// 注册表(与 BashTool 后台路径共享)
    pub registry: JobsRegistry,
}

impl JobTool {
    /// 以共享注册表构建
    pub fn new(registry: JobsRegistry) -> Self {
        Self { registry }
    }

    fn read_log(&self, path: &PathBuf) -> String {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                // 尾部 4KB(读取面截断;文件保留全文)
                let chars: Vec<char> = text.chars().collect();
                if chars.len() > 4096 {
                    chars[chars.len() - 4096..].iter().collect()
                } else {
                    text
                }
            }
            Err(e) => format!("(log unreadable: {e})"),
        }
    }
}

impl ToolPort for JobTool {
    fn specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "jobs",
                "description": "Manage background jobs started with bash run_in_background. list: show all jobs with status; read: show a job's output (tail 4KB); stop: terminate a running job.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["list", "read", "stop"], "description": "Operation to perform" },
                        "id": { "type": "integer", "description": "Job id (action=read/stop)" }
                    },
                    "required": ["action"],
                },
            },
        })]
    }

    async fn execute(&mut self, call: &ToolCallRequest) -> ToolOutput {
        if call.name != "jobs" {
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
        let Some(action) = arguments["action"].as_str() else {
            return fail("jobs requires arguments.action (list/read/stop)".into());
        };
        let id = arguments["id"].as_u64();
        // stop 先行处理(锁内取句柄,锁外 await——guard 不可跨 await)
        if action == "stop" {
            let Some(id) = id else {
                return fail("jobs stop requires arguments.id (integer)".into());
            };
            // 先行置位 stopped,再发信号:watcher 只改 running 状态,
            // 置位在前即不会被 watcher 的退出收尾覆盖为 failed
            let (killer, grace) = {
                let Ok(mut registry) = self.registry.lock() else {
                    return fail("jobs registry unavailable".into());
                };
                let Some(job) = registry.iter_mut().find(|j| j.id == id) else {
                    return fail(format!("job {id} not found"));
                };
                if job.status != "running" {
                    return fail(format!("job {id} is not running ({})", job.status));
                }
                let Some(killer) = job.killer.clone() else {
                    return fail(format!("job {id} handle unavailable"));
                };
                job.status = "stopped".into();
                (killer, job.grace)
            };
            // 分离终止:不持 Child(其正被 watcher 的 stdout 读占用),
            // 直接组信号;回收与状态收尾由 watcher 负责
            let killed = killer.kill_detached(grace).await;
            return ToolOutput {
                output: format!("stopping job {id} (signal sent: {killed})"),
                success: true,
                ..Default::default()
            };
        }
        let Ok(registry) = self.registry.lock() else {
            return fail("jobs registry unavailable".into());
        };
        match action {
            "list" => {
                if registry.is_empty() {
                    return ToolOutput {
                        output: "(no jobs)".into(),
                        success: true,
                        ..Default::default()
                    };
                }
                let lines: Vec<String> = registry
                    .iter()
                    .map(|j| format!("{}. [{}] {}", j.id, j.status, j.command))
                    .collect();
                ToolOutput {
                    output: lines.join("\n"),
                    success: true,
                    ..Default::default()
                }
            }
            "read" => {
                let Some(id) = id else {
                    return fail("jobs read requires arguments.id (integer)".into());
                };
                let Some(job) = registry.iter().find(|j| j.id == id) else {
                    return fail(format!("job {id} not found"));
                };
                let text = self.read_log(&job.log_path);
                ToolOutput {
                    output: format!("[{}] {}\n{}", job.status, job.command, text),
                    success: true,
                    ..Default::default()
                }
            }
            other => fail(format!("unknown action: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_id_is_monotonic() {
        let registry: JobsRegistry = Arc::new(Mutex::new(Vec::new()));
        assert_eq!(next_job_id(&registry), 1);
        registry.lock().unwrap().push(JobRecord {
            id: 1,
            command: "x".into(),
            status: "done".into(),
            log_path: PathBuf::from("/tmp/x.log"),
            killer: None,
            grace: std::time::Duration::from_secs(1),
        });
        assert_eq!(next_job_id(&registry), 2);
    }
}
