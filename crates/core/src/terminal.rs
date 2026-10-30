//! Interactive terminals for the model: a command runs under a PTY, the
//! model sends keys and reads the rendered screen.
//!
//! A [`TerminalPlan`] from the sandbox decides where the terminal runs: on
//! the host, or in a VM on a copy of the workspace.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::process_group::{Group, REGISTRY, Registry, has_exited};
use crate::sandbox::TerminalPlan;

/// Output is settled when nothing new arrived for this long.
const SETTLE: Duration = Duration::from_millis(300);

const POLL: Duration = Duration::from_millis(50);

/// Most terminals open at one time in one session. Each holds a process,
/// a PTY, a reader thread and the sandbox lease.
pub const MAX_TERMINALS: usize = 8;

/// Largest screen. vt100 keeps one cell per character of the screen.
pub const MAX_ROWS: u16 = 200;
pub const MAX_COLS: u16 = 500;

/// Most bytes of screen text in one read, after the escape.
pub const MAX_SCREEN_BYTES: usize = 32 * 1024;

pub struct Terminals {
    open: Mutex<HashMap<u32, Terminal>>,
    next_id: AtomicU32,
    registry: &'static Registry,
}

impl Default for Terminals {
    fn default() -> Self {
        Self {
            open: Mutex::default(),
            next_id: AtomicU32::new(0),
            registry: &REGISTRY,
        }
    }
}

struct Terminal {
    /// The PTY makes the command a session leader: its pid is the id of its
    /// process group.
    group: Group,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Output>,
    _lease: Option<Box<dyn Send + Sync>>,
    // Dropping the master closes the PTY.
    _master: Box<dyn MasterPty + Send>,
}

/// What the reader thread shares with the tools.
struct Output {
    parser: Mutex<vt100::Parser>,
    last_output: Mutex<Instant>,
    ended: AtomicBool,
}

pub struct Snapshot {
    pub screen: String,
    /// `Some` once the command has exited.
    pub exit_code: Option<u32>,
}

impl Terminals {
    /// Start the plan's program under a new PTY in `cwd`.
    pub fn open(&self, plan: TerminalPlan, cwd: &Path, rows: u16, cols: u16) -> io::Result<u32> {
        let (Some(screen_rows), Some(screen_cols)) = (NonZeroU16::new(rows), NonZeroU16::new(cols))
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rows and cols must be positive",
            ));
        };

        if rows > MAX_ROWS || cols > MAX_COLS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the screen can have {MAX_ROWS} rows and {MAX_COLS} cols or fewer"),
            ));
        }

        if self.registry.is_closed() {
            return Err(io::Error::other("tack is exiting; no terminal can open"));
        }

        if self.open.lock().unwrap().len() >= MAX_TERMINALS {
            return Err(io::Error::other(format!(
                "{MAX_TERMINALS} terminals are open; close one first"
            )));
        }

        // An id from a session file can be the last u32.
        let id = self
            .next_id
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| last.checked_add(1))
            .map(|last| last + 1)
            .map_err(|_| io::Error::other("no terminal id left"))?;

        let pty = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)?;

        let mut cmd = CommandBuilder::new(&plan.program);
        cmd.args(&plan.args);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");

        let mut child = pty.slave.spawn_command(cmd).map_err(io::Error::other)?;

        let pid = child.process_id();
        if !pid.is_some_and(|pid| self.registry.add(pid)) {
            let _ = child.kill();
            return Err(io::Error::other("tack is exiting; no terminal can open"));
        }
        let group = Group {
            pgid: pid,
            registry: self.registry,
        };
        let mut reader = pty.master.try_clone_reader().map_err(io::Error::other)?;
        let writer = pty.master.take_writer().map_err(io::Error::other)?;

        let output = Arc::new(Output {
            parser: Mutex::new(vt100::Parser::new(screen_rows, screen_cols, 0)),
            last_output: Mutex::new(Instant::now()),
            ended: AtomicBool::new(false),
        });

        let shared = output.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];

            while let Ok(n @ 1..) = reader.read(&mut buf) {
                shared.parser.lock().unwrap().process(&buf[..n]);
                *shared.last_output.lock().unwrap() = Instant::now();
            }

            shared.ended.store(true, Ordering::SeqCst);
        });


        self.open.lock().unwrap().insert(
            id,
            Terminal {
                group,
                child,
                writer,
                output,
                _lease: plan.lease,
                _master: pty.master,
            },
        );

        Ok(id)
    }

    /// Give new terminals ids above `last`.
    pub fn reserve(&self, last: u32) {
        self.next_id.fetch_max(last, Ordering::SeqCst);
    }

    pub fn send(&self, id: u32, bytes: &[u8]) -> io::Result<()> {
        let mut open = self.open.lock().unwrap();
        let terminal = open.get_mut(&id).ok_or_else(|| unknown(id))?;

        terminal.writer.write_all(bytes)?;
        terminal.writer.flush()
    }

    /// Wait for visible text on the screen, then until it settles. A VM or
    /// a remote shell can take seconds to print its prompt, and control
    /// sequences before it show nothing.
    pub async fn read_first(&self, id: u32, timeout: Duration) -> io::Result<Snapshot> {
        let output = {
            let open = self.open.lock().unwrap();
            open.get(&id).ok_or_else(|| unknown(id))?.output.clone()
        };

        let start = Instant::now();

        let blank = |output: &Output| {
            let parser = output.parser.lock().unwrap();
            parser.screen().contents().trim().is_empty()
        };

        while blank(&output) && !output.ended.load(Ordering::SeqCst) && start.elapsed() < timeout {
            tokio::time::sleep(POLL).await;
        }

        self.read(id, None, timeout.saturating_sub(start.elapsed()))
            .await
    }

    /// Wait until `wait_for` appears on the screen, or, without it, until
    /// the output settles; then return the screen. Gives up at `timeout`.
    pub async fn read(
        &self,
        id: u32,
        wait_for: Option<&str>,
        timeout: Duration,
    ) -> io::Result<Snapshot> {
        let output = {
            let open = self.open.lock().unwrap();
            open.get(&id).ok_or_else(|| unknown(id))?.output.clone()
        };

        let start = Instant::now();

        loop {
            tokio::time::sleep(POLL).await;

            let done = match wait_for {
                Some(text) => output
                    .parser
                    .lock()
                    .unwrap()
                    .screen()
                    .contents()
                    .contains(text),
                None => output.last_output.lock().unwrap().elapsed() >= SETTLE,
            };

            if done || output.ended.load(Ordering::SeqCst) || start.elapsed() >= timeout {
                break;
            }
        }

        self.snapshot(id, &output)
    }

    /// Kill the command and return its last screen.
    pub fn close(&self, id: u32) -> io::Result<Snapshot> {
        let output = {
            let open = self.open.lock().unwrap();
            open.get(&id).ok_or_else(|| unknown(id))?.output.clone()
        };

        let snapshot = self.snapshot(id, &output)?;

        if self.open.lock().unwrap().remove(&id).is_none() {
            return Err(unknown(id));
        }

        Ok(snapshot)
    }

    fn snapshot(&self, id: u32, output: &Output) -> io::Result<Snapshot> {
        let exit_code = {
            let mut open = self.open.lock().unwrap();
            let terminal = open.get_mut(&id).ok_or_else(|| unknown(id))?;

            // Kill what the command left in its group before try_wait
            // reaps the leader, while the group id is still ours.
            if terminal.group.pgid.is_some_and(has_exited) {
                terminal.group.kill();
            }

            terminal.child.try_wait()?.map(|status| status.exit_code())
        };

        let screen = output.parser.lock().unwrap().screen().contents();

        Ok(Snapshot {
            screen: for_the_model(screen.trim_end()),
            exit_code,
        })
    }
}

impl Drop for Terminal {
    /// Kill the group first: the child kill can reap the leader.
    fn drop(&mut self) {
        self.group.kill();
        let _ = self.child.kill();
    }
}

/// The screen as text for the model. Any program can print a format
/// character that the user cannot see, so each line is escaped like monitor
/// output. Over `MAX_SCREEN_BYTES`, the top lines go: the newest output is
/// at the bottom.
fn for_the_model(screen: &str) -> String {
    let lines: Vec<String> = screen.lines().map(crate::monitor::escape).collect();

    let mut size = 0;
    let kept = lines
        .iter()
        .rev()
        .take_while(|line| {
            size += line.len() + 1;
            size <= MAX_SCREEN_BYTES
        })
        .count();

    let dropped = lines.len() - kept;
    let mut text = lines[dropped..].join("\n");

    if dropped > 0 {
        text.insert_str(0, &format!("({dropped} lines above not shown)\n"));
    }

    text
}

fn unknown(id: u32) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no open terminal {id}"))
}

/// Turn the model's input into bytes, with [`crate::keys::keys`]: the
/// permission check reads the input with the same parser.
pub fn parse_keys(input: &str) -> Vec<u8> {
    crate::keys::keys(input)
        .into_iter()
        .flat_map(crate::keys::Key::bytes)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host_terminal;

    #[test]
    fn the_number_of_open_terminals_is_capped() {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        let open = || terminals.open(host_terminal("sleep 30"), dir.path(), 24, 80);

        let ids: Vec<u32> = (0..MAX_TERMINALS).map(|_| open().unwrap()).collect();
        let err = open().unwrap_err();
        assert!(err.to_string().contains("terminals are open"), "{err}");

        terminals.close(ids[0]).unwrap();
        open().unwrap();
    }

    #[test]
    fn a_screen_larger_than_the_limit_is_refused() {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();

        for (rows, cols) in [(u16::MAX, 80), (24, u16::MAX), (MAX_ROWS + 1, 80), (24, MAX_COLS + 1)] {
            let err = terminals
                .open(host_terminal("true"), dir.path(), rows, cols)
                .unwrap_err();
            assert!(err.to_string().contains("rows"), "{rows}x{cols}: {err}");
        }

        terminals
            .open(host_terminal("true"), dir.path(), MAX_ROWS, MAX_COLS)
            .unwrap();
    }

    #[tokio::test]
    async fn hidden_characters_on_the_screen_reach_the_model_escaped() {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        // A bidi override and a tag-encoded "hi" around visible text.
        let id = terminals
            .open(
                host_terminal("printf 'a\\342\\200\\256b\\363\\240\\201\\250c\\n'; sleep 5"),
                dir.path(),
                24,
                80,
            )
            .unwrap();

        let screen = terminals.read_first(id, Duration::from_secs(5)).await.unwrap().screen;

        assert!(!screen.contains('\u{202e}') && !screen.contains('\u{e0068}'), "{screen:?}");
        assert!(screen.contains("\\u{202e}"), "{screen:?}");
    }

    #[tokio::test]
    async fn a_full_screen_stays_under_the_byte_limit() {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        // Each cell holds a character that escapes to 8 bytes.
        let id = terminals
            .open(
                host_terminal("while true; do printf '\\342\\200\\256%.0s' $(seq 1 400); done"),
                dir.path(),
                MAX_ROWS,
                MAX_COLS,
            )
            .unwrap();

        let screen = terminals.read(id, None, Duration::from_secs(2)).await.unwrap().screen;
        terminals.close(id).unwrap();

        assert!(screen.len() <= MAX_SCREEN_BYTES + 100, "{} bytes", screen.len());
    }

    #[test]
    fn a_long_screen_keeps_its_last_lines_under_the_limit() {
        // 200 lines of 500 hidden characters: about 800 KB once escaped.
        let line = "\u{202e}".repeat(500);
        let screen: Vec<String> = (0..200).map(|i| format!("{i:03}{line}")).collect();

        let text = for_the_model(&screen.join("\n"));

        assert!(text.len() <= MAX_SCREEN_BYTES + 40, "{} bytes", text.len());
        assert!(text.starts_with('(') && text.contains("lines above not shown"));
        assert!(text.lines().last().unwrap().starts_with("199"));
    }

    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    async fn gone_soon(pid: &str) -> bool {
        for _ in 0..50 {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    /// Open a terminal whose child ignores SIGHUP, and return its pid.
    async fn child_that_ignores_hangup(terminals: &Terminals, dir: &Path) -> (u32, String) {
        let id = terminals
            .open(host_terminal("trap '' HUP; sleep 300 & echo pid=$!; wait"), dir, 24, 80)
            .unwrap();
        let screen = terminals.read(id, Some("pid="), Duration::from_secs(5)).await.unwrap().screen;
        let pid = screen.split("pid=").nth(1).unwrap().split_whitespace().next().unwrap().to_string();
        assert!(alive(&pid));
        (id, pid)
    }

    #[tokio::test]
    async fn close_kills_the_children_of_the_command() {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        let (id, pid) = child_that_ignores_hangup(&terminals, dir.path()).await;

        terminals.close(id).unwrap();

        assert!(gone_soon(&pid).await, "child {pid} outlived TerminalClose");
    }

    #[tokio::test]
    async fn dropping_the_terminals_kills_the_children_too() {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        let (_, pid) = child_that_ignores_hangup(&terminals, dir.path()).await;

        drop(terminals);

        assert!(gone_soon(&pid).await, "child {pid} outlived the session");
    }

    #[tokio::test]
    async fn kill_all_reaches_the_children_of_a_terminal() {
        let registry: &'static Registry = Box::leak(Box::new(Registry::new()));
        let terminals = Terminals {
            registry,
            ..Terminals::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let (_, pid) = child_that_ignores_hangup(&terminals, dir.path()).await;

        registry.kill_all();

        assert!(gone_soon(&pid).await, "child {pid} outlived kill_all");
        let err = terminals.open(host_terminal("true"), dir.path(), 24, 80).unwrap_err();
        assert!(err.to_string().contains("exiting"), "{err}");
    }

    #[test]
    fn the_last_id_is_an_error_not_an_overflow() {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        terminals.reserve(u32::MAX);

        let err = terminals
            .open(host_terminal("true"), dir.path(), 24, 80)
            .unwrap_err();
        assert!(err.to_string().contains("no terminal id left"), "{err}");
    }

    /// Run `command` and read it within `timeout`. Returns the screen and
    /// the time the read took.
    async fn read_of(command: &str, timeout: Duration, first: bool) -> (String, Duration) {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        let id = terminals
            .open(host_terminal(command), dir.path(), MAX_ROWS, MAX_COLS)
            .unwrap();

        let started = Instant::now();
        let snapshot = match first {
            true => terminals.read_first(id, timeout).await,
            false => terminals.read(id, None, timeout).await,
        }
        .unwrap();
        let took = started.elapsed();

        terminals.close(id).unwrap();
        (snapshot.screen, took)
    }

    #[tokio::test]
    async fn output_without_end_stops_the_read_at_its_timeout() {
        let (screen, took) = read_of("yes 'no end'", Duration::from_secs(1), false).await;

        assert!(took < Duration::from_secs(2), "{took:?}");
        assert!(screen.contains("no end"));
        assert!(screen.len() <= MAX_SCREEN_BYTES + 40, "{} bytes", screen.len());
    }

    #[tokio::test]
    async fn binary_output_comes_back_escaped_and_bounded() {
        let (screen, took) = read_of(
            "head -c 2000000 /dev/urandom; sleep 5",
            Duration::from_secs(3),
            true,
        )
        .await;

        assert!(took < Duration::from_secs(4), "{took:?}");
        assert!(screen.len() <= MAX_SCREEN_BYTES + 40, "{} bytes", screen.len());
        assert!(!screen.chars().any(|c| c != '\n' && crate::escape::is_hidden(c)));
    }

    #[tokio::test]
    async fn a_command_that_never_prints_returns_at_the_timeout() {
        let (screen, took) = read_of("sleep 30", Duration::from_secs(1), true).await;

        assert!(took >= Duration::from_secs(1) && took < Duration::from_secs(2), "{took:?}");
        assert!(screen.is_empty(), "{screen:?}");
    }

    #[test]
    fn keys_are_translated() {
        assert_eq!(parse_keys("ls<Enter>"), b"ls\r");
        assert_eq!(parse_keys("<C-c><Up>"), b"\x03\x1b[A");
        assert_eq!(parse_keys("a <lt>b> c"), b"a <b> c");
        assert_eq!(parse_keys("x < y <Nope>"), b"x < y <Nope>");
    }

    #[tokio::test]
    async fn shell_state_survives_between_sends() {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        let id = terminals
            .open(host_terminal(""), dir.path(), 24, 80)
            .unwrap();

        terminals
            .send(id, &parse_keys("export X=kept; PS1='$ '<Enter>"))
            .unwrap();
        terminals
            .read(id, None, Duration::from_secs(5))
            .await
            .unwrap();

        terminals
            .send(id, &parse_keys("echo \"X=$X\"<Enter>"))
            .unwrap();
        let snapshot = terminals
            .read(id, Some("X=kept"), Duration::from_secs(5))
            .await
            .unwrap();

        assert!(snapshot.screen.contains("X=kept"), "{}", snapshot.screen);
        assert_eq!(snapshot.exit_code, None);

        terminals.close(id).unwrap();
        assert!(terminals.send(id, b"x").is_err());
    }

    #[tokio::test]
    async fn exit_code_is_reported() {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        let id = terminals
            .open(host_terminal("exit 3"), dir.path(), 24, 80)
            .unwrap();

        let snapshot = terminals
            .read(id, None, Duration::from_secs(5))
            .await
            .unwrap();
        // The reader may see EOF before the child is reaped.
        let snapshot = match snapshot.exit_code {
            Some(_) => snapshot,
            None => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                terminals
                    .read(id, None, Duration::from_secs(1))
                    .await
                    .unwrap()
            }
        };

        assert_eq!(snapshot.exit_code, Some(3));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod container_tests {
    use super::*;
    use crate::sandbox::{AppleContainer, ExecRequest, FileChange, Sandbox};

    /// Needs `container system start` and pulls `debian:bookworm-slim`.
    #[tokio::test]
    #[ignore]
    async fn terminal_and_bash_share_one_vm() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("in.txt"), "hello").unwrap();

        let sandbox = AppleContainer::new("debian:bookworm-slim");
        let terminals = Terminals::default();

        let plan = sandbox.terminal("", workspace.path(), crate::sandbox::Io::Pty).await.unwrap();
        let id = terminals.open(plan, workspace.path(), 24, 80).unwrap();
        terminals
            .read_first(id, Duration::from_secs(30))
            .await
            .unwrap();

        // State outside the workspace, and a file inside it.
        terminals
            .send(id, &parse_keys("echo kept > /opt/marker; tr a-z A-Z < in.txt > out.txt; echo done-$((40+2))<Enter>"))
            .unwrap();
        let screen = terminals
            .read(id, Some("done-42"), Duration::from_secs(10))
            .await
            .unwrap();
        assert!(screen.screen.contains("done-42"), "{}", screen.screen);
        terminals.close(id).unwrap();

        let pending = sandbox.pending().await.unwrap();
        assert_eq!(
            pending,
            [FileChange::Written("out.txt".into(), b"HELLO".to_vec())]
        );
        assert!(!workspace.path().join("out.txt").exists());

        // Bash sees what the terminal left, in the same VM.
        let result = sandbox
            .exec(ExecRequest {
                command: "cat /opt/marker",
                workspace: workspace.path(),
                timeout: Duration::from_secs(30),
            })
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&result.stdout), "kept\n");
    }
}
