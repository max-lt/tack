use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::{ToolDef, ToolOutput};
use crate::sandbox::{ExecRequest, Sandbox};

pub struct BashTool {
    sandbox: Arc<dyn Sandbox>,
}

impl BashTool {
    pub fn new(sandbox: Arc<dyn Sandbox>) -> Self {
        Self { sandbox }
    }
}

impl ToolDef for BashTool {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn description(&self) -> &'static str {
        "Executes a bash command. Use for running programs, installing packages, git operations, \
         builds, and other terminal tasks. Do NOT use for reading or writing files: use the \
         Read and Write tools instead."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional timeout in milliseconds (max 600000, default 120000)"
                },
                "description": {
                    "type": "string",
                    "description": "A short description of what this command does"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: &serde_json::Value, cwd: &Path) -> ToolOutput {
        let command = match input.get("command").and_then(|c| c.as_str()) {
            Some(c) => c,
            None => return ToolOutput::error("Missing required parameter: command"),
        };

        let timeout_ms = input
            .get("timeout")
            .and_then(|t| t.as_u64())
            .unwrap_or(120_000)
            .min(600_000);

        let request = ExecRequest {
            command,
            workspace: cwd,
            timeout: Duration::from_millis(timeout_ms),
        };

        let output = match self.sandbox.exec(request).await {
            Ok(output) => output,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                return ToolOutput::error(format!("Command timed out after {timeout_ms}ms"));
            }
            Err(e) => return ToolOutput::error(format!("Failed to execute command: {e}")),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut content = String::new();

        if !stdout.is_empty() {
            content.push_str(&stdout);
        }

        if !stderr.is_empty() {
            if !content.is_empty() {
                content.push('\n');
            }

            content.push_str("stderr:\n");
            content.push_str(&stderr);
        }

        if content.is_empty() {
            content.push_str("(no output)");
        }

        let result = match output.exit_code {
            Some(0) => ToolOutput::success(content),
            code => {
                let code = code.unwrap_or(-1);
                ToolOutput::error(format!("Exit code {code}\n{content}"))
            }
        };

        result.with_changes(output.changes)
    }
}
