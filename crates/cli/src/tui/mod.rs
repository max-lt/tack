mod event;
mod markdown;
mod render;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use tack_core::api::{
    AVAILABLE_MODELS, Content, ContentBlock, DEFAULT_MODEL, Message, Usage,
};
use tack_core::history;
use tack_core::lmstudio::{self, LmModel};
use tack_core::provider::{context_window_for, lm_studio_model_id};
use tack_core::session::Session;

#[cfg(feature = "crosstalk")]
use tack_core::crosstalk::{self, Client as Crosstalk};

use crate::commands::{self, CommandResult};
use tack_core::permission::PermissionHandler;

pub use event::{ChannelEventHandler, SessionCmd, UiEvent};

/// Set while the TUI owns the terminal: raw mode, alternate screen, mouse
/// capture. Only then must an exit restore the terminal.
pub static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Lines that PageUp and PageDown scroll.
const PAGE_LINES: u16 = 10;

/// DECSET 1007: in the alternate screen, the terminal sends the wheel as
/// arrow keys. The terminal keeps the mouse, so text selection works.
pub(crate) struct AlternateScroll(pub(crate) bool);

impl crossterm::Command for AlternateScroll {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(if self.0 { "\x1b[?1007h" } else { "\x1b[?1007l" })
    }
}

// ---------------------------------------------------------------------------
// Display model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum AppState {
    Idle,
    Busy,
}

impl AppState {
    fn is_busy(&self) -> bool {
        matches!(self, Self::Busy)
    }
}

/// What is known about the local LM Studio server.
#[derive(Clone, PartialEq)]
pub enum LmStatus {
    /// Never probed.
    Unknown,
    Checking,
    /// Unreachable; carries the reason.
    Down(String),
    Up,
}

/// One row of the model picker.
pub struct PickerEntry {
    pub id: String,
    pub label: String,
    /// Shown dimmed after the label.
    pub detail: String,
    pub is_default: bool,
    /// Overrides the table lookup, for models whose window is only known live.
    pub context_window: Option<u64>,
}

pub struct PendingPermission {
    pub description: String,
    pub respond: std_mpsc::SyncSender<bool>,
}

pub enum DisplayMessage {
    User(String),
    AssistantText(String),
    Thinking(String),
    ToolUse {
        name: String,
        input: Option<serde_json::Value>,
        output: Option<String>,
        is_error: bool,
    },
    Error(String),
    Info(String),
}

/// Rebuild what the screen showed from a saved conversation.
/// Thinking blocks are not stored, so they do not come back.
pub fn display_history(messages: &[Message]) -> Vec<DisplayMessage> {
    let mut out = Vec::new();
    // tool_use id -> index of its DisplayMessage, to attach the result later.
    let mut tool_slots = std::collections::HashMap::new();

    for msg in messages {
        let is_user = msg.role == "user";

        let blocks = match &msg.content {
            Content::Text(text) => {
                out.push(if is_user {
                    DisplayMessage::User(text.clone())
                } else {
                    DisplayMessage::AssistantText(text.clone())
                });
                continue;
            }
            Content::Blocks(blocks) => blocks,
        };

        for block in blocks {
            match block {
                ContentBlock::Text { text } => out.push(if is_user {
                    DisplayMessage::User(text.clone())
                } else {
                    DisplayMessage::AssistantText(text.clone())
                }),

                ContentBlock::ToolUse { id, name, input } => {
                    tool_slots.insert(id.clone(), out.len());
                    out.push(DisplayMessage::ToolUse {
                        name: name.clone(),
                        input: Some(input.clone()),
                        output: None,
                        is_error: false,
                    });
                }

                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    if let Some(&slot) = tool_slots.get(tool_use_id)
                        && let Some(DisplayMessage::ToolUse {
                            output, is_error: err, ..
                        }) = out.get_mut(slot)
                    {
                        *output = Some(content.clone());
                        *err = is_error.unwrap_or(false);
                    }
                }
            }
        }
    }

    out
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

pub struct App {
    pub cwd: PathBuf,
    pub model: String,
    pub usage: Usage,
    pub context_window: u64,
    /// Conversation size after the last turn, in tokens.
    pub context_used: u64,
    pub messages: Vec<DisplayMessage>,
    pub scroll: u16,
    pub auto_scroll: bool,
    /// The wheel scrolls the conversation. F2 turns it off so that the
    /// terminal can select text.
    pub mouse_capture: bool,
    pub max_scroll: u16,
    pub input: String,
    pub cursor: usize,
    pub state: AppState,
    pub pending_perm: Option<PendingPermission>,
    /// Index into `AVAILABLE_MODELS` while the model picker is open.
    pub model_picker: Option<usize>,
    /// Messages typed while a turn is running, sent in order after it ends.
    pub queue: VecDeque<String>,
    /// Notices for the model (monitor output, crosstalk messages) that
    /// came during a turn. Not the user's text: a stop or an error does not
    /// hand them back to the input. They go after the user's queue, once a
    /// turn ends without an error.
    notices: VecDeque<String>,
    /// Monitor events below this id belong to a cleared conversation.
    /// `None` between a /clear and the session's answer: all are dropped.
    first_monitor_id: Option<u32>,
    /// Stop was pressed during the current turn. The queue is then handed
    /// back to the input instead of being sent.
    stopping: bool,
    /// A queued `/quit` ran after a turn ended; the main loop exits.
    quit_requested: bool,
    /// Models last reported by LM Studio.
    pub lm_models: Vec<LmModel>,
    pub lm_status: LmStatus,
    pub spinner_frame: usize,
    pub last_spinner_update: Instant,
    /// Resident memory of this process, in bytes. `None` if the OS refused.
    pub memory: Option<usize>,
    last_memory_update: Instant,
    ui_rx: mpsc::UnboundedReceiver<UiEvent>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    session_tx: mpsc::UnboundedSender<SessionCmd>,
    #[cfg(feature = "crosstalk")]
    crosstalk: Option<Arc<Crosstalk>>,
    /// How many crosstalk messages may still wake the model.
    #[cfg(feature = "crosstalk")]
    crosstalk_budget: crosstalk::Budget,
}

impl App {
    fn new(
        cwd: PathBuf,
        model: String,
        restored: Vec<DisplayMessage>,
        ui_rx: mpsc::UnboundedReceiver<UiEvent>,
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        session_tx: mpsc::UnboundedSender<SessionCmd>,
    ) -> Self {
        let context_window = context_window_for(&model);

        let mut messages = vec![DisplayMessage::Info(
            "Type your message to start.\nWhile it is working, Enter queues a follow-up.\nBackspace on an empty line pops the last one out. Ctrl+C to exit.".to_string(),
        )];

        if !restored.is_empty() {
            messages.push(DisplayMessage::Info("Resumed previous session.".to_string()));
            messages.extend(restored);
        }

        Self {
            cwd,
            model,
            usage: Usage::default(),
            context_window,
            context_used: 0,
            messages,
            scroll: 0,
            auto_scroll: true,
            mouse_capture: true,
            max_scroll: 0,
            input: String::new(),
            cursor: 0,
            state: AppState::Idle,
            pending_perm: None,
            model_picker: None,
            queue: VecDeque::new(),
            notices: VecDeque::new(),
            first_monitor_id: Some(0),
            stopping: false,
            quit_requested: false,
            lm_models: Vec::new(),
            lm_status: LmStatus::Unknown,
            spinner_frame: 0,
            last_spinner_update: Instant::now(),
            memory: current_memory(),
            last_memory_update: Instant::now(),
            ui_rx,
            ui_tx,
            session_tx,
            #[cfg(feature = "crosstalk")]
            crosstalk: None,
            #[cfg(feature = "crosstalk")]
            crosstalk_budget: crosstalk::Budget::default(),
        }
    }

    /// Everything the picker offers: the built-in models, then LM Studio's.
    pub fn picker_entries(&self) -> Vec<PickerEntry> {
        let mut entries: Vec<PickerEntry> = AVAILABLE_MODELS
            .iter()
            .map(|m| PickerEntry {
                id: m.id.to_string(),
                label: m.label.to_string(),
                detail: m.id.to_string(),
                is_default: m.id == DEFAULT_MODEL,
                context_window: None,
            })
            .collect();

        entries.extend(self.lm_models.iter().map(|m| {
            let state = match m.loaded {
                Some(true) => "loaded",
                Some(false) => "not loaded",
                None => "",
            };

            PickerEntry {
                id: lm_studio_model_id(&m.id),
                label: format!("[lm-studio] {}", m.id),
                detail: state.to_string(),
                is_default: false,
                // Only a loaded model has a window worth trusting.
                context_window: m.context_length.filter(|_| m.loaded != Some(false)),
            }
        }));

        entries
    }

    /// Probe LM Studio in the background; the answer comes back as a `UiEvent`.
    fn probe_lm_studio(&mut self) {
        self.lm_status = LmStatus::Checking;

        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            let result = lmstudio::list_models().await.map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::LmStudio(result));
        });
    }
    // -- Key handling -------------------------------------------------------

    /// Returns `true` if the app should quit.
    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        // Ctrl+C: stop Claude if busy, quit if idle
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.state == AppState::Busy {
                self.stop();
                return false;
            } else {
                return true;
            }
        }

        // Esc: stop Claude if busy, do nothing if idle
        if key.code == KeyCode::Esc {
            if self.state == AppState::Busy {
                self.stop();
                return false;
            }
        }

        // Permission prompt captures y/n
        if self.pending_perm.is_some() {
            return self.handle_perm_key(key.code);
        }

        // Model picker captures navigation keys
        if self.model_picker.is_some() {
            self.handle_picker_key(key.code);
            return false;
        }

        match key.code {
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    if self.state.is_busy() {
                        // Queue it: the session sends it when the current
                        // turn ends.
                        self.queue.push_back(self.input.trim_end().to_string());
                        self.input.clear();
                        self.cursor = 0;
                        self.auto_scroll = true;
                        return false;
                    }
                    return self.submit_input();
                }
            }

            KeyCode::Char(c) => {
                let byte_pos = self
                    .input
                    .char_indices()
                    .nth(self.cursor)
                    .map(|(i, _)| i)
                    .unwrap_or(self.input.len());
                self.input.insert(byte_pos, c);
                self.cursor += 1;
            }

            KeyCode::Backspace => {
                if self.input.is_empty() {
                    // Input is empty: pop the last queued message back for
                    // editing.
                    if let Some(text) = self.queue.pop_back() {
                        self.input = text;
                        self.cursor = self.input.chars().count();
                    }
                    return false;
                }
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let byte_pos = self
                        .input
                        .char_indices()
                        .nth(self.cursor)
                        .map(|(i, _)| i)
                        .unwrap_or(self.input.len());
                    self.input.remove(byte_pos);
                }
            }

            KeyCode::Delete => {
                if self.input.is_empty() {
                    if let Some(text) = self.queue.pop_front() {
                        self.input = text;
                        self.cursor = 0;
                    }
                } else if self.cursor < self.input.chars().count() {
                    let byte_pos = self
                        .input
                        .char_indices()
                        .nth(self.cursor)
                        .map(|(i, _)| i)
                        .unwrap_or(self.input.len());
                    self.input.remove(byte_pos);
                }
            }

            KeyCode::Tab => {
                // Complete the first matching slash command
                if let Some(name) = first_suggestion(&self.input) {
                    self.input = format!("{name} ");
                    self.cursor = self.input.chars().count();
                }
            }

            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
            }

            KeyCode::Right => {
                if self.cursor < self.input.chars().count() {
                    self.cursor += 1;
                }
            }

            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),

            KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                self.auto_scroll = false;
            }

            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1).min(self.max_scroll);
                self.auto_scroll = self.scroll >= self.max_scroll;
            }

            KeyCode::F(2) => self.mouse_capture = !self.mouse_capture,

            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(PAGE_LINES);
                self.auto_scroll = false;
            }

            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(PAGE_LINES).min(self.max_scroll);
                self.auto_scroll = self.scroll >= self.max_scroll;
            }

            _ => {}
        }

        false
    }

    fn handle_perm_key(&mut self, code: KeyCode) -> bool {
        let respond = match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
            _ => None,
        };

        if let Some(allowed) = respond
            && let Some(perm) = self.pending_perm.take()
        {
            let _ = perm.respond.send(allowed);
        }

        false
    }

    fn handle_picker_key(&mut self, code: KeyCode) {
        let Some(selected) = self.model_picker else {
            return;
        };
        let entries = self.picker_entries();
        let count = entries.len();
        // The list can shrink under the cursor when a probe comes back empty.
        let selected = selected.min(count.saturating_sub(1));

        match code {
            KeyCode::Up => {
                self.model_picker = Some((selected + count - 1) % count);
            }
            KeyCode::Down | KeyCode::Tab => {
                self.model_picker = Some((selected + 1) % count);
            }
            KeyCode::Enter => {
                self.model_picker = None;
                let Some(entry) = entries.get(selected) else {
                    return;
                };

                self.set_model(entry.id.clone(), entry.label.clone());

                if let Some(window) = entry.context_window {
                    self.context_window = window;
                }

                self.resume_queue();
            }
            KeyCode::Esc => {
                self.model_picker = None;
                self.resume_queue();
            }
            _ => {}
        }
    }

    fn set_model(&mut self, id: String, label: String) {
        self.context_window = context_window_for(&id);
        let _ = self.session_tx.send(SessionCmd::SetModel(id.clone()));
        self.model = id;
        self.messages
            .push(DisplayMessage::Info(format!("Switched to {label}.")));
    }

    /// Process the input line: slash command or message. Returns `true` to quit.
    fn submit_input(&mut self) -> bool {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.submit_text(text)
    }

    /// Run one entry, typed or queued: a slash command, a `!` command, or a
    /// message for the model. Returns `true` to quit.
    fn submit_text(&mut self, text: String) -> bool {
        // Slash commands
        if let Some(result) = commands::handle_command(&text, &self.model) {
            match result {
                CommandResult::Exit => return true,

                CommandResult::Clear => {
                    let _ = self.session_tx.send(SessionCmd::Clear);
                    // Notices of the old conversation must not reach the
                    // new one, also those still in the channel.
                    self.notices.clear();
                    self.first_monitor_id = None;
                    self.context_used = 0;
                    self.messages.clear();
                    self.messages
                        .push(DisplayMessage::Info("Conversation cleared.".to_string()));
                }

                CommandResult::SetModel { id, label } => {
                    self.set_model(id, label);
                }

                CommandResult::PickModel => {
                    // Open at once; LM Studio's models join the list when the
                    // probe answers, after the built-in ones, so indices hold.
                    self.probe_lm_studio();

                    let current = self
                        .picker_entries()
                        .iter()
                        .position(|e| e.id == self.model)
                        .unwrap_or(0);
                    self.model_picker = Some(current);
                }

                CommandResult::Info(info) => {
                    self.messages.push(DisplayMessage::Info(info));
                }

                #[cfg(feature = "crosstalk")]
                CommandResult::Crosstalk(args) => self.crosstalk_command(args),
            }

            return false;
        }

        // Regular message
        self.messages.push(DisplayMessage::User(text.clone()));
        self.state = AppState::Busy;
        self.auto_scroll = true;
        let _ = self.session_tx.send(SessionCmd::SendMessage(text));

        false
    }

    // -- UI event handling --------------------------------------------------

    fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Text(text) => {
                if let Some(DisplayMessage::AssistantText(existing)) = self.messages.last_mut() {
                    existing.push_str(&text);
                } else {
                    self.messages.push(DisplayMessage::AssistantText(text));
                }
            }

            UiEvent::Thinking(text) => {
                if let Some(DisplayMessage::Thinking(existing)) = self.messages.last_mut() {
                    existing.push_str(&text);
                } else {
                    self.messages.push(DisplayMessage::Thinking(text));
                }
            }

            UiEvent::Error(msg) => {
                self.messages.push(DisplayMessage::Error(msg));
            }

            UiEvent::ToolStart { name, input } => {
                self.messages.push(DisplayMessage::ToolUse {
                    name,
                    input: Some(input),
                    output: None,
                    is_error: false,
                });
            }

            UiEvent::ToolExecuting { input } => {
                if let Some(DisplayMessage::ToolUse { input: inp, .. }) = self.messages.last_mut() {
                    *inp = Some(input);
                }
            }

            UiEvent::ToolResult { output, is_error } => {
                if let Some(DisplayMessage::ToolUse {
                    output: out,
                    is_error: err,
                    ..
                }) = self.messages.last_mut()
                {
                    *out = Some(output);
                    *err = is_error;
                }
            }

            UiEvent::ToolEnd => {}

            UiEvent::Done {
                usage,
                context_used,
            } => {
                self.usage.add(&usage);
                self.context_used = context_used;

                // Stop can lose the race to a turn that was already ending:
                // the user still asked to stop, so do not send the queue.
                if std::mem::take(&mut self.stopping) {
                    self.hand_back_queue();
                } else {
                    self.drain_queue();
                }
            }

            UiEvent::Failed(msg) => {
                self.messages.push(DisplayMessage::Error(msg));
                // Stopped, or an API error: either way, do not fire the queue
                // into a broken or unwanted state.
                self.stopping = false;
                self.hand_back_queue();
            }

            UiEvent::LmStudio(result) => match result {
                Ok(models) => {
                    self.lm_models = models;
                    self.lm_status = LmStatus::Up;
                }
                Err(reason) => {
                    self.lm_models.clear();
                    self.lm_status = LmStatus::Down(reason);
                }
            },

            UiEvent::PermissionRequest {
                description,
                respond,
            } => {
                self.pending_perm = Some(PendingPermission {
                    description,
                    respond,
                });
            }

            UiEvent::Info(note) => self.messages.push(DisplayMessage::Info(note)),

            UiEvent::Monitor(event) => {
                if self.first_monitor_id.is_some_and(|first| event.id >= first) {
                    self.deliver_notice(event.notice());
                }
            }

            UiEvent::Cleared { first_monitor_id } => {
                self.first_monitor_id = Some(first_monitor_id);
            }

            #[cfg(feature = "crosstalk")]
            UiEvent::Crosstalk(raw) => self.crosstalk_events(raw),

            #[cfg(feature = "crosstalk")]
            UiEvent::CrosstalkReply(reply) => {
                self.messages.push(DisplayMessage::Info(reply));
            }
        }
    }

    /// Run `/crosstalk <cmd> [args]` against the daemon. These are the
    /// commands the model has no tool for: pairing is the user's decision.
    #[cfg(feature = "crosstalk")]
    fn crosstalk_command(&mut self, mut args: Vec<String>) {
        let Some(client) = self.crosstalk.clone() else {
            self.messages
                .push(DisplayMessage::Error("Crosstalk is off.".to_string()));
            return;
        };

        if args.is_empty() {
            args.push("whoami".to_string());
        }

        match (args[0].as_str(), args.len()) {
            ("accept" | "connect", 2) => args.push("tack session".to_string()),
            ("accept" | "connect", n) if n > 3 => {
                let description = args.split_off(2).join(" ");
                args.push(description);
            }
            ("forget", 1) => {
                self.messages.push(DisplayMessage::Error(
                    "Name a peer, or --all to drop every peer and the identity.".to_string(),
                ));
                return;
            }
            ("forget", 2) if args[1] == "--all" => {
                args.pop();
            }
            _ => {}
        }

        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            let (cmd, rest) = args.split_first().expect("args is not empty");
            let rest: Vec<&str> = rest.iter().map(String::as_str).collect();

            let reply = match client.request(cmd, &rest).await {
                Ok(reply) => reply,
                Err(e) => format!("{e:#}"),
            };
            let _ = tx.send(UiEvent::CrosstalkReply(reply));
        });
    }

    /// Show drained crosstalk events, and hand all but pairing requests to
    /// the model as a notice. A pairing request is for the user alone.
    #[cfg(feature = "crosstalk")]
    fn crosstalk_events(&mut self, raw: String) {
        let (intros, others): (Vec<_>, Vec<_>) = crosstalk::parse_events(&raw)
            .into_iter()
            .partition(|e| e.kind == "intro");

        // A stranger wrote this text: escape it, so that bidi or invisible
        // characters cannot change what the user reads.
        let shown = |text: &str| {
            text.lines()
                .map(tack_core::escape::escape)
                .collect::<Vec<_>>()
                .join("\n")
        };

        for intro in &intros {
            self.messages.push(DisplayMessage::Info(format!(
                "{}\n{}\nAccept with: /crosstalk accept <id> [description]",
                shown(&intro.header),
                shown(&intro.body)
            )));
        }

        // Each message costs a turn, and the peer chooses how many it sends.
        let admitted = self.crosstalk_budget.admit_now(others.len());
        if admitted < others.len() {
            self.messages.push(DisplayMessage::Info(format!(
                "{} crosstalk messages dropped: the limit is {} a minute and {} a session.",
                others.len() - admitted,
                crosstalk::MAX_PER_MINUTE,
                crosstalk::MAX_PER_SESSION
            )));
        }

        if admitted == 0 {
            return;
        }

        self.deliver_notice(crosstalk::notice(&others[..admitted]));
    }

    /// Hand a notice to the model: now if idle, after the turn if busy.
    fn deliver_notice(&mut self, notice: String) {
        if self.state.is_busy() {
            self.notices.push_back(notice);
        } else {
            self.auto_scroll = true;
            self.submit_text(notice);
        }
    }

    /// Picker closed while idle: carry on with whatever is still queued.
    fn resume_queue(&mut self) {
        if self.state == AppState::Idle && !self.queue.is_empty() {
            self.drain_queue();
        }
    }

    /// Ask the session to stop the current turn. The queue is not sent
    /// afterwards: it goes back to the input, see `hand_back_queue`.
    fn stop(&mut self) {
        self.stopping = true;
        let _ = self.session_tx.send(SessionCmd::Stop);
    }

    /// After a turn has ended: run queued entries in order until one starts a
    /// new turn, or the queue is empty. Slash and `!` commands run as if typed.
    fn drain_queue(&mut self) {
        self.state = AppState::Idle;

        while self.state == AppState::Idle
            && let Some(text) = self.queue.pop_front()
        {
            self.auto_scroll = true;

            if self.submit_text(text) {
                self.quit_requested = true;
                return;
            }

            // A queued `/model` opened the picker: wait for the choice, so
            // the rest of the queue runs on the model picked. Closing the
            // picker resumes the queue.
            if self.model_picker.is_some() {
                return;
            }
        }

        if self.state == AppState::Idle
            && let Some(notice) = self.notices.pop_front()
        {
            self.auto_scroll = true;
            self.submit_text(notice);
        }
    }

    /// After a stop or an error: put the queue back in the input for editing
    /// instead of sending it. Several entries are joined into one line; the
    /// input holds a single line.
    fn hand_back_queue(&mut self) {
        self.state = AppState::Idle;

        if self.queue.is_empty() {
            return;
        }

        let mut parts: Vec<String> = self.queue.drain(..).collect();

        // Keep what was being typed, after the queued text.
        if !self.input.trim().is_empty() {
            parts.push(std::mem::take(&mut self.input));
        }

        self.input = parts.join(" ");
        self.cursor = self.input.chars().count();
    }
}

/// Resident memory (RSS) of this process, in bytes.
fn current_memory() -> Option<usize> {
    memory_stats::memory_stats().map(|s| s.physical_mem)
}

/// First matching slash command for the current input, if any.
/// Mirrors the filter used to render the suggestion list.
fn first_suggestion(input: &str) -> Option<&'static str> {
    let trimmed = input.trim();

    if !trimmed.starts_with('/') || trimmed.contains(' ') {
        return None;
    }

    commands::matching_commands(trimmed)
        .first()
        .map(|cmd| cmd.name)
}

// ---------------------------------------------------------------------------
// Session background task
// ---------------------------------------------------------------------------

/// Write the conversation to disk and remember its id for the exit hint.
/// Does nothing while the session holds nothing beyond the bootstrap.
fn persist(
    session: &Session<Box<dyn PermissionHandler>>,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    last_saved: &Mutex<Option<String>>,
) {
    let Some(stored) = session.to_stored() else {
        return;
    };

    match history::save(&stored) {
        Ok(()) => {
            if let Ok(mut slot) = last_saved.lock() {
                *slot = Some(stored.id);
            }
        }
        Err(e) => {
            let _ = ui_tx.send(UiEvent::Error(format!("Could not save session: {e}")));
        }
    }
}

/// Wait on the daemon's inbox and pass every batch of events to the UI.
#[cfg(feature = "crosstalk")]
async fn crosstalk_pump(client: Arc<Crosstalk>, ui_tx: mpsc::UnboundedSender<UiEvent>) {
    let mut reported = false;

    loop {
        match client.inbox(Duration::from_secs(300)).await {
            Ok(Some(raw)) => {
                reported = false;
                if ui_tx.send(UiEvent::Crosstalk(raw)).is_err() {
                    return;
                }
            }
            Ok(None) => reported = false,
            Err(e) => {
                // Report a dead daemon once, then keep retrying quietly.
                if !std::mem::replace(&mut reported, true) {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e:#}")));
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn session_loop(
    mut session: Session<Box<dyn PermissionHandler>>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    last_saved: Arc<Mutex<Option<String>>>,
) {
    let mut handler = ChannelEventHandler { tx: ui_tx.clone() };

    // A resumed session starts its monitors again. It runs here, not before
    // the TUI, because each one asks for permission through the UI.
    if let Some(note) = session.restart_monitors().await {
        let _ = ui_tx.send(UiEvent::Info(note));
    }

    // Commands that arrived while a turn was running. The turn's future pins
    // `&mut session`, so they cannot be applied until it is dropped.
    let mut deferred = VecDeque::new();

    while let Some(cmd) = {
        match deferred.pop_front() {
            Some(cmd) => Some(cmd),
            None => cmd_rx.recv().await,
        }
    } {
        match cmd {
            SessionCmd::SendMessage(text) => {
                let cancel = CancellationToken::new();
                let token = cancel.clone();

                // Scoped so the future (and its `&mut session` borrow) is
                // dropped before the session is read back below. Commands
                // that arrive while the turn is running are held in
                // `deferred` so they can be applied right after.
                let result = {
                    let message_future = session.send_message(&text, &mut handler, &token);
                    let mut message_future = Box::pin(message_future);

                    loop {
                        tokio::select! {
                            res = &mut message_future => break res,
                            Some(cmd) = cmd_rx.recv() => {
                                match cmd {
                                    SessionCmd::Stop => cancel.cancel(),
                                    _ => deferred.push_back(cmd),
                                }
                            }
                        }
                    }
                };

                match result {
                    Ok(summary) => {
                        // Persist after every completed turn so --resume works
                        // even if the process is killed later.
                        persist(&session, &ui_tx, &last_saved);

                        let _ = ui_tx.send(UiEvent::Done {
                            usage: summary.usage,
                            context_used: summary.context_used,
                        });
                    }
                    Err(e) => {
                        let msg = e.to_string();

                        if msg == "Cancelled" {
                            let _ = ui_tx.send(UiEvent::Failed("Stopped.".to_string()));
                        } else {
                            let _ = ui_tx.send(UiEvent::Failed(msg));
                        }
                    }
                }
            }

            SessionCmd::Stop => {
                // Stop command received while idle, ignore
            }

            SessionCmd::SetModel(id) => {
                session.set_model(id);

                // The model is part of the saved session: without this, a
                // switch after the last turn would be lost on resume.
                persist(&session, &ui_tx, &last_saved);
            }

            SessionCmd::Clear => {
                let first_monitor_id = match session.clear() {
                    Ok(()) => session.next_monitor_id(),
                    Err(e) => {
                        let _ = ui_tx.send(UiEvent::Error(format!(
                            "/clear failed, and the conversation goes on (only the screen was cleared): {e:#}"
                        )));
                        0
                    }
                };
                let _ = ui_tx.send(UiEvent::Cleared { first_monitor_id });

                // The fresh session has no file yet: nothing to hint at.
                if let Ok(mut slot) = last_saved.lock() {
                    *slot = None;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(
    cwd: PathBuf,
    mut session: Session<Box<dyn PermissionHandler>>,
    restored: Vec<DisplayMessage>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    ui_rx: mpsc::UnboundedReceiver<UiEvent>,
    #[cfg(feature = "crosstalk")] crosstalk: Option<Arc<Crosstalk>>,
) -> Result<()> {
    let model = session.model().to_string();
    // A resumed session starts with the size it had when it was saved.
    let context_used = session.context_used();
    let resume_note = session.resume_note().map(str::to_string);

    if let Some(mut events) = session.take_monitor_events() {
        let tx = ui_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if tx.send(UiEvent::Monitor(event)).is_err() {
                    return;
                }
            }
        });
    }
    let app_ui_tx = ui_tx.clone();

    // Channel for UI -> session commands
    let (session_tx, session_rx) = mpsc::unbounded_channel();

    // Id of the last session written to disk, read back on exit for the
    // "resume with" hint. A resumed session is already on disk.
    let last_saved = Arc::new(Mutex::new(
        session.to_stored().map(|_| session.id().to_string()),
    ));

    // Spawn session loop in background
    tokio::spawn(session_loop(
        session,
        session_rx,
        ui_tx,
        Arc::clone(&last_saved),
    ));

    // Terminal setup
    crossterm::terminal::enable_raw_mode()?;
    ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        AlternateScroll(true),
        crossterm::event::EnableMouseCapture,
    )?;
    let mut captured = true;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    // Restore terminal on panic
    let original_hook = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        let mut stdout = std::io::stdout();
        let _ = crossterm::execute!(
            stdout,
            crossterm::event::DisableMouseCapture,
            AlternateScroll(false),
            crossterm::terminal::LeaveAlternateScreen,
        );
        let _ = crossterm::terminal::disable_raw_mode();
        original_hook(info);
    }));

    #[cfg(feature = "crosstalk")]
    if let Some(client) = crosstalk.clone() {
        tokio::spawn(crosstalk_pump(client, app_ui_tx.clone()));
    }

    let mut app = App::new(cwd, model, restored, ui_rx, app_ui_tx, session_tx);
    app.context_used = context_used;
    if let Some(note) = resume_note {
        app.messages.push(DisplayMessage::Info(note));
    }
    #[cfg(feature = "crosstalk")]
    {
        app.crosstalk = crosstalk;
    }

    // Start with a clean alternate screen
    terminal.clear()?;

    loop {
        // Update spinner frame if busy (~10 fps for spinner animation)
        if app.state == AppState::Busy
            && app.last_spinner_update.elapsed() >= Duration::from_millis(100)
        {
            app.spinner_frame = (app.spinner_frame + 1) % 10;
            app.last_spinner_update = Instant::now();
        }

        // Refresh memory usage about once per second
        if app.last_memory_update.elapsed() >= Duration::from_secs(1) {
            app.memory = current_memory();
            app.last_memory_update = Instant::now();
        }

        terminal.draw(|f| render::render(&mut app, f))?;

        // Poll crossterm events (~30 fps)
        if crossterm::event::poll(Duration::from_millis(33))? {
            match crossterm::event::read()? {
                Event::Key(key) => {
                    if app.handle_key(key) {
                        break;
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        app.scroll = app.scroll.saturating_sub(3);
                        app.auto_scroll = false;
                    }
                    MouseEventKind::ScrollDown => {
                        app.scroll = app.scroll.saturating_add(3).min(app.max_scroll);
                        app.auto_scroll = app.scroll >= app.max_scroll;
                    }
                    _ => {}
                },
                Event::Resize(_, _) => {
                    // Force full redraw after resize
                    terminal.clear()?;
                }
                _ => {}
            }
        }

        if app.mouse_capture != captured {
            captured = app.mouse_capture;
            if captured {
                crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
            } else {
                crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)?;
            }
        }

        // Drain all pending UI events (batches fast streaming)
        while let Ok(ev) = app.ui_rx.try_recv() {
            app.handle_ui_event(ev);
        }

        // A queued `/quit` ran when its turn came.
        if app.quit_requested {
            break;
        }
    }

    // Cleanup
    ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        AlternateScroll(false),
        crossterm::terminal::LeaveAlternateScreen,
    )?;

    // Printed after leaving the alternate screen, or it would vanish with it.
    let saved_id = last_saved.lock().ok().and_then(|slot| slot.clone());

    if let Some(id) = saved_id {
        println!("\nResume this conversation with:\n  tack --resume {id}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An app wired to channels the test can read, starting mid-turn.
    fn busy_app() -> (App, mpsc::UnboundedReceiver<SessionCmd>) {
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let (session_tx, session_rx) = mpsc::unbounded_channel();
        let mut app = App::new(
            PathBuf::from("/tmp"),
            DEFAULT_MODEL.to_string(),
            Vec::new(),
            ui_rx,
            ui_tx,
            session_tx,
        );
        app.state = AppState::Busy;
        (app, session_rx)
    }

    fn done() -> UiEvent {
        UiEvent::Done {
            usage: Usage::default(),
            context_used: 0,
        }
    }

    fn sent(rx: &mut mpsc::UnboundedReceiver<SessionCmd>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            out.push(match cmd {
                SessionCmd::SendMessage(t) => format!("send:{t}"),
                SessionCmd::SetModel(m) => format!("model:{m}"),
                SessionCmd::Clear => "clear".to_string(),
                SessionCmd::Stop => "stop".to_string(),
            });
        }
        out
    }

    #[test]
    fn page_keys_scroll_the_conversation() {
        let (mut app, _rx) = busy_app();
        app.max_scroll = 100;
        app.scroll = 50;
        app.auto_scroll = true;
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);

        app.handle_key(key(KeyCode::PageUp));
        assert!(app.scroll < 50);
        assert!(!app.auto_scroll);

        app.scroll = 95;
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.scroll, 100);
        assert!(app.auto_scroll);
    }

    #[test]
    fn f2_toggles_the_mouse_capture() {
        let (mut app, _rx) = busy_app();
        assert!(app.mouse_capture);
        let f2 = crossterm::event::KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE);

        app.handle_key(f2);
        assert!(!app.mouse_capture);
        app.handle_key(f2);
        assert!(app.mouse_capture);
    }

    // With alternate scroll, the terminal sends the wheel as arrow keys.
    #[test]
    fn arrow_keys_scroll_the_conversation() {
        let (mut app, _rx) = busy_app();
        app.max_scroll = 100;
        app.scroll = 50;
        app.auto_scroll = true;
        let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);

        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.scroll, 49);
        assert!(!app.auto_scroll);

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.scroll, 50);
        assert!(!app.auto_scroll, "not at the bottom yet");

        app.scroll = 100;
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.scroll, 100);
        assert!(app.auto_scroll);
    }

    #[test]
    fn queued_messages_run_one_turn_at_a_time() {
        let (mut app, mut rx) = busy_app();
        app.queue.extend(["first".to_string(), "second".to_string()]);

        app.handle_ui_event(done());
        assert_eq!(sent(&mut rx), ["send:first"]);
        assert!(app.state.is_busy());

        app.handle_ui_event(done());
        assert_eq!(sent(&mut rx), ["send:second"]);

        app.handle_ui_event(done());
        assert!(sent(&mut rx).is_empty());
        assert_eq!(app.state, AppState::Idle);
    }

    fn monitor_event() -> UiEvent {
        UiEvent::Monitor(tack_core::monitor::MonitorEvent {
            id: 1,
            description: "log".to_string(),
            lines: vec!["ERROR disk full".to_string()],
            dropped: 0,
            ended: None,
        })
    }

    #[test]
    fn monitor_output_waits_for_the_turn_to_end() {
        let (mut app, mut rx) = busy_app();

        app.handle_ui_event(monitor_event());
        assert!(sent(&mut rx).is_empty());
        assert_eq!(app.notices.len(), 1);

        app.handle_ui_event(done());
        let sent = sent(&mut rx);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].starts_with("send:Monitor 1 | log\n> ERROR disk full"), "{sent:?}");
    }

    #[test]
    fn monitor_output_during_a_permission_prompt_waits_for_the_turn() {
        let (mut app, mut rx) = busy_app();
        let (respond, _answer) = std_mpsc::sync_channel(1);
        app.handle_ui_event(UiEvent::PermissionRequest {
            description: "Run command: ls".to_string(),
            respond,
        });

        app.handle_ui_event(monitor_event());
        assert!(sent(&mut rx).is_empty());

        app.handle_ui_event(done());
        assert_eq!(sent(&mut rx).len(), 1);
        app.handle_ui_event(done());
        assert!(sent(&mut rx).is_empty(), "the notice was sent twice");
    }

    #[test]
    fn monitor_output_survives_a_failed_turn_and_is_sent_once() {
        for end in [UiEvent::Failed("overloaded".to_string()), UiEvent::Failed("Stopped.".to_string())] {
            let (mut app, mut rx) = busy_app();

            app.handle_ui_event(monitor_event());
            app.handle_ui_event(end);

            // The notice is not the user's text: it stays out of the input.
            assert!(!app.input.contains("Monitor 1"), "input: {:?}", app.input);
            assert!(sent(&mut rx).is_empty());

            // The next turn of the user runs, then the notice, once.
            app.input = "hi".to_string();
            app.submit_input();
            assert_eq!(sent(&mut rx), ["send:hi"]);
            app.handle_ui_event(done());
            let after = sent(&mut rx);
            assert_eq!(after.len(), 1, "{after:?}");
            assert!(after[0].starts_with("send:Monitor 1 | log"));
            app.handle_ui_event(done());
            assert!(sent(&mut rx).is_empty(), "the notice was sent twice");
        }
    }

    fn monitor_event_from(id: u32) -> UiEvent {
        UiEvent::Monitor(tack_core::monitor::MonitorEvent {
            id,
            description: "log".to_string(),
            lines: vec![format!("from {id}")],
            dropped: 0,
            ended: None,
        })
    }

    #[test]
    fn clear_drops_the_notices_of_the_old_conversation() {
        let (mut app, mut rx) = busy_app();
        app.handle_ui_event(monitor_event_from(1));
        app.handle_ui_event(UiEvent::Failed("overloaded".to_string()));
        assert_eq!(app.notices.len(), 1);

        app.input = "/clear".to_string();
        app.submit_input();
        assert!(app.notices.is_empty());
        sent(&mut rx);

        // Old events still in the channel, before and after the session
        // confirms the clear, do not reach the new conversation.
        app.handle_ui_event(monitor_event_from(1));
        app.handle_ui_event(UiEvent::Cleared { first_monitor_id: 3 });
        app.handle_ui_event(monitor_event_from(2));
        assert!(sent(&mut rx).is_empty());

        // A monitor of the new conversation gets through.
        app.handle_ui_event(monitor_event_from(3));
        assert_eq!(sent(&mut rx).len(), 1);
    }

    #[test]
    fn a_failed_clear_lets_the_old_monitors_through_again() {
        let (mut app, mut rx) = busy_app();
        app.handle_ui_event(done());
        sent(&mut rx);

        app.input = "/clear".to_string();
        app.submit_input();
        sent(&mut rx);
        app.handle_ui_event(UiEvent::Cleared { first_monitor_id: 0 });

        app.handle_ui_event(monitor_event_from(1));
        assert_eq!(sent(&mut rx).len(), 1);
    }

    #[cfg(feature = "crosstalk")]
    #[test]
    fn an_intro_goes_to_the_user_escaped_and_never_to_the_model() {
        let (mut app, mut rx) = busy_app();
        app.handle_ui_event(done());
        sent(&mut rx);

        let raw = "[intro] from crosstalk_7Xm2qB9vKcWzR4tY8nPfL3sHdJ6gA1eUoVbC5iNkQxZ via hub (Eve \u{1b}[2J)\nI am \u{202e}your bank";
        app.handle_ui_event(UiEvent::Crosstalk(raw.to_string()));

        assert!(sent(&mut rx).is_empty(), "the intro reached the model");
        let shown = app.messages.iter().rev().find_map(|m| match m {
            DisplayMessage::Info(text) => Some(text.clone()),
            _ => None,
        });
        let shown = shown.expect("the intro is shown");
        assert!(!shown.contains('\u{1b}') && !shown.contains('\u{202e}'), "{shown:?}");
        assert!(shown.contains("/crosstalk accept"));
    }

    #[cfg(feature = "crosstalk")]
    #[test]
    fn crosstalk_messages_over_the_limit_are_dropped_and_shown() {
        let (mut app, mut rx) = busy_app();
        app.handle_ui_event(done());
        sent(&mut rx);

        let peer = "crosstalk_7Xm2qB9vKcWzR4tY8nPfL3sHdJ6gA1eUoVbC5iNkQxZ";
        let raw: Vec<String> = (0..12)
            .map(|i| format!("[message] from {peer} via hub (Bob)\nmessage {i}"))
            .collect();
        app.handle_ui_event(UiEvent::Crosstalk(raw.join("\n\n")));

        let sent = sent(&mut rx);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].matches("Crosstalk | Message").count(), crosstalk::MAX_PER_MINUTE);
        assert!(app.messages.iter().any(|m| matches!(m, DisplayMessage::Info(text) if text.starts_with("2 crosstalk messages dropped"))));
    }

    #[test]
    fn monitor_output_starts_a_turn_when_idle() {
        let (mut app, mut rx) = busy_app();
        app.handle_ui_event(done());
        sent(&mut rx);

        app.handle_ui_event(monitor_event());
        assert_eq!(sent(&mut rx).len(), 1);
        assert!(app.state.is_busy());
    }

    #[test]
    fn queued_commands_run_as_commands() {
        let (mut app, mut rx) = busy_app();
        app.queue.extend([
            "/clear".to_string(),
            "/model opus".to_string(),
            "hello".to_string(),
        ]);

        app.handle_ui_event(done());

        let cmds = sent(&mut rx);
        assert_eq!(cmds[0], "clear");
        assert!(cmds[1].starts_with("model:") && cmds[1].contains("opus"));
        assert_eq!(cmds[2], "send:hello");
        assert_eq!(cmds.len(), 3);
    }

    #[test]
    fn queued_quit_exits() {
        let (mut app, _rx) = busy_app();
        app.queue.push_back("/quit".to_string());
        app.handle_ui_event(done());
        assert!(app.quit_requested);
    }

    #[test]
    fn stop_hands_queue_back_to_input() {
        let (mut app, mut rx) = busy_app();
        app.queue.extend(["one".to_string(), "two".to_string()]);
        app.input = "three".to_string();

        app.stop();
        app.handle_ui_event(UiEvent::Failed("Stopped.".to_string()));

        assert_eq!(sent(&mut rx), ["stop"]);
        assert_eq!(app.input, "one two three");
        assert_eq!(app.cursor, app.input.chars().count());
        assert!(app.queue.is_empty());
        assert_eq!(app.state, AppState::Idle);
    }

    #[test]
    fn stop_that_loses_the_race_still_holds_the_queue() {
        let (mut app, mut rx) = busy_app();
        app.queue.push_back("next".to_string());

        app.stop();
        app.handle_ui_event(done()); // turn finished before Stop landed

        assert_eq!(sent(&mut rx), ["stop"]);
        assert_eq!(app.input, "next");
        assert!(!app.stopping);
    }

    #[test]
    fn api_error_does_not_fire_the_queue() {
        let (mut app, mut rx) = busy_app();
        app.queue.push_back("next".to_string());

        app.handle_ui_event(UiEvent::Failed("overloaded".to_string()));

        assert!(sent(&mut rx).is_empty());
        assert_eq!(app.input, "next");
    }

    /// The screen as text, one string per row.
    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render::render(app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn queue_is_listed_above_the_input() {
        let (mut app, _rx) = busy_app();
        app.queue.extend(["first".to_string(), "second".to_string()]);
        app.input = "typing".to_string();

        let rows = screen(&mut app, 40, 20);
        let first = rows.iter().position(|r| r.contains("❯ first")).unwrap();
        let second = rows.iter().position(|r| r.contains("❯ second")).unwrap();
        let input = rows.iter().position(|r| r.contains("typing")).unwrap();

        assert_eq!(second, first + 1);
        assert!(input > second);
    }

    #[test]
    fn long_queue_collapses_oldest_entries() {
        let (mut app, _rx) = busy_app();
        app.queue.extend((1..=8).map(|n| format!("msg{n}")));

        let rows = screen(&mut app, 40, 20).join("\n");
        assert!(rows.contains("… 3 earlier queued"));
        assert!(!rows.contains("msg3"));
        assert!(rows.contains("msg4") && rows.contains("msg8"));
    }

    #[test]
    fn long_queued_line_is_truncated() {
        let (mut app, _rx) = busy_app();
        app.queue.push_back("x".repeat(100));

        let rows = screen(&mut app, 40, 20);
        let row = rows.iter().find(|r| r.contains("❯")).unwrap();
        assert!(row.contains('…'));
    }

    #[test]
    fn no_queue_no_panel() {
        let (mut app, _rx) = busy_app();
        assert!(!screen(&mut app, 40, 20).join("\n").contains("❯"));
    }

    // `/model` probes LM Studio with `tokio::spawn`, so this needs a runtime.
    #[tokio::test]
    async fn queued_model_picker_pauses_the_queue() {
        let (mut app, mut rx) = busy_app();
        app.queue.extend(["/model".to_string(), "after".to_string()]);

        app.handle_ui_event(done());
        assert!(app.model_picker.is_some());
        assert!(sent(&mut rx).is_empty());

        app.handle_picker_key(KeyCode::Esc);
        assert_eq!(sent(&mut rx), ["send:after"]);
    }
}
