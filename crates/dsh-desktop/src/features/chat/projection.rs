//! 客方事件 → 节点流投影(纯函数;vendor `ui-conversation` ChatView 的
//! 简化版)。节点按稳定 key 增量更新(append/定稿/配对),顺序 = 事件序。
//!
//! 事件形状源 `dsh-core/src/translate.rs`(客方 camelCase);未列类型忽略。

use dsh_core::proto::SessionEvent;
use serde_json::Value;

/// 工具执行态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    /// 执行中
    Running,
    /// 成功
    Done,
    /// 失败
    Error,
}

/// todo 条目(todo/write 投影;渲染在 todo_dock)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    /// 内容
    pub content: String,
    /// completed / in_progress / pending
    pub status: String,
}

/// 消息流节点(稳定 key;同 key 增量替换)
#[derive(Debug, Clone, PartialEq)]
pub enum ChatNode {
    /// 用户消息(user/message 与 agent/inbox/spliced 插入)
    User {
        /// 稳定 key(user:<seq> / user:<id>)
        key: String,
        /// 文本
        text: String,
        /// 图片块(attachment 引用数组;空 = 纯文本)
        images: Vec<serde_json::Value>,
    },
    /// 注入上下文(user/message + source.kind ≠ "user";4a 完整溯源模型):
    /// 模型实际收到的非用户来源消息(AGENTS.md / @session 快照),折叠渲染
    /// 为「注入行」。
    Context {
        /// 稳定 key(ctx:<seq>)
        key: String,
        /// 模型可见注入文本(content)
        content: String,
        /// 来源染色(source;kind/form + producer 扩展字段)
        source: serde_json::Value,
    },
    /// 助手消息(流式增量 → assistant/message 定稿)
    Assistant {
        /// 稳定 key(a:<turn>:<step>)
        key: String,
        /// 正文
        text: String,
        /// 思考内容(Think 折叠行,点击展开)
        reasoning: String,
        /// 流式进行中
        streaming: bool,
        /// 定稿用量(durationMs/ttftMs/completionTokens)
        usage: Option<Value>,
        /// 持久消息 id(assistant/message 落档;反馈定位 key)
        message_id: String,
    },
    /// 工具调用(tool/call → tool/result 配对)
    Tool {
        /// 稳定 key(call:<callId>)
        key: String,
        /// 工具名
        name: String,
        /// 折叠摘要(工具名感知键序;见 [`summarize_call`])
        summary: String,
        /// 执行态
        state: ToolState,
        /// 原始参数(JSON 字符串)
        arguments: String,
        /// 输出(配对 tool/result)
        output: Option<String>,
        /// 渲染意图(call 侧 = 运行中意图,result 侧 = 已应用事实;
        /// 无视图 = 通用 IN/OUT 卡。窄化在 ui/toolcard)
        view: Option<Value>,
        /// 结果图片(MCP 图片桥;image 引用块数组,缺席 = 无图)
        images: Vec<Value>,
    },
    /// 回合收尾(turn/end)
    TurnTail {
        /// 稳定 key(turn-end:<seq>)
        key: String,
        /// 中断(true)/正常完成
        aborted: bool,
        /// 用量摘要(耗时 · 首 token · tok/s;自尾部最近 assistant 用量)
        meta: Option<String>,
        /// 本 turn 产出的 diff/edit 路径(产物行;去重保序)
        deliverables: Vec<String>,
    },
    /// 通告行(turn/error 等异常落档的可视化)
    Notice {
        /// 稳定 key
        key: String,
        /// 文本
        text: String,
    },
    /// 计划归档卡(plan/submitted 落档;批准/取消仅更新状态)
    Plan {
        /// 稳定 key(plan:<seq>)
        key: String,
        /// 计划正文(markdown)
        plan: String,
        status: PlanStatus,
    },
    /// 模型请求重试行(llm/retry 落档;
    /// 状态随 llm/retry-started / 取消收尾演化)
    Retry {
        /// 稳定 key(retry:<seq>)
        key: String,
        /// 第几次重试(1 起)
        retry: u32,
        /// 重试上限
        max_retries: u32,
        /// 本次退避时长(毫秒;倒计时源)
        delay_ms: u64,
        /// 失败分类码(TRANSPORT/TIMEOUT/SERVER/RATE_LIMIT/EMPTY_RESPONSE)
        code: String,
        /// 失败摘要(展开详情「失败原因」)
        message: String,
        state: RetryState,
    },
}

/// 重试行状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryState {
    /// 等待退避(直播中带倒计时;重放为静态)
    Waiting,
    /// 退避结束、重发开始(llm/retry-started)
    Started,
    /// 退避中被取消(取消收尾且未见 started)
    Cancelled,
}

impl ChatNode {
    /// 稳定 key
    pub fn key(&self) -> &str {
        match self {
            ChatNode::User { key, .. }
            | ChatNode::Assistant { key, .. }
            | ChatNode::Tool { key, .. }
            | ChatNode::TurnTail { key, .. }
            | ChatNode::Context { key, .. }
            | ChatNode::Notice { key, .. }
            | ChatNode::Plan { key, .. }
            | ChatNode::Retry { key, .. } => key,
        }
    }
}

/// session/queue 帧 items → 队列投影(preview = text 块拼接;
/// 全部块均为文本才可编辑,与源 QueuedMessage.text 一致)
pub fn parse_queue_items(items: Vec<serde_json::Value>) -> Vec<QueueEntry> {
    items
        .iter()
        .filter_map(|item| {
            let id = item["id"].as_str()?.to_string();
            let placement = match item["placement"].as_str() {
                Some("steering") => QueuePlacement::Steering,
                _ => QueuePlacement::Queued,
            };
            let blocks = item["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut preview = String::new();
            let mut all_text = true;
            for b in &blocks {
                match b["type"].as_str() {
                    Some("text") => preview.push_str(b["text"].as_str().unwrap_or_default()),
                    _ => all_text = false,
                }
            }
            let text = all_text.then_some(preview.clone());
            Some(QueueEntry {
                id,
                placement,
                preview,
                text,
            })
        })
        .collect()
}

/// 队列/插队条目(session/queue 权威快照的 UI 投影;非持久节点)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    /// 条目 id(host 定位变更用)
    pub id: String,
    /// queued = 待运行(next-turn)/ steering = 中途插队(立即投递)
    pub placement: QueuePlacement,
    /// 文本预览(块数组内 text 块拼接;图片块不参与)
    pub preview: String,
    /// 纯文本(可编辑;含非文本块 = 不可编辑)
    pub text: Option<String>,
}

/// 队列条目落位
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePlacement {
    /// 待运行(next-turn)
    Queued,
    /// 中途插队(立即投递)
    Steering,
}

/// 计划归档卡状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    /// 已提交待批
    Pending,
    /// 已批准
    Approved,
    /// 已取消/驳回
    Cancelled,
}

/// 单会话投影状态
#[derive(Debug, Default)]
pub struct ChatState {
    /// 顺序节点流
    pub nodes: Vec<ChatNode>,
    /// todo 列表(todo/write 全量替换)
    pub todos: Vec<TodoItem>,
    /// 本地 running(turn/start~turn/end)
    pub running: bool,
    /// 队列/插队条目(session/queue 权威快照整体替换)
    pub queue: Vec<QueueEntry>,
    /// 计划模式(plan/mode)
    pub plan_mode: bool,
    /// 当前 turn 已产出的 diff/edit 路径(去重保序;turn/end 挂 TurnTail 产物行)
    pub turn_deliverables: Vec<String>,
    /// 节点出生时刻(入场动画年龄门控:超龄不包动画,gpui list 虚拟化
    /// 重挂不重放)。纯 UI 元数据,不参与相等性。
    pub node_born: std::collections::HashMap<String, std::time::Instant>,
    /// 重试行退避截止时刻(直播帧到达时刻 + delayMs;渲染期推算剩余秒,
    /// 1s tick 只触发重绘)。纯 UI 元数据,不参与相等性;重放不记。
    pub retry_deadlines: std::collections::HashMap<String, std::time::Instant>,
    /// 历史合并中(born 不记录:载入的历史行不做入场动画;仅方法内瞬态)
    merging: bool,
}

/// 相等性排除 UI 元数据:同事件序列的两个状态节点/业务字段全同,
/// 但出生时刻(Instant)不同;`merging` 仅 merge_history 执行期内为真。
impl PartialEq for ChatState {
    fn eq(&self, other: &Self) -> bool {
        self.nodes == other.nodes
            && self.todos == other.todos
            && self.running == other.running
            && self.queue == other.queue
            && self.plan_mode == other.plan_mode
            && self.turn_deliverables == other.turn_deliverables
    }
}

impl ChatState {
    /// 历史尾窗折叠(以现有投影为基线;节点 key 幂等,直播帧先行安全)。
    /// 折叠后 running 复位(断连窗口外的中间态不保留)。
    pub fn merge_history(&mut self, events: impl IntoIterator<Item = SessionEvent>) {
        self.merging = true;
        for ev in events {
            self.apply(&ev);
        }
        self.merging = false;
        self.running = false;
        // 清孤儿 born(被折叠掉的头窗节点;防跨会话累积)
        let live: std::collections::HashSet<&str> = self.nodes.iter().map(ChatNode::key).collect();
        self.node_born.retain(|k, _| live.contains(k.as_str()));
        self.retry_deadlines
            .retain(|k, _| live.contains(k.as_str()));
    }

    /// 应用单个客方事件
    pub fn apply(&mut self, ev: &SessionEvent) {
        match ev.ty.as_str() {
            // 新回合清空任务列表(源 todos 投影同语义:turn/start 置空,
            // turn/end 保留完成的清单可见)
            "turn/start" => {
                self.running = true;
                self.todos.clear();
            }
            "turn/end" => {
                self.running = false;
                let kind = ev.data["reason"]["kind"].as_str();
                let aborted = kind == Some("aborted");
                // 取消收尾:等待退避中的重试行翻转「已取消」
                if aborted {
                    for n in self.nodes.iter_mut() {
                        if let ChatNode::Retry { state, .. } = n
                            && *state == RetryState::Waiting
                        {
                            *state = RetryState::Cancelled;
                        }
                    }
                }
                // 错误终止(传输失败/悬挂超时):通告行替代收尾行
                if kind == Some("error") {
                    let msg = ev.data["reason"]["error"]["message"]
                        .as_str()
                        .unwrap_or("未知错误");
                    self.push_node(ChatNode::Notice {
                        key: format!("turn-error:{}", ev.seq),
                        text: format!("回合出错:{msg}"),
                    });
                } else {
                    let meta = self.usage_meta();
                    let deliverables = std::mem::take(&mut self.turn_deliverables);
                    self.push_node(ChatNode::TurnTail {
                        key: format!("turn-end:{}", ev.seq),
                        aborted,
                        meta,
                        deliverables,
                    });
                }
            }
            "user/message" => {
                // 分流(分类权威 source.kind):kind=user → 真实用户
                // 消息;否则注入上下文 → 折叠「注入行」。事件序已由引擎保证
                // 「真实用户先、注入后」,故无需 buffering,按 seq 直接追加。
                if ev.data["source"]["kind"].as_str().unwrap_or("user") == "user" {
                    self.push_node(ChatNode::User {
                        key: format!("user:{}", ev.seq),
                        text: content_text(&ev.data["content"]),
                        images: image_blocks(&ev.data["content"]),
                    });
                } else {
                    self.push_node(ChatNode::Context {
                        key: format!("ctx:{}", ev.seq),
                        content: content_text(&ev.data["content"]),
                        source: ev.data["source"].clone(),
                    });
                }
            }
            "agent/inbox/spliced" => {
                // 队列簿记(durable inbox):入队/编辑/转移的 inserted
                // 是重放素材,不是对话内容——排队条目经 session/queue 帧
                // 呈现,认领后由 user/message 渲染(避免重启后双渲染)
            }
            "assistant/chunk" => {
                let text = ev.data["chunk"]["text"].as_str().unwrap_or_default();
                self.with_streaming_node(&ev.data, |node| {
                    if let ChatNode::Assistant { text: t, .. } = node {
                        t.push_str(text);
                    }
                });
            }
            "assistant/reasoning" => {
                let text = ev.data["text"].as_str().unwrap_or_default();
                self.with_streaming_node(&ev.data, |node| {
                    // 增量是片段(逐 token,engine 逐条落档)——直接拼接;
                    // 换行属于内容本身
                    if let ChatNode::Assistant { reasoning, .. } = node {
                        reasoning.push_str(text);
                    }
                });
            }
            // 流式残段丢弃标记(重试前):清空该步流式缓冲,与引擎的
            // 累积重置同序——重放与实况同一可见语义(引擎重发前直接
            // 丢弃部分 chunks,RS 日志只追加,以标记达成)
            "assistant/stream-reset" => {
                let key = stream_key(&ev.data);
                if let Some(ix) = self.position(&key)
                    && let ChatNode::Assistant {
                        text,
                        reasoning,
                        streaming,
                        ..
                    } = &mut self.nodes[ix]
                {
                    text.clear();
                    reasoning.clear();
                    *streaming = true;
                }
            }
            // LLM 重试:落折叠行(直播记退避截止时刻供倒计时;重放不记)
            "llm/retry" => {
                let key = format!("retry:{}", ev.seq);
                self.push_node(ChatNode::Retry {
                    key,
                    retry: ev.data["retry"].as_u64().unwrap_or(0) as u32,
                    max_retries: ev.data["maxRetries"].as_u64().unwrap_or(0) as u32,
                    delay_ms: ev.data["delayMs"].as_u64().unwrap_or(0),
                    code: ev.data["code"].as_str().unwrap_or_default().to_string(),
                    message: ev.data["message"].as_str().unwrap_or_default().to_string(),
                    state: RetryState::Waiting,
                });
                if !self.merging {
                    let deadline = std::time::Instant::now()
                        + std::time::Duration::from_millis(
                            ev.data["delayMs"].as_u64().unwrap_or(0),
                        );
                    self.retry_deadlines
                        .insert(format!("retry:{}", ev.seq), deadline);
                }
            }
            // 退避结束、重发开始:最后一个等待中的重试行翻转「已重试」
            "llm/retry-started" => {
                if let Some(ChatNode::Retry { state, .. }) = self.nodes.iter_mut().rev().find(|n| {
                    matches!(
                        n,
                        ChatNode::Retry {
                            state: RetryState::Waiting,
                            ..
                        }
                    )
                }) {
                    *state = RetryState::Started;
                }
            }
            "assistant/message" => {
                let key = stream_key(&ev.data);
                let text = content_text(&ev.data["message"]["content"]);
                let usage = ev.data.get("usage").filter(|u| !u.is_null()).cloned();
                let message_id = ev.data["message"]["id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                // 定稿:流式节点替换为终值(无 chunk 时也直接建)
                match self.position(&key) {
                    Some(ix) => {
                        if let ChatNode::Assistant {
                            text: t,
                            streaming,
                            usage: u,
                            message_id: mid,
                            ..
                        } = &mut self.nodes[ix]
                        {
                            if !text.is_empty() {
                                *t = text;
                            }
                            *streaming = false;
                            if usage.is_some() {
                                *u = usage;
                            }
                            if !message_id.is_empty() {
                                *mid = message_id.clone();
                            }
                        }
                    }
                    None => {
                        self.push_node(ChatNode::Assistant {
                            key,
                            text,
                            reasoning: String::new(),
                            streaming: false,
                            usage,
                            message_id,
                        });
                    }
                }
            }
            "tool/call" => {
                let call_id = ev.data["callId"].as_str().unwrap_or_default().to_string();
                let name = ev.data["name"].as_str().unwrap_or_default().to_string();
                let arguments = ev.data["arguments"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                self.push_node(ChatNode::Tool {
                    summary: summarize_call(&name, &arguments),
                    key: format!("call:{call_id}"),
                    view: ev.data.get("view").filter(|v| !v.is_null()).cloned(),
                    name,
                    state: ToolState::Running,
                    arguments,
                    output: None,
                    images: Vec::new(),
                });
            }
            "tool/result" => {
                // message.content[0].toolCallId 配对
                let call_id = ev.data["message"]["content"][0]["toolCallId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let is_error = ev.data["message"]["content"][0]["isError"].as_bool();
                let output = content_text(&ev.data["message"]["content"][0]["content"]);
                if let Some(ix) = self.position(&format!("call:{call_id}"))
                    && let ChatNode::Tool {
                        state,
                        output: o,
                        view,
                        images,
                        ..
                    } = &mut self.nodes[ix]
                {
                    *state = if is_error == Some(true) {
                        ToolState::Error
                    } else {
                        ToolState::Done
                    };
                    *o = Some(output);
                    // result 侧视图权威替换:在场覆盖 call 意图,缺席清除
                    // (如编辑失败无 diff → 通用卡)
                    *view = ev.data.get("view").filter(|v| !v.is_null()).cloned();
                    // 结果图片(MCP 图片桥):引用数组权威替换
                    *images = ev
                        .data
                        .get("images")
                        .and_then(|v| v.as_array().cloned())
                        .unwrap_or_default();
                    // 产物:view 为 diff 卡 → 累积 diffs[].path(去重保序)
                    if let Some(v) = &*view
                        && v["card"].as_str() == Some("diff")
                        && let Some(diffs) = v["diffs"].as_array()
                    {
                        for d in diffs {
                            if let Some(p) = d["path"].as_str()
                                && !self.turn_deliverables.iter().any(|x| x == p)
                            {
                                self.turn_deliverables.push(p.to_string());
                            }
                        }
                    }
                }
            }
            // 计划归档三件:submitted 落归档卡节点;approved/cancelled
            // 更新最后一个待批节点的状态(事件序保证配对)
            "plan/submitted" => {
                let plan = ev.data["plan"].as_str().unwrap_or_default().to_string();
                self.nodes.push(ChatNode::Plan {
                    key: format!("plan:{}", ev.seq),
                    plan,
                    status: PlanStatus::Pending,
                });
            }
            "plan/approved" | "plan/cancelled" => {
                let status = if ev.ty == "plan/approved" {
                    PlanStatus::Approved
                } else {
                    PlanStatus::Cancelled
                };
                // 事件序保证配对:更新最后一个待批计划节点 →
                // 归档卡状态徽标随之变化(拒绝/取消的界面反馈)
                if let Some(ChatNode::Plan { status: s, .. }) =
                    self.nodes.iter_mut().rev().find(|n| {
                        matches!(
                            n,
                            ChatNode::Plan {
                                status: PlanStatus::Pending,
                                ..
                            }
                        )
                    })
                {
                    *s = status;
                }
            }
            "todo/write" => {
                self.todos = ev.data["todos"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .map(|t| TodoItem {
                                content: t["content"].as_str().unwrap_or_default().into(),
                                status: t["status"].as_str().unwrap_or("pending").into(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            }
            "plan/mode" => {
                self.plan_mode = ev.data["active"].as_bool().unwrap_or(false);
            }
            _ => {}
        }
    }

    /// 尾部追加节点(key 去重:同 key 已存在则跳过——历史/直播重放幂等)
    pub(crate) fn push_node(&mut self, node: ChatNode) {
        if self.position(node.key()).is_some() {
            return;
        }
        self.record_born(node.key());
        self.nodes.push(node);
    }

    /// 记录节点出生(仅直播帧;历史合并载入的行不做入场动画)
    fn record_born(&mut self, key: &str) {
        if !self.merging {
            self.node_born
                .entry(key.to_string())
                .or_insert_with(std::time::Instant::now);
        }
    }

    /// 流式节点定位/创建(assistant chunk/reasoning 目标)
    fn with_streaming_node(&mut self, data: &Value, f: impl FnOnce(&mut ChatNode)) {
        let key = stream_key(data);
        let ix = match self.position(&key) {
            Some(ix) => ix,
            None => {
                self.record_born(&key);
                self.nodes.push(ChatNode::Assistant {
                    key: key.clone(),
                    text: String::new(),
                    reasoning: String::new(),
                    streaming: true,
                    usage: None,
                    message_id: String::new(),
                });
                self.nodes.len() - 1
            }
        };
        f(&mut self.nodes[ix]);
    }

    fn position(&self, key: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.key() == key)
    }

    /// 回合用量摘要:自尾部最近带用量的 assistant 节点取
    /// `耗时 x · 首 token xms · x tok/s`
    fn usage_meta(&self) -> Option<String> {
        for n in self.nodes.iter().rev() {
            let ChatNode::Assistant { usage: Some(u), .. } = n else {
                continue;
            };
            let ms = u["durationMs"].as_i64()?;
            let ttft = u["ttftMs"].as_i64().unwrap_or(0);
            let toks = u["completionTokens"].as_u64().unwrap_or(0);
            let mut meta = format!("耗时 {}", fmt_duration(ms));
            if ttft > 0 {
                meta.push_str(&format!(" · 首 token {ttft}ms"));
            }
            if ms > 0 && toks > 0 {
                let tps = (toks as f64 / (ms as f64 / 1000.0)).round() as u64;
                meta.push_str(&format!(" · {tps} tok/s"));
            }
            return Some(meta);
        }
        None
    }
}

/// 零高节点:空正文且无思考的定稿 Assistant(纯 tool_calls 步的
/// assistant/message 占位)——渲染不产出任何可见内容,行槽仍会为它
/// 生成成员行,渲染层据此整行跳过(否则 py(8) 留 16px 空隙,
/// 展开组行间疏密不均)。
pub(crate) fn invisible_node(n: &ChatNode) -> bool {
    matches!(n, ChatNode::Assistant { text, reasoning, streaming, .. }
        if text.is_empty() && reasoning.is_empty() && !streaming)
}

/// 渲染行槽(读取层派生):`ChatNode` 平铺流 → 虚拟化列表行。
/// 已收口轮的过程项默认收成一行组摘要(open_turns 例外展开),直播末段
/// 恒平铺;nodes 本体不动(增量投影/merge 幂等根基)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowSlot {
    /// 平铺节点(携带节点下标;debug selector/element id 用原始序号)
    Node(usize),
    /// 折叠的轮过程组(单行摘要;first..=last = 段内首末成员节点下标,
    /// 含端点,成员间可穿插内容节点)
    Group {
        /// 段收口节点 key(turn-end:<seq> / turn-error:<seq>)
        turn_key: String,
        first: usize,
        last: usize,
    },
    /// 展开的轮过程组头(成员行紧随其后按事件原序平铺)
    GroupOpen {
        turn_key: String,
        first: usize,
        last: usize,
    },
    /// 展开态的组成员行(渲染时缩进 + 左侧引导线,表达「属于上方组」
    /// 的层级;与组外平铺节点视觉区分,防展开后迷失)
    GroupMember(usize),
}

/// 每节点「过程项」标记(纯函数,build_row_slots 与 group_counts 共用
/// 同一口径)。过程项 = Context / Tool / 非最终答复的 Assistant;
/// **段内最后一个 text 非空的 Assistant = 最终答复**(留在外面)——
/// DeepSeek 每步都带 reasoning + 过渡文本,若按「text 空才算过程」
/// 中间叙述会全部漏收(实测 72 步只收走 51 个工具)。
/// **答案豁免只对正常完成(TurnTail 非 aborted)段生效**:error 收尾
/// (Notice)与用户取消/中断(aborted TurnTail)的轮没有最终答复——
/// 唯一/最后的正文其实是中间叙述,须一并收进组(实测「回合出错」前
/// 与「已中断」前残留 Think+正文,真实会话日志重放验证)。
pub(crate) fn process_mask(nodes: &[ChatNode]) -> Vec<bool> {
    let mut mask = vec![false; nodes.len()];
    let mut answer_taken = false;
    for i in (0..nodes.len()).rev() {
        match &nodes[i] {
            // 正常完成:答案豁免生效(重置);取消/错误收尾:无答案(占用)
            ChatNode::TurnTail { aborted, .. } => {
                answer_taken = *aborted;
                continue;
            }
            ChatNode::Notice { .. } => {
                answer_taken = true;
                continue;
            }
            ChatNode::Context { .. } | ChatNode::Tool { .. } | ChatNode::Retry { .. } => {
                mask[i] = true
            }
            ChatNode::Assistant { text, .. } => {
                let is_answer = !text.is_empty() && !answer_taken;
                if is_answer {
                    answer_taken = true;
                }
                mask[i] = !is_answer;
            }
            _ => mask[i] = false,
        }
    }
    mask
}

/// 节点流 → 渲染行槽(纯函数)。「段」= 上一收口节点之后至下一收口
/// (TurnTail / error 的 Notice)之间的全部节点;不依赖 turn 号
/// (Context/Tool 节点不携带),倒扫标定后正向拼装。末尾未收口的段
/// = 直播段,恒平铺——「默认收 + 手动展开例外」由此天然实现
/// turn/end 自动收拢,无需事件 hook。
pub fn build_row_slots(
    nodes: &[ChatNode],
    open_turns: &std::collections::HashSet<String>,
) -> Vec<RowSlot> {
    let mask = process_mask(nodes);
    // 倒扫:每节点所属段的收口 key(收口自身也标自己,但收口非过程项,
    // 恒走 Node 分支)
    let mut close_key: Vec<Option<&str>> = vec![None; nodes.len()];
    let mut cur: Option<&str> = None;
    for (i, n) in nodes.iter().enumerate().rev() {
        if matches!(n, ChatNode::TurnTail { .. } | ChatNode::Notice { .. }) {
            cur = Some(n.key());
        }
        close_key[i] = cur;
    }
    // 段内首个用户消息下标 = 轮次开始(导航锚点同语义)。组区间不得
    // 跨越它:其前的段成员(attach 基线注入等轮前过程项)不属于任何
    // 轮,连同用户消息一起平铺;steering 插话不是段内首个,不受影响
    // (保留「区间内穿插内容节点」的既有语义)。
    let mut seg_first_user: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        if matches!(n, ChatNode::User { .. })
            && let Some(key) = close_key[i]
            && !seg_first_user.contains_key(key)
        {
            seg_first_user.insert(key, i);
        }
    }
    let mut slots: Vec<RowSlot> = Vec::with_capacity(nodes.len());
    let mut i = 0;
    while i < nodes.len() {
        // 非过程项,或直播段(close_key 缺):平铺
        let Some(key) = close_key[i].filter(|_| mask[i]) else {
            slots.push(RowSlot::Node(i));
            i += 1;
            continue;
        };
        // 组起点落在轮次开始之前:轮前成员 + 用户消息平铺,其后照常归组
        if let Some(&u) = seg_first_user.get(key)
            && i < u
        {
            while i <= u {
                slots.push(RowSlot::Node(i));
                i += 1;
            }
            continue;
        }
        // 首个成员起,收集段内首末成员下标(中间可穿插内容节点)
        let first = i;
        let mut last = i;
        let mut j = i;
        while j < nodes.len() && close_key[j] == Some(key) {
            if mask[j] {
                last = j;
            }
            j += 1;
        }
        let open = open_turns.contains(key);
        slots.push(if open {
            RowSlot::GroupOpen {
                turn_key: key.to_string(),
                first,
                last,
            }
        } else {
            RowSlot::Group {
                turn_key: key.to_string(),
                first,
                last,
            }
        });
        // 折叠态:组行后补输出区间内穿插的内容节点(保持原序);
        // 展开态:组头后逐项平铺全部成员(含交错内容节点,原序)——
        // 成员行用 GroupMember 槽(渲染层缩进+引导线,层级可辨)
        for (k, &m) in (first..=last).zip(mask[first..=last].iter()) {
            if open || !m {
                slots.push(if open {
                    RowSlot::GroupMember(k)
                } else {
                    RowSlot::Node(k)
                });
            }
        }
        i = last + 1;
    }
    slots
}

/// 组区间统计:(过程步数, 工具调用数)——组摘要行计数口径,
/// 与 trajectory「N 步 · M 个工具调用」同构
pub fn group_counts(nodes: &[ChatNode], first: usize, last: usize) -> (usize, usize) {
    let mask = process_mask(nodes);
    let span = first..=last;
    let steps = span.clone().filter(|&k| mask.get(k) == Some(&true)).count();
    let tools = nodes
        .get(first..=last)
        .map(|s| {
            s.iter()
                .filter(|n| matches!(n, ChatNode::Tool { .. }))
                .count()
        })
        .unwrap_or(0);
    (steps, tools)
}

// ---- 导航轨锚点(读取层派生)----

/// 锚点类别(轨上点的语义标签)
/// 导航轨锚点:一个用户消息行槽 = 轨上一个点
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavAnchor {
    /// 行槽下标(点击跳转目标;当前位置高亮也按它比较)
    pub slot_ix: usize,
    /// 用户节点稳定 key(user:<seq>)
    pub key: String,
    /// hover 卡标题:消息首行单行化截断(粗体)
    pub title: String,
    /// hover 卡正文预览:次行起单行化 ~240 字(无次行则为空)
    pub preview: String,
}

fn first_and_rest(text: &str) -> (String, String) {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let title = lines.next().unwrap_or_default();
    let rest: Vec<&str> = lines.collect();
    (one_line(title, 48), one_line(&rest.join(" "), 240))
}

/// 行槽 → 导航锚点(纯函数)。**锚点 = 用户消息(轮次开始)**——导航的
/// 语义就是在轮次之间跳转,其余(答复/组行/收尾/通告)一概不是锚点。
pub fn nav_anchors(slots: &[RowSlot], nodes: &[ChatNode]) -> Vec<NavAnchor> {
    let mut out = Vec::new();
    for (slot_ix, slot) in slots.iter().enumerate() {
        let RowSlot::Node(n) = slot else {
            continue;
        };
        let Some(ChatNode::User { key, text, .. }) = nodes.get(*n) else {
            continue;
        };
        let (title, preview) = first_and_rest(text);
        out.push(NavAnchor {
            slot_ix,
            key: key.clone(),
            title,
            preview,
        });
    }
    out
}

/// 毫秒 → 人读时长(与 statsbar 同形)
fn fmt_duration(ms: i64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

/// 流式节点 key:a:<turn>:<step>
fn stream_key(data: &Value) -> String {
    format!("a:{}:{}", data["turn"], data["step"])
}

/// content 块数组 → 拼接 text 块文本
fn content_text(content: &Value) -> String {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"].as_str() == Some("text"))
                .map(|b| b["text"].as_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// 内容里的 image 块(attachment 引用数组;纯字符串 = 空)
fn image_blocks(content: &Value) -> Vec<Value> {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"].as_str() == Some("image"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// 折叠行摘要:工具名感知的显式键序偏好表
/// ——修 file_edit 摘要错显 new_text 的旧病(字母序首串恰好是 new_text)。
/// 未列工具回落「参数首串」;错误态摘要另由渲染层取输出首行
fn summarize_call(name: &str, arguments: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(arguments) else {
        return one_line(arguments, 80);
    };
    let keys: &[&str] = match name {
        // bash 优先 description(必填参数,给用户看的
        // 一句意图说明),回退 command
        "bash" => &["description", "command"],
        "file_read" => &["path"],
        "file_edit" => &["path"],
        "file_search" => &["content", "glob", "path"],
        "subagent" | "ralph" => &["task"],
        "workflow" => &["steps"],
        "goal" | "jobs" => &["action"],
        "exit_plan_mode" => &["plan"],
        _ => {
            return first_string(&v)
                .map(|s| one_line(&s, 80))
                .unwrap_or_else(|| one_line(arguments, 80));
        }
    };
    for key in keys {
        match &v[*key] {
            // workflow.steps:数组首元素(字符串)
            Value::Array(a) if *key == "steps" => {
                if let Some(first) = a.iter().find_map(|s| s.as_str()) {
                    return one_line(first, 80);
                }
            }
            Value::String(s) if !s.is_empty() => return one_line(s, 80),
            _ => {}
        }
    }
    first_string(&v)
        .map(|s| one_line(&s, 80))
        .unwrap_or_default()
}

/// todo_write 行摘要:解析**该次调用自己的 arguments**,
/// 非全局当前态 —— 每行反映本次写入的列表。text 可截断,
/// extra(并行进行中额外数)不可 —— 窄行也不剪掉「还有几条在跑」。
/// 坏 JSON / 非对象根 / todos 非数组 / 元素非对象 → None(回落通用摘要)
pub(crate) struct TodoRowSummary {
    /// `{done}/{total} 已完成`(+ 首个进行中文本)
    pub text: String,
    /// 进行中条目超出首条的数量(0 = 无额外)
    pub extra: usize,
}

pub(crate) fn todo_row_summary(arguments: &str) -> Option<TodoRowSummary> {
    let parsed: Value = serde_json::from_str(arguments).ok()?;
    if !parsed.is_object() {
        return None;
    }
    let todos = parsed["todos"].as_array()?;
    if !todos.iter().all(|t| t.is_object()) {
        return None;
    }
    let done = todos.iter().filter(|t| t["status"] == "completed").count();
    let actives: Vec<&str> = todos
        .iter()
        .filter(|t| t["status"] == "in_progress")
        .filter_map(|t| t["content"].as_str())
        .collect();
    let mut text = format!("{done}/{} 已完成", todos.len());
    // 首个进行中文本可用(非空白)才挂名 + 计额外数
    let mut extra = 0;
    if let Some(first) = actives.first().filter(|c| !c.trim().is_empty()) {
        text.push_str(&format!(" · {}", one_line(first, 60)));
        extra = actives.len() - 1;
    }
    Some(TodoRowSummary { text, extra })
}

/// 单行化 + 字符级截断
fn one_line(s: &str, max: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, max)
}

/// 工作区根前缀剥离(显示面):绝对路径落在根内 → 相对形,其余原样
/// (源 relativizeToCcd 同语义;折叠行摘要与卡横幅 path 共用)
pub(crate) fn relativize(root: Option<&str>, text: &str) -> String {
    let Some(root) = root
        .map(|r| r.trim_end_matches('/'))
        .filter(|r| !r.is_empty())
    else {
        return text.to_string();
    };
    for sep in [format!("{root}/"), format!("{root}\\")] {
        if let Some(rest) = text.strip_prefix(&sep) {
            return rest.to_string();
        }
    }
    text.to_string()
}

fn first_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => a.iter().find_map(first_string),
        Value::Object(o) => o.values().find_map(first_string),
        _ => None,
    }
}

/// 字符级截断(中文安全)
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// session/queue items 解析:queued/steering 分流、preview 拼接、
    /// 非文本块置不可编辑
    #[test]
    fn parse_queue_items_splits_placement_and_preview() {
        let items = serde_json::json!([
            {
                "id": "q1",
                "placement": "queued",
                "message": { "role": "user", "content": [ { "type": "text", "text": "排队的问题" } ] }
            },
            {
                "id": "s1",
                "placement": "steering",
                "message": { "role": "user", "content": [
                    { "type": "text", "text": "插队的" },
                    { "type": "image", "url": "x" }
                ] }
            }
        ]);
        let entries = parse_queue_items(items.as_array().cloned().unwrap());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].placement, QueuePlacement::Queued);
        assert_eq!(entries[0].preview, "排队的问题");
        assert_eq!(entries[0].text.as_deref(), Some("排队的问题"));
        assert_eq!(entries[1].placement, QueuePlacement::Steering);
        assert_eq!(entries[1].preview, "插队的");
        assert_eq!(entries[1].text, None, "含图片块 = 不可编辑");
    }

    use dsh_core::proto::SurfaceOp;
    use serde_json::json;

    fn ev(ty: &str, seq: u64, data: Value) -> SessionEvent {
        SessionEvent {
            ty: ty.into(),
            seq,
            time: 0,
            data,
            source_event_seqs: None,
            surface_op: Some(SurfaceOp::Append),
            ignorable: None,
        }
    }

    /// 全回合样本(对齐 translate.rs 测试序列)
    fn full_turn() -> Vec<SessionEvent> {
        vec![
            ev("turn/start", 1, json!({ "turn": 1 })),
            ev(
                "user/message",
                2,
                json!({
                    "id": "u1", "role": "user",
                    "content": [ { "type": "text", "text": "hi" } ],
                    "source": { "kind": "user" },
                }),
            ),
            ev("step/start", 3, json!({ "turn": 1, "step": 1 })),
            ev(
                "assistant/chunk",
                4,
                json!({ "turn": 1, "step": 1, "chunk": { "type": "text-delta", "index": 0, "text": "he" } }),
            ),
            ev(
                "assistant/chunk",
                5,
                json!({ "turn": 1, "step": 1, "chunk": { "type": "text-delta", "index": 0, "text": "llo" } }),
            ),
            ev(
                "assistant/reasoning",
                6,
                json!({ "turn": 1, "step": 1, "text": "思考中" }),
            ),
            ev(
                "assistant/message",
                7,
                json!({
                    "turn": 1, "step": 1,
                    "message": { "id": "m1", "role": "assistant",
                        "content": [ { "type": "text", "text": "hello" } ] },
                    "usage": { "durationMs": 100, "ttftMs": 50, "completionTokens": 5 },
                }),
            ),
            ev(
                "tool/call",
                8,
                json!({ "turn": 1, "step": 2, "callId": "8", "name": "bash", "arguments": "{\"command\":\"ls -la\"}" }),
            ),
            ev(
                "tool/result",
                9,
                json!({
                    "turn": 1, "step": 2,
                    "message": { "id": "m2", "role": "user", "content": [ {
                        "type": "tool-result", "toolCallId": "8",
                        "content": [ { "type": "text", "text": "file-a" } ],
                        "isError": false } ] },
                }),
            ),
            ev(
                "assistant/message",
                10,
                json!({
                    "turn": 1, "step": 3,
                    "message": { "id": "m3", "role": "assistant",
                        "content": [ { "type": "text", "text": "done" } ] },
                }),
            ),
            ev(
                "turn/end",
                11,
                json!({ "turn": 1, "reason": { "kind": "completed" } }),
            ),
        ]
    }

    #[test]
    fn full_turn_projection() {
        let mut st = ChatState::default();
        st.merge_history(full_turn());
        assert!(!st.running);
        let keys: Vec<&str> = st.nodes.iter().map(|n| n.key()).collect();
        assert_eq!(
            keys,
            vec!["user:2", "a:1:1", "call:8", "a:1:3", "turn-end:11"]
        );
        // 用户节点来自 user/message(seq 2)
        assert!(matches!(&st.nodes[0], ChatNode::User { text, .. } if text == "hi"));
        // 助手流式定稿:chunk 拼接被定稿文本覆盖,usage 附着
        match &st.nodes[1] {
            ChatNode::Assistant {
                text,
                reasoning,
                streaming,
                usage,
                ..
            } => {
                assert_eq!(text, "hello");
                assert_eq!(reasoning, "思考中");
                assert!(!streaming);
                assert_eq!(usage.as_ref().unwrap()["durationMs"], 100);
            }
            other => panic!("expected assistant, got {other:?}"),
        }
        // 工具配对:running → done,输出附着
        match &st.nodes[2] {
            ChatNode::Tool {
                name,
                summary,
                state,
                output,
                ..
            } => {
                assert_eq!(name, "bash");
                assert_eq!(summary, "ls -la");
                assert_eq!(*state, ToolState::Done);
                assert_eq!(output.as_deref(), Some("file-a"));
            }
            other => panic!("expected tool, got {other:?}"),
        }
        // 回合收尾:用量摘要自尾部最近带用量 assistant(a:1:1)聚合
        match &st.nodes[4] {
            ChatNode::TurnTail { aborted, meta, .. } => {
                assert!(!aborted);
                assert_eq!(
                    meta.as_deref(),
                    Some("耗时 100ms · 首 token 50ms · 50 tok/s")
                );
            }
            other => panic!("expected turn tail, got {other:?}"),
        }
    }

    /// 错误终止的回合:Notice 通告替代收尾行,running 复位
    /// (translate 把 durable turn/error 译为 turn/end{kind:"error"})
    #[test]
    fn error_turn_end_projects_notice() {
        let mut st = ChatState::default();
        st.apply(&ev("turn/start", 1, json!({ "turn": 1 })));
        st.apply(&ev("user/message", 2, json!({ "content": "hi" })));
        assert!(st.running);
        st.apply(&ev(
            "turn/end",
            3,
            json!({
                "turn": 1,
                "reason": {
                    "kind": "error",
                    "error": { "code": "TRANSPORT", "message": "读超时" },
                },
            }),
        ));
        assert!(!st.running);
        match st.nodes.last() {
            Some(ChatNode::Notice { text, .. }) => {
                assert!(text.contains("读超时"), "通告应含错误信息: {text}");
            }
            other => panic!("expected notice, got {other:?}"),
        }
        assert!(
            !st.nodes
                .iter()
                .any(|n| matches!(n, ChatNode::TurnTail { .. })),
            "错误终止不应再出收尾行"
        );
    }

    /// 4a:user/message + source.kind≠user 折叠为注入行(渲染紧跟用户气泡)。
    /// 事件序已由引擎保证「真实用户先、注入后」,注入行按 seq 直接追加,
    /// 无需缓冲;注入行由 source.kind 分流识别。
    #[test]
    fn context_message_projects_after_user() {
        let mut st = ChatState::default();
        // 注入行(user/message + source.kind=agent-instructions)
        st.apply(&ev(
            "user/message",
            1,
            json!({
                "id": "ctx-1",
                "role": "user",
                "content": [ { "type": "text", "text": "AGENTS.md 全文" } ],
                "source": { "kind": "agent-instructions", "form": "instructions" },
            }),
        ));
        // 真实用户消息
        st.apply(&ev(
            "user/message",
            2,
            json!({ "content": [ { "type": "text", "text": "hi" } ] }),
        ));
        assert_eq!(st.nodes.len(), 2);
        match &st.nodes[0] {
            ChatNode::Context {
                content, source, ..
            } => {
                assert_eq!(content, "AGENTS.md 全文");
                assert_eq!(source["kind"], "agent-instructions");
            }
            other => panic!("第 0 节点应为注入行,实际 {other:?}"),
        }
        match &st.nodes[1] {
            ChatNode::User { text, .. } => assert_eq!(text, "hi"),
            other => panic!("第 1 节点应为用户消息,实际 {other:?}"),
        }
    }

    /// subagent 结算通知(user/message + source.kind=subagent-settled)
    /// 同走注入行分流,source 原样保留(chat_pane 凭 kind 渲染独立通知卡)
    #[test]
    fn subagent_settled_notice_projects_as_context_row() {
        let mut st = ChatState::default();
        st.apply(&ev(
            "user/message",
            1,
            json!({
                "id": "notice-1",
                "role": "user",
                "content": [ { "type": "text", "text": "Background subagent s-1 finished and will do no further work unless you send it more.\n\nIts closing message:\n\nthe report" } ],
                "source": {
                    "kind": "subagent-settled",
                    "form": "notice",
                    "summary": "Background subagent s-1 finished and will do no further work unless you send it more.",
                    "senderSessionId": "s-1",
                },
            }),
        ));
        assert_eq!(st.nodes.len(), 1);
        match &st.nodes[0] {
            ChatNode::Context {
                content, source, ..
            } => {
                assert!(content.contains("Its closing message:"));
                assert_eq!(source["kind"], "subagent-settled");
                assert_eq!(source["form"], "notice");
                assert_eq!(source["senderSessionId"], "s-1");
            }
            other => panic!("通知应为注入行(通知卡),实际 {other:?}"),
        }
    }

    #[test]
    fn streaming_chunks_accumulate() {
        let mut st = ChatState::default();
        st.apply(&ev(
            "assistant/chunk",
            1,
            json!({ "turn": 1, "step": 1, "chunk": { "text": "ab" } }),
        ));
        st.apply(&ev(
            "assistant/chunk",
            2,
            json!({ "turn": 1, "step": 1, "chunk": { "text": "cd" } }),
        ));
        assert_eq!(st.nodes.len(), 1);
        match &st.nodes[0] {
            ChatNode::Assistant {
                text, streaming, ..
            } => {
                assert_eq!(text, "abcd");
                assert!(streaming);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_error_pairs() {
        let mut st = ChatState::default();
        st.apply(&ev(
            "tool/call",
            1,
            json!({ "turn": 1, "step": 1, "callId": "1", "name": "bash", "arguments": "{}" }),
        ));
        st.apply(&ev("tool/result", 2, json!({
            "message": { "content": [ { "type": "tool-result", "toolCallId": "1", "isError": true,
                "content": [ { "type": "text", "text": "boom" } ] } ] },
        })));
        match &st.nodes[0] {
            ChatNode::Tool { state, output, .. } => {
                assert_eq!(*state, ToolState::Error);
                assert_eq!(output.as_deref(), Some("boom"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn aborted_tail_and_idempotent_push() {
        let mut st = ChatState::default();
        st.apply(&ev("turn/start", 1, json!({ "turn": 1 })));
        assert!(st.running);
        st.apply(&ev(
            "turn/end",
            2,
            json!({ "turn": 1, "reason": { "kind": "aborted", "reason": { "kind": "legacy" } } }),
        ));
        assert!(matches!(
            &st.nodes[0],
            ChatNode::TurnTail { aborted: true, .. }
        ));
        // 重放同 key 幂等
        st.apply(&ev(
            "turn/end",
            2,
            json!({ "turn": 1, "reason": { "kind": "aborted" } }),
        ));
        assert_eq!(st.nodes.len(), 1);
    }

    /// todos 的 turn 生命周期:turn/start 清空、
    /// turn/end 保留 —— 回合结束清单仍可见,新回合开始才重置
    #[test]
    fn todos_cleared_on_turn_start_kept_on_turn_end() {
        let mut st = ChatState::default();
        st.apply(&ev(
            "todo/write",
            1,
            json!({ "todos": [ { "content": "a", "status": "pending" } ] }),
        ));
        st.apply(&ev(
            "turn/end",
            2,
            json!({ "turn": 1, "reason": { "kind": "done" } }),
        ));
        assert_eq!(st.todos.len(), 1, "turn/end 保留清单");
        st.apply(&ev("turn/start", 3, json!({ "turn": 2 })));
        assert!(st.todos.is_empty(), "turn/start 清空清单");
    }

    #[test]
    fn spliced_inserted_and_todo_plan() {
        let mut st = ChatState::default();
        // splice 的 inserted 是队列簿记,不渲染为对话节点
        // (排队条目经 session/queue 帧呈现,认领后 user/message 渲染)
        st.apply(&ev(
            "agent/inbox/spliced",
            1,
            json!({
                "target": "next-turn", "start": 0, "removedCount": 0,
                "inserted": [ { "id": "q1", "content": "排队消息" } ],
            }),
        ));
        assert!(st.nodes.is_empty(), "splice inserted 不产生聊天节点");
        st.apply(&ev(
            "todo/write",
            2,
            json!({ "todos": [
            { "content": "a", "status": "completed" },
            { "content": "b", "status": "pending" },
        ] }),
        ));
        assert_eq!(st.todos.len(), 2);
        assert_eq!(st.todos[0].status, "completed");
        st.apply(&ev("plan/mode", 3, json!({ "active": true })));
        assert!(st.plan_mode);
    }

    #[test]
    fn summary_truncates_cjk() {
        let long = "很".repeat(100);
        assert_eq!(truncate_chars(&long, 80).chars().count(), 81); // 80 + 省略号
        assert_eq!(summarize_call("bash", "{\"command\":\"ls\"}"), "ls");
    }

    /// 折叠行摘要键序:file_edit 显 path(修 new_text 错显)、
    /// file_search content 优先、workflow 取 steps 首元素;
    /// bash 优先 description(必填意图说明),
    /// 旧日志无 description 回退 command
    #[test]
    fn summary_key_order_per_tool() {
        assert_eq!(
            summarize_call(
                "bash",
                "{\"command\":\"git status\",\"description\":\"Show working tree status\"}"
            ),
            "Show working tree status"
        );
        assert_eq!(
            summarize_call("bash", "{\"command\":\"git status\"}"),
            "git status"
        );
        assert_eq!(
            summarize_call(
                "file_edit",
                "{\"new_text\":\"fn main() {}\",\"old_text\":\"…\",\"path\":\"src/main.rs\"}"
            ),
            "src/main.rs"
        );
        assert_eq!(
            summarize_call(
                "file_search",
                "{\"content\":\"needle\",\"glob\":\"**/*.rs\"}"
            ),
            "needle"
        );
        assert_eq!(
            summarize_call("file_search", "{\"glob\":\"**/*.rs\",\"path\":\"/w\"}"),
            "**/*.rs"
        );
        assert_eq!(
            summarize_call("workflow", "{\"steps\":[\"第一步 规划\",\"第二步 执行\"]}"),
            "第一步 规划"
        );
        assert_eq!(
            summarize_call("subagent", "{\"task\":\"调查布局\"}"),
            "调查布局"
        );
        assert_eq!(
            summarize_call("goal", "{\"action\":\"add\",\"task\":\"t\"}"),
            "add"
        );
        // todo_write 无通用键序:回落参数首串(行专属摘要另由 todo_row_summary)
        assert_eq!(
            summarize_call(
                "todo_write",
                "{\"todos\":[{\"content\":\"c\",\"status\":\"pending\"}]}"
            ),
            "c"
        );
        // 未列工具:回落参数首串
        assert_eq!(summarize_call("whatever", "{\"x\":\"首串\"}"), "首串");
    }

    /// 工具配对 + 渲染意图投影:call 侧意图在场、result 侧权威替换、
    /// 失败清除(call 意图不残留)
    #[test]
    fn tool_pair_carries_view() {
        let mut st = ChatState::default();
        st.apply(&ev(
            "tool/call",
            1,
            json!({ "turn": 1, "step": 1, "callId": "7", "name": "bash",
                    "arguments": "{\"command\":\"ls -la\"}" }),
        ));
        st.apply(&ev(
            "tool/result",
            2,
            json!({
                "turn": 1, "step": 1,
                "message": { "content": [ {
                    "type": "tool-result", "toolCallId": "7",
                    "content": [ { "type": "text", "text": "file-a" } ],
                    "isError": false,
                } ] },
                "view": { "card": "terminal", "exitCode": 0, "signal": null, "cwd": "/w/proj" },
            }),
        ));
        match &st.nodes[0] {
            ChatNode::Tool {
                state,
                output,
                view,
                ..
            } => {
                assert!(matches!(state, ToolState::Done));
                assert_eq!(output.as_deref(), Some("file-a"));
                let v = view.as_ref().expect("result 侧视图在场");
                assert_eq!(v["card"], "terminal");
                assert_eq!(v["exitCode"], 0);
                assert_eq!(v["cwd"], "/w/proj");
            }
            other => panic!("工具节点缺失:{other:?}"),
        }

        // call 侧意图(file_edit 运行中)+ 失败清除
        st.apply(&ev(
            "tool/call",
            3,
            json!({ "turn": 1, "step": 2, "callId": "9", "name": "file_edit",
                    "arguments": "{\"path\":\"a.txt\",\"old_text\":\"o\",\"new_text\":\"n\"}",
                    "view": { "card": "diff", "diffs": [ { "path": "a.txt",
                        "oldText": "o", "newText": "n" } ] } }),
        ));
        match &st.nodes[1] {
            ChatNode::Tool { view, .. } => {
                assert_eq!(view.as_ref().unwrap()["card"], "diff", "call 侧意图在场");
            }
            other => panic!("{other:?}"),
        }
        st.apply(&ev(
            "tool/result",
            4,
            json!({
                "turn": 1, "step": 2,
                "message": { "content": [ {
                    "type": "tool-result", "toolCallId": "9",
                    "content": [ { "type": "text", "text": "old_text not found" } ],
                    "isError": true,
                } ] },
            }),
        ));
        match &st.nodes[1] {
            ChatNode::Tool { state, view, .. } => {
                assert!(matches!(state, ToolState::Error));
                assert!(view.is_none(), "失败无视图 → 通用卡");
            }
            other => panic!("{other:?}"),
        }
    }

    /// 多行 bash 命令的折叠摘要恒单行(空白序列折一空格)
    #[test]
    fn summary_collapses_multiline_command() {
        let script = "for f in $(ls *.rs); do\n  rustfmt --edition 2024 \"$f\" && \\\n    echo ok \"$f\"\ndone";
        let summary = summarize_call(
            "bash",
            &format!("{{\"command\":{}}}", serde_json::json!(script)),
        );
        assert!(!summary.contains('\n'), "摘要应单行:{summary}");
        assert!(
            summary.starts_with("for f in $(ls *.rs); do rustfmt"),
            "空白应折一空格:{summary}"
        );
    }

    /// todo_write 行摘要:解析该次调用 args(计数 + 首个进行中 + 并行额外数;
    /// 坏 JSON / 非数组 / 非对象项 → None)
    #[test]
    fn todo_row_summary_from_call_args() {
        let s = todo_row_summary(
            r#"{"todos": [
            {"content": "任务一", "status": "completed"},
            {"content": "任务二", "status": "in_progress"},
            {"content": "任务三", "status": "in_progress"}
        ]}"#,
        )
        .unwrap();
        assert_eq!(s.text, "1/3 已完成 · 任务二");
        assert_eq!(s.extra, 1, "并行额外数独立于可截断文本");

        // 无进行中:仅计数
        let s = todo_row_summary(
            r#"{"todos": [{"content": "a", "status": "completed"}, {"content": "b", "status": "pending"}]}"#,
        )
        .unwrap();
        assert_eq!(s.text, "1/2 已完成");
        assert_eq!(s.extra, 0);

        // 首个进行中内容空白:仅计数,不挂名不计数额外
        let s =
            todo_row_summary(r#"{"todos": [{"content": " ", "status": "in_progress"}]}"#).unwrap();
        assert_eq!(s.text, "0/1 已完成");
        assert_eq!(s.extra, 0);

        // 坏输入 → None(回落通用摘要)
        assert!(todo_row_summary("not json").is_none());
        assert!(todo_row_summary(r#"{"action": "add"}"#).is_none());
        assert!(todo_row_summary(r#"{"todos": [42]}"#).is_none());
    }

    /// 工作区根相对化:根内绝对路径剥前缀,其余原样
    #[test]
    fn relativize_strips_workspace_root() {
        assert_eq!(
            relativize(Some("/w/proj"), "/w/proj/src/main.rs"),
            "src/main.rs"
        );
        assert_eq!(relativize(Some("/w/proj/"), "/w/proj/a.txt"), "a.txt");
        assert_eq!(relativize(Some("/w/proj"), "/other/a.txt"), "/other/a.txt");
        assert_eq!(relativize(None, "/w/proj/a.txt"), "/w/proj/a.txt");
        assert_eq!(relativize(Some("/w/proj"), "相对路径.rs"), "相对路径.rs");
    }

    /// node_born(入场动画年龄门控素材):直播帧记录;历史合并载入不记
    /// (载入行不做入场动画);合并收尾清孤儿键(防跨会话累积)。
    #[test]
    fn node_born_live_only_and_orphan_prune() {
        let mut st = ChatState::default();
        st.apply(&ev(
            "user/message",
            2,
            json!({
                "id": "u1", "role": "user",
                "content": [ { "type": "text", "text": "hi" } ],
                "source": { "kind": "user" },
            }),
        ));
        st.apply(&ev(
            "assistant/chunk",
            3,
            json!({ "turn": 1, "step": 1, "chunk": { "type": "text-delta", "index": 0, "text": "he" } }),
        ));
        assert!(st.node_born.contains_key("user:2"), "直播 user 应记 born");
        assert!(st.node_born.contains_key("a:1:1"), "流式首建应记 born");

        // 纯历史载入:全程 merging → 不记
        let mut hist = ChatState::default();
        hist.merge_history(full_turn());
        assert!(hist.node_born.is_empty(), "历史载入不应记 born");

        // 折叠合并到已有直播态:既有 born 保留,历史新增节点不记
        st.merge_history(full_turn());
        assert!(st.node_born.contains_key("user:2"));
        assert!(!st.node_born.contains_key("call:8"), "历史补入不记 born");
        assert!(!st.node_born.contains_key("turn-end:11"));

        // 孤儿清理:折叠掉(或手工塞入)的键不残留
        st.node_born
            .insert("call:ghost".into(), std::time::Instant::now());
        st.merge_history(vec![]);
        assert!(!st.node_born.contains_key("call:ghost"));
    }

    // ---- 行槽派生层(build_row_slots)----

    use std::collections::HashSet as SlotSet;

    /// 简写断言:槽序列 → 可读形
    fn slot_shapes(slots: &[RowSlot]) -> Vec<String> {
        slots
            .iter()
            .map(|s| match s {
                RowSlot::Node(n) | RowSlot::GroupMember(n) => format!("n{n}"),
                RowSlot::Group {
                    turn_key,
                    first,
                    last,
                } => {
                    format!("g[{first}..={last}]{turn_key}")
                }
                RowSlot::GroupOpen {
                    turn_key,
                    first,
                    last,
                } => {
                    format!("G[{first}..={last}]{turn_key}")
                }
            })
            .collect()
    }

    /// 事件序列 → 投影节点
    fn project(events: Vec<SessionEvent>) -> Vec<ChatNode> {
        let mut st = ChatState::default();
        for e in &events {
            st.apply(e);
        }
        st.nodes
    }

    /// 基本收拢:think-only + 工具收进组行;正文与收尾行在外;
    /// 直播段(无收口)平铺;open_turns 展开 = 组头 + 成员全平铺
    #[test]
    fn row_slots_collapse_open_and_live() {
        let mut evs = vec![
            ev("turn/start", 1, json!({ "turn": 1 })),
            ev(
                "user/message",
                2,
                json!({ "content": [ { "type": "text", "text": "hi" } ] }),
            ),
        ];
        evs.push(ev(
            "assistant/message",
            3,
            json!({
                "turn": 1, "step": 1,
                "message": { "id": "m1", "role": "assistant",
                    "content": [ { "type": "text", "text": "" } ] },
            }),
        ));
        evs.push(ev(
            "assistant/reasoning",
            4,
            json!({ "turn": 1, "step": 1, "text": "思考中" }),
        ));
        evs.push(ev(
            "tool/call",
            5,
            json!({ "turn": 1, "step": 2, "callId": "8", "name": "bash", "arguments": "{}" }),
        ));
        evs.push(ev(
            "turn/end",
            6,
            json!({ "turn": 1, "reason": { "kind": "done" } }),
        ));
        let nodes = project(evs);
        // 节点序:user:2, a:1:1(空正文+reasoning → 过程项), call:8, turn-end:6
        assert_eq!(nodes.len(), 4);
        let mask = process_mask(&nodes);
        assert!(mask[1] && mask[2]);

        // open_turns 空 = 全收:正文前平铺,过程项收成组,收尾行在外
        let none = SlotSet::new();
        assert_eq!(
            slot_shapes(&build_row_slots(&nodes, &none)),
            vec!["n0", "g[1..=2]turn-end:6", "n3"]
        );
        // 计数:2 步 · 1 工具
        assert_eq!(group_counts(&nodes, 1, 2), (2, 1));

        // 展开:组头 + 成员逐项平铺
        let mut open = SlotSet::new();
        open.insert("turn-end:6".to_string());
        assert_eq!(
            slot_shapes(&build_row_slots(&nodes, &open)),
            vec!["n0", "G[1..=2]turn-end:6", "n1", "n2", "n3"]
        );

        // 直播段(截掉收尾事件):恒平铺
        let live = &nodes[..3];
        assert_eq!(
            slot_shapes(&build_row_slots(live, &none)),
            vec!["n0", "n1", "n2"]
        );
    }

    /// 零高节点口径:空正文+无思考的定稿 Assistant(纯 tool_calls 步
    /// 占位)= 不可见;有正文/有思考/流式中都可见
    #[test]
    fn invisible_node_only_for_empty_finalized_assistant() {
        let mk = |text: &str, reasoning: &str, streaming: bool| ChatNode::Assistant {
            key: "a:1:1".into(),
            text: text.into(),
            reasoning: reasoning.into(),
            streaming,
            usage: None,
            message_id: String::new(),
        };
        assert!(invisible_node(&mk("", "", false)));
        assert!(!invisible_node(&mk("先看一下", "", false)));
        assert!(!invisible_node(&mk("", "思考中", false)));
        assert!(!invisible_node(&mk("", "", true)));
        assert!(!invisible_node(&ChatNode::User {
            key: "user:1".into(),
            text: "hi".into(),
            images: vec![],
        }));
    }

    /// attach 基线注入先于轮次开始:注入行 + 用户消息平铺(用户消息
    /// 不被组区间收掉),组只收用户消息之后的过程项。真实会话形态:
    /// AGENTS.md 基线在 attach 时落档,首条用户消息晚于它——组区间
    /// 跨越用户消息会把轮次开始折叠进「思考与工具」(实测回归)。
    #[test]
    fn row_slots_baseline_injection_before_first_user_stays_flat() {
        let evs = vec![
            // attach 基线注入(无 turn,先于首条用户消息)
            ev(
                "user/message",
                1,
                json!({
                    "content": [ { "type": "text", "text": "AGENTS.md 基线" } ],
                    "source": { "kind": "agent-instructions" },
                }),
            ),
            ev("turn/start", 2, json!({ "turn": 1 })),
            ev("user/message", 3, json!({ "content": "规划接下来的开发" })),
            // 轮内注入(runtime context,过程项)
            ev(
                "user/message",
                4,
                json!({
                    "content": [ { "type": "text", "text": "runtime context" } ],
                    "source": { "kind": "plugin" },
                }),
            ),
            ev(
                "tool/call",
                5,
                json!({ "turn": 1, "step": 1, "callId": "a", "name": "bash", "arguments": "{}" }),
            ),
            // 最终答复
            ev(
                "assistant/message",
                6,
                json!({
                    "turn": 1, "step": 2,
                    "message": { "id": "m1", "role": "assistant",
                        "content": [ { "type": "text", "text": "计划如下" } ] },
                }),
            ),
            ev(
                "turn/end",
                7,
                json!({ "turn": 1, "reason": { "kind": "done" } }),
            ),
        ];
        let nodes = project(evs);
        // 0 基线注入, 1 用户消息, 2 轮内注入, 3 call, 4 答复, 5 turn-end
        assert_eq!(nodes.len(), 6);
        let none = SlotSet::new();
        assert_eq!(
            slot_shapes(&build_row_slots(&nodes, &none)),
            vec!["n0", "n1", "g[2..=3]turn-end:7", "n4", "n5"]
        );
        // 展开态:组成员按原序平铺,基线注入与用户消息保持组外
        let mut open = SlotSet::new();
        open.insert("turn-end:7".to_string());
        assert_eq!(
            slot_shapes(&build_row_slots(&nodes, &open)),
            vec!["n0", "n1", "G[2..=3]turn-end:7", "n2", "n3", "n4", "n5"]
        );
    }

    /// 整轮一组:正文交错不打断组;折叠态组行后的穿插正文保持原序;
    /// error 通告与 TurnTail 同为收口
    #[test]
    fn row_slots_whole_turn_group_with_interleaved_text() {
        let mut evs = vec![
            ev("turn/start", 1, json!({ "turn": 1 })),
            ev("user/message", 2, json!({ "content": "hi" })),
            // 注入行(过程项)
            ev(
                "user/message",
                3,
                json!({
                    "content": [ { "type": "text", "text": "AGENTS.md" } ],
                    "source": { "kind": "agent-instructions" },
                }),
            ),
            // 中途正文(内容节点)
            ev(
                "assistant/message",
                4,
                json!({
                    "turn": 1, "step": 1,
                    "message": { "id": "m1", "role": "assistant",
                        "content": [ { "type": "text", "text": "先看一下" } ] },
                }),
            ),
            ev(
                "tool/call",
                5,
                json!({ "turn": 1, "step": 2, "callId": "a", "name": "bash", "arguments": "{}" }),
            ),
            // think-only(过程项)
            ev(
                "assistant/reasoning",
                6,
                json!({ "turn": 1, "step": 3, "text": "继续" }),
            ),
            ev(
                "assistant/message",
                7,
                json!({
                    "turn": 1, "step": 3,
                    "message": { "id": "m2", "role": "assistant",
                        "content": [ { "type": "text", "text": "" } ] },
                }),
            ),
            ev(
                "tool/call",
                8,
                json!({ "turn": 1, "step": 4, "callId": "b", "name": "goal", "arguments": "{}" }),
            ),
            // 错误收口:Notice 替代收尾行
            ev(
                "turn/end",
                9,
                json!({ "turn": 1, "reason": { "kind": "error", "error": { "message": "x" } } }),
            ),
        ];
        let nodes = project(std::mem::take(&mut evs));
        // 0 user, 1 ctx, 2 正文, 3 call:a, 4 think-only, 5 call:b, 6 notice
        let none = SlotSet::new();
        // error 收口段无最终答复:唯一正文也是中间叙述,一并收进组
        assert_eq!(
            slot_shapes(&build_row_slots(&nodes, &none)),
            vec!["n0", "g[1..=5]turn-error:9", "n6"]
        );
        // 计数:5 过程步(ctx/正文/2 工具/think) · 2 工具
        assert_eq!(group_counts(&nodes, 1, 5), (5, 2));

        // 展开后交错正文位于原序位(组头 → 成员按事件序,正文夹在其间)
        let mut open = SlotSet::new();
        open.insert("turn-error:9".to_string());
        assert_eq!(
            slot_shapes(&build_row_slots(&nodes, &open)),
            vec![
                "n0",
                "G[1..=5]turn-error:9",
                "n1",
                "n2",
                "n3",
                "n4",
                "n5",
                "n6"
            ]
        );
    }

    /// 取消收尾(aborted TurnTail)与错误收口同待遇:轮无最终答复,
    /// 段内最后正文也是中间叙述,一并收进组(真实会话日志重放复现:
    /// 软取消「已中断」轮的 a:N:31 残留在组外)。
    #[test]
    fn row_slots_aborted_turn_folds_last_narration() {
        let mut evs = vec![
            ev("turn/start", 1, json!({ "turn": 1 })),
            ev("user/message", 2, json!({ "content": "接下来做什么" })),
        ];
        // 两个叙述步(reasoning + chunk 正文)
        for (seq, step, reasoning, text) in [
            (3, 1, "先看状态", "Let me understand the state."),
            (7, 2, "重新应用改动", "Let me re-apply the zip change."),
        ] {
            evs.push(ev(
                "assistant/reasoning",
                seq,
                json!({ "turn": 1, "step": step, "text": reasoning }),
            ));
            evs.push(ev(
                "assistant/chunk",
                seq + 1,
                json!({ "turn": 1, "step": step, "delta": text }),
            ));
            evs.push(ev(
                "tool/call",
                seq + 2,
                json!({ "turn": 1, "step": step, "callId": format!("{step}"), "name": "bash", "arguments": "{}" }),
            ));
        }
        // 软取消收尾(cancelled → aborted TurnTail)
        evs.push(ev(
            "turn/end",
            10,
            json!({ "turn": 1, "cancelled": "token" }),
        ));
        let nodes = project(evs);
        // 0 user, 1 叙①, 2 tool1, 3 叙②(最后正文,旧判定误豁免), 4 tool2, 5 tail(aborted)
        let none = SlotSet::new();
        assert_eq!(
            slot_shapes(&build_row_slots(&nodes, &none)),
            vec!["n0", "g[1..=4]turn-end:10", "n5"]
        );
        assert_eq!(group_counts(&nodes, 1, 4), (4, 2));
    }

    /// DeepSeek 真实形态:每步 reasoning+过渡文本。段内**最后一个**正文
    /// = 最终答复(留在外面),中间叙述连同思考一并收进组;
    /// 收口重置答案标记(答案判定不跨段)
    #[test]
    fn row_slots_interim_narration_folds_final_answer_stays() {
        let evs = vec![
            ev("turn/start", 1, json!({ "turn": 1 })),
            ev("user/message", 2, json!({ "content": "hi" })),
            // 中间叙述步①(思考+文本)
            ev(
                "assistant/reasoning",
                3,
                json!({ "turn": 1, "step": 1, "text": "先看状态" }),
            ),
            ev(
                "assistant/message",
                4,
                json!({
                    "turn": 1, "step": 1,
                    "message": { "id": "m1", "role": "assistant",
                        "content": [ { "type": "text", "text": "Let me check the state." } ] },
                }),
            ),
            ev(
                "tool/call",
                5,
                json!({ "turn": 1, "step": 2, "callId": "a", "name": "bash", "arguments": "{}" }),
            ),
            // 中间叙述步②
            ev(
                "assistant/reasoning",
                6,
                json!({ "turn": 1, "step": 3, "text": "跑测试" }),
            ),
            ev(
                "assistant/message",
                7,
                json!({
                    "turn": 1, "step": 3,
                    "message": { "id": "m2", "role": "assistant",
                        "content": [ { "type": "text", "text": "Run the tests now." } ] },
                }),
            ),
            // 最终答复(段内最后正文)
            ev(
                "assistant/message",
                8,
                json!({
                    "turn": 1, "step": 4,
                    "message": { "id": "m3", "role": "assistant",
                        "content": [ { "type": "text", "text": "全部通过,结论如下。" } ] },
                }),
            ),
            ev(
                "turn/end",
                9,
                json!({ "turn": 1, "reason": { "kind": "done" } }),
            ),
        ];
        let nodes = project(evs);
        // 0 user, 1 叙①(reasoning+text), 2 tool, 3 叙②, 4 答案, 5 tail
        let none = SlotSet::new();
        // 叙①②皆过程项收进组;最终答复与收尾在外
        assert_eq!(
            slot_shapes(&build_row_slots(&nodes, &none)),
            vec!["n0", "g[1..=3]turn-end:9", "n4", "n5"]
        );
        assert_eq!(group_counts(&nodes, 1, 3), (3, 1));
        // 展开态:中间叙述按原序平铺在组头之后、答复之前
        let mut open = SlotSet::new();
        open.insert("turn-end:9".to_string());
        assert_eq!(
            slot_shapes(&build_row_slots(&nodes, &open)),
            vec!["n0", "G[1..=3]turn-end:9", "n1", "n2", "n3", "n4", "n5"]
        );
    }

    /// LLM 重试行投影:llm/retry 落等待行(直播记退避截止),started
    /// 翻转「已重试」,取消收尾翻转「已取消」;重放不记截止;过程项
    /// 归组(收口后随轮折叠)
    #[test]
    fn retry_row_state_evolution() {
        let mut st = ChatState::default();
        st.apply(&ev("turn/start", 1, json!({ "turn": 1 })));
        st.apply(&ev(
            "llm/retry",
            2,
            json!({
                "turn": 1, "step": 1, "retry": 1, "maxRetries": 5,
                "delayMs": 1500, "code": "TRANSPORT", "message": "连接失败",
            }),
        ));
        assert!(st.nodes.last().is_some_and(|n| matches!(
            n,
            ChatNode::Retry {
                state: RetryState::Waiting,
                retry: 1,
                max_retries: 5,
                delay_ms: 1500,
                ..
            }
        )));
        assert!(
            st.retry_deadlines.contains_key("retry:2"),
            "直播帧应记退避截止时刻"
        );
        // started:最后一个等待行翻转
        st.apply(&ev(
            "llm/retry-started",
            3,
            json!({ "turn": 1, "step": 1, "retry": 1 }),
        ));
        assert!(st.nodes.iter().all(|n| matches!(
            n,
            ChatNode::Retry {
                state: RetryState::Started,
                ..
            }
        )));

        // 等待中被取消 → 「已取消」
        let mut st2 = ChatState::default();
        st2.apply(&ev(
            "llm/retry",
            1,
            json!({ "retry": 1, "maxRetries": 5, "delayMs": 8000 }),
        ));
        st2.apply(&ev(
            "turn/end",
            2,
            json!({ "turn": 1, "reason": { "kind": "aborted", "reason": { "kind": "legacy" } } }),
        ));
        assert!(
            st2.nodes.iter().any(|n| matches!(
                n,
                ChatNode::Retry {
                    state: RetryState::Cancelled,
                    ..
                }
            )),
            "取消后等待行应翻转「已取消」"
        );

        // 重放路径:merge 不记截止时刻(倒计时为静态排定值)
        let mut hist = ChatState::default();
        hist.merge_history(vec![ev(
            "llm/retry",
            1,
            json!({ "retry": 1, "maxRetries": 5, "delayMs": 8000 }),
        )]);
        assert!(hist.retry_deadlines.is_empty(), "历史载入不记倒计时截止");
    }

    /// stream-reset 清空该步流式缓冲(重试丢弃语义):已拼 chunk 清空、
    /// 保持流式态,后续 chunk 从零续拼
    #[test]
    fn stream_reset_clears_streaming_buffer() {
        let mut st = ChatState::default();
        for text in ["残", "段"] {
            st.apply(&ev(
                "assistant/chunk",
                text.len() as u64,
                json!({ "turn": 1, "step": 1, "chunk": { "type": "text-delta", "index": 0, "text": text } }),
            ));
        }
        st.apply(&ev(
            "assistant/stream-reset",
            9,
            json!({ "turn": 1, "step": 1 }),
        ));
        match &st.nodes[0] {
            ChatNode::Assistant {
                text,
                reasoning: _,
                streaming,
                ..
            } => {
                assert!(text.is_empty(), "残段应被清空: {text}");
                assert!(*streaming, "清空后仍为流式态");
            }
            other => panic!("{other:?}"),
        }
        st.apply(&ev(
            "assistant/chunk",
            10,
            json!({ "turn": 1, "step": 1, "chunk": { "type": "text-delta", "index": 0, "text": "成功" } }),
        ));
        match &st.nodes[0] {
            ChatNode::Assistant { text, .. } => assert_eq!(text, "成功"),
            other => panic!("{other:?}"),
        }
    }

    /// 重试行 = 过程项:收口后随轮折叠进组(与 Think/工具同待遇)
    #[test]
    fn retry_row_folds_into_turn_group() {
        let evs = vec![
            ev("turn/start", 1, json!({ "turn": 1 })),
            ev("user/message", 2, json!({ "content": "hi" })),
            ev(
                "llm/retry",
                3,
                json!({
                    "turn": 1, "step": 1, "retry": 1, "maxRetries": 5,
                    "delayMs": 500, "code": "TIMEOUT", "message": "读超时",
                }),
            ),
            ev(
                "assistant/message",
                4,
                json!({
                    "turn": 1, "step": 1,
                    "message": { "id": "m1", "role": "assistant",
                        "content": [ { "type": "text", "text": "答案" } ] },
                }),
            ),
            ev(
                "turn/end",
                5,
                json!({ "turn": 1, "reason": { "kind": "completed" } }),
            ),
        ];
        let mut st = ChatState::default();
        for e in &evs {
            st.apply(e);
        }
        let nodes = st.nodes.clone();
        let mask = process_mask(&nodes);
        assert!(mask[1], "重试行应为过程项");
        let none = SlotSet::new();
        // 0 user, 1 retry(过程), 2 答案, 3 tail → 组 [1..=1]
        assert_eq!(
            slot_shapes(&build_row_slots(&nodes, &none)),
            vec!["n0", "g[1..=1]turn-end:5", "n2", "n3"]
        );
    }

    /// 导航锚点:**只有用户消息(轮次开始)**是锚点;答复/组行/收尾/
    /// 通告一概不是(导航 = 轮次间跳转)
    #[test]
    fn nav_anchors_pick_user_turn_starts() {
        let mut evs = vec![
            ev("turn/start", 1, json!({ "turn": 1 })),
            ev(
                "user/message",
                2,
                json!({ "content": [ { "type": "text", "text": "第一问" } ] }),
            ),
            ev(
                "assistant/reasoning",
                3,
                json!({ "turn": 1, "step": 1, "text": "想想" }),
            ),
            ev(
                "tool/call",
                4,
                json!({ "turn": 1, "step": 1, "callId": "1", "name": "bash", "arguments": "{}" }),
            ),
            ev("turn/end", 5, json!({ "turn": 1, "cancelled": "token" })),
            ev("turn/start", 6, json!({ "turn": 2 })),
            ev(
                "user/message",
                7,
                json!({ "content": [ { "type": "text", "text": "第二问" } ] }),
            ),
            ev(
                "assistant/reasoning",
                8,
                json!({ "turn": 2, "step": 1, "text": "再想" }),
            ),
            ev(
                "assistant/chunk",
                9,
                // 客方形态:翻译层把引擎 delta 包进 chunk.text(勿用原始流字段)
                json!({ "turn": 2, "step": 1, "chunk": { "type": "text-delta", "index": 0, "text": "中间叙述" } }),
            ),
            ev(
                "tool/call",
                10,
                json!({ "turn": 2, "step": 2, "callId": "2", "name": "bash", "arguments": "{}" }),
            ),
            ev(
                "assistant/chunk",
                11,
                json!({ "turn": 2, "step": 3, "chunk": { "type": "text-delta", "index": 0, "text": "最终答复内容" } }),
            ),
            ev(
                "turn/end",
                12,
                json!({ "turn": 2, "reason": { "kind": "done" } }),
            ),
        ];
        let nodes = project(std::mem::take(&mut evs));
        let none = SlotSet::new();
        let slots = build_row_slots(&nodes, &none);
        let anchors = nav_anchors(&slots, &nodes);
        // 两轮 → 恰好两个用户消息锚点;key 为用户节点稳定 key
        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[0].slot_ix, 0);
        assert_eq!(anchors[0].key, "user:2");
        // 单行消息:标题 = 全文,预览为空(卡只出标题行)
        assert_eq!(anchors[0].title, "第一问");
        assert_eq!(anchors[0].preview, "");
        assert_eq!(anchors[1].slot_ix, 3);
        assert_eq!(anchors[1].key, "user:7");
        assert_eq!(anchors[1].title, "第二问");
        // 轮2 展开:成员行不占点,用户锚点仍在(槽位按当时行槽计)
        let mut open = SlotSet::new();
        open.insert("turn-end:12".to_string());
        let anchors_open = nav_anchors(&build_row_slots(&nodes, &open), &nodes);
        assert_eq!(anchors_open.len(), 2, "展开不减用户锚点");
    }

    /// hover 卡两级拆分:标题 = 首行单行化截 48,预览 = 次行起拼接截
    /// 240;空行跳过,单行消息预览为空(截断带省略号)
    #[test]
    fn nav_anchor_title_preview_split() {
        let (t, p) = first_and_rest("第一行标题\n\n第二行正文\n第三行正文");
        assert_eq!(t, "第一行标题");
        assert_eq!(p, "第二行正文 第三行正文");
        let (t, p) = first_and_rest("只有一行");
        assert_eq!(t, "只有一行");
        assert_eq!(p, "");
        let (t, _) = first_and_rest(&"长".repeat(60));
        assert_eq!(t.chars().count(), 49, "超长首行截 48 + 省略号");
        let (_, p) = first_and_rest(&format!("标题\n{}", "正".repeat(300)));
        assert_eq!(p.chars().count(), 241, "预览截 240 + 省略号");
    }
}
