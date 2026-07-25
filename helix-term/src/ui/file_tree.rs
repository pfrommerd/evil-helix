use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, atomic::Ordering, Arc},
};

use helix_view::{
    editor::Action,
    graphics::{Rect, Style},
    input::{Event, MouseButton, MouseEventKind},
    Editor,
};
use tui::buffer::Buffer as Surface;

use crate::{
    compositor::{Callback, Context, EventResult},
    ui::{Prompt, PromptEvent},
};

const MIN_EDITOR_WIDTH: u16 = 20;

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
}

/// Persistent, keyboard-focused file tree rendered beside the editor.
pub struct FileTree {
    root: PathBuf,
    rows: Vec<Row>,
    directory_entries: HashMap<PathBuf, Vec<Row>>,
    expanded: HashSet<PathBuf>,
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
}

impl FileTree {
    pub fn new(editor: &Editor) -> Self {
        let config = &editor.config().file_tree;
        let (min_width, max_width) = ordered_width_bounds(config.min_width, config.max_width);
        let root = helix_stdx::env::current_working_dir();
        let mut expanded = HashSet::new();
        expanded.insert(root.clone());
        let mut tree = Self {
            root,
            rows: Vec::new(),
            directory_entries: HashMap::new(),
            expanded,
            marked: HashSet::new(),
            cursor: 0,
            scroll: 0,
            visible: config.visible,
            focused: false,
            width: config.width.clamp(min_width, max_width),
            show_hidden: !config.hidden,
            clipboard: None,
            last_height: 1,
            last_area: None,
            dirty: Arc::new(AtomicBool::new(false)),
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
        if self.focused && editor.config().file_tree.auto_reveal {
            self.reveal_current(editor);
        }
    }

    pub fn focus_editor(&mut self) {
        self.focused = false;
    }

    pub fn focused(&self) -> bool {
        self.focused
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
            self.expanded.clear();
            self.expanded.insert(self.root.clone());
            self.directory_entries.clear();
            self.marked.clear();
            self.clipboard = None;
            self.cursor = 0;
            self.scroll = 0;
        }

        if !self.visible {
            self.rows.clear();
            self.directory_entries.clear();
            return;
        }

        let selected = self.rows.get(self.cursor).map(|row| row.path.clone());
        let mut directories: Vec<_> = self.expanded.iter().cloned().collect();
        directories.sort_by_key(|path| path.components().count());
        for directory in directories {
            if directory.is_dir() {
                self.load_directory(&directory, editor);
            } else {
                self.remove_expanded_subtree(&directory);
            }
        }
        self.rebuild_rows(editor);
        self.marked.retain(|path| path.exists());
        if let Some(selected) = selected {
            if let Some(index) = self.rows.iter().position(|row| row.path == selected) {
                self.cursor = index;
            }
        }
        self.clamp_cursor();
    }

    pub fn collapse_all(&mut self, editor: &Editor) {
        self.expanded.clear();
        self.expanded.insert(self.root.clone());
        self.directory_entries.retain(|path, _| path == &self.root);
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
                self.remove_expanded_subtree(&directory);
            }
        }
        self.rebuild_rows(editor);
        self.clamp_cursor();
    }

    fn load_directory(&mut self, dir: &Path, editor: &Editor) {
        self.directory_entries.insert(
            dir.to_path_buf(),
            read_directory(dir, self.show_hidden, editor),
        );
    }

    fn rebuild_rows(&mut self, editor: &Editor) {
        let mut rows = Vec::new();
        self.collect_rows(&self.root, 0, editor, &mut rows);
        self.rows = rows;
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
                while self.expanded.contains(&row.path) {
                    let Some(children) = self.directory_entries.get(&row.path) else {
                        break;
                    };
                    let [child] = children.as_slice() else {
                        break;
                    };
                    if !child.is_dir || !self.expanded.contains(&child.path) {
                        break;
                    }
                    row.name.push('/');
                    row.name.push_str(&child.name);
                    row.path = child.path.clone();
                }
            }
            out.push(row.clone());
            if row.is_dir && self.expanded.contains(&row.path) {
                self.collect_rows(&row.path, depth + 1, editor, out);
            }
        }
    }

    fn remove_expanded_subtree(&mut self, root: &Path) {
        self.expanded.retain(|path| !path.starts_with(root));
        self.directory_entries
            .retain(|path, _| !path.starts_with(root));
    }

    fn expand_directory(&mut self, path: PathBuf, editor: &Editor) {
        let flatten = editor.config().file_tree.flatten_dirs;
        let mut directory = path;
        loop {
            if !self.expanded.insert(directory.clone()) {
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

impl FileTree {
    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + self.last_height {
            self.scroll = self.cursor + 1 - self.last_height;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
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
        let mut ancestors = Vec::new();
        let mut parent = path.parent();
        while let Some(dir) = parent {
            if !dir.starts_with(&self.root) {
                break;
            }
            ancestors.push(dir.to_path_buf());
            parent = dir.parent();
        }
        ancestors.reverse();
        for directory in ancestors {
            self.expanded.insert(directory.clone());
            self.load_directory(&directory, editor);
        }
        self.rebuild_rows(editor);
        if let Some(index) = self.rows.iter().position(|row| row.path == path) {
            self.cursor = index;
            self.clamp_cursor();
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

    fn toggle_expand(&mut self, editor: &Editor) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if !row.is_dir {
            return;
        }
        if self.expanded.contains(&row.expansion_root) {
            self.remove_expanded_subtree(&row.expansion_root);
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
        if row.is_dir && self.expanded.contains(&row.expansion_root) {
            self.remove_expanded_subtree(&row.expansion_root);
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
        if row.is_dir {
            if !self.expanded.contains(&row.expansion_root) {
                self.expand_directory(row.path, editor);
            }
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
                    let index = self.scroll + usize::from(mouse.row - area.y);
                    if index < self.rows.len() {
                        let same = self.focused && self.cursor == index;
                        let disclosure_column = area.x.saturating_add(
                            3 + u16::try_from(self.rows[index].depth.saturating_mul(2))
                                .unwrap_or(u16::MAX),
                        );
                        self.focused = true;
                        self.cursor = index;
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
                self.cursor = 0;
                self.clamp_cursor();
            }
            FileTreeAction::CursorLast => {
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
        }
        None
    }

    pub fn render(&mut self, area: Rect, surface: &mut Surface, editor: &mut Editor) {
        self.last_area = Some(area);
        if self.dirty.swap(false, Ordering::Relaxed)
            || helix_stdx::env::current_working_dir() != self.root
        {
            self.refresh(editor);
        }
        self.last_height = area.height.max(1) as usize;
        self.clamp_cursor();

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
            let y = area.y + screen_row as u16;
            let marker = if self.marked.contains(&row.path) {
                "●"
            } else {
                " "
            };
            let disclosure = if row.is_dir {
                if self.expanded.contains(&row.path) {
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
                style = style.patch(editor.theme.get("ui.text.focus"));
            }
            if index == self.cursor && self.focused {
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
            expanded: HashSet::new(),
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
        tree.expanded
            .extend([root.clone(), open.clone(), nested.clone(), sibling.clone()]);
        for directory in [&root, &open, &nested, &sibling] {
            tree.directory_entries
                .insert(directory.to_path_buf(), Vec::new());
        }

        tree.remove_expanded_subtree(&open);

        assert_eq!(
            tree.expanded,
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
    fn hidden_tree_exposes_no_directory_watches() {
        let root = PathBuf::from("/workspace");
        let mut tree = empty_tree(root.clone());
        tree.directory_entries.insert(root, Vec::new());
        tree.visible = false;

        assert!(tree.watched_directories().is_empty());
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
