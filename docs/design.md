# dsh 设计文档

## 1. 引言与质量目标

dsh 是 agent harness:驱动 LLM 多轮对话与工具执行,以事件日志为会话的唯一事实源。宿主为原生 Rust(wasmtime 47+),会话组件产物目标 `wasm32-wasip2`,接口按 WASI 0.3 形状定义。

质量目标(按重要性排序):

| # | 质量属性 | 场景 |
|---|---|---|
| 1 | 正确性 | 模型收到的请求与日志派生不一致时,调用被拒绝——不依赖调用方自律 |
| 2 | 可重放 | 同一日志任意时刻重放,状态与派生 bit-exact;崩溃后从日志恢复 |
| 3 | 安全 | 沙箱不可用即拒绝工具执行;组件权限由能力束显式界定 |
| 4 | 可审计 | 每次跨边界调用可归因到触发它的会话事件 |
| 5 | 可扩展 | 新增 provider / 插件 / 事件类型不修改核心路径 |

## 2. 架构约束

| 约束 | 类型 | 内容 |
|---|---|---|
| C1 | 技术 | 组件运行于 wasmtime 47+,产物目标 `wasm32-wasip2`;Rust→wasm 工具链尚无 0.3 编译目标 |
| C2 | 技术 | 组件内不可达时钟与随机源(重放确定性要求其显式注入) |
| C3 | 组织 | 接口变更单向:契约(`wit/`)→ bindgen → 实现 |
| C4 | 安全 | 沙箱与一切校验 fail-closed:失败显式拒绝,不静默降级 |
| C5 | 组织 | 依赖版本 workspace 级锁定,升级独立 PR |

## 3. 系统上下文与范围

```mermaid
flowchart LR
    USER["用户"] -->|"CLI / REPL / JSON-RPC"| DSH
    DSH["dsh"] -->|"HTTPS + SSE"| LLM["LLM provider<br/>(deepseek-responses / openai-responses / anthropic / deepseek-chat / openai-chat)"]
    DSH -->|"沙箱子进程"| OS["操作系统<br/>(bash · seatbelt / landlock / bwrap)"]
    DSH -->|"读 / 写"| FS[("文件系统<br/>(JSONL 会话日志)")]
```

**范围**:单机会话引擎——对话循环、工具执行、事件持久化、审计、stdio 互操作。会话日志存于 `<DSH_RS_HOME\|~/.dshrs>/sessions/<projectKey(workspace)>/<sessionId>/session.jsonl`(`projectKey` = 路径分隔符折叠为 `-` 并包 `--…--`;独立目录名防与其他宿主混存;旧布局(工作区根/工作区 `.dshrs`)启动时幂等迁移)。

**Non-Goals**(明确不做):多机分布式会话;自带观测后端(OTLP 仅留接缝);沙箱逃逸防御以外的系统级隔离(容器/VM);LLM 训练与缓存;web/浏览器前端(UI 仅保留 GPUI 桌面客户端,stdio 网关保留通道抽象)。

## 4. 解决方案策略

四条根本决策,系统的其余部分是它们的展开:

1. **事件溯源:日志是唯一事实源。** 会话的全部状态是追加式事件日志;模型历史、审计、遥测、终端渲染都是投影。状态只有一份,派生有唯一实现。
2. **不变式置于结构性边界。** 「模型可见 ⟺ 已记录」由宿主闸门在唯一出网点强制;沙箱不可用即拒绝执行;确定性由注入强制。规则放在无法被绕过的位置,而非约定。
3. **能力即束。** 权限是随执行世界传递的数据(cwd + 可写根 + 取消令牌),窄化传递即子环境边界;不做散落各处的运行前检查。
4. **一切皆插件:核心无特权。** 能力一律注册,核心只依赖端口——provider 是注册的 `ProviderAdapter`,工具是实现 `ToolPort` 的注册方,插件是总线订阅加生命周期,日志后端实现 `LogBackend`,传输实现 `LlmTransport`。核心路径(日志、闸门、状态机)不出现任何具体能力的名字;装配决定具体实现,扩展等于注册,不等于修改。宿主拥有原语,但原语本身也在 trait 之后。端口定义在组件侧(dsh-agent-loop,对应 WIT import),宿主实现分属 `dsh-llm`(LLM transport / provider 方言)与 `dsh-sandbox`(沙箱 / spawn / PTY)——替换实现 = 换 crate 依赖,不触碰宿主核心。
5. **契约超前,产物保守。** 接口按 WASI 0.3 形状定义,产物走 0.2 目标;切换发生时无迁移债务。

## 5. 构件视图

### 5.1 白盒:dsh 系统

```mermaid
flowchart TB
    subgraph CLI["dsh(CLI 装配)"]
        CHAT["chat(单轮 / REPL)"]
        SERVE["serve(JSON-RPC 网关)"]
    end
    subgraph LOOP["dsh-agent-loop"]
        ENGINE["LoopEngine(turn/step 状态机)"]
        CT["CancelToken"]
    end
    subgraph TRANSPORT["LLM 接入(dsh-llm)"]
        GATE["InvariantGate"]
        ADP["ProviderAdapter ×5(通用引擎 ×3 + Ext 差异点)"]
        HTTP["HttpTransport"]
        GATE --> ADP --> HTTP
    end
    subgraph TOOLS["dsh-tools"]
        BASH["BashTool(pipes / PTY)"]
        FILE["FileTools(read / edit / search)"]
        SBX["沙箱链 + 组信号(dsh-sandbox)"]
        BASH --> SBX
    end
    subgraph SESSION["dsh-session"]
        LOG[("EventLog")]
        DERIVE["derive_messages"]
    end
    subgraph SINKS["sink 消费者"]
        PERSIST[("JSONL 事实流")]
        BUS["事件总线 / 插件"]
        TELE["遥测导出"]
    end

    CHAT --> ENGINE
    SERVE --> ENGINE
    ENGINE -->|"唯一出网 import"| GATE
    ENGINE -->|"ToolSet 名字分发"| BASH
    ENGINE -->|"ToolSet 名字分发"| FILE
    ENGINE -->|"append + sink"| LOG
    LOG --> DERIVE
    DERIVE -->|"期望侧"| GATE
    LOG --> PERSIST
    LOG --> BUS
    LOG --> TELE
    CT -.-> ENGINE
```

依赖方向:`dsh` → `dsh-host` → `dsh-agent-loop` → `dsh-session`;`dsh-tools` 依赖 dsh-host 与 agent-loop;LLM 接入(`dsh-llm`)与执行原语(`dsh-sandbox`)是宿主侧的独立能力 crate——替换 provider / 沙箱实现 = 换 crate 依赖,不触碰核心;契约层(`wit/`)独立于全部 crate。

### 5.2 构件职责

| 构件 | 职责 | 对外接口 |
|---|---|---|
| `dsh` | CLI 入口(薄壳):参数解析、终端渲染(REPL/Reporter)、stdio 网关装配 | `chat` / `serve` / `info` 子命令 |
| `dsh-app` | 会话装配层:配置合并、prompt 组装、transport 构建、preset 工具组装、`Session`(turn 驱动 + 会话级事件)、日志重载 | 库 API |
| `dsh-core` | 多会话应用核心:多会话注册表(worker/队列/计划问题)、客方线上协议类型、事件翻译(我方词汇 → 客方 SessionEvent)、轨迹/统计/上下文投影;构建于 `dsh-app` 之上(单会话装配 ↔ 多会话核心的分工) | 库 API(`dsh-desktop` 进程内直连) |
| `dsh-desktop` | 桌面客户端(GPUI 原生 UI,gpui-component):进程内直连 `dsh-core` AppHost(同步方法直调/异步经 tokio runtime,mux+host 广播帧经 futures channel 桥入 GPUI),唯一 UI 客户端;关窗即退出 | `dsh-desktop [--workspace <dir>] [--fake]` |
| `dsh-host` | 组件宿主:wasmtime 组件管理器、事件总线与插件、持久化(JSONL/turso)、总线 transport、rpc 网关、遥测、配置/preset | 库 API(LLM 接入见 dsh-llm,执行原语见 dsh-sandbox) |
| `dsh-llm` | LLM 接入:通用方言引擎(chat/responses/anthropic)+ Ext 差异点(deepseek-responses / openai-responses / anthropic / deepseek-chat / openai-chat)、HTTP/SSE transport、不变式闸门(InvariantGate)、假 provider 测试装备 | 库 API;扩展点 `ProviderAdapter` / `FrameMapper` / `ChatExt` / `ResponsesExt` / `AnthropicExt` |
| `dsh-sandbox` | 执行原语:沙箱链(fail-closed)、受控 spawn/PTY(独立进程组、SIGTERM→grace→SIGKILL) | 库 API |
| `dsh-agent-loop` | turn/step 状态机;驱动端口定义 | `LlmTransport` / `Summarizer` / `ToolPort` / `ToolSet`(名字分发)/ `CancelToken`(trait 由宿主实现) |
| `dsh-session` | 事件日志:信封、类型、seq 强制、消息派生、归因查询 | rlib API + WIT `dsh:session` 导出 |
| `dsh-prompt` | system prompt 组装(纯函数;宿主注入身份/环境/指令文件内容) | `assemble(ctx)` |
| `dsh-tools` | BashTool(沙箱执行、取消、PTY)+ FileTools(file_read / file_edit / file_search——检索为 ripgrep 引擎:ignore 遍历尊重 .gitignore,grep-searcher 行搜索)+ TodoTool(todo/state)+ PlanTool(exit_plan_mode)+ GoalTool(goal/state)+ SubagentTool(嵌套引擎,独立子日志,能力束窄化)+ SubagentControlTool + JobTool(后台任务 list/read/stop;输出落盘 .dsh/jobs) | `ToolPort` 实现 |
| `dsh-wit` | host 侧 bindgen 与组件契约测试 | 测试套件 |
| `dsh-example-tool` | 示例工具组件(echo_config / spin):`dsh:tools` world 参考实现与测试物料(rlib + wasm32-wasip2 双产物,照 dsh-session 模式) | WIT `dsh:tools` 导出 |
| `wit/` | 全部契约定义,唯一契约源 | 七个 WIT 包(§7.6) |

### 5.3 仓库布局

```
wit/                      契约层(dsh:json/session/loop/events/host/plugin/tools)
crates/dsh/               宿主二进制(CLI 薄壳)
crates/dsh-app/           会话装配层
crates/dsh-desktop/       GPUI 桌面客户端(进程内直连 dsh-core 宿主;唯一 UI 客户端)
crates/dsh-core/          多会话应用核心(注册表/协议类型/事件翻译/轨迹/统计/上下文)
crates/dsh-host/          组件宿主(组件管理器/总线/持久化/网关/遥测)
crates/dsh-llm/           LLM 接入(provider 方言/HTTP/SSE/不变式闸门)
crates/dsh-sandbox/       执行原语(沙箱链/进程/PTY)
crates/dsh-wit/           host bindgen + 组件契约测试
crates/dsh-session/       事件日志组件(wasm32-wasip2 产物 + rlib)
crates/dsh-agent-loop/    turn 引擎 + 端口 trait
crates/dsh-prompt/        prompt 组装
crates/dsh-tools/         工具注册表与内置工具(含 BashTool)
crates/dsh-example-tool/  示例工具组件(dsh:tools 参考实现,测试物料)
presets/                  内置能力 preset manifest(standard / minimal,YAML,编译进二进制)
scripts/                  verify-* 脚本
```

## 6. 运行视图

### 6.1 场景:turn 与工具执行

```mermaid
sequenceDiagram
    participant U as 输入
    participant E as LoopEngine
    participant G as InvariantGate→Provider
    participant T as 工具(沙箱)
    participant L as EventLog
    U->>E: turn(input)
    E->>L: turn/start · user/message
    loop 每 step(无步数上限·模型不再发 tool_calls 即终止·取消令牌兜底失控)
        E->>L: step/start · audit/call(llm → user/message)
        E->>G: stream(messages = derive_messages(日志))
        G-->>E: chunk* · assistant/message(携带 tool_calls)
        opt 携带 tool_calls
            loop 每个 tool_call
                E->>L: tool/call · audit/call(tool → assistant/message)
                E->>T: execute(select 挂取消)
                T-->>E: tool/result
            end
        end
        E->>L: step/end
    end
    E->>L: turn/end
```

每个事件 append 后同步触发 sink(落盘 / 下行 / 渲染),再进入后续处理——「记录优先」。请求 messages 的唯一来源是 `derive_messages(日志)`;工具失败不中断循环。

### 6.2 场景:取消

软取消(CancelToken)在三个安全点生效:step 边界、出网返回后、工具执行前;触发即 `turn/end {cancelled:"token"}`。工具执行中由 select 中止:杀子进程 → 失败 tool/result → 循环继续。硬取消兜底:epoch deadline 超时即销毁竞技场,fuel 防计算 DoS。

### 6.3 场景:崩溃恢复 / 重放

JSONL 日志逐事件重放:信封校验(§7.1)通过即重建 EventLog,`derive_messages` 恢复模型历史,归因链恢复审计。竞技场可销毁并重放到任意 seq 再继续;成本 O(会话长度)。

### 6.4 场景:网关并发

`dsh serve` 中 turn 后台执行,读端不被长 turn 阻塞;`cancel` 请求在 turn 执行期间可到达并触发软取消。事件通知与响应经同一 FIFO 下行通道写出(顺序 = 事件发生顺序);通道抽象与传输无关,写端可替换为 WebSocket。

### 6.5 场景:桌面会话(dsh-desktop)

`dsh-desktop` 在自身进程内直接构造 `dsh-core` 的 `AppHost`(专任 tokio runtime):同步方法直调、异步方法经 runtime 派发,mux/host 广播帧经 futures channel 桥入 GPUI 事件循环——无 loopback HTTP/WS,无序列化往返。UI = 事件投影:GPUI reducer 与 Rust 翻译表(`dsh-core/src/translate.rs`)同仓同源;历史回填与直播共用 `session.history` 分页重放 + `session/subscribed` 基线;计划审批 = `question/requested` 帧接管 composer → `respond` → `question/resolved`。队列/steer:提交 mode=queue|steer,队列瞬态经 `session/queue` 帧整表下发(queued + steering 两种 placement,基线随 subscribed 重推),steer 认领落 `agent/inbox/spliced`(inserted+removed 双 splice,id 与 user/message 同源),`session.updateQueue` 变更 edit/remove/steer;`session.export` 返回原始会话 JSONL。


## 7. 横切概念

### 7.1 事件模型

信封:`{ type, seq, time, data, surface_op?, source_event_seqs?, ignorable }`

| 字段 | 类型 | 约束 |
|---|---|---|
| type | string | 在已登记类型表中,或 `ignorable = true` |
| seq | u64 | 从 1 起连续;写入方运行时强制 |
| time | i64(ms) | 注入时钟产生 |
| surface_op | string? | 仅 surface 事件 |
| source_event_seqs | [u64]? | 仅归因集内事件;引用已存在的 seq |
| ignorable | bool | 缺省 false |

事件类型与分类:

| 类型 | 分类 | 载荷要点 |
|---|---|---|
| session/start · turn/start · turn/end · step/start · step/end | 骨架 | turn/end 可携带 `cancelled` |
| user/message | surface | `content` |
| assistant/message | surface | `content` + 可选 `tool_calls`(扁平 `{id, name, arguments}`) |
| tool/result | surface | `call`(tool/call 的 seq)、`id`(provider 调用标识)、`output`、`success` |
| tool/call | 簿记 | `name` + `arguments` |
| assistant/chunk | ignorable | `delta` |
| audit/call | 可归因 | `boundary`(llm/tool/process)、`operation`、`detail` |
| todo/state | 簿记 | `todos` 全量快照(id/task/status);恢复 = 读最近一条 |
| compaction/summary | 簿记 | `summary`、`throughSeq`(折叠覆盖边界);重放读记录不重调 |
| session/mode | 簿记 | `mode`(standard / plan);影响 prompt → 必须入日志 |
| plan/submitted | 簿记 | `plan`;模型经 exit_plan_mode 提交 |
| plan/approved | 簿记 | `plan`;批准后注入 active-plan 段 |
| goal/state | 簿记 | `goals` 全量快照(id/text/done);恢复 = 读最近一条 |

归因集 = surface 三类 + audit/call。

读取方校验(全部 fail-closed):

| 违例 | 处置 |
|---|---|
| 格式版本不匹配 | 拒绝,不迁移 |
| seq 不连续 | 拒绝 |
| 未知类型且未标 ignorable | 拒绝重建 |
| 不可归因事件携带 source_event_seqs | 拒绝 |
| 未知类型且 ignorable | 跳过并保留载荷(前向兼容) |

### 7.2 消息派生

`message_from_event` 是「事件 → 消息」的裸映射:user/message → user;assistant/message → assistant(content + tool_calls);tool/result → tool(output, call, id);其余类型不产生消息。

模型可见消息 = 裸映射 + 显式策略栈(`derive_visible_messages`,唯一实现):

1. **tool/result 裁剪**:输出超 8192 字符截断为 head 4096 + tail 1024,中段注明省略量;常量而非配置——投影必须跨重放稳定。日志保留全文(审计保真),裁剪只作用于请求面。
2. **历史折叠**:最近一条 `compaction/summary` 之前的事件折叠为单条摘要消息(用户角色、`<session-summary>` 包裹),其后照常派生。折叠由 engine 在 step 起点判定(可见消息超预算 96000 字符,保留最近 6 条),一次性摘要经 `Summarizer` 端口出网——**非会话面请求**,不经闸门比对;其持久化 = `audit/call`(operation=compaction)+ `compaction/summary` 事件,重放读记录、不重调。

engine 的请求构造与闸门的期望比对共用该函数——策略栈两侧同一实现,不变式不被策略破坏。

### 7.3 出网不变式

```
verify(messages, header):
  expected = derive_visible_messages(共享日志) + header 折叠
  actual == expected(整体等价,键序无关)→ 放行
  otherwise                            → 拒绝调用
```

例外面:一次性摘要调用(Summarizer 端口)不走上式——其载荷不是会话派生;持久化改走事件落档(见 §7.2),可重放性由记录承担。

### 7.4 持久化

JSONL 为事实流主格式(append-only,一行一事件);SQLite(turso)为派生索引,可从 JSONL 全量重建。二者实现同一 `LogBackend` trait。

### 7.5 LLM 接入分层

本层实现位于 `dsh-llm`;端口在 dsh-agent-loop,宿主侧契约不变。

连接语义与方言差异分离:HttpTransport 管连接池、endpoint、鉴权头、SSE 帧提取(`SseFramer`);方言差异封闭在 `ProviderAdapter` 之后的两层——**通用引擎**(`GenericChatAdapter` / `GenericResponsesAdapter` / `GenericAnthropicAdapter`:各 wire 形态的兼容 provider 共有部分——body 骨架、消息翻译、流解析——只写一次)+ **Ext 差异点 trait**(`ChatExt` / `ResponsesExt` / `AnthropicExt`:声明式常量 + 序列化钩子;分层形态吸收 rig-core 的 Generic<Ext> 设计,不引库)。每个 adapter 提供 `build_request`(内部方言 → wire)与 `FrameMapper`(帧 → 流事件,每请求一个、可带跨帧状态)。

五个方言,按 wire 形态归入三个通用引擎:

| 方言 | 引擎 | 差异点 |
|---|---|---|
| deepseek-responses(默认) | GenericResponsesAdapter | effort → `reasoning:{effort}`(照官方文档,与 OpenAI 同形) |
| openai-responses | GenericResponsesAdapter | 同上 |
| anthropic | GenericAnthropicAdapter | x-api-key+version、max_tokens |
| deepseek-chat | GenericChatAdapter | `thinking:{type}` + `reasoning_effort` 两顶层字段 |
| openai-chat | GenericChatAdapter | 净版无私参 |

方言差异对照:

| 差异点 | chat 系(deepseek-chat / openai-chat) | anthropic | responses 系(deepseek-responses / openai-responses) |
|---|---|---|---|
| system | messages 首条 | 顶层 `system` | `instructions` |
| tools | `{type, function{...}}` 嵌套 | 扁平 `{name, input_schema}` | 扁平 `{name, parameters}` |
| 工具调用(上行) | `tool_calls[].function.*` | `tool_use` block | `function_call` item |
| 工具结果(上行) | `{role:tool, tool_call_id, content}` | `tool_result` block | `function_call_output` item |
| 流式工具增量 | `delta.tool_calls`(index) | `input_json_delta`(block) | `function_call_arguments.delta`(item) |
| 流终止 | finish_reason / [DONE] | `message_stop` | `response.completed` 定稿 |

`ProviderAdapter` 契约:

```rust
trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn endpoint(&self) -> &'static str;                       // 相对 base_url
    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)>;
    fn build_request(&self, header: &RequestHeader, messages: &Value) -> Value;
    fn mapper(&self) -> Box<dyn FrameMapper>;
}
```

### 7.6 WIT 契约层

| 包 | 内容 |
|---|---|
| dsh:json | `type json = list<u8>` 字节背板(组件模型无递归类型) |
| dsh:session | event-log 四函数 + projection 三件套 |
| dsh:loop | driver |
| dsh:events | 总线五模式 + 续体 resource |
| dsh:host | process / cancel / registry / llm-transport(`%stream`、register-adapter)/ telemetry —— llm-transport 宿主实现在 dsh-llm,process 实现在 dsh-sandbox;WIT 契约零变更 |
| dsh:plugin | world:consumer 为 export + lifecycle(init / dispose / config-schema) |
| dsh:tools | world tool-component:lifecycle + tools(describe / execute);零 import(world 即授权面:纯 json→json,wasi/cancel/进程/LLM 面不可达);宿主桥 `WasmTool`,preset 路径型 mount 行装载 |

dsh-session 以 wasm32-wasip2 产物交付;外部工具组件同样以 wasm32-wasip2 交付(`dsh-example-tool` 为参考实现);loop / prompt / 续体 resource 的组件化以「cargo 出现 0.3 目标 + wit-bindgen stream/future 稳定」为触发条件。

### 7.7 总线与插件

分发模式:emit(触发即忘)、parallel(并发 join)、serial(依次,首个 bail 值短路)、bail(命中即止)、waterfall(链式传载荷;veto 为其内建语义)。

around 续体:waterfall listener 收 `(payload, Next)`;`Next.invoke` 继续链,链尾为内置默认行为;不调用即替换或否决。形态:transform(改载荷后调 Next)、替换(不调 Next,默认行为零调用)、包裹(多次调 Next / 外叠逻辑)。

插件生命周期:`running → disposing → destroyed`;销毁按注册逆序;单个失败被包含;disposing 期拒绝注册;订阅随销毁退订。配置经 JSON Schema 子集校验。契约细节见 [plugin-sdk.md](plugin-sdk.md)。

### 7.8 沙箱与进程

本子系统位于 `dsh-sandbox`。执行侧语义组织:策略形状(3 态 mode)、根推导单源、功能式探测与缓存、enforcement / 拒绝方言 / runner 失败分类、stderr 收集。

**策略形状(mode 3 态 + 根推导单源)**:`SandboxMode{ ReadOnly, WorkspaceWrite, FullAccess }`(与 dsh-core permission.rs 字符串命名一致);可写根经 `SandboxPolicy::writable_roots()` 单源推导——ReadOnly → 无;WorkspaceWrite → {workspace 根, `/tmp`, 平台 temp_dir}(canonicalize + 去重;bwrap 下临时区以隔离 `--tmpfs` 挂载,workspace 根 `--bind`);FullAccess → `/`。装配语义:read-only 下 bash 工具不装配;workspace-write 为默认;full-access 由权限切换触发(可写根 `/` 但**仍要求可用 rung**——不做绕过沙箱的语义)。

**沙箱链探测(功能式 + 缓存)**:

| 平台 | rung | 机制 | 探测 |
|---|---|---|---|
| Linux | bwrap | argv 包装 | 功能式(read-only profile 真跑 `true`) |
| Linux(无 bwrap) | landlock | pre_exec 自限制 | ABI 探测 |
| macOS | seatbelt | argv 包装 | 功能式(read-only SBPL 真跑 `true`;subpath 需 canonicalize) |
| Windows | 未实现 | fail-closed 拒绝 | — |

探测一次缓存为 rung(仅成功才缓存,`set_disabled_for_tests` 测试缝保留);探测失败且策略要求沙箱 → 拒绝执行(fail-closed,绝不静默降级为无沙箱)。

**Confined 元数据与退出分类**:argv 包装返回 `Confined{program, argv, enforcement, denial_dialect, runner_failure_rules}`——enforcement(full / partial 完整性声明)、拒绝方言(bwrap `read-only file system` / landlock `permission denied` / seatbelt `operation not permitted`)、runner 失败规则(命令未执行的判别:bwrap / seatbelt fatal 签名)。spawn 后 stderr 经 drain 任务读至 EOF(修复「stderr piped 无人读 → 子进程大 stderr 堵塞」;`stderr_text` await 收尾,分类/落盘无缺尾);退出分类序 = runner 失败(命令从未执行)→ 拒绝(执行了但被内核拦)→ 常规退出(退出码是结果数据、非执行失败)。工具结果呈现:RunnerFailed → 「sandbox runner 失败(命令未执行)」+ 命中行;Denied → `[sandbox: file access denied under <mode> mode]`+ stderr 原文——均走既有 `tool/result` 字段,不加新事件(single boundary rule)。

**进程**:独立进程组,SIGTERM → grace → SIGKILL;PTY(portable-pty)不暴露 pre_exec,沙箱仅经 argv 包装,landlock-only 系统拒绝 PTY 执行(fail-closed)。

### 7.9 遥测

span 为日志投影:turn/step → 区间 span(span_id = 起始事件 seq);audit/call → 点 span(属性带归因链);trace_id = 日志首事件 + 格式版本的稳定哈希。同一日志的导出与重放后导出 bit-exact;OTLP wire 分层接入,不触及派生逻辑。

### 7.10 扩展点

一切皆插件的落点:每类能力是一个契约 + 注册,核心对具体实现无感知。

| 能力 | 契约 | 注册形态 | 替换示例 |
|---|---|---|---|
| provider 方言 | `ProviderAdapter` + `FrameMapper`;chat/responses/anthropic 通用引擎 + Ext 差异点 | `adapter_by_name`(配置 dialect 选择) | 新增兼容 provider = 新增(或复用)Ext + 契约测试;新 wire 形态 = 新增通用引擎 + Ext |
| 工具 | `ToolPort`(specs + execute) | `ToolSet` 名字分发,装配期注入引擎;在树组件注册表(dsh-app `mount`)+ 外部 wasm 工具组件(`dsh:tools` world,`WasmTool` 桥) | 新工具不触碰引擎与循环;外部组件零代码接入 |
| 插件 | 总线订阅 + lifecycle(init / dispose / config-schema) | `PluginRegistry` 注册,逆序销毁 | 重试 / 回放 / title 均为插件 |
| 日志后端 | `LogBackend` | 装配期选择 | JSONL ⇄ SQLite(turso) |
| 传输 | `LlmTransport` | 闸门包裹装配 | HTTP ⇄ 假 provider ⇄ 总线传输 |
| prompt 策略 | `assemble(ctx)` 纯函数 | 装配期组装(AGENTS.md ≤64KB 注入;plan 态/active-plan 自日志每 turn 重组;persona mount 行可覆盖 identity/追加段) | 计划模式 / compaction 策略 |
| preset 组合 | `PresetManifest`(k8s 形态 YAML) | 装配期加载(内置 include_str + workspace `presets/` 覆盖;mount 行 = 在树组件 / wasm 路径 / OCI 引用) | 复制改一份 manifest 即增删能力,零代码 |

### 7.11 配置

两级装配输入 + 运行时设置层:**CLI > `dsh.toml`(工作区,只读装配输入)> `~/.dshrs/settings.json`(用户级设置存储,setter 落盘目标)> 内置默认**;会话内存覆盖最高(重启即回工作区默认)。`dsh.toml` 字段 model / base_url / session / workspace / dialect / preset;dialect 未知值装配期拒绝。CLI 开关:`--pty` / `--no-tools` / `--fake` / `--preset <id>`。设置层承载:onboarding 完成态、provider 注册表(id/base_url/dialect/凭据引用/默认模型)、工作区级默认(provider/model/permission/preset/effort,projectKey 键控);损坏文件旁置备份后回落内置默认(不拒启)。凭据四级链:显式注入 > 引用(`env:`/`dotenv:`/`keychain:`)> 默认环境变量名(env > 工作区 `.env`)> 钥匙串默认槽(`dsh/<provider>`,仅 macOS;其余平台该级缺席);设置只存引用,明文永不落 settings.json。

preset = k8s 形态 YAML manifest:`presets/<id>.yaml`(apiVersion: dsh/v1 / kind: Preset / metadata{name, displayName, description} / spec.mounts 装配清单;`---` 多文档流按 kind 路由、空文档跳过;deny_unknown_fields;metadata.name 必须与文件名 stem 一致)。内置 standard / minimal 经 `include_str!` 随二进制;workspace 同名文件覆盖内置。mount 行 source 三态:在树组件注册名 / 本地 wasm 路径(相对 workspace,`dsh:tools` 组件经 `WasmTool` 装载)/ OCI 引用(格式容纳,拉取随分发面启用);config 按组件 config-schema 校验后透传。分界:preset 只选模型面——沙箱、持久化、provider 路由、registry 永远留在宿主面。


## 8. 质量场景

| 场景 | 期望 | 验证 |
|---|---|---|
| 篡改请求 | 闸门拒绝,调用不发生 | verify-e2e(gate_rejects_tampered_request) |
| 日志重放 | 重建状态与派生 bit-exact | verify-component-contracts(跨实例重放) |
| 审计归因重放 | 归因链与活体一致 | verify-e2e(audit) |
| 沙箱不可用 | 工具执行被拒绝 | verify-e2e(沙箱套件) |
| 取消长工具 | 子进程被杀,turn 温和收尾,REPL 存活 | verify-e2e(cancel) |
| 方言往返 | 三方言请求形状与流映射符合各自 wire 规范 | adapters 契约测试 |
| 未知事件类型 | 读取方拒绝重建 | verify-component-contracts |

## 9. 风险与技术债

| 项 | 影响 | 触发条件 |
|---|---|---|
| OTLP wire 序列化 | 无自建观测后端可推送 | 接入 Jaeger / Tempo 等 |
| turso 崩溃恢复演练 | 数据风险已隔离(JSONL 为事实流) | 任意空档 |
| Windows ACL rung | Windows 无工具执行能力 | 具备 Windows 环境 |
| 0.3 产物切换 + loop/prompt/续体组件化 | 无(契约已按 0.3 形状) | Rust→wasm 工具链成熟 |
| 沙箱内 rustc 崩溃(macOS guard page 分配被拒) | 编译型工作负载(build/test 工具调用)在沙箱内不可用,模型重试浪费 step | 会话要求模型跑 rustc 时(实测) |
| dsh-host 剩余簇单体(bus/engine/arena/persistence/rpc/telemetry/config) | 能力实现与核心同 crate,替换需改核心 | wasm 组件化边界成熟(0.3 工具链) |
| dsh-wit bindgen 仅覆盖 session world | loop/prompt/plugin world 缺宿主调用桩 | loop 组件化触发条件(0.3 工具链) |
| escalation 加宽与拒绝提示链(后置) | 模型在沙箱拒绝处只能换路径,无「加宽 + 审批先于执行」升级通道(permission.rs 明示模型不得请求升级) | 需宿主审批执行通道(现存 plan 审批 / ask_user_question 之外另立,单独 PR) |

## 10. 术语表

| 术语 | 定义 |
|---|---|
| 事实流 | JSONL 事件日志;会话状态的唯一权威记录 |
| surface 事件 | 进入模型消息面的三类事件(user/message、assistant/message、tool/result) |
| 归因集 | 允许携带 sourceEventSeqs 的事件集合(surface 三类 + audit/call) |
| 派生面 | `derive_messages`:事件序列 → 模型可见消息的唯一映射 |
| 闸门 | InvariantGate:出网边界的内容级不变式校验器 |
| 竞技场 | 一次组件实例化的隔离环境(独立 Store + epoch 域),可整体销毁重建 |
| 能力束 | 执行世界的数据形态:cwd + 可写根 + 取消令牌 |
| rung | 沙箱链中某平台的可用机制(bwrap / landlock / seatbelt) |
| sink | 事件 append 后同步触发的消费者(落盘 / 下行 / 渲染 / 遥测) |
| 记录优先 | 事件先落日志并触发 sink,再进入后续处理 |
| 方言 | provider 的 wire 协议差异,封闭于 ProviderAdapter |
| 下行通道 | 网关中事件通知与响应的有序输出通道,与传输无关 |
| 续体 | waterfall listener 持有的 Next;调用继续链,不调用即替换/否决 |
