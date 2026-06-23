//! Integration tests for seeding persistent sandboxes (`--from`) and `sandbox
//! create`. Boots a real VM — requires a codesigned binary. All #[ignore]d by
//! default. Run with:
//!   DOME_BIN=target/debug/dome cargo test -p dome-cli --test sandbox_seed -- --ignored

use std::path::Path;
use std::process::Command;

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. just build)")
}

fn data_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/.local/share/dome", home)
}

/// A unique name per test run so repeated runs don't collide on the global namespace.
fn unique(suffix: &str) -> String {
    format!("itest-{}-{}", std::process::id(), suffix)
}

fn sandbox_run(name: &str, guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args(["sandbox", "run", name, "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

fn sandbox_run_from(name: &str, from: &str, guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args([
            "sandbox", "run", name, "--from", from, "--", "sh", "-c", guest_cmd,
        ])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

fn sandbox_create(name: &str, from: Option<&str>) -> std::process::Output {
    let mut args = vec![
        "sandbox".to_string(),
        "create".to_string(),
        name.to_string(),
    ];
    if let Some(f) = from {
        args.push("--from".to_string());
        args.push(f.to_string());
    }
    Command::new(dome_bin())
        .args(&args)
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

fn checkpoint_create(name: &str, guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args(["checkpoint", "create", name, "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

/// Spawn a sandbox session without waiting, so a second command can run while this
/// session holds the persistence lock. The owner acquires the lock at startup (before
/// the slow VM boot), so a short pause is enough for the lock to be held.
fn sandbox_spawn(name: &str, guest_cmd: &str) -> std::process::Child {
    Command::new(dome_bin())
        .args(["sandbox", "run", name, "--", "sh", "-c", guest_cmd])
        .spawn()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

fn sandbox_index(name: &str) -> String {
    format!("{}/sandboxes/{}.idx", data_dir(), name)
}

/// Stop a sandbox's persistent worker (since #24 the VM outlives a session and holds the
/// persistence lock; there is no user-facing `sandbox stop` until #27). SIGTERM makes it
/// save and tear the VM down, releasing the lock. Best-effort.
fn stop_worker(name: &str) {
    let _ = std::process::Command::new("pkill")
        .args(["-TERM", "-f", &format!("__worker {}", name)])
        .output();
    std::thread::sleep(std::time::Duration::from_secs(3));
}

fn rm_sandbox(name: &str) {
    // Since #24 a live worker owns the VM and lock; stop it before unlinking.
    stop_worker(name);
    let dir = format!("{}/sandboxes", data_dir());
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    let _ = std::fs::remove_file(format!("{}/{}.lock", dir, name));
}

fn rm_checkpoint(name: &str) {
    let dir = format!("{}/checkpoints", data_dir());
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    let _ = std::fs::remove_file(format!("{}/{}.ext4", dir, name));
}

/// `sandbox create` with no `--from` materializes an index from the base image
/// without booting; a later `run` resumes it and sees a pristine base filesystem.
#[test]
#[ignore]
fn create_without_from_materializes_from_base() {
    let name = unique("create-base");
    rm_sandbox(&name);

    let created = sandbox_create(&name, None);
    assert!(
        created.status.success(),
        "create should succeed; stderr: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(
        Path::new(&sandbox_index(&name)).exists(),
        "create must write the sandbox index"
    );

    // The materialized sandbox boots and behaves like a fresh base.
    let read = sandbox_run(&name, "cat /root/state.txt 2>/dev/null || echo PRISTINE");
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("PRISTINE"),
        "a base-materialized sandbox should have no prior app state; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_sandbox(&name);
}

/// `sandbox create --from <checkpoint>` produces a sandbox whose initial disk state
/// matches the checkpoint's content.
#[test]
#[ignore]
fn create_from_checkpoint_inherits_content() {
    let ckpt = unique("seed-ckpt");
    let name = unique("from-ckpt");
    rm_checkpoint(&ckpt);
    rm_sandbox(&name);

    let made = checkpoint_create(&ckpt, "echo seeded-by-checkpoint > /root/seed.txt");
    assert!(
        made.status.success(),
        "checkpoint create should succeed; stderr: {}",
        String::from_utf8_lossy(&made.stderr)
    );

    let created = sandbox_create(&name, Some(&ckpt));
    assert!(
        created.status.success(),
        "create --from checkpoint should succeed; stderr: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    let read = sandbox_run(&name, "cat /root/seed.txt");
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("seeded-by-checkpoint"),
        "seeded sandbox should carry the checkpoint's content; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_checkpoint(&ckpt);
    rm_sandbox(&name);
}

/// `sandbox create --from <other-sandbox>` seeds from another sandbox's current
/// saved state.
#[test]
#[ignore]
fn create_from_another_sandbox_inherits_saved_state() {
    let src = unique("seed-src-sb");
    let dst = unique("from-sb");
    rm_sandbox(&src);
    rm_sandbox(&dst);

    // Establish saved state in the source sandbox.
    let seeded = sandbox_run(&src, "echo seeded-by-sandbox > /root/seed.txt");
    assert!(
        seeded.status.success(),
        "seeding the source sandbox should succeed; stderr: {}",
        String::from_utf8_lossy(&seeded.stderr)
    );
    // Stop the source worker so its writes are flushed to its on-disk index — `create
    // --from <sandbox>` seeds from that saved index, not the still-live VM (since #24).
    stop_worker(&src);

    let created = sandbox_create(&dst, Some(&src));
    assert!(
        created.status.success(),
        "create --from sandbox should succeed; stderr: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    let read = sandbox_run(&dst, "cat /root/seed.txt");
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("seeded-by-sandbox"),
        "forked sandbox should carry the source's saved state; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_sandbox(&src);
    rm_sandbox(&dst);
}

/// `sandbox run <name> --from <X>` seeds at creation when the sandbox is absent.
#[test]
#[ignore]
fn run_from_seeds_at_creation_when_absent() {
    let ckpt = unique("run-seed-ckpt");
    let name = unique("run-from");
    rm_checkpoint(&ckpt);
    rm_sandbox(&name);

    checkpoint_create(&ckpt, "echo run-seed > /root/seed.txt");

    // First use of the sandbox seeds it from the checkpoint and reads the content back.
    let read = sandbox_run_from(&name, &ckpt, "cat /root/seed.txt");
    assert!(
        read.status.success(),
        "run --from should boot the seeded sandbox; stderr: {}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("run-seed"),
        "run --from should seed at creation; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_checkpoint(&ckpt);
    rm_sandbox(&name);
}

/// Passing `--from` when the sandbox already exists hard-errors and does not modify
/// the existing sandbox.
#[test]
#[ignore]
fn from_on_existing_sandbox_errors_and_preserves_state() {
    let ckpt = unique("clobber-ckpt");
    let name = unique("existing");
    rm_checkpoint(&ckpt);
    rm_sandbox(&name);

    // The existing sandbox holds the original marker.
    let original = sandbox_run(&name, "echo original > /root/marker.txt");
    assert!(original.status.success());
    // Stop the worker so the sandbox is a stopped, on-disk index again — the next
    // invocation then cold-boots and hits the `--from`-on-existing gate (rather than
    // attaching to a still-running worker, which would ignore `--from`).
    stop_worker(&name);

    // A checkpoint with a DIFFERENT marker that must never overwrite the sandbox.
    checkpoint_create(&ckpt, "echo clobber > /root/marker.txt");

    let attempt = sandbox_run_from(&name, &ckpt, "true");
    assert!(
        !attempt.status.success(),
        "re-seeding an existing sandbox must fail"
    );
    assert!(
        String::from_utf8_lossy(&attempt.stderr).contains("already exists"),
        "error should explain the sandbox already exists; stderr: {}",
        String::from_utf8_lossy(&attempt.stderr)
    );

    // The original state must be intact.
    let read = sandbox_run(&name, "cat /root/marker.txt");
    let out = String::from_utf8_lossy(&read.stdout);
    assert!(
        out.contains("original") && !out.contains("clobber"),
        "the existing sandbox must be untouched by a refused re-seed; stdout: {}",
        out
    );

    rm_checkpoint(&ckpt);
    rm_sandbox(&name);
}

/// `create <name>` on a sandbox that a live session is currently using must
/// hard-error rather than materialize a competing index the running owner would
/// clobber on exit. The running owner's eventual saved state must be intact.
#[test]
#[ignore]
fn create_on_a_sandbox_in_use_errors() {
    let name = unique("create-inuse");
    rm_sandbox(&name);

    // An owner holds the persistence lock for a while, writing a marker it will save.
    let mut owner = sandbox_spawn(&name, "echo owner-state > /root/marker.txt; sleep 25");
    std::thread::sleep(std::time::Duration::from_secs(4));

    let attempt = sandbox_create(&name, None);
    assert!(
        !attempt.status.success(),
        "create on an in-use sandbox must fail"
    );
    assert!(
        String::from_utf8_lossy(&attempt.stderr).contains("in use"),
        "error should explain the sandbox is in use; stderr: {}",
        String::from_utf8_lossy(&attempt.stderr)
    );

    owner.wait().expect("owner session should exit");

    // The owner's state must survive — create never raced its save.
    let read = sandbox_run(&name, "cat /root/marker.txt");
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("owner-state"),
        "the running owner's saved state must be intact; stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    rm_sandbox(&name);
}

/// Since #24, `run <name> --from <X>` on an already-running sandbox does not seed (you
/// cannot re-seed a live VM): it attaches to the running VM and ignores `--from` with a
/// clear warning, rather than erroring or silently forking from base.
#[test]
#[ignore]
fn run_from_on_a_running_sandbox_attaches_and_warns() {
    let ckpt = unique("inuse-seed-ckpt");
    let name = unique("run-from-inuse");
    rm_checkpoint(&ckpt);
    rm_sandbox(&name);

    checkpoint_create(&ckpt, "echo seed > /root/seed.txt");

    // An owner cold-boots the VM (from base — no --from) and keeps a session open.
    let mut owner = sandbox_spawn(&name, "sleep 25");
    std::thread::sleep(std::time::Duration::from_secs(8));

    // A second invocation passes --from while the VM is already running: it attaches to
    // the live VM and runs, ignoring --from with a warning (the VM was not re-seeded).
    let attempt = sandbox_run_from(&name, &ckpt, "true");
    assert!(
        attempt.status.success(),
        "run --from on a running sandbox should attach and run, not fail; stderr: {}",
        String::from_utf8_lossy(&attempt.stderr)
    );
    let stderr = String::from_utf8_lossy(&attempt.stderr);
    assert!(
        stderr.contains("already running") && stderr.contains("ignored"),
        "the user should be warned that --from/flags are ignored on a running VM; stderr: {stderr}"
    );

    let _ = owner.wait();

    rm_checkpoint(&ckpt);
    rm_sandbox(&name);
}

/// `create --from <missing>` fails clearly and writes nothing.
#[test]
#[ignore]
fn create_from_missing_seed_errors() {
    let name = unique("missing-seed");
    rm_sandbox(&name);

    let created = sandbox_create(&name, Some("does-not-exist-anywhere"));
    assert!(
        !created.status.success(),
        "create from a missing seed must fail"
    );
    assert!(
        !Path::new(&sandbox_index(&name)).exists(),
        "a failed seed must not leave a sandbox index behind"
    );

    rm_sandbox(&name);
}
