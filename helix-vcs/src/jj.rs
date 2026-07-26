//! Jujutsu integration through the public command-line interface.
//!
//! Read commands are pinned to the latest operation and explicitly ignore the working copy. This
//! keeps querying the editor UI from creating surprise snapshots. Call [`snapshot`] at deliberate
//! refresh points instead.

use std::{
    ffi::OsStr,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};

use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use serde::Deserialize;

use crate::FileChange;

// `jj status` has no template interface. `jj diff --template` exposes changed-path metadata
// through structured `TreeDiffEntry` records without rendering file hunks.
const STATUS_TEMPLATE: &str = r#""{\"path\":" ++ stringify(self.path()).escape_json() ++ ",\"status\":" ++ self.status().escape_json() ++ ",\"source_path\":" ++ stringify(self.source().path()).escape_json() ++ ",\"source_type\":" ++ self.source().file_type().escape_json() ++ ",\"target_type\":" ++ self.target().file_type().escape_json() ++ "}\n""#;

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
    let current = run_read(
        &repository,
        &[
            OsStr::new("file"),
            OsStr::new("show"),
            OsStr::new("--revision"),
            OsStr::new("@"),
            OsStr::new("--"),
            path.as_os_str(),
        ],
    )?;
    let patch = run_read(
        &repository,
        &[
            OsStr::new("diff"),
            OsStr::new("--revision"),
            OsStr::new("@"),
            OsStr::new("--git"),
            OsStr::new("--context=0"),
            OsStr::new("--"),
            path.as_os_str(),
        ],
    )?;
    reverse_patch(&current, &patch)
}

#[derive(Debug)]
struct PatchHunk {
    old_count: usize,
    new_start: usize,
    new_count: usize,
    lines: Vec<Vec<u8>>,
}

/// Reconstruct the parent-side file content by applying a zero-context Git patch backwards.
///
/// `jj diff -r @` lets jj compare the current working-copy commit with the automatic merge of
/// all of its parents, which is important for merge commits. We retain Helix's internal differ
/// by applying that patch to the persisted `@` content rather than to the editor buffer (which
/// may contain unsaved edits).
fn reverse_patch(current: &[u8], patch: &[u8]) -> Result<Vec<u8>> {
    if patch
        .split_inclusive(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"Binary files ") || line.starts_with(b"GIT binary patch"))
    {
        bail!("jj returned a binary patch");
    }

    let hunks = parse_patch_hunks(patch)?;
    let current_lines = split_lines(current);
    let mut base = Vec::with_capacity(current.len());
    let mut current_index = 0;

    for hunk in hunks {
        // Unified diff uses `+0,0` for deletion-only hunks at the start of a file.
        let hunk_start = hunk.new_start.saturating_sub(1);
        if hunk_start < current_index || hunk_start > current_lines.len() {
            bail!("jj patch hunks are out of order or outside the current file");
        }
        for line in &current_lines[current_index..hunk_start] {
            base.extend_from_slice(line);
        }
        current_index = hunk_start;

        let mut old_count = 0;
        let mut new_count = 0;
        for line in hunk.lines {
            let (kind, content) = line.split_first().context("empty line in jj patch hunk")?;
            match kind {
                b'-' => {
                    base.extend_from_slice(content);
                    old_count += 1;
                }
                b'+' => {
                    let target = current_lines
                        .get(current_index)
                        .context("jj patch adds past the end of the current file")?;
                    if *target != content {
                        bail!("jj patch does not match the current file content");
                    }
                    current_index += 1;
                    new_count += 1;
                }
                b' ' => {
                    let target = current_lines
                        .get(current_index)
                        .context("jj patch context is past the end of the current file")?;
                    if *target != content {
                        bail!("jj patch context does not match the current file content");
                    }
                    base.extend_from_slice(content);
                    current_index += 1;
                    old_count += 1;
                    new_count += 1;
                }
                _ => bail!("unsupported line in jj patch hunk"),
            }
        }
        if old_count != hunk.old_count || new_count != hunk.new_count {
            bail!("jj patch hunk line counts do not match its header");
        }
    }
    for line in &current_lines[current_index..] {
        base.extend_from_slice(line);
    }
    Ok(base)
}

fn split_lines(contents: &[u8]) -> Vec<&[u8]> {
    contents
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect()
}

fn parse_patch_hunks(patch: &[u8]) -> Result<Vec<PatchHunk>> {
    let lines = patch
        .split_inclusive(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    let mut hunks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if !line.starts_with(b"@@ ") {
            index += 1;
            continue;
        }
        let (old_count, new_start, new_count) = parse_hunk_header(line)?;
        index += 1;
        let mut hunk_lines: Vec<Vec<u8>> = Vec::new();
        while let Some(line) = lines.get(index) {
            if line.starts_with(b"@@ ") || line.starts_with(b"diff --git ") {
                break;
            }
            if line.starts_with(b"\\ No newline at end of file") {
                let previous = hunk_lines
                    .last_mut()
                    .context("jj newline marker has no preceding hunk line")?;
                if previous.pop() != Some(b'\n') {
                    bail!("jj newline marker does not follow a newline-terminated hunk line");
                }
                index += 1;
                continue;
            }
            if matches!(line.first(), Some(b'+' | b'-' | b' ')) {
                hunk_lines.push((*line).to_vec());
                index += 1;
                continue;
            }
            bail!("unexpected line in jj patch hunk");
        }
        hunks.push(PatchHunk {
            old_count,
            new_start,
            new_count,
            lines: hunk_lines,
        });
    }
    Ok(hunks)
}

fn parse_hunk_header(header: &[u8]) -> Result<(usize, usize, usize)> {
    let header = std::str::from_utf8(header).context("jj patch header is not UTF-8")?;
    let mut fields = header.split_whitespace();
    if fields.next() != Some("@@") {
        bail!("invalid jj patch hunk header");
    }
    let old = fields
        .next()
        .context("missing old range in jj patch hunk")?;
    let new = fields
        .next()
        .context("missing new range in jj patch hunk")?;
    if fields.next() != Some("@@") {
        bail!("invalid jj patch hunk header terminator");
    }
    let (_, old_count) = parse_hunk_range(old, '-')?;
    let (new_start, new_count) = parse_hunk_range(new, '+')?;
    Ok((old_count, new_start, new_count))
}

fn parse_hunk_range(range: &str, prefix: char) -> Result<(usize, usize)> {
    let range = range
        .strip_prefix(prefix)
        .context("invalid jj patch hunk range prefix")?;
    let (start, count) = match range.split_once(',') {
        Some((start, count)) => (start, count),
        None => (range, "1"),
    };
    Ok((
        start.parse().context("invalid jj patch hunk line number")?,
        count.parse().context("invalid jj patch hunk line count")?,
    ))
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
    let mut child = command(&repository)
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

    #[test]
    fn reverse_patch_reconstructs_multihunk_base() {
        let current = b"one\nchanged\nthree\nnew\n";
        let patch = b"diff --git a/file b/file\n\
index 4cb29ea38f..92e218fd46 100644\n\
--- a/file\n\
+++ b/file\n\
@@ -2,1 +2,1 @@\n\
-two\n\
+changed\n\
@@ -3,0 +4,1 @@\n\
+new\n";
        assert_eq!(reverse_patch(current, patch).unwrap(), b"one\ntwo\nthree\n");
    }

    #[test]
    fn reverse_patch_handles_added_files_and_crlf() {
        let added_patch = b"diff --git a/file b/file\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/file\n\
@@ -0,0 +1,2 @@\n\
+brand\n\
+new\n";
        assert_eq!(reverse_patch(b"brand\nnew\n", added_patch).unwrap(), b"");

        let crlf_patch = b"@@ -2,1 +2,1 @@\n-two\r\n+changed\r\n";
        assert_eq!(
            reverse_patch(b"one\r\nchanged\r\n", crlf_patch).unwrap(),
            b"one\r\ntwo\r\n"
        );

        let no_newline_patch = b"@@ -1,1 +1,1 @@\n-old\n\\ No newline at end of file\n+new\n";
        assert_eq!(reverse_patch(b"new\n", no_newline_patch).unwrap(), b"old");
    }

    #[test]
    fn reverse_patch_rejects_invalid_or_binary_patches() {
        assert!(reverse_patch(b"new\n", b"@@ -1,1 +1,1 @@\n-old\n+other\n").is_err());
        assert!(reverse_patch(b"", b"Binary files a/file and b/file differ\n").is_err());
    }
}
