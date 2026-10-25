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

impl FileChange {
    /// The path, relative to the workspace.
    pub fn path(&self) -> &Path {
        match self {
            FileChange::Written(path, _) | FileChange::Deleted(path) => path,
        }
    }
}

/// A timeout gives an `io::ErrorKind::TimedOut` error.
pub trait Sandbox: Send + Sync {
    fn exec<'a>(&'a self, request: ExecRequest<'a>) -> BoxFuture<'a, io::Result<ExecResult>>;

    /// What the model must know about where its commands run. `None` when
    /// they run on the host, in the workspace.
    fn describe(&self) -> Option<String> {
        None
    }
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
/// internet, but it can reach services that listen on the Mac. With an
/// allowlist, it reaches the allowed hosts over HTTPS through an
/// [`EgressProxy`](crate::egress::EgressProxy) on the host.
#[cfg(target_os = "macos")]
pub struct AppleContainer {
    image: String,
    egress: Egress,
    offline: tokio::sync::OnceCell<Offline>,
    proxy: tokio::sync::OnceCell<String>,
}

#[cfg(target_os = "macos")]
enum Egress {
    None,
    Allowlist(crate::egress::Allowlist),
    Internet,
}

/// The host-only network, as `container network inspect` reports it.
#[cfg(target_os = "macos")]
struct Offline {
    gateway: std::net::Ipv4Addr,
    subnet: crate::egress::Peers,
}

#[cfg(target_os = "macos")]
const GUEST_WORKSPACE: &str = "/workspace";

/// The host-only network the VMs join when they have no internet access.
#[cfg(target_os = "macos")]
const OFFLINE_NETWORK: &str = "ccrs-offline";

/// Fixed so that a firewall rule can let the VMs reach the proxy and
/// nothing else on the host.
#[cfg(target_os = "macos")]
pub const PROXY_PORT: u16 = 3128;

#[cfg(target_os = "macos")]
impl AppleContainer {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            egress: Egress::None,
            offline: tokio::sync::OnceCell::new(),
            proxy: tokio::sync::OnceCell::new(),
        }
    }

    /// Let the VM reach these hosts over HTTPS, and nothing else.
    pub fn with_allowlist(mut self, allowlist: crate::egress::Allowlist) -> Self {
        self.egress = Egress::Allowlist(allowlist);
        self
    }

    /// Put the VM on the default network, with internet access.
    pub fn with_internet(mut self) -> Self {
        self.egress = Egress::Internet;
        self
    }

    /// Arguments for `container run` that set up the network.
    async fn network_args(&self) -> io::Result<Vec<String>> {
        let allowlist = match &self.egress {
            Egress::Internet => return Ok(Vec::new()),
            Egress::None => None,
            Egress::Allowlist(allowlist) => Some(allowlist),
        };

        let offline = self.offline.get_or_try_init(offline_network).await?;

        let mut args = ["--network", OFFLINE_NETWORK, "--no-dns"]
            .map(String::from)
            .to_vec();

        if let Some(allowlist) = allowlist {
            let url = self
                .proxy
                .get_or_try_init(|| async {
                    let proxy = crate::egress::EgressProxy::start(
                        allowlist.clone(),
                        offline.subnet,
                        PROXY_PORT,
                    )
                    .await?;
                    io::Result::Ok(format!("http://{}:{}", offline.gateway, proxy.port))
                })
                .await?;

            for var in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
                args.push("--env".to_string());
                args.push(format!("{var}={url}"));
            }
        }

        Ok(args)
    }
}

/// Create the host-only network if it does not exist and read its gateway
/// and subnet.
#[cfg(target_os = "macos")]
async fn offline_network() -> io::Result<Offline> {
    let mut inspect = container(&["network", "inspect", OFFLINE_NETWORK]).await?;

    if !inspect.status.success() || inspect.stdout.starts_with(b"[]") {
        let created = container(&["network", "create", "--internal", OFFLINE_NETWORK]).await?;

        if !created.status.success() {
            return Err(io::Error::other(format!(
                "failed to create network {OFFLINE_NETWORK}: {}",
                String::from_utf8_lossy(&created.stderr)
            )));
        }

        inspect = container(&["network", "inspect", OFFLINE_NETWORK]).await?;
    }

    let report: serde_json::Value = serde_json::from_slice(&inspect.stdout)?;
    let status = &report[0]["status"];
    let invalid = || io::Error::other(format!("unexpected network report: {report}"));

    let gateway = status["ipv4Gateway"]
        .as_str()
        .and_then(|g| g.parse().ok())
        .ok_or_else(invalid)?;

    let (network, prefix) = status["ipv4Subnet"]
        .as_str()
        .and_then(|s| s.split_once('/'))
        .ok_or_else(invalid)?;

    Ok(Offline {
        gateway,
        subnet: crate::egress::Peers {
            network: network.parse().map_err(|_| invalid())?,
            prefix: prefix.parse().map_err(|_| invalid())?,
        },
    })
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
    fn describe(&self) -> Option<String> {
        let network = match &self.egress {
            Egress::None => "It has no network access.".to_string(),
            Egress::Allowlist(allowlist) => format!(
                "Its only network access is HTTPS, through a proxy set in HTTPS_PROXY, \
                 to these hosts: {}. Plain HTTP, other ports, DNS and other hosts fail.",
                allowlist.patterns().join(", ")
            ),
            Egress::Internet => "It has internet access.".to_string(),
        };

        Some(format!(
            "Bash commands run in a fresh Linux VM ({image}) on a copy of the workspace, \
             mounted at {GUEST_WORKSPACE}. Nothing persists between commands except files \
             in the workspace: installed packages and background processes are gone. {network} \
             After each command, the user approves or rejects the files it changed; \
             changes inside .git and to ignored files are discarded.",
            image = self.image,
        ))
    }

    fn exec<'a>(&'a self, request: ExecRequest<'a>) -> BoxFuture<'a, io::Result<ExecResult>> {
        Box::pin(async move {
            let copy = tempfile::tempdir()?;
            let before = snapshot(request.workspace, copy.path())?;

            let name = format!("ccrs-{:016x}", rand::random::<u64>());
            let network = self.network_args().await?;

            let run = tokio::process::Command::new("container")
                // The progress bar would end up in the command's stderr.
                .args(["run", "--rm", "--progress", "none", "--name", &name])
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
    changes.sort_by(|a, b| a.path().cmp(b.path()));

    changes
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

    /// Needs `container system start` and pulls `debian:bookworm-slim`. The
    /// image has no curl, so the probe speaks CONNECT by hand.
    #[tokio::test]
    #[ignore]
    async fn apple_container_reaches_allowed_hosts_only() {
        let workspace = tempfile::tempdir().unwrap();
        let sandbox = AppleContainer::new("debian:bookworm-slim")
            .with_allowlist(crate::egress::Allowlist::new(["example.com"]));

        // Send CONNECT to the proxy from HTTPS_PROXY and print its status line.
        let probe = |host: &str| {
            format!(
                "p=${{HTTPS_PROXY#http://}}; exec 3<>/dev/tcp/${{p%:*}}/${{p#*:}}; \
                 printf 'CONNECT {host}:443 HTTP/1.1\\r\\n\\r\\n' >&3; \
                 timeout 5 head -n1 <&3"
            )
        };

        for (host, expected) in [("example.com", "200"), ("example.org", "403")] {
            let command = probe(host);
            let result = sandbox
                .exec(ExecRequest {
                    command: &command,
                    workspace: workspace.path(),
                    timeout: Duration::from_secs(60),
                })
                .await
                .unwrap();
            let stdout = String::from_utf8_lossy(&result.stdout);

            assert!(
                stdout.contains(expected),
                "{host}: {stdout} {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
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
