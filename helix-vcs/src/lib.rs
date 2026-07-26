//! `helix_vcs` provides types for working with diffs from a Version Control System (VCS).
//! Jujutsu is queried through its command-line template interface. This avoids coupling the
//! editor to a particular `jj-lib` build or storage backend.

use anyhow::{anyhow, bail, Result};
use arc_swap::ArcSwap;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

mod jj;

mod diff;

pub use diff::{DiffHandle, Hunk};

mod status;

pub use status::FileChange;

/// Contains all active diff providers.
#[derive(Clone)]
pub struct DiffProviderRegistry {
    providers: Vec<DiffProvider>,
}

impl DiffProviderRegistry {
    /// Explicitly refresh jj's persisted working-copy state. Read queries never snapshot.
    pub fn snapshot_working_copy(self, cwd: PathBuf) {
        tokio::task::spawn_blocking(move || {
            if let Err(err) = jj::snapshot(&cwd) {
                log::debug!(
                    "failed to snapshot jj working copy in {}: {err:#}",
                    cwd.display()
                );
            }
        });
    }

    /// Get the given file from the VCS. This provides the unedited document as a "base"
    /// for a diff to be created.
    pub fn get_diff_base(&self, file: &Path) -> Option<Vec<u8>> {
        self.providers
            .iter()
            .find_map(|provider| match provider.get_diff_base(file) {
                Ok(res) => Some(res),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to open diff base for {}", file.display());
                    None
                }
            })
    }

    /// Get a display label for the current working-copy change.
    pub fn get_current_head_name(&self, file: &Path) -> Option<Arc<ArcSwap<Box<str>>>> {
        self.providers
            .iter()
            .find_map(|provider| match provider.get_current_head_name(file) {
                Ok(res) => Some(res),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to obtain current head name for {}", file.display());
                    None
                }
            })
    }

    /// Fire-and-forget changed file iteration. Runs everything in a background task. Keeps
    /// iteration until `on_change` returns `false`.
    pub fn for_each_changed_file(
        self,
        cwd: PathBuf,
        f: impl Fn(Result<FileChange>) -> bool + Send + 'static,
    ) {
        tokio::task::spawn_blocking(move || {
            // The picker is an explicit freshness boundary. Keep the subsequent status query
            // operation-pinned and snapshot-free.
            if let Err(err) = jj::snapshot(&cwd) {
                f(Err(err));
                return;
            }
            if self
                .providers
                .iter()
                .find_map(|provider| provider.for_each_changed_file(&cwd, &[], &f).ok())
                .is_none()
            {
                f(Err(anyhow!("no diff provider returns success")));
            }
        });
    }

    /// Iterate changed files under the supplied paths. An empty path list queries the workspace.
    pub fn for_each_changed_file_in(
        self,
        cwd: PathBuf,
        paths: Vec<PathBuf>,
        f: impl Fn(Result<FileChange>) -> bool + Send + 'static,
        on_complete: impl FnOnce(Result<()>) + Send + 'static,
    ) {
        tokio::task::spawn_blocking(move || {
            let result = self
                .providers
                .iter()
                .find_map(|provider| provider.for_each_changed_file(&cwd, &paths, &f).ok())
                .ok_or_else(|| anyhow!("no diff provider returns success"));
            on_complete(result);
        });
    }
}

impl Default for DiffProviderRegistry {
    fn default() -> Self {
        let providers = vec![DiffProvider::Jj, DiffProvider::None];
        DiffProviderRegistry { providers }
    }
}

/// A union type that includes all types that implement [DiffProvider]. We need this type to allow
/// cloning [DiffProviderRegistry] as `Clone` cannot be used in trait objects.
///
/// `Copy` is simply to ensure the `clone()` call is the simplest it can be.
#[derive(Copy, Clone)]
enum DiffProvider {
    Jj,
    None,
}

impl DiffProvider {
    fn get_diff_base(&self, file: &Path) -> Result<Vec<u8>> {
        match self {
            Self::Jj => jj::get_diff_base(file),
            Self::None => {
                let _ = file;
                bail!("No diff support compiled in")
            }
        }
    }

    fn get_current_head_name(&self, file: &Path) -> Result<Arc<ArcSwap<Box<str>>>> {
        match self {
            Self::Jj => jj::get_current_head_name(file),
            Self::None => {
                let _ = file;
                bail!("No diff support compiled in")
            }
        }
    }

    fn for_each_changed_file(
        &self,
        cwd: &Path,
        paths: &[PathBuf],
        f: impl Fn(Result<FileChange>) -> bool,
    ) -> Result<()> {
        match self {
            Self::Jj => jj::for_each_changed_file(cwd, paths, f),
            Self::None => {
                let _ = (cwd, paths, f);
                bail!("No diff support compiled in")
            }
        }
    }
}
