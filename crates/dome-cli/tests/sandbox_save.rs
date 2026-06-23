//! Integration test for issue #26: explicit `dome sandbox save` durability.
//!
//! Boots a REAL persistent VM through a worker, so it requires a codesigned binary and is
//! `#[ignore]`d by default (excluded from CI's `cargo test`). Run with:
//!   DOME_BIN=target/debug/dome cargo test -p dome-cli --test sandbox_save -- --ignored
//!
//! It exercises the #26 contract that a save is durable independently of the graceful
//! stop-save: a write made in a live VM, then forced to disk with `dome sandbox save`,
//! survives a hard crash of the worker (SIGKILL, no graceful save) — a subsequent cold
//! boot reads the saved index and sees the write.
//!
//! The auto-flush trigger thresholds (interval + dirty-cap) and the `sandbox.saved` event
//! rebroadcast have hypervisor-free unit tests in `src/worker.rs` (`flush_is_due`, the
//! `Save` op round-trip) and `src/daemon.rs` (`worker_saved_is_rebroadcast_...`).

use std::process::{Command, Stdio};
use std::time::Duration;

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. `just build`)")
}

fn sandbox_name(suffix: &str) -> String {
    format!("itest-save-{}-{}", std::process::id(), suffix)
}

fn sandbox_run(name: &str, guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args(["sandbox", "run", name, "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

/// SIGTERM the per-sandbox worker so it saves + tears the VM down cleanly. Best-effort.
fn stop_worker(name: &str) {
    let _ = Command::new("pkill")
        .args(["-TERM", "-f", &format!("__worker {}", name)])
        .output();
    std::thread::sleep(Duration::from_secs(3));
}

/// SIGKILL the per-sandbox worker — a hard crash with NO graceful save. Anything not
/// already flushed to the index is lost; this is how we prove the explicit save persisted.
fn kill_worker(name: &str) {
    let _ = Command::new("pkill")
        .args(["-KILL", "-f", &format!("__worker {}", name)])
        .output();
    std::thread::sleep(Duration::from_secs(2));
}

fn cleanup(name: &str) {
    stop_worker(name);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = format!("{}/.local/share/dome/sandboxes", home);
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    let _ = std::fs::remove_file(format!("{}/{}.lock", dir, name));
}

/// `dome sandbox save` forces an in-memory write to disk so it survives a worker crash:
/// after the explicit save, a SIGKILL (no graceful save) followed by a cold boot still
/// reads the saved write.
#[test]
#[ignore]
fn explicit_save_persists_across_a_worker_crash() {
    let name = sandbox_name("crash");
    cleanup(&name);

    // Terminal A cold-boots the VM, writes a marker file, then stays attached (sleep) so
    // the VM stays live while we issue the explicit save and then crash it.
    let mut a = Command::new(dome_bin())
        .args([
            "sandbox",
            "run",
            &name,
            "--",
            "sh",
            "-c",
            "echo persisted-by-save > /root/saved-marker.txt; sleep 12",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn terminal A");

    // Give A time to cold-boot the VM and write the file before we save.
    std::thread::sleep(Duration::from_secs(6));

    // Force the in-memory write durable while the VM is still live.
    let save = Command::new(dome_bin())
        .args(["sandbox", "save", &name])
        .output()
        .expect("failed to spawn dome sandbox save");
    assert!(
        save.status.success(),
        "`sandbox save` should succeed against a running sandbox; stderr: {}",
        String::from_utf8_lossy(&save.stderr)
    );

    // Hard-crash the worker: SIGKILL skips the graceful stop-save entirely, so only the
    // explicit save above can have made the write durable.
    kill_worker(&name);
    let _ = a.wait();

    // A fresh cold boot reads the saved index and sees the write.
    let c = sandbox_run(&name, "cat /root/saved-marker.txt");
    assert!(
        String::from_utf8_lossy(&c.stdout).contains("persisted-by-save"),
        "an explicitly saved write must survive a worker crash + cold boot; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&c.stdout),
        String::from_utf8_lossy(&c.stderr)
    );

    cleanup(&name);
}

/// Saving a sandbox that is not running is a clear error (an idle sandbox's on-disk index
/// is already its durable state — there is nothing buffered in memory to flush).
#[test]
#[ignore]
fn saving_an_idle_sandbox_errors_clearly() {
    let name = sandbox_name("idle");
    cleanup(&name);
    // Create the sandbox on disk without leaving a worker running.
    let create = Command::new(dome_bin())
        .args(["sandbox", "create", &name])
        .output()
        .expect("failed to spawn dome sandbox create");
    assert!(create.status.success());

    let save = Command::new(dome_bin())
        .args(["sandbox", "save", &name])
        .output()
        .expect("failed to spawn dome sandbox save");
    assert!(!save.status.success(), "saving an idle sandbox must fail");
    assert!(
        String::from_utf8_lossy(&save.stderr).contains("not running"),
        "the error should explain the sandbox is not running; stderr: {}",
        String::from_utf8_lossy(&save.stderr)
    );

    cleanup(&name);
}
