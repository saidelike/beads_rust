#![cfg(all(unix, feature = "mcp"))]

mod common;

use common::cli::isolated_temp_root;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn should_clear_inherited_br_env(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    key.starts_with("BD_")
        || key.starts_with("BEADS_")
        || matches!(
            key.as_ref(),
            "BR_DISABLE_READ_ONLY_FAST_OPEN"
                | "BR_OUTPUT_FORMAT"
                | "TOON_DEFAULT_FORMAT"
                | "TOON_STATS"
        )
}

fn br_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_br"));
    command.current_dir(root);
    for (key, _) in std::env::vars_os() {
        if should_clear_inherited_br_env(&key) {
            command.env_remove(key);
        }
    }
    command.env("HOME", root);
    command.env("NO_COLOR", "1");
    command.env("RUST_LOG", "error");
    command.env_remove("FASTMCP_NO_BANNER");
    command.env("FASTMCP_BANNER", "full");
    command
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn send_sigint(pid: u32) {
    let status = Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .expect("send SIGINT with kill");
    assert!(status.success(), "kill -INT {pid} failed: {status}");
}

fn capture_stderr_until_exit(child: &mut Child) -> (Receiver<()>, JoinHandle<Vec<u8>>) {
    let stderr = child.stderr.take().expect("capture serve stderr");
    let (ready_sender, ready_receiver) = sync_channel(1);
    let reader = thread::spawn(move || {
        let mut stderr = BufReader::new(stderr);
        let mut output = Vec::new();
        let mut line = Vec::new();
        let mut ready_sent = false;
        loop {
            line.clear();
            match stderr.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if !ready_sent
                        && line
                            .windows(b"Server ready.".len())
                            .any(|window| window == b"Server ready.")
                    {
                        let _ = ready_sender.send(());
                        ready_sent = true;
                    }
                    output.extend_from_slice(&line);
                }
                Err(error) => {
                    output.extend_from_slice(
                        format!("\nfailed to capture serve stderr: {error}\n").as_bytes(),
                    );
                    break;
                }
            }
        }
        output
    });
    (ready_receiver, reader)
}

fn collect_output(child: Child, stderr_reader: JoinHandle<Vec<u8>>) -> Output {
    let mut output = child.wait_with_output().expect("collect child output");
    output.stderr = stderr_reader.join().expect("join serve stderr reader");
    output
}

fn wait_with_timeout(
    mut child: Child,
    stderr_reader: JoinHandle<Vec<u8>>,
    timeout: Duration,
) -> Output {
    let start = Instant::now();
    loop {
        if child.try_wait().expect("poll child").is_some() {
            return collect_output(child, stderr_reader);
        }
        if start.elapsed() >= timeout {
            let pid = child.id();
            let status = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .expect("send SIGTERM with kill");
            assert!(status.success(), "kill -TERM {pid} failed: {status}");

            let cleanup_start = Instant::now();
            while cleanup_start.elapsed() < Duration::from_secs(2) {
                if child
                    .try_wait()
                    .expect("poll child after SIGTERM")
                    .is_some()
                {
                    let output = collect_output(child, stderr_reader);
                    assert_eq!(
                        output.status.code(),
                        Some(130),
                        "br serve did not exit within {timeout:?} after SIGINT and stdin close; \
                         after SIGTERM it exited with {}\nstdout:\n{}\nstderr:\n{}",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    return output;
                }
                thread::sleep(Duration::from_millis(20));
            }

            let status = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status()
                .expect("send SIGKILL with kill");
            assert!(status.success(), "kill -KILL {pid} failed: {status}");
            let output = collect_output(child, stderr_reader);
            assert_eq!(
                output.status.code(),
                Some(130),
                "br serve did not exit within {timeout:?} after SIGINT and stdin close; \
                 forced cleanup ended with {}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return output;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn serve_sigint_returns_through_main_and_preserves_reopenable_db() {
    let temp = TempDir::new_in(isolated_temp_root()).expect("isolated tempdir");
    let root = temp.path();

    assert_success(
        &br_command(root)
            .args(["init", "--prefix", "mcp"])
            .output()
            .expect("run br init"),
        "init",
    );
    assert_success(
        &br_command(root)
            .args(["create", "shutdown checkpoint proof", "--json"])
            .output()
            .expect("run br create"),
        "create",
    );

    let mut child = br_command(root)
        .args(["serve", "--actor", "mcp-shutdown-test"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn br serve");

    let (ready_receiver, stderr_reader) = capture_stderr_until_exit(&mut child);
    if let Err(error) = ready_receiver.recv_timeout(Duration::from_secs(5)) {
        let _ = child.kill();
        let output = collect_output(child, stderr_reader);
        panic!(
            "br serve did not report ready within 5s: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        child.try_wait().expect("poll br serve").is_none(),
        "br serve exited before SIGINT"
    );

    send_sigint(child.id());
    thread::sleep(Duration::from_millis(200));
    if let Some(mut stdin) = child.stdin.take() {
        stdin.flush().expect("flush serve stdin");
    }
    let output = wait_with_timeout(child, stderr_reader, Duration::from_secs(5));

    assert_eq!(
        output.status.code(),
        Some(130),
        "serve must return through br main's cooperative shutdown path\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_success(
        &br_command(root)
            .args(["list", "--json"])
            .output()
            .expect("reopen DB after serve shutdown"),
        "list after shutdown",
    );
    assert_success(
        &br_command(root)
            .args(["sync", "--status", "--json"])
            .output()
            .expect("check sync health after serve shutdown"),
        "sync status after shutdown",
    );
}
