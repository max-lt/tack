//! ccrs -p against a local mock of the Messages API, in a temporary HOME:
//! the model asks for a monitor, ccrs starts it, and the run ends.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Answers the first request with a Monitor call for `command`, and the
/// request with the tool result with a text, after `hold`.
struct MockApi {
    port: u16,
    tool_results: Arc<Mutex<Vec<String>>>,
}

impl MockApi {
    fn start(command: String, hold: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let tool_results = Arc::new(Mutex::new(Vec::new()));
        let results = tool_results.clone();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let (command, results) = (command.clone(), results.clone());

                std::thread::spawn(move || {
                    let body = read_body(&mut stream);
                    let last = &body["messages"].as_array().unwrap().last().unwrap()["content"];
                    let result = last
                        .as_array()
                        .and_then(|blocks| blocks.iter().find(|b| b["type"] == "tool_result"));

                    let events = match result {
                        Some(result) => {
                            results.lock().unwrap().push(result["content"].to_string());
                            std::thread::sleep(hold);
                            text_reply("mock done")
                        }
                        None => monitor_call(&command),
                    };

                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(events.as_bytes());
                });
            }
        });

        Self { port, tool_results }
    }
}

fn read_body(stream: &mut std::net::TcpStream) -> serde_json::Value {
    let mut reader = BufReader::new(stream);
    let mut length = 0;

    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
    }

    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn sse(event: &str, data: serde_json::Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn message(block: serde_json::Value, delta: serde_json::Value, stop: &str) -> String {
    [
        sse("message_start", serde_json::json!({
            "type": "message_start",
            "message": { "id": "msg_mock", "type": "message", "role": "assistant", "content": [],
                         "model": "mock", "stop_reason": null,
                         "usage": { "input_tokens": 1, "output_tokens": 1 } }
        })),
        sse("content_block_start", serde_json::json!({ "type": "content_block_start", "index": 0, "content_block": block })),
        sse("content_block_delta", serde_json::json!({ "type": "content_block_delta", "index": 0, "delta": delta })),
        sse("content_block_stop", serde_json::json!({ "type": "content_block_stop", "index": 0 })),
        sse("message_delta", serde_json::json!({ "type": "message_delta", "delta": { "stop_reason": stop },
                                                 "usage": { "output_tokens": 1 } })),
        sse("message_stop", serde_json::json!({ "type": "message_stop" })),
    ]
    .concat()
}

fn monitor_call(command: &str) -> String {
    let input = serde_json::json!({ "command": command, "description": "mock monitor" });
    message(
        serde_json::json!({ "type": "tool_use", "id": "toolu_mock", "name": "Monitor", "input": {} }),
        serde_json::json!({ "type": "input_json_delta", "partial_json": input.to_string() }),
        "tool_use",
    )
}

fn text_reply(text: &str) -> String {
    message(
        serde_json::json!({ "type": "text", "text": "" }),
        serde_json::json!({ "type": "text_delta", "text": text }),
        "end_turn",
    )
}

/// A temporary HOME and project. The project allows `Monitor(*)`, so -p
/// starts the monitor with no prompt and no --auto. A prefix rule does not
/// cover the test command: it holds `$`, `>` and `;`.
struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("home")).unwrap();
        std::fs::create_dir_all(root.path().join("project/.claude")).unwrap();
        std::fs::write(
            root.path().join("project/.claude/settings.json"),
            r#"{ "permissions": { "allow": ["Monitor(*)"] } }"#,
        )
        .unwrap();
        Self { root }
    }

    /// A monitor command that writes its pid to `<name>.pid`, then runs.
    fn command(&self, name: &str) -> String {
        format!("echo $$ > {}; sleep 300", self.pid_file(name).display())
    }

    fn pid_file(&self, name: &str) -> PathBuf {
        self.root.path().join(format!("{name}.pid"))
    }

    fn ccrs(&self, api: &MockApi, session: &str) -> Child {
        Command::new(env!("CARGO_BIN_EXE_ccrs"))
            .args(["-p", "Watch it.", "--session", session])
            .current_dir(self.root.path().join("project"))
            .env("HOME", self.root.path().join("home"))
            .env("ANTHROPIC_API_KEY", "sk-ant-mock")
            .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{}", api.port))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn session_file(&self, id: &str) -> PathBuf {
        self.root
            .path()
            .join("home/Library/Application Support/claude-code-rs/sessions")
            .join(format!("{id}.json"))
    }
}

fn wait_for_pid(path: &Path) -> String {
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if let Ok(pid) = std::fs::read_to_string(path)
            && !pid.trim().is_empty()
        {
            return pid.trim().to_string();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("no pid in {}", path.display());
}

fn alive(pid: &str) -> bool {
    Command::new("kill").args(["-0", pid]).stderr(Stdio::null()).status().unwrap().success()
}

fn gone_soon(pid: &str) -> bool {
    (0..50).any(|_| {
        std::thread::sleep(Duration::from_millis(20));
        !alive(pid)
    })
}

fn sigterm(child: &Child) {
    let sent = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
    assert!(sent.success());
}

#[cfg(target_os = "macos")]
#[test]
fn print_mode_starts_a_monitor_and_stops_it_at_exit() {
    let sandbox = Sandbox::new();
    let api = MockApi::start(sandbox.command("one"), Duration::ZERO);

    let run = sandbox.ccrs(&api, "mock-one");
    let pid = wait_for_pid(&sandbox.pid_file("one"));
    let output = run.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "mock done");
    assert!(gone_soon(&pid), "the monitor {pid} outlived ccrs -p");

    let results = api.tool_results.lock().unwrap().join("\n");
    assert!(results.contains("will not reach you"), "{results}");

    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sandbox.session_file("mock-one")).unwrap()).unwrap();
    assert_eq!(saved["store"]["monitors"][0]["description"], "mock monitor");
}

#[cfg(target_os = "macos")]
#[test]
fn sigterm_to_one_ccrs_leaves_the_monitors_of_another() {
    let sandbox = Sandbox::new();
    // The final reply waits, so both runs hold their monitors.
    let api_a = MockApi::start(sandbox.command("a"), Duration::from_secs(30));
    let api_b = MockApi::start(sandbox.command("b"), Duration::from_secs(30));

    let a = sandbox.ccrs(&api_a, "mock-a");
    let b = sandbox.ccrs(&api_b, "mock-b");
    let (pid_a, pid_b) = (wait_for_pid(&sandbox.pid_file("a")), wait_for_pid(&sandbox.pid_file("b")));

    sigterm(&a);
    let status_a = a.wait_with_output().unwrap().status;
    assert_eq!(status_a.code(), Some(143));
    assert!(gone_soon(&pid_a), "the monitor of A outlived A");
    assert!(alive(&pid_b), "SIGTERM to A killed the monitor of B");

    sigterm(&b);
    b.wait_with_output().unwrap();
    assert!(gone_soon(&pid_b), "the monitor of B outlived B");
}
