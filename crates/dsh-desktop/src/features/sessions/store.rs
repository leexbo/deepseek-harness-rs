//! 会话与工作区树的 store 域:侧栏行菜单/重命名/删除确认、会话行
//! CRUD(fork/archive/导出)、工作区 CRUD 与下拉/组头菜单、以及会话
//! 命名/工作区归属镜像查询(title_for/workspace_of/basename)。视图见
//! features::sessions::views;行/组菜单开态本域自持,shell 底座经
//! close_all_menus 统一关闭。

use std::collections::{HashMap, HashSet};

use gpui_kit::{AppContext, Context, Entity, Window};
use gpui_kit::component::input::{InputEvent, InputState};

use crate::features::chat::ChatNode;
use crate::shell::reducer;
use crate::shell::store::AppStore;
use dsh_core::proto::HistoryValue;

/// 会话与工作区树功能切片状态(侧栏行/组/工作区菜单开态与坐标锚、
/// 重命名与删除目标、工作区路径/标题/分支表、折叠组)。
#[derive(Default)]
pub(crate) struct SessionsStore {
    /// 行内菜单打开的会话(⋯)
    pub menu_open_session: Option<String>,
    /// 行菜单开时的点击坐标(菜单卡根级渲染的定位锚)
    pub row_menu_pos: Option<gpui_kit::Point<gpui_kit::Pixels>>,
    /// 重命名目标会话
    pub rename_target: Option<String>,
    /// 待确认删除的会话(确认模态)
    pub delete_target: Option<String>,
    /// 重命名输入态(挂窗后建)
    pub rename_input: Option<Entity<InputState>>,
    /// 标题栏工作区下拉开态
    pub workspace_menu_open: bool,
    /// 工作区名 → 路径(workspace_view 解析)
    pub ws_paths: HashMap<String, std::path::PathBuf>,
    /// 工作区名 → 显示标题(workspace_view 解析;标题仅显示层)
    pub ws_titles: HashMap<String, String>,
    /// 侧栏组头 ⋯ 菜单打开的工作区
    pub menu_open_ws: Option<String>,
    /// 工作区菜单开时的点击坐标(根级渲染定位锚)
    pub ws_menu_pos: Option<gpui_kit::Point<gpui_kit::Pixels>>,
    /// 重命名目标工作区(与会话重命名共用输入态)
    pub rename_ws_target: Option<String>,
    /// 工作区名 → git 分支(None = 非 repo)。模型可经 bash 切分支,
    /// running 期间随统计轮询刷新(非准静态数据)
    pub ws_branches: HashMap<String, Option<String>>,
    /// 侧栏折叠的工作区组(搜索词非空时渲染层忽略)
    pub collapsed_workspaces: HashSet<String>,
}

impl AppStore {
    /// 打开会话:切换 + 历史尾窗加载(后台折叠)+ 统计 + 配置缓存
    pub fn open_session(&mut self, id: &str, cx: &mut Context<Self>) {
        self.state.current_id = Some(id.to_string());
        // 命令行是输入意图,不属于会话状态:切换即弃(否则残留到别的
        // 会话,发送时被拼上 /plan 等——跨会话污染)
        self.chat.pending_command = None;
        self.sync_active_workspace_from_current();
        self.refresh_session_cfg(id);
        // 同步:切换会话即见终值(异步完成回调不唤醒空闲主循环)
        self.refresh_stats_sync(id);
        self.load_history(id.to_string(), cx);
        if self.trajectory_visible() {
            self.refresh_trajectory(cx);
        }
        self.sync_run_tick(cx);
        cx.notify();
    }

    /// 历史尾窗加载:未投影时后台折叠写入(标题投影一并落);
    /// 直播帧可能先行到达——以已投影状态为基线回放,节点 key 幂等。
    /// **必须经 [`HostBridge::call`] 上 tokio**:history 内部 attach 会
    /// `tokio::spawn` 每-会话 worker,GPUI 后台线程无 reactor 会 panic。
    fn load_history(&mut self, id: String, cx: &mut Context<Self>) {
        if self.state.chats.contains_key(&id) {
            return; // 已投影;更早历史分页未接入
        }
        let store = cx.entity().clone();
        let host = self.bridge.host().clone();
        let sid = id.clone();
        let rx = self
            .bridge
            .call(async move { host.history(&sid, None, 100).await });
        cx.spawn(async move |_this, cx| {
            let page = rx.await;
            store.update(cx, |s, cx| {
                let Ok(Ok(page)) = page else { return };
                let HistoryValue {
                    events,
                    projections,
                    ..
                } = page;
                let chat = s.state.chats.entry(id.clone()).or_default();
                chat.merge_history(events.into_iter().map(|e| e.event));
                // 历史消息含 image 块 → 收集 aid,异步拉取缓存
                // (与实时帧同路径;避免 chat 可变借用与 s 再借用冲突)
                let mut aids: Vec<String> = Vec::new();
                for node in &chat.nodes {
                    if let ChatNode::User { images, .. } = node {
                        for b in images {
                            if let Some(aid) = b["attachment"]["attachmentId"].as_str() {
                                aids.push(aid.to_string());
                            }
                        }
                    }
                }
                let sid = id.clone();
                for aid in aids {
                    s.ensure_image_loaded(&sid, &aid, cx);
                }
                if let Some(p) = projections
                    && let Some(t) = p.values.get("title")
                    && let Some(t) = t.as_str().filter(|t| !t.is_empty())
                {
                    s.state.titles.insert(id.clone(), t.to_string());
                }
                cx.notify();
            });
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 新建会话(目标工作区 = 选中工作区;非默认工作区才传名)
    pub fn create_session(&mut self, cx: &mut Context<Self>) {
        let ws = self.non_default_workspace();
        let id = self.bridge.host().create_session(None, None, ws);
        self.refresh_list();
        self.open_session(&id, cx);
    }

    /// 切换工作区:打开其最近会话(优先非空白),无则新建;
    /// 目标组自动展开,分支表顺带刷新(切到的可能是别的 repo)
    pub fn select_workspace(&mut self, ws: &str, cx: &mut Context<Self>) {
        self.state.active_workspace = Some(ws.to_string());
        self.sessions.collapsed_workspaces.remove(ws);
        self.refresh_branches();
        let default = self.default_workspace();
        let sessions: Vec<String> = self
            .state
            .sessions
            .iter()
            .filter(|s| reducer::workspace_of(&s.session_id, &default) == ws)
            .map(|s| s.session_id.clone())
            .collect();
        match sessions.iter().find(|id| !self.is_blank(id)) {
            Some(id) => self.open_session(id, cx),
            None => {
                let target = if ws == default {
                    None
                } else {
                    Some(ws.to_string())
                };
                let id = self.bridge.host().create_session(None, None, target);
                self.refresh_list();
                self.open_session(&id, cx);
            }
        }
    }

    /// 当前会话归属工作区的绝对路径(@file 补全根;源以 header.cwd 为根)。
    /// 会话 id 形如 `wsName/sessionId`(非默认工作区)或裸 id(默认)。
    pub fn current_workspace_dir(&self) -> Option<std::path::PathBuf> {
        let cid = self.state.current_id.clone()?;
        let default = self.default_workspace();
        let ws = crate::shell::reducer::workspace_of(&cid, &default);
        // ws_paths 以工作区名为键;兜底用自身(单工作区场景默认工作区)
        self.sessions
            .ws_paths
            .get(ws)
            .cloned()
            .or_else(|| self.sessions.ws_paths.values().next().cloned())
    }

    /// 工作区名顺序(设置页概览/组头菜单边界判定;现拉宿主)
    pub fn workspace_order(&self) -> Vec<String> {
        self.bridge.host().workspace_names()
    }

    /// 刷新工作区表:workspace_view 解析路径 + 读各工作区分支
    pub fn refresh_workspaces(&mut self) {
        let view = self.bridge.host().workspace_view();
        self.sessions.ws_paths.clear();
        self.sessions.ws_titles.clear();
        if let Some(items) = view["items"].as_array() {
            for it in items {
                if let (Some(id), Some(path)) = (
                    it["workspaceId"].as_str(),
                    it["path"].as_str().map(std::path::PathBuf::from),
                ) {
                    self.sessions.ws_paths.insert(id.to_string(), path);
                    if let Some(title) = it["title"].as_str() {
                        self.sessions
                            .ws_titles
                            .insert(id.to_string(), title.to_string());
                    }
                }
            }
        }
        self.refresh_branches();
    }

    /// 只读分支(ws_paths 已知)。HEAD 为本地微秒级单文件读,同步即可
    pub fn refresh_branches(&mut self) {
        let entries: Vec<(String, std::path::PathBuf)> =
            self.sessions.ws_paths.clone().into_iter().collect();
        for (name, path) in entries {
            let branch = crate::gitinfo::branch_of(&path);
            self.sessions.ws_branches.insert(name, branch);
        }
    }

    /// 活动工作区分支(徽标用;未知工作区/非 repo → None)
    pub fn active_branch(&self) -> Option<String> {
        let ws = self
            .state
            .active_workspace
            .clone()
            .unwrap_or_else(|| self.default_workspace());
        self.sessions.ws_branches.get(&ws).cloned().flatten()
    }

    /// 标题栏工作区下拉开关(开时刷新工作区表——分支可能已变)
    pub fn toggle_workspace_menu(&mut self, cx: &mut Context<Self>) {
        self.sessions.workspace_menu_open = !self.sessions.workspace_menu_open;
        if self.sessions.workspace_menu_open {
            self.refresh_workspaces();
        }
        cx.notify();
    }

    /// 侧栏工作区组折叠切换
    pub fn toggle_workspace_collapsed(&mut self, ws: &str, cx: &mut Context<Self>) {
        if !self.sessions.collapsed_workspaces.insert(ws.to_string()) {
            self.sessions.collapsed_workspaces.remove(ws);
        }
        cx.notify();
    }

    /// 在指定工作区新建会话。默认工作区规范化为 None:宿主对任何
    /// 已知工作区名都会加 `<ws>/` 前缀,而默认区会话历史无前缀,
    /// 传 Some(default) 会造出混合 id 形态(registry resolve_session)
    pub fn create_session_in(&mut self, ws: &str, cx: &mut Context<Self>) {
        let target = if ws == self.default_workspace() {
            None
        } else {
            Some(ws.to_string())
        };
        let id = self.bridge.host().create_session(None, None, target);
        self.refresh_list();
        self.open_session(&id, cx);
    }

    /// 系统目录选择器添加工作区(osascript 阻塞模态 → 宿主 runtime
    /// spawn_blocking;占普通 worker 会饿死共享 runtime 的会话 worker)。
    /// 成功 → 刷新 describe/工作区表并切换;取消(None)静默
    pub fn add_workspace_via_picker(&mut self, cx: &mut Context<Self>) {
        let host = self.bridge.host().clone();
        let rx = self.bridge.call(async move {
            tokio::task::spawn_blocking(move || host.pick_workspace_directory().ok())
                .await
                .ok()
                .flatten()
        });
        let store = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            if let Ok(Some(path)) = rx.await {
                store.update(cx, |s, cx| {
                    match s.bridge.host().add_workspace(&path) {
                        Ok(ws) => {
                            s.state.host_info = s.bridge.describe();
                            s.refresh_workspaces();
                            s.select_workspace(&ws, cx);
                        }
                        Err(e) => {
                            s.push_local_notice(&format!("添加工作区失败:{}", e.message), cx);
                        }
                    }
                    cx.notify();
                });
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
        cx.notify();
    }

    /// 组头 ⋯ 菜单开(带坐标;根级渲染定位)
    pub fn open_ws_menu_at(
        &mut self,
        name: &str,
        pos: gpui_kit::Point<gpui_kit::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.sessions.menu_open_session = None;
        self.sessions.menu_open_ws = Some(name.to_string());
        self.sessions.ws_menu_pos = Some(pos);
        cx.notify();
    }

    /// 打开工作区重命名(复用会话重命名输入态)
    pub fn open_rename_workspace(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sessions.rename_input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("工作区标题"));
            cx.subscribe(&input, |this, _i, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.confirm_rename(cx);
                }
            })
            .detach();
            self.sessions.rename_input = Some(input);
        }
        let current = self.title_for_workspace(name);
        if let Some(input) = &self.sessions.rename_input {
            input.update(cx, |s, cx| s.set_value(&current, window, cx));
        }
        self.sessions.rename_ws_target = Some(name.to_string());
        self.sessions.menu_open_ws = None;
        cx.notify();
    }

    /// 工作区上移/下移(基于当前顺序换算 insertBefore 锚点)
    pub fn move_workspace(&mut self, name: &str, up: bool, cx: &mut Context<Self>) {
        let names = self.bridge.host().workspace_names();
        let Some(ix) = names.iter().position(|n| n == name) else {
            return;
        };
        let before = if up {
            ix.checked_sub(1).and_then(|i| names.get(i))
        } else {
            names.get(ix + 2)
        };
        let result = self
            .bridge
            .host()
            .reorder_workspace(name, before.map(String::as_str));
        match result {
            Ok(()) => {
                self.sessions.menu_open_ws = None;
                self.refresh_workspaces();
                self.refresh_list();
            }
            Err(e) => self.push_local_notice(&format!("排序失败:{}", e.message), cx),
        }
        cx.notify();
    }

    /// 移除工作区(默认工作区拒绝;当前工作区被移除 → 切回首会话/新建)
    pub fn remove_workspace(&mut self, name: &str, cx: &mut Context<Self>) {
        match self.bridge.host().remove_workspace(name) {
            Ok(()) => {
                self.sessions.menu_open_ws = None;
                self.refresh_workspaces();
                self.refresh_list();
                if self.state.active_workspace.as_deref() == Some(name) {
                    self.state.active_workspace =
                        self.bridge.host().workspace_names().first().cloned();
                    match self.state.sessions.first().map(|s| s.session_id.clone()) {
                        Some(next) => self.open_session(&next, cx),
                        None => self.create_session(cx),
                    }
                } else {
                    cx.notify();
                }
            }
            Err(e) => self.push_local_notice(&format!("移除失败:{}", e.message), cx),
        }
    }

    /// 行内 ⋯ 菜单开关
    pub fn toggle_row_menu(&mut self, id: &str, cx: &mut Context<Self>) {
        self.sessions.menu_open_session = if self.sessions.menu_open_session.as_deref() == Some(id)
        {
            None
        } else {
            Some(id.to_string())
        };
        cx.notify();
    }

    /// 行菜单开(带点击坐标):菜单卡根级渲染按此定位——行内
    /// absolute 会被侧栏卡 overflow_hidden 裁剪 + 被内容卡(后绘
    /// 兄弟)遮挡,与工作区下拉同病同解
    pub fn open_row_menu_at(
        &mut self,
        id: &str,
        pos: gpui_kit::Point<gpui_kit::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.sessions.menu_open_session = Some(id.to_string());
        self.sessions.row_menu_pos = Some(pos);
        cx.notify();
    }

    /// 打开重命名(输入态惰建 + Enter 确认订阅;点击回调自带 window)
    pub fn open_rename(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.sessions.rename_input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("会话标题"));
            cx.subscribe(&input, |this, _i, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.confirm_rename(cx);
                }
            })
            .detach();
            self.sessions.rename_input = Some(input);
        }
        let current = self.title_for(id);
        if let Some(input) = &self.sessions.rename_input {
            input.update(cx, |s, cx| s.set_value(&current, window, cx));
        }
        self.sessions.rename_target = Some(id.to_string());
        self.sessions.menu_open_session = None;
        cx.notify();
    }

    /// 确认重命名(空标题视为取消;工作区与 会话共用输入态,按目标分派)
    pub fn confirm_rename(&mut self, cx: &mut Context<Self>) {
        let title = self
            .sessions
            .rename_input
            .as_ref()
            .map(|e| e.read(cx).value().trim().to_string())
            .unwrap_or_default();
        if let Some(ws) = self.sessions.rename_ws_target.take() {
            if !title.is_empty() {
                match self.bridge.host().rename_workspace(&ws, &title) {
                    Ok(()) => {
                        self.refresh_workspaces();
                        self.refresh_list();
                    }
                    Err(e) => self.push_local_notice(&format!("重命名失败:{}", e.message), cx),
                }
            }
            cx.notify();
            return;
        }
        let Some(id) = self.sessions.rename_target.clone() else {
            return;
        };
        if !title.is_empty() && self.bridge.host().rename(&id, &title).is_ok() {
            self.state.titles.insert(id.clone(), title);
            self.refresh_list();
        }
        self.sessions.rename_target = None;
        cx.notify();
    }

    /// 取消重命名
    pub fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.sessions.rename_target = None;
        cx.notify();
    }

    /// 分叉会话(复制日志为新会话并打开)
    pub fn fork(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Ok(new_id) = self.bridge.host().fork_session(id) {
            self.refresh_list();
            self.open_session(&new_id, cx);
        }
    }

    /// 归档会话(当前会话被归档 → 打开剩余首个,无则新建)
    pub fn archive(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.bridge.host().archive_session(id).is_ok() {
            self.refresh_list();
            if self.state.current_id.as_deref() == Some(id) {
                match self.state.sessions.first().map(|s| s.session_id.clone()) {
                    Some(next) => self.open_session(&next, cx),
                    None => self.create_session(cx),
                }
            }
        }
    }

    /// 打开删除确认(菜单「删除」入口;确认后才执行)
    pub fn ask_delete_session(&mut self, id: &str, cx: &mut Context<Self>) {
        self.sessions.delete_target = Some(id.to_string());
        self.sessions.menu_open_session = None;
        cx.notify();
    }

    /// 确认删除(执行并收模态)
    pub fn confirm_delete_session(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.sessions.delete_target.take() else {
            return;
        };
        self.delete_session(&id, cx);
        cx.notify();
    }

    /// 取消删除确认
    pub fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        self.sessions.delete_target = None;
        cx.notify();
    }

    /// 删除会话(永久移除日志;当前会话被删 → 打开剩余首个,无则新建)
    pub fn delete_session(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Err(e) = self.bridge.host().delete_session(id) {
            self.push_local_notice(&format!("删除失败:{}", e.message), cx);
            return;
        }
        self.refresh_list();
        if self.state.current_id.as_deref() == Some(id) {
            match self.state.sessions.first().map(|s| s.session_id.clone()) {
                Some(next) => self.open_session(&next, cx),
                None => self.create_session(cx),
            }
        }
        cx.notify();
    }

    /// 导出会话日志(写入 ~/Downloads/dsh-session-<id>.jsonl;
    /// 会话行菜单入口,按行 id 导出而非仅当前会话)
    pub fn export_session_log(&mut self, id: &str, cx: &mut Context<Self>) {
        // ZIP 导出(根 + fork 后代血缘),
        // 落 ~/Downloads;失败回落单文件文本
        let safe = id.replace('/', "-");
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        let zip_path = home
            .join("Downloads")
            .join(format!("dsh-session-{safe}.zip"));
        let mut exported = false;
        if let Ok(bytes) = self.bridge.host().export_session_zip(id, true)
            && std::fs::write(&zip_path, bytes).is_ok()
        {
            exported = true;
            self.push_local_notice(&format!("已导出(含分叉后代):{}", zip_path.display()), cx);
        }
        if !exported && let Ok(log) = self.bridge.host().export_session_log(id) {
            let path = home
                .join("Downloads")
                .join(format!("dsh-session-{safe}.jsonl"));
            if std::fs::write(&path, log).is_ok() {
                self.push_local_notice(&format!("已导出:{}", path.display()), cx);
            }
        }
        cx.notify();
    }

    /// 会话标题:重命名 > 投影 > 空白「新会话」 > id。
    /// 空串标题视同缺省(历史投影曾下发烧穿串,会永久遮蔽清单摘录)
    /// 工作区显示标题(覆盖 → basename;身份恒用 basename)
    pub fn title_for_workspace(&self, name: &str) -> String {
        self.sessions
            .ws_titles
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    pub fn title_for(&self, id: &str) -> String {
        if let Some(t) = self.state.titles.get(id).filter(|t| !t.is_empty()) {
            return t.clone();
        }
        if let Some(s) = self.state.sessions.iter().find(|s| s.session_id == id) {
            if let Some(t) = s
                .projections
                .as_ref()
                .and_then(|p| p.values.get("title"))
                .and_then(|v| v.as_str())
            {
                return t.to_string();
            }
            if s.blank {
                return "新会话".into();
            }
        }
        id.to_string()
    }

    /// 空白会话判定:清单 blank 且投影无**内容**节点(投影先行时以节点
    /// 为准)。注入行(ChatNode::Context,attach 基线/AGENTS.md/@session
    /// 快照)是系统注入,不是用户或模型说的话——不算内容,否则新会话
    /// 的 AGENTS.md 基线注入会顶掉 hero 空态(模式选择 chip)。
    pub fn is_blank(&self, id: &str) -> bool {
        let has_nodes = self
            .state
            .chats
            .get(id)
            .map(|c| {
                c.nodes
                    .iter()
                    .any(|n| !matches!(n, ChatNode::Context { .. }))
            })
            .unwrap_or(false);
        if has_nodes {
            return false;
        }
        self.state
            .sessions
            .iter()
            .find(|s| s.session_id == id)
            .map(|s| s.blank)
            .unwrap_or(true)
    }

    /// 默认工作区名(= basename(cwd),describe workspaces[0] 同源)
    pub fn default_workspace(&self) -> String {
        self.state
            .host_info
            .workspaces
            .first()
            .cloned()
            .unwrap_or_else(|| basename(&self.state.host_info.cwd))
    }

    /// 非默认工作区才返回 Some(新建会话目标)
    pub(crate) fn non_default_workspace(&self) -> Option<String> {
        let default = self.default_workspace();
        match &self.state.active_workspace {
            Some(ws) if *ws != default => Some(ws.clone()),
            _ => None,
        }
    }

    /// 会话活动连带其工作区选中(对齐 web)
    fn sync_active_workspace_from_current(&mut self) {
        if let Some(id) = &self.state.current_id {
            let default = self.default_workspace();
            self.state.active_workspace = Some(reducer::workspace_of(id, &default).to_string());
        }
    }
}

/// 路径末段(`/a/b` → `b`;空 → 空串)
fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_tail() {
        assert_eq!(basename("/Volumes/DATA/proj"), "proj");
        assert_eq!(basename("proj"), "proj");
        assert_eq!(basename("/"), "");
    }
}
