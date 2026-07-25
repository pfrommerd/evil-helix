use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, atomic::Ordering, Arc},
};

use helix_core::Position;
use helix_view::{
    editor::Action,
    graphics::{CursorKind, Modifier, Rect, Style},
    input::{Event, MouseButton, MouseEventKind},
    Editor,
};
use nucleo::{
    pattern::{CaseMatching, Normalization},
    Config, Matcher, Nucleo,
};
use tui::buffer::Buffer as Surface;

use crate::{
    compositor::{Callback, Component, Context, EventResult},
    ctrl, key,
    ui::{Prompt, PromptEvent},
};

const MIN_EDITOR_WIDTH: u16 = 20;

fn new_search_matcher(num_threads: Option<usize>) -> Nucleo<PathBuf> {
    Nucleo::new(
        Config::DEFAULT.match_paths(),
        Arc::new(helix_event::request_redraw),
        num_threads,
        1,
    )
}

#[derive(Debug, Clone)]
struct Row {
    path: PathBuf,
    name: String,
    depth: usize,
    is_dir: bool,
    expansion_root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardKind {
    Copy,
    Cut,
}

#[derive(Debug, Clone)]
struct TreeClipboard {
    kind: ClipboardKind,
    paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub enum FileTreeAction {
    CursorUp,
    CursorDown,
    PageUp,
    PageDown,
    CursorTop,
    CursorMiddle,
    CursorBottom,
    CursorFirst,
    CursorLast,
    Collapse,
    Expand,
    Open,
    OpenHorizontalSplit,
    OpenVerticalSplit,
    Mark,
    Copy,
    Cut,
    Paste,
    CreateFile,
    CreateDirectory,
    Rename,
    Delete,
    Refresh,
    CollapseAll,
    ToggleHidden,
    WidthIncrease,
    WidthDecrease,
    FocusEditor,
    ToggleSearchFocus,
}

/// Persistent, keyboard-focused file tree rendered beside the editor.
pub struct FileTree {
    root: PathBuf,
    rows: Vec<Row>,
    directory_entries: HashMap<PathBuf, Vec<Row>>,
    manual_expanded: HashSet<PathBuf>,
    provisional_expanded: HashSet<PathBuf>,
    marked: HashSet<PathBuf>,
    cursor: usize,
    scroll: usize,
    visible: bool,
    focused: bool,
    width: u16,
    show_hidden: bool,
    clipboard: Option<TreeClipboard>,
    last_height: usize,
    last_area: Option<Rect>,
    dirty: Arc<AtomicBool>,
    pending_center: bool,
    search_prompt: Prompt,
    search_focused: bool,
    search_matcher: Nucleo<PathBuf>,
    search_matches: HashSet<PathBuf>,
    search_boundary_dirs: HashSet<PathBuf>,
    search_collapsed: HashSet<PathBuf>,
    search_expanded: HashSet<PathBuf>,
    search_restore_path: Option<PathBuf>,
    search_results_dirty: bool,
    search_initialized: bool,
}

impl FileTree {
    pub fn new(editor: &Editor) -> Self {
        let config = &editor.config().file_tree;
        let (min_width, max_width) = ordered_width_bounds(config.min_width, config.max_width);
        let root = helix_stdx::env::current_working_dir();
        let mut manual_expanded = HashSet::new();
        manual_expanded.insert(root.clone());
        let mut tree = Self {
            root,
            rows: Vec::new(),
            directory_entries: HashMap::new(),
            manual_expanded,
            provisional_expanded: HashSet::new(),
            marked: HashSet::new(),
            cursor: 0,
            scroll: 0,
            visible: config.visible,
            focused: false,
            width: config.width.clamp(min_width, max_width),
            show_hidden: !config.hidden,
            clipboard: None,
            last_height: 0,
            last_area: None,
            dirty: Arc::new(AtomicBool::new(false)),
            pending_center: false,
            search_prompt: Prompt::new(
                Cow::Borrowed(""),
                None,
                |_editor, _| Vec::new(),
                |_cx, _input, _event| {},
            ),
            search_focused: false,
            search_matcher: new_search_matcher(None),
            search_matches: HashSet::new(),
            search_boundary_dirs: HashSet::new(),
            search_collapsed: HashSet::new(),
            search_expanded: HashSet::new(),
            search_restore_path: None,
            search_results_dirty: false,
            search_initialized: false,
        };
        if tree.visible {
            tree.refresh(editor);
        }
        tree
    }

    pub fn toggle(&mut self, editor: &mut Editor) {
        self.visible = !self.visible;
        if !self.visible {
            self.focused = false;
            self.search_focused = false;
            self.rows.clear();
            self.directory_entries.clear();
        } else {
            self.refresh(editor);
        }
    }

    pub fn toggle_focus(&mut self, editor: &mut Editor) {
        if !self.visible {
            self.visible = true;
            self.focused = true;
            self.refresh(editor);
        } else {
            self.focused = !self.focused;
        }
        if !self.focused {
            self.search_focused = false;
        }
        if self.focused && editor.config().file_tree.auto_reveal {
            self.reveal_current(editor);
        }
    }

    pub fn focus_editor(&mut self) {
        self.focused = false;
        self.search_focused = false;
    }

    pub fn focused(&self) -> bool {
        self.focused
    }

    pub fn cursor(&self, editor: &Editor) -> (Option<Position>, CursorKind) {
        if !self.search_focused {
            return (None, CursorKind::Hidden);
        }
        let Some(area) = self.last_area else {
            return (None, CursorKind::Hidden);
        };
        self.search_prompt
            .cursor(area.clip_left(1).with_height(1), editor)
    }

    pub fn toggle_search_focus(&mut self, editor: &mut Editor) {
        let already_focused = self.visible && self.focused;
        if !self.visible {
            self.visible = true;
            self.focused = true;
            self.refresh(editor);
        } else if !self.focused {
            self.focused = true;
        }
        if already_focused {
            self.search_focused = !self.search_focused;
        } else {
            if editor.config().file_tree.auto_reveal {
                self.reveal_current(editor);
            }
            self.search_focused = true;
        }
        if self.search_focused && !self.search_initialized {
            self.start_search_scan(editor);
        }
    }

    pub fn increase_width(&mut self, editor: &Editor) {
        let config = &editor.config().file_tree;
        let (_, max_width) = ordered_width_bounds(config.min_width, config.max_width);
        self.width = self.width.saturating_add(config.width_step).min(max_width);
    }

    pub fn decrease_width(&mut self, editor: &Editor) {
        let config = &editor.config().file_tree;
        let (min_width, _) = ordered_width_bounds(config.min_width, config.max_width);
        self.width = self.width.saturating_sub(config.width_step).max(min_width);
    }

    pub fn layout(&mut self, area: Rect) -> (Rect, Option<Rect>) {
        if !self.visible || area.width < MIN_EDITOR_WIDTH.saturating_add(1) {
            self.last_area = None;
            if self.focused {
                self.focused = false;
            }
            return (area, None);
        }
        let width = self.width.min(area.width.saturating_sub(MIN_EDITOR_WIDTH));
        if width == 0 {
            self.last_area = None;
            if self.focused {
                self.focused = false;
            }
            return (area, None);
        }
        (
            area.clip_right(width),
            Some(Rect::new(area.right() - width, area.y, width, area.height)),
        )
    }

    pub fn refresh(&mut self, editor: &Editor) {
        let cwd = helix_stdx::env::current_working_dir();
        if cwd != self.root {
            self.root = cwd;
            self.manual_expanded.clear();
            self.manual_expanded.insert(self.root.clone());
            self.provisional_expanded.clear();
            self.directory_entries.clear();
            self.marked.clear();
            self.clipboard = None;
            self.cursor = 0;
            self.scroll = 0;
            self.search_matcher.restart(true);
            self.search_matches.clear();
            self.search_boundary_dirs.clear();
            self.search_collapsed.clear();
            self.search_expanded.clear();
            self.search_restore_path = None;
            self.search_results_dirty = false;
            self.search_initialized = false;
        }

        if !self.visible {
            self.rows.clear();
            self.directory_entries.clear();
            return;
        }

        let selected = self.rows.get(self.cursor).map(|row| row.path.clone());
        let mut directories: Vec<_> = self.effective_expansions().cloned().collect();
        directories.sort_by_key(|path| path.components().count());
        for directory in directories {
            if directory.is_dir() {
                self.load_directory(&directory, editor);
            } else {
                self.remove_expansion_subtree(&directory);
            }
        }
        self.provisional_expanded = self.provisional_expansions(editor);
        let mut directories: Vec<_> = self.effective_expansions().cloned().collect();
        directories.sort_by_key(|path| path.components().count());
        for directory in directories {
            if !self.directory_entries.contains_key(&directory) {
                self.load_directory(&directory, editor);
            }
        }
        self.prune_directory_entries();
        self.rebuild_rows(editor);
        if self.search_initialized || self.search_focused || !self.search_prompt.line().is_empty() {
            self.start_search_scan(editor);
        }
        self.marked.retain(|path| path.exists());
        if let Some(selected) = selected {
            if let Some(index) = self.rows.iter().position(|row| row.path == selected) {
                self.cursor = index;
            }
        }
        self.clamp_cursor();
    }

    pub fn collapse_all(&mut self, editor: &Editor) {
        if !self.search_prompt.line().is_empty() {
            self.search_expanded.clear();
            self.search_collapsed = self
                .rows
                .iter()
                .filter(|row| row.is_dir)
                .map(|row| row.path.clone())
                .collect();
            self.rebuild_rows(editor);
            return;
        }
        self.manual_expanded.clear();
        self.manual_expanded.insert(self.root.clone());
        self.prune_directory_entries();
        self.rebuild_rows(editor);
        self.clamp_cursor();
    }

    pub fn watched_directories(&self) -> Vec<PathBuf> {
        if !self.visible {
            return Vec::new();
        }
        self.directory_entries.keys().cloned().collect()
    }

    pub fn handle_file_events(&mut self, paths: &HashSet<PathBuf>, rescan: bool, editor: &Editor) {
        if !self.visible {
            return;
        }
        if rescan {
            self.refresh(editor);
            return;
        }

        let affected: Vec<_> = self
            .directory_entries
            .keys()
            .filter(|directory| {
                paths.iter().any(|path| {
                    path == *directory || path.parent().is_some_and(|parent| parent == *directory)
                })
            })
            .cloned()
            .collect();
        if affected.is_empty() {
            return;
        }
        for directory in affected {
            if directory.is_dir() {
                self.load_directory(&directory, editor);
            } else {
                self.remove_expansion_subtree(&directory);
            }
        }
        self.prune_directory_entries();
        self.rebuild_rows(editor);
        if self.search_initialized {
            self.start_search_scan(editor);
        }
        self.clamp_cursor();
    }

    fn load_directory(&mut self, dir: &Path, editor: &Editor) {
        self.directory_entries.insert(
            dir.to_path_buf(),
            read_directory(dir, self.show_hidden, editor),
        );
    }

    fn rebuild_rows(&mut self, editor: &Editor) {
        let selected = self.rows.get(self.cursor).map(|row| row.path.clone());
        if !self.search_prompt.line().is_empty() {
            self.rebuild_search_rows(editor.config().file_tree.flatten_dirs);
            self.restore_selected_path(selected.as_deref(), false);
            return;
        }
        let mut rows = Vec::new();
        self.collect_rows(&self.root, 0, editor, &mut rows);
        self.rows = rows;
        self.restore_selected_path(selected.as_deref(), false);
    }

    fn restore_selected_path(&mut self, selected: Option<&Path>, anchor_ancestors: bool) {
        if let Some(index) =
            selected.and_then(|path| self.rows.iter().position(|row| row.path == path))
        {
            self.cursor = index;
        } else {
            self.cursor = self
                .rows
                .iter()
                .position(|row| self.search_matches.contains(&row.path))
                .unwrap_or(0);
        }
        if anchor_ancestors && !self.rows.is_empty() {
            self.scroll = first_visible_ancestor_index(&self.rows, self.cursor);
        }
        self.clamp_cursor();
    }

    fn rebuild_search_rows(&mut self, flatten_dirs: bool) {
        if self.search_prompt.line().is_empty() {
            return;
        }
        let snapshot = self.search_matcher.snapshot();
        let pattern = snapshot.pattern().column_pattern(0);
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let mut indices = Vec::new();
        let matched_paths = snapshot
            .matched_items(..)
            .map(|item| {
                indices.clear();
                pattern.indices(
                    item.matcher_columns[0].slice(..),
                    &mut matcher,
                    &mut indices,
                );
                SearchMatch {
                    path: item.data.clone(),
                    boundary: search_match_boundary(&self.root, item.data, &indices),
                }
            })
            .collect::<Vec<_>>();
        let (rows, matches, boundary_dirs) = build_search_rows(
            &self.root,
            &matched_paths,
            &self.search_collapsed,
            &self.search_expanded,
            flatten_dirs,
        );
        self.rows = rows;
        self.search_matches = matches;
        self.search_boundary_dirs = boundary_dirs;
    }

    fn search_query_changed(&mut self, previous: &str, editor: &Editor) {
        let query = self.search_prompt.line().clone();
        let restoring = if previous.is_empty() && !query.is_empty() {
            self.search_restore_path = self.rows.get(self.cursor).map(|row| row.path.clone());
            None
        } else if !previous.is_empty() && query.is_empty() {
            self.search_restore_path.take()
        } else {
            None
        };
        self.search_matcher.pattern.reparse(
            0,
            &query,
            CaseMatching::Smart,
            Normalization::Smart,
            query.starts_with(previous),
        );
        self.search_collapsed.clear();
        self.search_expanded.clear();
        if query.is_empty() {
            self.search_boundary_dirs.clear();
            self.rebuild_rows(editor);
            if let Some(path) = restoring {
                self.restore_selected_path(Some(&path), false);
            }
        }
    }

    fn start_search_scan(&mut self, editor: &Editor) {
        self.search_initialized = true;
        self.search_matcher.restart(true);
        self.search_results_dirty = false;
        let root = self.root.clone();
        let files =
            super::file_picker_paths_with_hidden(editor, root.clone(), Some(!self.show_hidden));
        let injector = self.search_matcher.injector();
        std::thread::spawn(move || {
            for path in files {
                injector.push(path, |path, columns| {
                    columns[0] = path
                        .strip_prefix(&root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .into();
                });
            }
            helix_event::request_redraw();
        });
    }

    fn poll_search_matcher(&mut self, editor: &Editor) {
        let status = self.search_matcher.tick(10);
        self.search_results_dirty |= status.changed;
        if status.running
            || self.search_matcher.active_injectors() > 0
            || !self.search_results_dirty
            || self.search_prompt.line().is_empty()
        {
            return;
        }
        self.search_results_dirty = false;
        let selected = self.rows.get(self.cursor).map(|row| row.path.clone());
        self.rebuild_rows(editor);
        self.restore_selected_path(selected.as_deref(), true);
    }

    fn collect_rows(&self, dir: &Path, depth: usize, editor: &Editor, out: &mut Vec<Row>) {
        let Some(entries) = self.directory_entries.get(dir) else {
            return;
        };
        for entry in entries {
            let mut row = entry.clone();
            row.depth = depth;
            row.expansion_root = entry.path.clone();
            if editor.config().file_tree.flatten_dirs && row.is_dir {
                while self.is_expanded(&row.path) {
                    let Some(children) = self.directory_entries.get(&row.path) else {
                        break;
                    };
                    let [child] = children.as_slice() else {
                        break;
                    };
                    if !child.is_dir || !self.is_expanded(&child.path) {
                        break;
                    }
                    row.name.push('/');
                    row.name.push_str(&child.name);
                    row.path = child.path.clone();
                }
            }
            out.push(row.clone());
            if row.is_dir && self.is_expanded(&row.path) {
                self.collect_rows(&row.path, depth + 1, editor, out);
            }
        }
    }

    fn is_expanded(&self, path: &Path) -> bool {
        self.manual_expanded.contains(path) || self.provisional_expanded.contains(path)
    }

    fn effective_expansions(&self) -> impl Iterator<Item = &PathBuf> {
        self.manual_expanded
            .iter()
            .chain(self.provisional_expanded.iter())
    }

    fn remove_expansion_subtree(&mut self, root: &Path) {
        self.manual_expanded.retain(|path| !path.starts_with(root));
        self.provisional_expanded
            .retain(|path| !path.starts_with(root));
        self.directory_entries
            .retain(|path, _| !path.starts_with(root));
    }

    fn collapse_manual_subtree(&mut self, root: &Path) {
        self.manual_expanded.retain(|path| !path.starts_with(root));
        self.prune_directory_entries();
    }

    fn prune_directory_entries(&mut self) {
        let expanded = self.effective_expansions().cloned().collect::<HashSet<_>>();
        self.directory_entries
            .retain(|path, _| expanded.contains(path));
    }

    fn expand_directory(&mut self, path: PathBuf, editor: &Editor) {
        let flatten = editor.config().file_tree.flatten_dirs;
        let mut ancestors = path
            .ancestors()
            .take_while(|ancestor| ancestor.starts_with(&self.root))
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        ancestors.reverse();
        for ancestor in ancestors.iter().take(ancestors.len().saturating_sub(1)) {
            self.manual_expanded.insert(ancestor.clone());
            if !self.directory_entries.contains_key(ancestor) {
                self.load_directory(ancestor, editor);
            }
        }
        let mut directory = path;
        loop {
            let newly_expanded = self.manual_expanded.insert(directory.clone());
            if !newly_expanded && self.directory_entries.contains_key(&directory) {
                break;
            }
            self.load_directory(&directory, editor);
            if !flatten {
                break;
            }
            let Some([child]) = self.directory_entries.get(&directory).map(Vec::as_slice) else {
                break;
            };
            if !child.is_dir {
                break;
            }
            directory = child.path.clone();
        }
        self.rebuild_rows(editor);
        self.clamp_cursor();
    }

    fn provisional_expansions(&self, editor: &Editor) -> HashSet<PathBuf> {
        let mut expanded = HashSet::new();
        for path in editor.documents().filter_map(|document| document.path()) {
            let Some(ancestors) = self.visible_ancestors(path, editor) else {
                continue;
            };
            expanded.extend(ancestors);
        }
        expanded
    }

    fn visible_ancestors(&self, path: &Path, editor: &Editor) -> Option<Vec<PathBuf>> {
        if path == self.root || !path.starts_with(&self.root) {
            return None;
        }

        let mut parent = self.root.clone();
        let mut ancestors = vec![parent.clone()];
        for component in path.strip_prefix(&self.root).ok()?.components() {
            let child = parent.join(component);
            let cached;
            let entries = if let Some(entries) = self.directory_entries.get(&parent) {
                entries
            } else {
                cached = read_directory(&parent, self.show_hidden, editor);
                &cached
            };
            let entry = entries.iter().find(|entry| entry.path == child)?;
            if entry.is_dir {
                ancestors.push(child.clone());
            }
            parent = child;
        }
        Some(ancestors)
    }
}

fn read_directory(dir: &Path, show_hidden: bool, editor: &Editor) -> Vec<Row> {
    let config = &editor.config().file_tree;
    let mut builder = ignore::WalkBuilder::new(dir);
    let mut entries: Vec<_> = builder
        .max_depth(Some(1))
        .hidden(!show_hidden)
        .parents(config.parents)
        .ignore(config.ignore)
        .git_ignore(config.git_ignore)
        .git_global(config.git_global)
        .git_exclude(config.git_exclude)
        .follow_links(config.follow_symlinks)
        .add_custom_ignore_filename(helix_loader::config_dir().join("ignore"))
        .add_custom_ignore_filename(".helix/ignore")
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.path() != dir)
        .collect();
    entries.sort_by(|a, b| {
        let a_dir = a.file_type().is_some_and(|ty| ty.is_dir());
        let b_dir = b.file_type().is_some_and(|ty| ty.is_dir());
        (
            !a_dir,
            a.file_name().to_string_lossy().to_lowercase(),
            a.file_name(),
        )
            .cmp(&(
                !b_dir,
                b.file_name().to_string_lossy().to_lowercase(),
                b.file_name(),
            ))
    });

    entries
        .into_iter()
        .filter_map(|entry| {
            let path = entry.path().to_path_buf();
            let Some(file_type) = entry.file_type() else {
                return None;
            };
            let is_dir = file_type.is_dir()
                || (config.follow_symlinks && file_type.is_symlink() && path.is_dir());
            Some(Row {
                expansion_root: path.clone(),
                path,
                name: entry.file_name().to_string_lossy().into_owned(),
                depth: 0,
                is_dir,
            })
        })
        .collect()
}

struct SearchNode {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

struct SearchMatch {
    path: PathBuf,
    boundary: PathBuf,
}

fn first_visible_ancestor_index(rows: &[Row], cursor: usize) -> usize {
    if rows.is_empty() {
        return 0;
    }
    rows[..=cursor.min(rows.len().saturating_sub(1))]
        .iter()
        .rposition(|row| row.depth == 0)
        .unwrap_or(0)
}

fn search_match_boundary(root: &Path, file: &Path, indices: &[u32]) -> PathBuf {
    let relative = file.strip_prefix(root).unwrap_or(file);
    let Some(last_match) = indices.iter().copied().max() else {
        return file.to_path_buf();
    };
    let component_count = relative
        .to_string_lossy()
        .chars()
        .enumerate()
        .take_while(|(index, _)| (*index as u32) < last_match)
        .filter(|(_, ch)| std::path::is_separator(*ch))
        .count()
        + 1;
    root.join(
        relative
            .components()
            .take(component_count)
            .collect::<PathBuf>(),
    )
}

fn collect_search_rows(
    directory: &Path,
    depth: usize,
    children: &HashMap<PathBuf, Vec<SearchNode>>,
    collapsed: &HashSet<PathBuf>,
    flatten_dirs: bool,
    out: &mut Vec<Row>,
) {
    let Some(entries) = children.get(directory) else {
        return;
    };
    for entry in entries {
        let expansion_root = entry.path.clone();
        let mut path = entry.path.clone();
        let mut name = entry.name.clone();
        if flatten_dirs && entry.is_dir {
            while !collapsed.contains(&path) {
                let Some([child]) = children.get(&path).map(Vec::as_slice) else {
                    break;
                };
                if !child.is_dir {
                    break;
                }
                name.push('/');
                name.push_str(&child.name);
                path = child.path.clone();
            }
        }
        out.push(Row {
            path: path.clone(),
            name,
            depth,
            is_dir: entry.is_dir,
            expansion_root,
        });
        if entry.is_dir && !collapsed.contains(&path) {
            collect_search_rows(&path, depth + 1, children, collapsed, flatten_dirs, out);
        }
    }
}

fn build_search_rows(
    root: &Path,
    matched_files: &[SearchMatch],
    collapsed: &HashSet<PathBuf>,
    expanded: &HashSet<PathBuf>,
    flatten_dirs: bool,
) -> (Vec<Row>, HashSet<PathBuf>, HashSet<PathBuf>) {
    let matches = matched_files
        .iter()
        .map(|matched| matched.boundary.clone())
        .collect::<HashSet<_>>();
    let boundary_dirs = matched_files
        .iter()
        .filter(|matched| matched.boundary != matched.path)
        .map(|matched| matched.boundary.clone())
        .collect::<HashSet<_>>();
    let mut nodes = HashMap::new();
    for matched in matched_files {
        nodes.insert(matched.path.clone(), false);
        let mut parent = matched.path.parent();
        while let Some(path) = parent {
            if path == root || !path.starts_with(root) {
                break;
            }
            nodes.insert(path.to_path_buf(), true);
            parent = path.parent();
        }
    }

    let mut children: HashMap<PathBuf, Vec<SearchNode>> = HashMap::new();
    for (path, is_dir) in nodes {
        let parent = path.parent().unwrap_or(root).to_path_buf();
        let Some(name) = path.file_name() else {
            continue;
        };
        children.entry(parent).or_default().push(SearchNode {
            name: name.to_string_lossy().into_owned(),
            path,
            is_dir,
        });
    }
    for entries in children.values_mut() {
        entries.sort_by(|a, b| {
            (!a.is_dir, a.name.to_lowercase(), &a.name).cmp(&(
                !b.is_dir,
                b.name.to_lowercase(),
                &b.name,
            ))
        });
    }

    let mut rows = Vec::new();
    let mut effective_collapsed = collapsed.clone();
    effective_collapsed.extend(
        boundary_dirs
            .iter()
            .filter(|path| !expanded.contains(*path))
            .cloned(),
    );
    collect_search_rows(
        root,
        0,
        &children,
        &effective_collapsed,
        flatten_dirs,
        &mut rows,
    );
    (rows, matches, boundary_dirs)
}

impl FileTree {
    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        if self.last_height == 0 {
            self.scroll = self.cursor;
            return;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + self.last_height {
            self.scroll = self.cursor + 1 - self.last_height;
        }
    }

    fn center_cursor(&mut self) {
        if self.rows.is_empty() || self.last_height == 0 {
            self.scroll = 0;
            return;
        }
        let max_scroll = self.rows.len().saturating_sub(self.last_height);
        self.scroll = self
            .cursor
            .saturating_sub(self.last_height / 2)
            .min(max_scroll);
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        self.pending_center = false;
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.rows.len() - 1);
        self.clamp_cursor();
    }

    fn move_cursor_to_visible_row(&mut self, position: VisiblePosition) {
        if self.rows.is_empty() {
            return;
        }
        self.pending_center = false;

        let first = self.scroll.min(self.rows.len() - 1);
        let visible_len = self.last_height.min(self.rows.len() - first).max(1);
        let offset = match position {
            VisiblePosition::Top => 0,
            VisiblePosition::Middle => (visible_len - 1) / 2,
            VisiblePosition::Bottom => visible_len - 1,
        };
        self.cursor = first + offset;
        self.clamp_cursor();
    }

    fn reveal_current(&mut self, editor: &Editor) {
        let Some(path) = editor
            .document(editor.tree.get(editor.tree.focus).doc)
            .and_then(|d| d.path())
        else {
            return;
        };
        let path = path.to_path_buf();
        if !path.starts_with(&self.root) {
            return;
        }
        self.refresh(editor);
        if let Some(index) = self.rows.iter().position(|row| row.path == path) {
            self.cursor = index;
            self.pending_center = true;
        }
    }

    fn open_selected(&mut self, editor: &mut Editor, action: Action) {
        let Some(path) = self
            .rows
            .get(self.cursor)
            .filter(|row| !row.is_dir)
            .map(|row| row.path.clone())
        else {
            return;
        };
        match editor.open(&path, action) {
            Ok(_) => self.focused = false,
            Err(err) => {
                editor.set_error(format!("Could not open {}: {err}", path.display()));
            }
        }
    }

    fn search_directory_expanded(&self, path: &Path) -> bool {
        !self.search_collapsed.contains(path)
            && (!self.search_boundary_dirs.contains(path) || self.search_expanded.contains(path))
    }

    fn toggle_expand(&mut self, editor: &Editor) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if !row.is_dir {
            return;
        }
        if !self.search_prompt.line().is_empty() {
            if self.search_directory_expanded(&row.path) {
                self.search_expanded.remove(&row.path);
                self.search_collapsed.insert(row.path);
            } else {
                self.search_collapsed.remove(&row.path);
                self.search_expanded.insert(row.path);
            }
            self.rebuild_rows(editor);
            return;
        }
        if self.is_expanded(&row.expansion_root) {
            self.collapse_manual_subtree(&row.expansion_root);
            self.rebuild_rows(editor);
            self.clamp_cursor();
        } else {
            self.expand_directory(row.path, editor);
        }
    }

    fn collapse_or_parent(&mut self, editor: &mut Editor) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if !self.search_prompt.line().is_empty() {
            if row.is_dir && self.search_directory_expanded(&row.path) {
                self.search_expanded.remove(&row.path);
                self.search_collapsed.insert(row.path);
                self.rebuild_rows(editor);
                return;
            }
            if let Some(parent) = row.path.parent() {
                if let Some(index) = self
                    .rows
                    .iter()
                    .position(|candidate| candidate.path == parent)
                {
                    self.cursor = index;
                    self.clamp_cursor();
                }
            }
            return;
        }
        if row.is_dir && self.is_expanded(&row.expansion_root) {
            self.collapse_manual_subtree(&row.expansion_root);
            self.rebuild_rows(editor);
            self.clamp_cursor();
            return;
        }
        let Some(parent) = row.path.parent() else {
            return;
        };
        if let Some(index) = self
            .rows
            .iter()
            .position(|candidate| candidate.path == parent)
        {
            self.cursor = index;
            self.clamp_cursor();
        }
    }

    fn expand_selected(&mut self, editor: &Editor) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if !self.search_prompt.line().is_empty() {
            if row.is_dir && !self.search_directory_expanded(&row.path) {
                self.search_collapsed.remove(&row.path);
                self.search_expanded.insert(row.path);
                self.rebuild_rows(editor);
            }
            return;
        }
        if row.is_dir {
            self.expand_directory(row.path, editor);
        }
    }

    fn toggle_mark(&mut self) {
        let Some(row) = self.rows.get(self.cursor) else {
            return;
        };
        if !self.marked.remove(&row.path) {
            self.marked.insert(row.path.clone());
        }
        if !self.rows.is_empty() {
            self.cursor = (self.cursor + 1).min(self.rows.len() - 1);
            self.clamp_cursor();
        }
    }

    fn operation_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<_> = if self.marked.is_empty() {
            self.rows
                .get(self.cursor)
                .map(|row| vec![row.path.clone()])
                .unwrap_or_default()
        } else {
            self.marked.iter().cloned().collect()
        };
        paths.sort();
        let mut normalized = Vec::new();
        for path in paths {
            if !normalized
                .iter()
                .any(|parent: &PathBuf| path.starts_with(parent))
            {
                normalized.push(path);
            }
        }
        normalized
    }

    fn set_clipboard(&mut self, kind: ClipboardKind, editor: &mut Editor) {
        let paths = self.operation_paths();
        if paths.is_empty() {
            return;
        }
        editor.set_status(format!(
            "{} {} path(s)",
            if kind == ClipboardKind::Copy {
                "Copied"
            } else {
                "Cut"
            },
            paths.len()
        ));
        self.clipboard = Some(TreeClipboard { kind, paths });
    }

    fn paste(&mut self, editor: &mut Editor) {
        let Some(clipboard) = self.clipboard.clone() else {
            editor.set_error("File-tree clipboard is empty");
            return;
        };
        let Some(row) = self.rows.get(self.cursor) else {
            return;
        };
        let destination = if row.is_dir {
            row.path.clone()
        } else {
            row.path.parent().unwrap_or(&self.root).to_path_buf()
        };
        let targets: Vec<_> = clipboard
            .paths
            .iter()
            .filter_map(|source| {
                source
                    .file_name()
                    .map(|name| (source, destination.join(name)))
            })
            .collect();

        if let Some((_, target)) = targets.iter().find(|(_, target)| target.exists()) {
            editor.set_error(format!("Destination already exists: {}", target.display()));
            return;
        }
        if let Some((source, target)) = targets
            .iter()
            .find(|(source, target)| target.starts_with(source))
        {
            editor.set_error(format!(
                "Cannot place {} inside itself ({})",
                source.display(),
                target.display()
            ));
            return;
        }

        let mut completed = 0;
        for (source, target) in targets {
            let result = match clipboard.kind {
                ClipboardKind::Cut => editor.move_path(source, &target),
                ClipboardKind::Copy => copy_path(source, &target),
            };
            if let Err(err) = result {
                editor.set_error(format!(
                    "File operation stopped after {completed} item(s): {err}"
                ));
                self.refresh(editor);
                return;
            }
            completed += 1;
        }
        if clipboard.kind == ClipboardKind::Cut {
            self.clipboard = None;
        }
        self.marked.clear();
        self.refresh(editor);
        editor.set_status(format!("Pasted {completed} item(s)"));
    }

    fn prompt_create(&self, is_dir: bool) -> Callback {
        let parent = self
            .rows
            .get(self.cursor)
            .map(|row| {
                if row.is_dir {
                    row.path.clone()
                } else {
                    row.path.parent().unwrap_or(&self.root).to_path_buf()
                }
            })
            .unwrap_or_else(|| self.root.clone());
        let dirty = Arc::clone(&self.dirty);
        Box::new(move |compositor, _cx| {
            let prompt = Prompt::new(
                if is_dir {
                    Cow::Borrowed("New directory: ")
                } else {
                    Cow::Borrowed("New file: ")
                },
                None,
                |_editor, _| Vec::new(),
                move |cx, input, event| {
                    if event == PromptEvent::Validate && !input.is_empty() {
                        let path = parent.join(input);
                        if let Err(err) = cx.editor.create_path(&path, is_dir) {
                            cx.editor
                                .set_error(format!("Could not create {}: {err}", path.display()));
                        } else {
                            dirty.store(true, Ordering::Relaxed);
                        }
                    }
                },
            );
            compositor.push(Box::new(prompt));
        })
    }

    fn prompt_rename(&self) -> Option<Callback> {
        let source = self.rows.get(self.cursor)?.path.clone();
        let initial = source.file_name()?.to_string_lossy().into_owned();
        let dirty = Arc::clone(&self.dirty);
        Some(Box::new(move |compositor, cx| {
            let target_source = source.clone();
            let prompt = Prompt::new(
                Cow::Borrowed("Rename: "),
                None,
                |_editor, _| Vec::new(),
                move |cx, input, event| {
                    if event == PromptEvent::Validate && !input.is_empty() {
                        let target = target_source.parent().unwrap().join(input);
                        if target.exists() {
                            cx.editor.set_error(format!(
                                "Destination already exists: {}",
                                target.display()
                            ));
                        } else if let Err(err) = cx.editor.move_path(&target_source, &target) {
                            cx.editor.set_error(format!("Could not rename: {err}"));
                        } else {
                            dirty.store(true, Ordering::Relaxed);
                        }
                    }
                },
            )
            .with_line(initial.clone(), cx.editor);
            compositor.push(Box::new(prompt));
        }))
    }

    fn prompt_delete(&self, editor: &Editor) -> Option<Callback> {
        let paths = self.operation_paths();
        if paths.is_empty() {
            return None;
        }
        let modified = editor
            .documents()
            .filter(|doc| {
                doc.is_modified()
                    && doc
                        .path()
                        .is_some_and(|path| paths.iter().any(|selected| path.starts_with(selected)))
            })
            .count();
        let dirty = Arc::clone(&self.dirty);
        Some(Box::new(move |compositor, _cx| {
            let delete_paths = paths.clone();
            let warning = if modified == 0 {
                String::new()
            } else {
                format!(" ({modified} modified buffer(s))")
            };
            let prompt = Prompt::new(
                format!(
                    "Permanently delete {} path(s){warning}? Type yes: ",
                    paths.len()
                )
                .into(),
                None,
                |_editor, _| Vec::new(),
                move |cx, input, event| {
                    if event != PromptEvent::Validate || input != "yes" {
                        return;
                    }
                    for path in &delete_paths {
                        if let Err(err) = cx.editor.delete_path(path, true) {
                            cx.editor
                                .set_error(format!("Could not delete {}: {err}", path.display()));
                            break;
                        } else {
                            dirty.store(true, Ordering::Relaxed);
                        }
                    }
                },
            );
            compositor.push(Box::new(prompt));
        }))
    }

    pub fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if !self.visible {
            return EventResult::Ignored(None);
        }
        self.poll_search_matcher(cx.editor);
        if self.search_focused {
            match event {
                Event::Key(key!(':')) => return EventResult::Ignored(None),
                Event::Key(key!(Esc) | key!(Enter) | ctrl!('c')) => {
                    self.search_focused = false;
                    return EventResult::Consumed(None);
                }
                Event::Key(_) | Event::Paste(_) => {
                    let previous = self.search_prompt.line().clone();
                    let _ = self.search_prompt.handle_event(event, cx);
                    if self.search_prompt.line() != &previous {
                        self.search_query_changed(&previous, cx.editor);
                    }
                    return EventResult::Consumed(None);
                }
                _ => {}
            }
        }
        if let Event::Mouse(mouse) = event {
            let Some(area) = self.last_area else {
                return EventResult::Ignored(None);
            };
            if mouse.column < area.x
                || mouse.column >= area.right()
                || mouse.row < area.y
                || mouse.row >= area.bottom()
            {
                return EventResult::Ignored(None);
            }
            match mouse.kind {
                MouseEventKind::ScrollUp => self.move_cursor(-3),
                MouseEventKind::ScrollDown => self.move_cursor(3),
                MouseEventKind::Down(MouseButton::Left) => {
                    if mouse.row == area.y {
                        self.focused = true;
                        self.search_focused = true;
                        if !self.search_initialized {
                            self.start_search_scan(cx.editor);
                        }
                        return EventResult::Consumed(None);
                    }
                    let index = self.scroll + usize::from(mouse.row - area.y - 1);
                    if index < self.rows.len() {
                        let same = self.focused && self.cursor == index;
                        let disclosure_column = area.x.saturating_add(
                            3 + u16::try_from(self.rows[index].depth.saturating_mul(2))
                                .unwrap_or(u16::MAX),
                        );
                        self.focused = true;
                        self.search_focused = false;
                        self.cursor = index;
                        self.pending_center = false;
                        self.clamp_cursor();
                        if self.rows[index].is_dir && (same || mouse.column == disclosure_column) {
                            self.toggle_expand(cx.editor);
                        }
                    }
                }
                _ => {}
            }
            return EventResult::Consumed(None);
        }
        EventResult::Ignored(None)
    }

    pub fn execute(&mut self, action: FileTreeAction, editor: &mut Editor) -> Option<Callback> {
        match action {
            FileTreeAction::CursorUp => self.move_cursor(-1),
            FileTreeAction::CursorDown => self.move_cursor(1),
            FileTreeAction::PageUp => {
                self.move_cursor(-(self.last_height as isize / 2).max(1));
            }
            FileTreeAction::PageDown => {
                self.move_cursor((self.last_height as isize / 2).max(1));
            }
            FileTreeAction::CursorTop => self.move_cursor_to_visible_row(VisiblePosition::Top),
            FileTreeAction::CursorMiddle => {
                self.move_cursor_to_visible_row(VisiblePosition::Middle);
            }
            FileTreeAction::CursorBottom => {
                self.move_cursor_to_visible_row(VisiblePosition::Bottom);
            }
            FileTreeAction::CursorFirst => {
                self.pending_center = false;
                self.cursor = 0;
                self.clamp_cursor();
            }
            FileTreeAction::CursorLast => {
                self.pending_center = false;
                self.cursor = self.rows.len().saturating_sub(1);
                self.clamp_cursor();
            }
            FileTreeAction::Collapse => self.collapse_or_parent(editor),
            FileTreeAction::Expand => self.expand_selected(editor),
            FileTreeAction::Open => {
                if self.rows.get(self.cursor).is_some_and(|row| row.is_dir) {
                    self.toggle_expand(editor);
                } else {
                    self.open_selected(editor, Action::Replace);
                }
            }
            FileTreeAction::OpenHorizontalSplit => {
                self.open_selected(editor, Action::HorizontalSplit);
            }
            FileTreeAction::OpenVerticalSplit => {
                self.open_selected(editor, Action::VerticalSplit);
            }
            FileTreeAction::Mark => self.toggle_mark(),
            FileTreeAction::Copy => self.set_clipboard(ClipboardKind::Copy, editor),
            FileTreeAction::Cut => self.set_clipboard(ClipboardKind::Cut, editor),
            FileTreeAction::Paste => self.paste(editor),
            FileTreeAction::CreateFile => return Some(self.prompt_create(false)),
            FileTreeAction::CreateDirectory => return Some(self.prompt_create(true)),
            FileTreeAction::Rename => return self.prompt_rename(),
            FileTreeAction::Delete => return self.prompt_delete(editor),
            FileTreeAction::Refresh => self.refresh(editor),
            FileTreeAction::CollapseAll => self.collapse_all(editor),
            FileTreeAction::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                self.refresh(editor);
            }
            FileTreeAction::WidthIncrease => self.increase_width(editor),
            FileTreeAction::WidthDecrease => self.decrease_width(editor),
            FileTreeAction::FocusEditor => self.focus_editor(),
            FileTreeAction::ToggleSearchFocus => self.toggle_search_focus(editor),
        }
        None
    }

    pub fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        self.last_area = Some(area);
        if self.dirty.swap(false, Ordering::Relaxed)
            || helix_stdx::env::current_working_dir() != self.root
            || self.provisional_expanded != self.provisional_expansions(cx.editor)
        {
            self.refresh(cx.editor);
        }
        self.poll_search_matcher(cx.editor);
        self.last_height = area.height.saturating_sub(1) as usize;
        if self.pending_center {
            self.center_cursor();
            self.pending_center = false;
        } else {
            self.clamp_cursor();
        }

        let editor = &mut *cx.editor;
        let background = editor.theme.get("ui.background");
        let text = editor.theme.get("ui.text");
        let directory = editor.theme.get("ui.text.directory");
        let selected = editor.theme.get("ui.selection");
        let border = editor.theme.get("ui.window");
        surface.clear_with(area, background);
        for y in area.top()..area.bottom() {
            surface[(area.x, y)].set_symbol("│").set_style(border);
        }

        let active_path = editor
            .document(editor.tree.get(editor.tree.focus).doc)
            .and_then(|doc| doc.path())
            .map(Path::to_path_buf);
        let open_paths = editor
            .documents()
            .filter_map(|document| document.path().map(Path::to_path_buf))
            .collect::<HashSet<_>>();
        let diagnostics = if editor.config().file_tree.diagnostics {
            aggregate_diagnostics(editor.workspace_diagnostic_counts(), &self.root)
        } else {
            HashMap::new()
        };
        for (screen_row, row) in self
            .rows
            .iter()
            .skip(self.scroll)
            .take(self.last_height)
            .enumerate()
        {
            let index = self.scroll + screen_row;
            let y = area.y + 1 + screen_row as u16;
            let marker = if self.marked.contains(&row.path) {
                "●"
            } else {
                " "
            };
            let disclosure = if row.is_dir {
                let expanded = if self.search_prompt.line().is_empty() {
                    self.is_expanded(&row.path)
                } else {
                    self.search_directory_expanded(&row.path)
                };
                if expanded {
                    "▾"
                } else {
                    "▸"
                }
            } else {
                " "
            };
            let name = &row.name;
            let badge = metadata_badge(diagnostics.get(&row.path).copied());
            let line = format!("{marker} {}{disclosure} {name}", "  ".repeat(row.depth));
            let mut style: Style = if row.is_dir { directory } else { text };
            if active_path.as_ref() == Some(&row.path) {
                style = style.add_modifier(Modifier::BOLD);
            } else if open_paths.contains(&row.path) {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if index == self.cursor && self.focused && !self.search_focused {
                style = style.patch(selected);
            }
            surface.set_stringn(
                area.x.saturating_add(1),
                y,
                &line,
                area.width.saturating_sub(2) as usize,
                style,
            );
            if !badge.is_empty() {
                let badge_width = badge.chars().count() as u16;
                if badge_width + 2 < area.width {
                    let diagnostic = diagnostics.get(&row.path).copied();
                    let badge_style = if diagnostic.is_some_and(|counts| counts.0 > 0) {
                        editor.theme.get("error")
                    } else if diagnostic.is_some_and(|counts| counts.1 > 0) {
                        editor.theme.get("warning")
                    } else {
                        editor.theme.get("diff.delta")
                    };
                    surface.set_stringn(
                        area.right().saturating_sub(badge_width + 1),
                        y,
                        &badge,
                        badge_width as usize,
                        badge_style,
                    );
                }
            }
        }
        let search_area = area.clip_left(1).with_height(1);
        self.search_prompt.render(search_area, surface, cx);
    }
}

fn copy_path(source: &Path, target: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        let link = fs::read_link(source)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(link, target)?;
        #[cfg(windows)]
        if source.is_dir() {
            std::os::windows::fs::symlink_dir(link, target)?;
        } else {
            std::os::windows::fs::symlink_file(link, target)?;
        }
    } else if metadata.is_dir() {
        fs::create_dir(target)?;
        fs::set_permissions(target, metadata.permissions())?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_path(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else {
        fs::copy(source, target)?;
    }
    Ok(())
}

fn ordered_width_bounds(a: u16, b: u16) -> (u16, u16) {
    (a.min(b), a.max(b))
}

#[derive(Debug, Clone, Copy)]
enum VisiblePosition {
    Top,
    Middle,
    Bottom,
}

fn aggregate_diagnostics(
    direct: HashMap<PathBuf, (usize, usize)>,
    root: &Path,
) -> HashMap<PathBuf, (usize, usize)> {
    let mut result = HashMap::new();
    for (path, counts) in direct {
        let mut current = Some(path.as_path());
        while let Some(path) = current {
            if !path.starts_with(root) {
                break;
            }
            let aggregate = result.entry(path.to_path_buf()).or_insert((0, 0));
            aggregate.0 += counts.0;
            aggregate.1 += counts.1;
            current = path.parent();
        }
    }
    result
}

fn metadata_badge(diagnostics: Option<(usize, usize)>) -> String {
    let mut badge = String::new();
    if let Some((errors, warnings)) = diagnostics {
        if errors > 0 {
            badge.push_str(&format!(" E{errors}"));
        }
        if warnings > 0 {
            badge.push_str(&format!(" W{warnings}"));
        }
    }
    badge
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_tree(root: PathBuf) -> FileTree {
        FileTree {
            root,
            rows: Vec::new(),
            directory_entries: HashMap::new(),
            manual_expanded: HashSet::new(),
            provisional_expanded: HashSet::new(),
            marked: HashSet::new(),
            cursor: 0,
            scroll: 0,
            visible: true,
            focused: false,
            width: 32,
            show_hidden: true,
            clipboard: None,
            last_height: 1,
            last_area: None,
            dirty: Arc::new(AtomicBool::new(false)),
            pending_center: false,
            search_prompt: Prompt::new(
                Cow::Borrowed(""),
                None,
                |_editor, _| Vec::new(),
                |_cx, _input, _event| {},
            ),
            search_focused: false,
            search_matcher: new_search_matcher(Some(1)),
            search_matches: HashSet::new(),
            search_boundary_dirs: HashSet::new(),
            search_collapsed: HashSet::new(),
            search_expanded: HashSet::new(),
            search_restore_path: None,
            search_results_dirty: false,
            search_initialized: false,
        }
    }

    fn add_rows(tree: &mut FileTree, count: usize) {
        tree.rows.extend((0..count).map(|index| Row {
            path: tree.root.join(index.to_string()),
            name: index.to_string(),
            depth: 0,
            is_dir: false,
            expansion_root: tree.root.clone(),
        }));
    }

    #[test]
    fn moves_cursor_to_visible_rows() {
        let mut tree = empty_tree(PathBuf::from("/workspace"));
        add_rows(&mut tree, 20);
        tree.scroll = 5;
        tree.cursor = 7;
        tree.last_height = 6;

        tree.move_cursor_to_visible_row(VisiblePosition::Top);
        assert_eq!(tree.cursor, 5);
        tree.move_cursor_to_visible_row(VisiblePosition::Middle);
        assert_eq!(tree.cursor, 7);
        tree.move_cursor_to_visible_row(VisiblePosition::Bottom);
        assert_eq!(tree.cursor, 10);
        assert_eq!(tree.scroll, 5);
    }

    #[test]
    fn visible_row_motions_stop_at_end_of_tree() {
        let mut tree = empty_tree(PathBuf::from("/workspace"));
        add_rows(&mut tree, 8);
        tree.scroll = 5;
        tree.cursor = 6;
        tree.last_height = 10;

        tree.move_cursor_to_visible_row(VisiblePosition::Top);
        assert_eq!(tree.cursor, 5);
        tree.move_cursor_to_visible_row(VisiblePosition::Middle);
        assert_eq!(tree.cursor, 6);
        tree.move_cursor_to_visible_row(VisiblePosition::Bottom);
        assert_eq!(tree.cursor, 7);
        assert_eq!(tree.scroll, 5);
    }

    #[test]
    fn centers_cursor_and_clamps_at_tree_edges() {
        let mut tree = empty_tree(PathBuf::from("/workspace"));
        add_rows(&mut tree, 20);
        tree.last_height = 5;

        tree.cursor = 10;
        tree.center_cursor();
        assert_eq!(tree.scroll, 8);

        tree.cursor = 1;
        tree.center_cursor();
        assert_eq!(tree.scroll, 0);

        tree.cursor = 19;
        tree.center_cursor();
        assert_eq!(tree.scroll, 15);
    }

    #[test]
    fn nucleo_search_matches_workspace_relative_paths() {
        let mut matcher = new_search_matcher(Some(1));
        let injector = matcher.injector();
        for path in ["src/main.rs", "tests/helper.rs"] {
            injector.push(PathBuf::from(path), |_path, columns| {
                columns[0] = path.into();
            });
        }
        drop(injector);
        matcher
            .pattern
            .reparse(0, "smain", CaseMatching::Smart, Normalization::Smart, false);

        for _ in 0..100 {
            if !matcher.tick(10).running {
                break;
            }
        }

        assert_eq!(
            matcher
                .snapshot()
                .matched_items(..)
                .map(|item| item.data.as_path())
                .collect::<Vec<_>>(),
            [Path::new("src/main.rs")]
        );
    }

    #[test]
    fn nucleo_match_indices_identify_a_directory_boundary() {
        let root = PathBuf::from("/workspace");
        let mut matcher = new_search_matcher(Some(1));
        let injector = matcher.injector();
        let path = root.join("foo/bar/a");
        injector.push(path.clone(), |_, columns| {
            columns[0] = "foo/bar/a".into();
        });
        drop(injector);
        matcher
            .pattern
            .reparse(0, "bar", CaseMatching::Smart, Normalization::Smart, false);

        for _ in 0..100 {
            if !matcher.tick(10).running {
                break;
            }
        }

        let snapshot = matcher.snapshot();
        let item = snapshot.matched_items(..).next().unwrap();
        let mut indices = Vec::new();
        snapshot.pattern().column_pattern(0).indices(
            item.matcher_columns[0].slice(..),
            &mut Matcher::new(Config::DEFAULT.match_paths()),
            &mut indices,
        );
        assert_eq!(
            search_match_boundary(&root, &path, &indices),
            root.join("foo/bar")
        );
    }

    #[test]
    fn matched_files_are_rendered_with_their_ancestors() {
        let root = PathBuf::from("/workspace");
        let matches = ["src/main.rs", "tests/main_spec.rs"]
            .into_iter()
            .map(|path| {
                let path = root.join(path);
                SearchMatch {
                    boundary: path.clone(),
                    path,
                }
            })
            .collect::<Vec<_>>();

        let (rows, direct_matches, _) =
            build_search_rows(&root, &matches, &HashSet::new(), &HashSet::new(), false);
        assert_eq!(
            rows.iter()
                .map(|row| row.path.strip_prefix(&root).unwrap())
                .collect::<Vec<_>>(),
            [
                Path::new("src"),
                Path::new("src/main.rs"),
                Path::new("tests"),
                Path::new("tests/main_spec.rs"),
            ]
        );
        assert!(!direct_matches.contains(&root.join("src")));
        assert!(direct_matches.contains(&root.join("src/main.rs")));
    }

    #[test]
    fn filename_matches_do_not_mark_ancestor_directories() {
        let root = PathBuf::from("/workspace");
        let path = root.join("tests/fixture.rs");
        let matches = vec![SearchMatch {
            boundary: path.clone(),
            path,
        }];

        let (rows, direct_matches, _) =
            build_search_rows(&root, &matches, &HashSet::new(), &HashSet::new(), false);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, root.join("tests"));
        assert!(!direct_matches.contains(&root.join("tests")));
        assert!(direct_matches.contains(&root.join("tests/fixture.rs")));
    }

    #[test]
    fn filtered_collapses_hide_only_the_selected_branch() {
        let root = PathBuf::from("/workspace");
        let matches = ["src/main.rs", "tests/main.rs"]
            .into_iter()
            .map(|path| {
                let path = root.join(path);
                SearchMatch {
                    boundary: path.clone(),
                    path,
                }
            })
            .collect::<Vec<_>>();
        let collapsed = HashSet::from([root.join("src")]);

        let (rows, _, _) = build_search_rows(&root, &matches, &collapsed, &HashSet::new(), false);
        assert_eq!(
            rows.iter()
                .map(|row| row.path.strip_prefix(&root).unwrap())
                .collect::<Vec<_>>(),
            [
                Path::new("src"),
                Path::new("tests"),
                Path::new("tests/main.rs"),
            ]
        );
    }

    #[test]
    fn directory_matches_stop_at_the_deepest_matched_component() {
        let root = PathBuf::from("/workspace");
        let matches = ["foo/bar/a", "foo/bar/b"]
            .into_iter()
            .map(|path| SearchMatch {
                path: root.join(path),
                boundary: root.join("foo/bar"),
            })
            .collect::<Vec<_>>();

        let (rows, direct_matches, boundary_dirs) =
            build_search_rows(&root, &matches, &HashSet::new(), &HashSet::new(), false);

        assert_eq!(
            rows.iter()
                .map(|row| row.path.strip_prefix(&root).unwrap())
                .collect::<Vec<_>>(),
            [Path::new("foo"), Path::new("foo/bar")]
        );
        assert_eq!(direct_matches, HashSet::from([root.join("foo/bar")]));
        assert_eq!(boundary_dirs, HashSet::from([root.join("foo/bar")]));
    }

    #[test]
    fn matched_directory_can_be_expanded_to_reveal_descendants() {
        let root = PathBuf::from("/workspace");
        let matches = ["foo/bar/a", "foo/bar/b"]
            .into_iter()
            .map(|path| SearchMatch {
                path: root.join(path),
                boundary: root.join("foo/bar"),
            })
            .collect::<Vec<_>>();
        let expanded = HashSet::from([root.join("foo/bar")]);

        let (rows, _, _) = build_search_rows(&root, &matches, &HashSet::new(), &expanded, false);

        assert_eq!(
            rows.iter()
                .map(|row| row.path.strip_prefix(&root).unwrap())
                .collect::<Vec<_>>(),
            [
                Path::new("foo"),
                Path::new("foo/bar"),
                Path::new("foo/bar/a"),
                Path::new("foo/bar/b"),
            ]
        );
    }

    #[test]
    fn filtered_single_directory_chains_are_flattened() {
        let root = PathBuf::from("/workspace");
        let path = root.join("foo/bar/baz/file.rs");
        let matches = vec![SearchMatch {
            boundary: path.clone(),
            path: path.clone(),
        }];

        let (rows, _, _) =
            build_search_rows(&root, &matches, &HashSet::new(), &HashSet::new(), true);

        assert_eq!(
            rows.iter()
                .map(|row| {
                    (
                        row.name.clone(),
                        row.path.clone(),
                        row.expansion_root.clone(),
                    )
                })
                .collect::<Vec<_>>(),
            [
                (
                    "foo/bar/baz".to_string(),
                    root.join("foo/bar/baz"),
                    root.join("foo"),
                ),
                ("file.rs".to_string(), path.clone(), path),
            ]
        );
    }

    #[test]
    fn flattened_matched_directory_remains_a_collapsible_boundary() {
        let root = PathBuf::from("/workspace");
        let file = root.join("foo/bar/file.rs");
        let matches = vec![SearchMatch {
            path: file,
            boundary: root.join("foo/bar"),
        }];

        let (collapsed_rows, _, _) =
            build_search_rows(&root, &matches, &HashSet::new(), &HashSet::new(), true);
        assert_eq!(collapsed_rows.len(), 1);
        assert_eq!(collapsed_rows[0].name, "foo/bar");
        assert_eq!(collapsed_rows[0].path, root.join("foo/bar"));

        let expanded = HashSet::from([root.join("foo/bar")]);
        let (expanded_rows, _, _) =
            build_search_rows(&root, &matches, &HashSet::new(), &expanded, true);
        assert_eq!(
            expanded_rows
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["foo/bar", "file.rs"]
        );
    }

    #[test]
    fn selected_search_result_scrolls_from_its_top_level_ancestor() {
        let root = PathBuf::from("/workspace");
        let rows = [
            ("foo", 0),
            ("bar", 1),
            ("result.rs", 2),
            ("other", 0),
            ("result.rs", 1),
        ]
        .into_iter()
        .map(|(name, depth)| Row {
            path: root.join(name),
            name: name.into(),
            depth,
            is_dir: depth < 2,
            expansion_root: root.clone(),
        })
        .collect::<Vec<_>>();

        assert_eq!(first_visible_ancestor_index(&rows, 2), 0);
        assert_eq!(first_visible_ancestor_index(&rows, 4), 3);
    }

    #[test]
    fn restoring_selection_after_collapse_preserves_scroll() {
        let mut tree = empty_tree(PathBuf::from("/workspace"));
        add_rows(&mut tree, 10);
        for row in &mut tree.rows[1..] {
            row.depth = 1;
        }
        tree.last_height = 5;
        tree.scroll = 3;
        tree.cursor = 4;
        let selected = tree.rows[tree.cursor].path.clone();

        tree.restore_selected_path(Some(&selected), false);

        assert_eq!(tree.cursor, 4);
        assert_eq!(tree.scroll, 3);
    }

    #[test]
    fn match_indices_select_their_containing_path_component() {
        let root = Path::new("/workspace");
        let file = root.join("foo/bar/a");

        assert_eq!(
            search_match_boundary(root, &file, &[4, 5, 6]),
            root.join("foo/bar")
        );
        assert_eq!(search_match_boundary(root, &file, &[8]), file);
        assert_eq!(search_match_boundary(root, &file, &[]), file);
    }

    #[test]
    fn marked_paths_drop_descendants() {
        let root = PathBuf::from("/workspace");
        let mut paths = vec![root.join("a/b"), root.join("a"), root.join("c")];
        paths.sort();
        let mut normalized = Vec::new();
        for path in paths {
            if !normalized
                .iter()
                .any(|parent: &PathBuf| path.starts_with(parent))
            {
                normalized.push(path);
            }
        }
        assert_eq!(normalized, vec![root.join("a"), root.join("c")]);
    }

    #[test]
    fn metadata_is_aggregated_to_ancestors() {
        let root = PathBuf::from("/workspace");
        let diagnostics =
            aggregate_diagnostics(HashMap::from([(root.join("src/lib.rs"), (2, 1))]), &root);
        assert_eq!(diagnostics[&root.join("src")], (2, 1));
        assert_eq!(diagnostics[&root], (2, 1));
    }

    #[test]
    fn collapsing_evicts_only_the_selected_subtree() {
        let root = PathBuf::from("/workspace");
        let open = root.join("open");
        let nested = open.join("nested");
        let sibling = root.join("sibling");
        let mut tree = empty_tree(root.clone());
        tree.manual_expanded
            .extend([root.clone(), open.clone(), nested.clone(), sibling.clone()]);
        for directory in [&root, &open, &nested, &sibling] {
            tree.directory_entries
                .insert(directory.to_path_buf(), Vec::new());
        }

        tree.remove_expansion_subtree(&open);

        assert_eq!(
            tree.manual_expanded,
            HashSet::from([root.clone(), sibling.clone()])
        );
        assert_eq!(
            tree.watched_directories()
                .into_iter()
                .collect::<HashSet<_>>(),
            HashSet::from([root, sibling])
        );
    }

    #[test]
    fn provisional_expansions_are_effective_and_watched() {
        let root = PathBuf::from("/workspace");
        let src = root.join("src");
        let nested = src.join("nested");
        let mut tree = empty_tree(root.clone());
        tree.manual_expanded.insert(root.clone());
        tree.provisional_expanded
            .extend([root.clone(), src.clone(), nested.clone()]);
        for directory in [&root, &src, &nested] {
            tree.directory_entries
                .insert(directory.to_path_buf(), Vec::new());
        }

        assert!(tree.is_expanded(&src));
        assert!(tree.is_expanded(&nested));
        assert_eq!(
            tree.watched_directories()
                .into_iter()
                .collect::<HashSet<_>>(),
            HashSet::from([root, src, nested])
        );
    }

    #[test]
    fn collapsing_provisional_directory_only_clears_manual_ownership() {
        let root = PathBuf::from("/workspace");
        let src = root.join("src");
        let mut tree = empty_tree(root.clone());
        tree.manual_expanded.extend([root.clone(), src.clone()]);
        tree.provisional_expanded
            .extend([root.clone(), src.clone()]);
        tree.directory_entries.insert(root, Vec::new());
        tree.directory_entries.insert(src.clone(), Vec::new());

        tree.collapse_manual_subtree(&src);

        assert!(!tree.manual_expanded.contains(&src));
        assert!(tree.is_expanded(&src));
        assert!(tree.directory_entries.contains_key(&src));
    }

    #[test]
    fn provisional_cache_is_removed_after_the_last_dependency_closes() {
        let root = PathBuf::from("/workspace");
        let manual = root.join("manual");
        let provisional = root.join("provisional");
        let mut tree = empty_tree(root.clone());
        tree.manual_expanded.extend([root.clone(), manual.clone()]);
        tree.provisional_expanded
            .extend([root.clone(), provisional.clone()]);
        for directory in [&root, &manual, &provisional] {
            tree.directory_entries
                .insert(directory.to_path_buf(), Vec::new());
        }

        tree.provisional_expanded.clear();
        tree.prune_directory_entries();

        assert_eq!(
            tree.watched_directories()
                .into_iter()
                .collect::<HashSet<_>>(),
            HashSet::from([root, manual])
        );
        assert!(!tree.is_expanded(&provisional));
    }

    #[test]
    fn hidden_tree_exposes_no_directory_watches() {
        let root = PathBuf::from("/workspace");
        let mut tree = empty_tree(root.clone());
        tree.directory_entries.insert(root, Vec::new());
        tree.visible = false;

        assert!(tree.watched_directories().is_empty());
    }

    #[test]
    fn hidden_files_are_hidden_by_default() {
        assert!(helix_view::editor::FileTreeConfig::default().hidden);
    }

    #[test]
    fn recursive_copy_preserves_contents() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), "hello").unwrap();

        copy_path(&source, &target).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("nested/file.txt")).unwrap(),
            "hello"
        );
    }

    #[cfg(unix)]
    #[test]
    fn recursive_copy_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file.txt"), "hello").unwrap();
        symlink("file.txt", source.join("link.txt")).unwrap();

        copy_path(&source, &target).unwrap();
        assert_eq!(
            fs::read_link(target.join("link.txt")).unwrap(),
            Path::new("file.txt")
        );
    }
}
