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

use crate::sandbox::TerminalPlan;

/// Output is settled when nothing new arrived for this long.
const SETTLE: Duration = Duration::from_millis(300);

const POLL: Duration = Duration::from_millis(50);

#[derive(Default)]
pub struct Terminals {
    open: Mutex<HashMap<u32, Terminal>>,
    next_id: AtomicU32,
}

struct Terminal {
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

        let child = pty.slave.spawn_command(cmd).map_err(io::Error::other)?;
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

        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;

        self.open.lock().unwrap().insert(
            id,
            Terminal {
                child,
                writer,
                output,
                _lease: plan.lease,
                _master: pty.master,
            },
        );

        Ok(id)
    }

    /// Hand out ids above `last` from now on.
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

        let Some(mut terminal) = self.open.lock().unwrap().remove(&id) else {
            return Err(unknown(id));
        };

        let _ = terminal.child.kill();

        Ok(snapshot)
    }

    fn snapshot(&self, id: u32, output: &Output) -> io::Result<Snapshot> {
        let exit_code = {
            let mut open = self.open.lock().unwrap();
            let terminal = open.get_mut(&id).ok_or_else(|| unknown(id))?;
            terminal.child.try_wait()?.map(|status| status.exit_code())
        };

        let screen = output.parser.lock().unwrap().screen().contents();

        Ok(Snapshot {
            screen: screen.trim_end().to_string(),
            exit_code,
        })
    }
}

impl Drop for Terminals {
    fn drop(&mut self) {
        for terminal in self.open.get_mut().unwrap().values_mut() {
            let _ = terminal.child.kill();
        }
    }
}

fn unknown(id: u32) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no open terminal {id}"))
}

/// Turn the model's input into bytes. `<Enter>`, `<Tab>`, `<Esc>`, `<BS>`,
/// `<Up>`, `<Down>`, `<Left>`, `<Right>` and `<C-x>` are keys; `<lt>` is a
/// literal `<`. Any other text is sent as is.
pub fn parse_keys(input: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut rest = input;

    while let Some(start) = rest.find('<') {
        bytes.extend_from_slice(&rest.as_bytes()[..start]);
        rest = &rest[start..];

        let key = rest[1..].find('>').map(|end| &rest[1..=end]);

        match key.and_then(key_bytes) {
            Some(key_bytes) => {
                bytes.extend_from_slice(&key_bytes);
                rest = &rest[key.unwrap().len() + 2..];
            }
            None => {
                bytes.push(b'<');
                rest = &rest[1..];
            }
        }
    }

    bytes.extend_from_slice(rest.as_bytes());
    bytes
}

fn key_bytes(name: &str) -> Option<Vec<u8>> {
    let bytes: &[u8] = match name {
        "Enter" => b"\r",
        "Tab" => b"\t",
        "Esc" => b"\x1b",
        "BS" => b"\x7f",
        "Up" => b"\x1b[A",
        "Down" => b"\x1b[B",
        "Right" => b"\x1b[C",
        "Left" => b"\x1b[D",
        "lt" => b"<",
        _ => {
            let letter = name.strip_prefix("C-")?;
            let [c] = letter.as_bytes() else {
                return None;
            };
            return c
                .is_ascii_alphabetic()
                .then(|| vec![c.to_ascii_lowercase() - b'a' + 1]);
        }
    };

    Some(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host_terminal;

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

        let plan = sandbox.terminal("", workspace.path()).await.unwrap();
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
