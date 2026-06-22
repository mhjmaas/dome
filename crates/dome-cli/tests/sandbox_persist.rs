//! Integration tests for persistent sandboxes. Boots a real VM — requires a
//! codesigned binary. All #[ignore]d by default. Run with:
//!   DOME_BIN=target/debug/dome cargo test -p dome-cli --test sandbox_persist -- --ignored

use std::process::Command;

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. just build)")
}

/// A unique sandbox name per test run so repeated runs don't collide on the global
/// sandbox namespace. Uses the test process pid plus a caller-supplied suffix.
fn sandbox_name(suffix: &str) -> String {
    format!("itest-{}-{}", std::process::id(), suffix)
}

fn sandbox_run(name: &str, guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args(["sandbox", "run", name, "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

/// Spawn a sandbox session without waiting for it, so a second session can run
/// concurrently against the same sandbox.
fn sandbox_spawn(name: &str, guest_cmd: &str) -> std::process::Child {
    Command::new(dome_bin())
        .args(["sandbox", "run", name, "--", "sh", "-c", guest_cmd])
        .spawn()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

fn ephemeral_run(guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args(["run", "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

fn rm_sandbox(name: &str) {
    // Best-effort cleanup; sandbox rm lands in a later slice, so remove the index
    // directly via the data dir if the command is unavailable.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = format!("{}/.local/share/dome/sandboxes", home);
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    // Drop any lock left by a crashed session so it can't wedge the next run.
    let _ = std::fs::remove_file(format!("{}/{}.lock", dir, name));
}

#[test]
#[ignore]
fn persistence_round_trip() {
    let name = sandbox_name("roundtrip");
    rm_sandbox(&name);

    // First session writes a file into the persistent root filesystem.
    let write = sandbox_run(&name, "echo persisted-content > /root/state.txt");
    assert!(
        write.status.success(),
        "first sandbox run should succeed; stderr: {}",
        String::from_utf8_lossy(&write.stderr)
    );

    // Second session resumes and reads it back.
    let read = sandbox_run(&name, "cat /root/state.txt");
    assert!(
        read.status.success(),
        "second sandbox run should succeed; stderr: {}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("persisted-content"),
        "resumed sandbox should see the previously written file; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_sandbox(&name);
}

#[test]
#[ignore]
fn ephemeral_run_does_not_see_sandbox_state() {
    let name = sandbox_name("isolation");
    rm_sandbox(&name);

    sandbox_run(&name, "echo secret > /root/state.txt");

    // A plain ephemeral run boots from the base image and must not see it.
    let read = ephemeral_run("cat /root/state.txt 2>/dev/null || echo MISSING");
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("MISSING"),
        "ephemeral run must not see sandbox-persisted state; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_sandbox(&name);
}

#[test]
#[ignore]
fn save_happens_on_nonzero_exit() {
    let name = sandbox_name("nonzero");
    rm_sandbox(&name);

    // The command writes a file and then exits non-zero (a "failed build").
    let failed = sandbox_run(&name, "echo built > /root/artifact.txt; exit 1");
    assert_eq!(
        failed.status.code(),
        Some(1),
        "exit code should propagate from the guest command"
    );

    // The disk state must still have been saved.
    let read = sandbox_run(&name, "cat /root/artifact.txt");
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("built"),
        "state from a non-zero-exit session should still persist; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_sandbox(&name);
}

#[test]
#[ignore]
fn lazy_create_then_resume() {
    let name = sandbox_name("lazy");
    rm_sandbox(&name);

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let idx = format!("{}/.local/share/dome/sandboxes/{}.idx", home, name);
    assert!(
        !std::path::Path::new(&idx).exists(),
        "sandbox index should not exist before first use"
    );

    let first = sandbox_run(&name, "true");
    assert!(first.status.success());
    assert!(
        std::path::Path::new(&idx).exists(),
        "first sandbox run should lazily create the index"
    );

    let second = sandbox_run(&name, "true");
    assert!(
        second.status.success(),
        "second run should resume the sandbox"
    );

    rm_sandbox(&name);
}

/// A second concurrent session on a locked sandbox must boot as an ephemeral fork:
/// it runs fully but never writes back to the owner's saved index, and it announces
/// itself so the user knows its changes are discarded.
#[test]
#[ignore]
fn concurrent_fork_does_not_alter_owner_saved_state() {
    let name = sandbox_name("concurrent");
    rm_sandbox(&name);

    // Seed the sandbox with a known marker and let the owner persist it.
    let seed = sandbox_run(&name, "echo owner-original > /root/marker.txt");
    assert!(
        seed.status.success(),
        "seeding the sandbox should succeed; stderr: {}",
        String::from_utf8_lossy(&seed.stderr)
    );

    // The owner holds the persistence lock for a while without changing the marker.
    // It acquires the lock early (before the slow VM boot), so by the time the fork
    // starts the lock is already held.
    let mut owner = sandbox_spawn(&name, "sleep 25");

    // Give the owner time to start and acquire the lock before the fork begins.
    std::thread::sleep(std::time::Duration::from_secs(6));

    // The fork tries to overwrite the marker. Because the sandbox is locked, this
    // session is an ephemeral fork and its write must be discarded.
    let fork = sandbox_run(&name, "echo fork-wrote-this > /root/marker.txt");
    assert!(
        String::from_utf8_lossy(&fork.stderr).contains("ephemeral fork"),
        "a concurrent session should announce itself as an ephemeral fork; stderr: {}",
        String::from_utf8_lossy(&fork.stderr)
    );

    // Let the owner exit cleanly and save (the marker unchanged).
    owner.wait().expect("owner session should exit");

    // Resuming the sandbox must show the OWNER's state, never the fork's write.
    let read = sandbox_run(&name, "cat /root/marker.txt");
    let out = String::from_utf8_lossy(&read.stdout);
    assert!(
        out.contains("owner-original"),
        "saved state should reflect the owner, not the fork; stdout: {}",
        out
    );
    assert!(
        !out.contains("fork-wrote-this"),
        "the ephemeral fork's write must NOT be persisted; stdout: {}",
        out
    );

    rm_sandbox(&name);
}
