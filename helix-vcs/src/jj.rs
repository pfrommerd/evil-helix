//! Jujutsu integration through the public command-line interface.
//!
//! Read commands are pinned to the latest operation and explicitly ignore the working copy. This
//! keeps querying the editor UI from creating surprise snapshots. Call [`snapshot`] at deliberate
//! refresh points instead.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use serde::Deserialize;

use crate::FileChange;

const DIFF_TEMPLATE: &str = r#""{\"path\":" ++ stringify(self.path()).escape_json() ++ ",\"status\":" ++ self.status().escape_json() ++ ",\"source_path\":" ++ stringify(self.source().path()).escape_json() ++ ",\"source_type\":" ++ self.source().file_type().escape_json() ++ ",\"target_type\":" ++ self.target().file_type().escape_json() ++ "}\n""#;

fn command(repository: &Path) -> Command {
    let mut command = Command::new("jj");
    command
        .arg("--repository")
        .arg(repository)
        .current_dir(repository)
        .arg("--at-op=@")
        .arg("--ignore-working-copy")
        .arg("--quiet")
        .arg("--no-pager")
        .arg("--color=never");
    command
}

fn workspace_root(start: &Path) -> Result<PathBuf> {
    let output = Command::new("jj")
        .current_dir(start)
        .arg("--at-op=@")
        .arg("--ignore-working-copy")
        .arg("--quiet")
        .arg("--no-pager")
        .arg("--color=never")
        .args(["workspace", "root"])
        .output()
        .context("failed to invoke jj workspace root")?;
    if !output.status.success() {
        bail!(
            "jj workspace root failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let root = String::from_utf8(output.stdout)
        .context("jj workspace root returned non-UTF-8 path")?
        .trim()
        .to_owned();
    if root.is_empty() {
        bail!("jj workspace root returned an empty path");
    }
    Ok(PathBuf::from(root))
}

fn run_read(repository: &Path, args: &[&OsStr]) -> Result<Vec<u8>> {
    let output = command(repository)
        .args(args)
        .output()
        .context("failed to invoke jj")?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        bail!(
            "jj command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

/// Explicitly synchronize the working copy. Unlike all read queries, this intentionally may
/// create a jj operation when files changed.
pub fn snapshot(repository: &Path) -> Result<()> {
    let repository = workspace_root(repository)?;
    let output = Command::new("jj")
        .arg("--repository")
        .arg(&repository)
        .current_dir(&repository)
        .arg("--quiet")
        .arg("--no-pager")
        .arg("--color=never")
        .args(["util", "snapshot"])
        .output()
        .context("failed to invoke jj util snapshot")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "jj util snapshot failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

fn canonical_file(file: &Path) -> Result<PathBuf> {
    let file = file.canonicalize().context("resolve symlinks")?;
    if !file.is_file() {
        bail!("{} is not a regular file", file.display());
    }
    Ok(file)
}

pub fn get_diff_base(file: &Path) -> Result<Vec<u8>> {
    let file = canonical_file(file)?;
    let directory = file.parent().context("file has no parent directory")?;
    let repository = workspace_root(directory)?;
    let path = file
        .strip_prefix(&repository)
        .context("file is outside jj workspace")?;
    run_read(
        &repository,
        &[
            OsStr::new("file"),
            OsStr::new("show"),
            OsStr::new("--revision"),
            OsStr::new("@-"),
            OsStr::new("--"),
            path.as_os_str(),
        ],
    )
}

#[derive(Deserialize)]
struct WorkingCopy {
    change_id: String,
}

pub fn get_current_head_name(file: &Path) -> Result<Arc<ArcSwap<Box<str>>>> {
    let file = canonical_file(file)?;
    let repository = workspace_root(file.parent().context("file has no parent directory")?)?;
    let output = run_read(
        &repository,
        &[
            OsStr::new("log"),
            OsStr::new("--no-graph"),
            OsStr::new("--revision"),
            OsStr::new("@"),
            OsStr::new("--template"),
            OsStr::new("json(self)"),
        ],
    )?;
    let working_copy: WorkingCopy =
        serde_json::from_slice(&output).context("parse jj log template")?;
    let name = working_copy.change_id.chars().take(8).collect::<String>();
    Ok(Arc::new(ArcSwap::from_pointee(name.into_boxed_str())))
}

#[derive(Deserialize)]
struct ChangedPath {
    path: String,
    status: String,
    source_path: String,
    source_type: String,
    target_type: String,
}

pub fn for_each_changed_file(cwd: &Path, f: impl Fn(Result<FileChange>) -> bool) -> Result<()> {
    let repository = workspace_root(cwd)?;
    let output = run_read(
        &repository,
        &[
            OsStr::new("diff"),
            OsStr::new("--revision"),
            OsStr::new("@"),
            OsStr::new("--template"),
            OsStr::new(DIFF_TEMPLATE),
        ],
    )?;
    for line in output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let change: ChangedPath =
            match serde_json::from_slice(line).context("parse jj diff template") {
                Ok(change) => change,
                Err(err) => {
                    if !f(Err(err)) {
                        break;
                    }
                    continue;
                }
            };
        let path = repository.join(&change.path);
        let result = match change.status.as_str() {
            "added" => FileChange::Untracked { path },
            "modified" => {
                if change.target_type == "conflict" {
                    FileChange::Conflict { path }
                } else {
                    FileChange::Modified { path }
                }
            }
            "removed" => FileChange::Deleted { path },
            "copied" | "renamed" => FileChange::Renamed {
                from_path: repository.join(change.source_path),
                to_path: path,
            },
            status => {
                if !f(Err(anyhow::anyhow!("unsupported jj diff status {status}"))) {
                    break;
                }
                continue;
            }
        };
        // Skip directory/submodule changes, which cannot provide a document diff base.
        if change.target_type == "tree"
            || (change.target_type == "git-submodule" && change.source_type != "")
        {
            continue;
        }
        if !f(Ok(result)) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use tempfile::TempDir;

    use super::*;

    fn run_jj(repository: &Path, args: &[&str]) {
        let output = Command::new("jj")
            .arg("--repository")
            .arg(repository)
            .current_dir(repository)
            .args(args)
            .output()
            .expect("jj must be installed to run jj integration tests");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository() -> TempDir {
        let repository = tempfile::tempdir().unwrap();
        let output = Command::new("jj")
            .args(["git", "init", repository.path().to_str().unwrap()])
            .output()
            .expect("jj must be installed to run jj integration tests");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        repository
    }

    #[test]
    fn reads_parent_base_and_structured_changed_paths() {
        let repository = repository();
        let file = repository.path().join("tracked.txt");
        fs::write(&file, "base\n").unwrap();
        run_jj(repository.path(), &["file", "track", "tracked.txt"]);
        run_jj(repository.path(), &["describe", "--message", "base"]);
        run_jj(repository.path(), &["new"]);

        fs::write(&file, "changed\n").unwrap();
        fs::write(repository.path().join("added.txt"), "added\n").unwrap();
        snapshot(repository.path()).unwrap();

        assert_eq!(get_diff_base(&file).unwrap(), b"base\n");

        let changes = std::sync::Mutex::new(Vec::new());
        for_each_changed_file(repository.path(), |change| {
            changes.lock().unwrap().push(change.unwrap());
            true
        })
        .unwrap();
        let changes = changes.into_inner().unwrap();
        assert!(changes
            .iter()
            .any(|change| matches!(change, FileChange::Modified { path } if path == &file)));
        assert!(changes.iter().any(|change| matches!(change, FileChange::Untracked { path } if path == &repository.path().join("added.txt"))));
    }
}
