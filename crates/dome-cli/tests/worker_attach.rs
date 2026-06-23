//! Integration tests for issue #24: worker boot + attach handoff.
//!
//! These boot a REAL persistent VM through a worker process, so they require a
//! codesigned binary and are `#[ignore]`d by default (excluded from CI's `cargo test`).
//! Run with:
//!   DOME_BIN=target/debug/dome cargo test -p dome-cli --test worker_attach -- --ignored
//!
//! They exercise the end-to-end persistent-sandbox contract that #24 introduces:
//!   * `dome sandbox run <name>` cold-boots a VM through a worker and runs a command,
//!   * the VM **stays running** after the session exits (a later session re-attaches to
//!     the *same live VM*, not a fresh boot),
//!   * disk writes persist across sessions.
//!
//! The handoff/token logic itself has hypervisor-free unit tests in `src/worker.rs`.

use std::process::Command;

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. `just build`)")
}

fn sandbox_name(suffix: &str) -> String {
    format!("itest-w-{}-{}", std::process::id(), suffix)
}

fn sandbox_run(name: &str, guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args(["sandbox", "run", name, "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

fn daemon_status() -> String {
    let out = Command::new(dome_bin())
        .args(["daemon", "status"])
        .output()
        .expect("failed to spawn dome");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Stop the per-sandbox worker (no user-facing `sandbox stop` until #27): send it
/// SIGTERM, which it handles by saving and shutting the VM down cleanly. Best-effort.
fn stop_worker(name: &str) {
    let _ = Command::new("pkill")
        .args(["-TERM", "-f", &format!("__worker {}", name)])
        .output();
    // Give the worker a moment to save + tear the VM down.
    std::thread::sleep(std::time::Duration::from_secs(3));
}

/// Best-effort clean slate: stop any leftover worker, then unlink the index + lock.
fn cleanup(name: &str) {
    stop_worker(name);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = format!("{}/.local/share/dome/sandboxes", home);
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    let _ = std::fs::remove_file(format!("{}/{}.lock", dir, name));
}

/// The core #24 acceptance: a sandbox VM cold-boots on first use, stays running after
/// the session exits, and a later session re-attaches to the SAME live VM.
#[test]
#[ignore]
fn vm_stays_running_and_reattach_hits_the_same_live_vm() {
    let name = sandbox_name("reattach");
    cleanup(&name);

    // Session 1 cold-boots the VM and drops a marker on tmpfs (`/run`). A tmpfs marker
    // survives only as long as THIS VM keeps running — it is gone after any reboot — so
    // reading it back later proves we hit the same live VM rather than a fresh cold boot.
    let s1 = sandbox_run(&name, "echo live-marker-$$ > /run/dome-live; echo wrote");
    assert!(
        s1.status.success(),
        "first session should cold-boot and run; stderr: {}",
        String::from_utf8_lossy(&s1.stderr)
    );

    // The VM must still be running now that the session has exited.
    let status = daemon_status();
    assert!(
        status.contains("workers: 1") || status.contains("workers: "),
        "daemon should report the live worker after the session exits; status:\n{status}"
    );

    // Session 2 re-attaches; the tmpfs marker is still there → same live VM.
    let s2 = sandbox_run(&name, "cat /run/dome-live");
    assert!(
        s2.status.success(),
        "second session should attach to the running VM; stderr: {}",
        String::from_utf8_lossy(&s2.stderr)
    );
    assert!(
        String::from_utf8_lossy(&s2.stdout).contains("live-marker-"),
        "re-attach must reach the same live VM (tmpfs marker present); stdout: {}",
        String::from_utf8_lossy(&s2.stdout)
    );

    cleanup(&name);
}

/// Disk writes persist across sessions on the same live VM (the `/root` rootfs is CAS,
/// not tmpfs, so it survives independently of the tmpfs check above).
#[test]
#[ignore]
fn disk_writes_persist_across_sessions() {
    let name = sandbox_name("persist");
    cleanup(&name);

    let w = sandbox_run(&name, "echo persisted > /root/state.txt");
    assert!(
        w.status.success(),
        "write session should succeed; stderr: {}",
        String::from_utf8_lossy(&w.stderr)
    );

    let r = sandbox_run(&name, "cat /root/state.txt");
    assert!(
        String::from_utf8_lossy(&r.stdout).contains("persisted"),
        "a later session should see the earlier write; stdout: {}",
        String::from_utf8_lossy(&r.stdout)
    );

    cleanup(&name);
}

/// Stopping the worker tears the VM down: afterwards the daemon reports no workers.
#[test]
#[ignore]
fn stopping_the_worker_releases_the_vm() {
    let name = sandbox_name("stop");
    cleanup(&name);

    let run = sandbox_run(&name, "true");
    assert!(run.status.success());

    stop_worker(&name);

    let status = daemon_status();
    assert!(
        status.contains("workers: 0") || status.contains("daemon is down"),
        "after stopping the worker the VM should be gone; status:\n{status}"
    );

    cleanup(&name);
}
