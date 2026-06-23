//! End-to-end tests for the `domed` control plane via the `dome` binary. These never
//! boot a VM (domed startup boots zero VMs), so they run by default — no `DOME_BIN` /
//! codesigning needed. Each test points `HOME` at a temp dir so the daemon socket, lock,
//! and logs land in an isolated `$HOME/.local/share/dome/daemon`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The dome binary cargo builds for this test crate.
const DOME_BIN: &str = env!("CARGO_BIN_EXE_dome");

fn dome(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(DOME_BIN)
        .args(args)
        .env("HOME", home)
        .output()
        .expect("failed to spawn dome binary")
}

fn daemon_dir(home: &Path) -> PathBuf {
    home.join(".local/share/dome/daemon")
}

fn socket(home: &Path) -> PathBuf {
    daemon_dir(home).join("domed.sock")
}

/// Best-effort: stop any daemon this test spawned so no process leaks past the test.
fn stop_daemon(home: &Path) {
    let _ = dome(home, &["daemon", "stop"]);
    // Give the daemon a moment to remove its socket.
    let deadline = Instant::now() + Duration::from_secs(2);
    while socket(home).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn status_reports_down_before_any_daemon_is_started() {
    let home = tempfile::tempdir().unwrap();
    let out = dome(home.path(), &["daemon", "status"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("down"),
        "status before start should report down, got: {stdout}"
    );
}

#[test]
fn start_then_status_then_stop_lifecycle() {
    let home = tempfile::tempdir().unwrap();

    // start brings domed up and reports a pid + socket path.
    let start = dome(home.path(), &["daemon", "start"]);
    assert!(start.status.success(), "daemon start should succeed");
    let start_out = String::from_utf8_lossy(&start.stdout);
    assert!(
        start_out.contains("started") || start_out.contains("already running"),
        "unexpected start output: {start_out}"
    );
    assert!(
        socket(home.path()).exists(),
        "start should create the socket"
    );

    // status reports up with the expected fields, and zero workers (no VM booted).
    let status = dome(home.path(), &["daemon", "status"]);
    let status_out = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_out.contains("up"),
        "status should be up: {status_out}"
    );
    assert!(
        status_out.contains("workers: 0"),
        "no VM booted: {status_out}"
    );
    assert!(
        status_out.contains("socket:"),
        "status shows socket: {status_out}"
    );

    // stop shuts it down and removes the socket.
    let stop = dome(home.path(), &["daemon", "stop"]);
    assert!(stop.status.success());
    let deadline = Instant::now() + Duration::from_secs(3);
    while socket(home.path()).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !socket(home.path()).exists(),
        "stop should remove the control socket"
    );

    // status after stop reports down again.
    let after = dome(home.path(), &["daemon", "status"]);
    assert!(String::from_utf8_lossy(&after.stdout).contains("down"));
}

#[test]
fn second_start_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let first = dome(home.path(), &["daemon", "start"]);
    assert!(first.status.success());
    let second = dome(home.path(), &["daemon", "start"]);
    assert!(second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("already running"),
        "a second start must report the daemon is already running"
    );
    stop_daemon(home.path());
}

#[test]
fn ls_auto_spawns_the_daemon() {
    let home = tempfile::tempdir().unwrap();
    // No daemon is running yet; `sandbox ls` must auto-spawn one and succeed.
    assert!(!socket(home.path()).exists());
    let out = dome(home.path(), &["sandbox", "ls"]);
    assert!(
        out.status.success(),
        "ls should succeed by auto-spawning domed"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.to_lowercase().contains("no sandboxes"),
        "expected a clear 'no sandboxes' message, got: {combined}"
    );
    assert!(
        socket(home.path()).exists(),
        "ls should have left a running daemon behind"
    );
    stop_daemon(home.path());
}

#[test]
fn a_stale_socket_from_a_crashed_daemon_is_reclaimed() {
    let home = tempfile::tempdir().unwrap();
    // Simulate a crashed daemon: a leftover socket file with no listener and a lock
    // recording a dead PID.
    let dir = daemon_dir(home.path());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("domed.sock"), b"stale").unwrap();
    std::fs::write(dir.join("domed.lock"), "2147483646").unwrap();

    // start must reclaim the stale socket + lock and come up cleanly.
    let start = dome(home.path(), &["daemon", "start"]);
    assert!(
        start.status.success(),
        "start should reclaim a stale socket, got: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    let status = dome(home.path(), &["daemon", "status"]);
    assert!(String::from_utf8_lossy(&status.stdout).contains("up"));
    stop_daemon(home.path());
}

#[test]
fn the_daemon_writes_a_log_under_the_state_dir() {
    let home = tempfile::tempdir().unwrap();
    dome(home.path(), &["daemon", "start"]);
    let log = daemon_dir(home.path()).join("domed.log");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !log.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(log.exists(), "domed should write a log under the state dir");
    stop_daemon(home.path());
}
