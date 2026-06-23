//! Integration tests for issue #27: lifecycle guards + crash supervision.
//!
//! Boots REAL persistent VMs through workers, so they require a codesigned binary and are
//! `#[ignore]`d by default (excluded from CI's `cargo test`). Run with:
//!   DOME_BIN=target/debug/dome cargo test -p dome-cli --test sandbox_lifecycle -- --ignored
//!
//! The control-plane logic (stop guard naming the attached count, --force, rm refusal,
//! crash → failed/`sandbox.crashed`, failed→cold-boot clears the marker) has hypervisor-
//! free unit tests in `src/daemon.rs`; these prove the same contract end-to-end against a
//! live VM.

use std::process::{Command, Stdio};
use std::time::Duration;

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. `just build`)")
}

fn sandbox_name(suffix: &str) -> String {
    format!("itest-life-{}-{}", std::process::id(), suffix)
}

fn dome(args: &[&str]) -> std::process::Output {
    Command::new(dome_bin())
        .args(args)
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

/// SIGKILL the per-sandbox worker — a hard crash with NO graceful save, so domed's reaper
/// must notice the unexpected exit and mark the sandbox failed.
fn kill_worker(name: &str) {
    let _ = Command::new("pkill")
        .args(["-KILL", "-f", &format!("__worker {}", name)])
        .output();
    std::thread::sleep(Duration::from_secs(2));
}

fn cleanup(name: &str) {
    let _ = dome(&["sandbox", "stop", "--force", name]);
    std::thread::sleep(Duration::from_secs(2));
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = format!("{}/.local/share/dome/sandboxes", home);
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    let _ = std::fs::remove_file(format!("{}/{}.lock", dir, name));
    let wdir = format!("{}/.local/share/dome/daemon/workers", home);
    let _ = std::fs::remove_file(format!("{}/{}.failed", wdir, name));
}

/// `stop` refuses while a terminal is attached (naming the count), `--force` detaches it
/// and stops, and `rm` is refused while running but succeeds once stopped.
#[test]
#[ignore]
fn stop_guard_force_and_rm_guard() {
    let name = sandbox_name("guard");
    cleanup(&name);

    // Terminal A cold-boots the VM and stays attached so a terminal is "in use".
    let mut a = Command::new(dome_bin())
        .args(["sandbox", "run", &name, "--", "sh", "-c", "sleep 20"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn terminal A");
    std::thread::sleep(Duration::from_secs(6));

    // `rm` on a running sandbox is refused and instructs to stop first.
    let rm = dome(&["sandbox", "rm", &name]);
    assert!(!rm.status.success(), "rm must refuse a running sandbox");
    let rm_err = String::from_utf8_lossy(&rm.stderr);
    assert!(
        rm_err.contains("running") && rm_err.contains("stop"),
        "rm refusal must instruct to stop first; stderr: {rm_err}"
    );

    // `stop` (no --force) is refused while a terminal is attached, naming the count.
    let stop = dome(&["sandbox", "stop", &name]);
    assert!(
        !stop.status.success(),
        "stop must refuse with a terminal attached"
    );
    assert!(
        String::from_utf8_lossy(&stop.stderr).contains("attached"),
        "stop refusal must name the attached terminals; stderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    // `--force` detaches the terminal and stops the VM.
    let forced = dome(&["sandbox", "stop", "--force", &name]);
    assert!(
        forced.status.success(),
        "stop --force must succeed; stderr: {}",
        String::from_utf8_lossy(&forced.stderr)
    );
    let _ = a.wait();

    // Now stopped, `rm` succeeds.
    let rm = dome(&["sandbox", "rm", &name]);
    assert!(
        rm.status.success(),
        "rm must succeed once the sandbox is stopped; stderr: {}",
        String::from_utf8_lossy(&rm.stderr)
    );

    cleanup(&name);
}

/// An unexpected worker exit marks the sandbox failed (`ls` shows it), and a subsequent
/// `shell`/`run` cold-boots from the last saved index (the write made before the crash,
/// once saved, is still there).
#[test]
#[ignore]
fn crash_marks_failed_and_reshell_cold_boots_from_last_save() {
    let name = sandbox_name("crash");
    cleanup(&name);

    // Cold-boot, write + save a marker durably, then keep the VM live.
    let mut a = Command::new(dome_bin())
        .args([
            "sandbox",
            "run",
            &name,
            "--",
            "sh",
            "-c",
            "echo survived-the-crash > /root/marker.txt; sleep 20",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn terminal A");
    std::thread::sleep(Duration::from_secs(6));
    let save = dome(&["sandbox", "save", &name]);
    assert!(save.status.success(), "save should succeed while running");

    // Hard-crash the worker (no graceful save) and let domed's reaper notice.
    kill_worker(&name);
    let _ = a.wait();
    std::thread::sleep(Duration::from_secs(2));

    // `ls` reports the sandbox as failed (not running, not silently idle).
    let ls = dome(&["sandbox", "ls"]);
    let ls_out = String::from_utf8_lossy(&ls.stdout);
    assert!(
        ls_out
            .lines()
            .any(|l| l.contains(&name) && l.contains("failed")),
        "a crashed sandbox must list as failed; ls:\n{ls_out}"
    );

    // Re-`run` cold-boots from the last saved index and sees the pre-crash write.
    let c = dome(&["sandbox", "run", &name, "--", "cat", "/root/marker.txt"]);
    assert!(
        String::from_utf8_lossy(&c.stdout).contains("survived-the-crash"),
        "a re-shell must cold-boot from the last save; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&c.stdout),
        String::from_utf8_lossy(&c.stderr)
    );

    cleanup(&name);
}
