//! 轨迹功能切片的 store 域:台账缓存与版本、检查器(目标/tab/宽度/折叠)、
//! 时间线(选区/视口/拖拽)、turns/calls 折叠、工具卡 Inspect 待定位。
//! 视图见 features::trajectory::views;状态自持,shell 底座经 method
//! 调用跨功能互作(open_session/open_panel_tab 触 refresh_trajectory)。

use std::collections::HashSet;

use gpui_kit::{Context, Entity};
use gpui_kit::component::input::InputState;

use dsh_core::trajectory::{TrajectoryRecord, TrajectoryRequest};

use crate::shell::panel::PanelTab;
use crate::shell::store::AppStore;

/// 检查器目标(台账记录 / 请求)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectTarget {
    /// 台账记录(全局 index)
    Record(u64),
    /// 请求(#N)
    Request(u64),
}

/// 会话轨迹台账缓存(来自 `AppHost::trajectory_page`;切会话重拉)。
#[derive(Debug, Default, Clone)]
pub struct TrajectoryView {
    /// 台账记录(尾窗,时间序)
    pub records: Vec<TrajectoryRecord>,
    /// 全量请求清单(#1..#N)
    pub requests: Vec<TrajectoryRequest>,
    /// 是否有更早记录(beforeIndex 分页)
    pub has_older: bool,
    /// 总记录数(全会话)
    pub total: u64,
    /// 首拉进行中(防重复)
    pub loading: bool,
    /// 「加载更早」进行中(防重复)
    pub loading_older: bool,
}

/// 轨迹初始尾窗大小(RPC clamp 1..2000)
pub const TRAJECTORY_WINDOW: usize = 200;
/// 「加载更早」页大小
pub const TRAJECTORY_PAGE: usize = 500;

/// 轨迹功能切片状态(台账缓存与滚动态、检查器与时间线交互态、
/// turns/calls 折叠与 Inspect 待定位)。
pub(crate) struct TrajectoryStore {
    /// 工具卡 Inspect 待定位 seq(切轨迹后按 seq+kind 选台账行)
    pub inspect_locate: Option<(String, u64)>,
    // ── 轨迹视图(数据缓存 + 交互态;trajectory_session 标识缓存归属)──
    /// 台账缓存(当前会话)
    pub trajectory: TrajectoryView,
    /// 轨迹缓存归属会话(失配 = 需重拉)
    pub trajectory_session: Option<String>,
    /// 轨迹搜索输入态(挂窗后建;渲染期读值过滤)
    pub trajectory_search: Option<Entity<InputState>>,
    /// 台账滚动句柄(跟随尾部 / prepend 锚定)
    pub trajectory_scroll: gpui_kit::ScrollHandle,
    /// 台账是否跟随尾部(距底 ≤2px;上滚即失跟)
    pub trajectory_follow: bool,
    /// 台账数据版本(拉取/翻页 +1;渲染侧比对驱动滚动)
    pub trajectory_version: u64,
    /// 渲染侧已消费的台账版本
    pub trajectory_rendered_version: u64,
    /// prepend 锚定(旧 offset.y / 旧 max 高 / 剩余重试帧;新布局
    /// 就绪后按高度增量回偏,「加载更早」不跳视口)
    pub trajectory_anchor: Option<(f32, f32, usize)>,
    /// Duration 切换(时间线按耗时投影;源持久化项,桌面进程内)
    pub trajectory_duration: bool,
    /// 折叠的 turn(turn 号)
    pub collapsed_turns: HashSet<u64>,
    /// Turns 全局折叠(工具栏)
    pub all_turns_collapsed: bool,
    /// Calls 折叠的 assistant(message record index)
    pub collapsed_calls: HashSet<u64>,
    /// Calls 全局折叠(工具栏)
    pub all_calls_collapsed: bool,
    /// 检查器目标(None = 关闭)
    pub inspector: Option<InspectTarget>,
    /// 检查器激活 tab(实体切换时按最近访问恢复)
    pub inspector_tab: Option<&'static str>,
    /// 检查器最近访问 tab(跨实体记忆)
    pub inspector_last_tab: &'static str,
    /// 检查器宽度(左缘拖拽;320..720)
    pub inspector_width: f32,
    /// 拖宽锚点(按下时光标 x / 当时宽度)
    pub inspector_resize_anchor: Option<(f32, f32)>,
    /// Raw tab 的 Thinking 折叠
    pub inspector_raw_thinking: bool,
    /// JSON 树展开的节点路径(源 JsonTree 子级默认折叠;key=`{ix}/{path}`,
    /// path 沿用源 pathId 编码 `s{len}:{key}` / `n{index}`)
    pub json_expanded: HashSet<String>,
    /// Tools 页展开的工具名(目录卡片折叠态)
    pub expanded_inspector_tools: HashSet<String>,
    /// 时间线选区(域归一化 0..1;Some = 表格按时间窗过滤)
    pub timeline_selection: Option<(f64, f64)>,
    /// 时间线视口(域归一化 0..1;None = 全览)
    pub timeline_viewport: Option<(f64, f64)>,
    /// 时间线拖拽中(锚点域分数;Some 时渲染期注册窗口级 move/up)
    pub timeline_drag: Option<f64>,
    /// 拖拽草稿选区(未提交;渲染层叠加显示)
    pub timeline_draft: Option<(f64, f64)>,
}

impl Default for TrajectoryStore {
    fn default() -> Self {
        Self {
            inspect_locate: None,
            trajectory: TrajectoryView::default(),
            trajectory_session: None,
            trajectory_search: None,
            trajectory_scroll: gpui_kit::ScrollHandle::new(),
            trajectory_follow: true,
            trajectory_version: 0,
            trajectory_rendered_version: 0,
            trajectory_anchor: None,
            trajectory_duration: false,
            collapsed_turns: HashSet::new(),
            all_turns_collapsed: false,
            collapsed_calls: HashSet::new(),
            all_calls_collapsed: false,
            inspector: None,
            inspector_tab: None,
            inspector_last_tab: "summary",
            inspector_width: 400.,
            inspector_resize_anchor: None,
            inspector_raw_thinking: false,
            json_expanded: HashSet::new(),
            expanded_inspector_tools: HashSet::new(),
            timeline_selection: None,
            timeline_viewport: None,
            timeline_drag: None,
            timeline_draft: None,
        }
    }
}

impl AppStore {
    /// 工具卡 Inspect:按 `call:{seq}` 登记待定位的 tool 记录并开轨迹
    /// 面板标签。与检索跳转同用「先登记、轨迹数据就绪后再定位」的延迟
    /// 模式(见 `open_search_hit`/`locate_search_hit`),避免会话在轨迹
    /// 打开前缓存为空导致同步 `records.find` 落空、跳转无声失败。
    /// callId == tool/call 事件 seq,与 chat.rs 的 `call:{seq}` key 同源。
    pub fn inspect_call(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(seq) = key
            .strip_prefix("call:")
            .and_then(|s| s.parse::<u64>().ok())
        else {
            return;
        };
        if let Some(sid) = self.state.current_id.clone() {
            self.trajectory.inspect_locate = Some((sid, seq));
        }
        self.open_panel_tab(PanelTab::Trajectory, cx);
        // 记录已在窗口(用户先前停在轨迹标签):立即定位;否则留待
        // refresh_trajectory 就绪后的 locate_inspect 收尾。
        self.locate_inspect(cx);
    }

    /// 轨迹就绪后按 seq 定位 `tool` 记录(工具卡 Inspect 收尾;展开所属 turn,
    /// 选中记录开检查器)。找不到时静默保留待定位,等下次轨迹数据覆盖再试。
    pub fn locate_inspect(&mut self, cx: &mut Context<Self>) {
        let Some((sid, seq)) = self.trajectory.inspect_locate.clone() else {
            return;
        };
        if self.state.current_id.as_deref() != Some(sid.as_str()) {
            return;
        }
        let Some(rec) = self
            .trajectory
            .trajectory
            .records
            .iter()
            .find(|r| r.kind == "tool" && r.seq == seq)
        else {
            return;
        };
        let turn = rec.turn;
        if let Some(t) = turn {
            self.trajectory.collapsed_turns.remove(&t);
        }
        self.select_trajectory_record(rec.index, cx);
        self.trajectory.inspect_locate = None;
    }

    /// 轨迹缓存是否失配当前会话(切入 tab / 切会话时判断)
    fn trajectory_stale(&self) -> bool {
        self.trajectory.trajectory_session.as_deref() != self.state.current_id.as_deref()
    }

    /// 轨迹台账是否钉在底部(2px 阈值,源 BOTTOM_FOLLOW_THRESHOLD_PX)
    pub fn trajectory_at_bottom(&self) -> bool {
        let h = &self.trajectory.trajectory_scroll;
        -h.offset().y >= h.max_offset().y - gpui_kit::px(2.)
    }

    /// 拉取轨迹台账(尾窗 200;tokio worker 折叠,大日志不卡 UI 线程)。
    /// 完成回调里会话已切换则丢弃;同会话直播重拉保留选中/折叠态
    pub fn refresh_trajectory(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.state.current_id.clone() else {
            return;
        };
        if self.trajectory.trajectory.loading {
            return;
        }
        let session_changed = self.trajectory_stale();
        let follow = self.trajectory_at_bottom() || self.trajectory.trajectory.records.is_empty();
        self.trajectory.trajectory.loading = true;
        self.trajectory.trajectory_follow = follow;
        cx.notify();
        let host = self.bridge.host().clone();
        let sid_for_call = sid.clone();
        let rx = self
            .bridge
            .call(async move { host.trajectory_page(&sid_for_call, TRAJECTORY_WINDOW, None) });
        let store = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            let Ok(page) = rx.await else {
                return Ok::<(), anyhow::Error>(());
            };
            store.update(cx, |s, cx| {
                // 会话已切换:丢弃过期页
                if s.state.current_id.as_deref() != Some(sid.as_str()) {
                    s.trajectory.trajectory.loading = false;
                    return;
                }
                let page = match page {
                    Ok(p) => p,
                    Err(e) => {
                        s.trajectory.trajectory.loading = false;
                        eprintln!("[dsh-desktop] 轨迹拉取失败:{}", e.message);
                        cx.notify();
                        return;
                    }
                };
                // 选中记录可能已不在窗口(翻页/直播后)——失配则关检查器
                if let Some(InspectTarget::Record(ix)) = s.trajectory.inspector
                    && !page.records.iter().any(|r| r.index == ix)
                {
                    s.trajectory.inspector = None;
                    s.trajectory.inspector_tab = None;
                }
                if session_changed {
                    s.trajectory.collapsed_turns.clear();
                    s.trajectory.collapsed_calls.clear();
                    s.trajectory.all_turns_collapsed = false;
                    s.trajectory.all_calls_collapsed = false;
                    s.trajectory.inspector = None;
                    s.trajectory.inspector_tab = None;
                    s.trajectory.timeline_selection = None;
                    s.trajectory.timeline_viewport = None;
                    s.trajectory.timeline_drag = None;
                    s.trajectory.timeline_draft = None;
                    s.trajectory.trajectory_anchor = None;
                }
                s.trajectory.trajectory = TrajectoryView {
                    records: page.records,
                    requests: page.requests,
                    has_older: page.has_older,
                    total: page.total,
                    loading: false,
                    loading_older: false,
                };
                s.trajectory.trajectory_session = Some(sid.clone());
                s.trajectory.trajectory_version += 1;
                s.locate_search_hit(cx);
                s.locate_inspect(cx);
                cx.notify();
            });
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 加载更早记录(beforeIndex = 已载首条;prepend + 视口锚定)
    pub fn load_earlier_trajectory(&mut self, cx: &mut Context<Self>) {
        if self.trajectory.trajectory.loading
            || self.trajectory.trajectory.loading_older
            || !self.trajectory.trajectory.has_older
        {
            return;
        }
        let Some(sid) = self.state.current_id.clone() else {
            return;
        };
        let Some(first) = self.trajectory.trajectory.records.first() else {
            return;
        };
        let before = first.index;
        // 锚定:记录当前滚动几何,写入新页后按内容高增量回偏
        let offset_y = f32::from(self.trajectory.trajectory_scroll.offset().y);
        let max_h = f32::from(self.trajectory.trajectory_scroll.max_offset().y);
        self.trajectory.trajectory.loading_older = true;
        cx.notify();
        let host = self.bridge.host().clone();
        let sid_for_call = sid.clone();
        let rx = self.bridge.call(async move {
            host.trajectory_page(&sid_for_call, TRAJECTORY_PAGE, Some(before))
        });
        let store = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            let Ok(page) = rx.await else {
                return Ok::<(), anyhow::Error>(());
            };
            store.update(cx, |s, cx| {
                if s.state.current_id.as_deref() != Some(sid.as_str()) {
                    s.trajectory.trajectory.loading_older = false;
                    return;
                }
                let Ok(page) = page else {
                    s.trajectory.trajectory.loading_older = false;
                    cx.notify();
                    return;
                };
                let mut records = page.records;
                records.extend(std::mem::take(&mut s.trajectory.trajectory.records));
                s.trajectory.trajectory.records = records;
                s.trajectory.trajectory.requests = page.requests;
                s.trajectory.trajectory.has_older = page.has_older;
                s.trajectory.trajectory.total = page.total;
                s.trajectory.trajectory.loading_older = false;
                s.trajectory.trajectory_anchor = Some((offset_y, max_h, 5));
                s.trajectory.trajectory_version += 1;
                cx.notify();
            });
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 滚动事件副作用:重估跟随 + 顶部 48px 内自动「加载更早」
    pub fn on_trajectory_scroll(&mut self, cx: &mut Context<Self>) {
        self.trajectory.trajectory_follow = self.trajectory_at_bottom();
        let near_top = self.trajectory.trajectory_scroll.offset().y >= -gpui_kit::px(48.);
        if near_top && self.trajectory.trajectory.has_older {
            self.load_earlier_trajectory(cx);
        }
    }

    /// 渲染期冲洗:prepend 锚定 / 跟随滚底(同 chat flush 模式;
    /// 订阅回调无窗口/滚动时机,滚动动作只能在渲染期执行)
    pub fn flush_trajectory_scroll(&mut self) {
        if let Some((old_y, old_max, retries)) = self.trajectory.trajectory_anchor {
            let new_max = f32::from(self.trajectory.trajectory_scroll.max_offset().y);
            // 新布局未就绪(max 未增):留到下一帧,重试上限后放弃
            if new_max <= old_max + 0.5 {
                if retries == 0 {
                    self.trajectory.trajectory_anchor = None;
                } else {
                    self.trajectory.trajectory_anchor = Some((old_y, old_max, retries - 1));
                }
                return;
            }
            self.trajectory.trajectory_anchor = None;
            let mut p = self.trajectory.trajectory_scroll.offset();
            p.y = gpui_kit::px(old_y - (new_max - old_max));
            self.trajectory.trajectory_scroll.set_offset(p);
        } else if self.trajectory.trajectory_version != self.trajectory.trajectory_rendered_version
        {
            if self.trajectory.trajectory_follow {
                self.trajectory.trajectory_scroll.scroll_to_bottom();
            }
            self.trajectory.trajectory_rendered_version = self.trajectory.trajectory_version;
        }
    }

    /// 选中台账记录(再点同记录 = 取消;开检查器并恢复最近 tab)
    pub fn select_trajectory_record(&mut self, index: u64, cx: &mut Context<Self>) {
        self.trajectory.inspector =
            if self.trajectory.inspector == Some(InspectTarget::Record(index)) {
                None
            } else {
                Some(InspectTarget::Record(index))
            };
        self.trajectory.inspector_tab = None;
        self.trajectory.inspector_raw_thinking = false;
        cx.notify();
    }

    /// 选中请求(Request #N 圆点;开检查器)
    pub fn select_trajectory_request(&mut self, number: u64, cx: &mut Context<Self>) {
        self.trajectory.inspector = Some(InspectTarget::Request(number));
        self.trajectory.inspector_tab = None;
        cx.notify();
    }

    /// 关闭检查器(× / 点表空白)
    pub fn close_inspector(&mut self, cx: &mut Context<Self>) {
        self.trajectory.inspector = None;
        self.trajectory.inspector_tab = None;
        cx.notify();
    }

    /// 切检查器 tab(记忆最近访问,跨实体恢复)
    pub fn set_inspector_tab(&mut self, tab: &'static str, cx: &mut Context<Self>) {
        self.trajectory.inspector_tab = Some(tab);
        self.trajectory.inspector_last_tab = tab;
        cx.notify();
    }

    /// Duration 切换(时间线耗时投影;源行为:模式切换清时间线选区)
    pub fn toggle_trajectory_duration(&mut self, cx: &mut Context<Self>) {
        self.trajectory.trajectory_duration = !self.trajectory.trajectory_duration;
        self.trajectory.timeline_selection = None;
        cx.notify();
    }

    /// Turns 全局折叠(工具栏;影响 turn_start 非首条之外整轮)
    pub fn toggle_all_turns(&mut self, cx: &mut Context<Self>) {
        self.trajectory.all_turns_collapsed = !self.trajectory.all_turns_collapsed;
        self.trajectory.collapsed_turns.clear();
        cx.notify();
    }

    /// 单 turn 折叠切换(摘要行/双击 turn 首)
    pub fn toggle_turn(&mut self, turn: u64, cx: &mut Context<Self>) {
        if !self.trajectory.collapsed_turns.insert(turn) {
            self.trajectory.collapsed_turns.remove(&turn);
        }
        cx.notify();
    }

    /// Calls 全局折叠(工具栏;assistant 连续工具调用)
    pub fn toggle_all_calls(&mut self, cx: &mut Context<Self>) {
        self.trajectory.all_calls_collapsed = !self.trajectory.all_calls_collapsed;
        self.trajectory.collapsed_calls.clear();
        cx.notify();
    }

    /// 单 assistant 调用折叠切换
    pub fn toggle_call(&mut self, message_index: u64, cx: &mut Context<Self>) {
        if !self.trajectory.collapsed_calls.insert(message_index) {
            self.trajectory.collapsed_calls.remove(&message_index);
        }
        cx.notify();
    }

    /// 提交/清空时间线选区(None = 清;右键/Esc/双击)
    pub fn set_timeline_selection(&mut self, sel: Option<(f64, f64)>, cx: &mut Context<Self>) {
        self.trajectory.timeline_selection = sel.map(|(a, b)| (a.min(b), a.max(b)));
        cx.notify();
    }

    /// 时间线视口(缩放;None = 全览)
    pub fn set_timeline_viewport(&mut self, vp: Option<(f64, f64)>, cx: &mut Context<Self>) {
        self.trajectory.timeline_viewport = vp.map(|(a, b)| (a.min(b), a.max(b)));
        cx.notify();
    }

    /// 时间线拖拽开始(锚点域分数;draft 清空,move 时填充)
    pub fn begin_timeline_drag(&mut self, anchor_frac: f64, cx: &mut Context<Self>) {
        self.trajectory.timeline_drag = Some(anchor_frac);
        self.trajectory.timeline_draft = None;
        cx.notify();
    }

    /// 时间线拖拽移动(更新草稿选区)
    pub fn move_timeline_drag(&mut self, cur_frac: f64, cx: &mut Context<Self>) {
        if let Some(anchor) = self.trajectory.timeline_drag {
            self.trajectory.timeline_draft = Some((anchor.min(cur_frac), anchor.max(cur_frac)));
            cx.notify();
        }
    }

    /// 时间线拖拽收尾(清拖拽态;选区提交由调用方决定)
    pub fn clear_timeline_drag(&mut self, cx: &mut Context<Self>) {
        self.trajectory.timeline_drag = None;
        self.trajectory.timeline_draft = None;
        cx.notify();
    }

    /// 检查器拖宽开始(锚点 = 光标 x + 当时宽度)
    pub fn inspector_resize_begin(&mut self, cursor_x: f32, cx: &mut Context<Self>) {
        self.trajectory.inspector_resize_anchor = Some((cursor_x, self.trajectory.inspector_width));
        cx.notify();
    }

    /// 检查器拖宽移动(clamp 320..720)
    pub fn inspector_resize_move(&mut self, cursor_x: f32, cx: &mut Context<Self>) {
        if let Some((anchor_x, anchor_w)) = self.trajectory.inspector_resize_anchor {
            self.trajectory.inspector_width = (anchor_w - (cursor_x - anchor_x)).clamp(320., 720.);
            cx.notify();
        }
    }

    /// 检查器拖宽结束
    pub fn inspector_resize_end(&mut self, cx: &mut Context<Self>) {
        self.trajectory.inspector_resize_anchor = None;
        cx.notify();
    }

    /// Raw tab 的 Thinking 折叠切换
    pub fn toggle_inspector_thinking(&mut self, cx: &mut Context<Self>) {
        self.trajectory.inspector_raw_thinking = !self.trajectory.inspector_raw_thinking;
        cx.notify();
    }

    /// Tools 页工具目录卡展开/折叠
    pub fn toggle_inspector_tool(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self
            .trajectory
            .expanded_inspector_tools
            .insert(name.to_string())
        {
            self.trajectory.expanded_inspector_tools.remove(name);
        }
        cx.notify();
    }

    /// JSON 树节点展开/折叠(源 JsonTree expander;子级默认折叠,
    /// 集合存「已展开」路径)
    pub fn toggle_json_node(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.trajectory.json_expanded.insert(key.to_string()) {
            self.trajectory.json_expanded.remove(key);
        }
        cx.notify();
    }
}
