//! What a session has set up besides its conversation: the declarations of
//! its terminals and monitors (and later its schedules). It is saved with the
//! history, so a resumed session knows what it had.
//!
//! A declaration is not the live thing. A process does not survive its
//! owner: on restore, terminals are reported as lost (their state is gone),
//! and monitors are started again (a `tail -F` loses nothing).

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Store {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminals: Vec<TerminalEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitors: Vec<MonitorEntry>,
}

/// A command whose output wakes the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorEntry {
    pub id: u32,
    pub command: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host: bool,
}

/// A terminal the model opened and has not closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalEntry {
    pub id: u32,
    pub command: String,
    /// Runs on the user's machine, outside the sandbox.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host: bool,
}

/// The store, shared by the session and the tools that write to it.
pub type SharedStore = Arc<Mutex<Store>>;

impl Store {
    pub fn is_empty(&self) -> bool {
        self.terminals.is_empty() && self.monitors.is_empty()
    }

    pub fn add_monitor(&mut self, entry: MonitorEntry) {
        self.monitors.push(entry);
    }

    pub fn remove_monitor(&mut self, id: u32) {
        self.monitors.retain(|m| m.id != id);
    }

    pub fn last_monitor_id(&self) -> u32 {
        self.monitors.iter().map(|m| m.id).max().unwrap_or(0)
    }

    pub fn add_terminal(&mut self, entry: TerminalEntry) {
        self.terminals.push(entry);
    }

    pub fn remove_terminal(&mut self, id: u32) {
        self.terminals.retain(|t| t.id != id);
    }

    /// The highest id ever handed out, so new terminals do not reuse an id
    /// the conversation already mentions.
    pub fn last_terminal_id(&self) -> u32 {
        self.terminals.iter().map(|t| t.id).max().unwrap_or(0)
    }

    /// Drop the terminals of a previous run and describe them for the model.
    /// `None` when there were none.
    pub fn take_lost(&mut self) -> Option<String> {
        if self.terminals.is_empty() {
            return None;
        }

        let mut note = String::from(
            "This session was resumed. These terminals were open and did not survive; \
             they were not restarted:",
        );

        for t in self.terminals.drain(..) {
            let place = if t.host { " (on the user's machine)" } else { "" };
            let command = if t.command.is_empty() { "bash" } else { &t.command };
            note.push_str(&format!("\n- terminal {}{place}: {command}", t.id));
        }

        Some(note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u32, command: &str) -> TerminalEntry {
        TerminalEntry {
            id,
            command: command.to_string(),
            host: false,
        }
    }

    #[test]
    fn terminals_come_and_go() {
        let mut store = Store::default();
        store.add_terminal(entry(1, "psql"));
        store.add_terminal(entry(2, ""));
        store.remove_terminal(1);

        assert_eq!(store.terminals, vec![entry(2, "")]);
        assert_eq!(store.last_terminal_id(), 2);
    }

    #[test]
    fn lost_terminals_are_reported_once() {
        let mut store = Store::default();
        store.add_terminal(entry(3, "npm run dev"));

        let note = store.take_lost().unwrap();
        assert!(note.contains("terminal 3: npm run dev"), "{note}");
        assert!(store.is_empty());
        assert_eq!(store.take_lost(), None);
    }

    #[test]
    fn an_empty_store_serializes_to_nothing() {
        assert_eq!(serde_json::to_string(&Store::default()).unwrap(), "{}");
    }
}
