use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;

use super::{ToolDef, ToolOutput};
use crate::fs::{EntryKind, Fs};

pub struct ListTool {
    fs: Arc<dyn Fs>,
}

impl ListTool {
    pub fn new(fs: Arc<dyn Fs>) -> Self {
        Self { fs }
    }
}

impl ToolDef for ListTool {
    fn name(&self) -> &'static str {
        "List"
    }

    fn description(&self) -> &'static str {
        "List directory contents. Returns file names with type indicators (/ for directories, \
         @ for symlinks). Use this instead of `ls` via Bash."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The directory to list (defaults to working directory)"
                }
            }
        })
    }

    async fn execute(&self, input: &serde_json::Value, cwd: &Path) -> ToolOutput {
        let dir = match input.get("path").and_then(|p| p.as_str()) {
            Some(p) if Path::new(p).is_absolute() => Path::new(p).to_path_buf(),
            Some(p) => cwd.join(p),
            None => cwd.to_path_buf(),
        };

        if !self.fs.is_dir(&dir).await {
            return ToolOutput::error(format!("Not a directory: {}", dir.display()));
        }

        let listing = match self.fs.read_dir(&dir).await {
            Ok(l) => l,
            Err(e) => return ToolOutput::error(format!("Failed to read directory: {e}")),
        };

        let mut entries: Vec<(String, &str)> = listing
            .into_iter()
            // Skip hidden files
            .filter(|entry| !entry.name.starts_with('.'))
            .map(|entry| {
                let suffix = match entry.kind {
                    EntryKind::Dir => "/",
                    EntryKind::Symlink => "@",
                    EntryKind::File => "",
                };

                (entry.name, suffix)
            })
            .collect();

        if entries.is_empty() {
            return ToolOutput::success("(empty directory)");
        }

        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut out = String::new();

        for (name, suffix) in &entries {
            writeln!(out, "{name}{suffix}").unwrap();
        }

        // Remove trailing newline
        out.pop();

        ToolOutput::success(out)
    }
}
