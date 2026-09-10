//! tokio ↔ GPUI 桥:AppHost 进程内直连。
//!
//! 专任 tokio runtime 承载 AppHost(异步方法/每会话 worker 必须在
//! tokio 上下文);同步方法(std 锁)任意线程直调。下行帧经
//! mux/host 双广播 → futures channel(运行时无关,GPUI 侧可 await)
//! 转发;上行异步 RPC 经 [`HostBridge::call`] 派发,结果 oneshot 回。

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use dsh_core::proto::{DescribeValue, ServerRequest};
use dsh_core::registry::AppHost;
use futures::channel::mpsc;
use futures::channel::oneshot;

/// 宿主桥:持有 runtime(保活)与帧发送端。
pub struct HostBridge {
    host: Arc<AppHost>,
    /// 仅保活:drop 即拆 runtime(worker/帧桥随之终止;关窗退出语义)
    #[allow(dead_code)]
    runtime: tokio::runtime::Runtime,
    /// 仅保活:UI 侧未退出前维持广播转发可达
    #[allow(dead_code)]
    frames_tx: mpsc::UnboundedSender<ServerRequest>,
}

impl HostBridge {
    /// 装配:runtime → AppHost → 模型探测(阻塞)→ 双流订阅 + 基线帧。
    pub fn new(
        workspace: PathBuf,
        fake: bool,
        api_key: &str,
    ) -> anyhow::Result<(Self, mpsc::UnboundedReceiver<ServerRequest>)> {
        Self::new_at(workspace, fake, api_key, None)
    }

    /// 指定会话根构建(测试注入临时根,避免污染 ~/.dshrs)
    #[allow(dead_code)]
    pub fn new_at(
        workspace: PathBuf,
        fake: bool,
        api_key: &str,
        sessions_root: Option<PathBuf>,
    ) -> anyhow::Result<(Self, mpsc::UnboundedReceiver<ServerRequest>)> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?;
        let host = Arc::new(match sessions_root {
            Some(root) => AppHost::new_at(workspace, fake, api_key, root)?,
            None => AppHost::new(workspace, fake, api_key)?,
        });
        // 启动即探测模型清单(attach 前缓存就绪,不落到编造默认模型名)
        runtime.block_on(host.ensure_models());

        let (frames_tx, frames_rx) = mpsc::unbounded();
        // 滞后自愈:广播容量打满(burst > 512)时 recv 返回 Lagged 并丢段,
        // `while let Ok` 会让消费任务**静默死亡**——此后所有帧(含
        // question/requested)永久不到达 UI。Lagged 时拉一次 mux_baseline
        // 重同步未决态(问题/队列快照),直播段内丢的个别事件帧由回合内
        // 后续帧自然覆盖;仅 Closed 才收尾
        let resync_host = host.clone();
        let resync_tx = frames_tx.clone();
        for mut rx in [host.mux_subscribe(), host.host_subscribe()] {
            let tx = frames_tx.clone();
            let resync_host = resync_host.clone();
            let resync_tx = resync_tx.clone();
            runtime.spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(frame) => {
                            if tx.unbounded_send(frame).is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // 丢段:拉当前未决基线重同步,继续消费
                            eprintln!("[host-bridge] 帧流滞后,基线重同步");
                            for frame in resync_host.mux_baseline() {
                                if resync_tx.unbounded_send(frame).is_err() {
                                    break;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        // 订阅基线(未决问题/队列快照;在直播帧语义之前送达)
        for frame in host.mux_baseline() {
            let _ = frames_tx.unbounded_send(frame);
        }
        // 启动即同步 MCP 端口池:存量 enabled 清单恢复连接(设置页保存
        // 即启动;此后 upsert/toggle/remove/import 各自动同步)
        let sync_host = host.clone();
        runtime.spawn(async move { sync_host.sync_mcp_ports() });
        Ok((
            Self {
                host,
                runtime,
                frames_tx,
            },
            frames_rx,
        ))
    }

    /// AppHost 同步方法入口(list/create/cancel/rename/set_* 等)
    pub fn host(&self) -> &Arc<AppHost> {
        &self.host
    }

    /// 专任 runtime 的 spawn 入口(UI 侧异步动作与宿主同池)
    pub fn spawn_on_host<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(async move {
            fut.await;
        });
    }

    /// 异步 RPC 上桥(prompt/history/updateQueue/set_mode):
    /// tokio 上执行,结果经 oneshot 回 GPUI async 上下文 await。
    pub fn call<T, F>(&self, fut: F) -> oneshot::Receiver<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(fut.await);
        });
        rx
    }

    /// host.describe 等价(直连组装;字段来源对齐 `proto` 描述面与 `registry` 各 getter)
    pub fn describe(&self) -> DescribeValue {
        let info = self.host.provider_info();
        DescribeValue {
            version: env!("CARGO_PKG_VERSION").into(),
            cwd: self.host.workspace().display().to_string(),
            provider: Some(info.provider),
            model: Some(info.model),
            models: self.host.models(),
            efforts: self.host.efforts(),
            permissions: self.host.permissions(),
            presets: self.host.presets(),
            workspaces: self.host.workspace_names(),
            attached_sessions: self.host.attached_count() as u64,
            can_open_path: false,
        }
    }
}
