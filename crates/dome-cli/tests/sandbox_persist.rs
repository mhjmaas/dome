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

/// Stop a sandbox's persistent worker (since #24 the VM outlives a session; there is no
/// user-facing `sandbox stop` until #27). SIGTERM makes the worker save the sandbox and
/// shut the VM down cleanly, releasing the persistence lock. Best-effort; waits for the
/// save + teardown to complete.
fn stop_worker(name: &str) {
    let _ = std::process::Command::new("pkill")
        .args(["-TERM", "-f", &format!("__worker {}", name)])
        .output();
    std::thread::sleep(std::time::Duration::from_secs(3));
}

fn rm_sandbox(name: &str) {
    // Best-effort cleanup independent of the binary under test. Since #24 a live worker
    // owns the VM and the persistence lock, so stop it first; then remove the index (and
    // any lock left by a crashed session) directly via the data dir so a broken `rm`
    // can never strand other tests.
    stop_worker(name);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = format!("{}/.local/share/dome/sandboxes", home);
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    // Drop any lock left by a crashed session so it can't wedge the next run.
    let _ = std::fs::remove_file(format!("{}/{}.lock", dir, name));
}

/// Run the real `dome sandbox rm <name>` command.
fn sandbox_rm_cmd(name: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args(["sandbox", "rm", name])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

/// Run the real `dome prune` command (instance cleanup + CAS mark-and-sweep).
fn prune_cmd() -> std::process::Output {
    Command::new(dome_bin())
        .arg("prune")
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

fn sandbox_index_path(name: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/.local/share/dome/sandboxes/{}.idx", home, name)
}

/// Number of chunk files currently in the global CAS chunk store.
fn chunk_count() -> usize {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let chunks = format!("{}/.local/share/dome/chunks", home);
    std::fs::read_dir(&chunks).map(|d| d.count()).unwrap_or(0)
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

    // Stop the worker so it flushes the sandbox to its on-disk index, then the next
    // session cold-boots from that saved state — proving a non-zero-exit session's
    // writes are durable (since #24 the save happens on worker stop, not per session).
    stop_worker(&name);
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

/// Since #24 the ephemeral-fork model is gone: a sandbox is owned by one persistent
/// worker, and a second concurrent session attaches to the SAME live VM with full write
/// access (a shared, writable filesystem). So a write from a concurrent session is
/// immediately visible to the owner — the opposite of the old fork behaviour.
#[test]
#[ignore]
fn concurrent_session_shares_the_same_live_vm() {
    let name = sandbox_name("concurrent");
    rm_sandbox(&name);

    // Owner cold-boots the VM and keeps a session open while a second session runs.
    let mut owner = sandbox_spawn(&name, "sleep 25");
    // Give the worker time to cold-boot before the second session attaches.
    std::thread::sleep(std::time::Duration::from_secs(8));

    // A concurrent session writes a marker; it must NOT announce itself as a fork
    // (that model is gone) and its write lands on the shared live filesystem.
    let writer = sandbox_run(&name, "echo shared-write > /root/marker.txt");
    assert!(
        writer.status.success(),
        "a concurrent session should attach and run; stderr: {}",
        String::from_utf8_lossy(&writer.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&writer.stderr).contains("ephemeral fork"),
        "the ephemeral-fork model was removed in #24; stderr: {}",
        String::from_utf8_lossy(&writer.stderr)
    );

    // A third session sees the concurrent write immediately — same live VM, shared fs.
    let read = sandbox_run(&name, "cat /root/marker.txt");
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("shared-write"),
        "concurrent sessions share one live writable filesystem; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    let _ = owner.wait();
    rm_sandbox(&name);
}

/// `dome sandbox rm` on a name with no index reports a clear error and fails — this
/// path needs no VM, so it is cheap even though the suite is `#[ignore]`d.
#[test]
#[ignore]
fn rm_reports_a_clear_error_for_a_missing_sandbox() {
    let name = sandbox_name("rm-missing");
    rm_sandbox(&name); // ensure it really is absent

    let out = sandbox_rm_cmd(&name);
    assert!(
        !out.status.success(),
        "removing a non-existent sandbox should fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&name) && stderr.contains("not found"),
        "error should name the missing sandbox; stderr: {}",
        stderr
    );
}

/// End-to-end of issue #10: `rm` unlinks only the index (chunks survive), and a
/// subsequent `prune` mark-and-sweep reclaims the now-orphaned chunks while a different,
/// still-referenced sandbox keeps its data and resumes intact.
#[test]
#[ignore]
fn rm_then_prune_reclaims_orphans_while_keeping_referenced() {
    let victim = sandbox_name("gc-victim");
    let keeper = sandbox_name("gc-keeper");
    rm_sandbox(&victim);
    rm_sandbox(&keeper);

    // The keeper writes a unique marker plus a few MB of unique data, then persists.
    let k = sandbox_run(
        &keeper,
        "head -c 3000000 /dev/urandom > /root/keep.bin; echo keeper-marker > /root/marker.txt",
    );
    assert!(
        k.status.success(),
        "seeding the keeper sandbox should succeed; stderr: {}",
        String::from_utf8_lossy(&k.stderr)
    );

    // The victim writes its own few MB of unique data (so it owns distinct chunks).
    let v = sandbox_run(&victim, "head -c 3000000 /dev/urandom > /root/victim.bin");
    assert!(
        v.status.success(),
        "seeding the victim sandbox should succeed; stderr: {}",
        String::from_utf8_lossy(&v.stderr)
    );

    // Since #24 the save happens on worker stop, not per session, and a live worker
    // holds the persistence lock (so `rm` would refuse). Stop both workers so their
    // writes are flushed to their on-disk indexes and the lock is released.
    stop_worker(&keeper);
    stop_worker(&victim);

    let before = chunk_count();

    // rm unlinks only the index: it succeeds, the index is gone, and — crucially — the
    // chunk store is untouched (reclamation is deferred to prune).
    let rm = sandbox_rm_cmd(&victim);
    assert!(
        rm.status.success(),
        "rm should succeed; stderr: {}",
        String::from_utf8_lossy(&rm.stderr)
    );
    assert!(
        !std::path::Path::new(&sandbox_index_path(&victim)).exists(),
        "rm should unlink the sandbox index"
    );
    assert_eq!(
        chunk_count(),
        before,
        "rm must NOT delete chunks — that is deferred to prune"
    );

    // prune sweeps the now-unreferenced chunks: the store shrinks.
    let prune = prune_cmd();
    assert!(
        prune.status.success(),
        "prune should succeed; stderr: {}",
        String::from_utf8_lossy(&prune.stderr)
    );
    assert!(
        chunk_count() < before,
        "prune should reclaim the victim's orphaned chunks ({} -> {})",
        before,
        chunk_count()
    );

    // The keeper's referenced data survived the sweep and still resumes.
    let read = sandbox_run(&keeper, "cat /root/marker.txt");
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("keeper-marker"),
        "a still-referenced sandbox must keep its data through prune; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_sandbox(&victim);
    rm_sandbox(&keeper);
}
