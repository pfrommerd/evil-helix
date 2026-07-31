//! Jujutsu integration through the public command-line interface.
//!
//! Read commands are pinned to an operation and explicitly ignore the working copy. This keeps
//! querying the editor UI from creating surprise snapshots.

use std::{
    ffi::OsStr,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::{FileChange, FileInfo};

// `jj status` has no template interface. `jj diff --template` exposes changed-path metadata
// through structured `TreeDiffEntry` records without rendering file hunks.
const STATUS_TEMPLATE: &str = r#""{\"path\":" ++ stringify(self.path()).escape_json() ++ ",\"status\":" ++ self.status().escape_json() ++ ",\"source_path\":" ++ stringify(self.source().path()).escape_json() ++ ",\"source_type\":" ++ self.source().file_type().escape_json() ++ ",\"target_type\":" ++ self.target().file_type().escape_json() ++ "}\n""#;

fn command(repository: &Path, operation: &OsStr) -> Command {
    let mut command = Command::new("jj");
    command
        .arg("--repository")
        .arg(repository)
        .current_dir(repository)
        .arg("--at-op")
        .arg(operation)
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

fn run_read(repository: &Path, operation: &OsStr, args: &[&OsStr]) -> Result<Vec<u8>> {
    let output = command(repository, operation)
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

fn operation_id(repository: &Path) -> Result<Vec<u8>> {
    let output = command(repository, OsStr::new("@"))
        .args([
            "op",
            "log",
            "--no-graph",
            "--limit",
            "1",
            "--template",
            "self.id()",
        ])
        .output()
        .context("failed to resolve jj operation")?;
    if !output.status.success() {
        bail!(
            "jj op log failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let id = output.stdout;
    if id.is_empty() {
        bail!("jj op log returned an empty operation ID");
    }
    Ok(id)
}

fn canonical_file(file: &Path) -> Result<PathBuf> {
    let file = file.canonicalize().context("resolve symlinks")?;
    if !file.is_file() {
        bail!("{} is not a regular file", file.display());
    }
    Ok(file)
}

pub fn get_file_info(file: &Path) -> Result<FileInfo> {
    let file = canonical_file(file)?;
    let directory = file.parent().context("file has no parent directory")?;
    let repository = workspace_root(directory)?;
    let path = file
        .strip_prefix(&repository)
        .context("file is outside jj workspace")?;
    let operation = operation_id(&repository)?;
    let output = run_read(
        &repository,
        OsStr::new(std::str::from_utf8(&operation).context("jj operation ID is not UTF-8")?),
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
    let head_name = Some(
        working_copy
            .change_id
            .chars()
            .take(8)
            .collect::<String>()
            .into_boxed_str(),
    );
    let diff_base = if working_copy.parents.len() != 1 {
        None
    } else {
        let operation = OsStr::new(std::str::from_utf8(&operation).unwrap());
        let parent = OsStr::new(&working_copy.parents[0]);
        match parent_file(&repository, operation, parent, path) {
            Ok(base) => Some(base),
            Err(err) => {
                log::debug!("failed to read jj parent file {}: {err:#}", path.display());
                None
            }
        }
    };
    Ok(FileInfo {
        diff_base,
        head_name,
    })
}

fn parent_file(
    repository: &Path,
    operation: &OsStr,
    parent: &OsStr,
    path: &Path,
) -> Result<Vec<u8>> {
    let listed = run_read(
        repository,
        operation,
        &[
            OsStr::new("file"),
            OsStr::new("list"),
            OsStr::new("--revision"),
            parent,
            OsStr::new("--"),
            path.as_os_str(),
        ],
    )?;
    if listed.is_empty() {
        Ok(Vec::new())
    } else {
        run_read(
            repository,
            operation,
            &[
                OsStr::new("file"),
                OsStr::new("show"),
                OsStr::new("--revision"),
                parent,
                OsStr::new("--"),
                path.as_os_str(),
            ],
        )
    }
}

#[derive(Deserialize)]
struct WorkingCopy {
    change_id: String,
    parents: Vec<String>,
}

#[derive(Deserialize)]
struct ChangedPath {
    path: String,
    status: String,
    source_path: String,
    source_type: String,
    target_type: String,
}

pub fn for_each_changed_file(
    cwd: &Path,
    paths: &[PathBuf],
    f: impl Fn(Result<FileChange>) -> bool,
) -> Result<()> {
    let repository = workspace_root(cwd)?;
    let mut args = vec![
        std::ffi::OsString::from("diff"),
        std::ffi::OsString::from("--revision"),
        std::ffi::OsString::from("@"),
        std::ffi::OsString::from("--template"),
        std::ffi::OsString::from(STATUS_TEMPLATE),
    ];
    if !paths.is_empty() {
        args.push(std::ffi::OsString::from("--"));
        for path in paths {
            if let Ok(path) = path.strip_prefix(&repository) {
                args.push(path.as_os_str().to_owned());
            }
        }
    }
    let mut child = command(&repository, OsStr::new("@"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to invoke jj")?;
    let stdout = child.stdout.take().context("jj did not provide stdout")?;
    for line in BufReader::new(stdout).split(b'\n') {
        let line = line.context("read jj diff template")?;
        if line.is_empty() {
            continue;
        }
        let change: ChangedPath =
            match serde_json::from_slice(&line).context("parse jj diff template") {
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
    let output = child.wait_with_output().context("wait for jj diff")?;
    if !output.status.success() {
        bail!(
            "jj command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
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
    fn reads_persisted_parent_base_and_change_id_without_snapshotting() {
        let repository = repository();
        let file = repository.path().join("tracked.txt");
        fs::write(&file, "base\n").unwrap();
        run_jj(repository.path(), &["file", "track", "tracked.txt"]);
        run_jj(repository.path(), &["describe", "--message", "base"]);
        run_jj(repository.path(), &["new"]);

        fs::write(&file, "changed\n").unwrap();
        let before = operation_id(repository.path()).unwrap();
        let info = get_file_info(&file).unwrap();
        let after = operation_id(repository.path()).unwrap();

        assert_eq!(info.diff_base.unwrap(), b"base\n");
        assert_eq!(info.head_name.as_deref().unwrap().len(), 8);
        assert_eq!(before, after, "VCS reads must not create a jj operation");
    }

    #[test]
    fn missing_parent_file_has_empty_base() {
        let repository = repository();
        run_jj(repository.path(), &["new"]);
        let file = repository.path().join("added.txt");
        fs::write(&file, "added\n").unwrap();

        let info = get_file_info(&file).unwrap();
        assert_eq!(info.diff_base, Some(Vec::new()));
        assert!(info.head_name.is_some());
    }

    #[test]
    fn merge_has_change_id_but_no_diff_base() {
        let repository = repository();
        let file = repository.path().join("tracked.txt");
        fs::write(&file, "base\n").unwrap();
        run_jj(repository.path(), &["file", "track", "tracked.txt"]);
        run_jj(repository.path(), &["describe", "--message", "left"]);
        run_jj(
            repository.path(),
            &["bookmark", "create", "left", "--revision", "@"],
        );
        run_jj(repository.path(), &["new", "root()"]);
        run_jj(repository.path(), &["describe", "--message", "right"]);
        run_jj(
            repository.path(),
            &["bookmark", "create", "right", "--revision", "@"],
        );
        run_jj(repository.path(), &["new", "left", "right"]);

        let info = get_file_info(&file).unwrap();
        assert!(info.diff_base.is_none());
        assert_eq!(info.head_name.as_deref().unwrap().len(), 8);
    }

    #[test]
    fn reads_structured_changed_paths() {
        let repository = repository();
        let file = repository.path().join("tracked.txt");
        fs::write(&file, "base\n").unwrap();
        run_jj(repository.path(), &["file", "track", "tracked.txt"]);
        run_jj(repository.path(), &["describe", "--message", "base"]);
        run_jj(repository.path(), &["new"]);

        fs::write(&file, "changed\n").unwrap();
        fs::write(repository.path().join("added.txt"), "added\n").unwrap();
        run_jj(repository.path(), &["util", "snapshot"]);

        let changes = std::sync::Mutex::new(Vec::new());
        for_each_changed_file(repository.path(), &[], |change| {
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
