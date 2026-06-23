//! End-to-end tests for `dome sandbox ls`. Unlike the other sandbox integration
//! tests, listing never boots a VM, so these run by default (no `DOME_BIN` /
//! codesigning needed): they drive the cargo-built binary against a temp data dir
//! seeded with real CAS index and lock files, and assert on the rendered table.
//!
//! `ls` now routes through the domed control plane (auto-spawning it), so each test
//! tears the daemon down afterwards via [`stop_daemon`] to avoid leaking a process.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use dome_store::ChunkIndex;

/// The dome binary cargo builds for this test crate.
const DOME_BIN: &str = env!("CARGO_BIN_EXE_dome");

/// Run `dome sandbox ls` with `HOME` pointed at `home` (which controls the data dir,
/// `$HOME/.local/share/dome`).
fn sandbox_ls(home: &Path) -> std::process::Output {
    Command::new(DOME_BIN)
        .args(["sandbox", "ls"])
        .env("HOME", home)
        .output()
        .expect("failed to spawn dome binary")
}

/// Stop the daemon `ls` auto-spawned so it does not outlive the test's temp dir.
fn stop_daemon(home: &Path) {
    let _ = Command::new(DOME_BIN)
        .args(["daemon", "stop"])
        .env("HOME", home)
        .output();
    let sock = home.join(".local/share/dome/daemon/domed.sock");
    let deadline = Instant::now() + Duration::from_secs(2);
    while sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn sandboxes_dir(home: &Path) -> std::path::PathBuf {
    home.join(".local/share/dome/sandboxes")
}

/// Write a sandbox index pinned to `rootfs-<version>.ext4` with `written_chunks`
/// non-ZERO chunks, mimicking a flatten-saved sandbox.
fn write_sandbox(home: &Path, name: &str, version: &str, written_chunks: usize) {
    let dir = sandboxes_dir(home);
    std::fs::create_dir_all(&dir).unwrap();
    let mut idx = ChunkIndex::new(64 * 1024 * 1024);
    idx.fallback_path = Some(
        home.join(format!(".local/share/dome/rootfs-{version}.ext4"))
            .to_string_lossy()
            .to_string(),
    );
    for i in 0..written_chunks {
        idx.set_hash(i, format!("hash{i:08x}"));
    }
    idx.save(dir.join(format!("{name}.idx")).to_str().unwrap())
        .unwrap();
}

#[test]
fn ls_on_an_empty_set_prints_a_clear_message() {
    let home = tempfile::tempdir().unwrap();
    let out = sandbox_ls(home.path());
    assert!(out.status.success(), "ls should succeed on an empty set");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.to_lowercase().contains("no sandboxes"),
        "expected a clear 'no sandboxes' message, got: {combined}"
    );
    stop_daemon(home.path());
}

#[test]
fn ls_lists_name_size_base_and_idle_status() {
    let home = tempfile::tempdir().unwrap();
    // One written chunk → 64 KiB CAS delta, pinned to base 1.2.3, no lock → idle.
    write_sandbox(home.path(), "web", "1.2.3", 1);

    let out = sandbox_ls(home.path());
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Header carries the sandbox-specific columns.
    assert!(stdout.contains("NAME"), "missing NAME header: {stdout}");
    assert!(stdout.contains("BASE"), "missing BASE header: {stdout}");
    assert!(stdout.contains("STATE"), "missing STATE header: {stdout}");

    let row = stdout
        .lines()
        .find(|l| l.starts_with("web"))
        .unwrap_or_else(|| panic!("no row for 'web' in:\n{stdout}"));
    assert!(row.contains("64 KB (cas)"), "wrong SIZE in row: {row}");
    assert!(row.contains("1.2.3"), "wrong BASE in row: {row}");
    assert!(
        row.contains("idle"),
        "an unlocked sandbox must be idle: {row}"
    );
    stop_daemon(home.path());
}

#[test]
fn ls_reports_running_when_a_live_session_holds_the_lock() {
    let home = tempfile::tempdir().unwrap();
    write_sandbox(home.path(), "api", "2.0.0", 0);
    // A lock recording this (live) test process's PID marks the sandbox as running.
    std::fs::write(
        sandboxes_dir(home.path()).join("api.lock"),
        std::process::id().to_string(),
    )
    .unwrap();

    let out = sandbox_ls(home.path());
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let row = stdout
        .lines()
        .find(|l| l.starts_with("api"))
        .unwrap_or_else(|| panic!("no row for 'api' in:\n{stdout}"));
    assert!(
        row.contains("running"),
        "a sandbox with a live lock must be running: {row}"
    );
    stop_daemon(home.path());
}
