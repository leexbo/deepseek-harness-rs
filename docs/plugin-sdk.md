# dsh 插件 SDK(native 形态)

> 状态:native 形态定稿;wasm 组件插件(`dsh:plugin` world)与
> 工具组件(`dsh:tools` world)的 WIT 映射见文末。
> 本文是插件作者的契约文档:生命周期、订阅、配置、三条硬规则。

## 0. 三条硬规则(违反即 bug)

1. **single boundary rule**:持久可重放事实走 session append(事件日志);
   活体拦截/瞬态信号走总线。**禁止镜像事件**——同一事实不得同时进两侧。
2. **模型可见 ⟺ 已记录**:任何要进模型视野的内容必须先成为日志事件;
   插件改写出网请求时,改动经总线 waterfall 发生在闸门内侧,
   内容级校验(derive-and-compare)仍由宿主强制。
3. **不读时钟/随机**(组件形态强制;native 形态自觉):重放确定性的前提。

## 1. 插件是什么

一个插件 = **配置 + 总线订阅 + 销毁回收**。native 形态下是宿主内的
一个安装函数;wasm 形态下是满足 `dsh:plugin` world 的组件。

```rust
// 最小骨架(完整示例见 crates/dsh-host/tests/plugins.rs 的 TitlePlugin)
pub fn install(bus: &EventBus, config: &Value, state: SharedState) -> u64 {
    validate_config(&Self::schema(), config).expect("配置校验");   // ① 配置先过 schema
    bus.subscribe("user/message", 0, Arc::new(move |payload| {     // ② 订阅声明事件与优先级
        Box::pin(async move {
            // 处理载荷……
            Ok(ListenerResult::Continue)                           // ③ Continue(无意见)或 Value(短路/命中)
        })
    }))
}
```

注册进 `PluginRegistry` 时申报全部订阅,销毁时自动退订:

```rust
registry.register("title", vec![("user/message".to_string(), sub)], Some(dispose))?;
```

## 2. 生命周期

```
running --dispose_all()--> disposing --> destroyed
```

- **销毁按注册逆序**:后装的先拆;
- **错误 per-plugin 包含**:单个插件销毁失败不阻断其余插件的回收
  (失败收集进返回值,不 panic、不饿死同伴);
- **disposing/destroyed 期拒绝新注册**;
- **订阅随插件销毁自动退订**(注册时申报,不靠插件自觉)。

真实初始化(init/config 读取)在注册**之前**完成——注册登记的是
已就绪的插件,不是待初始化的描述符。

## 3. 配置契约

- 插件声明 JSON Schema 子集(`type` / `properties` / `required` / `items`),
  用 `dsh_host::config::validate_config` 校验;
- 类型即校验:缺必填、类型不匹配在安装期拒绝,不带病运行;
- wasm 形态下 `config-schema()` 是组件 export,宿主与 native 插件
  走同一校验路径。

```rust
pub fn schema() -> Value {
    json!({
        "type": "object",
        "required": ["max_attempts"],
        "properties": { "max_attempts": { "type": "number" } },
    })
}
```

## 4. 总线:五模式 + 续体

订阅分两类入口:普通订阅 `bus.subscribe(event, priority, listener)`
(服务 emit/parallel/serial/bail 分发)与 waterfall/around 订阅
`bus.subscribe_around(event, priority, listener)`。分发由宿主按事件性质
选用模式:emit / parallel / serial / bail / waterfall(veto 不是第 6 种
模式,是 waterfall 的内建语义)。

| 模式 | 语义 | 典型用途 |
|---|---|---|
| emit | 触发即忘,不收集返回值 | 状态同步、UI 更新 |
| parallel | 并发 join 全部 listener | 聚合观察 |
| serial | 依次 await,首个 bail 值短路 | 顺序敏感的观察者链 |
| bail | 首个非 null/false 返回值即止 | 命中式查询 |
| waterfall | 链式传递载荷,后手收到前手结果 | 请求改写管线(llm/stream) |

**around 续体**:waterfall 的 listener 收 `(payload, Next)`:

```rust
// ① transform:改载荷后继续
let next_result = next.invoke(transformed).await?;

// ② 替换/否决:不调 next,直接返回替代结果(如回放插件替换整个 LLM 流)
//    或返回 Err(Stop::Veto) 硬否决

// ③ 包裹:调 next 多次(重试),或在外面加计时/指标
```

链尾默认行为:around 链的最后一环之后是**内置默认行为**(如真实
网络传输)——「回放插件替换整个流、默认网络行为零调用」即由此保证
(测试见 `tests/bus.rs::around_replace_skips_rest_and_default`)。

监听器**错误包含**:单个 listener 失败被总线收集,不阻断同事件的
其余 listener。

## 5. 事件面:订阅什么

宿主与 loop 发到总线的事件(`emit`/waterfall/around 的键):

- `user/message`、`assistant/message`(surface 事件镜像到总线的活体信号)
- `llm/stream`(around;出网请求管线,重试/回放插件的挂点)
- `tools/execute`(around;工具执行包裹)
- 工具/进程/LLM 的跨边界调用**同时**以 `audit/call` 事件入日志
  (E5)——插件行为经 sourceEventSeqs 归因,随会话重放可查。

## 6. wasm 组件形态(WIT 映射)

`wit/plugin/plugin.wit` 的 world:

- **export** `consumer`:事件消费(订阅声明为数据,经 lifecycle 交接);
- **export** `lifecycle`:`init(config)` / `dispose()` / `config-schema()`;
- import `dsh:events/*`(总线面)与 `dsh:host/*`(能力面)。

映射关系:

| native | wasm |
|---|---|
| `install(bus, config, state)` | `lifecycle.init(config)` export |
| `PluginRegistry.register(name, subs, dispose)` | 宿主按 world 实例化 + 登记订阅数据 |
| 逆序销毁回调 | `lifecycle.dispose()` 逆序 await |
| `validate_config(&schema(), cfg)` | `lifecycle.config-schema()` + 宿主同路径校验 |
| around 续体 `Next` | `dsh:events` 续体 resource(形状以 wit/ 定稿为准) |

## 7. 工具组件(dsh:tools world)

与总线插件的分工:总线插件在宿主内观察/拦截(事件订阅、请求改写);
**工具组件给模型声明并执行工具**——preset manifest 的 mount 行装载,
零宿主代码接入。

world `tool-component`(`wit/tools/tools.wit`,dsh:tools@0.1.0):

- **export** `dsh:plugin/lifecycle`:`init(config)` / `dispose()` /
  `config-schema()`——与总线插件同一生命周期与配置契约(§3 同一校验路径);
- **export** `tools`:`describe: func() -> list<tool-spec>`
  (name / description / input-schema,单组件可声明多工具)与
  `execute: func(name, input: json) -> result<json, string>`;
- **零 import**:world 即授权面——纯 json→json,wasi(文件/时钟/网络)、
  cancel、进程、LLM 面全部不可达。

装载与运行:

- mount 行 `source: ./my-tool.wasm`(相对 workspace 的本地路径;
  OCI 引用形态已容纳,镜像拉取随分发面启用);
- 装载序 = `config-schema()` → `validate_config`(preset mount 行
  config 透传)→ `init()`(失败装配期 fail-fast)→ `describe()`
  (空声明拒绝);input-schema 即模型面 JSON Schema 子集,宿主组装
  OpenAI function 形状下发;
- execute 在宿主 `spawn_blocking` 中同步调用,同组件串行;取消 =
  宿主放弃等待(工具结果为 cancelled,不硬停组件);死循环由 epoch
  预算硬停兜底(≈10 分钟);
- execute 同步形状是 0.2 产物约束(无 async export);0.3 工具链切换时
  升 `async func` 并 bump 契约版本。

参考实现:`crates/dsh-example-tool`(echo_config 吃 config 最小形态 /
spin 死循环硬停测试物料;rlib + wasm32-wasip2 双产物,
`cargo build -p dsh-example-tool --target wasm32-wasip2`)。

## 8. 参考实现

- `crates/dsh-host/tests/plugins.rs::TitlePlugin` — 完整插件形态:
  schema 校验 → 订阅 → 状态自持 → 注册 → 销毁回收;
- `crates/dsh-host/tests/bus.rs` — 重试(around 包裹)与回放
  (around 替换)插件的语义锁定测试。
