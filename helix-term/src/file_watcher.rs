use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use helix_view::Editor;

pub struct FileEventBatch {
    pub paths: HashSet<PathBuf>,
    pub rescan: bool,
}

pub struct FileWatcher {
    watcher: RecommendedWatcher,
    batches: mpsc::UnboundedReceiver<FileEventBatch>,
    watched: HashMap<PathBuf, RecursiveMode>,
}

impl FileWatcher {
    pub fn new(
        editor: &Editor,
        tree_directories: impl IntoIterator<Item = PathBuf>,
    ) -> notify::Result<Self> {
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<notify::Result<Event>>();
        let (batch_tx, batches) = mpsc::unbounded_channel();
        let timeout = Duration::from_millis(editor.config().file_watcher.debounce_timeout);
        let watcher = notify::recommended_watcher(move |event| {
            let _ = raw_tx.send(event);
        })?;

        tokio::spawn(async move {
            while let Some(first) = raw_rx.recv().await {
                let mut paths = HashSet::new();
                let mut rescan = false;
                collect_event(first, &mut paths, &mut rescan);
                while let Ok(Some(event)) = tokio::time::timeout(timeout, raw_rx.recv()).await {
                    collect_event(event, &mut paths, &mut rescan);
                }
                if rescan || !paths.is_empty() {
                    let _ = batch_tx.send(FileEventBatch { paths, rescan });
                }
            }
        });

        let mut this = Self {
            watcher,
            batches,
            watched: HashMap::new(),
        };
        this.reconcile(editor, tree_directories)?;
        Ok(this)
    }

    pub async fn recv(&mut self) -> Option<FileEventBatch> {
        self.batches.recv().await
    }

    pub fn reconcile(
        &mut self,
        editor: &Editor,
        tree_directories: impl IntoIterator<Item = PathBuf>,
    ) -> notify::Result<()> {
        let open_files = editor
            .documents()
            .filter_map(|document| document.path().map(PathBuf::from));
        let desired = desired_watches(open_files, tree_directories);

        let stale: Vec<_> = self
            .watched
            .keys()
            .filter(|path| !desired.contains_key(*path))
            .cloned()
            .collect();
        for path in stale {
            let _ = self.watcher.unwatch(&path);
            self.watched.remove(&path);
        }
        for (path, mode) in desired {
            if self.watched.get(&path) == Some(&mode) {
                continue;
            }
            self.watcher.watch(&path, mode)?;
            self.watched.insert(path, mode);
        }
        Ok(())
    }
}

fn desired_watches(
    open_files: impl IntoIterator<Item = PathBuf>,
    tree_directories: impl IntoIterator<Item = PathBuf>,
) -> HashMap<PathBuf, RecursiveMode> {
    open_files
        .into_iter()
        // A new file cannot be watched directly. Watch its parent instead so that a
        // concurrent create is delivered as an event for the target path.
        .filter_map(|path| {
            if path.exists() {
                Some(path)
            } else {
                path.parent().map(PathBuf::from)
            }
        })
        .chain(tree_directories)
        .map(|path| (path, RecursiveMode::NonRecursive))
        .collect()
}

fn collect_event(event: notify::Result<Event>, paths: &mut HashSet<PathBuf>, rescan: &mut bool) {
    match event {
        Ok(event) => {
            if event.kind.is_access() {
                return;
            }
            *rescan |= event.need_rescan();
            paths.extend(event.paths);
        }
        Err(error) => {
            log::warn!("filesystem watcher error: {error}");
            *rescan = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::EventKind;

    #[test]
    fn batches_paths_and_rescan_requests() {
        let mut paths = HashSet::new();
        let mut rescan = false;
        let event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(PathBuf::from("/tmp/a"))
            .set_flag(notify::event::Flag::Rescan);
        collect_event(Ok(event), &mut paths, &mut rescan);
        assert!(paths.contains(&PathBuf::from("/tmp/a")));
        assert!(rescan);
    }

    #[test]
    fn watcher_errors_request_a_rescan() {
        let mut paths = HashSet::new();
        let mut rescan = false;
        collect_event(
            Err(notify::Error::generic("synthetic watcher error")),
            &mut paths,
            &mut rescan,
        );
        assert!(rescan);
    }

    #[test]
    fn desired_watches_include_new_file_parents_and_loaded_directories() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().to_path_buf();
        let open = root.join("open.rs");
        std::fs::write(&open, "").unwrap();
        let new_file = root.join("new.rs");
        let another_new_file = root.join("another-new.rs");
        let expanded = root.join("expanded");
        let desired = desired_watches(
            [open.clone(), new_file, another_new_file],
            [root.clone(), expanded.clone()],
        );

        assert_eq!(desired.len(), 3);
        assert_eq!(desired[&open], RecursiveMode::NonRecursive);
        assert_eq!(desired[&root], RecursiveMode::NonRecursive);
        assert_eq!(desired[&expanded], RecursiveMode::NonRecursive);
        assert!(!desired.contains_key(&root.join("collapsed")));
        assert!(desired
            .values()
            .all(|mode| *mode == RecursiveMode::NonRecursive));
    }

    #[test]
    fn desired_watches_can_be_empty() {
        let desired = desired_watches(Vec::new(), Vec::new());
        assert!(desired.is_empty());
    }
}
