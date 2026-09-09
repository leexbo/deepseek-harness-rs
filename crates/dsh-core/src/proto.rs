//! 客方线上协议类型。
//!
//! 四象限信封:`client-request`(上行请求)/ `server-response`(响应)/
//! `server-request`(下行推送,method = 帧类型)/ `client-response`
//! (交互应答)。业务结果 [`RpcResult`]:`{ok:true,value}` 或
//! `{ok:false,error:{code,message,details?}}`——业务错误不抛异常,
//! 恒以错误应答(进程内调用面沿用)。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// ── 四象限信封 ───────────────────────────────────────────────────────

/// 上行请求(方法调用载荷;原 HTTP 载波为 `POST /api/<method>` body)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientRequest {
    /// 判别:恒 `client-request`
    pub r#type: String,
    /// 客户端铸造的不透明回显令牌(uuid)
    #[serde(rename = "rpcId")]
    pub rpc_id: String,
    /// 点分方法名(须等于路径段)
    pub method: String,
    /// 请求载荷
    #[serde(default)]
    pub payload: Value,
}

/// 下行响应(方法调用应答)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerResponse {
    /// 判别:恒 `server-response`
    pub r#type: String,
    /// 回显请求 rpcId
    #[serde(rename = "rpcId")]
    pub rpc_id: String,
    /// 业务结果(错误亦经此字段,不抛异常)
    pub result: RpcResult,
}

/// 下行推送帧(method = 帧类型)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerRequest {
    /// 判别:恒 `server-request`
    pub r#type: String,
    /// 本帧 rpcId(交互类帧的应答回显键;每连接代重放复用)
    #[serde(rename = "rpcId")]
    pub rpc_id: String,
    /// 帧类型(如 `session/event`)
    pub method: String,
    /// 帧载荷
    #[serde(default)]
    pub payload: Value,
}

/// 交互应答(rpcId 回显所答 server-request)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientResponse {
    /// 判别:恒 `client-response`
    pub r#type: String,
    /// 所答 server-request 的 rpcId
    #[serde(rename = "rpcId")]
    pub rpc_id: String,
    /// 应答结果
    pub result: RpcResult,
}

/// 交互应答回执(respond 路由)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RespondReceipt {
    /// 是否受理
    pub accepted: bool,
    /// 拒因(not-pending / bad-response)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ── 业务结果(手写 ser/de:`ok` 判别的两种形态) ─────────────────────

/// 业务错误体(约 45 个判别码,本仓按需使用其中少数)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// 判别码(bad-request / session-not-found / internal / …)
    pub code: String,
    /// 人读信息
    pub message: String,
    /// 附加上下文
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

impl RpcError {
    /// `internal` 错误(not implemented 桩也用它——客方按业务失败处理)
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "internal".into(),
            message: message.into(),
            details: Value::Null,
        }
    }

    /// `bad-request` 错误
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            code: "bad-request".into(),
            message: message.into(),
            details: Value::Null,
        }
    }

    /// `session-not-found` 错误
    pub fn session_not_found(session_id: &str) -> Self {
        Self {
            code: "session-not-found".into(),
            message: format!("session not found: {session_id}"),
            details: Value::Null,
        }
    }
}

/// 业务结果:成功(带 value)或失败(带 error)
#[derive(Debug, Clone, PartialEq)]
pub enum RpcResult {
    /// 成功;value 为 Null 时序列化省略与否由客方 zod 容忍,统一带 null
    Ok(Value),
    /// 失败
    Err(RpcError),
}

impl RpcResult {
    /// 成功快捷构造
    pub fn ok(value: Value) -> Self {
        Self::Ok(value)
    }
}

impl Serialize for RpcResult {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            RpcResult::Ok(value) => json!({ "ok": true, "value": value }).serialize(serializer),
            RpcResult::Err(e) => json!({ "ok": false, "error": e }).serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for RpcResult {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        match raw.get("ok").and_then(Value::as_bool) {
            Some(true) => Ok(RpcResult::Ok(
                raw.get("value").cloned().unwrap_or(Value::Null),
            )),
            Some(false) => {
                let error = raw
                    .get("error")
                    .cloned()
                    .ok_or_else(|| serde::de::Error::custom("error result missing error body"))?;
                Ok(RpcResult::Err(serde_json::from_value(error).map_err(
                    |e| serde::de::Error::custom(format!("bad error body: {e}")),
                )?))
            }
            _ => Err(serde::de::Error::custom("result missing ok discriminant")),
        }
    }
}

// ── 客方会话事件信封(camelCase) ─────────────────────────────────────

/// surface 操作:surface 三类事件必带 `append`,否则客方 transcript 丢弃
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceOp {
    /// 追加
    Append,
    /// 替换区间
    Replace {
        /// 区间起点 seq
        start: u64,
        /// 区间终点 seq
        end: u64,
    },
}

impl Serialize for SurfaceOp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            SurfaceOp::Append => "append".serialize(serializer),
            SurfaceOp::Replace { start, end } => {
                json!({ "op": "replace", "start": start, "end": end }).serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for SurfaceOp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        match &raw {
            Value::String(s) if s == "append" => Ok(SurfaceOp::Append),
            Value::Object(_) => {
                let op = raw["op"].as_str().unwrap_or_default();
                if op != "replace" {
                    return Err(serde::de::Error::custom(format!(
                        "unknown surface op: {op}"
                    )));
                }
                Ok(SurfaceOp::Replace {
                    start: raw["start"]
                        .as_u64()
                        .ok_or_else(|| serde::de::Error::custom("replace op missing start"))?,
                    end: raw["end"]
                        .as_u64()
                        .ok_or_else(|| serde::de::Error::custom("replace op missing end"))?,
                })
            }
            _ => Err(serde::de::Error::custom(
                "surface op must be string or object",
            )),
        }
    }
}

/// 客方 SessionEvent 信封(与我们的 EventEnvelope 字段同名异形:
/// camelCase + surface 三类必须带 surfaceOp/sourceEventSeqs)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// 事件类型
    #[serde(rename = "type")]
    pub ty: String,
    /// 连续序号(会话内单调)
    pub seq: u64,
    /// 毫秒 Unix 时间戳
    pub time: i64,
    /// 载荷(完整 Message 块结构,形状由类型判别)
    pub data: Value,
    /// surface 事件的引用链
    #[serde(
        rename = "sourceEventSeqs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub source_event_seqs: Option<Vec<u64>>,
    /// surface 操作(三类必带 append)
    #[serde(rename = "surfaceOp", default, skip_serializing_if = "Option::is_none")]
    pub surface_op: Option<SurfaceOp>,
    /// 未知类型守卫(客方读取方对未登记类型要求 ignorable)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignorable: Option<bool>,
}

// ── 数据面响应值 ─────────────────────────────────────────────────────

/// host.describe 响应值
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DescribeValue {
    /// 宿主版本
    pub version: String,
    /// 宿主工作目录
    pub cwd: String,
    /// provider 标识(可选)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 模型标识(可选)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 可选模型清单(模型选择菜单)
    pub models: Vec<String>,
    /// 推理等级清单(low / high / max)
    pub efforts: Vec<String>,
    /// 权限预设清单名(read-only / workspace-write / full-access)
    pub permissions: Vec<String>,
    /// preset 清单(名称 + 描述;预设选择菜单)
    pub presets: Vec<Value>,
    /// 工作区名清单(默认在前)
    pub workspaces: Vec<String>,
    /// 附着会话数
    pub attached_sessions: u64,
    /// 是否支持 openPath(本宿主不支持)
    pub can_open_path: bool,
}

/// 会话摘要(session.list 行)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    /// 会话标识(= workspace 顶层 JSONL 文件名 stem)
    pub session_id: String,
    /// 末次更新(ms)
    pub updated_at: u64,
    /// 是否执行中
    pub running: bool,
    /// 是否空会话(无 turn/start)
    pub blank: bool,
    /// 父会话(子代理会话;本宿主暂无)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// 来源标记
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// 工作目录
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// preset 标识
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_preset: Option<String>,
    /// 投影基线(title 等)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projections: Option<Projections>,
}

/// 投影块(高 seq 胜;asOfSeq = -1 表示空日志)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Projections {
    /// 投影所截至的 seq(-1 = 空日志)
    pub as_of_seq: i64,
    /// 投影键值(title 等)
    pub values: Value,
}

/// 历史条目(事件 + 可选视图意图)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// 客方事件
    pub event: SessionEvent,
    /// 宿主计算的视图(工具卡意图;暂不产出)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<Value>,
}

/// session.history 响应值(分页尾拉)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryValue {
    /// 本页事件(seq 升序)
    pub events: Vec<HistoryEntry>,
    /// 是否还有更早页
    pub has_more: bool,
    /// 投影基线(仅尾页带)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projections: Option<Projections>,
}

/// 工作区视图(workspace.list 行;本宿主 = 单工作区 = dsh workspace)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceView {
    /// 工作区标识
    pub workspace_id: String,
    /// 规范路径
    pub path: String,
    /// 显示名
    pub title: String,
    /// 会话清单(手动序)
    pub session_ids: Vec<String>,
    /// 创建时刻(ISO-8601)
    pub created_at: String,
    /// 末次变更时刻(ISO-8601)
    pub updated_at: String,
}

/// workspace.list 响应值
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceListValue {
    /// 工作区清单(本宿主恒一条)
    pub items: Vec<WorkspaceView>,
    /// 归档会话
    pub archived_session_ids: Vec<String>,
}

// ── 下行帧载荷(本宿主产出的帧类型;未列出的帧客方按设计忽略) ─────

/// `session/subscribed` 载荷:mux 流代开基线(lastSeq = 已发最高 seq)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribedFrame {
    /// 会话标识
    pub session_id: String,
    /// 已下发最高 seq(= 高水位)
    pub last_seq: u64,
}

/// `session/event` 载荷
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEventFrame {
    /// 会话标识
    pub session_id: String,
    /// 客方事件
    pub event: SessionEvent,
    /// 视图意图(暂不产出)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<Value>,
}

/// `session/projection` 载荷
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionFrame {
    /// 会话标识
    pub session_id: String,
    /// 投影键(title)
    pub key: String,
    /// 投影值
    pub value: Value,
    /// 截至 seq
    pub seq: u64,
}

/// 问题选项
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    /// 选项标签(应答回传所选标签)
    pub label: String,
    /// 选项说明
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// 问题(计划审批 = 单问题 + plan-review intent)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    /// 问题标识(应答回传)
    pub id: String,
    /// 问题文本
    pub question: String,
    /// 标题
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// 详情(计划正文 markdown)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// 选项
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<QuestionOption>>,
    /// 多选
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multi_select: Option<bool>,
    /// 意图(plan-review)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<Value>,
}

/// `question/requested` 载荷
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRequestedFrame {
    /// 会话标识
    pub session_id: String,
    /// 问题(≥1)
    pub questions: Vec<Question>,
}

/// `question/resolved` 载荷
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionResolvedFrame {
    /// 会话标识
    pub session_id: String,
    /// 所答问题的 rpcId
    pub question_rpc_id: String,
    /// 结局(answered / cancelled)
    pub outcome: String,
}

/// `host/session-added` 载荷
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSessionAdded {
    /// 会话标识
    pub session_id: String,
    /// 是否空会话
    pub blank: bool,
    /// 父会话
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// 来源
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// 工作目录
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// preset
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_preset: Option<String>,
}

/// `host/session-status` 载荷
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSessionStatus {
    /// 会话标识
    pub session_id: String,
    /// 是否执行中
    pub running: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 四象限信封 + RpcResult 的序列化往返与线上形状锁定
    #[test]
    fn envelope_shapes_roundtrip() {
        let req: ClientRequest = serde_json::from_value(json!({
            "type": "client-request", "rpcId": "r1", "method": "session.prompt",
            "payload": { "sessionId": "s", "mode": "queue", "content": [] }
        }))
        .unwrap();
        assert_eq!(req.method, "session.prompt");
        assert_eq!(serde_json::to_value(&req).unwrap()["rpcId"], "r1");

        let ok = ServerResponse {
            r#type: "server-response".into(),
            rpc_id: "r1".into(),
            result: RpcResult::Ok(json!({ "accepted": true })),
        };
        let v = serde_json::to_value(&ok).unwrap();
        assert_eq!(v["result"]["ok"], true);
        assert_eq!(v["result"]["value"]["accepted"], true);
        let back: ServerResponse = serde_json::from_value(v).unwrap();
        assert_eq!(back, ok);

        let err = ServerResponse {
            r#type: "server-response".into(),
            rpc_id: "r2".into(),
            result: RpcResult::Err(RpcError {
                code: "session-not-found".into(),
                message: "nope".into(),
                details: Value::Null,
            }),
        };
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["result"]["ok"], false);
        assert_eq!(v["result"]["error"]["code"], "session-not-found");
        // details 为 Null 时不序列化
        assert!(v["result"]["error"].get("details").is_none());
        let back: ServerResponse = serde_json::from_value(v).unwrap();
        assert_eq!(back, err);
    }

    #[test]
    fn server_request_and_respond() {
        let push = ServerRequest {
            r#type: "server-request".into(),
            rpc_id: "q1".into(),
            method: "session/event".into(),
            payload: json!({ "sessionId": "s", "event": { "type": "turn/start", "seq": 1 } }),
        };
        let v = serde_json::to_value(&push).unwrap();
        assert_eq!(v["rpcId"], "q1");
        assert_eq!(v["method"], "session/event");
        assert_eq!(serde_json::from_value::<ServerRequest>(v).unwrap(), push);

        let answer = ClientResponse {
            r#type: "client-response".into(),
            rpc_id: "q1".into(),
            result: RpcResult::Ok(json!({ "sessionId": "s", "answer": {} })),
        };
        let v = serde_json::to_value(&answer).unwrap();
        assert_eq!(v["rpcId"], "q1");
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn session_event_surface_op_shapes() {
        let mut ev = SessionEvent {
            ty: "user/message".into(),
            seq: 3,
            time: 1,
            data: json!({}),
            source_event_seqs: Some(vec![3]),
            surface_op: Some(SurfaceOp::Append),
            ignorable: None,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["surfaceOp"], "append");
        assert_eq!(v["sourceEventSeqs"], json!([3]));
        assert_eq!(serde_json::from_value::<SessionEvent>(v).unwrap(), ev);

        ev.surface_op = Some(SurfaceOp::Replace { start: 2, end: 4 });
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["surfaceOp"]["op"], "replace");
        assert_eq!(v["surfaceOp"]["start"], 2);
        assert_eq!(v["surfaceOp"]["end"], 4);
        assert_eq!(serde_json::from_value::<SessionEvent>(v).unwrap(), ev);
    }

    #[test]
    fn frame_payloads_camel_case() {
        let sub = SubscribedFrame {
            session_id: "s".into(),
            last_seq: 7,
        };
        let v = serde_json::to_value(&sub).unwrap();
        assert_eq!(v["sessionId"], "s");
        assert_eq!(v["lastSeq"], 7);

        let q = QuestionRequestedFrame {
            session_id: "s".into(),
            questions: vec![Question {
                id: "plan".into(),
                question: "批准该计划?".into(),
                header: None,
                detail: Some("# p".into()),
                options: Some(vec![QuestionOption {
                    label: "批准".into(),
                    description: None,
                }]),
                multi_select: Some(false),
                intent: Some(json!({ "kind": "plan-review", "approve": "批准" })),
            }],
        };
        let v = serde_json::to_value(&q).unwrap();
        assert_eq!(v["questions"][0]["multiSelect"], false);
        assert_eq!(v["questions"][0]["intent"]["kind"], "plan-review");
        assert_eq!(
            serde_json::from_value::<QuestionRequestedFrame>(v).unwrap(),
            q
        );

        let hist = HistoryValue {
            events: vec![HistoryEntry {
                event: SessionEvent {
                    ty: "turn/start".into(),
                    seq: 1,
                    time: 0,
                    data: json!({ "turn": 1 }),
                    source_event_seqs: None,
                    surface_op: None,
                    ignorable: None,
                },
                view: None,
            }],
            has_more: false,
            projections: Some(Projections {
                as_of_seq: 1,
                values: json!({ "title": "t" }),
            }),
        };
        let v = serde_json::to_value(&hist).unwrap();
        assert_eq!(v["hasMore"], false);
        assert_eq!(v["projections"]["asOfSeq"], 1);
    }
}
