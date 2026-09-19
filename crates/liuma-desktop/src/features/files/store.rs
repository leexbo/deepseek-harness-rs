//! 文件树功能切片状态与行为。
//!
//! 真相源 = [`FilesStore`](levels/expanded);kit [`TreeState`] 仅是
//! 展示投影——每次层装载完成整体重建 items(选中态随重建重置,树无
//! 选中语义,可接受)。lazy 语义:目录行未载/已载空目录各挂一条禁用
//! 占位子行,既驱动 `is_folder()`(kit 契约:children 非空才有折叠
//! 箭头),又充当「正在读取…」/「空目录」状态行。折叠保留已载层、
//! 失败层不自动重试(重开不重试,刷新即重试)——照源。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState};
use gpui_kit::{AppContext as _, Context, Entity, Subscription};

use super::face::{self, DirEntryRow, EntryKind, ListError, Listing, MAX_ENTRIES};
use crate::shell::store::AppStore;

/// 占位行 id 后缀(NUL 不可能出现在路径段,保证与真实行 id 不撞)
const PLACEHOLDER_MARK: char = '\u{0}';

/// 单层目录装载状态
pub enum LevelState {
    /// 装载中
    Loading,
    /// 已载
    Ready(Listing),
    /// 失败(文案已定:face::failure_line)
    Failed(String),
}

/// 树行元数据(重建树时同步重建;render_item 查表渲染)
#[derive(Clone)]
pub struct RowMeta {
    /// 显示名
    pub name: String,
    /// 条目类型
    pub kind: EntryKind,
    /// 特殊行(状态/占位行;恒禁用)
    pub special: RowSpecial,
}

/// 特殊行种类
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RowSpecial {
    /// 普通条目行
    Normal,
    /// 「正在读取…」
    Loading,
    /// 「空目录」
    EmptyDir,
    /// 「条目太多,只显示了一部分。」
    Truncated,
    /// 失败文案行
    Failed,
}

impl RowSpecial {
    /// 占位/状态行(非 Normal)恒禁用
    pub fn is_disabled(&self) -> bool {
        !matches!(self, RowSpecial::Normal)
    }
}

/// 文件树切片状态
#[derive(Default)]
pub struct FilesStore {
    /// 工作区根(None = 无工作区,单行空态)
    pub root: Option<PathBuf>,
    /// 已载/装载中/失败的层(键 = 目录绝对路径)
    pub levels: HashMap<PathBuf, LevelState>,
    /// 展开中的目录(折叠保留 levels,照源)
    pub expanded: HashSet<PathBuf>,
    /// 树控件实体(展示投影;mount 时创建)
    pub tree: Option<Entity<TreeState>>,
    /// 树事件订阅(保活持有:drop = 退订;无读取面故而 allow)
    #[allow(dead_code)]
    pub tree_sub: Option<Subscription>,
    /// 行元数据表(键 = 树行 id)
    pub row_meta: HashMap<String, RowMeta>,
    /// 刷新代号(手动刷新/换根换代;过期回包丢弃)
    pub generation: u64,
}

impl FilesStore {
    /// 挂载:创建树实体并订阅展开/折叠事件(须在 AppStore 上下文,
    /// 事件直投 [`AppStore::files_on_tree_event`])
    pub fn mount(cx: &mut Context<AppStore>) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
        let tree_sub = cx.subscribe(&tree, |st, _tree, event: &TreeEvent, cx| {
            st.files_on_tree_event(event, cx);
        });
        Self {
            tree: Some(tree),
            tree_sub: Some(tree_sub),
            ..Self::default()
        }
    }
}

impl AppStore {
    /// 文件树面板当前可见(激活标签 = 文件):切会话自动换根的门控判据
    pub fn files_visible(&self) -> bool {
        matches!(
            self.panel_active_tab,
            Some(crate::shell::panel::PanelTab::Files)
        )
    }

    /// 标签切入:根失配(首次打开/已切会话)才刷新;同根保持缓存
    /// (照源:tab 常驻,切入不重载)
    pub fn files_ensure(&mut self, cx: &mut Context<Self>) {
        let want = self.current_workspace_dir();
        if want != self.files.root {
            self.refresh_files(cx);
        }
    }

    /// 刷新(手动钮 / 换根):清层、保留 expanded(换根时清空)、
    /// root 与 expanded 各层重拉(照源 reload 语义)
    pub fn refresh_files(&mut self, cx: &mut Context<Self>) {
        let root = self.current_workspace_dir();
        let root_changed = root != self.files.root;
        self.files.generation += 1;
        self.files.levels.clear();
        if root_changed {
            self.files.expanded.clear();
        }
        self.files.root = root;
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(root) = self.files.root.as_ref() {
            dirs.push(root.clone());
            dirs.extend(self.files.expanded.iter().cloned());
        }
        for dir in dirs {
            self.files_load_dir(dir, cx);
        }
        self.files_rebuild_tree(cx);
        cx.notify();
    }

    /// 树事件:展开 = 记入 expanded,层缺失则 lazy 装载;折叠 = 移出
    /// expanded(层保留)。占位行 id 恒不触发(叶子无 toggle)
    pub fn files_on_tree_event(&mut self, event: &TreeEvent, cx: &mut Context<Self>) {
        let (id, expanded) = match event {
            TreeEvent::Expanded(id) => (id, true),
            TreeEvent::Collapsed(id) => (id, false),
        };
        if id.ends_with(PLACEHOLDER_MARK) {
            return;
        }
        let path = PathBuf::from(id.as_str());
        if expanded {
            self.files.expanded.insert(path.clone());
            if !self.files.levels.contains_key(&path) {
                self.files_load_dir(path, cx);
            }
        } else {
            self.files.expanded.remove(&path);
        }
    }

    /// 装载一层(tokio spawn_blocking;回包按刷新代号守卫丢弃过期)
    fn files_load_dir(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let Some(root) = self.files.root.clone() else {
            return;
        };
        if matches!(self.files.levels.get(&dir), Some(LevelState::Loading)) {
            return;
        }
        self.files.levels.insert(dir.clone(), LevelState::Loading);
        cx.notify();
        let generation = self.files.generation;
        let dir_task = dir.clone();
        let rx = self.bridge.call(async move {
            tokio::task::spawn_blocking(move || face::list_dir(&root, &dir_task, MAX_ENTRIES))
                .await
                .map_err(|e| ListError::Unavailable(format!("{e}")))
                .and_then(|result| result)
        });
        let store = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            let Ok(result) = rx.await else {
                return Ok::<(), anyhow::Error>(());
            };
            store.update(cx, |s, cx| {
                if s.files.generation != generation {
                    return; // 刷新已换代:丢弃过期回包
                }
                let state = match result {
                    Ok(listing) => LevelState::Ready(listing),
                    Err(err) => LevelState::Failed(face::failure_line(&err)),
                };
                s.files.levels.insert(dir, state);
                s.files_rebuild_tree(cx);
                cx.notify();
            });
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// 重建树投影(真相源 → TreeItem 集 + 行元数据表;层装载完成与
    /// 刷新时调用)
    fn files_rebuild_tree(&mut self, cx: &mut Context<Self>) {
        let Some(tree) = self.files.tree.clone() else {
            return;
        };
        let Some(root) = self.files.root.clone() else {
            self.files.row_meta.clear();
            tree.update(cx, |t, cx| t.set_items(Vec::new(), cx));
            return;
        };
        let mut meta = HashMap::new();
        let items = self.files_build_children(&root, &mut meta);
        self.files.row_meta = meta;
        tree.update(cx, |t, cx| t.set_items(items, cx));
    }

    /// 造一层子行。目录行的 children 恒非空(未载→「正在读取…」占位,
    /// 已载空→「空目录」占位)——kit 契约 children 空 = 非文件夹 =
    /// 无折叠箭头,占位行同时就是状态行
    fn files_build_children(
        &self,
        dir: &Path,
        meta: &mut HashMap<String, RowMeta>,
    ) -> Vec<TreeItem> {
        let Some(level) = self.files.levels.get(dir) else {
            return vec![self.files_placeholder_row(dir, "正在读取…", RowSpecial::Loading, meta)];
        };
        match level {
            LevelState::Loading => {
                vec![self.files_placeholder_row(dir, "正在读取…", RowSpecial::Loading, meta)]
            }
            LevelState::Failed(line) => {
                vec![self.files_placeholder_row(dir, line, RowSpecial::Failed, meta)]
            }
            LevelState::Ready(listing) => {
                if listing.entries.is_empty() {
                    return vec![self.files_placeholder_row(
                        dir,
                        "空目录",
                        RowSpecial::EmptyDir,
                        meta,
                    )];
                }
                let mut items: Vec<TreeItem> = listing
                    .entries
                    .iter()
                    .map(|row| self.files_build_entry(dir, row, meta))
                    .collect();
                if listing.truncated {
                    items.push(self.files_placeholder_row(
                        dir,
                        "条目太多，只显示了一部分。",
                        RowSpecial::Truncated,
                        meta,
                    ));
                }
                items
            }
        }
    }

    /// 造一条条目行(目录递归一层 children;File/Other 为叶子)
    fn files_build_entry(
        &self,
        dir: &Path,
        row: &DirEntryRow,
        meta: &mut HashMap<String, RowMeta>,
    ) -> TreeItem {
        let child = dir.join(&row.name);
        let id = path_id(&child);
        let mut item = TreeItem::new(id.clone(), row.name.clone());
        if row.kind == EntryKind::Directory {
            let expanded = self.files.expanded.contains(&child);
            let children = self.files_build_children(&child, meta);
            item = item.children(children).expanded(expanded);
        }
        meta.insert(
            id,
            RowMeta {
                name: row.name.clone(),
                kind: row.kind,
                special: RowSpecial::Normal,
            },
        );
        item
    }

    /// 造一条禁用占位/状态行
    fn files_placeholder_row(
        &self,
        dir: &Path,
        label: &str,
        special: RowSpecial,
        meta: &mut HashMap<String, RowMeta>,
    ) -> TreeItem {
        let id = format!("{}{PLACEHOLDER_MARK}", dir.display());
        meta.insert(
            id.clone(),
            RowMeta {
                name: label.to_string(),
                kind: EntryKind::Other,
                special,
            },
        );
        TreeItem::new(id, label).disabled(true)
    }
}

/// 绝对路径 → 树行 id(展示原样;unix 路径分隔符即字符串形态)
fn path_id(path: &Path) -> String {
    path.display().to_string()
}
