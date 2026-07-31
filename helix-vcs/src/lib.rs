//! `helix_vcs` provides types for working with diffs from a Version Control System (VCS).
//! Jujutsu is queried through its command-line template interface. This avoids coupling the
//! editor to a particular `jj-lib` build or storage backend.

use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

mod jj;

mod diff;

pub use diff::{DiffHandle, Hunk};

mod status;

pub use status::FileChange;

/// Version-control metadata resolved for one file at one consistent repository operation.
#[derive(Debug, Default)]
pub struct FileInfo {
    /// Parent content used as the document diff base, when a single parent exists.
    pub diff_base: Option<Vec<u8>>,
    /// Short, stable label for the working-copy change.
    pub head_name: Option<Box<str>>,
}

/// Contains all active diff providers.
#[derive(Clone)]
pub struct DiffProviderRegistry {
    providers: Vec<DiffProvider>,
}

impl DiffProviderRegistry {
    /// Resolve all VCS metadata for a file in one operation-consistent lookup.
    ///
    /// This is synchronous and intended to be called from `spawn_blocking`.
    pub fn get_file_info(&self, file: &Path) -> FileInfo {
        self.providers
            .iter()
            .find_map(|provider| match provider.get_file_info(file) {
                Ok(res) => Some(res),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to obtain VCS metadata for {}", file.display());
                    None
                }
            })
            .unwrap_or_default()
    }

    /// Fire-and-forget changed file iteration. Runs everything in a background task. Keeps
    /// iteration until `on_change` returns `false`.
    pub fn for_each_changed_file(
        self,
        cwd: PathBuf,
        f: impl Fn(Result<FileChange>) -> bool + Send + 'static,
    ) {
        tokio::task::spawn_blocking(move || {
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
    fn get_file_info(&self, file: &Path) -> Result<FileInfo> {
        match self {
            Self::Jj => jj::get_file_info(file),
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
