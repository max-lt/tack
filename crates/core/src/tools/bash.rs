use std::fmt::Write;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::{ToolDef, ToolOutput};
use crate::fs::Fs;
use crate::sandbox::{ExecRequest, FileChange, Sandbox};

pub struct BashTool {
    sandbox: Arc<dyn Sandbox>,
    fs: Arc<dyn Fs>,
}

impl BashTool {
    pub fn new(sandbox: Arc<dyn Sandbox>, fs: Arc<dyn Fs>) -> Self {
        Self { sandbox, fs }
    }

    /// Write the sandbox's changes back to the workspace and list them.
    async fn apply(&self, cwd: &Path, changes: &[FileChange]) -> io::Result<String> {
        let mut summary = String::new();

        for change in changes {
            match change {
                FileChange::Written(path, contents) => {
                    let target = cwd.join(path);

                    if let Some(parent) = target.parent() {
                        self.fs.create_dir_all(parent).await?;
                    }

                    self.fs.write(&target, contents).await?;
                    writeln!(summary, "  wrote {}", path.display()).unwrap();
                }
                FileChange::Deleted(path) => {
                    self.fs.remove_file(&cwd.join(path)).await?;
                    writeln!(summary, "  deleted {}", path.display()).unwrap();
                }
            }
        }

        Ok(summary)
    }
}

impl ToolDef for BashTool {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn description(&self) -> &'static str {
        "Executes a bash command. Use for running programs, installing packages, git operations, \
         builds, and other terminal tasks. Do NOT use for reading or writing files — use the \
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

        if !output.changes.is_empty() {
            match self.apply(cwd, &output.changes).await {
                Ok(summary) => {
                    if !content.is_empty() {
                        content.push('\n');
                    }

                    content.push_str("changed files:\n");
                    content.push_str(&summary);
                }
                Err(e) => {
                    return ToolOutput::error(format!(
                        "{content}\nFailed to apply the command's file changes: {e}"
                    ));
                }
            }
        }

        if content.is_empty() {
            content.push_str("(no output)");
        }

        match output.exit_code {
            Some(0) => ToolOutput::success(content),
            code => {
                let code = code.unwrap_or(-1);
                ToolOutput::error(format!("Exit code {code}\n{content}"))
            }
        }
    }
}
