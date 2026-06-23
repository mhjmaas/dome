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

/// `sandbox create` resolves dome.json + flags into a structured, versioned config sidecar,
/// without booting a VM. The sidecar must carry the schema `version` and the structured proxy
/// section (so a later cold boot reproduces from it without re-reading dome.json).
#[test]
#[ignore]
fn create_persists_a_structured_versioned_sidecar_without_booting() {
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
    assert!(
        cfg.contains("\"version\": 1"),
        "the sidecar must be versioned; got: {cfg}"
    );
    assert!(
        cfg.contains("\"proxy\""),
        "the sidecar must carry the structured proxy section; got: {cfg}"
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

/// A previously-enabled policy can be turned off with `--no-*`: a sandbox created with
/// `--allow-net` writes `allow_net: true`, and `config --no-allow-net` flips the sidecar to
/// `false` (so the next cold boot reproduces with networking disabled).
#[test]
#[ignore]
fn no_flag_disables_a_previously_enabled_policy_in_the_sidecar() {
    let name = unique("cfg-no-net");
    rm_sandbox(&name);

    Command::new(dome_bin())
        .args(["sandbox", "create", &name, "--allow-net"])
        .output()
        .expect("failed to spawn dome");
    let cfg = std::fs::read_to_string(config_path(&name)).expect("config sidecar must exist");
    assert!(
        cfg.contains("\"allow_net\": true"),
        "create --allow-net must enable the policy; got: {cfg}"
    );

    let edited = Command::new(dome_bin())
        .args(["sandbox", "config", &name, "--no-allow-net"])
        .output()
        .expect("failed to spawn dome");
    assert!(
        edited.status.success(),
        "config --no-allow-net should succeed; stderr: {}",
        String::from_utf8_lossy(&edited.stderr)
    );
    let cfg = std::fs::read_to_string(config_path(&name)).expect("config sidecar must exist");
    assert!(
        cfg.contains("\"allow_net\": false"),
        "--no-allow-net must flip the sidecar to disabled; got: {cfg}"
    );

    rm_sandbox(&name);
}

/// Flags always win even while a VM is running (#45): a config flag passed to `run` on a
/// running sandbox is persisted to the sidecar and the user is told it applies on the next
/// boot (the live VM keeps its current config until stopped).
#[test]
#[ignore]
fn flag_on_a_running_sandbox_persists_and_reports_next_boot() {
    let name = unique("cfg-running");
    rm_sandbox(&name);

    // Create with a distinctive cpu count, then keep a session (and thus the VM) alive.
    let created = Command::new(dome_bin())
        .args(["sandbox", "create", &name, "--cpus", "2"])
        .output()
        .expect("failed to spawn dome");
    assert!(created.status.success());

    let mut owner = Command::new(dome_bin())
        .args(["sandbox", "run", &name, "--", "sh", "-c", "sleep 25"])
        .spawn()
        .expect("failed to spawn owner session");
    std::thread::sleep(std::time::Duration::from_secs(8));

    // Change --cpus while the VM is running: it must persist and report a next-boot change.
    let out = Command::new(dome_bin())
        .args(["sandbox", "run", &name, "--cpus", "1", "--", "true"])
        .output()
        .expect("failed to spawn dome");
    assert!(
        out.status.success(),
        "run on a running sandbox should attach and succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("next cold boot") && stderr.contains("--cpus 1"),
        "a flag while running must report it applies on the next boot, naming the change; \
         stderr: {stderr}"
    );

    // The sidecar is updated immediately even though the running VM keeps cpus=2 until stopped.
    let cfg = std::fs::read_to_string(config_path(&name)).expect("config sidecar must exist");
    assert!(
        cfg.contains("\"cpus\": 1"),
        "a flag while running must update the sidecar to cpus=1; got: {cfg}"
    );

    let _ = owner.wait();
    rm_sandbox(&name);
}

/// Flags always win on an existing sandbox (#45): a config flag passed to `run` resolves
/// into and updates the sidecar before the cold boot, so a sandbox created with 2 cpus boots
/// with 1 when `run` passes `--cpus 1`, and the sidecar is updated to match.
#[test]
#[ignore]
fn cold_boot_applies_invocation_flags_and_updates_sidecar() {
    let name = unique("cfg-coldboot");
    rm_sandbox(&name);

    // Create pinned to a distinctive cpu count.
    let created = Command::new(dome_bin())
        .args(["sandbox", "create", &name, "--cpus", "2"])
        .output()
        .expect("failed to spawn dome");
    assert!(created.status.success());
    assert!(Path::new(&sandbox_index(&name)).exists());

    // Cold-boot via `run`, passing a different --cpus that must win and update the sidecar.
    // The guest reports its online CPU count; it should reflect the requested 1, not 2.
    let out = Command::new(dome_bin())
        .args([
            "sandbox", "run", &name, "--cpus", "1", "--", "sh", "-c", "nproc",
        ])
        .output()
        .expect("failed to spawn dome");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().contains('1'),
        "flags win on an existing sandbox: cold boot must use the requested cpus=1; nproc: {stdout}"
    );

    // The flag also resolved into the sidecar, so it is now the persisted truth.
    let cfg = std::fs::read_to_string(config_path(&name)).expect("config sidecar must exist");
    assert!(
        cfg.contains("\"cpus\": 1"),
        "a flag passed to run must update the sidecar to cpus=1; got: {cfg}"
    );

    rm_sandbox(&name);
}
