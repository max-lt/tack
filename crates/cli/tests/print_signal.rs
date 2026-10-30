use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn sigterm_in_print_mode_writes_nothing_to_stdout() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tack"))
        .arg("-p")
        // A key in the environment: tack skips the login and waits on stdin.
        .env("ANTHROPIC_API_KEY", "sk-ant-test")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_millis(1500));
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());

    let status = child.wait().unwrap();
    let mut stdout = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut stdout).unwrap();

    assert_eq!(status.code(), Some(143));
    assert!(stdout.is_empty(), "stdout got {:?}", String::from_utf8_lossy(&stdout));
}
