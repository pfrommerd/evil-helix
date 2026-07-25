use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::AtomicBool,
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use helix_vcs::FileChange;
use helix_view::{
    editor::Action,
    graphics::{Rect, Style},
    input::{Event, MouseButton, MouseEventKind},
    Editor,
};
use parking_lot::RwLock;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitState {
    Untracked,
    Modified,
    Renamed,
    Deleted,
    Conflict,
}

#[derive(Debug, Clone, Copy)]
pub enum FileTreeAction {
    CursorUp,
    CursorDown,
    PageUp,
    PageDown,
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
    git_status: Arc<RwLock<HashMap<PathBuf, GitState>>>,
    git_generation: Arc<AtomicU64>,
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
            git_status: Arc::new(RwLock::new(HashMap::new())),
            git_generation: Arc::new(AtomicU64::new(0)),
            last_area: None,
            dirty: Arc::new(AtomicBool::new(false)),
        };
        tree.refresh(editor);
        tree
    }

    pub fn toggle(&mut self, editor: &mut Editor) {
        self.visible = !self.visible;
        if !self.visible {
            self.focused = false;
            self.git_generation.fetch_add(1, Ordering::Relaxed);
            self.git_status.write().clear();
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
            self.marked.clear();
            self.clipboard = None;
            self.cursor = 0;
            self.scroll = 0;
        }

        let selected = self.rows.get(self.cursor).map(|row| row.path.clone());
        let mut rows = Vec::new();
        self.collect_rows(&self.root, 0, editor, &mut rows);
        self.rows = rows;
        self.marked.retain(|path| path.exists());
        if let Some(selected) = selected {
            if let Some(index) = self.rows.iter().position(|row| row.path == selected) {
                self.cursor = index;
            }
        }
        self.clamp_cursor();
        self.refresh_git(editor);
    }

    pub fn collapse_all(&mut self, editor: &Editor) {
        self.expanded.clear();
        self.expanded.insert(self.root.clone());
        self.refresh(editor);
    }

    fn collect_rows(&self, dir: &Path, depth: usize, editor: &Editor, out: &mut Vec<Row>) {
        let config = &editor.config().file_tree;
        let mut builder = ignore::WalkBuilder::new(dir);
        let mut entries: Vec<_> = builder
            .max_depth(Some(1))
            .hidden(!self.show_hidden)
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

        for entry in entries {
            let mut path = entry.path().to_path_buf();
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            let is_dir = file_type.is_dir()
                || (config.follow_symlinks && file_type.is_symlink() && path.is_dir());
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if is_dir && config.flatten_dirs {
                while let Some(child) = single_visible_directory(&path, self.show_hidden) {
                    name.push('/');
                    name.push_str(&child.file_name().unwrap_or_default().to_string_lossy());
                    path = child;
                }
            }
            out.push(Row {
                path: path.clone(),
                name,
                depth,
                is_dir,
            });
            if is_dir && self.expanded.contains(&path) {
                self.collect_rows(&path, depth + 1, editor, out);
            }
        }
    }

    fn refresh_git(&mut self, editor: &Editor) {
        if !self.visible || !editor.config().file_tree.git_status {
            self.git_status.write().clear();
            return;
        }
        let root = self.root.clone();
        let trust_full = editor
            .workspace_trust
            .query(&root, helix_loader::workspace_trust::TrustQuery::Git)
            .is_trusted();
        let statuses = Arc::clone(&self.git_status);
        let generations = Arc::clone(&self.git_generation);
        let generation = generations.fetch_add(1, Ordering::Relaxed) + 1;
        statuses.write().clear();
        editor
            .diff_providers
            .clone()
            .for_each_changed_file(root, trust_full, move |change| {
                if generations.load(Ordering::Relaxed) != generation {
                    return false;
                }
                if let Ok(change) = change {
                    let (path, state) = match change {
                        FileChange::Untracked { path } => (path, GitState::Untracked),
                        FileChange::Modified { path } => (path, GitState::Modified),
                        FileChange::Conflict { path } => (path, GitState::Conflict),
                        FileChange::Deleted { path } => (path, GitState::Deleted),
                        FileChange::Renamed { to_path, .. } => (to_path, GitState::Renamed),
                    };
                    statuses.write().insert(path, state);
                    helix_event::request_redraw();
                }
                true
            });
    }

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
        let mut parent = path.parent();
        while let Some(dir) = parent {
            if !dir.starts_with(&self.root) {
                break;
            }
            self.expanded.insert(dir.to_path_buf());
            parent = dir.parent();
        }
        self.refresh(editor);
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
        let Some(row) = self.rows.get(self.cursor) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        if !self.expanded.remove(&row.path) {
            self.expanded.insert(row.path.clone());
        }
        self.refresh(editor);
    }

    fn collapse_or_parent(&mut self, editor: &mut Editor) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if row.is_dir && self.expanded.remove(&row.path) {
            self.refresh(editor);
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
            self.expanded.insert(row.path);
            self.refresh(editor);
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
        let git_status = aggregate_git_status(&self.git_status.read(), &self.root);
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
            let badge = metadata_badge(
                git_status.get(&row.path).copied(),
                diagnostics.get(&row.path).copied(),
            );
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
                        match git_status.get(&row.path) {
                            Some(GitState::Untracked) => editor.theme.get("diff.plus"),
                            Some(GitState::Deleted) => editor.theme.get("diff.minus"),
                            Some(GitState::Conflict) => editor.theme.get("error"),
                            _ => editor.theme.get("diff.delta"),
                        }
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

fn single_visible_directory(path: &Path, show_hidden: bool) -> Option<PathBuf> {
    let mut entries = fs::read_dir(path)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| show_hidden || !entry.file_name().to_string_lossy().starts_with('.'));
    let entry = entries.next()?;
    if entries.next().is_some() || !entry.file_type().ok()?.is_dir() {
        return None;
    }
    Some(entry.path())
}

fn aggregate_git_status(
    direct: &HashMap<PathBuf, GitState>,
    root: &Path,
) -> HashMap<PathBuf, GitState> {
    let mut result = direct.clone();
    for (path, state) in direct {
        let mut parent = path.parent();
        while let Some(path) = parent {
            if !path.starts_with(root) {
                break;
            }
            result
                .entry(path.to_path_buf())
                .and_modify(|current| {
                    if git_priority(*state) > git_priority(*current) {
                        *current = *state;
                    }
                })
                .or_insert(*state);
            parent = path.parent();
        }
    }
    result
}

fn git_priority(state: GitState) -> u8 {
    match state {
        GitState::Untracked => 0,
        GitState::Modified | GitState::Renamed => 1,
        GitState::Deleted => 2,
        GitState::Conflict => 3,
    }
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

fn metadata_badge(git: Option<GitState>, diagnostics: Option<(usize, usize)>) -> String {
    let mut badge = String::new();
    if let Some(state) = git {
        badge.push(' ');
        badge.push(match state {
            GitState::Untracked => '?',
            GitState::Modified => 'M',
            GitState::Renamed => 'R',
            GitState::Deleted => 'D',
            GitState::Conflict => '!',
        });
    }
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
        let direct = HashMap::from([
            (root.join("src/lib.rs"), GitState::Modified),
            (root.join("src/main.rs"), GitState::Conflict),
        ]);
        let aggregated = aggregate_git_status(&direct, &root);
        assert_eq!(aggregated[&root.join("src")], GitState::Conflict);

        let diagnostics =
            aggregate_diagnostics(HashMap::from([(root.join("src/lib.rs"), (2, 1))]), &root);
        assert_eq!(diagnostics[&root.join("src")], (2, 1));
        assert_eq!(diagnostics[&root], (2, 1));
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
