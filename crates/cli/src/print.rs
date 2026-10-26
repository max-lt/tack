//! `--print`: one prompt, no TUI. The reply goes to stdout, everything else
//! to stderr, so the output can be piped.

use std::io::Read;

use anyhow::{Result, bail};
use tokio_util::sync::CancellationToken;

use claude_code_core::event::EventHandler;
use claude_code_core::history;
use claude_code_core::permission::PermissionHandler;
use claude_code_core::session::Session;

pub fn read_stdin() -> Result<String> {
    let mut prompt = String::new();
    std::io::stdin().read_to_string(&mut prompt)?;

    if prompt.trim().is_empty() {
        bail!("--print needs a prompt, as an argument or on stdin");
    }

    Ok(prompt)
}

/// Keeps the text after the last tool call: the answer, not the narration
/// between tool calls.
#[derive(Default)]
struct Reply {
    text: String,
}

impl EventHandler for Reply {
    fn on_text(&mut self, text: &str) {
        self.text.push_str(text);
    }

    fn on_error(&mut self, message: &str) {
        eprintln!("error: {message}");
    }

    fn on_tool_use_start(&mut self, _name: &str, _id: &str, _input: &serde_json::Value) {
        self.text.clear();
    }

    fn on_tool_result(&mut self, name: &str, output: &str, is_error: bool) {
        if is_error {
            let first = output.lines().next().unwrap_or("");
            eprintln!("{name} failed: {first}");
        }
    }
}

pub async fn run<P: PermissionHandler>(mut session: Session<P>, prompt: &str) -> Result<()> {
    let mut reply = Reply::default();
    let cancel = CancellationToken::new();

    let stop = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            stop.cancel();
        }
    });

    let result = session.send_message(prompt, &mut reply, &cancel).await;

    // Save even a cancelled turn: the next --session call picks it up.
    if let Some(stored) = session.to_stored() {
        history::save(&stored)?;
    }

    result?;

    println!("{}", reply.text.trim_end());
    eprintln!("session: {}", session.id());

    Ok(())
}
