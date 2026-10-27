//! Command execution capability used by the Bash and Terminal tools.
//!
//! A sandbox runs commands on a copy of the workspace and reports the files
//! they changed. The caller decides what to apply and tells the sandbox.

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

    /// How to start a long-running `command`: under a PTY for a terminal
    /// (`tty`), or with plain pipes for a monitor. By default it runs on the
    /// host, in the workspace.
    #[cfg(not(target_arch = "wasm32"))]
    fn terminal<'a>(
        &'a self,
        command: &'a str,
        _workspace: &'a Path,
        _io: Io,
    ) -> BoxFuture<'a, io::Result<TerminalPlan>> {
        Box::pin(std::future::ready(Ok(host_terminal(command))))
    }

    /// Files changed in the sandbox's copy of the workspace and not yet
    /// accepted or rejected.
    fn pending(&self) -> BoxFuture<'_, io::Result<Vec<FileChange>>> {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }

    /// Record the user's decision on `changes`. Accepted changes become the
    /// copy's new base; rejected ones are undone in the copy.
    fn settle<'a>(
        &'a self,
        _changes: &'a [FileChange],
        _accepted: bool,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// How a long-running program talks to ccrs.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Io {
    /// A terminal: TerminalOpen.
    Pty,
    /// Plain stdout and stderr, read line by line: Monitor.
    Pipes,
}

/// A program to start, under a PTY or with pipes.
#[cfg(not(target_arch = "wasm32"))]
pub struct TerminalPlan {
    pub program: String,
    pub args: Vec<String>,
    /// Held while the program runs. Dropping it tells the sandbox the
    /// program is gone.
    pub lease: Option<Box<dyn Send + Sync>>,
}

/// `bash` on the host, interactive when `command` is empty.
#[cfg(not(target_arch = "wasm32"))]
pub fn host_terminal(command: &str) -> TerminalPlan {
    let args = match command.trim() {
        "" => Vec::new(),
        _ => vec!["-c".to_string(), command.to_string()],
    };

    TerminalPlan {
        program: "bash".to_string(),
        args,
        lease: None,
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

#[cfg(target_os = "macos")]
pub use apple::AppleContainer;

/// Fixed so that a firewall rule can let the VMs reach the proxy and
/// nothing else on the host.
#[cfg(target_os = "macos")]
pub const PROXY_PORT: u16 = 3128;

#[cfg(target_os = "macos")]
mod apple {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, Weak};
    use std::time::Instant;

    use super::copy::SharedCopy;
    use super::*;

    const GUEST_WORKSPACE: &str = "/workspace";

    /// The host-only network the VMs join when they have no internet access.
    const OFFLINE_NETWORK: &str = "ccrs-offline";

    /// How long the VM may sit unused before it is stopped.
    const DEFAULT_IDLE: Duration = Duration::from_secs(5 * 60);

    /// Runs commands in one Linux VM per session with Apple's `container` CLI
    /// (macOS 26+, Apple Silicon). The VM sees a copy of the workspace at
    /// `/workspace`.
    ///
    /// The VM starts on first use and stops after it sits unused, unless a
    /// terminal is open. A stopped VM keeps its disk: installed packages and
    /// build caches survive, running processes do not. It is deleted when
    /// the sandbox is dropped.
    ///
    /// By default the VM is on a host-only network: it cannot reach the
    /// internet, but it can reach services that listen on the Mac. With an
    /// allowlist, it reaches the allowed hosts over HTTPS through an
    /// [`EgressProxy`](crate::egress::EgressProxy) on the host.
    pub struct AppleContainer {
        image: String,
        egress: Egress,
        idle: Duration,
        offline: tokio::sync::OnceCell<Offline>,
        proxy: tokio::sync::OnceCell<String>,
        inner: Arc<Inner>,
    }

    enum Egress {
        None,
        Allowlist(crate::egress::Allowlist),
        Internet,
    }

    /// The host-only network, as `container network inspect` reports it.
    struct Offline {
        gateway: std::net::Ipv4Addr,
        subnet: crate::egress::Peers,
    }

    /// What the idle reaper shares with the sandbox.
    struct Inner {
        vm: tokio::sync::Mutex<Option<Vm>>,
        /// Commands and terminals running now; the VM is not stopped under them.
        in_use: AtomicUsize,
        last_used: Mutex<Instant>,
    }

    struct Vm {
        name: String,
        copy: SharedCopy,
        running: bool,
    }

    /// Network arguments for `container create`, and the environment every
    /// process in the VM gets.
    struct Network {
        args: Vec<String>,
        env: Vec<String>,
    }

    impl AppleContainer {
        pub fn new(image: impl Into<String>) -> Self {
            Self {
                image: image.into(),
                egress: Egress::None,
                idle: DEFAULT_IDLE,
                offline: tokio::sync::OnceCell::new(),
                proxy: tokio::sync::OnceCell::new(),
                inner: Arc::new(Inner {
                    vm: tokio::sync::Mutex::new(None),
                    in_use: AtomicUsize::new(0),
                    last_used: Mutex::new(Instant::now()),
                }),
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

        /// Stop the VM after it sits unused this long (default 5 minutes).
        pub fn with_idle(mut self, idle: Duration) -> Self {
            self.idle = idle;
            self
        }

        async fn network(&self) -> io::Result<Network> {
            let allowlist = match &self.egress {
                Egress::Internet => {
                    return Ok(Network {
                        args: Vec::new(),
                        env: Vec::new(),
                    });
                }
                Egress::None => None,
                Egress::Allowlist(allowlist) => Some(allowlist),
            };

            let offline = self.offline.get_or_try_init(offline_network).await?;
            let args = ["--network", OFFLINE_NETWORK, "--no-dns"]
                .map(String::from)
                .to_vec();
            let mut env = Vec::new();

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
                    env.push(format!("{var}={url}"));
                }
            }

            Ok(Network { args, env })
        }

        /// Create or start the VM, bring the host's file changes into its
        /// copy, and return the VM name and the environment for `exec`.
        async fn ready(&self, workspace: &Path) -> io::Result<(String, Vec<String>)> {
            let network = self.network().await?;
            let mut vm = self.inner.vm.lock().await;

            if vm.is_none() {
                let copy = SharedCopy::new(workspace)?;
                let name = format!("ccrs-{:016x}", rand::random::<u64>());

                let mut args = ["create", "--name", &name].map(String::from).to_vec();
                args.extend(network.args.iter().cloned());
                for var in &network.env {
                    args.extend(["--env".to_string(), var.clone()]);
                }
                args.push("--volume".to_string());
                args.push(format!("{}:{GUEST_WORKSPACE}", copy.path().display()));
                // The init process only keeps the VM up; commands come in
                // with `exec`.
                args.extend(
                    [
                        "--workdir",
                        GUEST_WORKSPACE,
                        &self.image,
                        "sleep",
                        "infinity",
                    ]
                    .map(String::from),
                );

                check(container(&args.iter().map(String::as_str).collect::<Vec<_>>()).await?)?;

                *vm = Some(Vm {
                    name,
                    copy,
                    running: false,
                });

                spawn_reaper(Arc::downgrade(&self.inner), self.idle);
            }

            let vm = vm.as_mut().unwrap();

            if !vm.running {
                check(container(&["start", &vm.name]).await?)?;
                vm.running = true;
            }

            vm.copy.sync_from(workspace)?;
            self.inner.touch();

            Ok((vm.name.clone(), network.env))
        }

        #[cfg(test)]
        pub(super) async fn vm_name(&self) -> Option<String> {
            self.inner
                .vm
                .lock()
                .await
                .as_ref()
                .map(|vm| vm.name.clone())
        }

        fn exec_args(name: &str, env: &[String], io: Io) -> Vec<String> {
            let mut args = vec!["exec".to_string()];

            if io == Io::Pty {
                args.push("-it".to_string());
            }

            for var in env {
                args.extend(["--env".to_string(), var.clone()]);
            }

            args.extend(["--workdir", GUEST_WORKSPACE, name, "bash"].map(String::from));
            args
        }
    }

    impl Inner {
        fn touch(&self) {
            *self.last_used.lock().unwrap() = Instant::now();
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            if let Some(vm) = self.vm.get_mut() {
                let _ = std::process::Command::new("container")
                    .args(["rm", "--force", &vm.name])
                    .output();
            }
        }
    }

    /// Stops the VM once it sits unused for `idle` with nothing running.
    /// Ends when the sandbox is dropped.
    fn spawn_reaper(inner: Weak<Inner>, idle: Duration) {
        let tick = (idle / 10).clamp(Duration::from_millis(100), Duration::from_secs(30));

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;

                let Some(inner) = inner.upgrade() else {
                    return;
                };

                let unused = || {
                    inner.last_used.lock().unwrap().elapsed() >= idle
                        && inner.in_use.load(Ordering::SeqCst) == 0
                };

                if !unused() {
                    continue;
                }

                let mut vm = inner.vm.lock().await;

                // A command may have started the VM while this task waited
                // for the lock.
                if !unused() {
                    continue;
                }

                if let Some(vm) = vm.as_mut().filter(|vm| vm.running) {
                    // The init process ignores SIGTERM, so `stop` would wait
                    // for its timeout.
                    let _ = container(&["kill", &vm.name]).await;
                    vm.running = false;
                }
            }
        });
    }

    /// Counts a running command or an open terminal; dropping it marks the
    /// VM as used.
    struct Lease(Arc<Inner>);

    impl Lease {
        fn new(inner: &Arc<Inner>) -> Self {
            inner.in_use.fetch_add(1, Ordering::SeqCst);
            Self(inner.clone())
        }
    }

    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.in_use.fetch_sub(1, Ordering::SeqCst);
            self.0.touch();
        }
    }

    /// Create the host-only network if it does not exist and read its
    /// gateway and subnet.
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

    async fn container(args: &[&str]) -> io::Result<std::process::Output> {
        tokio::process::Command::new("container")
            .args(args)
            .output()
            .await
    }

    fn check(output: std::process::Output) -> io::Result<()> {
        if output.status.success() {
            return Ok(());
        }

        Err(io::Error::other(format!(
            "container: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

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
                "Bash commands and terminals run in one Linux VM ({image}) for this session, \
                 on a copy of the workspace mounted at {GUEST_WORKSPACE}. The VM keeps its \
                 disk between commands: installed packages and build caches stay. After \
                 {idle} minutes unused it is stopped: running processes end, files stay. \
                 {network} After each command, and when a terminal is closed, the user \
                 approves or rejects the files that changed; changes inside .git and to \
                 ignored files are never applied.",
                image = self.image,
                idle = self.idle.as_secs().div_ceil(60),
            ))
        }

        fn terminal<'a>(
            &'a self,
            command: &'a str,
            workspace: &'a Path,
            io: Io,
        ) -> BoxFuture<'a, io::Result<TerminalPlan>> {
            Box::pin(async move {
                let lease = Lease::new(&self.inner);
                let (name, env) = self.ready(workspace).await?;
                let mut args = Self::exec_args(&name, &env, io);

                if !command.trim().is_empty() {
                    args.extend(["-c".to_string(), command.to_string()]);
                }

                Ok(TerminalPlan {
                    program: "container".to_string(),
                    args,
                    lease: Some(Box::new(lease)),
                })
            })
        }

        fn exec<'a>(&'a self, request: ExecRequest<'a>) -> BoxFuture<'a, io::Result<ExecResult>> {
            Box::pin(async move {
                let lease = Lease::new(&self.inner);
                let (name, env) = self.ready(request.workspace).await?;
                let mut args = Self::exec_args(&name, &env, Io::Pipes);
                args.extend(["-c".to_string(), request.command.to_string()]);

                let run = tokio::process::Command::new("container")
                    .args(&args)
                    .kill_on_drop(true)
                    .output();

                let output = match tokio::time::timeout(request.timeout, run).await {
                    Ok(output) => output?,
                    Err(_) => {
                        // Killing `exec` leaves the command running in the
                        // VM; stopping the VM ends it and keeps the disk.
                        let mut vm = self.inner.vm.lock().await;
                        if let Some(vm) = vm.as_mut() {
                            let _ = container(&["kill", &vm.name]).await;
                            vm.running = false;
                        }
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                };

                drop(lease);

                Ok(ExecResult {
                    stdout: output.stdout,
                    stderr: output.stderr,
                    exit_code: output.status.code(),
                    changes: self.pending().await?,
                })
            })
        }

        fn pending(&self) -> BoxFuture<'_, io::Result<Vec<FileChange>>> {
            Box::pin(async move {
                match self.inner.vm.lock().await.as_ref() {
                    Some(vm) => vm.copy.pending(),
                    None => Ok(Vec::new()),
                }
            })
        }

        fn settle<'a>(
            &'a self,
            changes: &'a [FileChange],
            accepted: bool,
        ) -> BoxFuture<'a, io::Result<()>> {
            Box::pin(async move {
                match self.inner.vm.lock().await.as_mut() {
                    Some(vm) => vm.copy.settle(changes, accepted),
                    None => Ok(()),
                }
            })
        }
    }
}

// The copy helpers serve the container sandboxes; LocalShell works in place.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod copy {
    use std::collections::BTreeMap;
    use std::io;
    use std::path::{Path, PathBuf};

    use super::FileChange;

    /// File contents by path relative to the root. `.git` and files that
    /// `.gitignore` excludes are left out.
    pub(super) type Tree = BTreeMap<PathBuf, Vec<u8>>;

    /// A copy of the workspace that lives as long as the sandbox, and the
    /// contents both sides agree on.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) struct SharedCopy {
        dir: tempfile::TempDir,
        accepted: Tree,
    }

    #[cfg(not(target_arch = "wasm32"))]
    impl SharedCopy {
        pub(super) fn new(workspace: &Path) -> io::Result<Self> {
            let dir = tempfile::tempdir()?;
            let accepted = snapshot(workspace, dir.path())?;

            Ok(Self { dir, accepted })
        }

        pub(super) fn path(&self) -> &Path {
            self.dir.path()
        }

        /// Bring files the host changed since the last sync into the copy.
        /// A file the sandbox also changed keeps the sandbox's version; its
        /// base becomes the host's, so the pending change shows against it.
        pub(super) fn sync_from(&mut self, workspace: &Path) -> io::Result<()> {
            let host = read_tree(workspace)?;
            let copy = read_tree(self.dir.path())?;

            for change in diff(self.accepted.clone(), host) {
                let path = change.path().to_path_buf();
                let untouched = copy.get(&path) == self.accepted.get(&path);

                if untouched {
                    apply(self.dir.path(), &change)?;
                }

                record(&mut self.accepted, change);
            }

            Ok(())
        }

        /// Changes in the copy that are not yet accepted or rejected.
        pub(super) fn pending(&self) -> io::Result<Vec<FileChange>> {
            Ok(diff(self.accepted.clone(), read_tree(self.dir.path())?))
        }

        pub(super) fn settle(&mut self, changes: &[FileChange], accepted: bool) -> io::Result<()> {
            for change in changes {
                if accepted {
                    record(&mut self.accepted, clone(change));
                    continue;
                }

                // Put back what the base holds for this path.
                let path = change.path();
                let undo = match self.accepted.get(path) {
                    Some(contents) => FileChange::Written(path.to_path_buf(), contents.clone()),
                    None => FileChange::Deleted(path.to_path_buf()),
                };
                apply(self.dir.path(), &undo)?;
            }

            Ok(())
        }
    }

    fn clone(change: &FileChange) -> FileChange {
        match change {
            FileChange::Written(path, contents) => {
                FileChange::Written(path.clone(), contents.clone())
            }
            FileChange::Deleted(path) => FileChange::Deleted(path.clone()),
        }
    }

    fn record(tree: &mut Tree, change: FileChange) {
        match change {
            FileChange::Written(path, contents) => {
                tree.insert(path, contents);
            }
            FileChange::Deleted(path) => {
                tree.remove(&path);
            }
        }
    }

    fn apply(root: &Path, change: &FileChange) -> io::Result<()> {
        let target = root.join(change.path());

        match change {
            FileChange::Written(_, contents) => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(target, contents)
            }
            FileChange::Deleted(_) => match std::fs::remove_file(target) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                result => result,
            },
        }
    }

    pub(super) fn read_tree(root: &Path) -> io::Result<Tree> {
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
    pub(super) fn snapshot(workspace: &Path, target: &Path) -> io::Result<Tree> {
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

    pub(super) fn diff(before: Tree, mut after: Tree) -> Vec<FileChange> {
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
}

#[cfg(test)]
mod tests {
    use super::copy::*;
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

    fn write(root: &Path, name: &str, contents: &str) {
        std::fs::write(root.join(name), contents).unwrap();
    }

    #[test]
    fn shared_copy_settles_and_syncs() {
        let host = tempfile::tempdir().unwrap();
        write(host.path(), "a.txt", "a");
        write(host.path(), "b.txt", "b");

        let mut copy = SharedCopy::new(host.path()).unwrap();
        assert!(copy.pending().unwrap().is_empty());

        // The sandbox changes two files; the user takes one, rejects the other.
        write(copy.path(), "a.txt", "a2");
        write(copy.path(), "b.txt", "b2");
        let pending = copy.pending().unwrap();
        assert_eq!(pending.len(), 2);

        copy.settle(&pending[..1], true).unwrap();
        copy.settle(&pending[1..], false).unwrap();
        assert!(copy.pending().unwrap().is_empty());
        assert_eq!(std::fs::read(copy.path().join("b.txt")).unwrap(), b"b");

        // The host applies the accepted change and edits another file.
        write(host.path(), "a.txt", "a2");
        write(host.path(), "c.txt", "c");
        copy.sync_from(host.path()).unwrap();

        assert!(copy.pending().unwrap().is_empty());
        assert_eq!(std::fs::read(copy.path().join("c.txt")).unwrap(), b"c");
    }

    #[test]
    fn shared_copy_keeps_the_sandbox_side_of_a_conflict() {
        let host = tempfile::tempdir().unwrap();
        write(host.path(), "a.txt", "base");

        let mut copy = SharedCopy::new(host.path()).unwrap();
        write(copy.path(), "a.txt", "sandbox");
        write(host.path(), "a.txt", "host");
        copy.sync_from(host.path()).unwrap();

        // The pending change now reads against the host's version.
        assert_eq!(
            copy.pending().unwrap(),
            [FileChange::Written("a.txt".into(), b"sandbox".to_vec())]
        );

        copy.settle(&copy.pending().unwrap(), false).unwrap();
        assert_eq!(std::fs::read(copy.path().join("a.txt")).unwrap(), b"host");
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
    async fn apple_container_scales_to_zero_and_keeps_its_disk() {
        let workspace = tempfile::tempdir().unwrap();
        let sandbox = AppleContainer::new("debian:bookworm-slim").with_idle(Duration::from_secs(1));

        let run = |command: &'static str| {
            sandbox.exec(ExecRequest {
                command,
                workspace: workspace.path(),
                timeout: Duration::from_secs(60),
            })
        };

        run("echo installed > /opt/marker").await.unwrap();
        let name = sandbox.vm_name().await.unwrap();

        let running = || {
            let list = std::process::Command::new("container")
                .arg("list")
                .output()
                .unwrap();
            String::from_utf8_lossy(&list.stdout).contains(&name)
        };
        assert!(running());

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!running(), "the VM should be stopped after 1 s unused");

        let result = run("cat /opt/marker").await.unwrap();
        assert_eq!(String::from_utf8_lossy(&result.stdout), "installed\n");
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
