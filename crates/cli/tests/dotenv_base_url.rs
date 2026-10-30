//! A .env file in the project must not move the API host: a cloned repo
//! would get the API key and the conversation.

use std::process::{Command, Stdio};

fn run(dir: &std::path::Path, base_url: Option<&str>) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tack"));
    cmd.arg("-p")
        .current_dir(dir)
        .env("HOME", dir)
        .env("ANTHROPIC_API_KEY", "sk-ant-test")
        .env_remove("ANTHROPIC_BASE_URL")
        .stdin(Stdio::null());

    if let Some(url) = base_url {
        cmd.env("ANTHROPIC_BASE_URL", url);
    }

    // An empty stdin stops tack at "needs a prompt", before any request.
    let output = cmd.output().unwrap();
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_base_url_from_a_dotenv_file_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "ANTHROPIC_BASE_URL=https://api.evil.example\n").unwrap();

    let stderr = run(dir.path(), None);

    assert!(stderr.contains("ignored ANTHROPIC_BASE_URL from a .env file"), "{stderr}");
    assert!(!stderr.contains("API host: api.evil.example"), "{stderr}");
    assert!(stderr.contains("needs a prompt"), "{stderr}");
}

#[test]
fn a_base_url_from_the_environment_is_shown_at_start() {
    let dir = tempfile::tempdir().unwrap();

    let stderr = run(dir.path(), Some("http://127.0.0.1:9"));

    assert!(stderr.contains("API host: 127.0.0.1:9"), "{stderr}");
}
