//! ask_user_question 工具。
//!
//! 模型调用 `ask_user_question` 向用户提一组问题(单选/多选/自定义),**阻塞**等待
//! 用户应答(严格阻塞语义):宿主经 [`AskQuestionPort`] 落 pending + 广播
//! `question/requested`,用户应答后 resolve,结果作为**同一 tool-call 的
//! tool/result 回填**(`{"answers":[{id, selected[], custom?}]}`),模型继续同一 turn。
//! 无超时预算;取消仅经用户放弃/中断。

use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use dsh_agent_loop::{ToolCallRequest, ToolOutput, ToolPort};

/// 一个问题选项(源 AskUserQuestionOption)
#[derive(Debug, Clone)]
pub struct QuestionOption {
    /// 选项 label(用户可见;推荐项约定 label 后追加 "(Recommended)")
    pub label: String,
    /// 选项说明(权衡/影响;可缺省)
    pub description: Option<String>,
}

/// 一个问题项(源 AskUserQuestionItem)
#[derive(Debug, Clone)]
pub struct QuestionItem {
    /// 稳定 id(应答时原样回显)
    pub id: String,
    /// 问题文本
    pub question: String,
    /// 可选标题(heading,如 "Confirm"/"Choose Mode")
    pub header: Option<String>,
    /// 可选选项(单选默认;multi_select 时多选)
    pub options: Vec<QuestionOption>,
    /// 是否多选
    pub multi_select: bool,
}

/// 用户应答端口(宿主注入;实现方:dsh-core AppHost)。
/// 阻塞 ask:落 pending → 广播 question/requested → await 用户 respond →
/// 返回 tool/result 文本 JSON(`{"answers":[...]}`)。
pub trait AskQuestionPort: Send + Sync {
    /// 阻塞提问(单个 ask 可含多问题);取消/中断 → Err(可恢复文案)。
    fn ask(
        &self,
        session_id: &str,
        questions: &[QuestionItem],
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>;
}

/// ask_user_question 工具(第 13 个工具;standard preset 开启)
pub struct AskQuestionTool {
    port: Arc<dyn AskQuestionPort>,
    current: String,
}

impl AskQuestionTool {
    /// 构造(port = 宿主问答面;current = 归属会话 id)
    pub fn new(port: Arc<dyn AskQuestionPort>, current: &str) -> Self {
        Self {
            port,
            current: current.to_string(),
        }
    }
}

impl ToolPort for AskQuestionTool {
    fn specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "ask_user_question",
                "description": "Ask the user a concise question when you need confirmation, a choice, or missing information before proceeding. Send one or more questions, each with a stable id that will be echoed in the answer.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "description": "Questions to ask the user before continuing.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": { "type": "string", "description": "Stable id for this question; echoed in the answer." },
                                    "question": { "type": "string", "description": "The specific question to ask the user." },
                                    "header": { "type": "string", "description": "Optional short heading for the question, such as \"Confirm\" or \"Choose Mode\"." },
                                    "options": {
                                        "type": "array",
                                        "description": "Optional choices. If you recommend one, put it first and append \"(Recommended)\" to that label.",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "label": { "type": "string", "description": "Short user-facing option label." },
                                                "description": { "type": "string", "description": "One sentence explaining the tradeoff or impact." }
                                            },
                                            "required": ["label"]
                                        }
                                    },
                                    "multi_select": { "type": "boolean", "description": "Whether the user may select more than one option. Defaults to false." }
                                },
                                "required": ["id", "question"]
                            }
                        }
                    },
                    "required": ["questions"]
                }
            }
        })]
    }

    fn execute(
        &mut self,
        call: &ToolCallRequest,
    ) -> impl std::future::Future<Output = ToolOutput> + Send {
        let port = self.port.clone();
        let current = self.current.clone();
        async move {
            let questions = match parse_questions(&call.arguments) {
                Ok(q) => q,
                Err(e) => {
                    return ToolOutput {
                        output: format!("ask_user_question 参数无效:{e}"),
                        success: false,
                        ..Default::default()
                    };
                }
            };
            match port.ask(&current, &questions).await {
                Ok(text) => ToolOutput {
                    output: text,
                    success: true,
                    ..Default::default()
                },
                Err(e) => ToolOutput {
                    output: e,
                    success: false,
                    ..Default::default()
                },
            }
        }
    }
}

/// 从工具入参解析 questions(id/question 必填;options label 必填)。
/// arguments 容忍 JSON 字符串(与 BashTool/FileTools/todo 同策略):模型对话方言
/// 把 tool 参数作为字符串下发,先尝试解析,为对象则原样使用。
fn parse_questions(args: &Value) -> Result<Vec<QuestionItem>, String> {
    let args = args_or_json_string(args);
    let arr = args["questions"]
        .as_array()
        .ok_or_else(|| "缺少 questions 数组".to_string())?;
    if arr.is_empty() {
        return Err("questions 不能为空".to_string());
    }
    arr.iter()
        .map(|q| {
            let id = q["id"]
                .as_str()
                .ok_or_else(|| "每个 question 需要 id".to_string())?
                .to_string();
            let question = q["question"]
                .as_str()
                .ok_or_else(|| "每个 question 需要 question 文本".to_string())?
                .to_string();
            let header = q["header"].as_str().map(String::from);
            let options = q["options"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|o| {
                            Ok(QuestionOption {
                                label: o["label"]
                                    .as_str()
                                    .ok_or_else(|| "选项需要 label".to_string())?
                                    .to_string(),
                                description: o["description"].as_str().map(String::from),
                            })
                        })
                        .collect::<Result<Vec<_>, String>>()
                })
                .transpose()?
                .unwrap_or_default();
            let multi_select = q["multi_select"].as_bool().unwrap_or(false);
            Ok(QuestionItem {
                id,
                question,
                header,
                options,
                multi_select,
            })
        })
        .collect()
}

/// 把工具入参统一成对象:若为 JSON 字符串则解析,否则原样。
fn args_or_json_string(args: &Value) -> Value {
    if let Some(s) = args.as_str() {
        serde_json::from_str(s).unwrap_or_else(|_| json!({}))
    } else {
        args.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_questions_valid() {
        let args = json!({
            "questions": [
                { "id": "q1", "question": "继续?", "multi_select": false,
                  "options": [ { "label": "是", "description": "进行" }, { "label": "否" } ] },
                { "id": "q2", "question": "多选?", "multi_select": true,
                  "options": [ { "label": "A" }, { "label": "B" } ] },
            ]
        });
        let q = parse_questions(&args).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].id, "q1");
        assert_eq!(q[0].options.len(), 2);
        assert_eq!(q[0].options[0].description.as_deref(), Some("进行"));
        assert!(!q[0].multi_select);
        assert!(q[1].multi_select);
    }

    #[test]
    fn parse_questions_rejects_missing_id() {
        let args = json!({ "questions": [ { "question": "无 id" } ] });
        assert!(parse_questions(&args).is_err());
    }

    #[test]
    fn parse_questions_rejects_empty() {
        assert!(parse_questions(&json!({ "questions": [] })).is_err());
        assert!(parse_questions(&json!({})).is_err());
    }

    #[test]
    fn parse_questions_accepts_json_string_args() {
        // deepseek/OpenAI 对话方言把 tool 参数作为字符串下发:arguments 是 JSON 文本
        let payload = serde_json::to_string(&json!({
            "questions": [ { "id": "q1", "question": "继续?", "options": [ { "label": "是" } ] } ]
        }))
        .unwrap();
        let args = json!(payload); // Value::String(JSON 文本)
        let q = parse_questions(&args).unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].id, "q1");
    }
}
