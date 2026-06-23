//! Integration tests for per-sandbox config persistence (`dome sandbox create`/`config`
//! and cold-boot-uses-config). The cold-boot test boots a real VM and requires a codesigned
//! binary; all #[ignore]d by default. Run with:
//!   DOME_BIN=target/debug/dome cargo test -p dome-cli --test sandbox_config -- --ignored

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

fn unique(suffix: &str) -> String {
    format!("itest-{}-{}", std::process::id(), suffix)
}

fn config_path(name: &str) -> String {
    format!("{}/sandboxes/{}.config.json", data_dir(), name)
}

fn sandbox_index(name: &str) -> String {
    format!("{}/sandboxes/{}.idx", data_dir(), name)
}

fn stop_worker(name: &str) {
    let _ = Command::new("pkill")
        .args(["-TERM", "-f", &format!("__worker {}", name)])
        .output();
    std::thread::sleep(std::time::Duration::from_secs(3));
}

fn rm_sandbox(name: &str) {
    stop_worker(name);
    let dir = format!("{}/sandboxes", data_dir());
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    let _ = std::fs::remove_file(format!("{}/{}.lock", dir, name));
    let _ = std::fs::remove_file(format!("{}/{}.config.json", dir, name));
}

/// `sandbox create` persists a config sidecar capturing the flags, without booting a VM.
#[test]
#[ignore]
fn create_persists_config_without_booting() {
    let name = unique("cfg-create");
    rm_sandbox(&name);

    let created = Command::new(dome_bin())
        .args([
            "sandbox", "create", &name, "--cpus", "4", "--memory", "3072",
        ])
        .output()
        .expect("failed to spawn dome");
    assert!(
        created.status.success(),
        "create should succeed; stderr: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    let cfg = std::fs::read_to_string(config_path(&name)).expect("config sidecar must exist");
    assert!(
        cfg.contains("\"cpus\": 4") && cfg.contains("\"memory\": 3072"),
        "config must capture the create flags; got: {cfg}"
    );

    rm_sandbox(&name);
}

/// `sandbox config <name>` edits the persisted metadata; the change is written to the
/// sidecar (and would take effect on the next cold boot).
#[test]
#[ignore]
fn config_edits_the_persisted_metadata() {
    let name = unique("cfg-edit");
    rm_sandbox(&name);

    Command::new(dome_bin())
        .args(["sandbox", "create", &name, "--cpus", "2"])
        .output()
        .expect("failed to spawn dome");

    let edited = Command::new(dome_bin())
        .args(["sandbox", "config", &name, "--cpus", "8", "--allow-net"])
        .output()
        .expect("failed to spawn dome");
    assert!(
        edited.status.success(),
        "config edit should succeed; stderr: {}",
        String::from_utf8_lossy(&edited.stderr)
    );

    let cfg = std::fs::read_to_string(config_path(&name)).expect("config sidecar must exist");
    assert!(
        cfg.contains("\"cpus\": 8") && cfg.contains("\"allow_net\": true"),
        "config edit must update the sidecar; got: {cfg}"
    );

    rm_sandbox(&name);
}

/// A cold boot uses the persisted config (cpus/memory), not the attaching invocation's
/// flags: a sandbox created with 2 cpus boots with 2 even when `run` passes `--cpus 1`.
#[test]
#[ignore]
fn cold_boot_uses_persisted_config_not_invocation_flags() {
    let name = unique("cfg-coldboot");
    rm_sandbox(&name);

    // Create pinned to a distinctive cpu count.
    let created = Command::new(dome_bin())
        .args(["sandbox", "create", &name, "--cpus", "2"])
        .output()
        .expect("failed to spawn dome");
    assert!(created.status.success());
    assert!(Path::new(&sandbox_index(&name)).exists());

    // Cold-boot via `run`, deliberately passing a different --cpus that must be ignored.
    // The guest reports its online CPU count; it should reflect the persisted 2, not 1.
    let out = Command::new(dome_bin())
        .args([
            "sandbox", "run", &name, "--cpus", "1", "--", "sh", "-c", "nproc",
        ])
        .output()
        .expect("failed to spawn dome");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().contains('2'),
        "cold boot must use the persisted cpus=2, not the invocation --cpus 1; nproc: {stdout}"
    );

    rm_sandbox(&name);
}
