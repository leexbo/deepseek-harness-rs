//! 轨迹视图折叠(台账 UI 的服务侧数据源)。
//!
//! 增量内核 [`TrajectoryFolder`]:逐事件 feed 驻留台账终态,宿主直播
//! 经其变更缓冲推 trajectory/delta;批量折叠 [`fold_trajectory`] 是
//! 「新建 folder + 逐条 feed」的包装——直播与冷会话重放同源同代码。
//! 语义:
//! - 记录分类 SYSTEM / USER / COMPACTED / ASSISTANT(message)/ TOOL,
//!   左列 Request #N 边界、Turn 标签、文本摘要(`(tool call only)` /
//!   `No output` / `Initial System Prompt` 等);
//! - 每个 LLM 请求一条 `TrajectoryRequest`(usage/ttft/时长/累计),
//!   工具记录与 tool/result 按 call seq 配对,时长取审计完成记录。

use serde::Serialize;
use serde_json::Value;

use dsh_session::EventEnvelope;

/// 请求用量(prompt 侧三桶 + 输出两桶;Usage 面板 Input/Cached/Other/Output/Reasoning)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct TrajectoryUsage {
    /// 输入 tok(input_tokens)
    pub input: u64,
    /// 缓存命中读(cached_tokens)
    pub cached: u64,
    /// 其余输入(input - cached;写缓存 + 未命中)
    pub other: u64,
    /// 输出 tok(output_tokens)
    pub output: u64,
    /// 推理 tok(reasoning_tokens)
    pub reasoning: u64,
}

impl TrajectoryUsage {
    /// 内容 tok(输出 - 推理)
    pub fn content(&self) -> u64 {
        self.output.saturating_sub(self.reasoning)
    }

    fn add(&mut self, other: &TrajectoryUsage) {
        self.input += other.input;
        self.cached += other.cached;
        self.other += other.other;
        self.output += other.output;
        self.reasoning += other.reasoning;
    }

    fn from_usage_json(usage: &Value) -> TrajectoryUsage {
        // usage 为映射器归一后的规范形(见 dsh-llm::usage)
        let input = usage["input_tokens"].as_u64().unwrap_or(0);
        let cached = usage["cached_tokens"].as_u64().unwrap_or(0);
        let output = usage["output_tokens"].as_u64().unwrap_or(0);
        TrajectoryUsage {
            input,
            cached,
            other: input.saturating_sub(cached),
            output,
            reasoning: usage["reasoning_tokens"].as_u64().unwrap_or(0),
        }
    }
}

/// 一次 LLM 请求(台账 Request #N 边界 + 详情面板数据)
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryRequest {
    /// 全局请求序号(#N)
    pub number: u64,
    /// 轮次(1 起)
    pub turn: u64,
    /// 步(轮内 1 起;详情定位 Turn N · Step N)
    pub step: u64,
    /// 模型标识
    pub model: String,
    /// provider 方言(详情面板 Provider 行;registry 后填)
    pub provider: String,
    /// 推理等级(详情 Options 块;老日志无此字段 = None)
    pub reasoning_effort: Option<String>,
    /// complete / error(无完成记录 = error)
    pub status: String,
    /// 请求开始(epoch ms;audit llm request 落档时刻)
    pub started_at: i64,
    /// 请求完成(epoch ms;0 = 未知)
    pub completed_at: i64,
    /// 总时长 ms
    pub duration_ms: i64,
    /// 首 token 延迟 ms(provider 附带)
    pub ttft_ms: Option<i64>,
    /// 本请求用量(无 usage = None)
    pub usage: Option<TrajectoryUsage>,
    /// 会话累计用量(含本请求)
    pub cumulative: TrajectoryUsage,
    /// 本步工具调用数
    pub tool_calls: u64,
}

/// 一条台账记录(表格一行)
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryRecord {
    /// 全局记录序号(#N)
    pub index: u64,
    /// 源事件 seq
    pub seq: u64,
    /// system / user / compacted / message / tool
    pub kind: String,
    /// 所属轮(1 起;None = 轮外)
    pub turn: Option<u64>,
    /// 分组:"Step N"(助手请求步)或 "Message"
    pub group: String,
    /// 本轮首条记录(Turn 标签)
    pub turn_start: bool,
    /// 单行摘要(表格主文本)
    pub text: String,
    /// 工具结果摘要(→ 后缀;"No output" = 空输出)
    pub result: Option<String>,
    /// 失败态(工具失败/请求失败)
    pub is_error: bool,
    /// 时长秒(None = 未知)
    pub time_seconds: Option<f64>,
    /// 开始时刻(epoch ms)
    pub started_at: Option<i64>,
    /// 本记录开启的请求序号(Request #N 边界)
    pub request_number: Option<u64>,
    /// 输入 tok(message 专用)
    pub input: Option<u64>,
    /// 输出 tok(message 专用)
    pub output: Option<u64>,
    /// 推理 tok(message 专用)
    pub think: Option<u64>,
    /// 首 token 延迟 ms(message 专用)
    pub ttft_ms: Option<i64>,
    /// 详情 Payload:用户文本 / 工具参数 JSON / 系统信封说明
    pub payload: Option<String>,
    /// 详情 Result:助手内容 / 工具输出 / 折叠摘要
    pub output_detail: Option<String>,
    /// 详情 Thinking:推理文本
    pub thinking_detail: Option<String>,
    /// SYSTEM 详情:System Prompt 全文(信封变更时审计附带)
    pub system_prompt: Option<String>,
    /// SYSTEM 详情:工具目录全量(信封变更时审计附带)
    pub tools_catalog: Option<Vec<Value>>,
    /// TOOL 详情:Schema(name/description/parameters;取自当前信封目录)
    pub schema_detail: Option<String>,
    /// CONTEXT 详情:注入染色对象(ev.data.source;源 messageSource——
    /// Summary 的 Source 行与 Source tab 的数据面)
    pub source: Option<Value>,
}

/// 单行摘要:首行 + 压缩空白 + 截断
fn one_line(text: &str, cap: usize) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let collapsed: String = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > cap {
        let truncated: String = collapsed.chars().take(cap).collect();
        format!("{truncated}…")
    } else {
        collapsed
    }
}

/// 从事件 content 抽出纯文本:可能是字符串(AGENTS.md 注入)或数组
/// (@session 快照经桌面拼 block),统一为字符串。
fn content_text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"].as_str() == Some("text"))
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// 当前信封工具目录按名取 spec(OpenAI function 形状优先,扁平兜底;
/// pretty 序列化 → TOOL 记录 Schema 页数据)
fn tool_schema(st: &TrajectoryFolder, name: &str) -> Option<String> {
    let spec = st.current_tools.iter().find(|t| {
        t["function"]["name"].as_str() == Some(name) || t["name"].as_str() == Some(name)
    })?;
    serde_json::to_string_pretty(spec).ok()
}

/// 工具参数规范化:内部方言的 `arguments` 是 JSON **字符串**(流式
/// 增量累积),直接 to_string 会二次转义(`"{\"command\":…}"`)。
/// 返回 (紧凑单行, pretty 全文);字符串不可解析时原文直用
fn normalize_tool_args(v: &Value) -> (String, String) {
    match v {
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(parsed) => (
                serde_json::to_string(&parsed).unwrap_or_else(|_| s.clone()),
                serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| s.clone()),
            ),
            Err(_) => (s.clone(), s.clone()),
        },
        other => (
            serde_json::to_string(other).unwrap_or_default(),
            serde_json::to_string_pretty(other).unwrap_or_default(),
        ),
    }
}

/// 折叠结果
pub struct TrajectoryData {
    /// 台账记录(时间序)
    pub records: Vec<TrajectoryRecord>,
    /// 请求清单(#1..#N)
    pub requests: Vec<TrajectoryRequest>,
}

/// 分页结果(`session.trajectory` 出口;桌面进程内消费 typed 形态)。
/// records 为尾窗,requests 恒为全量
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryPage {
    /// 台账记录(尾窗)
    pub records: Vec<TrajectoryRecord>,
    /// 全量请求清单(#1..#N)
    pub requests: Vec<TrajectoryRequest>,
    /// 是否有更早记录(beforeIndex 向前分页)
    pub has_older: bool,
    /// 总记录数(全会话,不受窗口影响)
    pub total: u64,
}

/// 上一个请求信封(变更检测 → SYSTEM 记录)。
/// 新日志携带全量快照 → 深比对(headerEquals);老日志缺字段
/// 时退化为 (model, systemChars, toolsChars) 字符数比对
#[derive(Clone)]
enum Envelope {
    Full(String, String, Vec<Value>),
    Chars(String, u64, u64),
}

/// 一批台账变更(upsert 语义:records 按 index 定位,requests 按 number)。
/// 宿主 trajectory/delta 帧的载荷来源;同键重复出现取末次(全量对象覆盖)
#[derive(Debug, Default, Clone)]
pub struct TrajectoryChanges {
    /// 新增/更新的记录(全量对象)
    pub records: Vec<TrajectoryRecord>,
    /// 新增/更新的请求(全量对象)
    pub requests: Vec<TrajectoryRequest>,
}

impl TrajectoryChanges {
    /// 是否无待取变更
    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && self.requests.is_empty()
    }
}

/// 增量轨迹折叠器:逐事件 `feed`,驻留台账终态(`records`/`requests`),
/// 快照与直播增量同一份状态——批量折叠 [`fold_trajectory`] 即「新建
/// folder + 逐条 feed + data」的包装,两条路径输出必然一致。
///
/// 批量折叠尾部后处理的增量等价:
/// - Initial System Prompt 置顶:创建时插队首 + 全量重编号(仅一次);
/// - 无结果调用冲刷:tool/call 缓冲,turn/end 冲刷(旧批量在日志尽头,
///   按 seq 插回事件序位置,两者同序);
/// - 每请求 tool_calls:tool/call 到达时就地更新(旧批量尾部回填);
/// - 工具时长:result 配对时附着;晚到的审计完成事件就地补写。
#[derive(Default)]
pub struct TrajectoryFolder {
    /// 最近喂入事件的 seq(宿主增量补喂的连续性判据)
    last_seq: u64,
    turn: u64,
    step: u64,
    /// 本轮是否已出记录(turn_start 归属)
    turn_has_record: bool,
    request_no: u64,
    /// 上一个请求信封(变更检测 → SYSTEM 记录)
    last_envelope: Option<Envelope>,
    /// 当前信封的工具目录(schema 附着数据源)
    current_tools: Vec<Value>,
    /// 打开的请求(audit request 已见,等 request-done)
    open_request: Option<OpenRequest>,
    /// 已完成请求的指标,附着到本步 assistant/message
    pending_metrics: Option<RequestMetrics>,
    /// 本步累积推理文本
    step_reasoning: String,
    /// call seq → (时长 ms)来自审计完成记录
    tool_duration: std::collections::HashMap<u64, i64>,
    /// 等待结果的调用(seq,记录;index 占位 0,入表时定)
    pending_calls: Vec<(u64, TrajectoryRecord)>,
    /// (turn, step) → 工具记录条数
    tools_by_step: std::collections::HashMap<(u64, u64), u64>,
    cumulative: TrajectoryUsage,
    /// 台账记录(显示序);不变式:records[i].index == i+1
    records: Vec<TrajectoryRecord>,
    /// 已完成请求(number 升序)
    requests: Vec<TrajectoryRequest>,
    /// 上次 take_changes 以来的变更积压
    dirty: TrajectoryChanges,
}

struct OpenRequest {
    number: u64,
    start_ts: i64,
    model: String,
    reasoning_effort: Option<String>,
}

struct RequestMetrics {
    number: u64,
    start_ts: i64,
    duration_ms: i64,
    ttft_ms: Option<i64>,
    usage: Option<TrajectoryUsage>,
}

/// 批量折叠日志为轨迹数据(增量包装:新建 folder 逐条 feed 后取终态,
/// 与直播增量同一份代码,输出必然一致)。
pub fn fold_trajectory(events: &[EventEnvelope]) -> TrajectoryData {
    let mut folder = TrajectoryFolder::new();
    for ev in events {
        folder.feed(ev);
    }
    folder.data()
}

/// 全量数据裁窗为分页视图(窗口语义唯一实现;快照与冷读路径共用):
/// before_index 给定时保留更早记录仍取尾窗;total 不受窗口影响
pub fn page_of(
    data: TrajectoryData,
    max_records: usize,
    before_index: Option<u64>,
) -> TrajectoryPage {
    let total = data.records.len() as u64;
    let max_records = max_records.clamp(1, 2000);
    let mut records = data.records;
    if let Some(before) = before_index {
        records.retain(|r| r.index < before);
    }
    let has_older = records.len() > max_records;
    if has_older {
        let start = records.len() - max_records;
        records.drain(..start);
    }
    TrajectoryPage {
        records,
        requests: data.requests,
        has_older,
        total,
    }
}

/// 在显示序记录表中按 seq(事件序)插入并修复 index 不变式(纯克隆版;
/// 驻留态版本见 [`TrajectoryFolder::insert_record_ordered`])
fn insert_ordered(records: &mut Vec<TrajectoryRecord>, mut rec: TrajectoryRecord) {
    let pos = records
        .iter()
        .position(|r| r.seq > rec.seq)
        .unwrap_or(records.len());
    rec.index = pos as u64 + 1;
    records.insert(pos, rec);
    for r in &mut records[pos + 1..] {
        r.index += 1;
    }
}

impl TrajectoryFolder {
    /// 新建空折叠器
    pub fn new() -> Self {
        Self::default()
    }

    /// 最近喂入事件的 seq
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// 台账记录总数(驻留 + 等冲刷;turn/end 后与快照 total 一致)
    pub fn total(&self) -> u64 {
        (self.records.len() + self.pending_calls.len()) as u64
    }

    /// 喂入一个事件;返回台账是否有对外可见变更。变更积压在内部
    /// 缓冲,经 [`Self::take_changes`] 取走排空
    pub fn feed(&mut self, ev: &EventEnvelope) -> bool {
        self.last_seq = ev.seq;
        self.feed_inner(ev);
        !self.dirty.is_empty()
    }

    /// 取走并排空积压变更(upsert 载荷)
    pub fn take_changes(&mut self) -> TrajectoryChanges {
        std::mem::take(&mut self.dirty)
    }

    /// 台账终态(尾冲刷 + 占位补全;不分页)
    pub fn data(&self) -> TrajectoryData {
        let mut records = self.records.clone();
        // 冷尾冲刷:无 turn/end 收尾的日志(崩溃/截断)——未配对调用
        // 按事件序插回(时长从审计表补读),在途请求补 error 占位
        // (与批量折叠尾部同构)
        for (seq, rec) in &self.pending_calls {
            let mut rec = rec.clone();
            rec.time_seconds = self.tool_duration.get(seq).map(|ms| *ms as f64 / 1000.0);
            insert_ordered(&mut records, rec);
        }
        let mut requests = self.requests.clone();
        if let Some(open) = &self.open_request {
            requests.push(self.error_request(open));
        }
        TrajectoryData { records, requests }
    }

    /// 分页快照(桌面出口;语义与批量折叠后裁窗一致)
    pub fn snapshot(&self, max_records: usize, before_index: Option<u64>) -> TrajectoryPage {
        page_of(self.data(), max_records, before_index)
    }

    /// 在途请求 → error 占位(批量折叠尾部同构)
    fn error_request(&self, open: &OpenRequest) -> TrajectoryRequest {
        TrajectoryRequest {
            number: open.number,
            turn: self.turn,
            step: self.step,
            model: open.model.clone(),
            provider: String::new(),
            reasoning_effort: open.reasoning_effort.clone(),
            status: "error".into(),
            started_at: open.start_ts,
            completed_at: 0,
            duration_ms: 0,
            ttft_ms: None,
            usage: None,
            cumulative: self.cumulative,
            tool_calls: self
                .tools_by_step
                .get(&(self.turn, self.step))
                .copied()
                .unwrap_or(0),
        }
    }

    /// 追加一条已完成请求
    fn push_request(&mut self, req: TrajectoryRequest) {
        self.dirty.requests.push(req.clone());
        self.requests.push(req);
    }

    /// 追加一条记录(显示序尾;index = 行序不变式)
    fn stage_record(&mut self, mut rec: TrajectoryRecord) {
        rec.index = self.records.len() as u64 + 1;
        self.dirty.records.push(rec.clone());
        self.records.push(rec);
    }

    /// 按 seq(事件序)插入记录并修复 index 不变式。非纯追加(中途
    /// 插入)时后段整体重编号 → 脏缓冲全量重发(桌面按 index upsert
    /// 收敛);常态追加只重发自身
    fn insert_record_ordered(&mut self, mut rec: TrajectoryRecord) {
        let pos = self
            .records
            .iter()
            .position(|r| r.seq > rec.seq)
            .unwrap_or(self.records.len());
        let appended = pos == self.records.len();
        rec.index = pos as u64 + 1;
        self.records.insert(pos, rec);
        if !appended {
            for r in &mut self.records[pos + 1..] {
                r.index += 1;
            }
            self.dirty.records = self.records.clone();
        } else if let Some(rec) = self.records.last().cloned() {
            self.dirty.records.push(rec);
        }
    }

    /// 冲刷等待结果的调用(取消/中断;无结果列)。时长从审计表补读
    /// (完成事件已到的取消场景)
    fn flush_pending_calls(&mut self) {
        let pendings = std::mem::take(&mut self.pending_calls);
        for (seq, mut rec) in pendings {
            rec.time_seconds = self.tool_duration.get(&seq).map(|ms| *ms as f64 / 1000.0);
            self.insert_record_ordered(rec);
        }
    }

    fn feed_inner(&mut self, ev: &EventEnvelope) {
        match ev.r#type.as_str() {
            "turn/start" => {
                self.turn += 1;
                self.step = 0;
                self.turn_has_record = false;
            }
            "step/start" => self.step += 1,
            "turn/end" => {
                // 无结果调用冲刷 + 在途请求收口(取消/中断;旧批量在
                // 日志尽头冲刷,按 seq 插回事件序位置两者同序)。
                // take 后快照不再重复发占位
                self.flush_pending_calls();
                if let Some(open) = self.open_request.take() {
                    let req = self.error_request(&open);
                    self.push_request(req);
                }
            }
            "user/message" => {
                // 分流(trajectory-message-definitions 的
                // `source.kind !== 'user'` → kind:'context'):真实用户消息
                // 记 kind:"user";注入上下文(AGENTS.md/@session 等,content
                // 可为字符串或块数组)记独立 CONTEXT 行。注入行是用户回合的
                // 上下文注入口,不顶替 Turn 标签(不置 turn_has_record)。
                let source_kind = ev.data["source"]["kind"].as_str().unwrap_or("user");
                let inject = source_kind != "user";
                let text = if inject {
                    content_text_of(&ev.data["content"])
                } else {
                    ev.data["content"].as_str().unwrap_or_default().to_string()
                };
                self.stage_record(TrajectoryRecord {
                    index: 0,
                    seq: ev.seq,
                    kind: if inject { "context" } else { "user" }.into(),
                    turn: Some(self.turn),
                    group: "Message".into(),
                    turn_start: !inject && !self.turn_has_record && self.turn > 0,
                    text: one_line(&text, 200),
                    result: None,
                    is_error: false,
                    time_seconds: None,
                    started_at: Some(ev.time),
                    request_number: None,
                    input: None,
                    output: None,
                    think: None,
                    ttft_ms: None,
                    payload: if text.is_empty() { None } else { Some(text) },
                    output_detail: None,
                    thinking_detail: None,
                    system_prompt: None,
                    tools_catalog: None,
                    schema_detail: None,
                    // 源 messageSource:注入行带 source 染色(真用户消息无)
                    source: inject.then(|| ev.data["source"].clone()),
                });
                if !inject {
                    self.turn_has_record = true;
                }
            }
            "assistant/reasoning" => {
                if let Some(text) = ev.data["text"].as_str() {
                    self.step_reasoning.push_str(text);
                }
            }
            "audit/call" => {
                let boundary = ev.data["boundary"].as_str().unwrap_or_default();
                let operation = ev.data["operation"].as_str().unwrap_or_default();
                let detail = &ev.data["detail"];
                // 审计完成记录:时长键值入表;若对应工具记录已入表
                // (result 先于审计完成的罕见序),就地补写时长
                if boundary == "tool"
                    && let Some(call) = detail["call"].as_u64()
                    && let Some(ms) = detail["durationMs"].as_i64()
                {
                    self.tool_duration.insert(call, ms);
                    if let Some(rec) = self
                        .records
                        .iter_mut()
                        .find(|r| r.kind == "tool" && r.seq == call)
                    {
                        rec.time_seconds = Some(ms as f64 / 1000.0);
                        let rec = rec.clone();
                        self.dirty.records.push(rec);
                    }
                }
                if boundary == "llm" && operation == "request" {
                    // 新请求:#N + 信封变更检测(SYSTEM 记录)。
                    // 全量快照在场 → 深比对;否则退化字符数比对(老日志)
                    self.request_no += 1;
                    let model = detail["model"].as_str().unwrap_or_default().to_string();
                    let system_chars = detail["systemChars"].as_u64().unwrap_or(0);
                    let tools_chars = detail["toolsChars"].as_u64().unwrap_or(0);
                    let full_system = detail["systemPrompt"].as_str().map(String::from);
                    let full_tools = detail["tools"].as_array().cloned();
                    let full = full_system.clone().zip(full_tools.clone());
                    let (system_changed, tools_changed) = match (&self.last_envelope, &full) {
                        (Some(Envelope::Full(pm, ps, pt)), Some((s, t))) => {
                            (pm != &model || ps != s, pt != t)
                        }
                        (Some(Envelope::Chars(pm, sc, tc)), None) => {
                            (pm != &model || *sc != system_chars, *tc != tools_chars)
                        }
                        // 已落全量快照后事件缺快照 = 引擎判未变更而省略
                        (Some(Envelope::Full(_, _, _)), None) => (false, false),
                        (None, _) => (true, true),
                        // 形态混用(升级前后事件同日志):保守视为变更
                        (Some(Envelope::Chars(_, _, _)), Some(_)) => (true, true),
                    };
                    let label = match &self.last_envelope {
                        None => "Initial System Prompt",
                        Some(_) if system_changed || tools_changed => {
                            match (system_changed, tools_changed) {
                                (true, true) => "System Prompt and Tools Updated",
                                (true, false) => "System Prompt Updated",
                                _ => "Tools Updated",
                            }
                        }
                        Some(_) => "",
                    };
                    if !label.is_empty() {
                        let rec = TrajectoryRecord {
                            index: 0,
                            seq: ev.seq,
                            kind: "system".into(),
                            turn: if self.turn == 0 {
                                None
                            } else {
                                Some(self.turn)
                            },
                            group: "Message".into(),
                            turn_start: false,
                            text: label.into(),
                            result: None,
                            is_error: false,
                            time_seconds: Some(0.0),
                            started_at: Some(ev.time),
                            request_number: None,
                            input: None,
                            output: None,
                            think: None,
                            ttft_ms: None,
                            payload: Some(format!(
                                "Model {model} · system {system_chars} chars · tools {tools_chars} chars"
                            )),
                            output_detail: None,
                            thinking_detail: None,
                            system_prompt: full_system.clone(),
                            tools_catalog: full_tools.clone(),
                            schema_detail: None,
                            source: None,
                        };
                        if label == "Initial System Prompt" {
                            // 置顶(源 layoutEntryOrder 对 initial 给
                            // NEGATIVE_INFINITY:事件时序上晚于首条用户消息,
                            // 显示层钉在台账首位;Turn 标签仍归属用户行)。
                            // 插队首 + 全量重编号,脏缓冲全量重发
                            self.records.insert(0, rec);
                            for (i, r) in self.records.iter_mut().enumerate() {
                                r.index = i as u64 + 1;
                            }
                            self.dirty.records = self.records.clone();
                        } else {
                            self.stage_record(rec);
                        }
                        if self.turn > 0 {
                            self.turn_has_record = true;
                        }
                    }
                    if let Some((s, t)) = &full {
                        self.last_envelope =
                            Some(Envelope::Full(model.clone(), s.clone(), t.clone()));
                        self.current_tools = t.clone();
                    } else {
                        self.last_envelope =
                            Some(Envelope::Chars(model.clone(), system_chars, tools_chars));
                    }
                    self.open_request = Some(OpenRequest {
                        number: self.request_no,
                        start_ts: ev.time,
                        reasoning_effort: detail["reasoningEffort"].as_str().map(String::from),
                        model,
                    });
                } else if boundary == "llm" && operation == "request-done" {
                    // 完成请求:usage + 时长,附着到本步 assistant/message。
                    // 工具数读当前步计数(本步工具调用晚于 request-done,
                    // 到达时经 tool/call 臂就地回填)
                    let Some(open) = self.open_request.take() else {
                        return;
                    };
                    let duration_ms = detail["durationMs"].as_i64().unwrap_or(0);
                    let usage = if detail["usage"].is_null() {
                        None
                    } else {
                        Some(TrajectoryUsage::from_usage_json(&detail["usage"]))
                    };
                    let ttft_ms = detail["usage"]["ttftMs"].as_i64();
                    if let Some(u) = &usage {
                        self.cumulative.add(u);
                    }
                    let cumulative = self.cumulative;
                    let req = TrajectoryRequest {
                        number: open.number,
                        turn: self.turn,
                        step: self.step,
                        model: open.model,
                        provider: String::new(),
                        reasoning_effort: open.reasoning_effort,
                        status: "complete".into(),
                        started_at: open.start_ts,
                        completed_at: ev.time,
                        duration_ms,
                        ttft_ms,
                        usage,
                        cumulative,
                        tool_calls: self
                            .tools_by_step
                            .get(&(self.turn, self.step))
                            .copied()
                            .unwrap_or(0),
                    };
                    self.push_request(req);
                    self.pending_metrics = Some(RequestMetrics {
                        number: open.number,
                        start_ts: open.start_ts,
                        duration_ms,
                        ttft_ms,
                        usage,
                    });
                }
            }
            "assistant/message" => {
                let content = ev.data["content"].as_str().unwrap_or_default();
                let tool_calls = ev.data["tool_calls"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let m = self.pending_metrics.take();
                let reasoning = std::mem::take(&mut self.step_reasoning);
                let text = if content.trim().is_empty() && !tool_calls.is_empty() {
                    "(tool call only)".to_string()
                } else {
                    one_line(content, 200)
                };
                self.stage_record(TrajectoryRecord {
                    index: 0,
                    seq: ev.seq,
                    kind: "message".into(),
                    turn: if self.turn == 0 {
                        None
                    } else {
                        Some(self.turn)
                    },
                    group: format!("Step {}", self.step.max(1)),
                    turn_start: !self.turn_has_record && self.turn > 0,
                    text,
                    result: None,
                    is_error: false,
                    time_seconds: m.as_ref().map(|m| m.duration_ms as f64 / 1000.0),
                    started_at: m.as_ref().map(|m| m.start_ts),
                    request_number: m.as_ref().map(|m| m.number),
                    input: m.as_ref().and_then(|m| m.usage.map(|u| u.input)),
                    output: m.as_ref().and_then(|m| m.usage.map(|u| u.output)),
                    think: m.as_ref().and_then(|m| m.usage.map(|u| u.reasoning)),
                    ttft_ms: m.as_ref().and_then(|m| m.ttft_ms),
                    payload: None,
                    output_detail: if content.is_empty() {
                        None
                    } else {
                        Some(content.to_string())
                    },
                    thinking_detail: if reasoning.is_empty() {
                        None
                    } else {
                        Some(reasoning)
                    },
                    system_prompt: None,
                    tools_catalog: None,
                    schema_detail: None,
                    source: None,
                });
                if self.turn > 0 {
                    self.turn_has_record = true;
                }
            }
            "tool/call" => {
                let name = ev.data["name"].as_str().unwrap_or_default().to_string();
                let (args_json, args_pretty) = normalize_tool_args(&ev.data["arguments"]);
                let record = TrajectoryRecord {
                    // index 占位:配对入表时按事件序定(显示序不变式)
                    index: 0,
                    seq: ev.seq,
                    kind: "tool".into(),
                    turn: if self.turn == 0 {
                        None
                    } else {
                        Some(self.turn)
                    },
                    group: format!("Step {}", self.step.max(1)),
                    turn_start: false,
                    text: format!("{name} {args_json}"),
                    result: None,
                    is_error: false,
                    // 时长在 result 配对时附着(晚到的审计完成就地补写)
                    time_seconds: None,
                    started_at: Some(ev.time),
                    request_number: None,
                    input: None,
                    output: None,
                    think: None,
                    ttft_ms: None,
                    payload: Some(args_pretty),
                    output_detail: None,
                    thinking_detail: None,
                    system_prompt: None,
                    tools_catalog: None,
                    schema_detail: tool_schema(self, &name),
                    source: None,
                };
                *self
                    .tools_by_step
                    .entry((self.turn, self.step))
                    .or_insert(0) += 1;
                // 每请求工具数就地回填(旧批量尾部回填;事件序保证本步
                // 请求已入表,重试同 (turn,step) 的多个请求一并更新)
                let calls = self
                    .tools_by_step
                    .get(&(self.turn, self.step))
                    .copied()
                    .unwrap_or(0);
                for req in self.requests.iter_mut() {
                    if req.turn == self.turn && req.step == self.step {
                        req.tool_calls = calls;
                        let req = req.clone();
                        self.dirty.requests.push(req);
                    }
                }
                self.pending_calls.push((ev.seq, record));
                if self.turn > 0 {
                    self.turn_has_record = true;
                }
            }
            "tool/result" => {
                // 配对:等待中的 tool/call(同 seq)→ 补结果与失败态
                let call_seq = ev.data["call"].as_u64().unwrap_or(0);
                if let Some(pos) = self.pending_calls.iter().position(|(s, _)| *s == call_seq) {
                    let (_, mut record) = self.pending_calls.remove(pos);
                    let output = ev.data["output"].as_str().unwrap_or_default();
                    record.result = Some(if output.trim().is_empty() {
                        "No output".into()
                    } else {
                        one_line(output, 160)
                    });
                    record.is_error = !ev.data["success"].as_bool().unwrap_or(true);
                    record.output_detail = if output.is_empty() {
                        None
                    } else {
                        Some(output.to_string())
                    };
                    // 时长配对附着(审计完成事件通常先于 result)
                    record.time_seconds = self
                        .tool_duration
                        .get(&call_seq)
                        .map(|ms| *ms as f64 / 1000.0);
                    self.insert_record_ordered(record);
                }
            }
            "compaction/summary" => {
                let summary = ev.data["summary"].as_str().unwrap_or_default();
                self.stage_record(TrajectoryRecord {
                    index: 0,
                    seq: ev.seq,
                    kind: "compacted".into(),
                    turn: if self.turn == 0 {
                        None
                    } else {
                        Some(self.turn)
                    },
                    group: "Message".into(),
                    turn_start: !self.turn_has_record && self.turn > 0,
                    text: if summary.trim().is_empty() {
                        "Context compacted".into()
                    } else {
                        one_line(summary, 200)
                    },
                    result: None,
                    is_error: false,
                    time_seconds: None,
                    started_at: Some(ev.time),
                    request_number: None,
                    input: None,
                    output: None,
                    think: None,
                    ttft_ms: None,
                    payload: None,
                    output_detail: if summary.is_empty() {
                        None
                    } else {
                        Some(summary.to_string())
                    },
                    thinking_detail: None,
                    system_prompt: None,
                    tools_catalog: None,
                    schema_detail: None,
                    source: None,
                });
                if self.turn > 0 {
                    self.turn_has_record = true;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(r#type: &str, data: Value) -> EventEnvelope {
        EventEnvelope::new(r#type, 0, data)
    }

    fn ev_seq(r#type: &str, seq: u64, ts: i64, data: Value) -> EventEnvelope {
        let mut e = ev(r#type, data);
        e.seq = seq;
        e.time = ts;
        e
    }

    fn audit_request(ts: i64, model: &str, system: u64, tools: u64) -> EventEnvelope {
        ev_seq(
            "audit/call",
            1,
            ts,
            json!({
                "boundary": "llm",
                "operation": "request",
                "detail": { "model": model, "systemChars": system, "toolsChars": tools },
            }),
        )
    }

    fn audit_done(ts: i64, usage: Value) -> EventEnvelope {
        ev_seq(
            "audit/call",
            2,
            ts,
            json!({
                "boundary": "llm",
                "operation": "request-done",
                "detail": { "durationMs": 3300, "usage": usage },
            }),
        )
    }

    #[test]
    fn full_turn_with_tool_call_folds_to_request_ledger() {
        let log = vec![
            ev_seq("turn/start", 1, 1000, json!({})),
            ev_seq("user/message", 2, 1010, json!({ "content": "修复 bug" })),
            ev_seq("step/start", 3, 1020, json!({})),
            audit_request(1030, "deepseek-v4-flash", 500, 2000),
            ev_seq(
                "assistant/reasoning",
                4,
                1040,
                json!({ "text": "先看文件" }),
            ),
            audit_done(
                4300,
                json!({
                    "input_tokens": 1000,
                    "cached_tokens": 900,
                    "output_tokens": 50,
                    "reasoning_tokens": 20,
                    "ttftMs": 500,
                }),
            ),
            ev_seq(
                "assistant/message",
                5,
                4310,
                json!({
                    "content": "",
                    "tool_calls": [{ "id": "c1", "name": "read", "arguments": { "path": "x.rs" } }],
                }),
            ),
            ev_seq(
                "tool/call",
                6,
                4320,
                json!({ "name": "read", "arguments": { "path": "x.rs" } }),
            ),
            ev_seq(
                "audit/call",
                7,
                4330,
                json!({ "boundary": "tool", "operation": "read", "detail": { "call": 6 } }),
            ),
            ev_seq(
                "audit/call",
                8,
                4350,
                json!({ "boundary": "tool", "operation": "read",
                        "detail": { "call": 6, "durationMs": 25 } }),
            ),
            ev_seq(
                "tool/result",
                9,
                4360,
                json!({ "call": 6, "output": "fn main() {}", "success": true }),
            ),
            ev_seq("step/end", 10, 4370, json!({})),
            ev_seq("turn/end", 11, 4380, json!({})),
        ];
        let data = fold_trajectory(&log);

        // 记录:SYSTEM + USER + MESSAGE + TOOL(Initial System Prompt 显示层
        // 置顶——源 layoutEntryOrder 对 initial 给 NEGATIVE_INFINITY;事件
        // 时序上它本晚于 user/message,折叠后钉到台账首位并重编号)
        let kinds: Vec<&str> = data.records.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, vec!["system", "user", "message", "tool"]);

        let system = &data.records[0];
        assert_eq!(system.text, "Initial System Prompt");
        assert_eq!(system.index, 1, "置顶后重编号");
        assert!(!system.turn_start, "Turn 标签仍归属用户行");
        let user = &data.records[1];
        assert_eq!(user.text, "修复 bug");
        assert!(user.turn_start, "user 是本轮首条(Turn 标签)");
        assert_eq!(user.turn, Some(1));

        let message = &data.records[2];
        assert_eq!(message.text, "(tool call only)");
        assert_eq!(message.group, "Step 1");
        assert_eq!(message.request_number, Some(1));
        assert_eq!(message.input, Some(1000));
        assert_eq!(message.output, Some(50));
        assert_eq!(message.think, Some(20));
        assert_eq!(message.time_seconds, Some(3.3));
        assert_eq!(message.thinking_detail.as_deref(), Some("先看文件"));

        let tool = &data.records[3];
        assert!(tool.text.starts_with("read {\"path\":\"x.rs\"}"));
        assert_eq!(tool.result.as_deref(), Some("fn main() {}"));
        assert_eq!(tool.time_seconds, Some(0.025));
        assert!(!tool.is_error);

        // 请求:#1 complete + usage 桶 + 工具数
        assert_eq!(data.requests.len(), 1);
        let req = &data.requests[0];
        assert_eq!(req.status, "complete");
        assert_eq!(req.turn, 1);
        assert_eq!(req.duration_ms, 3300);
        assert_eq!(req.ttft_ms, Some(500));
        assert_eq!(req.tool_calls, 1);
        let u = req.usage.unwrap();
        assert_eq!((u.input, u.cached, u.other), (1000, 900, 100));
        assert_eq!((u.output, u.reasoning, u.content()), (50, 20, 30));
        assert_eq!(req.cumulative.input, 1000);
    }

    /// 4a:user/message + source.kind≠user(注入上下文)折叠为独立 CONTEXT 记录。
    /// 源以 `source.kind !== 'user'` 落 kind:'context',独立于用户行。
    #[test]
    fn context_message_folds_to_context_record() {
        let log = vec![
            ev_seq("turn/start", 1, 1000, json!({})),
            ev_seq(
                "user/message",
                2,
                1010,
                json!({
                    "id": "ctx-1",
                    "role": "user",
                    "content": [ { "type": "text", "text": "AGENTS.md 全文" } ],
                    "source": { "kind": "agent-instructions", "form": "instructions" },
                }),
            ),
            ev_seq("user/message", 3, 1020, json!({ "content": "修复 bug" })),
            ev_seq("turn/end", 4, 1030, json!({})),
        ];
        let data = fold_trajectory(&log);
        let kinds: Vec<&str> = data.records.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, vec!["context", "user"]);
        let ctx = &data.records[0];
        assert_eq!(ctx.kind, "context");
        assert_eq!(ctx.text, "AGENTS.md 全文");
        assert_eq!(ctx.group, "Message");
        assert!(!ctx.turn_start, "context 不应顶替 Turn 标签");
        let payload = ctx.payload.as_ref().expect("context 应有 payload");
        assert_eq!(payload, "AGENTS.md 全文");
        // 字符串 content 也应能折叠(AGENTS.md 走字符串源)
        let log2 = vec![ev_seq(
            "user/message",
            1,
            1010,
            json!({ "content": "@session 快照文本", "source": { "kind": "session-reference" } }),
        )];
        let data2 = fold_trajectory(&log2);
        assert_eq!(data2.records.len(), 1);
        assert_eq!(data2.records[0].kind, "context");
        assert_eq!(data2.records[0].text, "@session 快照文本");
    }

    /// 全量快照数据面:信封变更时 SYSTEM 记录携带 systemPrompt/tools;
    /// 深比对(等长不同内容不再漏报);工具按名附着 schema
    #[test]
    fn full_snapshot_deep_compare_and_schema_attach() {
        let tools = json!([
            { "type": "function", "function": {
                "name": "bash", "description": "Run a command",
                "parameters": { "type": "object", "properties": {} } } },
        ]);
        let audit_full = |ts: i64, system: &str, tools: &Value| {
            ev_seq(
                "audit/call",
                1,
                ts,
                json!({
                    "boundary": "llm",
                    "operation": "request",
                    "detail": {
                        "model": "m1",
                        "systemChars": system.chars().count(),
                        "toolsChars": serde_json::to_string(tools).unwrap().chars().count(),
                        "systemPrompt": system,
                        "tools": tools,
                    },
                }),
            )
        };
        let log = vec![
            ev_seq("turn/start", 1, 1000, json!({})),
            ev_seq("user/message", 2, 1010, json!({ "content": "hi" })),
            audit_full(1020, "prompt-a", &tools),
            ev_seq(
                "audit/call",
                3,
                1030,
                json!({
                "boundary": "llm", "operation": "request-done",
                "detail": { "durationMs": 100, "usage": {} } }),
            ),
            ev_seq(
                "assistant/message",
                4,
                1040,
                json!({ "content": "", "tool_calls": [] }),
            ),
            // 等长不同内容的 system(字符数比对会漏;深比对必须报变更)
            ev_seq("turn/start", 5, 2000, json!({})),
            ev_seq("user/message", 6, 2010, json!({ "content": "again" })),
            audit_full(2020, "prompt-b", &tools),
            ev_seq(
                "audit/call",
                7,
                2030,
                json!({
                "boundary": "llm", "operation": "request-done",
                "detail": { "durationMs": 100, "usage": {} } }),
            ),
            ev_seq(
                "tool/call",
                8,
                2040,
                json!({ "name": "bash", "arguments": "{\"command\":\"ls\"}" }),
            ),
            ev_seq(
                "tool/result",
                9,
                2050,
                json!({ "call": 8, "output": "ok", "success": true }),
            ),
        ];
        let data = fold_trajectory(&log);
        let systems: Vec<&str> = data
            .records
            .iter()
            .filter(|r| r.kind == "system")
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(
            systems,
            vec!["Initial System Prompt", "System Prompt Updated"],
            "等长不同内容:深比对须报变更"
        );
        // 快照字段:两条 SYSTEM 各携带当时的全文
        assert_eq!(data.records[0].system_prompt.as_deref(), Some("prompt-a"));
        let updated = data
            .records
            .iter()
            .find(|r| r.text == "System Prompt Updated")
            .unwrap();
        assert_eq!(updated.system_prompt.as_deref(), Some("prompt-b"));
        let catalog = data.records[0].tools_catalog.as_ref().unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0]["function"]["name"], json!("bash"));
        // 工具记录:按名附着 schema(pretty 全文含 parameters)
        let tool_rec = data.records.iter().find(|r| r.kind == "tool").unwrap();
        let schema = tool_rec.schema_detail.as_deref().unwrap();
        assert!(
            schema.contains("\"bash\"") && schema.contains("parameters"),
            "{schema}"
        );
    }

    /// 字符串形态 arguments(内部方言:流式增量累积的 JSON 字符串)→
    /// 折叠前解析,台账/详情不再二次转义
    #[test]
    fn string_form_arguments_fold_without_escape() {
        let log = vec![
            ev_seq(
                "tool/call",
                6,
                4320,
                json!({
                    "name": "bash",
                    "arguments": "{\"command\":\"echo hi\",\"description\":\"演示\"}",
                }),
            ),
            ev_seq(
                "tool/result",
                9,
                4360,
                json!({ "call": 6, "output": "hi", "success": true }),
            ),
        ];
        let data = fold_trajectory(&log);
        let tool = &data.records[0];
        assert_eq!(
            tool.text, "bash {\"command\":\"echo hi\",\"description\":\"演示\"}",
            "台账单行不出现 \\\" 双重转义"
        );
        let payload = tool.payload.as_deref().unwrap();
        assert!(
            payload.contains("\n") && payload.contains("\"command\": \"echo hi\""),
            "payload 应为 pretty 对象,实为:{payload}"
        );
        assert!(!payload.contains("\\\""), "payload 不含转义引号");

        // 不可解析字符串:原文直用(不套引号)
        let log = vec![ev_seq(
            "tool/call",
            6,
            4320,
            json!({ "name": "bash", "arguments": "not-json" }),
        )];
        let data = fold_trajectory(&log);
        assert_eq!(data.records[0].text, "bash not-json");
        assert_eq!(data.records[0].payload.as_deref(), Some("not-json"));
    }

    #[test]
    fn envelope_change_emits_system_update_records() {
        let log = vec![
            ev_seq("turn/start", 1, 0, json!({})),
            ev_seq("user/message", 2, 0, json!({ "content": "hi" })),
            ev_seq("step/start", 3, 0, json!({})),
            audit_request(0, "m1", 100, 100),
            audit_done(0, json!({})),
            ev_seq("assistant/message", 4, 0, json!({ "content": "ok" })),
            ev_seq("step/start", 5, 0, json!({})),
            audit_request(0, "m1", 100, 900),
            audit_done(0, json!({})),
            ev_seq("assistant/message", 6, 0, json!({ "content": "ok2" })),
        ];
        let data = fold_trajectory(&log);
        let systems: Vec<&str> = data
            .records
            .iter()
            .filter(|r| r.kind == "system")
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(
            systems,
            vec!["Initial System Prompt", "Tools Updated"],
            "工具目录变更 → Tools Updated;system/model 未变不重复"
        );
        assert_eq!(data.requests.len(), 2);
        assert_eq!(data.requests[1].number, 2);
        assert_eq!(data.requests[1].step, 2);
    }

    #[test]
    fn failed_tool_and_failed_request_marked() {
        let log = vec![
            ev_seq("turn/start", 1, 0, json!({})),
            ev_seq("step/start", 2, 0, json!({})),
            audit_request(0, "m", 0, 0),
            // 无 request-done:请求失败
        ];
        let data = fold_trajectory(&log);
        assert_eq!(data.requests.len(), 1);
        assert_eq!(data.requests[0].status, "error");
        assert_eq!(data.requests[0].usage, None);
    }

    #[test]
    fn compaction_summary_folds_as_compacted() {
        let log = vec![
            ev_seq("turn/start", 1, 0, json!({})),
            ev_seq(
                "compaction/summary",
                2,
                0,
                json!({ "summary": "早期讨论了 A。\n后续做了 B。", "throughSeq": 50 }),
            ),
        ];
        let data = fold_trajectory(&log);
        let c = &data.records[0];
        assert_eq!(c.kind, "compacted");
        assert_eq!(c.text, "早期讨论了 A。");
        assert!(c.turn_start);
    }

    #[test]
    fn empty_output_result_is_no_output() {
        let log = vec![
            ev_seq(
                "tool/call",
                1,
                0,
                json!({ "name": "bash", "arguments": {} }),
            ),
            ev_seq(
                "tool/result",
                2,
                0,
                json!({ "call": 1, "output": "", "success": true }),
            ),
        ];
        let data = fold_trajectory(&log);
        assert_eq!(data.records[0].result.as_deref(), Some("No output"));
    }

    #[test]
    fn tool_duration_missing_when_no_audit_done() {
        let log = vec![
            ev_seq(
                "tool/call",
                1,
                0,
                json!({ "name": "bash", "arguments": {} }),
            ),
            ev_seq(
                "tool/result",
                2,
                0,
                json!({ "call": 1, "output": "x", "success": false }),
            ),
        ];
        let data = fold_trajectory(&log);
        let t = &data.records[0];
        assert_eq!(t.time_seconds, None);
        assert!(t.is_error, "success=false → 失败态");
    }

    // ----- 增量内核差分锁 -----

    /// 跨回合富夹具:信封全量快照深比对、工具成败、字符串 arguments、
    /// 上下文注入、压缩、取消轮(未配对调用 + 在途请求)
    fn rich_fixture() -> Vec<EventEnvelope> {
        vec![
            ev_seq("turn/start", 1, 1000, json!({})),
            ev_seq(
                "user/message",
                2,
                1010,
                json!({
                    "content": [{ "type": "text", "text": "AGENTS.md 注入" }],
                    "source": { "kind": "agent-instructions" },
                }),
            ),
            ev_seq("user/message", 3, 1020, json!({ "content": "第一问" })),
            ev_seq("step/start", 4, 1030, json!({})),
            ev_seq(
                "audit/call",
                5,
                1040,
                json!({
                    "boundary": "llm",
                    "operation": "request",
                    "detail": {
                        "model": "m1",
                        "systemChars": 10,
                        "toolsChars": 20,
                        "systemPrompt": "prompt-a",
                        "tools": [{ "name": "t1" }],
                    },
                }),
            ),
            ev_seq("assistant/reasoning", 6, 1050, json!({ "text": "想一下" })),
            ev_seq(
                "audit/call",
                7,
                1060,
                json!({ "boundary": "llm", "operation": "request-done",
                        "detail": { "durationMs": 100,
                                    "usage": { "input_tokens": 5, "output_tokens": 2 } } }),
            ),
            ev_seq(
                "assistant/message",
                8,
                1070,
                json!({ "content": "答一", "tool_calls": [] }),
            ),
            // 第二轮:等长不同内容的 system(深比对路径)
            ev_seq("turn/start", 9, 2000, json!({})),
            ev_seq("user/message", 10, 2010, json!({ "content": "第二问" })),
            ev_seq("step/start", 11, 2020, json!({})),
            ev_seq(
                "audit/call",
                12,
                2030,
                json!({
                    "boundary": "llm",
                    "operation": "request",
                    "detail": {
                        "model": "m1",
                        "systemChars": 10,
                        "toolsChars": 20,
                        "systemPrompt": "prompt-b",
                        "tools": [{ "name": "t1" }],
                    },
                }),
            ),
            ev_seq(
                "audit/call",
                13,
                2040,
                json!({ "boundary": "llm", "operation": "request-done",
                        "detail": { "durationMs": 50, "usage": {} } }),
            ),
            ev_seq(
                "assistant/message",
                14,
                2050,
                json!({ "content": "", "tool_calls": [{ "id": "c1", "name": "t1" }] }),
            ),
            ev_seq(
                "tool/call",
                15,
                2060,
                json!({ "name": "t1", "arguments": { "p": 1 } }),
            ),
            ev_seq(
                "audit/call",
                16,
                2070,
                json!({ "boundary": "tool", "operation": "t1",
                        "detail": { "call": 15, "durationMs": 30 } }),
            ),
            ev_seq(
                "tool/result",
                17,
                2080,
                json!({ "call": 15, "output": "ok", "success": true }),
            ),
            ev_seq(
                "tool/call",
                18,
                2090,
                json!({ "name": "t2", "arguments": "not-json" }),
            ),
            ev_seq(
                "tool/result",
                19,
                2100,
                json!({ "call": 18, "output": "", "success": false }),
            ),
            ev_seq("step/start", 20, 2110, json!({})),
            ev_seq("turn/end", 21, 2200, json!({})),
            // 第三轮:取消——未配对调用 + 在途请求,turn/end 冲刷
            ev_seq("turn/start", 22, 3000, json!({})),
            ev_seq("user/message", 23, 3010, json!({ "content": "第三问" })),
            ev_seq("step/start", 24, 3020, json!({})),
            ev_seq(
                "audit/call",
                25,
                3030,
                json!({ "boundary": "llm", "operation": "request",
                        "detail": { "model": "m1", "systemChars": 10, "toolsChars": 20 } }),
            ),
            ev_seq(
                "tool/call",
                26,
                3040,
                json!({ "name": "t3", "arguments": {} }),
            ),
            ev_seq(
                "audit/call",
                27,
                3050,
                json!({ "boundary": "tool", "operation": "t3",
                        "detail": { "call": 26, "durationMs": 70 } }),
            ),
            ev_seq("turn/end", 28, 3100, json!({ "cancelled": "token" })),
            ev_seq(
                "compaction/summary",
                29,
                3200,
                json!({ "summary": "折叠摘要" }),
            ),
        ]
    }

    /// 差分基石:同一 folder 逐事件喂入后,任意前缀的快照必须与
    /// 「新建 folder 批量折叠同一前缀」逐字段一致(增量应用无跨事件
    /// 状态泄漏)
    #[test]
    fn incremental_feed_matches_batch_fold_on_every_prefix() {
        let events = rich_fixture();
        let mut folder = TrajectoryFolder::new();
        for (i, ev) in events.iter().enumerate() {
            folder.feed(ev);
            let page = folder.snapshot(usize::MAX, None);
            let batch = fold_trajectory(&events[..=i]);
            assert_eq!(page.records, batch.records, "前缀 {i} records");
            assert_eq!(page.requests, batch.requests, "前缀 {i} requests");
        }
    }

    /// turn/end 冲刷:取消轮的未配对调用照常落表(时长从审计表补读)、
    /// 在途请求收口为 error——粒度从日志尽头缩到回合
    #[test]
    fn turn_end_flushes_pending_calls_and_open_request() {
        let events = vec![
            ev_seq("turn/start", 1, 0, json!({})),
            ev_seq("step/start", 2, 0, json!({})),
            ev_seq(
                "audit/call",
                3,
                0,
                json!({ "boundary": "llm", "operation": "request",
                        "detail": { "model": "m", "systemChars": 0, "toolsChars": 0 } }),
            ),
            ev_seq(
                "tool/call",
                4,
                0,
                json!({ "name": "bash", "arguments": {} }),
            ),
            ev_seq(
                "audit/call",
                5,
                0,
                json!({ "boundary": "tool", "operation": "bash",
                        "detail": { "call": 4, "durationMs": 40 } }),
            ),
            ev_seq("turn/end", 6, 0, json!({ "cancelled": "token" })),
        ];
        let data = fold_trajectory(&events);
        let kinds: Vec<&str> = data.records.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, vec!["system", "tool"]);
        let tool = &data.records[1];
        assert_eq!(tool.result, None, "取消调用无结果列");
        assert_eq!(tool.time_seconds, Some(0.04), "已到的审计时长补附着");
        assert_eq!(data.requests.len(), 1);
        assert_eq!(data.requests[0].status, "error", "在途请求收口");
        // 冲刷发生在 turn/end:之后的快照(data 路径)不得重复出占位
        let mut folder = TrajectoryFolder::new();
        for ev in &events {
            folder.feed(ev);
        }
        assert_eq!(folder.data().requests.len(), 1);
    }

    /// 裁窗语义与宿主旧实现一致:before_index 保留更早记录仍取尾窗;
    /// total 恒为全会话记录数;max_records clamp 1..=2000
    #[test]
    fn snapshot_windowing_matches_registry_semantics() {
        let mut folder = TrajectoryFolder::new();
        for i in 0..30u64 {
            folder.feed(&ev_seq(
                "tool/call",
                i + 1,
                (i * 10) as i64,
                json!({ "name": "t", "arguments": {} }),
            ));
            folder.feed(&ev_seq(
                "tool/result",
                100 + i,
                (i * 10 + 5) as i64,
                json!({ "call": i + 1, "output": "x", "success": true }),
            ));
        }
        let page = folder.snapshot(10, None);
        assert_eq!(page.total, 30, "total 不受窗口影响");
        assert!(page.has_older);
        assert_eq!(page.records.len(), 10);
        assert_eq!(page.records[0].index, 21, "尾窗从更早一侧起");
        assert_eq!(page.records[9].index, 30);
        let page = folder.snapshot(10, Some(11));
        assert_eq!(page.records[0].index, 1, "before_index 保留更早记录");
        assert_eq!(page.records[9].index, 10);
        assert!(!page.has_older);
        assert_eq!(page.total, 30);
    }

    /// 增量流语义锁(桌面 apply 原型):records 按 index / requests 按
    /// number upsert,从空台账重放全量变更流必须收敛到**驻留台账**——
    /// 含置顶重编号/冲刷插入引发的 index 整段位移(全量重发覆盖)。
    /// 快照的冷尾补全(无 turn/end 的未配对调用/在途请求占位)是
    /// view-time 补全,不进变更流:崩溃会话无直播;有直播时 turn/end
    /// 已把两者收口进流
    #[test]
    fn delta_stream_replays_to_full_state() {
        let events = rich_fixture();
        let mut folder = TrajectoryFolder::new();
        let mut recs: std::collections::BTreeMap<u64, TrajectoryRecord> = Default::default();
        let mut reqs: std::collections::BTreeMap<u64, TrajectoryRequest> = Default::default();
        for ev in &events {
            folder.feed(ev);
            let changes = folder.take_changes();
            for r in changes.records {
                recs.insert(r.index, r);
            }
            for q in changes.requests {
                reqs.insert(q.number, q);
            }
        }
        let replayed: Vec<TrajectoryRecord> = recs.into_values().collect();
        assert_eq!(replayed, folder.records, "增量流重放 ≡ 驻留 records");
        let replayed: Vec<TrajectoryRequest> = reqs.into_values().collect();
        assert_eq!(replayed, folder.requests, "增量流重放 ≡ 驻留 requests");
    }

    /// 确定性伪随机事件序列(xorshift64,不引 rand):增量喂入不 panic、
    /// index 连续不变式成立、变更流重放收敛
    #[test]
    fn random_event_sequences_hold_invariants() {
        let mut s: u64 = 88172645463325252;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _case in 0..24 {
            let mut events = Vec::new();
            let mut seq = 0u64;
            let mut last_call = 1u64;
            for _ in 0..(rnd() % 80 + 20) {
                seq += 1;
                let ev = match rnd() % 12 {
                    0 => ev_seq("turn/start", seq, 0, json!({})),
                    1 => ev_seq("step/start", seq, 0, json!({})),
                    2 => ev_seq(
                        "user/message",
                        seq,
                        0,
                        json!({ "content": format!("u{seq}") }),
                    ),
                    3 => ev_seq(
                        "audit/call",
                        seq,
                        0,
                        json!({ "boundary": "llm", "operation": "request",
                                "detail": { "model": "m", "systemChars": rnd() % 100,
                                            "toolsChars": rnd() % 100 } }),
                    ),
                    4 => ev_seq(
                        "audit/call",
                        seq,
                        0,
                        json!({ "boundary": "llm", "operation": "request",
                                "detail": { "model": "m", "systemChars": 7, "toolsChars": 9,
                                            "systemPrompt": "p", "tools": [{ "name": "t" }] } }),
                    ),
                    5 => ev_seq(
                        "audit/call",
                        seq,
                        0,
                        json!({ "boundary": "llm", "operation": "request-done",
                                "detail": { "durationMs": (rnd() % 1000) as i64,
                                            "usage": { "input_tokens": rnd() % 50,
                                                       "output_tokens": rnd() % 50 } } }),
                    ),
                    6 => ev_seq(
                        "assistant/message",
                        seq,
                        0,
                        json!({ "content": format!("a{seq}"), "tool_calls": [] }),
                    ),
                    7 => {
                        last_call = seq;
                        ev_seq(
                            "tool/call",
                            seq,
                            0,
                            json!({ "name": "t", "arguments": { "i": seq } }),
                        )
                    }
                    8 => ev_seq(
                        "tool/result",
                        seq,
                        0,
                        json!({ "call": last_call, "output": "o", "success": true }),
                    ),
                    9 => ev_seq(
                        "audit/call",
                        seq,
                        0,
                        json!({ "boundary": "tool", "operation": "t",
                                "detail": { "call": last_call,
                                            "durationMs": (rnd() % 500) as i64 } }),
                    ),
                    10 => ev_seq(
                        "compaction/summary",
                        seq,
                        0,
                        json!({ "summary": format!("s{seq}") }),
                    ),
                    _ => ev_seq("assistant/reasoning", seq, 0, json!({ "text": "r" })),
                };
                events.push(ev);
            }
            let mut folder = TrajectoryFolder::new();
            let mut recs: std::collections::BTreeMap<u64, TrajectoryRecord> = Default::default();
            let mut reqs: std::collections::BTreeMap<u64, TrajectoryRequest> = Default::default();
            for ev in &events {
                folder.feed(ev);
                let changes = folder.take_changes();
                for r in changes.records {
                    recs.insert(r.index, r);
                }
                for q in changes.requests {
                    reqs.insert(q.number, q);
                }
            }
            let page = folder.snapshot(usize::MAX, None);
            for (i, r) in page.records.iter().enumerate() {
                assert_eq!(r.index, i as u64 + 1, "case index 连续不变式");
            }
            let replayed: Vec<TrajectoryRecord> = recs.into_values().collect();
            assert_eq!(replayed, folder.records, "case 变更流重放 ≡ 驻留 records");
            let replayed: Vec<TrajectoryRequest> = reqs.into_values().collect();
            assert_eq!(replayed, folder.requests, "case 变更流重放 ≡ 驻留 requests");
        }
    }
}
