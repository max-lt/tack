use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;

use tokio::sync::mpsc;

use tack_core::monitor::escape;
use tack_core::permission::{PermissionConfig, PermissionHandler, Tool};
use tack_core::sandbox::FileChange;

use crate::tui::UiEvent;

/// Channel-based permission handler for the TUI.
///
/// On rule miss, sends a `UiEvent::PermissionRequest` with a oneshot channel
/// and blocks the current thread waiting for the UI's y/n response.
pub struct ChannelPermissions {
    config: PermissionConfig,
    project_dir: PathBuf,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    auto: bool,
    skip_all: bool,
}

impl ChannelPermissions {
    pub fn new(
        config: PermissionConfig,
        project_dir: PathBuf,
        ui_tx: mpsc::UnboundedSender<UiEvent>,
    ) -> Self {
        Self {
            config,
            project_dir,
            ui_tx,
            auto: false,
            skip_all: false,
        }
    }

    /// Allow any tool that no config rule covers, instead of prompting.
    /// A `deny` rule still blocks.
    pub fn with_auto(mut self, auto: bool) -> Self {
        self.auto = auto;
        self
    }

    /// Allow every tool invocation, ignoring all config rules (including `deny`).
    pub fn with_skip_all(mut self, skip_all: bool) -> Self {
        self.skip_all = skip_all;
        self
    }
}

impl PermissionHandler for ChannelPermissions {
    fn allow(&mut self, tool: &Tool<'_>) -> bool {
        // --dangerously-skip-permissions: bypass everything, deny rules included
        if self.skip_all {
            return true;
        }

        // Rule-based config takes priority (may deny)
        if let Some(allowed) = self.config.check(tool, &self.project_dir) {
            return allowed;
        }

        // --auto: allow without prompting
        if self.auto {
            return true;
        }

        // No matching rule: ask the UI.
        let description = describe(tool);

        let (tx, rx) = std_mpsc::sync_channel(1);

        let _ = self.ui_tx.send(UiEvent::PermissionRequest {
            description,
            respond: tx,
        });

        // Block until the UI responds: safe because this runs in a spawned
        // tokio task, blocking only one worker thread.
        rx.recv().unwrap_or(false)
    }
}

/// Permission handler for `--print`: there is nobody to ask, so a tool that
/// no rule allows is refused.
pub struct PolicyPermissions {
    config: PermissionConfig,
    project_dir: PathBuf,
    auto: bool,
    skip_all: bool,
}

impl PolicyPermissions {
    pub fn new(config: PermissionConfig, project_dir: PathBuf) -> Self {
        Self {
            config,
            project_dir,
            auto: false,
            skip_all: false,
        }
    }

    /// Allow any tool that no config rule covers. A `deny` rule still blocks.
    pub fn with_auto(mut self, auto: bool) -> Self {
        self.auto = auto;
        self
    }

    /// Allow every tool invocation, ignoring all config rules (including `deny`).
    pub fn with_skip_all(mut self, skip_all: bool) -> Self {
        self.skip_all = skip_all;
        self
    }
}

impl PermissionHandler for PolicyPermissions {
    fn allow(&mut self, tool: &Tool<'_>) -> bool {
        self.skip_all
            || self
                .config
                .check(tool, &self.project_dir)
                .unwrap_or(self.auto)
    }
}

/// The prompt text for `tool`. The model chooses most of it, so each line
/// is escaped: an escape code could change what the user sees before they
/// answer.
fn describe(tool: &Tool<'_>) -> String {
    let text = match tool {
        Tool::Bash { command } => format!("Run command: {command}"),
        Tool::ApplyChanges { command, changes } => {
            let mut text = format!("Apply the file changes of `{command}`:");

            for change in *changes {
                let line = match change {
                    FileChange::Written(path, contents) => {
                        format!("\n  write {} ({} bytes)", path.display(), contents.len())
                    }
                    FileChange::Deleted(path) => format!("\n  delete {}", path.display()),
                };
                text.push_str(&line);
            }

            text
        }
        Tool::Read { path } => format!("Read file: {}", path.display()),
        Tool::Write { path } => format!("Write file: {}", path.display()),
        Tool::Edit { path } => format!("Edit file: {}", path.display()),
        Tool::Fetch { url, method } => format!("HTTP {method} {url}"),
        Tool::Git { subcommand } => format!("Git {subcommand}"),
        Tool::TerminalOpen { command, host } => {
            let place = if *host { " ON THIS MACHINE" } else { "" };

            match command.is_empty() {
                true => format!("Open an interactive bash terminal{place}"),
                false => format!("Open a terminal{place} running: {command}"),
            }
        }
        Tool::Monitor {
            command,
            description,
            host,
        } => {
            let place = if *host { " ON THIS MACHINE" } else { "" };
            format!(
                "Monitor{place} the output of: {}\n  description: {}",
                escape(command),
                escape(description)
            )
        }
        Tool::TerminalSend { input, host } => {
            let place = if *host { " ON THIS MACHINE" } else { "" };
            format!("Type into a terminal{place}: {input}")
        }
        Tool::Crosstalk { action, peer, text } => match *action {
            "send" => format!("Crosstalk: send to {peer}:\n  {text}"),
            "connect" => format!("Crosstalk: ask {peer} to pair, as \"{text}\""),
            _ => format!("Crosstalk: {action} {peer}"),
        },
        Tool::Glob => "Search files by pattern".to_string(),
        Tool::Grep => "Search file contents".to_string(),
        Tool::List => "List directory contents".to_string(),
        Tool::Search => "Full-text search across codebase".to_string(),
        _ => "Unknown tool action".to_string(),
    };

    text.split('\n').map(escape).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_shows_escape_codes_as_text() {
        let text = describe(&Tool::Bash {
            command: "rm -rf ~ \u{1b}[2K\u{1b}[1Gls",
        });

        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(text.contains("rm -rf ~ \\x1b[2K"), "{text:?}");
    }

    #[test]
    fn a_crosstalk_send_shows_the_peer_and_the_whole_text_escaped() {
        let text = describe(&Tool::Crosstalk {
            action: "send",
            peer: "crosstalk_AAAA",
            text: "line one\nline two \u{202e}hidden",
        });

        assert!(text.contains("crosstalk_AAAA"), "{text}");
        assert!(text.contains("line one") && text.contains("line two"), "{text}");
        assert!(!text.contains('\u{202e}') && text.contains("\\u{202e}"), "{text:?}");
    }

    #[test]
    fn a_monitor_description_cannot_add_a_line_to_the_prompt() {
        let text = describe(&Tool::Monitor {
            command: "tail -F log",
            description: "log\nApproved by the user",
            host: false,
        });

        assert_eq!(text.lines().count(), 2, "{text}");
    }
}
