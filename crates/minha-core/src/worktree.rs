//! Safe, argument-vector based Git worktree and recovery operations.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
};

#[derive(Debug)]
pub struct GitError {
    pub operation: String,
    pub code: Option<i32>,
    pub stderr: String,
}
impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "git {} failed: {}", self.operation, self.stderr.trim())
    }
}
impl std::error::Error for GitError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub bare: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryPoint {
    pub head: String,
    pub patch: String,
    pub staged_patch: String,
    pub untracked: Vec<PathBuf>,
    /// Contents are retained because Git's diff commands omit new files.
    pub untracked_contents: Vec<(PathBuf, Vec<u8>)>,
}

#[derive(Clone, Debug)]
pub struct GitRepo {
    root: PathBuf,
}

impl GitRepo {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn run(&self, args: &[&str]) -> Result<Output, GitError> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .map_err(|e| GitError {
                operation: args.join(" "),
                code: None,
                stderr: e.to_string(),
            })?;
        if !output.status.success() {
            return Err(GitError {
                operation: args.join(" "),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(output)
    }
    fn text(&self, args: &[&str]) -> Result<String, GitError> {
        Ok(String::from_utf8_lossy(&self.run(args)?.stdout)
            .trim_end()
            .to_owned())
    }
    pub fn head(&self) -> Result<String, GitError> {
        self.text(&["rev-parse", "HEAD"])
    }
    pub fn status_porcelain(&self) -> Result<String, GitError> {
        self.text(&["status", "--porcelain=v1", "-uall"])
    }
    pub fn is_inside_work_tree(&self) -> bool {
        self.text(&["rev-parse", "--is-inside-work-tree"])
            .is_ok_and(|value| value == "true")
    }
    pub fn diff(&self) -> Result<String, GitError> {
        self.text(&["diff", "--binary", "--no-ext-diff"])
    }
    pub fn list_worktrees(&self) -> Result<Vec<Worktree>, GitError> {
        let raw = self.text(&["worktree", "list", "--porcelain"])?;
        let mut result = Vec::new();
        let mut path = None;
        let mut head = None;
        let mut branch = None;
        let mut bare = false;
        let flush = |result: &mut Vec<Worktree>,
                     path: &mut Option<PathBuf>,
                     head: &mut Option<String>,
                     branch: &mut Option<String>,
                     bare: &mut bool| {
            if let (Some(path), Some(head)) = (path.take(), head.take()) {
                result.push(Worktree {
                    path,
                    head,
                    branch: branch.take(),
                    bare: *bare,
                });
            }
            *bare = false;
        };
        for line in raw.lines() {
            if line.is_empty() {
                flush(&mut result, &mut path, &mut head, &mut branch, &mut bare);
            } else if let Some(v) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(v));
            } else if let Some(v) = line.strip_prefix("HEAD ") {
                head = Some(v.to_owned());
            } else if let Some(v) = line.strip_prefix("branch refs/heads/") {
                branch = Some(v.to_owned());
            } else if line == "bare" {
                bare = true;
            }
        }
        flush(&mut result, &mut path, &mut head, &mut branch, &mut bare);
        Ok(result)
    }
    pub fn add_worktree(
        &self,
        path: impl AsRef<Path>,
        branch: &str,
        start: Option<&str>,
    ) -> Result<(), GitError> {
        let path = path.as_ref().to_string_lossy().into_owned();
        let mut args = vec!["worktree", "add", "-b", branch, &path];
        if let Some(start) = start {
            args.push(start);
        }
        self.run(&args).map(|_| ())
    }
    /// Include untracked files in a binary patch by marking them intent-to-add
    /// in this linked worktree's private index.
    pub fn patch_with_untracked(&self) -> Result<String, GitError> {
        self.run(&["add", "-N", "--all"])?;
        self.text(&["diff", "--binary", "--no-ext-diff"])
    }
    pub fn remove_worktree(&self, path: impl AsRef<Path>, force: bool) -> Result<(), GitError> {
        let path = path.as_ref().to_string_lossy().into_owned();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&path);
        self.run(&args).map(|_| ())
    }
    pub fn capture_recovery(&self) -> Result<RecoveryPoint, GitError> {
        let untracked: Vec<PathBuf> = self
            .status_porcelain()?
            .lines()
            .filter_map(|l| l.strip_prefix("?? "))
            .map(PathBuf::from)
            .collect();
        let untracked_contents = untracked
            .iter()
            .map(|path| {
                std::fs::read(self.root.join(path))
                    .map(|bytes| (path.clone(), bytes))
                    .map_err(GitError::from)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RecoveryPoint {
            head: self.head()?,
            patch: self.text(&["diff", "--binary"])?,
            staged_patch: self.text(&["diff", "--cached", "--binary"])?,
            untracked,
            untracked_contents,
        })
    }
    pub fn rewind(&self, target: &str) -> Result<(), GitError> {
        self.run(&["reset", "--hard", target]).map(|_| ())
    }
    pub fn merge(&self, branch: &str, ff_only: bool) -> Result<(), GitError> {
        let args = if ff_only {
            vec!["merge", "--ff-only", branch]
        } else {
            vec!["merge", branch]
        };
        self.run(&args).map(|_| ())
    }
    pub fn recover(&self, point: &RecoveryPoint) -> Result<(), GitError> {
        self.rewind(&point.head)?;
        if !point.staged_patch.is_empty() {
            self.apply_patch(&point.staged_patch, true)?;
        }
        if !point.patch.is_empty() {
            self.apply_patch(&point.patch, false)?;
        }
        for (path, contents) in &point.untracked_contents {
            let destination = self.root.join(path);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(GitError::from)?;
            }
            std::fs::write(destination, contents).map_err(GitError::from)?;
        }
        Ok(())
    }
    fn apply_patch(&self, patch: &str, cached: bool) -> Result<(), GitError> {
        let mut child = Command::new("git")
            .args(if cached {
                vec!["apply", "--cached", "-"]
            } else {
                vec!["apply", "-"]
            })
            .current_dir(&self.root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| GitError {
                operation: "apply".into(),
                code: None,
                stderr: e.to_string(),
            })?;
        use std::io::Write;
        let mut stdin = child.stdin.take().ok_or_else(|| GitError {
            operation: "apply".into(),
            code: None,
            stderr: "git apply did not provide piped stdin".into(),
        })?;
        stdin.write_all(patch.as_bytes()).map_err(|e| GitError {
            operation: "apply".into(),
            code: None,
            stderr: e.to_string(),
        })?;
        drop(stdin);
        let output = child.wait_with_output().map_err(|e| GitError {
            operation: "apply".into(),
            code: None,
            stderr: e.to_string(),
        })?;
        if !output.status.success() {
            return Err(GitError {
                operation: "apply".into(),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(())
    }
}

/// Copy a coding workspace into an isolated lane without carrying repository
/// metadata, build products, or Minha's own recovery directories with it.
pub fn copy_workspace(source: &Path, destination: &Path) -> io::Result<()> {
    if destination.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("snapshot lane already exists: {}", destination.display()),
        ));
    }
    fs::create_dir_all(destination)?;
    copy_workspace_dir(source, destination)
}

fn copy_workspace_dir(source: &Path, destination: &Path) -> io::Result<()> {
    for item in fs::read_dir(source)? {
        let item = item?;
        let name = item.file_name();
        if matches!(
            name.to_str(),
            Some(".git" | ".minha" | "target" | "node_modules" | ".cache")
        ) {
            continue;
        }
        let from = item.path();
        let to = destination.join(&name);
        let kind = item.file_type()?;
        if kind.is_dir() {
            fs::create_dir(&to)?;
            copy_workspace_dir(&from, &to)?;
        } else if kind.is_symlink() {
            copy_symlink(&from, &to)?;
        } else if kind.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, destination: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, destination)
}

#[cfg(windows)]
fn copy_symlink(source: &Path, destination: &Path) -> io::Result<()> {
    let target = fs::read_link(source)?;
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    }
}

/// Produce a binary-capable Git patch containing only changes between two
/// sibling snapshots. Prefixes are normalized so it applies at workspace root.
pub fn diff_snapshots(baseline: &Path, changed: &Path) -> Result<String, GitError> {
    let parent = baseline
        .parent()
        .filter(|parent| changed.parent() == Some(*parent))
        .ok_or_else(|| GitError {
            operation: "diff snapshots".into(),
            code: None,
            stderr: "snapshot directories must be siblings".into(),
        })?;
    let baseline_name = baseline
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| GitError {
            operation: "diff snapshots".into(),
            code: None,
            stderr: "baseline has no UTF-8 directory name".into(),
        })?;
    let changed_name = changed
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| GitError {
            operation: "diff snapshots".into(),
            code: None,
            stderr: "changed snapshot has no UTF-8 directory name".into(),
        })?;
    let output = Command::new("git")
        .args([
            "diff",
            "--no-index",
            "--binary",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--",
            baseline_name,
            changed_name,
        ])
        .current_dir(parent)
        .output()
        .map_err(|error| GitError {
            operation: "diff --no-index".into(),
            code: None,
            stderr: error.to_string(),
        })?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(GitError {
            operation: "diff --no-index".into(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let patch = String::from_utf8_lossy(&output.stdout);
    let mut normalized = patch
        .lines()
        .map(|line| normalize_snapshot_patch_line(line, baseline_name, changed_name))
        .collect::<Vec<_>>()
        .join("\n");
    if !normalized.is_empty() {
        normalized.push('\n');
    }
    Ok(normalized)
}

fn normalize_snapshot_patch_line(line: &str, baseline: &str, changed: &str) -> String {
    if line.starts_with("diff --git ") || line.starts_with("--- ") || line.starts_with("+++ ") {
        line.replace(&format!("a/{baseline}/"), "a/")
            .replace(&format!("b/{changed}/"), "b/")
    } else {
        line.to_owned()
    }
}

impl From<io::Error> for GitError {
    fn from(e: io::Error) -> Self {
        Self {
            operation: "git".into(),
            code: None,
            stderr: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    fn repo() -> (tempfile::TempDir, GitRepo) {
        let d = tempdir().expect("test operation should succeed");
        let r = GitRepo::new(d.path());
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(d.path())
            .status()
            .expect("test operation should succeed");
        Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(d.path())
            .output()
            .expect("test operation should succeed");
        (d, r)
    }
    #[test]
    fn recovery_round_trip_restores_untracked_file() {
        let (_d, r) = repo();
        fs::write(r.root().join("x"), "one").expect("test operation should succeed");
        let p = r.capture_recovery().expect("test operation should succeed");
        r.rewind("HEAD").expect("test operation should succeed");
        fs::remove_file(r.root().join("x")).expect("test operation should succeed");
        r.recover(&p).expect("test operation should succeed");
        assert_eq!(r.head().expect("test operation should succeed"), p.head);
        assert_eq!(
            fs::read_to_string(r.root().join("x")).expect("test operation should succeed"),
            "one"
        );
    }

    #[test]
    fn snapshot_diff_is_root_relative_and_skips_runtime_artifacts() {
        let directory = tempdir().expect("test operation should succeed");
        let source = directory.path().join("source");
        let baseline = directory.path().join("baseline");
        let changed = directory.path().join("changed");
        fs::create_dir_all(source.join("src")).expect("test operation should succeed");
        fs::create_dir_all(source.join("target")).expect("test operation should succeed");
        fs::write(source.join("src/lib.rs"), "old\n").expect("test operation should succeed");
        fs::write(source.join("target/ignored"), "large\n").expect("test operation should succeed");
        copy_workspace(&source, &baseline).expect("test operation should succeed");
        copy_workspace(&source, &changed).expect("test operation should succeed");
        fs::write(changed.join("src/lib.rs"), "new\n").expect("test operation should succeed");

        let patch = diff_snapshots(&baseline, &changed).expect("test operation should succeed");
        assert!(patch.contains("--- a/src/lib.rs"));
        assert!(patch.contains("+++ b/src/lib.rs"));
        assert!(patch.ends_with('\n'));
        assert!(!baseline.join("target").exists());

        let applied = directory.path().join("applied");
        copy_workspace(&baseline, &applied).expect("copy patch destination");
        GitRepo::new(&applied)
            .apply_patch(&patch, false)
            .expect("generated snapshot patch applies");
        assert_eq!(
            fs::read_to_string(applied.join("src/lib.rs")).expect("patched source"),
            "new\n"
        );
    }
}
