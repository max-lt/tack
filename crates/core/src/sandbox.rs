//! Command execution capability used by the Bash tool.
//!
//! A sandbox runs the command on a copy of the workspace and returns the
//! files the command changed. The caller decides what to apply.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::future::BoxFuture;

pub struct ExecRequest<'a> {
    pub command: &'a str,
    pub workspace: &'a Path,
    pub timeout: Duration,
}

pub struct ExecResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// `None` when a signal ended the command.
    pub exit_code: Option<i32>,
    /// Files changed under the workspace, as paths relative to it.
    pub changes: Vec<FileChange>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FileChange {
    Written(PathBuf, Vec<u8>),
    Deleted(PathBuf),
}

/// A timeout gives an `io::ErrorKind::TimedOut` error.
pub trait Sandbox: Send + Sync {
    fn exec<'a>(&'a self, request: ExecRequest<'a>) -> BoxFuture<'a, io::Result<ExecResult>>;
}

/// Runs the command with `bash` on the host, in the workspace itself. The
/// command changes files in place, so `changes` is always empty.
#[cfg(not(target_arch = "wasm32"))]
pub struct LocalShell;

#[cfg(not(target_arch = "wasm32"))]
impl Sandbox for LocalShell {
    fn exec<'a>(&'a self, request: ExecRequest<'a>) -> BoxFuture<'a, io::Result<ExecResult>> {
        Box::pin(async move {
            let output = tokio::process::Command::new("bash")
                .arg("-c")
                .arg(request.command)
                .current_dir(request.workspace)
                .kill_on_drop(true)
                .output();

            let output = tokio::time::timeout(request.timeout, output)
                .await
                .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;

            Ok(ExecResult {
                stdout: output.stdout,
                stderr: output.stderr,
                exit_code: output.status.code(),
                changes: Vec::new(),
            })
        })
    }
}

/// Runs the command in a Linux VM with Apple's `container` CLI (macOS 26+,
/// Apple Silicon). The VM sees a copy of the workspace at `/workspace`.
///
/// By default the VM is on a host-only network: it cannot reach the
/// internet, but it can reach services that listen on the Mac.
#[cfg(target_os = "macos")]
pub struct AppleContainer {
    image: String,
    internet: bool,
    network_ready: tokio::sync::OnceCell<()>,
}

#[cfg(target_os = "macos")]
const GUEST_WORKSPACE: &str = "/workspace";

/// The host-only network the VMs join when they have no internet access.
#[cfg(target_os = "macos")]
const OFFLINE_NETWORK: &str = "ccrs-offline";

#[cfg(target_os = "macos")]
impl AppleContainer {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            internet: false,
            network_ready: tokio::sync::OnceCell::new(),
        }
    }

    /// Put the VM on the default network, with internet access.
    pub fn with_internet(mut self) -> Self {
        self.internet = true;
        self
    }

    async fn network_args(&self) -> io::Result<Vec<&'static str>> {
        if self.internet {
            return Ok(Vec::new());
        }

        self.network_ready
            .get_or_try_init(|| async {
                let exists = container(&["network", "inspect", OFFLINE_NETWORK]).await?;

                if !exists.status.success() {
                    let created =
                        container(&["network", "create", "--internal", OFFLINE_NETWORK]).await?;

                    if !created.status.success() {
                        return Err(io::Error::other(format!(
                            "failed to create network {OFFLINE_NETWORK}: {}",
                            String::from_utf8_lossy(&created.stderr)
                        )));
                    }
                }

                Ok(())
            })
            .await?;

        Ok(vec!["--network", OFFLINE_NETWORK, "--no-dns"])
    }
}

#[cfg(target_os = "macos")]
async fn container(args: &[&str]) -> io::Result<std::process::Output> {
    tokio::process::Command::new("container")
        .args(args)
        .output()
        .await
}

#[cfg(target_os = "macos")]
impl Sandbox for AppleContainer {
    fn exec<'a>(&'a self, request: ExecRequest<'a>) -> BoxFuture<'a, io::Result<ExecResult>> {
        Box::pin(async move {
            let copy = tempfile::tempdir()?;
            let before = snapshot(request.workspace, copy.path())?;

            let name = format!("ccrs-{:016x}", rand::random::<u64>());
            let network = self.network_args().await?;

            let run = tokio::process::Command::new("container")
                .args(["run", "--rm", "--name", &name])
                .args(network)
                .arg("--volume")
                .arg(format!("{}:{GUEST_WORKSPACE}", copy.path().display()))
                .args(["--workdir", GUEST_WORKSPACE, &self.image])
                .args(["bash", "-c", request.command])
                .kill_on_drop(true)
                .output();

            let output = match tokio::time::timeout(request.timeout, run).await {
                Ok(output) => output?,
                Err(_) => {
                    // Killing the CLI does not stop the VM.
                    let _ = container(&["rm", "--force", &name]).await;
                    return Err(io::ErrorKind::TimedOut.into());
                }
            };

            let after = read_tree(copy.path())?;

            Ok(ExecResult {
                stdout: output.stdout,
                stderr: output.stderr,
                exit_code: output.status.code(),
                changes: diff(before, after),
            })
        })
    }
}

// The helpers below serve the container sandboxes only: LocalShell works in
// place and has no snapshot to take.

/// File contents by path relative to the root. `.git` and files that
/// `.gitignore` excludes are left out.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
type Tree = BTreeMap<PathBuf, Vec<u8>>;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn read_tree(root: &Path) -> io::Result<Tree> {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();

    let mut tree = Tree::new();

    for entry in walker {
        let entry = entry.map_err(io::Error::other)?;

        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }

        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(io::Error::other)?
            .to_path_buf();

        tree.insert(relative, std::fs::read(entry.path())?);
    }

    Ok(tree)
}

/// Copy the workspace to `target` and return what was copied. `.git` is
/// copied too, so git commands work, but its changes are never reported.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn snapshot(workspace: &Path, target: &Path) -> io::Result<Tree> {
    let tree = read_tree(workspace)?;

    for (relative, contents) in &tree {
        let path = target.join(relative);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(&path, contents)?;
        std::fs::set_permissions(
            &path,
            std::fs::metadata(workspace.join(relative))?.permissions(),
        )?;
    }

    let git = workspace.join(".git");

    if git.is_dir() {
        copy_dir(&git, &target.join(".git"))?;
    }

    Ok(tree)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;

    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = to.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }

    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn diff(before: Tree, mut after: Tree) -> Vec<FileChange> {
    let mut changes = Vec::new();

    for (path, contents) in before {
        match after.remove(&path) {
            Some(new) if new != contents => changes.push(FileChange::Written(path, new)),
            Some(_) => {}
            None => changes.push(FileChange::Deleted(path)),
        }
    }

    changes.extend(
        after
            .into_iter()
            .map(|(path, contents)| FileChange::Written(path, contents)),
    );
    changes.sort_by(|a, b| change_path(a).cmp(change_path(b)));

    changes
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn change_path(change: &FileChange) -> &Path {
    match change {
        FileChange::Written(path, _) | FileChange::Deleted(path) => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_reports_writes_and_deletes() {
        let before = Tree::from([
            ("kept".into(), b"same".to_vec()),
            ("edited".into(), b"old".to_vec()),
            ("removed".into(), b"x".to_vec()),
        ]);
        let after = Tree::from([
            ("kept".into(), b"same".to_vec()),
            ("edited".into(), b"new".to_vec()),
            ("added".into(), b"y".to_vec()),
        ]);

        assert_eq!(
            diff(before, after),
            [
                FileChange::Written("added".into(), b"y".to_vec()),
                FileChange::Written("edited".into(), b"new".to_vec()),
                FileChange::Deleted("removed".into()),
            ]
        );
    }

    #[test]
    fn snapshot_skips_ignored_files() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target/big.bin"), "artifact").unwrap();

        let copy = tempfile::tempdir().unwrap();
        let tree = snapshot(root, copy.path()).unwrap();

        assert_eq!(
            tree.keys().collect::<Vec<_>>(),
            [Path::new(".gitignore"), Path::new("main.rs")]
        );
        assert!(!copy.path().join("target").exists());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod container_tests {
    use super::*;

    /// Needs `container system start` and pulls `debian:bookworm-slim`.
    #[tokio::test]
    #[ignore]
    async fn apple_container_returns_changes() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("in.txt"), "hello").unwrap();
        std::fs::write(workspace.path().join("gone.txt"), "x").unwrap();

        let sandbox = AppleContainer::new("debian:bookworm-slim");

        let result = sandbox
            .exec(ExecRequest {
                command: "uname -s; tr a-z A-Z < in.txt > out.txt; rm gone.txt",
                workspace: workspace.path(),
                timeout: Duration::from_secs(120),
            })
            .await
            .unwrap();

        assert_eq!(
            result.exit_code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&result.stdout), "Linux\n");
        assert_eq!(
            result.changes,
            [
                FileChange::Deleted("gone.txt".into()),
                FileChange::Written("out.txt".into(), b"HELLO".to_vec()),
            ]
        );

        // The workspace itself is untouched.
        assert!(workspace.path().join("gone.txt").exists());
        assert!(!workspace.path().join("out.txt").exists());
    }

    /// Needs `container system start` and pulls `debian:bookworm-slim`.
    #[tokio::test]
    #[ignore]
    async fn apple_container_is_offline_by_default() {
        let workspace = tempfile::tempdir().unwrap();
        let probe = "timeout 3 bash -c 'echo > /dev/tcp/1.1.1.1/80' && echo online || echo offline";

        let run = |sandbox: AppleContainer| {
            let workspace = workspace.path().to_path_buf();
            async move {
                let result = sandbox
                    .exec(ExecRequest {
                        command: probe,
                        workspace: &workspace,
                        timeout: Duration::from_secs(60),
                    })
                    .await
                    .unwrap();
                String::from_utf8_lossy(&result.stdout).trim().to_string()
            }
        };

        assert_eq!(
            run(AppleContainer::new("debian:bookworm-slim")).await,
            "offline"
        );
        assert_eq!(
            run(AppleContainer::new("debian:bookworm-slim").with_internet()).await,
            "online"
        );
    }
}
