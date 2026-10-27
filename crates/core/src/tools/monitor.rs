use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;

use super::{ToolDef, ToolOutput};
use crate::monitor::Monitors;
use crate::sandbox::{Sandbox, host_terminal};

pub struct MonitorTool {
    monitors: Arc<Monitors>,
    sandbox: Arc<dyn Sandbox>,
}

pub struct MonitorListTool {
    monitors: Arc<Monitors>,
}

pub struct MonitorStopTool {
    monitors: Arc<Monitors>,
}

/// The three tools, sharing one set of monitors.
pub fn tools(
    monitors: Arc<Monitors>,
    sandbox: Arc<dyn Sandbox>,
) -> (MonitorTool, MonitorListTool, MonitorStopTool) {
    (
        MonitorTool {
            monitors: monitors.clone(),
            sandbox,
        },
        MonitorListTool {
            monitors: monitors.clone(),
        },
        MonitorStopTool { monitors },
    )
}

impl ToolDef for MonitorTool {
    fn name(&self) -> &'static str {
        "Monitor"
    }

    fn description(&self) -> &'static str {
        "Start a background command and get woken by its output: each line it prints \
         (stdout or stderr) reaches you as a \"Monitor\" message, with lines that come close \
         together batched. Filter in the command so only lines worth a wake-up come out, \
         e.g. `tail -F app.log | grep --line-buffered ERROR`. It runs where Bash runs; set \
         host to run on the user's machine. It keeps running across turns and is started \
         again when the session is resumed. Stop it with MonitorStop. A stop kills the \
         process group of the command; a process that leaves the group (setsid, a \
         daemon) keeps running."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Command to run with bash -c" },
                "description": {
                    "type": "string",
                    "description": "What this watches, shown with every event"
                },
                "host": {
                    "type": "boolean",
                    "description": "Run on the user's machine, outside the sandbox (default false)"
                }
            },
            "required": ["command", "description"]
        })
    }

    async fn execute(&self, input: &serde_json::Value, cwd: &Path) -> ToolOutput {
        let field = |key| input.get(key).and_then(|v| v.as_str()).unwrap_or("").trim();
        let (command, description) = (field("command"), field("description"));

        if command.is_empty() || description.is_empty() {
            return ToolOutput::error("Missing required parameter: command and description");
        }

        let host = input.get("host").and_then(|v| v.as_bool()).unwrap_or(false);

        match start(&self.monitors, &*self.sandbox, cwd, command, description, host, None).await {
            Ok(id) => ToolOutput::success(format!(
                "Monitor {id} started. Its output will reach you as messages."
            )),
            Err(e) => ToolOutput::error(format!("Failed to start monitor: {e}")),
        }
    }
}

/// Start a monitor where `host` says: on the user's machine or in the sandbox.
pub async fn start(
    monitors: &Monitors,
    sandbox: &dyn Sandbox,
    cwd: &Path,
    command: &str,
    description: &str,
    host: bool,
    id: Option<u32>,
) -> std::io::Result<u32> {
    let plan = if host {
        host_terminal(command)
    } else {
        sandbox.terminal(command, cwd, false).await?
    };

    monitors.start(plan, cwd, command, description, host, id)
}

impl ToolDef for MonitorListTool {
    fn name(&self) -> &'static str {
        "MonitorList"
    }

    fn description(&self) -> &'static str {
        "List the running monitors."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: &serde_json::Value, _cwd: &Path) -> ToolOutput {
        let list = self.monitors.list();

        if list.is_empty() {
            return ToolOutput::success("No monitor is running.");
        }

        let mut out = String::new();
        for m in list {
            let place = if m.host { " [user's machine]" } else { "" };
            writeln!(out, "{}: {}{place}\n   {}", m.id, m.description, m.command).unwrap();
        }

        ToolOutput::success(out.trim_end().to_string())
    }
}

impl ToolDef for MonitorStopTool {
    fn name(&self) -> &'static str {
        "MonitorStop"
    }

    fn description(&self) -> &'static str {
        "Stop a monitor and kill its command."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Monitor id from Monitor" }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, input: &serde_json::Value, _cwd: &Path) -> ToolOutput {
        let Some(id) = input
            .get("id")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok())
        else {
            return ToolOutput::error("Missing required parameter: id");
        };

        match self.monitors.stop(id) {
            true => ToolOutput::success(format!(
                "Monitor {id} stopped. A process that left its process group (setsid, a \
                 daemon) is not stopped."
            )),
            false => ToolOutput::error(format!("No running monitor {id}.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::LocalShell;
    use crate::store::SharedStore;

    fn tools() -> (MonitorTool, MonitorListTool, MonitorStopTool) {
        let (monitors, _rx) = Monitors::new(SharedStore::default());
        super::tools(monitors, Arc::new(LocalShell))
    }

    #[tokio::test]
    async fn stop_on_an_unknown_id_is_an_error() {
        let (_, _, stop) = tools();

        let out = stop.execute(&serde_json::json!({ "id": 42 }), Path::new("/")).await;
        assert!(out.is_error);
        assert!(out.content.contains("No running monitor 42"));

        let out = stop.execute(&serde_json::json!({}), Path::new("/")).await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn list_drops_a_stopped_monitor() {
        let (start, list, stop) = tools();
        let input = |cmd: &str, what: &str| serde_json::json!({ "command": cmd, "description": what });

        let a = start.execute(&input("sleep 30", "first"), Path::new("/")).await;
        let b = start.execute(&input("sleep 30", "second"), Path::new("/")).await;
        assert!(a.content.starts_with("Monitor 1 started"), "{}", a.content);
        assert!(b.content.starts_with("Monitor 2 started"), "{}", b.content);

        stop.execute(&serde_json::json!({ "id": 1 }), Path::new("/")).await;

        let out = list.execute(&serde_json::json!({}), Path::new("/")).await.content;
        assert!(!out.contains("first") && out.contains("2: second"), "{out}");

        stop.execute(&serde_json::json!({ "id": 2 }), Path::new("/")).await;
        let out = list.execute(&serde_json::json!({}), Path::new("/")).await.content;
        assert_eq!(out, "No monitor is running.");
    }

    #[tokio::test]
    async fn start_needs_a_command_and_a_description() {
        let (start, _, _) = tools();

        let out = start
            .execute(&serde_json::json!({ "command": "true" }), Path::new("/"))
            .await;
        assert!(out.is_error);
    }
}
