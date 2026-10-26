mod commands;
mod permissions;
mod print;
mod tui;
mod ui;

use anyhow::Result;
use clap::Parser;

#[cfg(any(target_os = "macos", feature = "crosstalk"))]
use std::sync::Arc;

#[cfg(target_os = "macos")]
use claude_code_core::capabilities::Capabilities;
#[cfg(target_os = "macos")]
use claude_code_core::egress::Allowlist;
use claude_code_core::config::{Credentials, TokenType};
use claude_code_core::history::{self, StoredSession};
#[cfg(target_os = "macos")]
use claude_code_core::sandbox::AppleContainer;
use claude_code_core::session::SessionBuilder;
use claude_code_core::{auth, config};

use claude_code_core::permission::PermissionHandler;
use permissions::{ChannelPermissions, PolicyPermissions};

#[derive(Parser)]
#[command(name = "ccrs", version, about = "Claude Code — Rust edition")]
struct Cli {
    /// Force re-login, ignoring saved credentials
    #[arg(long)]
    login: bool,

    /// Approve tools without prompting. Deny rules still apply.
    #[arg(long)]
    auto: bool,

    /// Approve every tool without prompting, ignoring all rules (deny included).
    #[arg(long)]
    dangerously_skip_permissions: bool,

    /// Resume a saved conversation. With no ID, pick one from this directory.
    #[arg(short, long, value_name = "ID", num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,

    /// Resume the most recent conversation in this directory
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    continue_last: bool,

    /// Use this session id: resume it if it is saved, else start it
    #[arg(long, value_name = "ID", conflicts_with_all = ["resume", "continue_last"])]
    session: Option<String>,

    /// Answer one prompt without the TUI, print the reply and exit. With no
    /// prompt, read it from stdin. Tools that no rule allows are refused.
    #[arg(short, long, value_name = "PROMPT", num_args = 0..=1, default_missing_value = "")]
    print: Option<String>,

    /// Run Bash commands in an offline Linux VM from this OCI image (Apple `container`)
    #[cfg(target_os = "macos")]
    #[arg(long, value_name = "IMAGE")]
    sandbox: Option<String>,

    /// Hosts the sandbox may reach over HTTPS (e.g. crates.io,*.crates.io)
    #[cfg(target_os = "macos")]
    #[arg(long, value_name = "HOSTS", value_delimiter = ',', requires = "sandbox")]
    sandbox_allow: Vec<String>,
}

async fn login() -> Result<Credentials> {
    let method = ui::prompt_login_method()?;

    match method {
        ui::LoginMethod::OAuth => {
            let store_refresh = ui::prompt_store_refresh()?;
            let session = auth::start_oauth()?;

            eprintln!("Opening browser for authentication...");

            if webbrowser::open(&session.auth_url).is_err() {
                eprintln!("Could not open browser. Please visit this URL manually:");
                eprintln!("{}", session.auth_url);
            }

            let input = ui::prompt_oauth_code()?;
            let code = auth::parse_callback(&session, &input)?;
            auth::exchange_oauth_code(&session, &code, store_refresh).await
        }
        ui::LoginMethod::ApiKey => {
            let key = ui::prompt_api_key()?;
            Ok(Credentials {
                token: key,
                is_oauth: false,
            })
        }
    }
}

async fn get_access_token(creds: &Credentials) -> Result<(String, bool, Option<Credentials>)> {
    match creds.token_type() {
        TokenType::OAuthAccess => Ok((creds.token.clone(), true, None)),
        TokenType::OAuthRefresh => {
            eprintln!("Refreshing access token...");
            let (access_token, updated_creds) = auth::refresh_access_token(creds).await?;
            Ok((access_token, true, Some(updated_creds)))
        }
        TokenType::ApiKey => Ok((creds.token.clone(), false, None)),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env from the working directory upwards. Absent file is not an
    // error: the keys may already be exported.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    let quiet = cli.print.is_some();

    if !quiet {
        eprintln!("claude-code-rs v0.1.0\n");
    }

    // An exported key wins over saved credentials, as with `claude`.
    let env_key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|key| !key.is_empty() && !cli.login);

    let creds = match config::load_credentials()? {
        _ if env_key.is_some() => Credentials {
            token: env_key.unwrap_or_default(),
            is_oauth: false,
        },
        Some(c) if !cli.login => {
            if !quiet {
                eprintln!("Loaded saved credentials.");
            }
            c
        }
        _ => {
            let c = login().await?;
            config::save_credentials(&c)?;
            eprintln!("Credentials saved.");
            c
        }
    };

    let (access_token, is_oauth, updated_creds) = get_access_token(&creds).await?;

    if let Some(new_creds) = updated_creds {
        config::save_credentials(&new_creds)?;
    }

    let cwd = std::env::current_dir()?;
    let settings = config::load_settings(&cwd);

    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let perms: Box<dyn PermissionHandler> = if cli.print.is_some() {
        Box::new(
            PolicyPermissions::new(settings.permissions, cwd.clone())
                .with_auto(cli.auto)
                .with_skip_all(cli.dangerously_skip_permissions),
        )
    } else {
        Box::new(
            ChannelPermissions::new(settings.permissions, cwd.clone(), ui_tx.clone())
                .with_auto(cli.auto)
                .with_skip_all(cli.dangerously_skip_permissions),
        )
    };

    if cli.dangerously_skip_permissions {
        eprintln!("WARNING: --dangerously-skip-permissions is set. All tools run without prompting; deny rules are ignored.\n");
    }

    // Read the prompt before anything slow, so a missing one fails fast.
    let prompt = match cli.print.as_deref() {
        Some("") => Some(print::read_stdin()?),
        Some(prompt) => Some(prompt.to_string()),
        None => None,
    };

    let stored = select_session(&cli, &cwd)?;
    let id = match (&stored, &cli.session) {
        (Some(stored), _) => stored.id.clone(),
        (None, Some(id)) => id.clone(),
        (None, None) => history::new_id(),
    };

    let mut builder = SessionBuilder::new(access_token, is_oauth).id(id.clone());

    // One crosstalk daemon per session id: a resumed session keeps its
    // identity and its peers.
    #[cfg(feature = "crosstalk")]
    let crosstalk = match start_crosstalk(&id).await {
        Ok(client) => {
            builder = builder.crosstalk(client.clone());
            Some(client)
        }
        Err(e) => {
            eprintln!("Crosstalk is off: {e:#}\n");
            None
        }
    };

    #[cfg(target_os = "macos")]
    if let Some(image) = cli.sandbox.clone() {
        let mut caps = Capabilities::local();
        let mut sandbox = AppleContainer::new(image);

        if !cli.sandbox_allow.is_empty() {
            sandbox = sandbox.with_allowlist(Allowlist::new(cli.sandbox_allow.clone()));
        }

        caps.sandbox = Arc::new(sandbox);
        builder = builder.capabilities(caps);
    }

    let mut session = builder.permissions(perms)?;

    let mut restored = Vec::new();

    if let Some(stored) = stored {
        restored = tui::display_history(&stored.messages);
        session.restore(stored);
    }

    if let Some(prompt) = prompt {
        let result = print::run(session, &prompt).await;

        #[cfg(feature = "crosstalk")]
        if let Some(client) = crosstalk {
            client.stop().await;
        }

        return result;
    }

    #[cfg(feature = "crosstalk")]
    {
        let result = tui::run(cwd, session, restored, ui_tx, ui_rx, crosstalk.clone());

        if let Some(client) = crosstalk {
            client.stop().await;
        }

        result
    }

    #[cfg(not(feature = "crosstalk"))]
    tui::run(cwd, session, restored, ui_tx, ui_rx)
}

#[cfg(feature = "crosstalk")]
async fn start_crosstalk(id: &str) -> Result<Arc<claude_code_core::crosstalk::Client>> {
    let client = claude_code_core::crosstalk::Client::new(id)?;
    client.start().await?;
    Ok(Arc::new(client))
}

/// Resolve `--resume [ID]` / `--continue` to a saved session, if requested.
fn select_session(cli: &Cli, cwd: &std::path::Path) -> Result<Option<StoredSession>> {
    if let Some(id) = &cli.session {
        return history::find(id);
    }

    if let Some(id) = cli.resume.as_deref().filter(|id| !id.is_empty()) {
        return history::load(id).map(Some);
    }

    if !cli.continue_last && cli.resume.is_none() {
        return Ok(None);
    }

    let saved = history::list(cwd)?;

    if saved.is_empty() {
        eprintln!("No saved conversations in this directory. Starting a new one.\n");
        return Ok(None);
    }

    let id = if cli.continue_last {
        saved[0].id.clone()
    } else {
        match ui::prompt_resume(&saved)? {
            Some(i) => saved[i].id.clone(),
            None => return Ok(None),
        }
    };

    history::load(&id).map(Some)
}
