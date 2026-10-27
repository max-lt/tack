use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::{ToolDef, ToolOutput};
use crate::sandbox::{Sandbox, host_terminal};
use crate::store::{SharedStore, TerminalEntry};
use crate::terminal::{Snapshot, Terminals, parse_keys};

const DEFAULT_ROWS: u16 = 30;
const DEFAULT_COLS: u16 = 120;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 120_000;

pub struct TerminalOpenTool {
    terminals: Arc<Terminals>,
    sandbox: Arc<dyn Sandbox>,
    store: SharedStore,
}

pub struct TerminalSendTool {
    terminals: Arc<Terminals>,
}

pub struct TerminalReadTool {
    terminals: Arc<Terminals>,
}

pub struct TerminalCloseTool {
    terminals: Arc<Terminals>,
    sandbox: Arc<dyn Sandbox>,
    store: SharedStore,
}

/// The four tools, sharing one set of terminals. Open and close keep the
/// session's store in step.
pub fn tools(
    terminals: Arc<Terminals>,
    sandbox: Arc<dyn Sandbox>,
    store: SharedStore,
) -> (
    TerminalOpenTool,
    TerminalSendTool,
    TerminalReadTool,
    TerminalCloseTool,
) {
    (
        TerminalOpenTool {
            terminals: terminals.clone(),
            sandbox: sandbox.clone(),
            store: store.clone(),
        },
        TerminalSendTool {
            terminals: terminals.clone(),
        },
        TerminalReadTool {
            terminals: terminals.clone(),
        },
        TerminalCloseTool {
            terminals,
            sandbox,
            store,
        },
    )
}

fn id_of(input: &serde_json::Value) -> Result<u32, ToolOutput> {
    input
        .get("id")
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| ToolOutput::error("Missing required parameter: id"))
}

fn timeout_of(input: &serde_json::Value) -> Duration {
    let ms = input
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .min(MAX_TIMEOUT_MS);

    Duration::from_millis(ms)
}

fn render(id: u32, snapshot: &Snapshot) -> String {
    let mut out = format!("Terminal {id} screen:\n{}", snapshot.screen);

    if let Some(code) = snapshot.exit_code {
        write!(out, "\n[process exited with code {code}]").unwrap();
    }

    out
}

fn wait_properties() -> serde_json::Value {
    serde_json::json!({
        "wait_for": {
            "type": "string",
            "description": "Return once this text is on the screen. Without it, return once the output stops changing."
        },
        "timeout_ms": {
            "type": "integer",
            "description": "Give up waiting after this long (default 10000, max 120000)"
        }
    })
}

impl ToolDef for TerminalOpenTool {
    fn name(&self) -> &'static str {
        "TerminalOpen"
    }

    fn description(&self) -> &'static str {
        "Open an interactive terminal: the command runs under a PTY and keeps running between \
         calls. Use it for programs that need a TTY or keep state: REPLs, database shells, dev \
         servers, TUIs. It runs where Bash runs; with a sandbox, its file changes are offered \
         to the user when the terminal is closed. Set host to run on the user's machine \
         instead (for ssh to a real server, say); the user must approve it. Returns the \
         terminal id and the first screen. Use Bash for one-off commands."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Command to run with bash -c. Empty for an interactive bash."
                },
                "host": {
                    "type": "boolean",
                    "description": "Run on the user's machine, outside the sandbox (default false)"
                },
                "rows": { "type": "integer", "description": "Screen height (default 30)" },
                "cols": { "type": "integer", "description": "Screen width (default 120)" }
            }
        })
    }

    async fn execute(&self, input: &serde_json::Value, cwd: &Path) -> ToolOutput {
        let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let size = |key, default| {
            input
                .get(key)
                .and_then(|v| v.as_u64())
                .and_then(|v| u16::try_from(v).ok())
                .unwrap_or(default)
        };

        let host = input.get("host").and_then(|v| v.as_bool()).unwrap_or(false);

        let plan = if host {
            host_terminal(command)
        } else {
            match self.sandbox.terminal(command, cwd, crate::sandbox::Io::Pty).await {
                Ok(plan) => plan,
                Err(e) => return ToolOutput::error(format!("Failed to prepare terminal: {e}")),
            }
        };

        let id = match self.terminals.open(
            plan,
            cwd,
            size("rows", DEFAULT_ROWS),
            size("cols", DEFAULT_COLS),
        ) {
            Ok(id) => id,
            Err(e) => return ToolOutput::error(format!("Failed to open terminal: {e}")),
        };

        self.store.lock().unwrap().add_terminal(TerminalEntry {
            id,
            command: command.to_string(),
            host,
        });

        match self
            .terminals
            .read_first(id, Duration::from_millis(DEFAULT_TIMEOUT_MS))
            .await
        {
            Ok(snapshot) => ToolOutput::success(render(id, &snapshot)),
            Err(e) => ToolOutput::error(format!("Failed to read terminal {id}: {e}")),
        }
    }
}

impl ToolDef for TerminalSendTool {
    fn name(&self) -> &'static str {
        "TerminalSend"
    }

    fn description(&self) -> &'static str {
        "Type into an open terminal, then return its screen. Keys: <Enter>, <Tab>, <Esc>, <BS>, \
         <Up>, <Down>, <Left>, <Right>, <C-c> (any <C-letter>), <lt> for a literal <. \
         Add <Enter> to submit a line."
    }

    fn input_schema(&self) -> serde_json::Value {
        let mut properties = serde_json::json!({
            "id": { "type": "integer", "description": "Terminal id from TerminalOpen" },
            "input": { "type": "string", "description": "Text and keys to send" }
        });
        properties
            .as_object_mut()
            .unwrap()
            .extend(wait_properties().as_object().unwrap().clone());

        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": ["id", "input"]
        })
    }

    async fn execute(&self, input: &serde_json::Value, _cwd: &Path) -> ToolOutput {
        let id = match id_of(input) {
            Ok(id) => id,
            Err(e) => return e,
        };

        let Some(keys) = input.get("input").and_then(|v| v.as_str()) else {
            return ToolOutput::error("Missing required parameter: input");
        };

        if let Err(e) = self.terminals.send(id, &parse_keys(keys)) {
            return ToolOutput::error(format!("Failed to send to terminal {id}: {e}"));
        }

        let wait_for = input.get("wait_for").and_then(|v| v.as_str());

        match self.terminals.read(id, wait_for, timeout_of(input)).await {
            Ok(snapshot) => ToolOutput::success(render(id, &snapshot)),
            Err(e) => ToolOutput::error(format!("Failed to read terminal {id}: {e}")),
        }
    }
}

impl ToolDef for TerminalReadTool {
    fn name(&self) -> &'static str {
        "TerminalRead"
    }

    fn description(&self) -> &'static str {
        "Return the screen of an open terminal, waiting for text or for the output to settle. \
         Use it to wait for a slow command."
    }

    fn input_schema(&self) -> serde_json::Value {
        let mut properties = serde_json::json!({
            "id": { "type": "integer", "description": "Terminal id from TerminalOpen" }
        });
        properties
            .as_object_mut()
            .unwrap()
            .extend(wait_properties().as_object().unwrap().clone());

        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": ["id"]
        })
    }

    async fn execute(&self, input: &serde_json::Value, _cwd: &Path) -> ToolOutput {
        let id = match id_of(input) {
            Ok(id) => id,
            Err(e) => return e,
        };

        let wait_for = input.get("wait_for").and_then(|v| v.as_str());

        match self.terminals.read(id, wait_for, timeout_of(input)).await {
            Ok(snapshot) => ToolOutput::success(render(id, &snapshot)),
            Err(e) => ToolOutput::error(format!("Failed to read terminal {id}: {e}")),
        }
    }
}

impl ToolDef for TerminalCloseTool {
    fn name(&self) -> &'static str {
        "TerminalClose"
    }

    fn description(&self) -> &'static str {
        "Close an open terminal and kill its command. Returns the last screen."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Terminal id from TerminalOpen" }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, input: &serde_json::Value, _cwd: &Path) -> ToolOutput {
        let id = match id_of(input) {
            Ok(id) => id,
            Err(e) => return e,
        };

        let snapshot = match self.terminals.close(id) {
            Ok(snapshot) => snapshot,
            Err(e) => return ToolOutput::error(format!("Failed to close terminal {id}: {e}")),
        };

        self.store.lock().unwrap().remove_terminal(id);

        let output = ToolOutput::success(format!("Closed. {}", render(id, &snapshot)));

        match self.sandbox.pending().await {
            Ok(changes) => output.with_changes(changes),
            Err(e) => ToolOutput::error(format!(
                "Closed terminal {id}, but failed to read the file changes: {e}"
            )),
        }
    }
}
