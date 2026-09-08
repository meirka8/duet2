// SPDX-License-Identifier: MIT
//! T-4.2.5's Tree view mode (FR-NAV-04): the panel shows the directory
//! tree instead of a listing. Rooted at `/`, expanded down to the tab's
//! current directory with the cursor on it, every other branch collapsed
//! until asked for.
//!
//! Children are listed lazily, one directory at a time, on the core
//! Tokio runtime (`std::fs::read_dir`, directories only, sorted case-
//! insensitively) and applied on the foreground; a node shows `…` while
//! its listing is in flight. Nothing is ever walked recursively: a
//! 50k-subdirectory tree costs exactly the directories the user opened.
//!
//! Keys (dispatched here by `FileTable`, which owns focus and the key
//! context): Up/Down move the cursor; Right expands the node, or moves
//! into its first child when already expanded; Left collapses it, or
//! moves to its parent when already collapsed; Home/End/PageUp/PageDown
//! as in a list; Enter hands the node's path back through `on_enter`,
//! which navigates the tab there and returns it to its list mode. A
//! click moves the cursor, a click on the arrow toggles the node, a
//! double-click enters.
//!
//! The tree is a plain `Vec` of nodes with parent/child indices plus a
//! flattened `visible` order rebuilt on every expand/collapse; that
//! rebuild is O(visible), and rendering is a `uniform_list` over it, so
//! the view scrolls like the table does whatever the tree's size.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use duet_widgets::layout::h_flex;
use duet_widgets::theme::TokenPalette;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, BorrowAppContext as _, ClickEvent, Context, ImageSource, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, UniformListScrollHandle, Window, div, img, px,
    uniform_list,
};

use crate::icons::{ICON_PX, IconCache, IconId};

/// Row height in the tree.
pub(crate) const TREE_ROW_HEIGHT: f32 = 24.0;
/// Indent per depth level.
const INDENT_PX: f32 = 16.0;

/// Called with the node's path when the user enters it.
pub(crate) type EnterHandler = Rc<dyn Fn(PathBuf, &mut Window, &mut App)>;

#[derive(Debug)]
struct TreeNode {
    path: PathBuf,
    name: SharedString,
    depth: usize,
    parent: Option<usize>,
    /// `None` until listed; `Some(vec![])` for a leaf.
    children: Option<Vec<usize>>,
    expanded: bool,
    loading: bool,
}

pub(crate) struct TreeView {
    nodes: Vec<TreeNode>,
    /// Node indices in display order (root first, expanded subtrees
    /// inline), rebuilt by [`Self::rebuild_visible`].
    visible: Vec<usize>,
    /// Index into `visible`.
    cursor: usize,
    /// A path still being expanded towards -- see [`Self::reveal`].
    pending_reveal: Option<PathBuf>,
    scroll: UniformListScrollHandle,
    tokio_handle: tokio::runtime::Handle,
    on_enter: EnterHandler,
    folder_icon: (IconId, Arc<Vec<String>>),
    /// Bumped by [`Self::reset`]; a listing that comes back for an older
    /// generation is dropped.
    generation: u64,
}

impl TreeView {
    /// A tree rooted at `root` (normally `/`), nothing loaded yet.
    pub(crate) fn new(
        root: PathBuf,
        tokio_handle: tokio::runtime::Handle,
        on_enter: EnterHandler,
    ) -> Self {
        let name: SharedString = match root.file_name() {
            Some(n) => n.to_string_lossy().into_owned().into(),
            None => root.to_string_lossy().into_owned().into(),
        };
        let mut tree = Self {
            nodes: Vec::new(),
            visible: Vec::new(),
            cursor: 0,
            pending_reveal: None,
            scroll: UniformListScrollHandle::new(),
            tokio_handle,
            on_enter,
            folder_icon: (
                Arc::from("folder|inode-directory"),
                Arc::new(vec!["folder".to_string(), "inode-directory".to_string()]),
            ),
            generation: 0,
        };
        tree.nodes.push(TreeNode {
            path: root,
            name,
            depth: 0,
            parent: None,
            children: None,
            expanded: false,
            loading: false,
        });
        tree.rebuild_visible();
        tree
    }

    /// Expands the tree down to `path` (loading whatever is missing on
    /// the way) and parks the cursor on it; ancestors of `path` are
    /// expanded, nothing else changes. If `path` is outside the root the
    /// cursor stays where it is.
    pub(crate) fn reveal(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !path.starts_with(&self.nodes[0].path) {
            return;
        }
        self.pending_reveal = Some(path.to_path_buf());
        self.advance_reveal(cx);
    }

    /// One step of [`Self::reveal`]: from the deepest loaded ancestor of
    /// the pending path, either park the cursor (arrived), expand and
    /// descend (children known), or start the listing that will call
    /// this again (children unknown).
    fn advance_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.pending_reveal.clone() else {
            return;
        };
        let mut node = 0;
        loop {
            if self.nodes[node].path == target {
                self.pending_reveal = None;
                self.rebuild_visible();
                if let Some(pos) = self.visible.iter().position(|&n| n == node) {
                    self.cursor = pos;
                    self.scroll.scroll_to_item(pos, ScrollStrategy::Center);
                }
                cx.notify();
                return;
            }
            match &self.nodes[node].children {
                None => {
                    self.nodes[node].expanded = true;
                    self.load_children(node, cx);
                    return;
                }
                Some(children) => {
                    let children = children.clone();
                    self.nodes[node].expanded = true;
                    let next = children
                        .iter()
                        .copied()
                        .find(|&c| target.starts_with(&self.nodes[c].path));
                    match next {
                        Some(next) => node = next,
                        None => {
                            // The path's next component isn't a listed
                            // subdirectory (vanished, or not a directory):
                            // stop at the deepest ancestor.
                            self.pending_reveal = None;
                            self.rebuild_visible();
                            if let Some(pos) = self.visible.iter().position(|&n| n == node) {
                                self.cursor = pos;
                            }
                            cx.notify();
                            return;
                        }
                    }
                }
            }
        }
    }

    fn load_children(&mut self, node: usize, cx: &mut Context<Self>) {
        if self.nodes[node].loading {
            return;
        }
        self.nodes[node].loading = true;
        let path = self.nodes[node].path.clone();
        let generation = self.generation;
        let (tx, rx) = tokio::sync::oneshot::channel::<Vec<(String, PathBuf)>>();
        self.tokio_handle.spawn(async move {
            let mut dirs: Vec<(String, PathBuf)> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&path) {
                for entry in entries.flatten() {
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                        || std::fs::metadata(entry.path())
                            .map(|m| m.is_dir())
                            .unwrap_or(false);
                    if is_dir {
                        dirs.push((
                            entry.file_name().to_string_lossy().into_owned(),
                            entry.path(),
                        ));
                    }
                }
            }
            dirs.sort_by_key(|(name, _)| name.to_lowercase());
            let _ = tx.send(dirs);
        });
        cx.spawn(async move |this, cx| {
            let dirs = rx.await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || node >= this.nodes.len() {
                    return;
                }
                this.nodes[node].loading = false;
                let depth = this.nodes[node].depth + 1;
                let mut children = Vec::with_capacity(dirs.len());
                for (name, path) in dirs {
                    children.push(this.nodes.len());
                    this.nodes.push(TreeNode {
                        path,
                        name: name.into(),
                        depth,
                        parent: Some(node),
                        children: None,
                        expanded: false,
                        loading: false,
                    });
                }
                this.nodes[node].children = Some(children);
                this.rebuild_visible();
                cx.notify();
                this.advance_reveal(cx);
            });
        })
        .detach();
    }

    /// Drops every listing and starts over at the root -- for a
    /// directory change from outside the tree, so the next
    /// [`Self::reveal`] reflects the filesystem as it is now.
    pub(crate) fn reset(&mut self) {
        self.generation += 1;
        self.nodes.truncate(1);
        let root = &mut self.nodes[0];
        root.children = None;
        root.expanded = false;
        root.loading = false;
        self.pending_reveal = None;
        self.cursor = 0;
        self.rebuild_visible();
    }

    fn rebuild_visible(&mut self) {
        let cursor_node = self.visible.get(self.cursor).copied();
        self.visible.clear();
        let mut stack = vec![0usize];
        while let Some(node) = stack.pop() {
            self.visible.push(node);
            if self.nodes[node].expanded
                && let Some(children) = &self.nodes[node].children
            {
                for &child in children.iter().rev() {
                    stack.push(child);
                }
            }
        }
        // Keep the cursor on the same node if it is still visible, else
        // on its nearest visible ancestor, else clamp.
        if let Some(mut node) = cursor_node {
            loop {
                if let Some(pos) = self.visible.iter().position(|&n| n == node) {
                    self.cursor = pos;
                    break;
                }
                match self.nodes[node].parent {
                    Some(parent) => node = parent,
                    None => {
                        self.cursor = 0;
                        break;
                    }
                }
            }
        }
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
    }

    pub(crate) fn cursor_path(&self) -> PathBuf {
        self.nodes[self.visible[self.cursor]].path.clone()
    }

    fn set_cursor(&mut self, pos: usize, cx: &mut Context<Self>) {
        let pos = pos.min(self.visible.len().saturating_sub(1));
        self.cursor = pos;
        self.scroll.scroll_to_item(pos, ScrollStrategy::Top);
        cx.notify();
    }

    pub(crate) fn move_cursor(&mut self, delta: i64, cx: &mut Context<Self>) {
        let len = self.visible.len() as i64;
        if len == 0 {
            return;
        }
        let target = (self.cursor as i64 + delta).clamp(0, len - 1) as usize;
        self.set_cursor(target, cx);
    }

    pub(crate) fn cursor_home(&mut self, cx: &mut Context<Self>) {
        self.set_cursor(0, cx);
    }

    pub(crate) fn cursor_end(&mut self, cx: &mut Context<Self>) {
        self.set_cursor(usize::MAX, cx);
    }

    /// Right: expand the node (listing it if needed), or step into its
    /// first child when it is already open.
    pub(crate) fn expand_cursor(&mut self, cx: &mut Context<Self>) {
        let node = self.visible[self.cursor];
        if self.nodes[node].expanded {
            if self.nodes[node]
                .children
                .as_ref()
                .is_some_and(|c| !c.is_empty())
            {
                self.set_cursor(self.cursor + 1, cx);
            }
            return;
        }
        self.nodes[node].expanded = true;
        if self.nodes[node].children.is_none() {
            self.load_children(node, cx);
        }
        self.rebuild_visible();
        cx.notify();
    }

    /// Left: collapse the node, or move to its parent when it is already
    /// closed (or a leaf).
    pub(crate) fn collapse_cursor(&mut self, cx: &mut Context<Self>) {
        let node = self.visible[self.cursor];
        if self.nodes[node].expanded {
            self.nodes[node].expanded = false;
            self.rebuild_visible();
            cx.notify();
            return;
        }
        if let Some(parent) = self.nodes[node].parent
            && let Some(pos) = self.visible.iter().position(|&n| n == parent)
        {
            self.set_cursor(pos, cx);
        }
    }

    fn toggle(&mut self, node: usize, cx: &mut Context<Self>) {
        if let Some(pos) = self.visible.iter().position(|&n| n == node) {
            self.cursor = pos;
        }
        if self.nodes[node].expanded {
            self.collapse_cursor(cx);
        } else {
            self.expand_cursor(cx);
        }
    }

    pub(crate) fn enter_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.cursor_path();
        (self.on_enter)(path, window, cx);
    }

    /// Visible rows that fit `viewport_height`, for paging.
    pub(crate) fn page(viewport_height: f32) -> usize {
        ((viewport_height / TREE_ROW_HEIGHT).floor() as usize).max(1)
    }

    #[cfg(test)]
    pub(crate) fn visible_paths(&self) -> Vec<PathBuf> {
        self.visible
            .iter()
            .map(|&n| self.nodes[n].path.clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub(crate) fn is_expanded(&self, path: &Path) -> bool {
        self.nodes.iter().any(|n| n.path == path && n.expanded)
    }

    fn folder_image(&self, window: &Window, cx: &mut App) -> Option<Arc<gpui::RenderImage>> {
        if !cx.has_global::<IconCache>() {
            return None;
        }
        let scale = window.scale_factor().ceil().max(1.0) as u32;
        let (id, names) = &self.folder_icon;
        cx.update_global::<IconCache, _>(|cache, cx| {
            cache.get_or_load(id, names, ICON_PX as u32, scale, cx)
        })
    }

    fn render_row(
        &mut self,
        pos: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (cursor_bg, cursor_fg) = {
            let tokens = TokenPalette::current(cx);
            (tokens.color.cursor_bg, tokens.color.cursor_fg)
        };
        let node_ix = self.visible[pos];
        let node = &self.nodes[node_ix];
        let is_cursor = pos == self.cursor;
        let arrow: &'static str = if node.loading {
            "…"
        } else if node.expanded {
            "▾"
        } else if node.children.as_ref().is_some_and(|c| c.is_empty()) {
            " "
        } else {
            "▸"
        };
        let name = node.name.clone();
        let indent = node.depth as f32 * INDENT_PX;
        let image = self.folder_image(window, cx);
        let mut row = h_flex()
            .id(("tree-row", pos))
            .h(px(TREE_ROW_HEIGHT))
            .w_full()
            .items_center()
            .gap_1()
            .pl(px(indent + 4.0))
            .pr_2()
            .text_ellipsis()
            .overflow_hidden()
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() >= 2 {
                    this.set_cursor(pos, cx);
                    this.enter_cursor(window, cx);
                } else {
                    this.set_cursor(pos, cx);
                }
            }))
            .child(
                div()
                    .id(("tree-arrow", pos))
                    .w(px(14.0))
                    .flex_none()
                    .text_center()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.toggle(node_ix, cx);
                    }))
                    .child(arrow),
            )
            .child(
                div()
                    .flex_none()
                    .size(px(ICON_PX))
                    .when_some(image, |slot, image| {
                        slot.child(img(ImageSource::Render(image)).size_full())
                    }),
            )
            .child(div().flex_1().min_w_0().truncate().child(name));
        if is_cursor {
            row = row.bg(cursor_bg).text_color(cursor_fg);
        }
        row.into_any_element()
    }
}

impl Render for TreeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.visible.len();
        uniform_list(
            "tree-view",
            count,
            cx.processor(|this, range: std::ops::Range<usize>, window, cx| {
                range
                    .map(|pos| this.render_row(pos, window, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .size_full()
        .track_scroll(self.scroll.clone())
    }
}
