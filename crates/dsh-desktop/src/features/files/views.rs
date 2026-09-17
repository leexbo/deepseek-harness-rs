//! 文件树视图:头行(根路径 + 刷新钮)+ kit Tree 树体。
//!
//! 行渲染查 [`crate::features::files::store`] 的 `row_meta` 表(真相源
//! 重建时同步);选中/禁用态由组件树壳统一施加(`TreeState` 内部选中 +
//! 包装层 `.selected/.disabled`),此处只管行内容与文件行点击。

use gpui_kit::component::list::ListItem;
use gpui_kit::component::tree::{TreeEntry, tree as kit_tree};
use gpui_kit::component::{IconName, StyledExt};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Window, div, px,
};

use super::face::EntryKind;
use super::store::RowMeta;
use crate::kits::filetype::class_icon;
use crate::kits::filetype::file_class;
use crate::kits::icons::{DshIcon, fixed};
use crate::kits::theme;
use crate::shell::store::AppStore;

/// 文件树视图(面板正文)
pub fn render(
    store: &Entity<AppStore>,
    _window: &mut Window,
    cx: &mut App,
) -> gpui_kit::AnyElement {
    let (root, tree) = {
        let st = store.read(cx);
        (st.files.root.clone(), st.files.tree.clone())
    };
    let mut col = div().v_flex().size_full().min_h(px(0.));
    match root {
        None => {
            col = col.child(
                div()
                    .debug_selector(|| "files-no-workspace".to_string())
                    .p(px(14.))
                    .text_size(px(12.))
                    .text_color(theme::CAPTION())
                    .child("这个会话没有工作区目录。"),
            );
        }
        Some(root) => {
            col = col.child(files_header(store, &root)).child(
                div()
                    .debug_selector(|| "files-tree".to_string())
                    .flex_1()
                    .min_h(px(0.))
                    .when_some(tree, |this, tree| {
                        let s = store.clone();
                        this.child(kit_tree(
                            &tree,
                            move |_ix, entry, _selected, _window, cx| files_row(&s, entry, cx),
                        ))
                    }),
            );
        }
    }
    col.into_any_element()
}

/// 头行:根路径(目录前缀灰显 + 末段全色,truncate)+ 刷新钮
/// (照源 body header:唯一控件)
fn files_header(store: &Entity<AppStore>, root: &std::path::Path) -> impl IntoElement {
    let display = root.display().to_string();
    let (prefix, last) = match display.rsplit_once('/') {
        Some((head, tail)) if !tail.is_empty() => (format!("{head}/"), tail.to_string()),
        _ => (String::new(), display.clone()),
    };
    let s_refresh = store.clone();
    div()
        .id("files-header")
        .debug_selector(|| "files-header".to_string())
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(6.))
        .h(px(36.))
        .px(px(12.))
        .border_b_1()
        .border_color(theme::BORDER())
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_baseline()
                .text_size(px(12.))
                .child(div().truncate().text_color(theme::CAPTION()).child(prefix))
                .child(div().truncate().text_color(theme::LABEL_2()).child(last)),
        )
        .child(
            div()
                .id("files-refresh")
                .debug_selector(|| "files-refresh".to_string())
                .flex()
                .size(px(24.))
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(theme::CAPTION())
                .hover(|s| s.bg(theme::DOCK()).text_color(theme::LABEL()))
                .child(fixed(DshIcon::RefreshCw, 13.))
                .on_click(move |_, _, cx| {
                    s_refresh.update(cx, |st, cx| st.refresh_files(cx));
                }),
        )
}

/// 单行(查 row_meta;特殊行 = 禁用状态行,文件行点击开预览)
fn files_row(store: &Entity<AppStore>, entry: &TreeEntry, cx: &mut App) -> ListItem {
    let id = entry.item().id.clone();
    let depth = entry.depth();
    let meta = store.read(cx).files.row_meta.get(id.as_ref()).cloned();
    let Some(RowMeta {
        name,
        kind,
        special,
    }) = meta
    else {
        return ListItem::new(id).h(px(26.)).text_size(px(13.));
    };
    let mut item = ListItem::new(id.clone()).h(px(26.)).pr(px(6.));
    if special.is_disabled() {
        // 状态/占位行:缩进一级,灰显禁点(禁用态由树壳施加)
        return item
            .pl(px((6 + (depth + 1) * 14) as f32))
            .child(div().text_size(px(12.)).child(name));
    }
    item = item.pl(px((6 + depth * 14) as f32));
    let mut row = div().flex().items_center().gap(px(6.)).min_w(px(0.));
    // 折叠箭头槽(定宽车道,文件行空槽保持标签对齐)
    let chevron = if kind == EntryKind::Directory {
        Some(fixed(
            if entry.is_expanded() {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            },
            12.,
        ))
    } else {
        None
    };
    row = row.child(
        div()
            .w(px(12.))
            .flex()
            .justify_center()
            .when_some(chevron, |this, icon| this.child(icon)),
    );
    // 图标槽:目录 FolderClosed;文件按类型家族染色;Other 灰
    row = row.child(div().w(px(16.)).flex().justify_center().child(match kind {
        EntryKind::Directory => fixed(IconName::FolderClosed, 14.),
        EntryKind::Other => fixed(DshIcon::FileSymlink, 14.).text_color(theme::CAPTION()),
        EntryKind::File => {
            let class = file_class(&name);
            class_icon(class, 14.).text_color(theme::FILE_TYPE_TINT(class))
        }
    }));
    row = row.child(
        div()
            .text_size(px(13.))
            .min_w(px(0.))
            .truncate()
            .child(name.clone()),
    );
    item = item.child(row);
    // 文件行:点击开预览(目录行点击 = 树壳 toggle;Other 行禁用无点击)
    if kind == EntryKind::File {
        let s_open = store.clone();
        let click_path = id.to_string();
        let sel_path = id.to_string();
        item = item
            .on_click(move |_, _, cx| {
                s_open.update(cx, |st, cx| st.open_file_preview(&click_path, None, cx));
            })
            .debug_selector(move || format!("files-row-{}", file_selector_name(&sel_path)));
    } else {
        item = item.debug_selector(move || format!("files-row-{name}"));
    }
    item
}

/// 行测试选择器名(basename;测试 fixture 保证唯一名)
fn file_selector_name(id: &str) -> String {
    id.rsplit('/').next().unwrap_or(id).to_string()
}
