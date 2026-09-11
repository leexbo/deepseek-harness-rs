use super::*;
use crate::features::chat::projection::{ChatNode, ChatState, ToolState};
use crate::shell::host::HostBridge;
use crate::shell::store::AppStore;
use gpui_kit::{AppContext, Bounds, TestAppContext};

fn long_para(seed: usize) -> String {
    let unit = "深度迭代消息流布局验证文本,包含中英混排 English words 与标点!";
    unit.repeat(8 + seed % 5)
}

fn big_md(prefix: &str) -> String {
    format!(
        "{prefix} 段落,**加粗** 与 `code`。{long}\n\n- 甲:{long}\n- 乙:{long}\n\n```rust\nfn main() {{ println!(\"hi\"); }}\n```\n\n| A | B |\n|:--|--:|\n| 1 | 2 |\n\n尾段 {long}\n",
        prefix = prefix,
        long = long_para(3),
    )
}

#[gpui_kit::test]
fn workspace_chat_nodes_do_not_overlap(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_kit::component::init(app);
        crate::kits::theme::init(app);
    });
    allow_host_parking(cx);
    let root = std::env::temp_dir().join(format!("dsh-desktop-test-{}", std::process::id()));
    let (bridge, _rx) = HostBridge::new_at(root.join("ws"), true, "", Some(root.join("sessions")))
        .expect("桥构建失败");

    let assistant_shared = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(usize, String)>::new()));
    let assistant_capture = assistant_shared.clone();
    let node_count = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let node_count_capture = node_count.clone();
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let store = cx.new(|cx| AppStore::new(bridge, cx));
        let id = store
            .read(cx)
            .state
            .current_id
            .clone()
            .expect("启动后有当前会话");
        let mut chat = ChatState::default();
        for i in 0..12usize {
            if i % 3 == 0 {
                chat.nodes.push(ChatNode::User {
                    key: format!("user:{i}"),
                    text: long_para(i),
                    images: Vec::new(),
                });
            } else {
                chat.nodes.push(ChatNode::Assistant {
                    key: format!("a:1:{i}"),
                    text: big_md(&format!("消息{}", i)),
                    reasoning: long_para(i + 1),
                    streaming: false,
                    usage: None,
                    message_id: format!("mid-{i}"),
                });
            }
            if i % 5 == 0 {
                chat.nodes.push(ChatNode::Tool {
                    key: format!("call:{i}"),
                    name: "bash".into(),
                    summary: "ls -la".into(),
                    state: ToolState::Done,
                    arguments: "{\"command\":\"ls -la\"}".into(),
                    output: Some("file-a\nfile-b".into()),
                    view: Some(serde_json::json!({
                        "card": "terminal", "exitCode": 0, "signal": null,
                        "cwd": "/tmp/ws",
                    })),
                });
            }
        }
        chat.nodes.push(ChatNode::TurnTail {
            key: "turn-end:99".into(),
            aborted: false,
            meta: Some("耗时 1.2s".into()),
            deliverables: vec![],
        });
        for (n, node) in chat.nodes.iter().enumerate() {
            if let crate::features::chat::ChatNode::Assistant { text, .. } = node {
                assistant_capture.borrow_mut().push((n, text.clone()));
            }
        }
        node_count_capture.set(chat.nodes.len());
        store.update(cx, |s, _| {
            s.state.chats.insert(id, chat);
        });
        WorkspaceView::new(store, cx)
    });

    // 走窗口自然渲染(refresh):裸 draw 的 request_layout 期无
    // view 上下文,TextView::markdown 的 use_keyed_state 会 panic
    cx.refresh().expect("窗口刷新失败");
    cx.run_until_parked();
    let _ = view;

    // 双胶囊卡分离(主行 padding/gap 回归锚):侧栏卡右缘不得
    // 越过内容卡左缘
    let sb = cx
        .debug_bounds("sidebar-card")
        .expect("sidebar-card bounds 缺失");
    let cc = cx
        .debug_bounds("content-card")
        .expect("content-card bounds 缺失");
    assert!(
        sb.right() <= cc.left() + px(0.5),
        "双卡重叠:sidebar right {:?} > content left {:?}",
        sb.right(),
        cc.left()
    );

    // 虚拟化(gpui list)后仅可视(+overdraw)节点有 bounds:
    // 在场节点依序不重叠;缺席节点跳过(Bottom 对齐下首屏只有
    // 尾部节点在视口,头部节点被虚拟化裁剪是正确行为)。全节点
    // 扫描(种子 16 节点),断言在场 ≥3 且顺序单调
    let total_nodes = node_count.get();
    let mut prev: Option<Bounds<gpui_kit::Pixels>> = None;
    let mut present = 0usize;
    for n in 0..=total_nodes {
        let name = format!("node-{n}").leak() as &'static str;
        let Some(b) = cx.debug_bounds(name) else {
            continue;
        };
        present += 1;
        if let Some(p) = prev {
            assert!(
                b.top() >= p.bottom() - px(0.5),
                "{name} 与上一节点重叠:top {:?} < prev.bottom {:?}",
                b.top(),
                p.bottom()
            );
        }
        prev = Some(b);
    }
    assert!(present >= 3, "虚拟化后应至少有数个节点在场,得到 {present}");
    // 绘制溢出探测:在场 assistant 节点盒高不得低于估算行数所需高度
    // (wrap 测量偏矮时内容将溢出盒子与相邻消息重叠)。
    let col_width: f32 = crate::shell::metrics::chat_col_w(cc.size.width).into();
    let assistant_at = assistant_shared.borrow().clone();
    for (n, text) in assistant_at {
        let Some(b) = cx.debug_bounds(format!("node-{n}").leak()) else {
            continue; // 虚拟化缺席:不在场不探测
        };
        let est_lines: f32 = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let units: f32 = l
                    .chars()
                    .map(|c| if c.is_ascii() { 0.55 } else { 1.0 })
                    .sum();
                (units * 14. / col_width).ceil().max(1.)
            })
            .sum();
        let est_height = est_lines * 22.4 * 0.8; // 留 20% 余量
        assert!(
            b.size.height >= px(est_height),
            "node-{n} 盒高 {:?} 低于估算 {:.0}px(行数 {:.0})——测量偏矮,内容将溢出盒子",
            b.size.height,
            est_height,
            est_lines
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

/// 用户气泡内容自适应:短消息气泡宽度贴合内容(远小于 70% 列宽封顶),
/// 长消息在封顶内 wrap 且盒高足够(不溢出)。回归源截图「📄 justfile」
/// 气泡紧贴内容、非整列拉伸。
#[gpui_kit::test]
fn user_bubble_width_adapts_to_content(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_kit::component::init(app);
        crate::kits::theme::init(app);
    });
    allow_host_parking(cx);
    let root = std::env::temp_dir().join(format!("dsh-desktop-bubble-{}", std::process::id()));
    let (bridge, _rx) = HostBridge::new_at(root.join("ws"), true, "", Some(root.join("sessions")))
        .expect("桥构建失败");

    let (view, cx) = cx.add_window_view(|_window, cx| {
        let store = cx.new(|cx| AppStore::new(bridge, cx));
        let id = store
            .read(cx)
            .state
            .current_id
            .clone()
            .expect("启动后有当前会话");
        let mut chat = ChatState::default();
        // 短消息:单 @ 引用胶囊(源「📄 justfile」同构)→ 应贴合内容
        chat.nodes.push(ChatNode::User {
            key: "user:short".into(),
            text: "@file:justfile".into(),
            images: Vec::new(),
        });
        chat.nodes.push(ChatNode::User {
            key: "user:long".into(),
            text: long_para(4),
            images: Vec::new(),
        });
        store.update(cx, |s, _| {
            s.state.chats.insert(id, chat);
        });
        WorkspaceView::new(store, cx)
    });

    cx.refresh().expect("窗口刷新失败");
    cx.run_until_parked();
    let _ = view;

    // 列宽从内容卡 bounds 推导(与主布局测试同源),封顶气泡宽随列等比
    let cc = cx
        .debug_bounds("content-card")
        .expect("content-card bounds 缺失");
    let col_w: f32 = crate::shell::metrics::chat_col_w(cc.size.width).into();
    let bw = crate::shell::metrics::bubble_w(crate::shell::metrics::chat_col_w(cc.size.width));

    let short = cx.debug_bounds("user-bubble-0").expect("短用户气泡缺失");
    let long = cx.debug_bounds("user-bubble-1").expect("长用户气泡缺失");
    // 短气泡内容垂直居中:唯一内容 = @file 胶囊,其中心应与气泡
    // (含 py10 padding)的内容盒中心对齐——回归「空图块 div + gap(8)
    // 把内容挤出气泡垂直中心」错位。
    let chip = cx
        .debug_bounds("ref-chip-file:justfile")
        .expect("短气泡应含 ref 胶囊");
    let short_center = short.origin.y + short.size.height / 2.;
    let chip_center = chip.origin.y + chip.size.height / 2.;
    assert!(
        (short_center - chip_center).abs() < px(1.),
        "短气泡内容未垂直居中:气泡中心 {:.1} vs 胶囊中心 {:.1}",
        short_center,
        chip_center
    );
    // 短气泡宽应显著小于封顶(贴合内容,不整列拉伸)
    assert!(
        short.size.width < bw * 0.6,
        "短气泡应贴合内容(< 封顶 60%),实际宽 {:?},封顶 {:?}",
        short.size.width,
        bw
    );
    // 长气泡宽应在封顶附近(被 max_w 截断),允许留 padding 余量
    assert!(
        long.size.width <= bw + px(0.5),
        "长气泡不得超封顶 {:?},实际 {:?}",
        bw,
        long.size.width
    );
    // 长气泡盒高足够:多行 wrap 不应溢出盒(测量偏矮会与邻消息重叠)
    let est_lines = long_para(4)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let units: f32 = l
                .chars()
                .map(|c| if c.is_ascii() { 0.55 } else { 1.0 })
                .sum();
            (units * 14. / col_w).ceil().max(1.)
        })
        .sum::<f32>();
    let est_height = est_lines * 22.4 * 0.8;
    assert!(
        long.size.height >= px(est_height),
        "长气泡盒高 {:?} 低于估算 {:.0}px(行数 {:.0})——文本将溢出",
        long.size.height,
        est_height,
        est_lines
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 工具卡(Read)展开后:收起按钮**恒显示**(源 ReadBlock.hidden>0 无条件
/// 渲染,不随展开消失)+ 展开体底部有 Inspect 药丸;点 Inspect 开右栏
/// 轨迹面板标签并打开该 tool 调用的记录检查器(源 ToolRow.inspect →
/// inspectCall)。
#[gpui_kit::test]
fn tool_read_expanded_keeps_collapse_and_inspect_jumps(cx: &mut TestAppContext) {
    use crate::features::chat::projection::{ChatNode, ToolState};
    use crate::features::trajectory::{InspectTarget, TrajectoryView};
    use crate::shell::panel::PanelTab;
    use dsh_core::trajectory::TrajectoryRecord;
    let (store, mut wcx, root) = menu_harness(cx, "read-inspect");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.run_until_parked();
    };

    // 10 行 read 卡(key=call:55;CHAT_CARD_MAX_LINES=8 → hidden=2,有折叠)。
    let lines: Vec<serde_json::Value> = (1..=10)
        .map(|n| serde_json::json!({ "number": n, "text": format!("line {n} of ten") }))
        .collect();
    let read_view = serde_json::json!({
        "card": "read",
        "path": "/ws/src/main.rs",
        "lines": lines,
        "totalLines": 10,
        "lang": "rust",
    });
    let rec = |index: u64, kind: &str, turn: Option<u64>| TrajectoryRecord {
        index,
        seq: index,
        kind: kind.into(),
        turn,
        group: "Step 1".into(),
        turn_start: false,
        text: format!("记录 {index}"),
        result: None,
        is_error: false,
        time_seconds: None,
        started_at: Some(1000),
        request_number: None,
        input: None,
        output: None,
        think: None,
        ttft_ms: None,
        payload: None,
        output_detail: None,
        thinking_detail: None,
        system_prompt: None,
        tools_catalog: None,
        schema_detail: None,
        source: None,
    };
    cx.update(|app| {
        store.update(app, |st, _| {
            let id = st.state.current_id.clone().expect("当前会话");
            let mut chat = crate::features::chat::ChatState::default();
            chat.nodes.push(ChatNode::Tool {
                key: "call:55".into(),
                name: "file_read".into(),
                summary: "src/main.rs".into(),
                state: ToolState::Done,
                arguments: "{\"path\":\"/ws/src/main.rs\"}".into(),
                output: Some("10 行".into()),
                view: Some(read_view),
            });
            st.state.chats.insert(id.clone(), chat);
            // 展开工具卡 + 展开 read 卡(hidden>0 展开态)
            st.chat.expanded_tools.insert("call:55".into());
            st.chat.card_expanded.insert("call:55".into());
            // 轨迹先置空 + 无 loading:模拟「会话在轨迹 tab 打开前缓存为
            // 空白」的真实场景(见 trajectory_tab_entry_repulls_stale_blank_cache)。
            // 此时 inspect_call 同步 find 必落空,必须走延迟定位。
            st.trajectory.trajectory = TrajectoryView {
                records: vec![],
                requests: vec![],
                has_older: false,
                total: 0,
                loading: false,
                loading_older: false,
            };
            st.trajectory.trajectory_session = Some(id);
        });
    });
    redraw(cx, &mut wcx);

    // 展开后收起按钮 + Inspect 药丸都在场
    assert!(
        wcx.debug_bounds("inspect-call:55").is_some(),
        "展开体底部应有 Inspect 药丸"
    );
    assert!(
        wcx.debug_bounds("read-expand-0").is_some(),
        "展开后收起按钮仍应渲染(hidden>0)"
    );

    // Inspect 点击:轨迹缓存为空 → 只开面板标签,记录到位后(延迟定位
    // 收尾)才打开检查器。先验证登记成功但未立即选中。
    cx.update(|app| {
        store.update(app, |st, cx| st.inspect_call("call:55", cx));
    });
    redraw(cx, &mut wcx);
    let (active, insp, pending): (Option<PanelTab>, Option<InspectTarget>, bool) =
        cx.update(|app| {
            let st = store.read(app);
            (
                st.panel_active_tab,
                st.trajectory.inspector,
                st.trajectory.inspect_locate.is_some(),
            )
        });
    assert_eq!(active, Some(PanelTab::Trajectory), "Inspect 开轨迹面板标签");
    assert_eq!(insp, None, "记录未到位,暂不选中(延迟定位)");
    assert!(pending, "待定位 seq 已登记");

    // 轨迹数据到位(模拟 refresh_trajectory 完成回调)→ 延迟定位收尾:
    // 展开所属 turn + 选中记录开检查器。
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.trajectory.trajectory = TrajectoryView {
                records: vec![rec(55, "tool", Some(3))],
                requests: vec![],
                has_older: false,
                total: 1,
                loading: false,
                loading_older: false,
            };
            st.locate_inspect(cx);
        });
    });
    redraw(cx, &mut wcx);
    let insp = cx.update(|app| store.read(app).trajectory.inspector);
    assert_eq!(
        insp,
        Some(InspectTarget::Record(55)),
        "Inspect 应选中 call:55 的记录"
    );
    let _ = std::fs::remove_dir_all(root);
}

fn send_roundtrip(
    cx: &mut TestAppContext,
    fake: bool,
    message: &str,
    seed: Option<&std::path::Path>,
) {
    use futures::StreamExt as _;

    cx.update(|app| {
        gpui_kit::component::init(app);
        crate::kits::theme::init(app);
    });
    allow_host_parking(cx);
    let key = if fake {
        String::new()
    } else {
        // 测试 cwd 在 crate 目录;key 从仓库根 .env 读(与 CLI 同源)
        std::env::set_current_dir(env!("CARGO_MANIFEST_DIR").to_owned() + "/../..")
            .expect("回到仓库根失败");
        dsh_core::Resolved::resolve_api_key(None).expect("api key 解析失败")
    };
    let root = std::env::temp_dir().join(format!(
        "dsh-desktop-send-{}-{}",
        if fake { "fake" } else { "real" },
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let (bridge, frames_rx) =
        HostBridge::new_at(root.join("ws"), fake, &key, Some(root.join("sessions")))
            .expect("桥构建失败");

    // 种子历史:新建会话落位后覆写日志(此时尚未 attach,重载安全)
    let seeded = if let Some(src) = seed {
        let id = bridge.host().create_session(None, None, None);
        let proj_dir = std::fs::read_dir(root.join("sessions"))
            .expect("会话根可读")
            .flatten()
            .find(|e| e.path().is_dir())
            .map(|e| e.path())
            .expect("project 目录存在");
        let target = proj_dir.join(&id).join("session.jsonl");
        std::fs::copy(src, &target).expect("种子日志覆写失败");
        eprintln!("[seed] {src:?} → {target:?}");
        true
    } else {
        false
    };

    let store_cell = std::rc::Rc::new(std::cell::RefCell::new(None::<Entity<AppStore>>));
    let store_capture = store_cell.clone();
    let (view, wcx) = cx.add_window_view(|window, cx| {
        let store = cx.new(|cx| AppStore::new(bridge, cx));
        store.update(cx, |s, cx| {
            s.attach_window_state(window, cx);
        });
        // 帧泵(与 main.rs 同款)
        let pump = store.clone();
        store.update(cx, |_, cx| {
            cx.spawn(async move |_this, cx| {
                let mut rx = frames_rx;
                while let Some(frame) = rx.next().await {
                    pump.update(cx, |s, cx| s.apply_frame(frame, cx));
                }
                Ok::<(), anyhow::Error>(())
            })
            .detach();
        });
        // 新建会话(同步建 + 异步打开;种子场景下首个即种子会话,
        // AppStore::new 已自动打开)
        if !seeded {
            store.update(cx, |s, cx| s.create_session(cx));
        }
        *store_capture.borrow_mut() = Some(store.clone());
        let view = cx.new(|cx| WorkspaceView::new(store.clone(), cx));
        gpui_kit::component::Root::new(view, window, cx)
    });

    wcx.run_until_parked();
    // 真实用户路径:先布局 → 点击输入区聚焦 → 键入 + 回车
    wcx.refresh().expect("窗口刷新失败");
    wcx.run_until_parked();
    let bounds = wcx
        .debug_bounds("composer-hit")
        .expect("composer 输入区 bounds 缺失");
    let center = gpui_kit::Point {
        x: bounds.origin.x + bounds.size.width / 2.,
        y: bounds.origin.y + bounds.size.height / 2.,
    };
    wcx.simulate_click(center, gpui_kit::Modifiers::default());
    wcx.run_until_parked();
    wcx.simulate_input(message);
    wcx.simulate_keystrokes("enter");
    wcx.run_until_parked();

    let store = store_cell.borrow().clone().expect("store 未捕获");
    let current = cx.update(|app| store.read(app).state.current_id.clone());
    assert!(current.is_some(), "新建会话应已成为当前会话");

    // 用户气泡应到达(user/message 事件经帧泵);回复流式较慢,
    // 轮询等待(真时序:fake ~16s / 真实 LLM ~90s)
    let rounds = if fake { 80 } else { 450 };
    let mut user_ok = false;
    let mut assistant_text = String::new();
    for _ in 0..rounds {
        std::thread::sleep(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        let snapshot = cx.update(|app| store.read(app).current_nodes().to_vec());
        for node in &snapshot {
            match node {
                crate::features::chat::ChatNode::User { text, .. } if text == message => {
                    user_ok = true;
                }
                crate::features::chat::ChatNode::Assistant { text, .. } => {
                    assistant_text = text.clone();
                }
                _ => {}
            }
        }
        if user_ok && !assistant_text.is_empty() {
            break;
        }
    }
    assert!(user_ok, "用户气泡未出现(发送链断裂:before user/message)");
    assert!(
        !assistant_text.is_empty(),
        "助手回复未出现(turn 事件未回流)"
    );
    // 诊断回显:测试自己这一轮的完整事件尾部
    if let Some(id) = &current
        && let Ok(entries) = std::fs::read_dir(root.join("sessions"))
    {
        for entry in entries.flatten() {
            let log = entry.path().join(id).join("session.jsonl");
            if let Ok(content) = std::fs::read_to_string(&log) {
                for line in content
                    .lines()
                    .rev()
                    .take(10)
                    .collect::<Vec<_>>()
                    .iter()
                    .rev()
                {
                    eprintln!("[diag] {line}");
                }
            }
        }
    }
    let _ = view;
    let _ = std::fs::remove_dir_all(root);
}

#[gpui_kit::test]
fn composer_enter_sends_end_to_end(cx: &mut TestAppContext) {
    send_roundtrip(cx, true, "hello from test", None);
}

/// @ 补全回归锁:①长标题会话行必须单行截断——换行文本会溢出定高
/// 行框叠绘到后续行(实测报障);②会话候选与文件候选同上限
/// (MAX_RESULTS=20),否则会话多时补全卡一路顶到窗高。
#[gpui_kit::test]
fn at_completion_rows_truncate_and_cap(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "at-rows");
    // 注入 25 只长标题会话;排序 = session_id 升序,封顶后只应出前 20
    cx.update(|app| {
        store.update(app, |st, _| {
            st.state.sessions = (0..25usize)
                .map(|i| dsh_core::proto::SessionSummary {
                    session_id: format!("s-at-{i:02}"),
                    updated_at: 0,
                    running: false,
                    blank: false,
                    parent_session_id: None,
                    origin: None,
                    cwd: None,
                    agent_preset: None,
                    projections: Some(dsh_core::proto::Projections {
                        as_of_seq: -1,
                        values: serde_json::json!({
                            "title": format!("长标题会话{i:02}——{}", long_para(i)),
                        }),
                    }),
                })
                .collect();
        });
    });
    // 聚焦 composer 输入「@」,触发补全(Change → update_at_completion)
    let bounds = wcx
        .debug_bounds("composer-hit")
        .expect("composer 输入区缺失");
    wcx.simulate_click(
        gpui_kit::Point {
            x: bounds.origin.x + bounds.size.width / 2.,
            y: bounds.origin.y + bounds.size.height / 2.,
        },
        gpui_kit::Modifiers::default(),
    );
    wcx.run_until_parked();
    wcx.simulate_input("@");
    wcx.run_until_parked();

    let row_sel = |i: usize| -> &'static str {
        Box::leak(format!("at-row-长标题会话{i:02}——{}", long_para(i)).into_boxed_str())
    };
    // 等补全卡出现
    let mut visible = false;
    for _ in 0..10 {
        wcx.refresh().expect("刷新失败");
        wcx.run_until_parked();
        if wcx.debug_bounds("at-completion-anchor").is_some() {
            visible = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(visible, "@ 补全卡未出现");
    let _ = std::fs::remove_dir_all(root);

    // ① 截断:行高保持定高,截断包装层(-text)不得超一行
    // (无截断时文本换行,该层高度为多行 ≈ 行高 3 倍)
    for i in [0usize, 1, 19] {
        let row = wcx
            .debug_bounds(row_sel(i))
            .unwrap_or_else(|| panic!("会话行 {i} 未渲染"));
        assert!(
            row.size.height <= px(32.),
            "行 {i} 高度 {} 超 32px",
            row.size.height
        );
        let text_sel: &'static str = Box::leak(format!("{}-text", row_sel(i)).into_boxed_str());
        let text = wcx
            .debug_bounds(text_sel)
            .unwrap_or_else(|| panic!("行 {i} 文本层缺失"));
        assert!(
            text.size.height <= px(24.),
            "行 {i} 文本层高 {} = 多行(截断失效)",
            text.size.height
        );
    }
    // 行间不重叠
    let (a, b) = (
        wcx.debug_bounds(row_sel(0)).unwrap(),
        wcx.debug_bounds(row_sel(1)).unwrap(),
    );
    assert!(
        b.origin.y >= a.origin.y + a.size.height - px(1.),
        "行 1 与行 0 竖向重叠"
    );
    // ② 封顶:第 21 只及以后不渲染
    for i in [20usize, 24] {
        assert!(
            wcx.debug_bounds(row_sel(i)).is_none(),
            "会话行 {i} 超上限仍渲染(封顶失效)"
        );
    }
}

/// Mermaid 查看器全链路:内嵌**卡片**工具有控(图表/代码、±缩放、下载、
/// 放大);点击卡片图 → 查看器(纯图:画布 + 角落关闭钮,无工具栏)→
/// Esc 关闭。放大/下载动作落在卡片上,查看器"放大只放大图片"。
/// 全程持单槽测试锁(开图同步渲染/关闭驱逐都触全局 VIEWER_SLOT,与
/// kits 单槽测试、防抖生命周期测试并发交叠会互相覆盖档位)。
#[gpui_kit::test]
fn mermaid_viewer_full_interaction(cx: &mut TestAppContext) {
    let _slot_guard = crate::kits::mermaid::VIEWER_SLOT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let (store, mut wcx, root) = menu_harness(cx, "mermaid");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.run_until_parked();
    };

    // 发送一条消息 → fakes 演示首段(mermaid 演示)回流为助手回复
    let bounds = wcx
        .debug_bounds("composer-hit")
        .expect("composer 输入区缺失");
    wcx.simulate_click(
        gpui_kit::Point {
            x: bounds.origin.x + bounds.size.width / 2.,
            y: bounds.origin.y + bounds.size.height / 2.,
        },
        gpui_kit::Modifiers::default(),
    );
    wcx.run_until_parked();
    wcx.simulate_input("查看 mermaid 演示");
    wcx.simulate_keystrokes("enter");
    wcx.run_until_parked();

    // 等待助手段落就绪(演示首段 = 第 1 块引导段 + 图在第 1 块)
    let mut fig_sel: Option<&'static str> = None;
    for _ in 0..80 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        redraw(cx, &mut wcx);
        if let Some(key) = cx.update(|app| {
            store
                .read(app)
                .current_nodes()
                .iter()
                .find_map(|n| match n {
                    ChatNode::Assistant { key, text, .. } if !text.is_empty() => Some(key.clone()),
                    _ => None,
                })
        }) {
            let sel: &'static str = Box::leak(format!("{key}-md-mermaid-1").into_boxed_str());
            if wcx.debug_bounds(sel).is_some() {
                fig_sel = Some(sel);
                break;
            }
        }
    }
    let sel = fig_sel.expect("内嵌图未渲染(演示首段无图?)");
    let card = sel; // 卡片外层 id

    // 卡片工具条应常显(图表/代码、±、下载、放大)——控件在卡片上
    // (card 是 'static;派生的 selector 也需 'static,经 Box::leak)
    let (toolbar, seg_chart, seg_code, copy, copy_done, download, enlarge, figure) = {
        let mk = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        (
            mk(format!("{card}-toolbar")),
            mk(format!("{card}-seg-chart")),
            mk(format!("{card}-seg-code")),
            mk(format!("{card}-copy")),
            mk(format!("{card}-copy-done")),
            mk(format!("{card}-download")),
            mk(format!("{card}-enlarge")),
            // figure 的 debug_selector = `{prefix}-md-mermaid-figure-{ix}`;
            // card key = `{prefix}-md-mermaid-{ix}`,将其尾部段替换即可
            mk(card.replacen("-md-mermaid-", "-md-mermaid-figure-", 1)),
        )
    };
    assert!(wcx.debug_bounds(toolbar).is_some(), "卡片工具条应出现");
    assert!(wcx.debug_bounds(copy).is_some(), "卡片复制按钮应出现");
    assert!(wcx.debug_bounds(seg_chart).is_some(), "卡片图表按钮应出现");
    assert!(wcx.debug_bounds(download).is_some(), "卡片下载按钮应出现");

    // 点击卡片图 body(`{card}-figure`)→ 查看器打开(纯图)
    click_sel(&mut wcx, figure);
    wcx.run_until_parked();
    redraw(cx, &mut wcx);
    assert!(wcx.debug_bounds("mv-canvas").is_some(), "查看器画布应出现");
    assert!(wcx.debug_bounds("mv-close").is_some(), "查看器关闭钮应出现");
    assert!(
        wcx.debug_bounds("mv-toolbar").is_none(),
        "查看器不应有工具栏(纯图)"
    );
    let zoom0 = cx.update(|app| store.read(app).chat.mermaid_viewer.as_ref().unwrap().zoom);
    assert!(zoom0 > 0.0, "自适应 zoom 未回写:{zoom0}");

    // 查看器画布内图有尺寸(非空)
    let fig = wcx.debug_bounds("mv-figure");
    if let Some(f) = &fig {
        assert!(
            f.size.width.as_f32() > 10.0 && f.size.height.as_f32() > 10.0,
            "查看器内图应有尺寸:{f:?}"
        );
    } else {
        panic!("mv-figure 未渲染");
    }
    // 居中:图中心应贴近画布中心(fit 全览的居中由 origin 计算,非
    // m_auto/flex —— 曾被 flex 收缩吸收拖拽位移,此断言锁住回归)
    if let (Some(cv), Some(fig)) = (wcx.debug_bounds("mv-canvas"), wcx.debug_bounds("mv-figure")) {
        let ccx = (cv.origin.x + cv.size.width / 2.).as_f32();
        let ccy = (cv.origin.y + cv.size.height / 2.).as_f32();
        let fcx = (fig.origin.x + fig.size.width / 2.).as_f32();
        let fcy = (fig.origin.y + fig.size.height / 2.).as_f32();
        let tol = 24.0; // 图小于画布时中心应重合(留像素容差)
        assert!(
            (fcx - ccx).abs() <= tol && (fcy - ccy).abs() <= tol,
            "查看器图应水平+垂直居中:figure 中心=({fcx:.1},{fcy:.1}) canvas 中心=({ccx:.1},{ccy:.1})"
        );
    } else {
        panic!("mv-canvas/mv-figure bounds 缺失");
    }

    // 查看器内 Esc → 关闭
    wcx.simulate_keystrokes("escape");
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store.read(app).chat.mermaid_viewer.is_none()),
        "Esc 应关闭查看器"
    );

    // 卡片「复制」:源码进剪贴板 + **按钮级反馈**(✓ 已复制态切换,
    // 不靠异步通知);超窗复原(detach 定时任务;测试时钟显式推进)
    click_sel(&mut wcx, copy);
    redraw(cx, &mut wcx);
    let clip = cx.update(|app| app.read_from_clipboard());
    assert!(
        clip.as_ref()
            .and_then(|item| item.text())
            .is_some_and(|t| t.contains("flowchart")),
        "复制应把 mermaid 源码写入剪贴板:{clip:?}"
    );
    assert!(
        wcx.debug_bounds(copy_done).is_some() && wcx.debug_bounds(copy).is_none(),
        "复制后按钮应切换为「已复制」反馈态"
    );
    assert!(
        cx.update(|app| store.read(app).chat.mermaid_viewer.is_none()),
        "复制不应打开查看器"
    );
    cx.executor()
        .advance_clock(crate::kits::mermaid::MERMAID_COPY_FEEDBACK * 2);
    cx.run_until_parked();
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds(copy).is_some() && wcx.debug_bounds(copy_done).is_none(),
        "反馈窗过后按钮应复原为「复制」"
    );

    // 卡片 图表 → 代码 → 回图表
    click_sel(&mut wcx, seg_code);
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store
            .read(app)
            .chat
            .mermaid_cards
            .get(card)
            .map(|c| c.show_code)
            .unwrap_or(false)),
        "show_code 应翻转"
    );
    click_sel(&mut wcx, seg_chart);
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store
            .read(app)
            .chat
            .mermaid_cards
            .get(card)
            .map(|c| !c.show_code)
            .unwrap_or(false)),
        "应回到图表模式"
    );

    // 下载:临时目录落 PNG(卡片下载按钮)
    let dl = std::env::temp_dir().join(format!("dsh-dl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dl);
    // SAFETY: 变量仅本测试的下载路径读取;测试内同步设置/清理
    unsafe { std::env::set_var("DSH_DOWNLOAD_DIR", &dl) };
    click_sel(&mut wcx, download);
    redraw(cx, &mut wcx);
    let saved: Vec<_> = std::fs::read_dir(&dl)
        .expect("下载目录应存在")
        .flatten()
        .map(|e| e.path())
        .collect();
    assert!(
        saved
            .iter()
            .any(|p| p.extension().is_some_and(|x| x == "png")),
        "应保存 PNG:{saved:?}"
    );
    let _ = std::fs::remove_dir_all(&dl);
    // SAFETY: 与上方 set_var 配对;测试内同步清理
    unsafe { std::env::remove_var("DSH_DOWNLOAD_DIR") };

    // 卡片「放大」按钮 → 查看器打开;角落关闭钮 → 关闭
    click_sel(&mut wcx, enlarge);
    wcx.run_until_parked();
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store.read(app).chat.mermaid_viewer.is_some()),
        "放大应打开查看器"
    );
    click_sel(&mut wcx, "mv-close");
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store.read(app).chat.mermaid_viewer.is_none()),
        "角落关闭钮应关闭查看器"
    );

    let _ = std::fs::remove_dir_all(root);
}

/// 查看器缩放的防抖重光栅生命周期(store 级,不做 UI 断言):fit 落定/
/// 视口回写/每次滚轮都(重)排任务;静止满 300ms 任务收尾清位;窗口内
/// 连续滚轮只有最终意图生效;关闭取消未决任务。滚轮中心锚定 pan 断言。
/// 这是内存泄漏修复(同步逐帧重光栅 + atlas 纹理不回收 + 整图光栅随
/// zoom² 膨胀)的行为锁。任务会触全局单槽 → 与 kits 单槽测试互斥。
#[gpui_kit::test]
fn mermaid_viewer_zoom_debounce_lifecycle(cx: &mut TestAppContext) {
    // 全程持锁:任务/绘制触全局 VIEWER_SLOT 的时点不只在 advance 窗口内
    let _slot_guard = crate::kits::mermaid::VIEWER_SLOT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let (store, _wcx, root) = menu_harness(cx, "mmd-debounce");
    // 超宽图:放大后整图 ≫ 视口,pan 中心锚定断言才会真正触发
    let source: std::sync::Arc<str> = "flowchart LR\n    N0[0] --> N1[1] --> N2[2] --> N3[3] --> N4[4] --> N5[5] --> N6[6] --> N7[7] --> N8[8] --> N9[9] --> N10[10] --> N11[11] --> N12[12] --> N13[13]\n".into();
    let has_task =
        |cx: &mut TestAppContext| cx.update(|app| store.read(app).chat.mermaid_reraster.is_some());

    // 打开 + 视口回写 + fit 落定 → 防抖任务就位(真实窗口绘制会把视口
    // 覆盖为窗口实况并可能已同步渲染首档 —— 任务凭 gen/upto_date 自洽)
    cx.update(|app| {
        store.update(app, |s, cx| {
            s.open_mermaid_viewer(source.clone(), None, cx);
            s.set_mermaid_viewer_viewport(1000., 800., cx);
            s.set_mermaid_viewer_fit(1.0, cx);
        })
    });
    assert!(has_task(cx), "fit 落定应排防抖重光栅任务");

    // 静止满防抖窗口 → 任务完成清位(含「单槽已精确档」早退路径)
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(350));
    cx.run_until_parked();
    assert!(!has_task(cx), "静止超窗后任务应完成并清位");

    // 窗口内连续滚轮:每次重排(timer 重置),最终意图 = 最后一次;
    // 视口中心锚定:pan=0、z 1→1.25⁸ → pan_x = c×(z′−z) 钳制 [0, 图−视口]
    cx.update(|app| {
        store.update(app, |s, cx| {
            for _ in 0..8 {
                s.set_mermaid_zoom(1.25, None, cx);
            }
        })
    });
    assert!(has_task(cx), "滚轮应(重)排防抖任务");
    let (settled_zoom, settled_pan, vp) = cx.update(|app| {
        let v = store.read(app).chat.mermaid_viewer.as_ref();
        (
            v.map(|v| v.zoom).unwrap_or(0.0),
            v.map(|v| v.pan).unwrap_or((0.0, 0.0)),
            v.map(|v| v.viewport).unwrap_or((0.0, 0.0)),
        )
    });
    let z8 = 1.25f32.powi(8);
    assert!((settled_zoom - z8).abs() < 1e-4, "八次 1.25 步进 = {z8}");
    let nat = crate::kits::mermaid::natural_size(&source);
    if let Some((nat_w, _)) = nat
        && nat_w * z8 > vp.0 + 1.0
    {
        let expect_x = (nat_w * z8 - vp.0).min(vp.0 / 2.0 * (z8 - 1.0));
        assert!(
            (settled_pan.0 - expect_x).abs() < 8.0 + 1e-3,
            "中心锚定 pan_x 应 ≈ {expect_x:.1},实测 {:.1}",
            settled_pan.0
        );
    } else {
        panic!("超宽图放大后应超出视口,断言未覆盖: nat={nat:?} z8={z8} vp={vp:?}");
    }
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(350));
    cx.run_until_parked();
    assert!(!has_task(cx), "静止后任务应完成");

    // 关闭 → 未决任务取消 + 查看器清空 + 单槽驱逐
    cx.update(|app| {
        store.update(app, |s, cx| {
            s.set_mermaid_zoom(1.25, None, cx); // 再排一个未决任务
            s.close_mermaid_viewer(cx);
        })
    });
    assert!(!has_task(cx), "关闭应取消未决任务");
    assert!(
        cx.update(|app| store.read(app).chat.mermaid_viewer.is_none()),
        "查看器应关闭"
    );

    let _ = std::fs::remove_dir_all(root);
}

/// 查看器拖拽平移(UI 级,`simulate_mouse_*` 全链路):开图 → kick 首档
/// 后台渲染入槽 → 放大至超视口 → 画布内按下拖动 → pan 随动(指针左拖
/// = 原点右移)→ 松手清 drag 态并排最终档任务。这是「无法拖动/松不开」
/// 修复(监听器读渲染快照丢移动、画布外抬起滞留)的行为锁。
#[gpui_kit::test]
fn mermaid_viewer_drag_panning(cx: &mut TestAppContext) {
    let _slot_guard = crate::kits::mermaid::VIEWER_SLOT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let (store, mut wcx, root) = menu_harness(cx, "mmd-drag");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.run_until_parked();
    };
    let source: std::sync::Arc<str> = "flowchart LR\n    N0[0] --> N1[1] --> N2[2] --> N3[3] --> N4[4] --> N5[5] --> N6[6] --> N7[7] --> N8[8] --> N9[9] --> N10[10] --> N11[11] --> N12[12] --> N13[13]\n".into();

    // 开图 → 绘制(kick 首档 0 延迟任务,run_until_parked 收尾入槽)
    cx.update(|app| {
        store.update(app, |s, cx| {
            s.open_mermaid_viewer(source.clone(), None, cx);
        })
    });
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store.read(app).chat.mermaid_viewer.is_some()),
        "查看器应打开"
    );
    assert!(wcx.debug_bounds("mv-canvas").is_some(), "查看器画布应渲染");

    // 放大到整图 ≫ 视口(pan 有可动范围)
    cx.update(|app| {
        store.update(app, |s, cx| {
            for _ in 0..8 {
                s.set_mermaid_zoom(1.25, None, cx);
            }
        })
    });
    redraw(cx, &mut wcx);
    let pan0 = cx.update(|app| {
        store
            .read(app)
            .chat
            .mermaid_viewer
            .as_ref()
            .map(|v| v.pan)
            .unwrap_or((0.0, 0.0))
    });
    // 拖前图位。测试时钟不走真实时间:0 延迟 kick 与 300ms 防抖都要
    // 显式推进才触发;8× 缩放已换代,早先的 kick 过期不渲染,推进到
    // 最终防抖档落地(k=1 基准,拖拽期间 mv-figure 原点移动量 = pan
    // 增量 —— 视觉真的动了的行为锁)
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(350));
    cx.run_until_parked();
    redraw(cx, &mut wcx);
    let fig0 = wcx.debug_bounds("mv-figure").expect("拖前图 bounds");

    // 画布中心按下 → 左拖 120px → pan.x 应增大(图内容左移 = 看到右侧)
    let canvas = wcx.debug_bounds("mv-canvas").expect("画布 bounds");
    let center = gpui_kit::Point {
        x: canvas.origin.x + canvas.size.width / 2.,
        y: canvas.origin.y + canvas.size.height / 2.,
    };
    wcx.simulate_mouse_down(
        center,
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    cx.run_until_parked();
    wcx.simulate_mouse_move(
        gpui_kit::Point {
            x: center.x - px(120.),
            y: center.y,
        },
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    cx.run_until_parked();
    let pan1 = cx.update(|app| {
        store
            .read(app)
            .chat
            .mermaid_viewer
            .as_ref()
            .map(|v| v.pan)
            .unwrap_or((0.0, 0.0))
    });
    assert!(
        pan1.0 > pan0.0 + 100.0,
        "左拖应增大 pan.x(原点右移):{pan0:?} → {pan1:?}"
    );
    // 视觉断言:图随手左移(此前 m_auto 布局吸收 margin 位移 → pan 变了
    // 但图纹丝不动;绝对定位后原点 = −pan 直跟)
    redraw(cx, &mut wcx);
    let fig1 = wcx.debug_bounds("mv-figure").expect("拖后图 bounds");
    assert!(
        fig1.origin.x.as_f32() < fig0.origin.x.as_f32() - 100.0,
        "拖拽后图原点应左移(图随手):{:?} → {:?}",
        fig0.origin.x,
        fig1.origin.x
    );

    // 抬起(画布外根层兜底同效):drag 态清空 + 排最终档任务
    wcx.simulate_mouse_up(
        gpui_kit::Point {
            x: px(2.),
            y: px(2.),
        }, // 环外遮罩位置 = 画布外
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    cx.run_until_parked();
    assert!(
        cx.update(|app| {
            store
                .read(app)
                .chat
                .mermaid_viewer
                .as_ref()
                .map(|v| v.drag_last.is_none())
                .unwrap_or(true)
        }),
        "松手(画布外)应清 drag 态"
    );
    // 松手后排的最终档任务:静止收尾
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(350));
    cx.run_until_parked();

    let _ = std::fs::remove_dir_all(root);
}

/// 菜单交互测试公共装配:fake 桥 + store + 视图 + 帧泵 + 首帧刷新
fn menu_harness(
    cx: &mut TestAppContext,
    tag: &str,
) -> (
    Entity<AppStore>,
    gpui_kit::VisualTestContext,
    std::path::PathBuf,
) {
    menu_harness_opts(cx, tag, false)
}

/// 工作区预置 AGENTS.md 的变体(attach 基线注入路径的行为锁环境)
fn menu_harness_agents(
    cx: &mut TestAppContext,
    tag: &str,
) -> (
    Entity<AppStore>,
    gpui_kit::VisualTestContext,
    std::path::PathBuf,
) {
    menu_harness_opts(cx, tag, true)
}

fn menu_harness_opts(
    cx: &mut TestAppContext,
    tag: &str,
    agents_md: bool,
) -> (
    Entity<AppStore>,
    gpui_kit::VisualTestContext,
    std::path::PathBuf,
) {
    use futures::StreamExt as _;
    cx.update(|app| {
        gpui_kit::component::init(app);
        crate::kits::theme::init(app);
        crate::shell::bind_global_keys(app);
    });
    allow_host_parking(cx);
    let root = std::env::temp_dir().join(format!("dsh-desktop-menu-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let ws = root.join("ws");
    if agents_md {
        std::fs::create_dir_all(&ws).expect("mkdir ws");
        std::fs::write(ws.join("AGENTS.md"), "# 项目规范\n\n用 Rust。").expect("write AGENTS.md");
    }
    let (bridge, frames_rx) =
        HostBridge::new_at(ws, true, "", Some(root.join("sessions"))).expect("桥构建失败");
    // 技能家目录隔离:真实 ~/.agents/skills 的技能会泄进 `/` 菜单技能节
    // (session_skills 扫真实 home),撑高菜单把命令行测的 goal 行顶出
    // 窗口顶、点击落空。测试统一注入空目录;需要技能夹具的测试再写入
    bridge
        .host()
        .set_skill_user_home(Some(root.join("skills-home")));
    let store_cell = std::rc::Rc::new(std::cell::RefCell::new(None::<Entity<AppStore>>));
    let store_capture = store_cell.clone();
    let (_view, wcx) = cx.add_window_view(|window, cx| {
        let store = cx.new(|cx| AppStore::new(bridge, cx));
        store.update(cx, |s, cx| {
            s.attach_window_state(window, cx);
        });
        // 帧泵(与 main.rs 同款;plan/事件回流依赖)
        let pump = store.clone();
        store.update(cx, |_, cx| {
            cx.spawn(async move |_this, cx| {
                let mut rx = frames_rx;
                while let Some(frame) = rx.next().await {
                    pump.update(cx, |s, cx| s.apply_frame(frame, cx));
                }
                Ok::<(), anyhow::Error>(())
            })
            .detach();
        });
        *store_capture.borrow_mut() = Some(store.clone());
        let view = cx.new(|cx| WorkspaceView::new(store, cx));
        // Root 包裹(与 main/send_roundtrip 同款):Input 点击路径
        // 经 Root::read 取 window root,裸 WorkspaceView 会 unwrap panic
        gpui_kit::component::Root::new(view, window, cx)
    });
    wcx.run_until_parked();
    wcx.refresh().expect("窗口刷新失败");
    wcx.run_until_parked();
    let store = store_cell.borrow().clone().expect("store 未捕获");
    // 克隆解除对 cx 的借用(VisualTestContext Clone + Deref 到
    // TestAppContext,内部 Arc 共享,测试可交替用 cx / wcx)
    (store, wcx.clone(), root)
}

/// 按 debug selector 点击元素中心
fn click_sel(wcx: &mut gpui_kit::VisualTestContext, sel: &'static str) {
    let b = wcx
        .debug_bounds(sel)
        .unwrap_or_else(|| panic!("selector {sel} bounds 缺失"));
    wcx.simulate_click(
        gpui_kit::Point {
            x: b.origin.x + b.size.width / 2.,
            y: b.origin.y + b.size.height / 2.,
        },
        gpui_kit::Modifiers::default(),
    );
}

/// HostBridge 是专任 tokio runtime:帧通道与 oneshot 的唤醒天然从
/// tokio 线程跨到测试线程。新 zed test-scheduler(≥badf23c)把「测试
/// 线程外唤醒本地任务」记成非确定性失误,end_test 即炸;开 parking
/// = zed 对含真实 I/O 测试的官方逃生门,恢复 gpui 0.2.2 语义
/// (测试自有轮询护栏 wait_permission / sleep+run_until_parked 承接)。
fn allow_host_parking(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
}

/// 轮询宿主会话权限直到期望值或超时(异步 set_permission 落盘日志事件,
/// 与 dsh-core 侧 wait_log_sandbox 同义;desktop 测试环境用真实线程阻塞)。
fn wait_permission(host: &dsh_core::registry::AppHost, id: &str, want: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while host.session_permission(id) != want && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// 权限下拉全链路:开菜单 → 点「完全权限」→ **风险确认弹窗**(取消不动 /
/// 确认才切)→ 宿主落 sandbox/mode 事件 + 缓存回写 +
/// 菜单关(豁免机制保证行点击不被外点关闭吞掉)
#[gpui_kit::test]
fn composer_menu_open_select_permission(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "perm");
    let id = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("当前会话");
    let host = cx.update(|app| store.read(app).bridge.host().clone());

    // 取消路径:弹窗出现 → 取消 → 权限不动、弹窗关、菜单已收
    click_sel(&mut wcx, "chip-perm");
    cx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    cx.run_until_parked();
    assert!(
        wcx.debug_bounds("完全权限").is_some(),
        "权限菜单未弹出(触发/豁免链断裂)"
    );
    click_sel(&mut wcx, "完全权限");
    cx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    assert!(
        wcx.debug_bounds("full-access-card").is_some(),
        "完全权限风险确认弹窗未出现"
    );
    click_sel(&mut wcx, "full-access-cancel");
    cx.run_until_parked();
    // 弹窗退场不走 debug_bounds(该 map 只增不清,残留不可靠),以
    // store 确认态兜底
    let ask = cx.update(|app| store.read(app).settings.full_access_confirm);
    assert_eq!(ask, None, "取消后确认态未清");
    assert_ne!(
        host.session_permission(&id),
        "full-access",
        "取消路径不应切换权限"
    );
    let menu = cx.update(|app| store.read(app).chat.composer_menu);
    assert_eq!(
        menu,
        crate::features::chat::ComposerMenu::None,
        "ask 应顺带收起菜单"
    );

    // 确认路径:重开菜单 → 完全权限 → 弹窗确认 → 宿主落档 + 缓存回写
    click_sel(&mut wcx, "chip-perm");
    cx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    cx.run_until_parked();
    assert!(
        wcx.debug_bounds("完全权限").is_some(),
        "二次开权限菜单未弹出"
    );
    click_sel(&mut wcx, "完全权限");
    cx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    assert!(
        wcx.debug_bounds("full-access-confirm").is_some(),
        "确认按钮未渲染"
    );
    click_sel(&mut wcx, "full-access-confirm");
    cx.run_until_parked();
    // 回归锁(真机取证):确认单击即生效——乐观更新就地写
    // 缓存。旧实现成功回调 refresh 折叠磁盘日志,driver 落档前读到旧值
    // 把标签刷回去(真机 6 击成对同值=首击已切但界面不动,被迫补点)
    let cached = cx.update(|app| {
        store
            .read(app)
            .session_cfg_by_id
            .get(&id)
            .map(|c| c.permission.clone())
    });
    assert_eq!(
        cached.as_deref(),
        Some("full-access"),
        "确认后缓存应立即为目标值(乐观更新)"
    );
    // 异步 set_permission → worker 落盘 → 轮询磁盘 fold
    wait_permission(&host, &id, "full-access");

    assert_eq!(
        host.session_permission(&id),
        "full-access",
        "宿主权限未落日志事件"
    );
    // 让 bridge.call 完成后的缓存回写回调跑完:跨执行器唤醒时序不稳
    // (重编译后首跑系统性偏慢,单次断言边缘翻挂),轮询清空勿单次
    // (同 search_locate/延迟定位模式);并行套件负载下回调可能饿到
    // 数秒,预算放宽到 10s
    let cached = {
        let mut cached;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            cached = cx.update(|app| {
                store
                    .read(app)
                    .session_cfg_by_id
                    .get(&id)
                    .map(|c| c.permission.clone())
            });
            if cached.as_deref() == Some("full-access") || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cached
    };
    assert_eq!(cached.as_deref(), Some("full-access"), "配置缓存未回写");
    let menu = cx.update(|app| store.read(app).chat.composer_menu);
    assert_eq!(
        menu,
        crate::features::chat::ComposerMenu::None,
        "菜单应关闭"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 额度静默自动刷新:default provider 配计费端点(本地 mock,DeepSeek
/// 余额形状)→ auto_refresh_billing 即查即写缓存(settings_view 含
/// billing_cache)且**不落设置页通告**(静默纪律;手动路径才有通告)
#[gpui_kit::test]
fn billing_auto_refresh_quiet_writes_cache(cx: &mut TestAppContext) {
    let (store, _wcx, root) = menu_harness(cx, "billing-auto");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 2048];
        let _ = std::io::Read::read(&mut stream, &mut buf);
        let body =
            r#"{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"9.52"}]}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        std::io::Write::write_all(&mut stream, resp.as_bytes()).unwrap();
    });
    let host = cx.update(|app| store.read(app).bridge.host().clone());
    // harness 桥的显式 api_key 为空串 → 凭据链需自给:env 引用 + 环境变量
    // (测试进程内一次性注入;edition 2024 set_var 为 unsafe)
    // SAFETY:测试单线程设置阶段调用,无并发读
    unsafe { std::env::set_var("DSH_BILLING_TEST_KEY", "test-key") };
    let mut p = dsh_core::settings::builtin_provider();
    p.credential_ref = Some("env:DSH_BILLING_TEST_KEY".into());
    p.base_url = format!("http://127.0.0.1:{port}/v1");
    p.billing = Some(dsh_core::settings::BillingConfig {
        kind: dsh_core::settings::BillingKind::Balance,
        url: format!("http://127.0.0.1:{port}/user/balance"),
        paths: dsh_core::settings::BillingPaths {
            balance: Some("balance_infos.0.total_balance".into()),
            currency: Some("balance_infos.0.currency".into()),
            ..Default::default()
        },
    });
    host.upsert_provider(p).unwrap();
    cx.update(|app| store.update(app, |s, cx| s.settings_refresh(cx)));
    let default_pid = cx.update(|app| {
        store.read(app).settings.settings_snapshot["defaultProvider"]
            .as_str()
            .map(str::to_string)
    });
    assert_eq!(
        default_pid.as_deref(),
        Some("deepseek"),
        "harness 默认 provider 应为 deepseek"
    );

    // 触发静默自动刷新(与 turn/end 同一入口;60s 防抖首放行)
    cx.update(|app| store.update(app, |s, cx| s.auto_refresh_billing(cx)));
    // 跨执行器(bridge tokio → 帧泵/回写)时序:轮询到缓存写入,勿单次断言
    let mut cached_kind = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        cx.run_until_parked();
        cached_kind = cx.update(|app| {
            store.read(app).settings.settings_snapshot["providers"]
                .as_array()
                .and_then(|ps| {
                    ps.iter()
                        .find(|p| p["id"] == "deepseek")
                        .and_then(|p| p["billing_cache"]["kind"].as_str().map(str::to_string))
                })
        });
        if cached_kind.as_deref() == Some("balance") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        cached_kind.as_deref(),
        Some("balance"),
        "自动刷新未写入 billing_cache"
    );
    server.join().unwrap();
    // 静默纪律:自动路径不得落设置页通告(手动刷新才有「计费已更新」)
    let notice = cx.update(|app| store.read(app).settings.settings_notice.clone());
    assert!(notice.is_none(), "静默刷新不应落通告:{notice:?}");
    let _ = std::fs::remove_dir_all(root);
}

/// 菜单外点关闭:开权限菜单 → 点输入区(冒泡到根级关闭)
#[gpui_kit::test]
fn menu_closes_on_outside_click(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "outside");
    click_sel(&mut wcx, "chip-perm");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|app| store.read(app).chat.composer_menu),
        crate::features::chat::ComposerMenu::Permission,
        "菜单应已打开"
    );

    click_sel(&mut wcx, "composer-hit");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|app| store.read(app).chat.composer_menu),
        crate::features::chat::ComposerMenu::None,
        "外点应关闭菜单"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 首条消息后标题 = 内容摘录(60 字):turn 开始边沿刷清单即取到;
/// 回归锚:此前 history 对空会话恒下发 title=""(title_of 烧穿空串),
/// 客户端 titles 表被空串永久遮蔽,清单摘录进不来 → 标题永远「新会话」
#[gpui_kit::test]
fn first_message_titles_session(cx: &mut TestAppContext) {
    let (store, _wcx, root) = menu_harness(cx, "title");
    let id = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("当前会话");
    cx.update(|app| {
        store.update(app, |st, cx| st.send("帮我写一个排序算法示例", cx));
    });
    let mut title = String::new();
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        title = cx.update(|app| store.read(app).title_for(&id));
        if title != "新会话" {
            break;
        }
    }
    assert_eq!(title, "帮我写一个排序算法示例", "标题未随首条消息更新");
    let _ = std::fs::remove_dir_all(root);
}

/// 设置失败落本地通告:非法模型名被宿主模型表确定性拒绝
/// (无需伪造 running;Notice 尾插 + key 幂等)
#[gpui_kit::test]
fn setter_error_pushes_notice(cx: &mut TestAppContext) {
    let (store, _wcx, root) = menu_harness(cx, "err");
    cx.update(|app| {
        store.update(app, |st, cx| st.set_session_model("__no_such_model__", cx));
    });
    cx.run_until_parked();
    let last = cx.update(|app| store.read(app).current_nodes().last().cloned());
    match last {
        Some(ChatNode::Notice { text, .. }) => {
            assert!(text.contains("切换失败"), "通告文案异常: {text}")
        }
        other => panic!("尾部应为 Notice 节点,实为 {other:?}"),
    }
    let _ = std::fs::remove_dir_all(root);
}

/// 轨迹台账(右栏面板标签):行序/Request 圆点/时间线渲染;点行开
/// 检查器(标签页按数据在场裁剪);点 Request 圆点切请求模式;台账
/// 贴面板底缘(面板全高列)
#[gpui_kit::test]
fn trajectory_ledger_rows_inspector_and_tabs(cx: &mut TestAppContext) {
    use crate::features::trajectory::{InspectTarget, TrajectoryView};
    use dsh_core::trajectory::{TrajectoryRecord, TrajectoryRequest};

    let (store, mut wcx, root) = menu_harness(cx, "traj");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };

    let rec = |index: u64, kind: &str, turn: Option<u64>, group: &str| {
        TrajectoryRecord {
            index,
            seq: index,
            kind: kind.into(),
            turn,
            group: group.into(),
            turn_start: index == 2,
            text: match kind {
                "tool" => format!("bash {{\"command\":\"ls {index}\"}}"),
                _ => format!("记录 {index}"),
            },
            result: (kind == "tool").then(|| "ok".to_string()),
            is_error: false,
            time_seconds: (kind != "user").then_some(1.2),
            started_at: Some(1000 + index as i64 * 100),
            request_number: (index == 3).then_some(1),
            input: None,
            output: (kind == "message").then_some(120),
            think: (kind == "message").then_some(30),
            ttft_ms: (kind == "message").then_some(300),
            payload: (kind == "tool").then(|| "{\"command\":\"ls\"}".to_string()),
            output_detail: (kind != "user").then(|| format!("详情 {index}")),
            thinking_detail: None,
            system_prompt: (kind == "system").then(|| "# System prompt".to_string()),
            tools_catalog: (kind == "system").then(|| {
                vec![serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": "bash", "description": "Run a command",
                        "parameters": { "type": "object", "properties": {} }
                    }
                })]
            }),
            schema_detail: (kind == "tool").then(|| {
                "{\"type\":\"function\",\"function\":{\"name\":\"bash\",\"description\":\"Run a command\",\"parameters\":{\"type\":\"object\"}}".to_string()
            }),
            source: (kind == "context")
                .then(|| serde_json::json!({ "kind": "agent-instructions" })),
        }
    };
    let request = TrajectoryRequest {
        number: 1,
        turn: 1,
        step: 1,
        model: "deepseek-chat".into(),
        provider: "deepseek".into(),
        reasoning_effort: Some("high".into()),
        status: "complete".into(),
        started_at: 1200,
        completed_at: 1500,
        duration_ms: 1500,
        ttft_ms: Some(300),
        usage: Some(dsh_core::trajectory::TrajectoryUsage {
            input: 1000,
            cached: 400,
            other: 600,
            output: 120,
            reasoning: 30,
        }),
        cumulative: dsh_core::trajectory::TrajectoryUsage {
            input: 1000,
            cached: 400,
            other: 600,
            output: 120,
            reasoning: 30,
        },
        tool_calls: 1,
    };

    cx.update(|app| {
        store.update(app, |st, _| {
            let id = st.state.current_id.clone().expect("当前会话");
            st.trajectory.trajectory = TrajectoryView {
                records: vec![
                    rec(1, "system", None, "Message"),
                    rec(2, "user", Some(1), "Message"),
                    rec(3, "message", Some(1), "Step 1"),
                    rec(4, "tool", Some(1), "Step 1"),
                    rec(5, "context", Some(1), "Message"),
                ],
                requests: vec![request],
                has_older: false,
                total: 5,
                loading: false,
                loading_older: false,
            };
            st.trajectory.trajectory_session = Some(id);
            // 直开轨迹面板标签(不经 handler,不触发拉取)
            st.panel_open = true;
            st.panel_tabs = vec![crate::shell::panel::PanelTab::Trajectory];
            st.panel_active_tab = Some(crate::shell::panel::PanelTab::Trajectory);
        });
    });
    redraw(cx, &mut wcx);

    // 行序:四行自上而下
    let tops: Vec<f32> = [
        "trajectory-row-1",
        "trajectory-row-2",
        "trajectory-row-3",
        "trajectory-row-4",
    ]
    .iter()
    .map(|sel| {
        let b = wcx
            .debug_bounds(sel)
            .unwrap_or_else(|| panic!("行 {sel} 缺失"));
        f32::from(b.origin.y)
    })
    .collect();
    assert!(
        tops.windows(2).all(|w| w[0] < w[1]),
        "行应自上而下,实为 {tops:?}"
    );
    // Request 圆点 + 时间线 + 无加载更早;台账滚动区贴面板底缘
    // (面板全高列;composer 在主区,不占面板)
    assert!(
        wcx.debug_bounds("traj-request-1").is_some(),
        "Request 圆点缺失"
    );
    assert!(wcx.debug_bounds("timeline-track").is_some(), "时间线缺失");
    assert!(
        wcx.debug_bounds("load-earlier").is_none(),
        "无更早记录不应有加载钮"
    );
    let rp = wcx.debug_bounds("right-panel").expect("面板列缺失");
    let ts = wcx
        .debug_bounds("trajectory-scroll")
        .expect("台账滚动区缺失");
    let gap = f32::from(rp.bottom() - ts.bottom());
    assert!(gap < 40., "台账应贴面板底缘(全高列),底缘差 {gap}px");

    // 工具栏动作钮接线:Turns(⊞/⊟ action 钮)点击 → 全局折叠生效
    click_sel(&mut wcx, "traj-toolbar-turns");
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store.read(app).trajectory.all_turns_collapsed),
        "Turns 钮应触发全局折叠"
    );
    assert!(
        wcx.debug_bounds("turn-summary-1").is_some(),
        "折叠后应出现摘要行"
    );
    // 还原(展开),避免影响后续行选择断言
    click_sel(&mut wcx, "traj-toolbar-turns");
    redraw(cx, &mut wcx);

    // 点工具行 → 检查器:Summary/Payload/Result/Timing 四页
    click_sel(&mut wcx, "trajectory-row-4");
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("trajectory-inspector").is_some(),
        "检查器未开"
    );
    for tab in [
        "inspector-tab-summary",
        "inspector-tab-payload",
        "inspector-tab-result",
        "inspector-tab-timing",
    ] {
        assert!(wcx.debug_bounds(tab).is_some(), "工具行应含 {tab} 页");
    }
    // Summary 结构:Hierarchy(Request #N + Assistant Message
    // 跳转)+ Request Timing / Timing 小节 + Payload/Result 预览
    assert!(
        wcx.debug_bounds("goto-request").is_some(),
        "工具 Summary 应含 Request #N 层级跳转"
    );
    assert!(
        wcx.debug_bounds("goto-message").is_some(),
        "工具 Summary 应含 Assistant Message 层级跳转"
    );
    for sec in ["sec-req-timing", "sec-timing", "sec-payload", "sec-result"] {
        assert!(
            wcx.debug_bounds(sec).is_some(),
            "工具 Summary 应含 {sec} 小节"
        );
    }
    // 选中态:同实体(记录 4)
    assert_eq!(
        cx.update(|app| store.read(app).trajectory.inspector),
        Some(InspectTarget::Record(4))
    );
    // 新数据面页:工具行带 Schema 页(快照 schema_detail 在场)
    assert!(
        wcx.debug_bounds("inspector-tab-schema").is_some(),
        "工具行应含 Schema 页"
    );

    // SYSTEM 行(System Prompt / Tools 两页,
    // 无 Summary;initial 记录无前序快照 → 无 Diff 页
    cx.update(|app| {
        store.update(app, |st, cx| st.select_trajectory_record(1, cx));
    });
    redraw(cx, &mut wcx);
    for tab in ["inspector-tab-system", "inspector-tab-tools"] {
        assert!(wcx.debug_bounds(tab).is_some(), "SYSTEM 行应含 {tab} 页");
    }
    assert!(
        wcx.debug_bounds("inspector-tab-summary").is_none(),
        "SYSTEM 行照源不应有 Summary 页"
    );
    // initial 记录无前序快照 → 无 Diff 页
    assert!(
        wcx.debug_bounds("inspector-tab-diff").is_none(),
        "initial 记录不应有 Diff 页"
    );

    // 点 Request 圆点 → 请求模式(Summary/Usage/Timing)
    click_sel(&mut wcx, "traj-request-1");
    redraw(cx, &mut wcx);
    assert_eq!(
        cx.update(|app| store.read(app).trajectory.inspector),
        Some(InspectTarget::Request(1))
    );
    assert!(
        wcx.debug_bounds("inspector-tab-usage").is_some(),
        "请求应含 Usage 页"
    );

    // CONTEXT 行(源 isMarkdownRecord):Summary / Preview / Raw / Source
    // 四页;Summary 含 Source ›(跳 Source)与 Preview 小节
    cx.update(|app| {
        store.update(app, |st, cx| st.select_trajectory_record(5, cx));
    });
    redraw(cx, &mut wcx);
    for tab in [
        "inspector-tab-summary",
        "inspector-tab-preview",
        "inspector-tab-raw",
        "inspector-tab-source",
    ] {
        assert!(wcx.debug_bounds(tab).is_some(), "CONTEXT 行应含 {tab} 页");
    }
    assert!(
        wcx.debug_bounds("goto-source").is_some(),
        "CONTEXT Summary 应含 Source › 跳转"
    );
    assert!(
        wcx.debug_bounds("sec-preview").is_some(),
        "CONTEXT Summary 应含 Preview 小节"
    );
    // Preview 页:注入文本 markdown 渲染
    cx.update(|app| {
        store.update(app, |st, cx| st.set_inspector_tab("preview", cx));
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("traj-preview-5").is_some(),
        "Preview 页应渲染注入文本"
    );

    // MESSAGE 行(源 isMarkdownRecord):Summary / Preview / Raw 三页;
    // Summary 的 Source ›(Request #1)+ Status + Preview 小节
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.select_trajectory_record(3, cx);
            st.set_inspector_tab("summary", cx);
        });
    });
    redraw(cx, &mut wcx);
    for tab in [
        "inspector-tab-summary",
        "inspector-tab-preview",
        "inspector-tab-raw",
    ] {
        assert!(wcx.debug_bounds(tab).is_some(), "MESSAGE 行应含 {tab} 页");
    }
    assert!(
        wcx.debug_bounds("goto-request").is_some(),
        "MESSAGE Summary 的 Source 应跳 Request #1"
    );
    assert!(
        wcx.debug_bounds("sec-preview").is_some(),
        "MESSAGE Summary 应含 Preview 小节"
    );
    // Preview 页:正文渲染 + 同步工具调用行(Step 1 的 bash)
    cx.update(|app| {
        store.update(app, |st, cx| st.set_inspector_tab("preview", cx));
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("traj-preview-3").is_some(),
        "Preview 页应渲染正文"
    );
    assert!(
        wcx.debug_bounds("assistant-call-4").is_some(),
        "Preview 页应含工具调用行(可跳工具记录)"
    );
    // Raw 页:SourceBlocks 形态,tool-call 块头带跳转
    cx.update(|app| {
        store.update(app, |st, cx| st.set_inspector_tab("raw", cx));
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("block-jump-4").is_some(),
        "Raw 页 tool-call 块头应可跳工具记录"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 会话行 ⋯ 菜单:根级渲染(行内 absolute 被侧栏卡裁剪 + 内容卡
/// 遮挡);点击钮 → 菜单卡在场且锚在点击点左下
#[gpui_kit::test]
fn session_row_menu_renders_at_root(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "rowmenu");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };
    let btn = wcx
        .debug_bounds("row-menu-btn-0")
        .expect("⋯ 钮缺失(首个会话行)");
    click_sel(&mut wcx, "row-menu-btn-0");
    redraw(cx, &mut wcx);
    let card = wcx.debug_bounds("row-menu-card").expect("菜单卡未渲染");
    // 锚定:卡顶在钮下方、卡整体在钮左侧(向左展开避开右缘)
    assert!(
        f32::from(card.origin.y) > f32::from(btn.origin.y),
        "菜单应在钮下方"
    );
    assert!(
        f32::from(card.right()) <= f32::from(btn.origin.x) + 8.,
        "菜单应向左展开: card.right={:?} btn.x={:?}",
        card.right(),
        btn.origin.x
    );
    // 导出入口已从顶栏药丸移入菜单(该窗口从未渲染过药丸,缺席可断言)
    assert!(
        wcx.debug_bounds("导出日志").is_some(),
        "菜单应含「导出日志」项"
    );
    assert!(
        wcx.debug_bounds("export-log").is_none(),
        "顶栏不应再有导出药丸"
    );
    // 删除项在场;「关闭菜单」已撤(外点即关,该项多余)
    assert!(wcx.debug_bounds("删除").is_some(), "菜单应含「删除」项");
    assert!(
        wcx.debug_bounds("关闭菜单").is_none(),
        "「关闭菜单」项应已移除"
    );
    let _ = store;
    let _ = std::fs::remove_dir_all(root);
}

/// 删除会话端到端:第二个会话行菜单「删除」→ 日志文件移除、清单
/// 收缩、当前会话不受影响
#[gpui_kit::test]
fn session_row_menu_deletes_session(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "del");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };
    // 再建两个会话;清单最新在前 → 行序 [s3(当前), s2, s1]
    let (s2, s3) = cx.update(|app| {
        store.update(app, |st, cx| {
            let s2 = st.bridge.host().create_session(None, None, None);
            let s3 = st.bridge.host().create_session(None, None, None);
            st.refresh_list();
            st.open_session(&s3, cx);
            (s2, s3)
        })
    });
    redraw(cx, &mut wcx);
    let first = cx.update(|app| {
        let st = store.read(app);
        st.state
            .sessions
            .iter()
            .map(|s| s.session_id.clone())
            .find(|id| id != &s2 && id != &s3)
            .expect("应存在首个会话")
    });

    // 穿透探针(强断言):打开中间行 s2 的菜单(卡体盖住下方的
    // 另一会话行区域),点卡内空白垫区——occlude 必须挡住命中,
    // 否则点击会落到后面行把 current 切走。按行 id 几何定位 ⋯ 钮
    // (清单排序在同毫秒创建时不稳定,不假设行序)
    let s2_sel: &'static str = Box::leak(format!("session-row-{s2}").into_boxed_str());
    let s2_row = wcx.debug_bounds(s2_sel).expect("s2 行缺失");
    let s2_dots = gpui_kit::Point {
        x: s2_row.right() - px(14.),
        y: s2_row.origin.y + s2_row.size.height / 2.,
    };
    wcx.simulate_click(s2_dots, gpui_kit::Modifiers::default());
    redraw(cx, &mut wcx);
    let card = wcx.debug_bounds("row-menu-card").expect("菜单卡缺失");
    let blank = gpui_kit::Point {
        x: card.origin.x + px(2.),
        y: card.origin.y + px(40.),
    };
    wcx.simulate_click(blank, gpui_kit::Modifiers::default());
    redraw(cx, &mut wcx);
    assert_eq!(
        cx.update(|app| store.read(app).state.current_id.clone()),
        Some(s3.clone()),
        "菜单区点击不得穿透选中后方会话行"
    );

    // 菜单仍在(垫区点击只挡不关);点「删除」→ 确认模态 → 确认
    assert!(
        cx.update(|app| store.read(app).sessions.menu_open_session.is_some()),
        "卡内空白点击不应关菜单"
    );
    click_sel(&mut wcx, "删除");
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("delete-confirm").is_some(),
        "删除应先弹确认模态"
    );
    assert_eq!(
        cx.update(|app| store.read(app).sessions.delete_target.clone()),
        Some(s2.clone()),
        "确认目标应为 s2"
    );
    // 先验证取消路径:点「取消」不删除
    click_sel(&mut wcx, "delete-cancel");
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store
            .read(app)
            .state
            .sessions
            .iter()
            .any(|s| s.session_id == s2)),
        "取消不应删除"
    );
    // 再走确认路径(取消后菜单已收,重开 s2 菜单再确认)
    wcx.simulate_click(s2_dots, gpui_kit::Modifiers::default());
    redraw(cx, &mut wcx);
    click_sel(&mut wcx, "删除");
    redraw(cx, &mut wcx);
    click_sel(&mut wcx, "delete-confirm");
    redraw(cx, &mut wcx);
    let ids = cx.update(|app| {
        store
            .read(app)
            .state
            .sessions
            .iter()
            .map(|s| s.session_id.clone())
            .collect::<Vec<_>>()
    });
    assert!(!ids.contains(&s2), "s2 应被删除: {ids:?}");
    assert!(ids.contains(&first) && ids.contains(&s3), "其余会话保留");
    assert_eq!(
        cx.update(|app| store.read(app).state.current_id.clone()),
        Some(s3),
        "删除非当前会话不影响当前"
    );
    assert!(
        cx.update(|app| store.read(app).sessions.menu_open_session.is_none()),
        "动作后菜单应收起"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 分离式侧栏(方案 B):侧栏卡满高(顶到窗口顶、底到窗口底),
/// 标题行/状态栏只占右列——状态栏不压缩侧栏,侧栏顶高出内容卡
#[gpui_kit::test]
fn detached_sidebar_full_height(cx: &mut TestAppContext) {
    let (_store, mut wcx, root) = menu_harness(cx, "detach");
    wcx.refresh().expect("刷新失败");
    cx.run_until_parked();
    let sb = wcx.debug_bounds("sidebar-card").expect("侧栏卡缺失");
    let cc = wcx.debug_bounds("content-card").expect("内容卡缺失");
    let st = wcx.debug_bounds("statusbar").expect("状态栏缺失");
    // 侧栏顶高出内容卡(不被标题行压缩;仅 8px 窗口边距)
    assert!(
        f32::from(sb.origin.y) < f32::from(cc.origin.y),
        "侧栏应顶到窗口顶: sb.top={:?} cc.top={:?}",
        sb.origin.y,
        cc.origin.y
    );
    // 侧栏底越过状态栏上缘(不被状态栏压缩)
    assert!(
        f32::from(sb.bottom()) > f32::from(st.origin.y),
        "侧栏应满高到窗口底: sb.bottom={:?} statusbar.top={:?}",
        sb.bottom(),
        st.origin.y
    );
    // 状态栏只在内容区(左缘在侧栏右侧)
    assert!(
        f32::from(st.origin.x) >= f32::from(sb.right()),
        "状态栏应仅占右列: st.x={:?} sb.right={:?}",
        st.origin.x,
        sb.right()
    );
    // 拖拽条在场(交通灯让位区)
    assert!(
        wcx.debug_bounds("sidebar-drag-strip").is_some(),
        "侧栏顶部拖拽条缺失"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 侧栏拖宽:右缘把手拖拽 → sidebar_px 变化 + clamp 到源范围
/// [SIDEBAR_MIN, SIDEBAR_MAX],展开态把手在场、折叠态无把手。
#[gpui_kit::test]
fn sidebar_resize_drag(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "sbresize");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.run_until_parked();
    };
    redraw(cx, &mut wcx);

    // 初始宽(默认 280)与把手都在场
    let init_w = cx.update(|app| store.read(app).sidebar_px);
    assert_eq!(
        init_w,
        crate::shell::metrics::SIDEBAR_W,
        "初始宽应为默认 280"
    );
    assert!(
        wcx.debug_bounds("sidebar-resize").is_some(),
        "展开态右缘应有拖宽把手"
    );

    // 拖拽:在把手中心按下 → 右移 60px → 释放。把手在侧栏右缘外侧 4px,
    // 命中区 8px;取把手 bounds 中心起点。
    let handle = wcx
        .debug_bounds("sidebar-resize")
        .expect("拖宽把手 bounds 缺失");
    let start = gpui_kit::Point {
        x: handle.origin.x + handle.size.width / 2.,
        y: handle.origin.y + handle.size.height / 2.,
    };
    let end = gpui_kit::Point {
        x: start.x + gpui_kit::px(60.),
        y: start.y,
    };
    wcx.simulate_mouse_down(
        start,
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    wcx.simulate_mouse_move(
        end,
        Some(gpui_kit::MouseButton::Left),
        gpui_kit::Modifiers::default(),
    );
    wcx.simulate_mouse_up(
        end,
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    redraw(cx, &mut wcx);

    let after_w = cx.update(|app| store.read(app).sidebar_px);
    let after_bounds = wcx.debug_bounds("sidebar-card").expect("侧栏卡缺失");
    // 锚点 = 光标 x + 起始宽:向右拖 +60 → sidebar_px ≈ 280+60=340
    assert!(
        after_w > init_w,
        "右拖后宽度应增大: init={init_w} after={after_w}"
    );
    assert_eq!(after_w, crate::shell::metrics::clamp_sidebar(after_w));
    assert!(
        f32::from(after_bounds.size.width) >= crate::shell::metrics::SIDEBAR_MIN
            && f32::from(after_bounds.size.width) <= crate::shell::metrics::SIDEBAR_MAX,
        "侧栏卡宽应在 clamp 区间: {:?}",
        after_bounds.size.width
    );
    assert!(
        f32::from(after_bounds.size.width) > init_w - 1.,
        "侧栏卡随拖宽变宽: {:?} > {init_w:?}",
        after_bounds.size.width
    );

    // 折叠:宽字段不受影响(独立 bool;debug_bounds 只增不清,折叠态
    // 的 rail 复用 sidebar-card 名,宽度断言不可靠——此处只断 store 状态)
    cx.update(|app| store.update(app, |st, cx| st.toggle_sidebar(cx)));
    redraw(cx, &mut wcx);
    let collapsed = cx.update(|app| store.read(app).sidebar_collapsed);
    assert!(collapsed, "折叠态应为 true");
    let collapsed_px = cx.update(|app| store.read(app).sidebar_px);
    assert_eq!(collapsed_px, after_w, "折叠不写 0,宽字段保留");

    // 再展开:保留自定宽(折叠/展开后记住自定宽)
    cx.update(|app| store.update(app, |st, cx| st.toggle_sidebar(cx)));
    redraw(cx, &mut wcx);
    let re_w = cx.update(|app| store.read(app).sidebar_px);
    assert_eq!(re_w, after_w, "折叠/展开后应记住自定宽");
    assert!(
        !cx.update(|app| store.read(app).sidebar_collapsed),
        "再展开应为展开态"
    );

    let _ = std::fs::remove_dir_all(root);
}

/// 计划审批卡(**选择→批准两步**:
/// 点选项行只标记选择不提交,底部「批准」钮才提交):卡不内嵌计划正文
/// (plan-detail 不存在),「查看」开右栏计划标签;①是,实施此计划
/// ②否,并告诉它应该如何做不同(选中②展开行内输入;有反馈 = 拒绝+
/// 反馈,空反馈 = 仅拒绝——原「跳过」语义并入);✕ = 取消请求。
#[gpui_kit::test]
fn plan_review_compact_card_two_options(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "plan-compact");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };
    // 预置用户行:hero 空会话不渲染 bottom-stack,审批卡无载体
    let seed_user = |cx: &mut TestAppContext, store: &Entity<AppStore>| {
        cx.update(|app| {
            store.update(app, |st, _| {
                let id = st.state.current_id.clone().unwrap();
                let chat = st.state.chats.entry(id.clone()).or_default();
                chat.nodes.push(ChatNode::User {
                    key: format!("user:seed-{}", chat.nodes.len()),
                    text: "先聊着".into(),
                    images: vec![],
                });
            });
        });
    };
    let seed_plan = |cx: &mut TestAppContext, store: &Entity<AppStore>| {
        cx.update(|app| {
            store.update(app, |st, _| {
                let id = st.state.current_id.clone().unwrap();
                st.state.pending_plan = Some(crate::shell::reducer::PendingPlan {
                    rpc_id: "rpc-plan".into(),
                    session_id: id,
                    question: dsh_core::proto::Question {
                        id: "plan-1".into(),
                        question: "批准该计划?".into(),
                        header: Some("测试: 计划待批准".into()),
                        // 长 detail 在场:证明正文不再内嵌(计划/批准分离)
                        detail: Some("## 长计划\n计划正文".to_string()),
                        options: None,
                        multi_select: None,
                        intent: Some(serde_json::json!("plan-review")),
                        data: None,
                    },
                });
            });
        });
    };
    seed_user(cx, &store);
    seed_plan(cx, &store);
    redraw(cx, &mut wcx);
    // 计划/批准分离:正文容器不应存在;查看入口与 ✕ 在场
    assert!(
        wcx.debug_bounds("plan-detail").is_none(),
        "审批卡不应内嵌计划正文(计划/批准分离)"
    );
    let card = wcx.debug_bounds("plan-review").expect("卡根应渲染");
    let view = wcx.debug_bounds("plan-view").expect("查看入口应渲染");
    let dismiss = wcx.debug_bounds("plan-dismiss").expect("✕ 应渲染");
    let approve = wcx.debug_bounds("plan-approve").expect("选项1应渲染");
    let decline = wcx.debug_bounds("plan-decline").expect("选项2应渲染");
    let confirm = wcx.debug_bounds("plan-confirm").expect("批准钮应渲染");
    assert!(
        approve.top() < decline.top() && decline.top() < confirm.top(),
        "纵向序应为 ①②批准钮"
    );
    assert!(
        view.right() <= dismiss.left() && approve.top() >= card.top(),
        "查看在 ✕ 之前、选项在卡内,view={view:?} dismiss={dismiss:?}"
    );
    // 未选择直接点批准 = no-op(防误触)
    click_sel(&mut wcx, "plan-confirm");
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store.read(app).state.pending_plan.is_some()),
        "未选择时批准钮不应生效"
    );
    // 选择 ≠ 批准:点选项 1 只标记选择,pending 仍在
    // (旧病:一点选项就批准)
    click_sel(&mut wcx, "plan-approve");
    redraw(cx, &mut wcx);
    let (pending, selection) = cx.update(|app| {
        let s = store.read(app);
        (s.state.pending_plan.is_some(), s.ask.plan_selection)
    });
    assert!(pending, "点选项 1 不应直接批准");
    assert_eq!(selection, Some(true), "点选项 1 应标记选择态");
    // 点「批准」才提交 → pending 清空
    click_sel(&mut wcx, "plan-confirm");
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store.read(app).state.pending_plan.is_none()),
        "确认后 pending 应清空"
    );
    // ② 流程:选中 → 输入展开 → 空反馈提交 = 仅拒绝(原「跳过」并入)
    seed_plan(cx, &store);
    redraw(cx, &mut wcx);
    click_sel(&mut wcx, "plan-decline");
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("plan-decline-input").is_some(),
        "选中②应展开行内输入"
    );
    let pending = cx.update(|app| store.read(app).state.pending_plan.is_some());
    assert!(pending, "选②不应直接提交");
    click_sel(&mut wcx, "plan-confirm");
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store.read(app).state.pending_plan.is_none()),
        "空反馈提交(仅拒绝)后 pending 应清空"
    );
    // ② + 反馈:输入 → 点批准 = 拒绝+反馈提交
    seed_plan(cx, &store);
    redraw(cx, &mut wcx);
    click_sel(&mut wcx, "plan-decline");
    redraw(cx, &mut wcx);
    click_sel(&mut wcx, "plan-decline-input");
    wcx.run_until_parked();
    wcx.simulate_input("把登录改成 OAuth,不要自研");
    click_sel(&mut wcx, "plan-confirm");
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store.read(app).state.pending_plan.is_none()),
        "反馈提交后 pending 应清空"
    );
    assert!(
        cx.update(|app| store.read(app).ask.plan_decline_input.is_none()),
        "提交后输入态应清空(下一张卡全新)"
    );
    // 「查看」→ 右栏开 + 计划标签(复用查看链路)
    seed_plan(cx, &store);
    redraw(cx, &mut wcx);
    click_sel(&mut wcx, "plan-view");
    redraw(cx, &mut wcx);
    let (open, active) = cx.update(|app| {
        let s = store.read(app);
        (s.panel_open, s.panel_active_tab)
    });
    assert!(open, "查看应开右栏");
    assert_eq!(
        active,
        Some(crate::shell::panel::PanelTab::Plan),
        "查看应激活计划标签"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 面板拖宽协商:窄于 PANEL_MIN(540,320→640 实测
/// 过宽→终值 540)就地抬到下限 → 加宽越过当前形态上限时自动收左栏
/// (280→56)→ 继续加宽至新上限。以 store 动作直接驱动(viewport 经
/// begin 固定;2000 视口:展开态上限 = 2000−280−748 = 972,收起态 =
/// 2000−56−748 = 1196;面板默认 = 下限 540)
#[gpui_kit::test]
fn panel_resize_negotiation_collapses_sidebar(cx: &mut TestAppContext) {
    let (store, _wcx, root) = menu_harness(cx, "panel-nego");
    let viewport = 2000.;
    // 阶段0:向右拖缩窄(want = 540−400 < 0)→ 就地抬到下限 540,不收左栏
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.toggle_panel(cx);
            st.panel_resize_begin(1000., viewport, cx);
            st.panel_resize_move(1400., cx);
        });
    });
    let px0 = cx.update(|app| store.read(app).panel_px);
    assert!(
        (px0 - 540.).abs() < 1.,
        "窄于下限应抬到 PANEL_MIN,px0={px0}"
    );
    assert!(
        !cx.update(|app| store.read(app).sidebar_collapsed),
        "触下限不越上限,不应收左栏"
    );
    // 阶段1:加宽到 want 860(cursor 680;972 上限内)→ 面板随动,左栏不动
    cx.update(|app| {
        store.update(app, |st, cx| st.panel_resize_move(680., cx));
    });
    let (collapsed1, px1) = cx.update(|app| {
        let s = store.read(app);
        (s.sidebar_collapsed, s.panel_px)
    });
    assert!((px1 - 860.).abs() < 1., "上限内面板随拖加宽,px1={px1}");
    assert!(!collapsed1, "阶段1内不应收起左栏");
    // 阶段2:继续加宽到 want 1260(> 972)→ 自动收左栏,面板 1252(新上限
    // = 2000 − 0(收起隐藏)− 748)
    cx.update(|app| {
        store.update(app, |st, cx| st.panel_resize_move(280., cx));
    });
    let (collapsed2, px2) = cx.update(|app| {
        let s = store.read(app);
        (s.sidebar_collapsed, s.panel_px)
    });
    assert!(collapsed2, "越上限应自动收起左栏");
    assert!((px2 - 1252.).abs() < 1., "收左栏后面板到协商上限,px2={px2}");
    let _ = std::fs::remove_dir_all(root);
}

/// 面板拖宽全链路(真鼠标事件链:把手 mousedown → 窗口级 move → up):
/// 加宽随动 → 越当前形态上限自动收左栏 → 抬起收尾;渲染宽 = 协商宽
/// (无固定上限墙)。窗口 1600:展开态上限 572,收起态(隐藏)852。
#[gpui_kit::test]
fn panel_drag_widens_via_mouse_and_negotiates(cx: &mut TestAppContext) {
    let (store, wcx, root) = menu_harness(cx, "panel-drag");
    let mut wcx = wcx;
    wcx.simulate_resize(gpui_kit::size(gpui_kit::px(1600.), gpui_kit::px(1000.)));
    wcx.run_until_parked();
    wcx.refresh().expect("窗口刷新失败");
    wcx.run_until_parked();

    // 打开面板 → 把手在场
    cx.update(|app| {
        store.update(app, |st, cx| st.toggle_panel(cx));
    });
    wcx.refresh().expect("窗口刷新失败");
    wcx.run_until_parked();
    let handle = wcx.debug_bounds("panel-resize").expect("拖宽把手应在场");
    let start = gpui_kit::point(
        handle.origin.x + handle.size.width / 2.,
        handle.origin.y + handle.size.height / 2.,
    );

    // 面板默认 = 下限 540。按下 → 左移 100(want 640 > 展开态上限 572)
    // → 自动收左栏,面板 640(≤ 收起态上限 796)
    wcx.simulate_mouse_down(
        start,
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    wcx.run_until_parked();
    wcx.refresh().expect("按下后刷新失败");
    wcx.run_until_parked();
    wcx.simulate_mouse_move(
        gpui_kit::point(start.x - px(100.), start.y),
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    wcx.run_until_parked();
    let (px1, collapsed) = cx.update(|app| {
        let s = store.read(app);
        (s.panel_px, s.sidebar_collapsed)
    });
    assert!((px1 - 640.).abs() < 1., "面板随拖加宽到 640,px1={px1}");
    assert!(collapsed, "越展开态上限应自动收起左栏");

    // 继续左移至 want 840(≤ 收起态上限 852):上限内随动(收起侧栏
    // 不占宽,可再宽 56)
    wcx.simulate_mouse_move(
        gpui_kit::point(start.x - px(300.), start.y),
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    wcx.run_until_parked();
    wcx.refresh().expect("拖拽中刷新失败");
    wcx.run_until_parked();
    let (collapsed2, px2) = cx.update(|app| {
        let s = store.read(app);
        (s.sidebar_collapsed, s.panel_px)
    });
    assert!(collapsed2, "左栏应保持收起");
    assert!((px2 - 840.).abs() < 1., "面板随动到 840,px2={px2}");
    let col = wcx.debug_bounds("right-panel").expect("面板列应渲染");
    assert!(
        (col.size.width - px(px2)).abs() < px(1.),
        "渲染宽应等于协商宽(无固定上限墙),col={col:?} px2={px2}"
    );

    // 抬起 → 拖拽态收尾(锚点清位)
    wcx.simulate_mouse_up(
        gpui_kit::point(start.x - px(300.), start.y),
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    wcx.run_until_parked();
    assert!(
        cx.update(|app| store.read(app).panel_resize_anchor)
            .is_none(),
        "抬起应结束拖宽"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 最窄窗口(960×640 = window_min_size)布局协商(面板开着
/// 拖到最窄,导航轨/滚动条双双漂进内容区——面板列 flex_shrink_0 固定
/// 意愿宽不让位,聊天列被压破 MIN_COL)。回归口径:面板渲染宽必须真
/// 让位 = min(意愿宽, viewport − 侧栏 − MIN_COL) 且不让位到负数;
/// 聊天列拿足下限;宽窗行为不变。
#[gpui_kit::test]
fn narrow_window_panel_yields_and_column_holds(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "narrow-win");
    // 离开 hero 态 + 内容明显可滚(导航轨显示条件 = 有锚点且
    // scrollable > 视口 1/4):注入多条长消息
    cx.update(|app| {
        store.update(app, |st, cx| {
            use crate::features::chat::projection::{ChatNode, ChatState};
            let id = st.state.current_id.clone().expect("当前会话");
            let mut chat = ChatState::default();
            for ix in 0..4 {
                chat.nodes.push(ChatNode::User {
                    key: format!("user:{ix}"),
                    text: big_md(&format!("窄窗布局 {ix}")),
                    images: vec![],
                });
            }
            st.state.chats.insert(id, chat);
            cx.notify();
        })
    });
    let resize = |wcx: &mut gpui_kit::VisualTestContext, w: f32| {
        wcx.simulate_resize(gpui_kit::size(gpui_kit::px(w), gpui_kit::px(640.)));
        wcx.run_until_parked();
        wcx.refresh().expect("窗口刷新失败");
        wcx.run_until_parked();
    };
    let open_panel = |cx: &mut TestAppContext, store: &Entity<AppStore>| {
        cx.update(|app| store.update(app, |st, cx| st.toggle_panel(cx)));
    };

    // 场景 A:960 窗 + 收起侧栏(完全隐藏 = 0)+ 面板意愿 540:
    // 让位宽 = 960 − 0 − 748 = 212,面板渲染 212,聊天列拿足 748
    resize(&mut wcx, 960.);
    open_panel(cx, &store);
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.sidebar_collapsed = true;
            cx.notify();
        })
    });
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("sidebar-card").is_none(),
        "收起后侧栏应完全隐藏"
    );
    let panel = wcx.debug_bounds("right-panel").expect("面板列应渲染");
    assert!(
        (panel.size.width - px(212.)).abs() < px(1.),
        "窄窗面板应让位到 212,panel={panel:?}"
    );
    let card = wcx.debug_bounds("content-card").expect("内容区应渲染");
    assert!(
        (card.size.width - px(748.)).abs() < px(1.),
        "聊天列应拿足 MIN_COL=748,card={card:?}"
    );
    let rail = wcx.debug_bounds("nav-rail").expect("导航轨应渲染");
    assert!(
        (rail.origin.x - card.origin.x).abs() < px(1.),
        "导航轨应贴内容区左缘,rail.x={} card.x={}",
        rail.origin.x,
        card.origin.x
    );
    let gap = panel.origin.x - bounds_right(card);
    assert!(
        gap.abs() < px(1.),
        "滚动条挂点(content-card 右缘)应贴面板左缘,gap={gap:?} card={card:?} panel={panel:?}"
    );

    // 场景 B:同窗展开侧栏(280):960 − 280 − 748 < 0 → 面板让位到 0
    // (不渲染),聊天区全宽可用(680)
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.sidebar_collapsed = false;
            cx.notify();
        })
    });
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("right-panel").is_none(),
        "让位到 0 的面板不应渲染"
    );
    let card_b = wcx.debug_bounds("content-card").expect("内容区应渲染");
    assert!(
        (card_b.size.width - px(680.)).abs() < px(1.),
        "面板让没后内容区应全宽 680,card={card_b:?}"
    );
    let rail_b = wcx.debug_bounds("nav-rail").expect("导航轨应渲染");
    assert!(
        (rail_b.origin.x - card_b.origin.x).abs() < px(1.),
        "导航轨应贴内容区左缘"
    );

    // 场景 C:宽窗(1800)不变量:面板意愿 540 足额渲染,聊天列 748
    resize(&mut wcx, 1800.);
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.sidebar_collapsed = true;
            cx.notify();
        })
    });
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    let panel_c = wcx.debug_bounds("right-panel").expect("宽窗面板应渲染");
    assert!(
        (panel_c.size.width - px(540.)).abs() < px(1.),
        "宽窗面板应足额 540,panel={panel_c:?}"
    );
    let card_c = wcx.debug_bounds("content-card").expect("内容区应渲染");
    assert!(
        (card_c.size.width - px(1800. - 540.)).abs() < px(1.),
        "宽窗内容区 = 视口 − 面板(收起侧栏不占宽),card={card_c:?}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 最窄窗「边槽」回归(锚点和滚动条侵入内容区域:
/// 960 窗侧栏展开、列吃满内容区——刻度叠在文字左缘、滚动
/// 条 thumb 压着文字右缘)。锁:消息列左缘须让过锚点带最宽刻度(起点
/// 24 + 激活 26),右缘须让开滚动条槽(content 右缘 − SCROLLBAR_GUTTER)。
#[gpui_kit::test]
fn narrow_window_gutters_keep_anchors_and_scrollbar_out_of_text(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "edge-gutters");
    // 多条长消息:内容可滚(导航轨显示)+ node-0 在场
    cx.update(|app| {
        store.update(app, |st, cx| {
            use crate::features::chat::projection::{ChatNode, ChatState};
            let id = st.state.current_id.clone().expect("当前会话");
            let mut chat = ChatState::default();
            for ix in 0..4 {
                chat.nodes.push(ChatNode::User {
                    key: format!("user:{ix}"),
                    text: big_md(&format!("边槽 {ix}")),
                    images: vec![],
                });
            }
            st.state.chats.insert(id, chat);
            cx.notify();
        })
    });
    wcx.simulate_resize(gpui_kit::size(gpui_kit::px(960.), gpui_kit::px(640.)));
    wcx.run_until_parked();
    wcx.refresh().expect("窗口刷新失败");
    wcx.run_until_parked();

    let card = wcx.debug_bounds("content-card").expect("内容区应渲染");
    // Bottom 对齐虚拟化:顶部行滚出视口不绘制,取任一在场行断言
    let row = (0..4)
        .find_map(|ix| wcx.debug_bounds(Box::leak(format!("node-{ix}").into_boxed_str())))
        .expect("至少一条消息行应在场");
    // 左槽:刻度最右缘(content 左 + 起点 24 + 激活宽 26)不得触文字
    let tick_right = card.origin.x + px(24. + 26.);
    assert!(
        row.origin.x >= tick_right,
        "锚点刻度侵入文字列:刻度右缘 {} ≥ 行左缘 {}",
        tick_right,
        row.origin.x
    );
    // 右槽:滚动条占位(content 右 − 槽宽)不得压文字
    let thumb_left = bounds_right(card) - px(crate::shell::metrics::SCROLLBAR_GUTTER_W);
    assert!(
        bounds_right(row) <= thumb_left,
        "滚动条侵入文字列:行右缘 {} > 滚动条槽左缘 {}",
        bounds_right(row),
        thumb_left
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 全高轨道滚动映射回归(「滚动条不能拉到底部」):
/// 组件库 thumb 满行程以 (content_size − 轨道高) 为滚动域,且轨道默认
/// resolve 到列表视口(缩在列表段)。锁:viewport_from_layout 下轨道 =
/// content-card 全列,FullTrackHandle 补偿后 content_size − track_h ==
/// ListState 真实可滚量(max_offset)——拖到轨道底 == 列表滚到真底。
#[gpui_kit::test]
fn full_track_scrollbar_reaches_true_bottom(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "full-track");
    cx.update(|app| {
        store.update(app, |st, cx| {
            use crate::features::chat::projection::{ChatNode, ChatState};
            let id = st.state.current_id.clone().expect("当前会话");
            let mut chat = ChatState::default();
            for ix in 0..4 {
                chat.nodes.push(ChatNode::User {
                    key: format!("user:{ix}"),
                    text: big_md(&format!("全轨 {ix}")),
                    images: vec![],
                });
            }
            st.state.chats.insert(id, chat);
            cx.notify();
        })
    });
    wcx.simulate_resize(gpui_kit::size(gpui_kit::px(960.), gpui_kit::px(640.)));
    wcx.run_until_parked();
    wcx.refresh().expect("窗口刷新失败");
    wcx.run_until_parked();

    cx.update(|app| {
        let st = store.read(app);
        let viewport = f32::from(st.chat.chat_list.viewport_bounds().size.height);
        let track = st.chat.track_h;
        let max_off = f32::from(st.chat.chat_list.max_offset_for_scrollbar().y);
        assert!(max_off > 200., "测试前提:内容明显可滚,max_off={max_off}");
        assert!(
            track > viewport + 100.,
            "轨道应明显长于列表视口(全列),track={track} viewport={viewport}"
        );
        // FullTrackHandle 报告的 content_size − 轨道高 == 真实可滚量:
        // 组件库拖拽把 thumb 推到轨道底时给 set_offset 恰为 max_offset
        let extra = (track - viewport).max(0.);
        let reported = viewport + max_off + extra;
        assert!(
            (reported - track - max_off).abs() < 1.,
            "拖到底应等于列表真底:reported−track={} ≠ max_offset={max_off}",
            reported - track
        );
    });
    let _ = std::fs::remove_dir_all(root);
}

/// 面板标签生命周期:开计划标签 → 标签条 +
/// 计划视图在场;关最后标签 → 空态快捷菜单在场且面板列不自动收;
/// 空态行点击 → 计划视图恢复
#[gpui_kit::test]
fn panel_tab_lifecycle_and_empty_menu(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "panel-tabs");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };
    use crate::shell::panel::PanelTab;
    // 开面板 + 计划标签(⇧⌘P/「+」/空态行/chat chip 最终都归此动作)
    cx.update(|app| {
        store.update(app, |st, cx| st.open_panel_tab(PanelTab::Plan, cx));
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("panel-tab-plan").is_some(),
        "标签条应有计划标签"
    );
    assert!(
        wcx.debug_bounds("panel-plan-view").is_some(),
        "计划视图应在场"
    );
    // 「+」紧随标签条之后(不挂右缘控制组)
    let tab = wcx.debug_bounds("panel-tab-plan").expect("计划标签应在场");
    let plus = wcx.debug_bounds("panel-plus").expect("「+」应在场");
    assert!(
        plus.origin.x >= tab.right() - px(2.) && plus.origin.x <= tab.right() + px(40.),
        "「+」应紧随标签条,tab.right={:?} plus.x={:?}",
        tab.right(),
        plus.origin.x
    );
    // 关掉最后标签 → 空态菜单;面板不自动收
    cx.update(|app| {
        store.update(app, |st, cx| st.close_panel_tab(PanelTab::Plan, cx));
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("panel-empty-menu").is_some(),
        "关最后标签应显空态菜单"
    );
    assert!(
        wcx.debug_bounds("panel-plus").is_none(),
        "空态头部不应有「+」(三轮:默认标题与加号撤除)"
    );
    assert!(
        wcx.debug_bounds("panel-empty-row-plan").is_some(),
        "空态清单应有计划行"
    );
    let col = wcx.debug_bounds("right-panel").expect("面板列应在场");
    assert!(col.size.width > px(0.), "关最后标签不应收面板");
    // 空态行点击 → 计划标签恢复
    click_sel(&mut wcx, "panel-empty-row-plan");
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("panel-plan-view").is_some(),
        "空态行点击应开计划标签"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 面板「+」菜单:空态头部无「+」(三轮撤除默认标题与加号);有标签后
/// 点「+」→ 清单卡在场;点「计划」项 → 激活,菜单自关
#[gpui_kit::test]
fn panel_plus_menu_opens_plan(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "panel-plus");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };
    // 空态(无标签):正文显空态清单,头部无「+」
    cx.update(|app| {
        store.update(app, |st, cx| st.toggle_panel(cx));
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("panel-empty-menu").is_some(),
        "开面板无标签应显空态菜单"
    );
    assert!(
        wcx.debug_bounds("panel-plus").is_none(),
        "空态头部不应有「+」"
    );
    // 有标签后:「+」在标签条旁,点开清单
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.open_panel_tab(crate::shell::panel::PanelTab::Plan, cx)
        });
    });
    redraw(cx, &mut wcx);
    click_sel(&mut wcx, "panel-plus");
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("panel-plus-menu").is_some(),
        "「+」应开视图清单菜单"
    );
    click_sel(&mut wcx, "panel-plus-item-plan");
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("panel-plan-view").is_some(),
        "点计划项应开计划标签"
    );
    assert!(
        wcx.debug_bounds("panel-plus-menu").is_none(),
        "点项后清单菜单应自关"
    );
    assert_eq!(
        cx.update(|app| store.read(app).panel_active_tab),
        Some(crate::shell::panel::PanelTab::Plan),
        "计划标签应激活"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 轨迹标签收录(ALL 清单驱动):「+」菜单与空态清单
/// 均有轨迹行;点项/点行 → 轨迹面板标签激活且整页视图在场
#[gpui_kit::test]
fn panel_trajectory_tab_listing_and_entry(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "panel-traj");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };
    // 开面板 + 计划标签 → 点「+」开清单 → 轨迹项在场
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.open_panel_tab(crate::shell::panel::PanelTab::Plan, cx)
        });
    });
    redraw(cx, &mut wcx);
    click_sel(&mut wcx, "panel-plus");
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("panel-plus-item-trajectory").is_some(),
        "「+」清单应有轨迹项"
    );
    // 点轨迹项 → 轨迹标签激活 + 整页视图在场
    click_sel(&mut wcx, "panel-plus-item-trajectory");
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("panel-tab-trajectory").is_some(),
        "标签条应有轨迹标签"
    );
    assert!(
        wcx.debug_bounds("trajectory-view").is_some(),
        "轨迹整页视图应在面板内渲染"
    );
    assert_eq!(
        cx.update(|app| store.read(app).panel_active_tab),
        Some(crate::shell::panel::PanelTab::Trajectory),
        "轨迹标签应激活"
    );
    // 关掉全部标签(轨迹 + 计划)→ 空态清单有轨迹行;点行恢复
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.close_panel_tab(crate::shell::panel::PanelTab::Trajectory, cx);
            st.close_panel_tab(crate::shell::panel::PanelTab::Plan, cx);
        });
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("panel-empty-row-trajectory").is_some(),
        "空态清单应有轨迹行"
    );
    click_sel(&mut wcx, "panel-empty-row-trajectory");
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("trajectory-view").is_some(),
        "空态行点击应开轨迹标签"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// ⇧⌘P 键绑定:无焦点直接按键 → 面板开 + 计划标签激活(键表经
/// bind_global_keys 与 main.rs 同源;action 无焦点回落 root 分派路径)
#[gpui_kit::test]
fn panel_plan_shortcut_binding(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "panel-hotkey");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };
    assert!(!cx.update(|app| store.read(app).panel_open), "初始面板应关");
    wcx.simulate_keystrokes("shift-cmd-p");
    redraw(cx, &mut wcx);
    let (open, active) = cx.update(|app| {
        let s = store.read(app);
        (s.panel_open, s.panel_active_tab)
    });
    assert!(open, "⇧⌘P 应开面板");
    assert_eq!(
        active,
        Some(crate::shell::panel::PanelTab::Plan),
        "⇧⌘P 应激活计划标签"
    );
    assert!(wcx.debug_bounds("panel-plan-view").is_some());
    let _ = std::fs::remove_dir_all(root);
}

/// 计划卡「查看」chip:注入计划节点 → chip 在场;点击 → 面板开 +
/// 计划标签激活(且不触发行展开)
#[gpui_kit::test]
fn plan_card_view_chip_opens_panel(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "plan-view-chip");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };
    let sid = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("当前会话应在场");
    cx.update(|app| {
        store.update(app, |st, cx| {
            let chat = st.state.chats.entry(sid).or_default();
            chat.nodes.push(crate::features::chat::ChatNode::Plan {
                key: "plan:test-1".into(),
                plan: "# 计划\n\n1. 先做 A\n2. 再做 B".into(),
                status: crate::features::chat::PlanStatus::Approved,
            });
            cx.notify();
        });
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("plan-view-chip-0").is_some(),
        "计划卡应有「查看」chip"
    );
    click_sel(&mut wcx, "plan-view-chip-0");
    redraw(cx, &mut wcx);
    let (open, active) = cx.update(|app| {
        let s = store.read(app);
        (s.panel_open, s.panel_active_tab)
    });
    assert!(open, "点「查看」应开面板");
    assert_eq!(
        active,
        Some(crate::shell::panel::PanelTab::Plan),
        "点「查看」应激活计划标签"
    );
    assert!(
        wcx.debug_bounds("panel-plan-view").is_some(),
        "计划视图应渲染"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 面板开关归位:关态钮在标题栏;开态挪入面板头右缘
/// (原 ✕ 关闭钮撤除);点头部开关收起面板,开关回标题栏
#[gpui_kit::test]
fn panel_toggle_lives_in_header_when_open(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "panel-toggle-move");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };
    // 关态:标题栏有开关,面板列不渲染
    assert!(
        wcx.debug_bounds("panel-toggle").is_some(),
        "关态标题栏应有面板开关"
    );
    assert!(
        wcx.debug_bounds("right-panel").is_none(),
        "关态面板列不渲染"
    );
    // 打开:开关挪入面板头(位于面板列界内);✕ 关闭钮不再存在
    cx.update(|app| {
        store.update(app, |st, cx| st.toggle_panel(cx));
    });
    redraw(cx, &mut wcx);
    let rp = wcx.debug_bounds("right-panel").expect("面板列应渲染");
    let tb = wcx
        .debug_bounds("panel-toggle")
        .expect("开态面板头应有面板开关");
    assert!(
        tb.origin.x >= rp.origin.x,
        "开态开关应挪入面板头,tb.x={:?} rp.x={:?}",
        tb.origin.x,
        rp.origin.x
    );
    assert!(
        wcx.debug_bounds("panel-close").is_none(),
        "✕ 关闭钮应已撤除"
    );
    // 点头部开关 → 收起;开关回标题栏
    click_sel(&mut wcx, "panel-toggle");
    redraw(cx, &mut wcx);
    assert!(
        !cx.update(|app| store.read(app).panel_open),
        "点面板头开关应收起面板"
    );
    assert!(
        wcx.debug_bounds("panel-toggle").is_some(),
        "收起后开关应回标题栏"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// ask_user_question 端到端:宿主发射 question/requested → 帧泵 →
/// 问答卡弹在内容区(线上「不弹窗」的行为锁;卡选择器 = ask-question)
#[gpui_kit::test]
fn ask_user_question_card_pops_via_pump(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "ask-pop");
    let sid = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("自动新建会话应在场");
    // 先落一条用户消息(hero = 空会话时 bottom-stack 不渲染,问答卡无
    // 载体;线上场景 = 对话中追问,同非空)
    cx.update(|app| {
        store.update(app, |st, _| {
            let id = st.state.current_id.clone().unwrap();
            let chat = st.state.chats.entry(id).or_default();
            chat.nodes.push(ChatNode::User {
                key: "user:seed".into(),
                text: "先聊着".into(),
                images: vec![],
            });
        });
    });
    // 直接走宿主问答面:ask_questions 发射 question/requested(随后阻塞
    // 等应答,接收端丢弃即可)→ 帧泵 → reducer → 问答卡。fake 会话无
    // 工具面(NoTools),工具执行段由宿主测试另测
    cx.update(|app| {
        let questions: Vec<serde_json::Value> = serde_json::from_value(serde_json::json!([
            {
                "id": "switch_mode",
                "header": "切换语义",
                "question": "「TTS/ASR/LLM 都切换为阿里百炼」的含义是？",
                "options": [
                    { "label": "默认走百炼 (Recommended)", "description": "新增 DashScope 缺省" },
                    { "label": "彻底替换", "description": "移除本地后端" }
                ],
                "multi_select": false
            }
        ]))
        .unwrap();
        store.update(app, |st, cx| {
            st.ask_questions_json(&sid, questions, cx);
        });
    });
    let mut popped = false;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        if wcx.debug_bounds("ask-question").is_some() {
            popped = true;
            break;
        }
    }
    assert!(popped, "question/requested 后问答卡应弹在内容区");
    let _ = std::fs::remove_dir_all(root);
}

/// 问答卡选项含长 ASCII 词元(不可断行)时不得撑破卡片:内容列
/// min_w(0) 让文本换行,选项行右缘不超出卡右缘(此前 flex item
/// 缺省最小宽 = max-content,整卡溢出弹窗)
#[gpui_kit::test]
fn ask_option_long_ascii_description_stays_in_card(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "ask-overflow");
    let sid = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("自动新建会话应在场");
    cx.update(|app| {
        store.update(app, |st, _| {
            let id = st.state.current_id.clone().unwrap();
            let chat = st.state.chats.entry(id).or_default();
            chat.nodes.push(ChatNode::User {
                key: "user:seed".into(),
                text: "先聊着".into(),
                images: vec![],
            });
        });
    });
    cx.update(|app| {
        let questions: Vec<serde_json::Value> = serde_json::from_value(serde_json::json!([
            {
                "id": "overflow",
                "question": "长词元描述不应撑破卡片",
                "options": [
                    {
                        "label": "长词元选项 (Recommended)",
                        "description": "落 approval/asked-decided 审计事件 + host 审批闸门 + bash sandbox_permissions/justification 一次性升级 + 拒绝提示链,关闭 design 唯一「提示承诺未落地」项,并为 MCP 引入 sandbox_permissions/justification 审批闸门与 audit-trail 语义闭环"
                    },
                    { "label": "短选项", "description": "正常描述" }
                ],
                "multi_select": false
            }
        ]))
        .unwrap();
        store.update(app, |st, cx| {
            st.ask_questions_json(&sid, questions, cx);
        });
    });
    let mut popped = false;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        if wcx.debug_bounds("ask-question").is_some() {
            popped = true;
            break;
        }
    }
    assert!(popped, "问答卡应弹出");
    let card = wcx.debug_bounds("ask-question").expect("问答卡缺失");
    let opt = wcx
        .debug_bounds("ask-opt-长词元选项 (Recommended)")
        .expect("溢出选项行未渲染");
    let opt_right = f32::from(opt.origin.x) + f32::from(opt.size.width);
    let card_right = f32::from(card.origin.x) + f32::from(card.size.width);
    assert!(
        opt_right <= card_right + 1.0,
        "选项行右缘 {opt_right} 超出卡片右缘 {card_right}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 沙箱升级审批卡:渲染(一步两钮)+ 应答(批准 → pending 清空)
#[gpui_kit::test]
fn approval_card_renders_and_answers(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "approval");
    // 底部栈需要非空会话(hero 态不渲染问答/审批卡)
    cx.update(|app| {
        store.update(app, |st, _| {
            let id = st.state.current_id.clone().unwrap();
            let chat = st.state.chats.entry(id.clone()).or_default();
            chat.nodes.push(ChatNode::User {
                key: "user:seed".into(),
                text: "先聊着".into(),
                images: vec![],
            });
            st.state.pending_approval = Some(crate::shell::reducer::PendingApproval {
                rpc_id: "rpc-approval".into(),
                session_id: id,
                question: dsh_core::proto::Question {
                    id: "audit-1".into(),
                    question: "命令需要写工作区外的用户目录".into(),
                    header: Some("沙箱升级审批".into()),
                    detail: None,
                    options: None,
                    multi_select: Some(false),
                    intent: Some(serde_json::json!({ "kind": "sandbox-escalation" })),
                    data: Some(serde_json::json!({
                        "toolName": "bash",
                        "command": "touch ~/out",
                        "currentMode": "workspace-write",
                        "targetMode": "full-access",
                    })),
                },
            });
        });
    });
    cx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    assert!(wcx.debug_bounds("approval-card").is_some(), "审批卡未渲染");
    assert!(wcx.debug_bounds("approval-approve").is_some(), "批准钮缺失");
    assert!(wcx.debug_bounds("approval-reject").is_some(), "拒绝钮缺失");
    // 批准一次 → host.respond → pending 清空
    click_sel(&mut wcx, "approval-approve");
    cx.run_until_parked();
    assert!(
        cx.update(|app| store.read(app).state.pending_approval.is_none()),
        "批准后 pending 应清空"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 对话/轨迹切换(自标题栏移入):**内容区左上角**——tabs 贴内容卡
/// 左缘(px16 内边距)、顶部紧贴标题栏分割线之下;超长会话标题不再
/// 影响其位置
/// 标题栏侧栏缩进钮(logo 行已撤,折叠入口移入标题栏,位于工作区
/// 选择框之前):点击收起 = 侧栏完全隐藏(不渲染),再点展开恢复
#[gpui_kit::test]
fn topbar_fold_button_toggles_sidebar(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "fold");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };
    let fold = wcx.debug_bounds("fold-sidebar").expect("标题栏缩进钮缺失");
    let ws = wcx.debug_bounds("ws-trigger").expect("工作区触发钮缺失");
    assert!(
        f32::from(fold.origin.x) < f32::from(ws.origin.x),
        "缩进钮应位于工作区选择框之前"
    );
    assert!(
        wcx.debug_bounds("sidebar-card").is_some(),
        "初始应为展开侧栏"
    );
    click_sel(&mut wcx, "fold-sidebar");
    redraw(cx, &mut wcx);
    assert!(
        cx.update(|app| store.read(app).sidebar_collapsed),
        "点击后应收起"
    );
    assert!(
        wcx.debug_bounds("sidebar-card").is_none(),
        "收起后侧栏应完全隐藏(不渲染 rail)"
    );
    // 再点展开恢复
    click_sel(&mut wcx, "fold-sidebar");
    redraw(cx, &mut wcx);
    assert!(
        !cx.update(|app| store.read(app).sidebar_collapsed)
            && wcx.debug_bounds("sidebar-card").is_some(),
        "再点应展开恢复侧栏"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 轨迹缓存失效:会话空白期拉到的空台账(trajectory_session 已
/// 指向当前会话),内容增长后打开轨迹面板标签必须重拉——此前只按
/// 「会话失配」判失效,空白缓存永不刷新(新会话轨迹「没生成」)
#[gpui_kit::test]
fn trajectory_tab_entry_repulls_stale_blank_cache(cx: &mut TestAppContext) {
    use crate::features::trajectory::TrajectoryView;
    let (store, mut wcx, root) = menu_harness(cx, "traj-cache");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };

    // 一轮对话落档(fake)
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.send("生成一些轨迹", cx);
        });
    });
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        cx.run_until_parked();
        let done = cx.update(|app| {
            store
                .read(app)
                .current_nodes()
                .iter()
                .any(|n| matches!(n, ChatNode::TurnTail { .. }))
        });
        if done {
            break;
        }
    }

    // 空白期缓存:会话归属已记录、records 为空(会话在轨迹面板
    // 打开/新建时的形态);面板收着 = 不在轨迹视图
    cx.update(|app| {
        store.update(app, |st, _| {
            let id = st.state.current_id.clone().expect("当前会话");
            st.trajectory.trajectory = TrajectoryView::default();
            st.trajectory.trajectory_session = Some(id);
            st.panel_open = false;
            st.panel_active_tab = None;
        });
    });
    // 打开轨迹面板标签(handler 路径)→ 异步重拉完成
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.open_panel_tab(crate::shell::panel::PanelTab::Trajectory, cx)
        });
    });
    redraw(cx, &mut wcx);
    let n = cx.update(|app| store.read(app).trajectory.trajectory.records.len());
    assert!(n >= 2, "切入 tab 应重拉轨迹(空白缓存失效),实得 {n} 条");
    assert!(
        wcx.debug_bounds("trajectory-view").is_some(),
        "轨迹视图未渲染"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 轨迹(右栏面板标签):空会话空态;has_older 加载钮;Turns 全局折叠
#[gpui_kit::test]
fn trajectory_empty_load_earlier_and_turn_collapse(cx: &mut TestAppContext) {
    use crate::features::trajectory::TrajectoryView;
    use dsh_core::trajectory::TrajectoryRecord;

    let (store, mut wcx, root) = menu_harness(cx, "traj-empty");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };

    // 空会话直开轨迹面板(不经 handler,不触发拉取)→ 空态
    cx.update(|app| {
        store.update(app, |st, _| {
            st.panel_open = true;
            st.panel_tabs = vec![crate::shell::panel::PanelTab::Trajectory];
            st.panel_active_tab = Some(crate::shell::panel::PanelTab::Trajectory);
        });
    });
    redraw(cx, &mut wcx);
    assert!(wcx.debug_bounds("trajectory-empty").is_some(), "空态缺失");
    assert!(wcx.debug_bounds("trajectory-view").is_some());

    // 注入两轮数据 + has_older → 加载钮出现;全局折叠 turn1 → 摘要行
    let rec = |index: u64, kind: &str, turn: Option<u64>| TrajectoryRecord {
        index,
        seq: index,
        kind: kind.into(),
        turn,
        group: "Step 1".into(),
        turn_start: index == 2 || index == 5,
        text: format!("记录 {index}"),
        result: None,
        is_error: false,
        time_seconds: None,
        started_at: Some(1000),
        request_number: None,
        input: None,
        output: None,
        think: None,
        ttft_ms: None,
        payload: None,
        output_detail: None,
        thinking_detail: None,
        system_prompt: None,
        tools_catalog: None,
        schema_detail: None,
        source: None,
    };
    cx.update(|app| {
        store.update(app, |st, _| {
            let id = st.state.current_id.clone().expect("当前会话");
            st.trajectory.trajectory = TrajectoryView {
                records: vec![
                    rec(1, "system", None),
                    rec(2, "user", Some(1)),
                    rec(3, "message", Some(1)),
                    rec(4, "tool", Some(1)),
                    rec(5, "user", Some(2)),
                ],
                requests: vec![],
                has_older: true,
                total: 50,
                loading: false,
                loading_older: false,
            };
            st.trajectory.trajectory_session = Some(id);
            st.trajectory.all_turns_collapsed = true;
        });
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("load-earlier").is_some(),
        "has_older 应有加载钮"
    );
    assert!(
        wcx.debug_bounds("turn-summary-1").is_some(),
        "折叠摘要行缺失"
    );
    assert!(
        wcx.debug_bounds("trajectory-row-3").is_none(),
        "turn1 折叠后非首条不应渲染"
    );
    assert!(
        wcx.debug_bounds("trajectory-row-5").is_some(),
        "turn2 首条应保留"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 模式切换端到端(fake):+ 菜单「计划模式」→ 无 hint 即点即执行
/// (/plan 经 host 短路 set_mode)→ session/mode 落档 → seq 定向回声
/// → 帧回泵 → Plan chip 出现;点击 chip → /plan off → chip 消失
/// (chip 仅激活态渲染)
#[gpui_kit::test]
fn plan_toggle_end_to_end_fake(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "plan");
    // 注:run_until_parked 只跑后台执行器,不冲刷前台效果队列;
    // 状态经帧泵(前台任务)变化后须补一次 update 边界落帧
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.update(|_: &mut gpui_kit::App| {});
        cx.run_until_parked();
    };
    click_sel(&mut wcx, "composer-cmd");
    redraw(cx, &mut wcx);
    // Plan chip:未进入计划模式时
    // chip 不存在(此前自创的灰色常驻态已废除;此时从未渲染过,可以
    // 断言缺席——debug_bounds 只增不清,仅首次缺席可断)
    assert!(
        wcx.debug_bounds("chip-plan").is_none(),
        "未进入计划模式不应渲染 chip"
    );
    // 命令菜单点 plan:无 hint = 即点即执行,不走命令行组参
    click_sel(&mut wcx, "plan");
    cx.run_until_parked();
    redraw(cx, &mut wcx);

    let wait_plan = |cx: &mut TestAppContext, want: bool| -> bool {
        // 预算 30s:并行负载下帧泵与 tokio 竞争,5s→15s 后仍会偶发耗尽
        // (面板测试族新增 5 个常驻 HostBridge harness 后
        // 整段串行也偶发,加大只降概率,非纯慢)
        for _ in 0..300 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            // 帧(模式回声/turn 帧)经泵(前台任务)到达后须补 update
            // 边界落帧(见测试头注);带 turn 的路径帧更多,缺边界偶发
            // 轮询耗尽
            cx.update(|_: &mut gpui_kit::App| {});
            cx.run_until_parked();
            let got = cx
                .update(|app| store.read(app).current_chat().map(|c| c.plan_mode))
                .unwrap_or(false);
            if got == want {
                return true;
            }
        }
        false
    };
    assert!(wait_plan(cx, true), "/plan on 未生效(set_mode 链路未回流)");
    redraw(cx, &mut wcx);
    assert!(wcx.debug_bounds("chip-plan").is_some(), "Plan chip 未出现");
    // 计划模式 placeholder 文案切换
    assert_eq!(
        cx.update(|app| store.read(app).chat.composer_placeholder),
        "描述你的任务以生成计划",
        "计划模式 placeholder 未切换"
    );

    // hover 换 ⓧ 取消态,点击 chip 退出→整个消失
    let chip = wcx.debug_bounds("chip-plan").expect("chip bounds");
    wcx.simulate_mouse_move(
        gpui_kit::Point {
            x: chip.origin.x + chip.size.width / 2.,
            y: chip.origin.y + chip.size.height / 2.,
        },
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    cx.run_until_parked();
    assert!(
        cx.update(|app| store.read(app).chat.plan_chip_hovered),
        "hover 未置位(ⓧ 取消态不可达)"
    );

    // 点击 chip 退出:方向绝对值,即使真机 MouseUp Capture 连发也只是
    // 重复 standard,不得反向补发 plan
    click_sel(&mut wcx, "chip-plan");
    cx.run_until_parked();
    // chip 消失不做 debug_bounds 断言:该 map 只增不清(Frame::clear
    // 不含 debug_bounds),退出态由 store 投影断言兜底
    assert!(wait_plan(cx, false), "/plan off 未生效");
    // 标准态 placeholder 恢复
    assert_eq!(
        cx.update(|app| store.read(app).chat.composer_placeholder),
        "输入消息,Enter 发送 / Shift+Enter 换行",
        "标准态 placeholder 未恢复"
    );

    // 回归锁(真机取证):ⓧ 退出后父级 toggle 曾读到翻转的
    // 乐观态反向补发 plan,真机日志 standard/plan 严格交替 51 条、永远
    // 退不出。锁日志层:最后一个 standard 之后不得再出现 plan
    let proj_dir = std::fs::read_dir(root.join("sessions"))
        .expect("sessions 根可读")
        .flatten()
        .find(|e| e.path().is_dir())
        .map(|e| e.path())
        .expect("project 目录存在");
    let log_file = std::fs::read_dir(&proj_dir)
        .expect("project 目录可读")
        .flatten()
        .find(|e| e.path().join("session.jsonl").is_file())
        .map(|e| e.path().join("session.jsonl"))
        .expect("session.jsonl 存在");
    let modes: Vec<String> = std::fs::read_to_string(&log_file)
        .expect("日志可读")
        .lines()
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            if v["type"] == "session/mode" {
                Some(v["data"]["mode"].as_str().unwrap_or("").to_string())
            } else {
                None
            }
        })
        .collect();
    let last_std = modes.iter().rposition(|m| m == "standard");
    let plan_after_std = modes
        .iter()
        .rposition(|m| m == "plan")
        .is_some_and(|p| last_std.is_none_or(|s| p > s));
    assert!(
        !plan_after_std,
        "ⓧ 退出后日志又出现 plan 跟随(方向非绝对值):{modes:?}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 命令行渲染与移除:点带参命令(goal)→ 命令行(品牌色 /命令 + hint)
/// 在场;点 × 清除(输入参数保留为普通文本)。plan 已改无 hint 即点
/// 即执行,不再走命令行
#[gpui_kit::test]
fn command_line_renders_and_clears(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "cmdline");
    click_sel(&mut wcx, "composer-cmd");
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    click_sel(&mut wcx, "goal");
    cx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    assert!(
        wcx.debug_bounds("composer-command-line").is_some(),
        "命令行未渲染"
    );
    click_sel(&mut wcx, "composer-command-clear");
    cx.run_until_parked();
    let cleared = cx.update(|app| store.read(app).chat.pending_command.is_none());
    assert!(cleared, "× 应清除命令行");
    let _ = std::fs::remove_dir_all(root);
}

/// 切换会话丢弃命令行:命令行是输入意图不是会话状态——残留会在
/// 别的会话发送时被拼上 /命令(跨会话污染)。夹具用 goal(plan 已改
/// 无 hint 即点即执行,不设命令行)
#[gpui_kit::test]
fn command_line_cleared_on_session_switch(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "cmdline-switch");
    click_sel(&mut wcx, "composer-cmd");
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    click_sel(&mut wcx, "goal");
    cx.run_until_parked();
    let set = cx.update(|app| store.read(app).chat.pending_command.is_some());
    assert!(set, "命令行应已设置");
    let id = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("当前会话");
    cx.update(|app| store.update(app, |st, cx| st.open_session(&id, cx)));
    let cleared = cx.update(|app| store.read(app).chat.pending_command.is_none());
    assert!(cleared, "切换会话应丢弃命令行");
    let _ = std::fs::remove_dir_all(root);
}

/// 技能节渲染与草稿 chip:`/` 菜单打开时列出 session_skills 候选(项目根
/// 夹具;用户根已被 harness 隔离),点技能行落 pending chip(标题 =
/// 技能名,发送拼 /name args 走 host 手势注入)。user-invocable only、
/// 「仅用户」标由 session_skills 面保证,此处锁渲染与点击路径
#[gpui_kit::test]
fn skill_menu_section_sets_pending_chip(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "skill-menu");
    // 项目根技能夹具(ws 无 .git → 项目根 = ws 自身)
    let skill_dir = root
        .join("ws")
        .join(".agents")
        .join("skills")
        .join("repo-review");
    std::fs::create_dir_all(&skill_dir).expect("mkdir skill");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: repo-review\ndescription: Reviews the repo\n---\nbody",
    )
    .expect("write SKILL.md");
    // 打开 `/` 菜单(打开时刷新技能候选)
    click_sel(&mut wcx, "composer-cmd");
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    let skill_sel: &'static str = Box::leak("repo-review".to_string().into_boxed_str());
    assert!(
        wcx.debug_bounds(skill_sel).is_some(),
        "技能节行未渲染(session_skills 候选缺失)"
    );
    // 点技能行 → 草稿 chip(pending_command = 技能名)
    click_sel(&mut wcx, skill_sel);
    cx.run_until_parked();
    let pending = cx.update(|app| {
        store
            .read(app)
            .chat
            .pending_command
            .as_ref()
            .map(|p| p.name.clone())
    });
    assert_eq!(
        pending.as_deref(),
        Some("repo-review"),
        "技能行应设 pending chip"
    );
    wcx.refresh().expect("刷新失败");
    assert!(
        wcx.debug_bounds("composer-command-line").is_some(),
        "技能 chip 命令行未渲染"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 标题栏工作区下拉:列出全部工作区 → 点选切换 active_workspace
/// + 菜单关(select_workspace 无会话则在该区新建)
#[gpui_kit::test]
fn workspace_dropdown_lists_and_selects(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "wsdd");
    let host = cx.update(|app| store.read(app).bridge.host().clone());
    // 第二工作区(临时目录)
    let ws2_dir = root.join("extra-ws");
    std::fs::create_dir_all(&ws2_dir).expect("ws2 目录");
    let ws2 = cx.update(|_| {
        host.add_workspace(ws2_dir.to_str().expect("路径 utf-8"))
            .expect("添加工作区")
    });
    // host/workspace-changed 帧 → pump → describe 刷新(须 update
    // 边界冲刷效果,见 plan 测试注)
    cx.run_until_parked();
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();

    click_sel(&mut wcx, "ws-trigger");
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    let row_sel = Box::leak(format!("ws-row-{ws2}").into_boxed_str());
    assert!(
        wcx.debug_bounds(row_sel).is_some(),
        "工作区行未列出(host_info 未刷新?)"
    );

    click_sel(&mut wcx, row_sel);
    cx.run_until_parked();
    let (active, menu_open) = cx.update(|app| {
        let st = store.read(app);
        (
            st.state.active_workspace.clone(),
            st.sessions.workspace_menu_open,
        )
    });
    assert_eq!(active.as_deref(), Some(ws2.as_str()), "工作区未切换");
    assert!(!menu_open, "下拉应随选择关闭");
    // 新工作区无会话 → 应已在该区新建并打开(前缀形态)
    let current = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("当前会话");
    assert!(
        current.starts_with(&format!("{ws2}/")),
        "新工作区会话 id 形态异常:{current}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 侧栏树形:组头 chevron 折叠(store 态 + 开合异键渲染)、
/// 组头「+」在该工作区新建
#[gpui_kit::test]
fn sidebar_group_collapse_and_new(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "tree");

    // 初始展开 → 点 chevron 折叠
    assert!(
        wcx.debug_bounds("ws-chevron-open").is_some(),
        "初始应展开态 chevron"
    );
    click_sel(&mut wcx, "ws-chevron-open");
    cx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    // debug_bounds map 只增不清:折叠以「closed 键新出现」+ store 态双证
    assert!(
        wcx.debug_bounds("ws-chevron-closed").is_some(),
        "折叠态 chevron 未渲染"
    );
    let collapsed = cx.update(|app| store.read(app).sessions.collapsed_workspaces.clone());
    let default = cx.update(|app| store.read(app).default_workspace());
    assert!(collapsed.contains(&default), "折叠态未落 store");

    // 组头主体点击 = select_workspace(自动展开)
    click_sel(&mut wcx, "ws-chevron-closed");
    cx.run_until_parked();
    // 再点组头行本体(chevron 之外的文件夹/名区域):组头行无独立
    // selector,以 store 断言直接驱动等价路径
    cx.update(|app| {
        store.update(app, |st, cx| st.select_workspace(&default, cx));
    });
    cx.run_until_parked();
    let collapsed = cx.update(|app| store.read(app).sessions.collapsed_workspaces.clone());
    assert!(!collapsed.contains(&default), "select_workspace 应自动展开");

    // 组头「+」:该工作区新建会话(before 取此刻——select_workspace
    // 对全空白会话列表本身也会新建一只,属预期)
    let before = cx.update(|app| store.read(app).state.sessions.len());
    cx.update(|app| {
        store.update(app, |st, cx| st.create_session_in(&default, cx));
    });
    cx.run_until_parked();
    let after = cx.update(|app| store.read(app).state.sessions.len());
    assert_eq!(after, before + 1, "组头新建未增加会话");
    let current = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("当前会话");
    assert!(
        !current.contains('/'),
        "默认工作区新建应为无前缀形态:{current}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 侧栏组 = 工作区清单驱动:无会话的工作区仍渲染组头
/// (删除会话后组不可消失;组头保留「+」入口)
#[gpui_kit::test]
fn sidebar_keeps_empty_workspace_group(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "ws-empty-group");
    let host = cx.update(|app| store.read(app).bridge.host().clone());
    let ws2_dir = root.join("empty-ws");
    std::fs::create_dir_all(&ws2_dir).expect("ws2 目录");
    let ws2 = cx.update(|_| {
        host.add_workspace(ws2_dir.to_str().expect("路径 utf-8"))
            .expect("添加工作区")
    });
    // host/workspace-changed → describe 刷新 + 清单重拉
    cx.run_until_parked();
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();

    let head_sel = Box::leak(format!("ws-head-{ws2}").into_boxed_str());
    assert!(
        wcx.debug_bounds(head_sel).is_some(),
        "无会话工作区组头未渲染"
    );
    let default = cx.update(|app| store.read(app).default_workspace());
    let def_sel = Box::leak(format!("ws-head-{default}").into_boxed_str());
    assert!(wcx.debug_bounds(def_sel).is_some(), "默认区组头未渲染");
    let _ = std::fs::remove_dir_all(root);
}

/// 徽标迁移回归:状态栏右侧已清空(权限/模型徽标不再渲染);
/// git 分支徽标在标题栏工作区右侧,随 .git 出现(纯文件读)
#[gpui_kit::test]
fn statusbar_badges_render(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "badge");
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    // 状态栏右侧徽标组已移除(本窗口从未渲染,is_none 可靠)
    assert!(
        wcx.debug_bounds("statusbar-perm").is_none(),
        "权限徽标应已随状态栏右栏移除"
    );
    assert!(
        wcx.debug_bounds("statusbar-model").is_none(),
        "模型徽标应已随状态栏右栏移除"
    );
    // 工作区根造 .git/HEAD → 标题栏分支徽标出现
    let ws_root = root.join("ws");
    std::fs::create_dir_all(ws_root.join(".git")).expect("git 目录");
    std::fs::write(ws_root.join(".git/HEAD"), "ref: refs/heads/topic-x\n").expect("HEAD");
    assert!(
        wcx.debug_bounds("topbar-branch").is_none(),
        "前置:分支徽标应不存在"
    );
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.refresh_workspaces();
            cx.notify();
        });
    });
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    assert!(
        wcx.debug_bounds("topbar-branch").is_some(),
        "标题栏分支徽标未随 repo 出现"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// hero 模式(preset)选择:点 chip → 卡片 → 选「最小模式」→
/// 宿主 override + 缓存回写 + 菜单关
#[gpui_kit::test]
fn hero_preset_select(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "preset");
    let id = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("当前会话");
    let host = cx.update(|app| store.read(app).bridge.host().clone());

    click_sel(&mut wcx, "hero-preset");
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    assert!(
        wcx.debug_bounds("preset-item-minimal").is_some(),
        "preset 卡未弹出"
    );

    click_sel(&mut wcx, "preset-item-minimal");
    cx.run_until_parked();
    assert_eq!(
        host.session_preset(&id),
        "minimal",
        "preset override 未生效"
    );
    let (cached, menu) = cx.update(|app| {
        let st = store.read(app);
        (
            st.session_cfg_by_id.get(&id).map(|c| c.preset.clone()),
            st.hero_menu,
        )
    });
    assert_eq!(cached.as_deref(), Some("minimal"), "preset 缓存未回写");
    assert_eq!(menu, crate::shell::store::HeroMenu::None, "hero 菜单应关闭");
    let _ = std::fs::remove_dir_all(root);
}

/// 新会话空白态与注入行共存:attach 不再注入基线(差距1——基线改由
/// 首步 per-step 注入,时序在用户消息之后),空白会话语义 = 用户/模型
/// 还没说话,模式选择 chip 必须在;基线行(首步后才有)属注入行,
/// is_blank 忽略——hero 不得被顶掉
#[gpui_kit::test]
fn agents_baseline_keeps_hero_blank(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness_agents(cx, "agents-hero");
    cx.run_until_parked();
    let id = cx.update(|app| store.read(app).state.current_id.clone());
    let Some(id) = id else {
        panic!("当前会话未建立");
    };
    // attach 期:无注入行(基线延后到首步)、空白态、hero 保留
    let (ctx_node, blank, hero) = cx.update(|app| {
        let st = store.read(app);
        let ctx = st
            .state
            .chats
            .get(&id)
            .map(|c| {
                c.nodes
                    .iter()
                    .any(|n| matches!(n, ChatNode::Context { .. }))
            })
            .unwrap_or(false);
        (ctx, st.is_blank(&id), st.hero())
    });
    assert!(!ctx_node, "attach 不再注入基线(首步才注)");
    assert!(blank, "空白会话 is_blank 应为 true");
    assert!(hero, "hero 空态应保留");
    wcx.refresh().expect("刷新失败");
    assert!(
        wcx.debug_bounds("hero-preset").is_some(),
        "模式选择 chip 应可见可点"
    );
    // 首步后的基线注入行(手工落一条模拟):注入行不算内容,hero 不顶掉
    cx.update(|app| {
        store.update(app, |st, _cx| {
            let chat = st.state.chats.entry(id.clone()).or_default();
            chat.push_node(ChatNode::Context {
                key: "ctx:baseline".into(),
                content: "AGENTS.md 全文".into(),
                source: serde_json::json!({ "kind": "agent-instructions" }),
            });
        });
    });
    cx.run_until_parked();
    let (blank, hero) = cx.update(|app| {
        let st = store.read(app);
        (st.is_blank(&id), st.hero())
    });
    assert!(blank, "注入行不算内容,is_blank 应保持 true");
    assert!(hero, "hero 空态应保留(注入行不得顶掉模式选择)");
    wcx.refresh().expect("刷新失败");
    assert!(
        wcx.debug_bounds("hero-preset").is_some(),
        "模式选择 chip 应可见可点"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 上下文占用圆环:stats 带 contextUsed 后——composer 圆环 +
/// StatusBar 占用出现;点击圆环开详情卡
#[gpui_kit::test]
fn context_meter_renders_and_opens(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "ctx");
    // 注入占用(正常路径 = 首个 LLM 请求后 session_stats 产出;
    // 不触发真实 refresh_stats 以免后台回读覆盖注入值)
    cx.update(|app| {
        let id = store.read(app).state.current_id.clone().unwrap();
        store.update(app, |st, _| {
            st.stats_by_id.insert(
                id,
                serde_json::json!({
                    "turns": 2,
                    "contextUsed": 250_000,
                    "contextWindow": 1_000_000,
                    "contextBreakdown": {
                        "systemTokens": 50_000,
                        "toolsTokens": 30_000,
                        "messageTokens": 170_000,
                    }
                }),
            );
        });
    });
    cx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    assert!(
        wcx.debug_bounds("context-ring").is_some(),
        "composer 圆环未出现"
    );

    click_sel(&mut wcx, "context-ring");
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    assert!(
        wcx.debug_bounds("context-ring-open").is_some(),
        "详情卡未打开"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 统计实时推送端到端(事件驱动):发送消息 → 宿主落档点推
/// session/stats → 帧泵 → reducer → stats_by_id——不等 turn 结算
#[gpui_kit::test]
fn stats_push_lands_before_turn_settles(cx: &mut TestAppContext) {
    let (store, _wcx, root) = menu_harness(cx, "live-stats");
    let id = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("当前会话");
    cx.update(|app| {
        store.update(app, |st, cx| st.send("统计推送链路验证", cx));
    });
    // 轮询推送落表(turn/start 推送在回合开始即达,先于 turn/end)
    let mut landed = false;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        let turns = cx.update(|app| {
            store.read(app).stats_by_id.get(&id).and_then(|v| {
                if v["turns"].as_u64().unwrap_or(0) > 0 || v["steps"].as_u64().unwrap_or(0) > 0 {
                    Some(v["turns"].clone())
                } else {
                    None
                }
            })
        });
        if turns.is_some() {
            landed = true;
            break;
        }
    }
    assert!(landed, "session/stats 推送未落表(实时链路断裂)");
    let _ = std::fs::remove_dir_all(root);
}

/// 消息复制:点击用户消息复制钮 → 剪贴板 = 消息文本 + Check 反馈态
#[gpui_kit::test]
fn message_copy_to_clipboard(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "copy");
    let id = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("当前会话");
    // 注入一条用户消息(隔离复制行为本身,不依赖发送时序)
    cx.update(|app| {
        store.update(app, |st, _| {
            let chat = st.state.chats.entry(id.clone()).or_default();
            chat.nodes.push(ChatNode::User {
                key: "user:42".into(),
                text: "要被复制的消息文本".into(),
                images: Vec::new(),
            });
        });
    });
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    let sel = Box::leak("copy-user:42".to_string().into_boxed_str());
    assert!(wcx.debug_bounds(sel).is_some(), "复制钮未渲染");

    click_sel(&mut wcx, sel);
    cx.run_until_parked();
    let (copied, text) = cx.update(|app| {
        (
            store.read(app).chat.copied_key.clone(),
            app.read_from_clipboard()
                .and_then(|c| c.text().map(|t| t.to_string())),
        )
    });
    assert_eq!(copied.as_deref(), Some("user:42"), "反馈态未置位");
    assert_eq!(
        text.as_deref(),
        Some("要被复制的消息文本"),
        "剪贴板内容不符"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 真实 provider 全链路(发送 → LLM → 流式回流)。显式运行:
/// `cargo test -p dsh-desktop -- --ignored composer_real_roundtrip`
/// (消耗一次极短对话的 API 额度)
#[gpui_kit::test]
#[ignore = "真实 LLM 调用,显式 --ignored 运行"]
fn composer_real_roundtrip(cx: &mut TestAppContext) {
    send_roundtrip(cx, false, "只回复两个字:收到", None);
}

/// 真流式验证(假流式回归):以带节奏的合成 session/event 帧驱动
/// ——断言① 每 chunk 都触发视图重绘(store→view 观察链,缺失时
/// 窗口只在 OS 事件时重绘 = 假流式);② 文本逐帧增长;③ 内容超
/// 视口后跟随钉底(at_bottom 负值 offset 语义)。
#[gpui_kit::test]
fn streaming_frames_render_incrementally(cx: &mut TestAppContext) {
    use serde_json::json;
    cx.update(|app| {
        gpui_kit::component::init(app);
        crate::kits::theme::init(app);
    });
    allow_host_parking(cx);
    let root = std::env::temp_dir().join(format!("dsh-desktop-stream-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bridge, _frames_rx) =
        HostBridge::new_at(root.join("ws"), true, "", Some(root.join("sessions")))
            .expect("桥构建失败");
    let sid = bridge.host().create_session(None, None, None);

    let store_cell = std::rc::Rc::new(std::cell::RefCell::new(None::<Entity<AppStore>>));
    let store_capture = store_cell.clone();
    let (view, _wcx) = cx.add_window_view(|window, cx| {
        let store = cx.new(|cx| AppStore::new(bridge, cx));
        store.update(cx, |s, cx| {
            s.attach_window_state(window, cx);
            s.open_session(&sid, cx);
        });
        *store_capture.borrow_mut() = Some(store.clone());
        WorkspaceView::new(store, cx)
    });
    cx.run_until_parked();

    let store = store_cell.borrow().clone().expect("store 未捕获");
    let sid = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("会话应已打开");

    // 合成帧(turn/step 计数与 translate 产出的客方形状一致)
    let frame = |seq: u64, ty: &str, data: serde_json::Value| dsh_core::proto::ServerRequest {
        r#type: "server-request".into(),
        rpc_id: String::new(),
        method: "session/event".into(),
        payload: serde_json::json!({
            "sessionId": sid,
            "event": {
                "type": ty,
                "seq": seq,
                "time": 0,
                "data": data,
            },
        }),
    };
    let mut seq = 1u64;
    cx.update(|app| {
        store.update(app, |s, cx| {
            s.apply_frame(frame(seq, "turn/start", json!({ "turn": 1 })), cx)
        })
    });
    seq += 1;
    cx.update(|app| {
        store.update(app, |s, cx| {
            s.apply_frame(
                frame(seq, "step/start", json!({ "turn": 1, "step": 1 })),
                cx,
            )
        })
    });
    seq += 1;
    cx.run_until_parked();

    let render_at = |cx: &mut TestAppContext| cx.update(|app| view.read(app).render_count);
    let mut distinct_lens: Vec<usize> = Vec::new();
    let mut renders_per_chunk: Vec<usize> = Vec::new();
    let mut follow_all = true;
    for i in 0..12usize {
        let line = format!("第{i}行:{}\n", "很长的内容".repeat(12));
        cx.update(|app| {
            store.update(app, |s, cx| {
                s.apply_frame(
                    frame(
                        seq,
                        "assistant/chunk",
                        json!({
                            "turn": 1, "step": 1,
                            "chunk": { "type": "text-delta", "index": 0, "text": line },
                        }),
                    ),
                    cx,
                );
                seq += 1;
            })
        });
        // 节奏:真 chunk 间隔量级;测试上下文在效果周期重绘
        std::thread::sleep(std::time::Duration::from_millis(40));
        cx.run_until_parked();
        let before = renders_per_chunk.last().copied().unwrap_or(0);
        let now = render_at(cx);
        renders_per_chunk.push(now);
        assert!(now > before, "chunk #{i} 未触发重绘(观察链断裂 = 假流式)");
        let (len, bottom) = cx.update(|app| {
            let st = store.read(app);
            let chat = st.current_chat().expect("chat");
            let len = chat
                .nodes
                .iter()
                .find_map(|n| match n {
                    crate::features::chat::ChatNode::Assistant { text, .. } => Some(text.len()),
                    _ => None,
                })
                .unwrap_or(0);
            (len, st.at_bottom())
        });
        distinct_lens.push(len);
        follow_all &= bottom;
    }
    // 文本逐帧增长(真流式的状态面)
    let uniq: std::collections::BTreeSet<usize> = distinct_lens.iter().copied().collect();
    assert!(uniq.len() >= 10, "应有逐 chunk 的中间长度,得到 {uniq:?}");
    // 12 个长行足以超视口高 → 跟随必须全程钉底
    assert!(follow_all, "流式期间应保持钉底(at_bottom 语义/跟随断裂)");
    let _ = std::fs::remove_dir_all(root);
}

/// 带长历史的既有会话续发(复现「能发不能收」现场):
/// `DSH_DESKTOP_TEST_SEED=<session.jsonl 路径> cargo test -p dsh-desktop -- \
///   --ignored composer_seeded_history_roundtrip`
/// 未设 env 时跳过(种子是用户本地数据,不入仓)。
#[gpui_kit::test]
#[ignore = "真实 LLM 调用 + 本地种子日志,显式运行"]
fn composer_seeded_history_roundtrip(cx: &mut TestAppContext) {
    let Some(seed) = std::env::var_os("DSH_DESKTOP_TEST_SEED") else {
        eprintln!("[skip] 未设 DSH_DESKTOP_TEST_SEED");
        return;
    };
    send_roundtrip(
        cx,
        false,
        "只回复两个字:收到",
        Some(std::path::Path::new(&seed)),
    );
}

/// 设置页路由:侧栏设置行 → 独立设置页接管右列(provider 卡
/// 在场 = 页面实际绘制);「完成」关闭复原。注:debug_bounds 只增
/// 不清,「内容卡让位」无法用缺席断言,以 store 路由态为准
#[gpui_kit::test]
fn settings_page_route_end_to_end(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "setpage");
    click_sel(&mut wcx, "settings-row");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    assert!(
        cx.update(|app| store.read(app).settings.settings_open),
        "设置模式开"
    );
    assert!(
        wcx.debug_bounds("settings-page").is_some(),
        "内容区设置页在场"
    );
    // 侧栏已切换为设置菜单;内容区首运行 setup 姿态(fake 桥无凭据:
    // 默认 provider 直接渲染为打开的设置卡,源 needsSetup 语义)
    assert!(
        wcx.debug_bounds("settings-menu").is_some(),
        "侧栏设置菜单在场"
    );
    assert!(wcx.debug_bounds("provider-setup-deepseek").is_some());
    click_sel(&mut wcx, "settings-back");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    assert!(
        !cx.update(|app| store.read(app).settings.settings_open),
        "设置模式关"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// MCP 添加详情页:表单输入框必须有宽度(弹性行内 wrap 层须持 flex_1;
/// 输入框塌成小方块的回归锁)+ JSON 粘贴区高度足额
#[gpui_kit::test]
fn mcp_detail_inputs_have_width(cx: &mut TestAppContext) {
    let (_store, mut wcx, root) = menu_harness(cx, "mcpdet");
    click_sel(&mut wcx, "settings-row");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    click_sel(&mut wcx, "settings-nav-MCP");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    click_sel(&mut wcx, "mcp-add");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    for sel in ["mcp-id-input", "mcp-command-input", "mcp-cwd-input"] {
        let b = wcx
            .debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} 不在场"));
        assert!(
            b.size.width >= px(200.),
            "{sel} 输入框塌陷:宽 {:?}",
            b.size.width
        );
    }
    // JSON 页签:粘贴区高度足额(内容不被截断)
    click_sel(&mut wcx, "mcp-tab-json");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    let jb = wcx
        .debug_bounds("mcp-json-input")
        .expect("JSON 粘贴区不在场");
    assert!(
        jb.size.height >= px(240.),
        "JSON 粘贴区过矮:{:?}",
        jb.size.height
    );
    let _ = std::fs::remove_dir_all(root);
}

/// JSON 编辑器可交互:点击聚焦 + 键入落值(编辑器输入失效回归锁)
#[gpui_kit::test]
fn mcp_json_editor_accepts_typing(cx: &mut TestAppContext) {
    let (_store, mut wcx, root) = menu_harness(cx, "mcptype");
    click_sel(&mut wcx, "settings-row");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    click_sel(&mut wcx, "settings-nav-MCP");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    click_sel(&mut wcx, "mcp-add");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    click_sel(&mut wcx, "mcp-tab-json");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    // 点击编辑器聚焦,键入,值必须变化
    let b = wcx
        .debug_bounds("mcp-json-input")
        .expect("JSON 粘贴区不在场");
    wcx.simulate_click(
        gpui_kit::Point {
            x: b.origin.x + b.size.width / 2.,
            y: b.origin.y + b.size.height / 2.,
        },
        gpui_kit::Modifiers::default(),
    );
    wcx.run_until_parked();
    wcx.simulate_keystrokes("abc");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    let typed = cx.update(|app| {
        _store
            .read(app)
            .settings
            .mcp_detail
            .as_ref()
            .and_then(|d| d.json_input.as_ref())
            .map(|input| input.read(app).value().to_string())
            .unwrap_or_default()
    });
    assert!(
        typed.contains('a'),
        "键入未落值:编辑器不可交互,value={typed:?}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 编辑卡姿态流:首运行 setup 卡 / 取消后回退普通行 /
/// 行内编辑再展开 / 添加卡 / 删除确认模态
#[gpui_kit::test]
fn provider_editor_postures(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "pform");
    click_sel(&mut wcx, "settings-row");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    // 首运行:默认 provider 是打开的 setup 卡(内嵌编辑卡)
    assert!(wcx.debug_bounds("provider-setup-deepseek").is_some());
    assert!(wcx.debug_bounds("provider-editor-deepseek").is_some());
    // 取消 → setup dismiss,本会话回退普通行卡
    click_sel(&mut wcx, "provider-editor-cancel");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    assert!(wcx.debug_bounds("provider-row-deepseek").is_some());
    assert!(cx.update(|app| {
        let st = store.read(app);
        st.settings.dismissed_setup.contains("deepseek") && st.settings.editing_provider.is_none()
    }));
    // 行内「编辑」→ 编辑卡在行卡内展开
    click_sel(&mut wcx, "provider-edit-deepseek");
    wcx.run_until_parked();
    assert_eq!(
        cx.update(|app| store.read(app).settings.editing_provider.clone())
            .as_deref(),
        Some("deepseek")
    );
    // 「添加」→ 添加卡(id 输入 + 编辑器)
    click_sel(&mut wcx, "provider-add");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    assert!(cx.update(|app| store.read(app).settings.adding_provider));
    assert!(wcx.debug_bounds("provider-add-card").is_some());
    click_sel(&mut wcx, "provider-editor-cancel");
    wcx.run_until_parked();
    // 「移除」→ 确认模态;取消不删
    click_sel(&mut wcx, "provider-remove-deepseek");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    assert!(wcx.debug_bounds("provider-delete-card").is_some());
    assert_eq!(
        cx.update(|app| store.read(app).settings.delete_provider_target.clone())
            .as_deref(),
        Some("deepseek")
    );
    click_sel(&mut wcx, "provider-delete-cancel");
    wcx.run_until_parked();
    assert!(cx.update(|app| store.read(app).settings.delete_provider_target.is_none()));
    let _ = std::fs::remove_dir_all(root);
}

/// 全库检索命中面板 → 点击跳转(开会话/开轨迹面板标签/定位台账
/// 行)。命中数据手动注入(registry 检索链路在 dsh-core 已测);fake
/// turn 的 user/message seq 确定性 = 4(splice×2 + turn/start 之后)
#[gpui_kit::test]
fn global_search_hit_to_trajectory_row(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "gsearch");
    let sid = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("会话在场");
    // 跑一轮 fake turn(落 user/message;seq 随权限 pin+快照前移,
    // 动态定位而非硬编码)。turn 异步落盘——轮询磁盘直到 user/message
    // 出现(驱动 gpui 后台执行器推进 host 的 fake turn)。
    cx.update(|app| store.update(app, |st, cx| st.send("怎么修复队列持久化的 bug", cx)));
    let useq: u64 = {
        let host = cx.update(|app| store.read(app).bridge.host().clone());
        let mut useq = None;
        for _ in 0..200 {
            let log = host.export_session_log(&sid).unwrap_or_default();
            useq = log
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|v| v["type"] == "user/message")
                .and_then(|v| v["seq"].as_u64());
            if useq.is_some() {
                break;
            }
            wcx.run_until_parked();
            wcx.refresh().expect("刷新失败");
        }
        useq.expect("应含 user/message seq")
    };

    // 注入命中(fake turn 的 user/message 定位到真实 seq)
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.search.search_hits = Some(vec![serde_json::json!({
                "sessionId": sid, "seq": useq, "kind": "user",
                "content": "怎么修复队列持久化的 bug"
            })]);
            cx.notify();
        })
    });
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("search-hits-panel").is_some(),
        "命中面板在场"
    );
    assert!(wcx.debug_bounds("search-hit-0").is_some(), "命中行在场");

    // 点击 → 开会话 + 开轨迹面板标签 + 定位台账行
    click_sel(&mut wcx, "search-hit-0");
    // 轨迹数据异步加载后 locate_search_hit 才清 search_locate;并行
    // 测试下加载时序不定,轮询清空而非单次断言(同上方 seq 轮询模式)。
    let mut located = false;
    for _ in 0..200 {
        wcx.run_until_parked();
        wcx.refresh().expect("刷新失败");
        located = cx.update(|app| store.read(app).search.search_locate.is_none());
        if located {
            break;
        }
    }
    let (active, inspector) = cx.update(|app| {
        let st = store.read(app);
        (st.panel_active_tab, st.trajectory.inspector.is_some())
    });
    assert_eq!(
        active,
        Some(crate::shell::panel::PanelTab::Trajectory),
        "命中跳转开轨迹面板标签"
    );
    assert!(located, "定位完成(search_locate 清空)");
    assert!(inspector, "台账行选中(检查器开)");
    let _ = std::fs::remove_dir_all(root);
}

/// 通用区行几何回归:Select 自带 size_full,必须装定尺寸容器,
/// 否则撑爆行高并把文字列挤成竖排(实测回归锁)
#[gpui_kit::test]
fn general_rows_geometry(cx: &mut TestAppContext) {
    let (_store, mut wcx, root) = menu_harness(cx, "geodiag");
    click_sel(&mut wcx, "settings-row");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    click_sel(&mut wcx, "settings-nav-常规");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    for sel in [
        "pref-row-agent-preset",
        "pref-row-permission",
        "pref-row-language",
        "pref-row-busy-enter",
    ] {
        let b = wcx
            .debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} bounds 缺失"));
        assert!(
            b.size.width > px(600.),
            "{sel} 行宽 {} 应占满 720 列",
            b.size.width
        );
        assert!(
            b.size.height < px(120.),
            "{sel} 行高 {} 异常(Select 未装定尺寸容器?)",
            b.size.height
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

/// 通用区五行在场(Select 组件承载下拉,交互由组件库保证)
#[gpui_kit::test]
fn general_rows_present(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "genrows");
    click_sel(&mut wcx, "settings-row");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    click_sel(&mut wcx, "settings-nav-常规");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    for row in [
        "pref-row-agent-preset",
        "pref-row-permission",
        "pref-row-language",
        "appearance-group",
        "pref-row-busy-enter",
    ] {
        assert!(wcx.debug_bounds(row).is_some(), "{row} 在场");
    }
    // 四个 Select 状态已构建(挂窗态)
    assert!(cx.update(|app| {
        let st = store.read(app);
        st.settings.preset_select.is_some()
            && st.settings.permission_select.is_some()
            && st.settings.language_select.is_some()
            && st.settings.busy_enter_select.is_some()
    }));
    let _ = std::fs::remove_dir_all(root);
}

/// 工作区 ⋯ 菜单:单默认工作区 → 仅「重命名」(无上下移/移除);
/// 点重命名 → 工作区重命名模态目标接上
#[gpui_kit::test]
fn ws_menu_single_workspace_items(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "wsmenu");
    click_sel(&mut wcx, "ws-menu-btn");
    wcx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();
    assert!(wcx.debug_bounds("ws-menu-card").is_some(), "菜单卡在场");
    assert!(wcx.debug_bounds("重命名").is_some());
    assert!(wcx.debug_bounds("上移").is_none(), "首项无上移");
    assert!(wcx.debug_bounds("下移").is_none(), "单工作区无下移");
    assert!(wcx.debug_bounds("移除").is_none(), "默认工作区不可移除");
    click_sel(&mut wcx, "重命名");
    wcx.run_until_parked();
    let (target, default) = cx.update(|app| {
        let st = store.read(app);
        (st.sessions.rename_ws_target.clone(), st.default_workspace())
    });
    assert_eq!(
        target.as_deref(),
        Some(default.as_str()),
        "重命名目标 = 默认工作区"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 宽变后消息列滚动恢复(回归:窗口调整后滚动失效 + 卡顿)。
/// gpui list 宽变失效(`elements/list.rs` prepaint)只重测可视带,未测项
/// 0 高 → `scroll_max`(items.summary() 总高推导)缩到带区级:大上滚
/// 被 clamp,Bottom 对齐下等于 scroll_max 即甩回钉底。settle 全量
/// 重测(sync_chat_list_width / remeasure_chat_list)后总高精确,大上滚
/// 保留位移、可继续上滚、下滚甩底恢复钉底。
#[gpui_kit::test]
fn chat_scroll_survives_window_width_change(cx: &mut TestAppContext) {
    use gpui_kit::{ScrollDelta, ScrollWheelEvent, size};
    cx.update(|app| {
        gpui_kit::component::init(app);
        crate::kits::theme::init(app);
    });
    allow_host_parking(cx);
    let root = std::env::temp_dir().join(format!("dsh-desktop-scroll-{}", std::process::id()));
    let (bridge, _rx) = HostBridge::new_at(root.join("ws"), true, "", Some(root.join("sessions")))
        .expect("桥构建失败");

    let store_cell = std::rc::Rc::new(std::cell::RefCell::new(None::<gpui_kit::Entity<AppStore>>));
    let store_capture = store_cell.clone();
    let (_view, wcx) = cx.add_window_view(|_window, cx| {
        let store = cx.new(|cx| AppStore::new(bridge, cx));
        let id = store
            .read(cx)
            .state
            .current_id
            .clone()
            .expect("启动后有当前会话");
        let mut chat = ChatState::default();
        // 足够高的会话:总高 ≫ 宽变后带区(视口+600px overdraw)+ 1500px
        for i in 0..14usize {
            if i % 4 == 0 {
                chat.nodes.push(ChatNode::User {
                    key: format!("user:{i}"),
                    text: long_para(i),
                    images: Vec::new(),
                });
            }
            chat.nodes.push(ChatNode::Assistant {
                key: format!("a:1:{i}"),
                text: big_md(&format!("消息{}", i)),
                reasoning: long_para(i + 1),
                streaming: false,
                usage: None,
                message_id: format!("mid-{i}"),
            });
        }
        store.update(cx, |s, _| {
            s.state.chats.insert(id, chat);
        });
        *store_capture.borrow_mut() = Some(store.clone());
        WorkspaceView::new(store, cx)
    });
    // 克隆解除对 cx 的借用(与 menu_harness 同款;内部 Arc 共享)
    let mut wcx = wcx.clone();
    wcx.refresh().expect("窗口刷新失败");
    wcx.run_until_parked();

    let store = store_cell.borrow().clone().expect("store 未捕获");

    // 拉宽窗口(默认 1440 → 2400):列宽超过 748 保底档,宽变失效触发
    wcx.simulate_resize(size(gpui_kit::px(2400.), gpui_kit::px(1400.)));
    wcx.run_until_parked();
    wcx.refresh().expect("窗口刷新失败");
    wcx.run_until_parked();

    // settle(生产路径为 250ms 定时;测试直调同款重臂保证确定性)
    cx.update(|app| {
        store.update(app, |s, cx| s.remeasure_chat_list(cx));
    });
    wcx.refresh().expect("刷新失败");
    wcx.run_until_parked();

    // 全量测高后总高精确:可为的上滚距离(scroll_max)≥ 一个大幅位移
    let (max_off, item_count) = cx.update(|app| {
        let st = store.read(app);
        let list = &st.chat.chat_list;
        (
            f32::from(list.max_offset_for_scrollbar().y),
            list.item_count(),
        )
    });
    assert!(
        max_off >= 1500.,
        "会话内容不足:scroll_max {max_off} 不足 1500px"
    );

    // 起点 = 钉底跟随(None → logical 落末项)
    let ix0 = cx.update(|app| {
        let st = store.read(app);
        st.chat.chat_list.logical_scroll_top().item_ix
    });
    assert!(
        ix0 >= item_count,
        "初始应钉底(item_ix {ix0} >= {item_count})"
    );

    // 上滚一次:返回 (距底距离 px, 是否钉底)。距底 = max_offset_for_scrollbar
    // − 内容顶像素;钉底(None)态内容顶像素是 total 高、max 是 total−viewport,
    // 该差式不成立 → 以 at_bottom() 判定为主,距离仅用于 Some 态。
    // 注意列表元素自身 .py(8)(上 8 下 8):scroll() 的 scroll_max 计入
    // padding,而 max_offset_for_scrollbar 只计 item 总高 → 距底恒偏少 16px,
    // 断言期望 = dy − 16
    let scroll = |wcx: &mut gpui_kit::VisualTestContext, dy: f32| {
        let cc = wcx
            .debug_bounds("content-card")
            .expect("content-card bounds 缺失");
        let p = gpui_kit::point(
            cc.origin.x + cc.size.width / 2.,
            cc.origin.y + cc.size.height / 2.,
        );
        wcx.simulate_event(ScrollWheelEvent {
            position: p,
            delta: ScrollDelta::Pixels(gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(dy))),
            ..Default::default()
        });
        wcx.run_until_parked();
        cx.update(|app| {
            let st = store.read(app);
            let list = &st.chat.chat_list;
            let bottom = st.at_bottom();
            let mx = f32::from(list.max_offset_for_scrollbar().y);
            let cur = f32::from(-list.scroll_px_offset_for_scrollbar().y);
            (mx - cur, bottom)
        })
    };
    // 大上滚 1500px:修复后保留位移(期望 1500 − 16px padding 偏置);
    // 未修复则被 clamp 甩回钉底
    let expected1 = 1500. - (8. + 8.);
    let (d1, bottom1) = scroll(&mut wcx, 1500.);
    assert!(!bottom1, "大上滚后仍未离开钉底 —— 滚动被带区 clamp 甩回");
    assert!(
        (d1 - expected1).abs() < 10.,
        "上滚 1500px 距底 {d1:.0}px 不符(应 ≈{expected1:.0})"
    );
    let (d2, bottom2) = scroll(&mut wcx, 500.);
    assert!(!bottom2, "第二滚后不应回底");
    assert!(
        d2 > d1 + 400.,
        "第二滚未再上移:距底 {d1:.0} → {d2:.0}(被带区 clamp?)"
    );

    // 大幅下滚(负 delta)→ 恢复钉底
    let (_, bottom3) = scroll(&mut wcx, -100000.);
    assert!(bottom3, "大幅下滚后应恢复钉底");
    let _ = std::fs::remove_dir_all(root);
}

/// 流式入场动画年龄门控:born 在窗内的新节点渲染带入场 wrapper;
/// 超窗 / 无 born(历史载入)原样渲染 —— gpui list 虚拟化把滚出
/// overdraw 的项重挂,无门控则每次滚回都重放入场。工具行 Running
/// 带扫光,Done/Error 无。
#[gpui_kit::test]
fn streaming_entrance_gate_and_tool_sweep(cx: &mut TestAppContext) {
    use crate::features::chat::projection::{ChatNode, ChatState, ToolState};
    let (store, mut wcx, root) = menu_harness(cx, "enter-anim");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.run_until_parked();
    };

    // 直播新节点:born 刚记录 → 入场 wrapper 在场
    let id = cx.update(|app| {
        store.update(app, |st, _| {
            let id = st.state.current_id.clone().expect("当前会话");
            let mut chat = ChatState::default();
            chat.nodes.push(ChatNode::User {
                key: "user:1".into(),
                text: "hi".into(),
                images: vec![],
            });
            chat.node_born
                .insert("user:1".into(), std::time::Instant::now());
            st.state.chats.insert(id.clone(), chat);
            id
        })
    });
    redraw(cx, &mut wcx);
    let enter = "node-enter-user:1";
    assert!(
        wcx.debug_bounds(enter).is_some(),
        "窗内 born 的新节点应包入场 wrapper"
    );

    // 超窗:born 拨回过去 → 门控生效,wrapper 摘除(原样渲染无缝)
    let stale = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_millis(500))
        .expect("Instant 回拨 500ms");
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.state
                .chats
                .get_mut(&id)
                .unwrap()
                .node_born
                .insert("user:1".into(), stale);
            cx.notify();
        })
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds(enter).is_none(),
        "超窗 born 不应再包入场动画(虚拟化重挂不重放)"
    );

    // 无 born(历史载入路径)不包 + 工具行扫光随执行态出现/消失
    cx.update(|app| {
        store.update(app, |st, cx| {
            let chat = st.state.chats.get_mut(&id).unwrap();
            chat.node_born.clear();
            chat.nodes.push(ChatNode::Tool {
                key: "call:9".into(),
                name: "bash".into(),
                summary: "ls -la".into(),
                state: ToolState::Running,
                arguments: String::new(),
                output: None,
                view: None,
            });
            cx.notify();
        })
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("tool-sweep-1").is_some(),
        "Running 工具行应有扫光"
    );
    cx.update(|app| {
        store.update(app, |st, cx| {
            let chat = st.state.chats.get_mut(&id).unwrap();
            if let ChatNode::Tool { state, .. } = &mut chat.nodes[1] {
                *state = ToolState::Done;
            }
            cx.notify();
        })
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("tool-sweep-1").is_none(),
        "Done 后扫光应消失"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// TurnStatus 行:running 时出现「深入探索中…」
/// (带 1.8s shimmer 呼吸);turn 结束随 running 消失。
#[gpui_kit::test]
fn turn_status_gated_by_running(cx: &mut TestAppContext) {
    use crate::features::chat::projection::{ChatNode, ChatState};
    let (store, mut wcx, root) = menu_harness(cx, "turn-status");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.run_until_parked();
    };

    cx.update(|app| {
        store.update(app, |st, cx| {
            let id = st.state.current_id.clone().expect("当前会话");
            let mut chat = ChatState::default();
            chat.nodes.push(ChatNode::User {
                key: "user:1".into(),
                text: "hi".into(),
                images: vec![],
            });
            chat.node_born
                .insert("user:1".into(), std::time::Instant::now());
            st.state.chats.insert(id.clone(), chat);
            st.state.running_by_id.insert(id, true);
            cx.notify();
        })
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("turn-status").is_some(),
        "running 应显示转写状态行"
    );

    cx.update(|app| {
        store.update(app, |st, cx| {
            let id = st.state.current_id.clone().unwrap();
            st.state.running_by_id.insert(id, false);
            cx.notify();
        })
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("turn-status").is_none(),
        "turn 结束后状态行应消失"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// todo_write 折叠行:摘要取该次调用 args —— 并行进行中
/// 时 +N 不收缩后缀在场;args 坏 JSON 回落通用摘要(无后缀)。
#[gpui_kit::test]
fn todo_write_row_summary_from_call_args(cx: &mut TestAppContext) {
    use crate::features::chat::projection::{ChatNode, ChatState, ToolState};
    let (store, mut wcx, root) = menu_harness(cx, "todo-row");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.run_until_parked();
    };
    let inject = |cx: &mut TestAppContext, args: &str| {
        cx.update(|app| {
            store.update(app, |st, cx| {
                let id = st.state.current_id.clone().expect("当前会话");
                let mut chat = ChatState::default();
                chat.nodes.push(ChatNode::Tool {
                    key: "call:7".into(),
                    name: "todo_write".into(),
                    summary: String::new(),
                    state: ToolState::Done,
                    arguments: args.to_string(),
                    output: None,
                    view: None,
                });
                st.state.chats.insert(id, chat);
                cx.notify();
            })
        })
    };

    // 两条 in_progress → +1 后缀(解析成功且并行计数接线)
    inject(
        cx,
        r#"{"todos":[
            {"content":"任务一","status":"completed"},
            {"content":"任务二","status":"in_progress"},
            {"content":"任务三","status":"in_progress"}]}"#,
    );
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("todo-row-extra").is_some(),
        "并行进行中应有 +N 后缀"
    );

    // 坏 JSON → 回落通用摘要,无后缀
    inject(cx, "not json");
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("todo-row-extra").is_none(),
        "解析失败回落通用摘要(无 +N)"
    );

    // 展开体:好 args → 结构化任务卡(非 IN/OUT JSON 卡)
    inject(cx, r#"{"todos":[{"content":"任务一","status":"pending"}]}"#);
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.chat.expanded_tools.insert("call:7".into());
            cx.notify();
        })
    });
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("todo-write-card").is_some(),
        "展开应显示任务列表卡(结构化渲染)"
    );

    // 展开体:坏 args → 回落 IN/OUT 通用卡
    inject(cx, "not json");
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("todo-write-card").is_none(),
        "坏 args 展开回落 IN/OUT 通用卡"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 轮过程组折叠:已收口轮的过程项(think-only/工具)默认收成一行
/// 组摘要,穿插内容节点与收尾行在外;点击组头展开 → 成员原序平铺,
/// 再点收拢。行号走锚定 reset(记账不经 splice)。
#[gpui_kit::test]
fn turn_group_collapse_expand_roundtrip(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "turn-group");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.run_until_parked();
    };

    cx.update(|app| {
        store.update(app, |st, _| {
            let id = st.state.current_id.clone().expect("当前会话");
            let mut chat = crate::features::chat::ChatState::default();
            // user(0) / think-only(1) / tool(2) / turn-end(3):
            // 过程项 1..=2 折进组,user 与收尾行在外
            chat.nodes.push(ChatNode::User {
                key: "user:1".into(),
                text: "hi".into(),
                images: vec![],
            });
            chat.nodes.push(ChatNode::Assistant {
                key: "a:1:1".into(),
                text: String::new(),
                reasoning: "思考中".into(),
                streaming: false,
                usage: None,
                message_id: String::new(),
            });
            chat.nodes.push(ChatNode::Tool {
                key: "call:8".into(),
                name: "bash".into(),
                summary: "ls -la".into(),
                state: ToolState::Done,
                arguments: "{\"command\":\"ls -la\"}".into(),
                output: None,
                view: None,
            });
            chat.nodes.push(ChatNode::TurnTail {
                key: "turn-end:99".into(),
                aborted: false,
                meta: None,
                deliverables: vec![],
            });
            st.state.chats.insert(id, chat);
        });
    });
    redraw(cx, &mut wcx);

    // 折叠态:组行在场,user/收尾在外,过程项被折(bounds 缺席)
    assert!(
        wcx.debug_bounds("turn-group-turn-end:99").is_some(),
        "组摘要行应在场"
    );
    assert!(
        wcx.debug_bounds("node-0").is_some() && wcx.debug_bounds("node-3").is_some(),
        "用户消息与收尾行不应折叠"
    );
    assert!(
        wcx.debug_bounds("node-1").is_none() && wcx.debug_bounds("node-2").is_none(),
        "think-only 与工具行应收进组"
    );
    // 行槽数(4 节点 − 2 过程项 + 1 组行)与列表记账一致
    cx.update(|app| {
        store.update(app, |st, _| {
            assert_eq!(st.chat.row_slots.len(), 3);
            assert_eq!(st.chat.chat_list.item_count(), 3);
        });
    });

    // 展开:组头 + 成员原序平铺
    click_sel(&mut wcx, "turn-group-turn-end:99");
    redraw(cx, &mut wcx);
    cx.update(|app| {
        store.update(app, |st, _| {
            assert!(st.chat.open_turns.contains("turn-end:99"), "展开态登记");
        });
    });
    assert!(
        wcx.debug_bounds("node-1").is_some() && wcx.debug_bounds("node-2").is_some(),
        "展开后 think/工具行应平铺在场"
    );
    // 层级视觉:成员行带左引导线(位于行内缩进处的 2px 细线,与组外
    // 平铺节点可辨——组头/成员同款卡片导致展开迷失的跟进;node bounds
    // 是整行 wrapper,故以「线在行内的偏移」锁缩进)
    let rail = wcx
        .debug_bounds("group-rail")
        .expect("展开态应有成员引导线");
    let member = wcx.debug_bounds("node-2").expect("成员行应在场");
    let indent = rail.left() - member.left();
    assert!(
        (px(3.)..=px(10.)).contains(&indent),
        "引导线应在行内缩进处,实际偏移 {indent:?}"
    );
    let rail_w = rail.right() - rail.left();
    assert!(rail_w <= px(4.), "引导线应为细线,实际宽 {rail_w:?}");

    // 再点收拢:成员折回
    click_sel(&mut wcx, "turn-group-turn-end:99");
    redraw(cx, &mut wcx);
    assert!(wcx.debug_bounds("node-1").is_none(), "再点组头应收回成员");
    let _ = std::fs::remove_dir_all(root);
}

/// 折叠态列表无异常间隙:组行收走过程项后,剩余内容行
/// (最终答复等)与错误通告行之间不得出现巨大空白。构造 DeepSeek 真实
/// 形态:每步 reasoning+过渡文本(中间叙述入组,最终答复在外)。
#[gpui_kit::test]
fn collapsed_turn_has_no_gap_before_notice(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "turn-gap");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.run_until_parked();
    };

    cx.update(|app| {
        store.update(app, |st, _| {
            let id = st.state.current_id.clone().expect("当前会话");
            let mut chat = crate::features::chat::ChatState::default();
            chat.nodes.push(ChatNode::User {
                key: "user:1".into(),
                text: "跑一下测试".into(),
                images: vec![],
            });
            // 30 × (中间叙述 + 工具) + 最终答复 + 错误通告:
            // 列表高远超视口,折叠 reset 后必须钉底可见尾部
            for s in 1..=30 {
                chat.nodes.push(ChatNode::Assistant {
                    key: format!("a:1:{s}"),
                    text: format!("Step {s} narration text, a bit longer to take space."),
                    reasoning: format!("thinking {s}"),
                    streaming: false,
                    usage: None,
                    message_id: String::new(),
                });
                chat.nodes.push(ChatNode::Tool {
                    key: format!("call:{s}"),
                    name: "bash".into(),
                    summary: "ls".into(),
                    state: ToolState::Done,
                    arguments: "{}".into(),
                    output: None,
                    view: None,
                });
            }
            chat.nodes.push(ChatNode::Assistant {
                key: "a:1:61".into(),
                text: "最终答复:测试失败,结论如下。".into(),
                reasoning: String::new(),
                streaming: false,
                usage: None,
                message_id: String::new(),
            });
            chat.nodes.push(ChatNode::Notice {
                key: "turn-error:9".into(),
                text: "回合出错:transport: request: error sending request".into(),
            });
            st.state.chats.insert(id, chat);
        });
    });
    redraw(cx, &mut wcx);

    // 折叠态(error 收口段无最终答复):用户 + 组行(含全部过程/正文) + 通告
    cx.update(|app| {
        store.update(app, |st, _| {
            let shapes: Vec<String> = st
                .chat
                .row_slots
                .iter()
                .map(|s| match s {
                    crate::features::chat::RowSlot::Node(n)
                    | crate::features::chat::RowSlot::GroupMember(n) => format!("n{n}"),
                    crate::features::chat::RowSlot::Group { first, last, .. } => {
                        format!("g[{first}..={last}]")
                    }
                    crate::features::chat::RowSlot::GroupOpen { first, last, .. } => {
                        format!("G[{first}..={last}]")
                    }
                })
                .collect();
            assert_eq!(shapes, vec!["n0", "g[1..=61]", "n62"]);
        });
    });

    // 钉底可见性 + 相邻间隙(remeasure 前后都成立:测试时钟不走真实
    // 时间,advance_clock 推过 250ms settle 才触发 measure_all)
    let assert_tail = |wcx: &mut gpui_kit::VisualTestContext, phase: &str| {
        let group = wcx
            .debug_bounds("turn-group-turn-error:9")
            .unwrap_or_else(|| panic!("{phase}: 组行应在场(滚动位不得悬挂)"));
        let notice = wcx
            .debug_bounds("turn-notice")
            .unwrap_or_else(|| panic!("{phase}: 通告行应钉底在场"));
        let gap = notice.top() - group.bottom();
        assert!(gap < px(60.), "{phase}: 组行与通告之间间隙异常:{gap:?}");
    };
    assert_tail(&mut wcx, "reset 后");
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(400));
    cx.run_until_parked();
    redraw(cx, &mut wcx);
    assert_tail(&mut wcx, "remeasure 后");
    let _ = std::fs::remove_dir_all(root);
}

/// 消息导航轨 = 滚动条一体化:锚点**只有用户消息(轮次开始)**,按文档
/// 坐标比例落位(canvas 捕获/邻点插值);轨上一条通高轨道线 + 可拖视口
/// 拇指 + 当前位白点;hover 摘要卡;点圆点跳轮次顶对齐;拖拇指滚动、
/// 拖到底恢复钉底。锚点槽位口径:每轮 [user, think-only(折组), 答复,
/// tail] 折叠后 4 槽,用户锚槽号 = 4t。
#[gpui_kit::test]
fn nav_rail_show_hover_card_and_jump(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "nav-rail");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.run_until_parked();
    };

    cx.update(|app| {
        store.update(app, |st, _| {
            let id = st.state.current_id.clone().expect("当前会话");
            let mut chat = crate::features::chat::ChatState::default();
            for t in 0..10u32 {
                // 两行消息:hover 卡走「标题 + 正文」两级拆分
                chat.nodes.push(ChatNode::User {
                    key: format!("user:{t}"),
                    text: format!("第{t}问\n这是第{t}轮的补充说明正文"),
                    images: vec![],
                });
                chat.nodes.push(ChatNode::Assistant {
                    key: format!("a:{t}:1"),
                    text: String::new(),
                    reasoning: "思考".into(),
                    streaming: false,
                    usage: None,
                    message_id: String::new(),
                });
                // 长答复(40 段):10 轮累计总高必超一屏
                chat.nodes.push(ChatNode::Assistant {
                    key: format!("a:{t}:2"),
                    text: "答案".repeat(160),
                    reasoning: String::new(),
                    streaming: false,
                    usage: None,
                    message_id: String::new(),
                });
                chat.nodes.push(ChatNode::TurnTail {
                    key: format!("turn-end:{t}"),
                    aborted: false,
                    meta: None,
                    deliverables: vec![],
                });
            }
            st.state.chats.insert(id, chat);
        });
    });
    redraw(cx, &mut wcx);
    // 未布局/未测齐时 viewport 与总高均记 0,须推过 settle 全量重测;
    // 首个 settle 后的 paint 才捕获正确 nav_track(通知走 250ms 防抖
    // 任务,需再推一轮时钟让锚线位置渲染出来)
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(400));
    cx.run_until_parked();
    redraw(cx, &mut wcx);
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(300));
    cx.run_until_parked();
    redraw(cx, &mut wcx);

    // 轨在场;锚点只有用户消息(槽 4t):横线在场,组行不再是锚
    let diag = cx.update(|app| {
        let s = store.read(app);
        let l = &s.chat.chat_list;
        let anchors =
            crate::features::chat::projection::nav_anchors(&s.chat.row_slots, s.current_nodes());
        format!(
            "slots={} anchors={} vp={:?} content={:?}",
            s.chat.row_slots.len(),
            anchors.len(),
            f32::from(l.viewport_bounds().size.height),
            f32::from(l.max_offset_for_scrollbar().y),
        )
    });
    assert!(
        wcx.debug_bounds("nav-rail").is_some(),
        "超一屏应显示导航轨({diag})"
    );
    let rail_b = wcx.debug_bounds("nav-rail").expect("刻度列 bounds");
    // 锚点刻度列(窗口左缘):用户消息出线,组行不是锚
    assert!(
        wcx.debug_bounds("nav-line-0").is_some() && wcx.debug_bounds("nav-line-4").is_some(),
        "用户消息锚点应出线(槽 4t)"
    );
    assert!(wcx.debug_bounds("nav-line-1").is_none(), "组行不应再是锚点");
    let l0 = wcx.debug_bounds("nav-line-0").expect("首刻度 bounds");
    let l36 = wcx.debug_bounds("nav-line-36").expect("当前轮刻度 bounds");
    // 常态一律 6×2——当前轮只用白色区分不改长(实测)
    assert_eq!(l0.size.width, px(6.), "普通刻度应为 6px");
    assert_eq!(l36.size.width, px(6.), "当前轮刻度同宽,只白不改长");
    // 全部左对齐:线左缘 = 面板左缘 + 24(实测 23.5)
    assert!(
        (l0.left() - (rail_b.left() + px(24.))).abs() < px(3.),
        "刻度应左对齐于 24px 处,l0.left={:?}",
        l0.left()
    );
    // 固定间距 10px 密排:相邻刻度中心距 = 10(4 格 = 40)
    let c0 = l0.top() + l0.size.height / 2.;
    let c36 = l36.top() + l36.size.height / 2.;
    let c4 = wcx
        .debug_bounds("nav-line-4")
        .expect("轮1刻度 bounds")
        .top()
        + px(1.);
    assert!(
        (c4 - c0).abs() - px(40.) < px(3.),
        "刻度固定间距 10px,c4-c0={:?}",
        c4 - c0
    );
    // 整带纵向居中:首末刻度中点 = 轨中点(实测带中心=视口中心)
    let band_mid = (c0 + c36) / 2.;
    assert!(
        (band_mid - (rail_b.top() + rail_b.size.height / 2.)).abs() < px(4.),
        "刻度带应纵向居中,band_mid={band_mid:?} rail_mid={:?}",
        rail_b.top() + rail_b.size.height / 2.
    );
    assert!(
        wcx.debug_bounds("nav-card-0").is_none(),
        "未 hover 不应浮摘要卡"
    );

    // hover 首刻度 → 激活线加长变白 + 邻线渐变长 + 多行摘要卡浮现在
    // 刻度右侧
    wcx.simulate_mouse_move(
        gpui_kit::Point {
            x: l0.origin.x + px(2.),
            y: c0,
        },
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    cx.run_until_parked();
    redraw(cx, &mut wcx);
    // 渐变宽度:激活 26,距离 1 → 20,距离 9(≥4)→ 常态 6
    let l0h = wcx.debug_bounds("nav-line-0").expect("激活刻度 bounds");
    let l4h = wcx.debug_bounds("nav-line-4").expect("邻刻度 bounds");
    let l36h = wcx.debug_bounds("nav-line-36").expect("远刻度 bounds");
    assert_eq!(l0h.size.width, px(26.), "激活刻度应加长为 26px");
    assert_eq!(l4h.size.width, px(20.), "距离 1 的邻刻度应渐变为 20px");
    assert_eq!(l36h.size.width, px(6.), "距离 ≥4 刻度应保持常态 6px");
    let card = wcx
        .debug_bounds("nav-card-0")
        .expect("hover 刻度后应浮出摘要卡");
    assert_eq!(card.size.width, px(320.), "摘要卡应为 320px 宽");
    assert!(
        card.left() >= l0h.right() + px(8.),
        "摘要卡应在刻度右侧,card.left={:?} tick.right={:?}",
        card.left(),
        l0h.right()
    );
    // 卡垂直居中对准激活刻度
    let card_mid = card.top() + card.size.height / 2.;
    assert!(
        (card_mid - c0).abs() < px(35.),
        "摘要卡应垂直居中对准激活刻度,card_mid={card_mid:?} c0={c0:?}"
    );

    // 移出(向右挪进消息区中央,不在任何刻度上)→ 卡收起(hover 清空)
    let away_x = rail_b.origin.x + px(500.);
    let away_y = rail_b.origin.y + rail_b.size.height / 2.;
    wcx.simulate_mouse_move(
        gpui_kit::Point {
            x: away_x,
            y: away_y,
        },
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    cx.run_until_parked();
    redraw(cx, &mut wcx);
    assert!(
        wcx.debug_bounds("nav-card-0").is_none(),
        "移出刻度后摘要卡应收起"
    );

    // 点击轮 2 用户锚(槽 8 = 行 node-8)→ 目标行顶对齐在场 + 解锁钉底
    click_sel(&mut wcx, "nav-point-8");
    redraw(cx, &mut wcx);
    let top = cx.update(|app| store.read(app).chat.chat_list.logical_scroll_top());
    assert_eq!(top.item_ix, 8, "点击导航刻度应顶对齐目标槽");
    assert!(
        !cx.update(|app| store.read(app).at_bottom()),
        "跳转应解锁钉底"
    );
    assert!(wcx.debug_bounds("node-8").is_some(), "目标用户行应在场");

    // 组件库默认滚动条(shell 层挂 content-card 全高;无自绘 debug
    // selector,其拖拽/显隐由组件库自身测试保障,此处不断言)
    let _ = std::fs::remove_dir_all(root);
}

/// Bounds 右/下缘(Bounds 无 max_x/max_y 方法)
fn bounds_right(b: gpui_kit::Bounds<gpui_kit::Pixels>) -> gpui_kit::Pixels {
    b.origin.x + b.size.width
}
fn bounds_bottom(b: gpui_kit::Bounds<gpui_kit::Pixels>) -> gpui_kit::Pixels {
    b.origin.y + b.size.height
}

/// 展开体视口内回归:subagent 工具行(超长 prompt 参数)展开后,展开体
/// io-card 必须落在聊天列表视口内、不叠绘其他行
#[gpui_kit::test]
fn subagent_tool_expand_body_stays_in_viewport(cx: &mut TestAppContext) {
    use crate::features::chat::projection::{ChatNode, ChatState, ToolState};
    let (store, mut wcx, root) = menu_harness(cx, "io-overlap");
    let redraw = |cx: &mut TestAppContext, wcx: &mut gpui_kit::VisualTestContext| {
        wcx.refresh().expect("刷新失败");
        cx.run_until_parked();
    };

    // 真机形态的 subagent 调用参数(description + 数百字 prompt 的 JSON)
    let long_prompt = "请统计仓库 /Volumes/DATA/projects/voice-harness-agent/ 中所有的 TODO 注释，并返回一份汇报。\n\n任务要求：\n1. 仅统计源代码中的 TODO 注释，忽略 target/、node_modules/、vendor/、.git/ 等目录。\n2. 识别多种 TODO 写法：`TODO`、`TODO:`、`TODO(作者)`、`FIXME`。\n3. 汇报内容包括：\n   - TODO 注释总数（按文件 / 按语言分组统计）。\n   - 每个文件的具体位置（文件路径 + 行号）与 TODO 文本摘要。\n4. 使用 shell 工具（grep/ripgrep）在仓库根目录执行搜索。\n5. 结果用简体中文整理成清晰的 markdown 汇报返回给我。\n\n注意：这是一个独立任务，请自行完成全部搜索与统计。".repeat(2);
    let arguments = serde_json::json!({
        "description": "统计仓库 TODO 注释",
        "prompt": long_prompt,
    })
    .to_string();

    cx.update(|app| {
        store.update(app, |st, cx| {
            let id = st.state.current_id.clone().expect("当前会话");
            let mut chat = ChatState::default();
            chat.nodes.push(ChatNode::User {
                key: "user:1".into(),
                text: "后台派一个子代理，统计这个仓库里所有 TODO 注释并汇报".into(),
                images: vec![],
            });
            chat.nodes.push(ChatNode::Tool {
                key: "call:9".into(),
                name: "subagent".into(),
                summary: "统计仓库 TODO 注释".into(),
                state: ToolState::Done,
                arguments,
                output: Some("started subagent s-x".into()),
                view: None,
            });
            st.state.chats.insert(id, chat);
            cx.notify();
        })
    });
    redraw(cx, &mut wcx);

    // 折叠态:展开体不在场
    assert!(
        wcx.debug_bounds("io-card-1").is_none(),
        "折叠态不应渲染展开体"
    );

    // 展开 subagent 工具行
    click_sel(&mut wcx, "tool-row-call:9");
    redraw(cx, &mut wcx);
    let io = wcx
        .debug_bounds("io-card-1")
        .unwrap_or_else(|| panic!("展开后 io-card 应渲染"));
    let viewport = wcx
        .debug_bounds("content-card")
        .expect("聊天列表视口应在场");

    // 断言一:展开体完全落在列表视口内(不错乱到消息区之外)
    let inside = bounds_right(io) <= bounds_right(viewport) + gpui_kit::px(1.0)
        && bounds_bottom(io) <= bounds_bottom(viewport) + gpui_kit::px(1.0);
    assert!(
        inside,
        "展开体越出列表视口: io={io:?} viewport={viewport:?}"
    );

    // 断言二:不叠绘用户行(行槽高度失真时会叠到别的行上)
    let user = wcx.debug_bounds("user-bubble-0").expect("用户行应在场");
    let overlap = io.origin.x < bounds_right(user)
        && user.origin.x < bounds_right(io)
        && io.origin.y < bounds_bottom(user)
        && user.origin.y < bounds_bottom(io);
    assert!(!overlap, "展开体叠绘到用户气泡: io={io:?} user={user:?}");
    let _ = std::fs::remove_dir_all(root);
}

/// 任务条端到端:运行中子代理 → 聊天框上方常驻 chip;点击切到
/// 子会话视图(主线 chip 出现);点主线切回;全部结束后条退场
#[gpui_kit::test]
fn task_bar_switches_between_main_and_subagent(cx: &mut gpui_kit::TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "task-bar");
    let parent = cx
        .update(|app| store.read(app).state.current_id.clone())
        .expect("当前会话");
    let child = cx.update(|app| {
        store
            .read(app)
            .bridge
            .host()
            .create_subagent_session(&parent)
    });

    // 空会话处于 hero 态(聊天栈不渲染):注入一条用户消息进入对话视图
    cx.update(|app| {
        store.update(app, |st, cx| {
            use crate::features::chat::projection::{ChatNode, ChatState};
            let id = st.state.current_id.clone().expect("当前会话");
            let mut chat = ChatState::default();
            chat.nodes.push(ChatNode::User {
                key: "user:1".into(),
                text: "后台派一个子代理".into(),
                images: vec![],
            });
            st.state.chats.insert(id, chat);
            cx.notify();
        })
    });

    // 运行中子代理 → 任务条在场(主线视图无主线 chip)
    cx.update(|app| {
        store.update(app, |st, _| {
            st.state.jobs_by_id.insert(
                parent.clone(),
                vec![serde_json::json!({
                    "id": child, "kind": "subagent", "label": "统计仓库 TODO 注释",
                    "status": "running", "startedAt": 1_000,
                    "prompt": "请统计仓库 TODO",
                })],
            );
        });
    });
    cx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    assert!(wcx.debug_bounds("task-bar").is_some(), "任务条未出现");
    assert!(
        wcx.debug_bounds("task-chip").is_some(),
        "子代理 chip 未出现"
    );
    assert!(
        wcx.debug_bounds("task-chip-main").is_none(),
        "主线视图不应出现主线 chip"
    );
    // 运行中行尾打断钮在场;点击只打断、不冒泡触发行切换
    assert!(
        wcx.debug_bounds("task-stop").is_some(),
        "运行中行的打断钮未出现"
    );
    click_sel(&mut wcx, "task-stop");
    cx.run_until_parked();
    let current = cx.update(|app| store.read(app).state.current_id.clone());
    assert_eq!(
        current.as_deref(),
        Some(parent.as_str()),
        "点击打断钮不应切换会话"
    );

    // 子会话在真机上委派即有事件(非 blank);测试注入等价投影
    cx.update(|app| {
        store.update(app, |st, cx| {
            use crate::features::chat::projection::{ChatNode, ChatState};
            let mut chat = ChatState::default();
            chat.nodes.push(ChatNode::User {
                key: format!("user:{child}"),
                text: "请统计仓库 TODO".into(),
                images: vec![],
            });
            st.state.chats.insert(child.clone(), chat);
            cx.notify();
        })
    });

    // 点子代理 chip → 切到子会话视图;主线 chip 出现
    click_sel(&mut wcx, "task-chip");
    cx.run_until_parked();
    let current = cx.update(|app| store.read(app).state.current_id.clone());
    assert_eq!(current.as_deref(), Some(child.as_str()), "未切到子会话");
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    assert!(
        wcx.debug_bounds("task-chip-main").is_some(),
        "子会话视图应有主线 chip"
    );

    // 点主线 → 切回;子代理结束后条退场
    click_sel(&mut wcx, "task-chip-main");
    cx.run_until_parked();
    let current = cx.update(|app| store.read(app).state.current_id.clone());
    assert_eq!(current.as_deref(), Some(parent.as_str()), "未切回主线");
    cx.update(|app| {
        store.update(app, |st, _| {
            st.state.jobs_by_id.insert(
                parent.clone(),
                vec![serde_json::json!({
                    "id": child, "kind": "subagent", "label": "统计仓库 TODO 注释",
                    "status": "completed", "detail": "可继续",
                    "startedAt": 1_000, "finishedAt": 29_000,
                    "prompt": "请统计仓库",
                })],
            );
        });
    });
    cx.run_until_parked();
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    assert!(wcx.debug_bounds("task-bar").is_none(), "全部结束后条应退场");
    let _ = std::fs::remove_dir_all(root);
}

/// 权限下拉锚在触发行上方、盖过输入卡体
/// (**仅权限**用此锚,其余下拉维持卡顶上方原形态)。
/// 锁:卡底缘贴触发 chip 顶上方 ≤12px、左缘对齐触发 chip、卡体越过
/// 输入卡顶缘
#[gpui_kit::test]
fn composer_menu_floats_above_trigger(cx: &mut TestAppContext) {
    let (store, mut wcx, root) = menu_harness(cx, "perm-float");
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    cx.update(|app| {
        store.update(app, |st, cx| {
            st.set_composer_menu(crate::features::chat::ComposerMenu::Permission, cx)
        });
    });
    wcx.refresh().expect("刷新失败");
    cx.update(|_: &mut gpui_kit::App| {});
    cx.run_until_parked();
    let card = wcx
        .debug_bounds("composer-menu-card")
        .expect("权限菜单应渲染");
    let trigger = wcx.debug_bounds("chip-perm").expect("权限 chip 应渲染");
    assert!(
        card.bottom() <= trigger.origin.y + gpui_kit::px(2.)
            && card.bottom() >= trigger.origin.y - gpui_kit::px(12.),
        "菜单底缘应贴触发行上方(间隙 ≤12px),menu.bottom={:?} trigger.top={:?}",
        card.bottom(),
        trigger.origin.y
    );
    assert!(
        (f32::from(card.left()) - f32::from(trigger.origin.x)).abs() < 2.,
        "菜单左缘应对齐触发钮,menu.left={:?} trigger.left={:?}",
        card.left(),
        trigger.origin.x
    );
    let input = wcx.debug_bounds("composer-card").expect("输入卡应渲染");
    assert!(
        card.bottom() > input.origin.y,
        "菜单体应盖过输入卡顶缘(源式锚定),menu.bottom={:?} input.top={:?}",
        card.bottom(),
        input.origin.y
    );
    let _ = std::fs::remove_dir_all(root);
}
