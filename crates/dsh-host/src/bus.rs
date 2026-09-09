//! 宿主事件总线:5 种分发模式 + around 续体。
//!
//! 语义按 DispatchMode 一一对应:
//! - `emit` 不等结果;`parallel` 并发 join;`serial` 依次 await 且 bail 值短路;
//!   `bail` 首个非 null/false/undefined 返回值即止;`waterfall` 依次传载荷。
//! - veto 不是第 6 种模式,是 waterfall 的内建语义:listener 不调续体即否决。
//!
//! around 续体(native 形态):waterfall listener 收到 `(payload, Next)`;
//! 调 `Next` 继续链(含内置默认行为),不调即否决/替换——
//! `llm/stream`、`tools/execute` 的包裹语义(超时/重试/回放替换)由此表达。
//! WIT resource 形态的映射:Next ↔ 续体 resource,listener ↔ consumer.handle。
//!
//! 错误包含:listener 级——单个 listener 失败被单独捕获,
//! 不吞同事件其余 listener、不阻断分发(per-observer 语义)。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use thiserror::Error;

/// 盒装异步(订阅者闭包的统一返回形态)
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 普通 listener 的返回值(transform 类事件)
#[derive(Debug, Clone, PartialEq)]
pub enum ListenerResult {
    /// 无返回值(emit/parallel);或「继续、无意见」(serial/bail)
    Continue,
    /// 携带结果(serial 短路 / bail 命中)
    Value(Value),
}

/// 普通 listener(emit/parallel/serial/bail);错误经 Err 传达(单独包含)
pub type Listener = Arc<dyn Fn(Value) -> BoxFuture<Result<ListenerResult, String>> + Send + Sync>;

/// 带优先级的普通订阅(priority, 订阅序, listener)
type PriorityListener = (i32, u64, Listener);
/// around listener(waterfall):收到载荷与续体;返回最终值或停止
pub type Around = Arc<dyn Fn(Value, Next) -> BoxFuture<Result<Value, Stop>> + Send + Sync>;

/// waterfall 的停止方式
#[derive(Debug, Error, PartialEq)]
pub enum Stop {
    /// 否决:不调续体,链中止(向调用方返回否决信号)
    #[error("vetoed")]
    Veto,
    /// listener 错误(单独包含,不阻断其它 listener)
    #[error("listener error: {0}")]
    Error(String),
}

/// waterfall 续体:调用即执行链上剩余部分(最后一个 listener 之后是内置默认行为)
#[derive(Clone)]
pub struct Next {
    chain: Arc<ChainInner>,
    idx: usize,
}

struct ChainInner {
    event: String,
    listeners: Vec<PriorityAround>,
    default: Box<dyn Fn(Value) -> BoxFuture<Result<Value, String>> + Send + Sync>,
}

/// 优先级包装(高优先级先执行;同优先级按订阅序)
#[derive(Clone)]
struct PriorityAround {
    priority: i32,
    seq: u64,
    listener: Around,
}

impl Next {
    /// 继续链:剩余 listener(或默认行为)以新载荷执行
    pub async fn invoke(&self, payload: Value) -> Result<Value, Stop> {
        run_chain(&self.chain, self.idx, payload).await
    }
}

async fn run_chain(chain: &Arc<ChainInner>, idx: usize, payload: Value) -> Result<Value, Stop> {
    let Some(entry) = chain.listeners.get(idx) else {
        // 链尾:内置默认行为(接收链上传来的最终载荷)
        return (chain.default)(payload)
            .await
            .map_err(|e| Stop::Error(format!("default: {e}")));
    };
    let listener = Arc::clone(&entry.listener);
    let next = Next {
        chain: Arc::clone(chain),
        idx: idx + 1,
    };
    // listener 级错误包含:panic 与 Err 都不波及链的其余部分,
    // 但当前 listener 的失败即本次调用失败(waterfall 的顺序依赖使然)
    listener(payload, next)
        .await
        .map_err(|e| map_stop(e, &chain.event))
}

fn map_stop(stop: Stop, event: &str) -> Stop {
    match stop {
        Stop::Error(e) => Stop::Error(format!("{event}: {e}")),
        other => other,
    }
}

/// 事件总线(订阅是数据;按 (priority, 订阅序) 排序)
#[derive(Default)]
pub struct EventBus {
    listeners: Mutex<HashMap<String, Vec<PriorityListener>>>,
    around: Mutex<HashMap<String, Vec<PriorityAround>>>,
    seq: Mutex<u64>,
}

impl EventBus {
    /// 空总线
    pub fn new() -> Self {
        Self::default()
    }

    fn next_seq(&self) -> u64 {
        let mut seq = self.seq.lock().expect("bus seq 锁中毒(宿主 bug)");
        *seq += 1;
        *seq
    }

    /// 订阅普通事件(emit/parallel/serial/bail)
    pub fn subscribe(&self, event: &str, priority: i32, listener: Listener) -> u64 {
        let seq = self.next_seq();
        self.listeners
            .lock()
            .expect("bus listeners 锁中毒(宿主 bug)")
            .entry(event.to_string())
            .or_default()
            .push((priority, seq, listener));
        seq
    }

    /// 退订(按事件名 + 订阅 id;插件销毁时逆序回收)
    pub fn unsubscribe(&self, event: &str, id: u64) {
        if let Some(v) = self
            .listeners
            .lock()
            .expect("bus listeners 锁中毒(宿主 bug)")
            .get_mut(event)
        {
            v.retain(|(_, seq, _)| *seq != id);
        }
        if let Some(v) = self
            .around
            .lock()
            .expect("bus around 锁中毒(宿主 bug)")
            .get_mut(event)
        {
            v.retain(|e| e.seq != id);
        }
    }

    /// 订阅 around 事件(waterfall + 续体)
    pub fn subscribe_around(&self, event: &str, priority: i32, listener: Around) -> u64 {
        let seq = self.next_seq();
        self.around
            .lock()
            .expect("bus around 锁中毒(宿主 bug)")
            .entry(event.to_string())
            .or_default()
            .push(PriorityAround {
                priority,
                seq,
                listener,
            });
        seq
    }

    /// 按序取 listener:(priority 降序, 订阅序升序)
    fn ordered(&self, event: &str) -> Vec<PriorityListener> {
        let map = self.listeners.lock().expect("bus 锁中毒(宿主 bug)");
        let mut v: Vec<_> = map.get(event).cloned().unwrap_or_default();
        v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        v
    }

    /// emit:不等结果;listener 错误包含(不传播)
    pub async fn emit(&self, event: &str, payload: Value) {
        for (_, _, listener) in self.ordered(event) {
            let _ = listener(payload.clone()).await;
        }
    }

    /// parallel:并发执行全部;错误收集(不中断同伴)
    pub async fn parallel(
        &self,
        event: &str,
        payload: Value,
    ) -> Vec<Result<ListenerResult, String>> {
        let futures: Vec<_> = self
            .ordered(event)
            .into_iter()
            .map(|(_, _, l)| l(payload.clone()))
            .collect();
        futures::future::join_all(futures).await
    }

    /// serial:依次 await;首个携带值(Value)即短路返回
    pub async fn serial(&self, event: &str, payload: Value) -> Option<Value> {
        for (_, _, listener) in self.ordered(event) {
            match listener(payload.clone()).await {
                Ok(ListenerResult::Value(v)) => return Some(v),
                Ok(ListenerResult::Continue) => {}
                Err(e) => {
                    // listener 级包含:记录并继续(失败不阻断链)
                    eprintln!("serial listener error (contained): {e}");
                }
            }
        }
        None
    }

    /// waterfall(around + 续体):payload 依次穿过 listener,链尾是默认行为。
    ///
    /// listener 不调 `Next` 而直接返回 → 替换/否决剩余链;
    /// 返回 [`Stop::Veto`] → 向调用方传达否决。
    pub async fn waterfall<F>(&self, event: &str, payload: Value, default: F) -> Result<Value, Stop>
    where
        F: Fn(Value) -> BoxFuture<Result<Value, String>> + Send + Sync + 'static,
    {
        let mut listeners: Vec<PriorityAround> = self
            .around
            .lock()
            .expect("bus around 锁中毒(宿主 bug)")
            .get(event)
            .cloned()
            .unwrap_or_default();
        listeners.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.seq.cmp(&b.seq)));
        let chain = Arc::new(ChainInner {
            event: event.to_string(),
            listeners,
            default: Box::new(default),
        });
        run_chain(&chain, 0, payload).await
    }
}
