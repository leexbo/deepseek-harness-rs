//! ServerRequest 帧 → 状态变更(纯函数;语义对齐 web `useAppStore`
//! 双 WS 订阅分支)。副作用以 [`Effect`] 返回,由 store 执行——
//! 单测只喂帧断言状态与效果,不碰 GPUI。

use std::collections::HashMap;

use dsh_core::proto::{
    DescribeValue, HostSessionStatus, ProjectionFrame, Question, QuestionRequestedFrame,
    ServerRequest, SessionEventFrame, SessionSummary,
};

use crate::features::chat::ChatState;

/// 待审计划(question/requested → UI 审批卡 → host.respond)
#[derive(Debug, Clone, PartialEq)]
pub struct PendingPlan {
    /// 应答回显键(本帧 rpcId)
    pub rpc_id: String,
    /// 所属会话
    pub session_id: String,
    /// 问题(含 detail = 计划正文)
    pub question: Question,
}

/// 通用问答(ask_user_question;question/requested 无 intent → UI 问答卡)
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAsk {
    /// 应答回显键(本帧 rpcId)
    pub rpc_id: String,
    /// 所属会话
    pub session_id: String,
    /// 完整问题集(UI pager 分页,整批提交)
    pub questions: Vec<Question>,
}

/// 帧应用后请求的副作用(由 store 执行:重拉清单/宿主信息/统计)
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// 重拉会话清单(session-added/removed、turn 边沿)
    Sessions,
    /// 重拉宿主信息(工作区变更)
    HostInfo,
    /// 拉会话统计(running 边沿;丢帧自愈的安全网)
    Stats(String),
    /// 统计落表(session/stats 推送;载荷 = 宿主聚合 JSON)
    StatsUpsert(String, serde_json::Value),
}

/// 多会话状态(useAppStore 的可纯化子集)
pub struct StoreState {
    /// 会话清单(list_sessions 全量)
    pub sessions: Vec<SessionSummary>,
    /// 会话 → 标题(投影/重命名;清单投影之上)
    pub titles: HashMap<String, String>,
    /// 当前会话
    pub current_id: Option<String>,
    /// 会话 → 执行中(host/session-status 维护;清单自带 running 仅在刷新时可见)
    pub running_by_id: HashMap<String, bool>,
    /// 会话 → 本 turn 起始墙钟(运行时长显示的记时起点;rising edge 记录)
    pub running_since_by_id: HashMap<String, std::time::Instant>,
    /// 宿主信息(describe 组装)
    pub host_info: DescribeValue,
    /// 选中工作区(hero 态新建目标;会话态由 current_id 前缀推导)
    pub active_workspace: Option<String>,
    /// 会话 → 消息流投影(session/event 与 history 折叠)
    pub chats: HashMap<String, ChatState>,
    /// 会话 → 后台子代理清单(session/jobs 帧整体替换,job 含
    /// id/kind/label/status/detail/startedAt/finishedAt/prompt)
    pub jobs_by_id: HashMap<String, Vec<serde_json::Value>>,
    /// 待审计划(question/requested;resolved 清空)
    pub pending_plan: Option<PendingPlan>,
    /// 通用问答(ask_user_question;question/requested 无 intent;resolved 清空)
    pub pending_ask: Option<PendingAsk>,
}

/// 应用一帧,返回待执行副作用。
///
/// 未列出的帧类型(`session/queue`、`session/subscribed`)落 `_` 兜底静默忽略。
pub fn apply_frame(state: &mut StoreState, frame: ServerRequest) -> Vec<Effect> {
    match frame.method.as_str() {
        "host/session-added" | "host/session-removed" => vec![Effect::Sessions],
        "host/session-status" => {
            let Ok(p) = serde_json::from_value::<HostSessionStatus>(frame.payload) else {
                return vec![];
            };
            let was = state.running_by_id.insert(p.session_id.clone(), p.running);
            let mut effects = vec![Effect::Stats(p.session_id.clone())];
            if p.running && !was.unwrap_or(false) {
                // 上升沿:记 turn 起始墙钟(运行时长显示)+ 刷清单
                // (首条消息的标题摘录随 turn 开始可见)
                state
                    .running_since_by_id
                    .insert(p.session_id.clone(), std::time::Instant::now());
                effects.push(Effect::Sessions);
            }
            if !p.running {
                // 对齐 web:turn 结束 → 清单终值刷新;清记时(时长已结算进 turn_tail)
                state.running_since_by_id.remove(&p.session_id);
                effects.push(Effect::Sessions);
            }
            effects
        }
        // 统计推送(事件驱动:宿主在 turn/start、step/start、audit/call
        // 落档点增量聚合后推;替代轮询)
        "session/stats" => {
            let Some(id) = frame.payload["sessionId"].as_str() else {
                return vec![];
            };
            let stats = frame.payload["stats"].clone();
            if stats.is_null() {
                return vec![];
            }
            vec![Effect::StatsUpsert(id.to_string(), stats)]
        }
        // 后台子代理清单(整体替换;host 注册表状态变化即广播)
        "session/jobs" => {
            let Some(id) = frame.payload["sessionId"].as_str() else {
                return vec![];
            };
            let jobs = frame.payload["jobs"].as_array().cloned().unwrap_or_default();
            state.jobs_by_id.insert(id.to_string(), jobs);
            vec![]
        }
        "host/workspace-changed" => vec![Effect::HostInfo, Effect::Sessions],
        // 队列/插队权威快照(整体替换;host 每次变更广播)。条目仅含文本
        // 块时可编辑(与源 QueuedMessage.text 一致)
        "session/queue" => {
            let Some(id) = frame.payload["sessionId"].as_str() else {
                return vec![];
            };
            let entries = crate::features::chat::parse_queue_items(
                frame.payload["items"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            );
            state.chats.entry(id.to_string()).or_default().queue = entries;
            vec![]
        }
        "session/event" => {
            let Ok(f) = serde_json::from_value::<SessionEventFrame>(frame.payload) else {
                return vec![];
            };
            state.chats.entry(f.session_id).or_default().apply(&f.event);
            vec![]
        }
        // 投影增量(4b:LLM 标题生成成功后推送;全量兜底在 history/list)
        "session/projection" => {
            let Ok(f) = serde_json::from_value::<ProjectionFrame>(frame.payload) else {
                return vec![];
            };
            if f.key == "title"
                && let Some(t) = f.value.as_str().filter(|t| !t.is_empty())
            {
                state.titles.insert(f.session_id, t.to_string());
            }
            vec![]
        }
        "question/requested" => {
            let Ok(f) = serde_json::from_value::<QuestionRequestedFrame>(frame.payload.clone())
            else {
                eprintln!(
                    "[q] question/requested 解析失败: {}",
                    &frame.payload.to_string()[..frame.payload.to_string().len().min(300)]
                );
                return vec![];
            };
            eprintln!(
                "[q] question/requested 就位 session={} questions={}",
                f.session_id,
                f.questions.len()
            );
            if f.questions.iter().any(|q| q.intent.is_some()) {
                // plan-review:保留既有 plan 审批通道
                if let Some(q) = f.questions.into_iter().next() {
                    state.pending_plan = Some(PendingPlan {
                        rpc_id: frame.rpc_id,
                        session_id: f.session_id,
                        question: q,
                    });
                }
            } else {
                // 通用 ask_user_question(完整问题集;UI pager 分页)
                state.pending_ask = Some(PendingAsk {
                    rpc_id: frame.rpc_id,
                    session_id: f.session_id,
                    questions: f.questions,
                });
            }
            vec![]
        }
        "question/resolved" => {
            state.pending_plan = None;
            state.pending_ask = None;
            vec![]
        }
        _ => vec![],
    }
}

/// 运行时长格式(秒取整、分钟位两位零填充)。
/// 例:`0秒`、`15秒`、`2分05秒`。
pub fn format_run_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}秒")
    } else {
        let (minutes, seconds) = (secs / 60, secs % 60);
        format!("{minutes}分{seconds:02}秒")
    }
}

/// 相对时间(侧栏行;now 注入以便测试)
pub fn relative_time(now_ms: u64, ts_ms: u64) -> String {
    let d = now_ms.saturating_sub(ts_ms);
    if d < 60_000 {
        "刚刚".into()
    } else if d < 3_600_000 {
        format!("{} 分钟前", d / 60_000)
    } else if d < 86_400_000 {
        format!("{} 小时前", d / 3_600_000)
    } else if d < 7 * 86_400_000 {
        format!("{} 天前", d / 86_400_000)
    } else {
        "更早".into()
    }
}

/// 会话工作区键:带前缀取前缀,否则默认工作区名(= basename(cwd),
/// 由调用方随 describe 填入 workspaces[0])
pub fn workspace_of<'a>(session_id: &'a str, default_ws: &'a str) -> &'a str {
    match session_id.split_once('/') {
        Some((ws, _)) => ws,
        None => default_ws,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_run_duration_seconds_and_minutes() {
        assert_eq!(
            format_run_duration(std::time::Duration::from_secs(0)),
            "0秒"
        );
        assert_eq!(
            format_run_duration(std::time::Duration::from_secs(15)),
            "15秒"
        );
        assert_eq!(
            format_run_duration(std::time::Duration::from_secs(59)),
            "59秒"
        );
        assert_eq!(
            format_run_duration(std::time::Duration::from_secs(60)),
            "1分00秒"
        );
        assert_eq!(
            format_run_duration(std::time::Duration::from_secs(125)),
            "2分05秒"
        );
    }

    /// session/jobs 帧整体替换 jobs_by_id(运行中清单实时可达,
    /// 不等会话清单刷新)
    #[test]
    fn jobs_frame_replaces_jobs_by_id() {
        let mut st = state();
        st.current_id = Some("s-p".into());
        let f = frame(
            "session/jobs",
            serde_json::json!({
                "sessionId": "s-p",
                "jobs": [
                    { "id": "s-c1", "kind": "subagent", "label": "统计 TODO",
                      "status": "running", "startedAt": 1_000,
                      "prompt": "请统计仓库 TODO" },
                    { "id": "s-c2", "kind": "subagent", "label": "旧任务",
                      "status": "completed", "detail": "可继续",
                      "startedAt": 1_000, "finishedAt": 9_000 },
                ],
            }),
        );
        let effects = apply_frame(&mut st, f);
        assert!(effects.is_empty());
        let jobs = &st.jobs_by_id["s-p"];
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0]["status"], "running");
        assert_eq!(jobs[1]["detail"], "可继续");

        // 第二帧整体替换(清单缩为 0 = 全部离场)
        let f = frame(
            "session/jobs",
            serde_json::json!({ "sessionId": "s-p", "jobs": [] }),
        );
        apply_frame(&mut st, f);
        assert!(st.jobs_by_id["s-p"].is_empty());
    }

    fn state() -> StoreState {
        StoreState {
            sessions: vec![],
            titles: HashMap::new(),
            current_id: None,
            running_by_id: HashMap::new(),
            running_since_by_id: HashMap::new(),
            jobs_by_id: HashMap::new(),
            host_info: DescribeValue {
                version: "0".into(),
                cwd: "/w".into(),
                provider: None,
                model: None,
                models: vec![],
                efforts: vec![],
                permissions: vec![],
                presets: vec![],
                workspaces: vec!["w".into()],
                attached_sessions: 0,
                can_open_path: false,
            },
            active_workspace: None,
            chats: HashMap::new(),
            pending_plan: None,
            pending_ask: None,
        }
    }

    fn frame(method: &str, payload: serde_json::Value) -> ServerRequest {
        ServerRequest {
            r#type: "server-request".into(),
            rpc_id: "t".into(),
            method: method.into(),
            payload,
        }
    }

    #[test]
    fn session_added_removed_refresh_list() {
        let mut st = state();
        assert_eq!(
            apply_frame(&mut st, frame("host/session-added", json!({}))),
            vec![Effect::Sessions]
        );
        assert_eq!(
            apply_frame(&mut st, frame("host/session-removed", json!({}))),
            vec![Effect::Sessions]
        );
    }

    #[test]
    fn status_edges_drive_stats_and_list() {
        let mut st = state();
        // 起跑:running=true → 统计安全网 + 清单(标题摘录随 turn 开始)
        let eff = apply_frame(
            &mut st,
            frame(
                "host/session-status",
                json!({ "sessionId": "s1", "running": true }),
            ),
        );
        assert_eq!(eff, vec![Effect::Stats("s1".into()), Effect::Sessions,]);
        assert_eq!(st.running_by_id.get("s1"), Some(&true));
        // 重复 running=true:边沿已过,仅统计安全网
        let eff = apply_frame(
            &mut st,
            frame(
                "host/session-status",
                json!({ "sessionId": "s1", "running": true }),
            ),
        );
        assert_eq!(eff, vec![Effect::Stats("s1".into())]);
        // 收尾:running=false → 统计安全网 + 清单
        let eff = apply_frame(
            &mut st,
            frame(
                "host/session-status",
                json!({ "sessionId": "s1", "running": false }),
            ),
        );
        assert_eq!(eff, vec![Effect::Stats("s1".into()), Effect::Sessions,]);
        assert_eq!(st.running_by_id.get("s1"), Some(&false));
    }

    /// session/stats 推送 → StatsUpsert(事件驱动实时统计)
    #[test]
    fn stats_push_maps_to_upsert() {
        let mut st = state();
        let eff = apply_frame(
            &mut st,
            frame(
                "session/stats",
                json!({ "sessionId": "s1", "stats": { "turns": 2, "contextUsed": 5000 } }),
            ),
        );
        assert_eq!(
            eff,
            vec![Effect::StatsUpsert(
                "s1".into(),
                json!({ "turns": 2, "contextUsed": 5000 })
            )]
        );
        // 形状残缺:静默忽略
        assert!(apply_frame(&mut st, frame("session/stats", json!({}))).is_empty());
    }

    #[test]
    fn workspace_changed_refreshes_both() {
        let mut st = state();
        assert_eq!(
            apply_frame(&mut st, frame("host/workspace-changed", json!({}))),
            vec![Effect::HostInfo, Effect::Sessions]
        );
    }

    #[test]
    fn session_event_feeds_chat_projection() {
        let mut st = state();
        let eff = apply_frame(
            &mut st,
            frame(
                "session/event",
                json!({
                    "sessionId": "s1",
                    "event": { "type": "user/message", "seq": 2, "time": 0,
                        "data": { "id": "u1", "content": [ { "type": "text", "text": "hi" } ] },
                        "surfaceOp": "append" },
                }),
            ),
        );
        assert_eq!(eff, vec![]);
        let chat = st.chats.get("s1").expect("投影已建");
        assert_eq!(chat.nodes.len(), 1);
    }

    #[test]
    fn projection_title_updates_state_titles() {
        let mut st = state();
        assert!(
            apply_frame(
                &mut st,
                frame(
                    "session/projection",
                    json!({ "sessionId": "s1", "key": "title", "value": "Justfile", "seq": 0 }),
                ),
            )
            .is_empty()
        );
        assert_eq!(st.titles.get("s1").map(String::as_str), Some("Justfile"));
        // 空值投影:不覆盖(清单摘录兜底)
        assert!(
            apply_frame(
                &mut st,
                frame(
                    "session/projection",
                    json!({ "sessionId": "s1", "key": "title", "value": "", "seq": 0 }),
                ),
            )
            .is_empty()
        );
        assert_eq!(st.titles.get("s1").map(String::as_str), Some("Justfile"));
        // 非 title 键:不影响标题
        assert!(
            apply_frame(
                &mut st,
                frame(
                    "session/projection",
                    json!({ "sessionId": "s1", "key": "other", "value": "x", "seq": 0 }),
                ),
            )
            .is_empty()
        );
        assert_eq!(st.titles.get("s1").map(String::as_str), Some("Justfile"));
    }

    #[test]
    fn question_lifecycle_sets_and_clears_plan() {
        let mut st = state();
        apply_frame(
            &mut st,
            frame(
                "question/requested",
                json!({
                    "sessionId": "s1",
                    "questions": [ { "id": "plan", "question": "批准该计划?",
                        "header": "计划待批准", "detail": "# 计划",
                        "options": [ { "label": "批准" } ],
                        "intent": { "kind": "plan-review", "approve": "批准" } } ],
                }),
            ),
        );
        let plan = st.pending_plan.as_ref().expect("待审计划已设");
        assert_eq!(plan.session_id, "s1");
        assert_eq!(plan.question.detail.as_deref(), Some("# 计划"));
        // resolved 清空
        apply_frame(
            &mut st,
            frame("question/resolved", json!({ "questionRpcId": "t" })),
        );
        assert!(st.pending_plan.is_none());
    }

    /// 真实 ask_user_question 载荷(线上会话日志原样)→ pending_ask
    /// 就位(此前线上「不弹窗」,用真实形状锁解析)
    #[test]
    fn question_requested_real_payload_sets_pending_ask() {
        let mut st = state();
        let payload: serde_json::Value = serde_json::from_str(
            r#"{"sessionId":"s-3cb1","questions":[{"id":"switch_mode","header":"切换语义","question":"「TTS/ASR/LLM 都切换为阿里百炼」的含义是？","options":[{"label":"默认走百炼，保留本地后端可切回 (Recommended)","description":"backend 枚举新增 DashScope 并设为缺省真实后端"},{"label":"彻底替换，移除本地后端","description":"移除 sherpa/IndexTTS"}],"multi_select":false}]}"#,
        )
        .unwrap();
        let eff = apply_frame(
            &mut st,
            ServerRequest {
                r#type: "server-request".into(),
                rpc_id: "rpc-1".into(),
                method: "question/requested".into(),
                payload,
            },
        );
        assert!(eff.is_empty());
        let ask = st.pending_ask.as_ref().expect("pending_ask 应就位");
        assert_eq!(ask.session_id, "s-3cb1");
        assert_eq!(ask.questions.len(), 1);
        assert_eq!(ask.questions[0].id, "switch_mode");
        assert_eq!(ask.questions[0].options.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn unknown_frames_ignored() {
        let mut st = state();
        for method in [
            "session/subscribed",
            "question/requested", // 空 payload 解析失败 → 静默忽略
            "host/whatever",
        ] {
            assert_eq!(apply_frame(&mut st, frame(method, json!({}))), vec![]);
        }
    }

    #[test]
    fn relative_time_buckets() {
        let now = 10_000_000_000u64;
        assert_eq!(relative_time(now, now), "刚刚");
        assert_eq!(relative_time(now, now - 5 * 60_000), "5 分钟前");
        assert_eq!(relative_time(now, now - 3 * 3_600_000), "3 小时前");
        assert_eq!(relative_time(now, now - 2 * 86_400_000), "2 天前");
        assert_eq!(relative_time(now, now - 30 * 86_400_000), "更早");
    }

    #[test]
    fn workspace_key_split() {
        assert_eq!(workspace_of("proj/s-1", "w"), "proj");
        assert_eq!(workspace_of("s-1", "w"), "w");
    }
}
