//! `dsh` 宿主二进制:CLI 入口(薄壳)。
//!
//! 装配在 [`dsh_app`](https://docs.rs/ 库形态):配置合并、prompt 组装、
//! transport 构建、preset 驱动的工具组装、Session(turn 驱动)。
//! 本文件只做 CLI 解析、终端渲染(REPL/Reporter)与 stdio 网关装配。
//! 组件回路(Engine → InstancePre → Arena → 组件调用)保留为 version/info 电路。

use clap::{Args, Parser, Subcommand};
use dsh_agent_loop::{LlmEvent, ToolPort, ToolSet, TurnOutcome};
use dsh_app as app;
use dsh_app::{ResolveArgs, Resolved, Session};
use dsh_host::HostEngine;
use dsh_llm::{FakeProvider, InvariantGate};
use dsh_session::EventEnvelope;

#[derive(Parser)]
#[command(
    name = "dsh",
    version,
    about = "deepseek-harness-rs — Rust + WASM Component Agent Harness"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

/// chat/serve 共用的连接与装配参数(合并优先级 CLI > dsh.toml > 默认)
#[derive(Args)]
struct CommonOpts {
    /// 模型标识(HTTP 模式;fake 模式仅记录)
    #[arg(long)]
    model: Option<String>,
    /// API base URL(HTTP 模式)
    #[arg(long)]
    base_url: Option<String>,
    /// API key(HTTP 模式;缺省读 DEEPSEEK_API_KEY)
    #[arg(long)]
    api_key: Option<String>,
    /// 假 provider(脚本化回声,不联网;自检/演示用)
    #[arg(long)]
    fake: bool,
    /// 会话日志路径(追加式 JSONL)
    #[arg(long)]
    session: Option<String>,
    /// 配置文件路径(默认 dsh.toml,存在才读)
    #[arg(long)]
    config: Option<String>,
    /// 工作目录 = 沙箱可写根 = 工具 cwd(默认当前目录)
    #[arg(long)]
    workspace: Option<String>,
    /// 禁用工具(纯对话;fake 模式恒无工具)
    #[arg(long)]
    no_tools: bool,
    /// provider 方言(openai-chat / anthropic / openai-responses)
    #[arg(long)]
    dialect: Option<String>,
    /// bash 工具走 PTY(终端语义:isatty/彩色;沙箱经 argv 包装)
    #[arg(long)]
    pty: bool,
    /// 能力 preset(standard / minimal / <workspace>/presets/<id>.yaml)
    #[arg(long)]
    preset: Option<String>,
    /// 推理等级(low / high / max;缺省 = provider 默认)
    #[arg(long)]
    reasoning_effort: Option<String>,
}

impl CommonOpts {
    /// 配置合并(CLI > dsh.toml > 默认;preset 加载失败即拒绝)
    fn resolve(&self) -> anyhow::Result<Resolved> {
        Resolved::resolve(
            ResolveArgs {
                model: self.model.clone(),
                base_url: self.base_url.clone(),
                session: self.session.clone(),
                workspace: self.workspace.clone(),
                dialect: self.dialect.clone(),
                preset: self.preset.clone(),
                reasoning_effort: self.reasoning_effort.clone(),
                models: None,
            },
            std::path::Path::new(self.config.as_deref().unwrap_or("dsh.toml")),
        )
    }
}

#[derive(Subcommand)]
enum Commands {
    /// 经组件调用链打印版本(出口验证)
    Info,
    /// 对话:带 message 为单轮;缺省进入交互式 REPL(流式输出)
    Chat {
        /// 用户输入(缺省进入 REPL)
        message: Option<String>,
        #[command(flatten)]
        common: CommonOpts,
    },
    /// JSON-RPC 2.0 网关(stdio,行分帧):turn/log/attribution/status/cancel/
    /// mode/approve/shutdown,事件以 `event` 通知下行(通道抽象与传输无关)
    Serve {
        #[command(flatten)]
        common: CommonOpts,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None | Some(Commands::Info) => {
            let version = component_version().await?;
            println!("dsh {version} (host + component circuit)");
        }
        Some(Commands::Chat { message, common }) => {
            chat(message, common).await?;
        }
        Some(Commands::Serve { common }) => {
            serve(common).await?;
        }
    }
    Ok(())
}

/// 组件回路:Engine → InstancePre → Arena → 组件函数调用。
async fn component_version() -> anyhow::Result<String> {
    let engine = HostEngine::new()?;
    engine.register("hello", dsh_host::hello::HELLO_WAT.as_bytes())?;
    let mut arena = dsh_host::Arena::new(&engine, "hello").await?;
    let instance = *arena.instance("hello")?;
    let hello = dsh_host::hello::Hello::new(arena.store_mut(), &instance)?;
    let v = hello.call_version(arena.store_mut())?;
    Ok(dsh_host::hello::decode_semver(v))
}

/// 对话回路:组装 → 闸门 → transport → 工具 → 记录优先落盘。
/// 带 message 单轮;缺省进入 REPL(跨 turn 共享日志,流式终端输出)。
async fn chat(message: Option<String>, common: CommonOpts) -> anyhow::Result<()> {
    let resolved = common.resolve()?;
    let parts = app::prompt_parts(&resolved, false);
    let backend = app::open_backend(&resolved.session)?;
    let session_path = resolved.session.clone();
    let cancel = dsh_agent_loop::CancelToken::new();

    if common.fake {
        let mut provider = FakeProvider::new();
        provider.then(vec![
            LlmEvent::Chunk("echo: ".into()),
            LlmEvent::AssistantMessage(serde_json::json!({ "content": "echo (fake)" })),
            LlmEvent::Done,
        ]);
        let gate = InvariantGate::new(provider, app::fresh_log());
        let log = gate.log();
        let session = Session::new(
            parts,
            gate,
            log,
            dsh_agent_loop::NoTools,
            backend,
            session_path,
            cancel,
        );
        dispatch(session, message).await?;
    } else if common.no_tools {
        let api_key = Resolved::resolve_api_key(common.api_key.clone())?;
        let gate = InvariantGate::new(
            app::build_raw_transport(&resolved, &api_key, None)?,
            app::fresh_log(),
        );
        let log = gate.log();
        let session = Session::new(
            parts,
            gate,
            log,
            dsh_agent_loop::NoTools,
            backend,
            session_path,
            cancel,
        );
        dispatch(session, message).await?;
    } else {
        // 工具集按 preset 声明式组装(dsh-app):preset 决定模型面,
        // 宿主面(沙箱/持久化/路由)不受影响
        let api_key = Resolved::resolve_api_key(common.api_key.clone())?;
        let gate = InvariantGate::new(
            app::build_raw_transport(&resolved, &api_key, None)?,
            app::fresh_log(),
        );
        let log = gate.log();
        let tools = app::build_tools(
            &resolved,
            &api_key,
            &log,
            &cancel,
            common.pty,
            "workspace-write",
            // CLI 静态装配:无动态权限源(单会话、权限固定)
            None,
            // CLI 单会话形态无 AppHost 检索/问答/会话工厂/结算通知面
            // (session_query/ask_user_question/subagent 会话化与后台通知为
            // 桌面/多会话宿主能力)——显式缺省
            None,
            None,
            None,
            None,
            None,
            None,
        )?;
        let session = Session::new(parts, gate, log, tools, backend, session_path, cancel);
        dispatch(session, message).await?;
    }
    Ok(())
}

async fn dispatch<T, TOOLS>(
    mut session: Session<T, TOOLS>,
    message: Option<String>,
) -> anyhow::Result<()>
where
    T: dsh_agent_loop::LlmTransport + dsh_agent_loop::Summarizer + Send,
    TOOLS: ToolPort + Send,
{
    match message {
        Some(m) => {
            turn_with_report(&mut session, &m).await?;
        }
        None => repl(&mut session).await?,
    }
    Ok(())
}

/// 驱动一个 turn:记录优先(落盘 + 终端流式渲染)+ TTFT 度量
async fn turn_with_report<T, TOOLS>(
    session: &mut Session<T, TOOLS>,
    input: &str,
) -> anyhow::Result<TurnOutcome>
where
    T: dsh_agent_loop::LlmTransport + dsh_agent_loop::Summarizer + Send,
    TOOLS: ToolPort + Send,
{
    let started = std::time::Instant::now();
    let mut reporter = Reporter {
        started,
        ttft: None,
        streamed: 0,
    };
    let mut sink_events = 0usize;
    let outcome = session
        .turn_with(input, None, &[], &[], &mut |ev: &EventEnvelope| {
            sink_events += 1;
            reporter.on_event(ev);
        })
        .await?;
    reporter.finish(&outcome.assistant_message);
    eprintln!(
        "[turn {}..{} · {sink_events} events · TTFT {} → {}]",
        outcome.seq_range.0,
        outcome.seq_range.1,
        reporter.ttft_label(),
        session.session_path()
    );
    Ok(outcome)
}

/// 交互式 REPL:逐行读入,每行一个 turn;exit/quit/Ctrl-D 退出;
/// Ctrl-C 软取消当前 turn(安全点生效:出网返回/step 边界/工具前后)
async fn repl<T, TOOLS>(session: &mut Session<T, TOOLS>) -> anyhow::Result<()>
where
    T: dsh_agent_loop::LlmTransport + dsh_agent_loop::Summarizer + Send,
    TOOLS: ToolPort + Send,
{
    println!(
        "dsh repl · exit/quit 退出 · Ctrl-C 取消当前 turn · /plan 计划模式 · 会话日志 {}",
        session.session_path()
    );

    // SIGINT → 软取消(替换默认的进程终止;安装后即全进程生效)
    let cancel = session.cancel_token();
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            cancel.cancel();
            eprintln!("^C");
        }
    });

    // stdin 独立线程:阻塞读不占用异步执行器
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(4);
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if line_tx.blocking_send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    while let Some(line) = line_rx.recv().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if matches!(trimmed, "exit" | "quit" | "/exit" | "/quit") {
            break;
        }
        match trimmed {
            "/plan" => {
                session.session_event("session/mode", serde_json::json!({ "mode": "plan" }))?;
                println!("[plan mode] 探索只读;模型经 exit_plan_mode 提交计划");
                continue;
            }
            "/standard" => {
                session.session_event("session/mode", serde_json::json!({ "mode": "standard" }))?;
                println!("[standard mode]");
                continue;
            }
            "/approve" => match session.pending_plan() {
                Some(plan) => {
                    session.session_event("plan/approved", serde_json::json!({ "plan": plan }))?;
                    session
                        .session_event("session/mode", serde_json::json!({ "mode": "standard" }))?;
                    println!("[approved]\n{plan}\n[标准模式;计划已注入 prompt]");
                }
                None => println!("没有待批准的计划(模型尚未经 exit_plan_mode 提交)"),
            },
            _ => {}
        }
        match turn_with_report(session, trimmed).await {
            Ok(_) => {}
            // 软取消:turn 已温和收尾(turn/end 已记录),REPL 继续
            Err(e) if e.to_string().contains("cancelled") => {
                println!("[cancelled]");
            }
            Err(e) => return Err(e),
        }
        // 模型提交了计划:提示批准入口
        if let Some(plan) = session.pending_plan() {
            println!("\n[计划待批准]\n{plan}\n/approve 批准 · /standard 不批准退出计划态");
        }
    }
    Ok(())
}

/// 终端流式渲染器:记录优先的事件流 → 终端。
///
/// chunk 事件落日志即打印(记录 ⟺ 显示);TTFT = turn 起点到首个
/// assistant/chunk 的墙钟距离;工具往返以摘要行走 stderr。
struct Reporter {
    started: std::time::Instant,
    /// 首个 chunk 的到达时刻(None = 本 turn 无 chunk)
    ttft: Option<std::time::Duration>,
    /// 已流式打印的字符数(0 = 无 chunk,收尾改打完整消息)
    streamed: usize,
}

impl Reporter {
    fn on_event(&mut self, ev: &EventEnvelope) {
        use std::io::Write;
        match ev.r#type.as_str() {
            "assistant/chunk" => {
                if self.ttft.is_none() {
                    self.ttft = Some(self.started.elapsed());
                }
                if let Some(delta) = ev.data["delta"].as_str() {
                    self.streamed += delta.chars().count();
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                }
            }
            "tool/call" => {
                eprintln!(
                    "\n[tool] {} {}",
                    ev.data["name"].as_str().unwrap_or("?"),
                    ev.data["arguments"]
                );
            }
            "tool/result" => {
                let success = ev.data["success"].as_bool().unwrap_or(false);
                let output = ev.data["output"].as_str().unwrap_or_default();
                let brief: String = output
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(120)
                    .collect();
                eprintln!("[tool {}] {brief}", if success { "✓" } else { "✗" });
            }
            _ => {}
        }
    }

    /// turn 收尾:流式打印过的补换行;未流式(无 chunk)打印完整消息
    fn finish(&self, assistant_message: &str) {
        if self.streamed > 0 {
            println!();
        } else {
            println!("{assistant_message}");
        }
    }

    fn ttft_label(&self) -> String {
        self.ttft
            .map(|d| format!("{d:?}"))
            .unwrap_or_else(|| "n/a".into())
    }
}

/// JSON-RPC 网关(stdio):装配 Gateway 并进入 serve 循环。
/// 工具集与 chat 同源(preset 驱动,经 dsh-app)。
async fn serve(common: CommonOpts) -> anyhow::Result<()> {
    use dsh_host::rpc::{Gateway, serve_stdio};

    let resolved = common.resolve()?;
    let parts = app::prompt_parts(&resolved, false);
    let header = app::build_header(&parts, &dsh_session::EventLog::new());
    let backend = app::open_backend(&resolved.session)?;
    let cancel = dsh_agent_loop::CancelToken::new();

    // SIGINT → 软取消当前 turn(cancel 方法与 Ctrl-C 同源)
    let sig_cancel = cancel.clone();
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            sig_cancel.cancel();
            eprintln!("^C");
        }
    });

    if common.fake {
        let mut provider = FakeProvider::new();
        provider.then(vec![LlmEvent::AssistantMessage(serde_json::json!({
            "content": "gateway ready (fake provider)"
        }))]);
        let mut gateway: Gateway<FakeProvider> =
            Gateway::new(header, provider, dsh_agent_loop::NoTools, backend);
        gateway.set_cancel_token(cancel);
        serve_stdio(gateway, tokio::io::stdin(), tokio::io::stdout()).await?;
        return Ok(());
    }

    let api_key = Resolved::resolve_api_key(common.api_key.clone())?;
    let transport = app::build_raw_transport(&resolved, &api_key, None)?;

    if common.no_tools {
        let mut gateway: Gateway<dsh_llm::HttpTransport, dsh_agent_loop::NoTools> =
            Gateway::new(header, transport, dsh_agent_loop::NoTools, backend);
        gateway.set_cancel_token(cancel);
        gateway.set_header_rebuilder(app::header_rebuilder(parts));
        serve_stdio(gateway, tokio::io::stdin(), tokio::io::stdout()).await?;
        return Ok(());
    }

    // 工具与网关共享同一日志(todo/plan/goal 状态恢复的期望侧)
    let log = app::fresh_log();
    let tools: ToolSet = app::build_tools(
        &resolved,
        &api_key,
        &log,
        &cancel,
        common.pty,
        "workspace-write",
        // CLI 静态装配:无动态权限源
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )?;
    let mut gateway: Gateway<dsh_llm::HttpTransport, ToolSet> =
        Gateway::with_log(header, transport, tools, backend, log);
    gateway.set_cancel_token(cancel);
    // 每 turn 前按日志态重建 prompt(plan 模式/活跃计划)
    gateway.set_header_rebuilder(app::header_rebuilder(parts));
    serve_stdio(gateway, tokio::io::stdin(), tokio::io::stdout()).await?;
    Ok(())
}
