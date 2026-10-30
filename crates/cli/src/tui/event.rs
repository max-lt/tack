use std::sync::mpsc as std_mpsc;

use tokio::sync::mpsc;

use tack_core::api::Usage;
use tack_core::event::EventHandler;
use tack_core::lmstudio::LmModel;

/// Events sent from the session task to the UI.
pub enum UiEvent {
    Text(String),
    /// Reasoning tokens, streamed separately from the answer.
    Thinking(String),
    Error(String),
    ToolStart {
        name: String,
        input: serde_json::Value,
    },
    ToolExecuting {
        input: serde_json::Value,
    },
    ToolResult {
        output: String,
        is_error: bool,
    },
    ToolEnd,
    /// Turn finished: tokens billed, and the conversation size it left behind.
    Done {
        usage: Usage,
        context_used: u64,
    },
    Failed(String),
    /// Outcome of probing LM Studio: its models, or why it is not usable.
    LmStudio(Result<Vec<LmModel>, String>),
    PermissionRequest {
        description: String,
        respond: std_mpsc::SyncSender<bool>,
    },
    /// The session handled /clear. Monitor events from ids below
    /// `first_monitor_id` belong to the old conversation. 0 when the clear
    /// failed and the old conversation goes on.
    Cleared { first_monitor_id: u32 },
    /// A note for the user from the session task.
    Info(String),
    /// Output of a monitor, or its end.
    Monitor(tack_core::monitor::MonitorEvent),
    /// Events drained from the crosstalk daemon, as it printed them.
    #[cfg(feature = "crosstalk")]
    Crosstalk(String),
    /// Reply of a `/crosstalk` command.
    #[cfg(feature = "crosstalk")]
    CrosstalkReply(String),
}

/// Commands sent from the UI to the session task.
pub enum SessionCmd {
    SendMessage(String),
    SetModel(String),
    Clear,
    Stop,
}

/// Bridges `EventHandler` trait calls into `UiEvent` channel sends.
pub struct ChannelEventHandler {
    pub tx: mpsc::UnboundedSender<UiEvent>,
}

impl EventHandler for ChannelEventHandler {
    fn on_text(&mut self, text: &str) {
        let _ = self.tx.send(UiEvent::Text(text.to_string()));
    }

    fn on_thinking(&mut self, text: &str) {
        let _ = self.tx.send(UiEvent::Thinking(text.to_string()));
    }

    fn on_error(&mut self, message: &str) {
        let _ = self.tx.send(UiEvent::Error(message.to_string()));
    }

    fn on_tool_use_start(&mut self, name: &str, _id: &str, input: &serde_json::Value) {
        let _ = self.tx.send(UiEvent::ToolStart {
            name: name.to_string(),
            input: input.clone(),
        });
    }

    fn on_tool_executing(&mut self, _name: &str, input: &serde_json::Value) {
        let _ = self.tx.send(UiEvent::ToolExecuting {
            input: input.clone(),
        });
    }

    fn on_tool_result(&mut self, _name: &str, output: &str, is_error: bool) {
        let _ = self.tx.send(UiEvent::ToolResult {
            output: output.to_string(),
            is_error,
        });
    }

    fn on_tool_use_end(&mut self, _name: &str) {
        let _ = self.tx.send(UiEvent::ToolEnd);
    }
}
