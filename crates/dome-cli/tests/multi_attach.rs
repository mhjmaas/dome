//! Integration test for issue #25: multi-terminal attach to the same live sandbox VM.
//!
//! Boots a REAL persistent VM through a worker, so it requires a codesigned binary and is
//! `#[ignore]`d by default (excluded from CI's `cargo test`). Run with:
//!   DOME_BIN=target/debug/dome cargo test -p dome-cli --test multi_attach -- --ignored
//!
//! It exercises the #25 contract:
//!   * a second `dome sandbox run/shell <name>` attaches to the EXISTING live VM (no new
//!     VM, no read-only fork),
//!   * files written in one attached terminal are immediately visible in another,
//!   * `ls` reflects the live attached-terminal count (>=1 while a session is attached,
//!     0 once they all detach),
//!   * closing every terminal leaves the VM running.
//!
//! The routing/refcount logic itself has hypervisor-free unit tests in `src/worker.rs`
//! (the `Count` op round-trip) and `src/daemon.rs` (overlay applies the live count, and a
//! worker with zero attached terminals still reads as `running`).

use std::process::{Command, Stdio};
use std::time::Duration;

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. `just build`)")
}

fn sandbox_name(suffix: &str) -> String {
    format!("itest-m-{}-{}", std::process::id(), suffix)
}

fn sandbox_run(name: &str, guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args(["sandbox", "run", name, "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

/// The `ATTACHED` count `ls` reports for `name` (None if the sandbox is not listed).
fn ls_attached(name: &str) -> Option<usize> {
    let out = Command::new(dome_bin())
        .args(["sandbox", "ls"])
        .output()
        .expect("failed to spawn dome");
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // Columns: NAME SIZE BASE STATE ATTACHED CREATED
        if cols.first() == Some(&name) && cols.len() >= 5 {
            return cols[4].parse::<usize>().ok();
        }
    }
    None
}

fn ls_state(name: &str) -> Option<String> {
    let out = Command::new(dome_bin())
        .args(["sandbox", "ls"])
        .output()
        .expect("failed to spawn dome");
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.first() == Some(&name) && cols.len() >= 4 {
            return Some(cols[3].to_string());
        }
    }
    None
}

/// Stop the per-sandbox worker (no user-facing `sandbox stop` until #27): SIGTERM it so
/// it saves + tears the VM down cleanly. Best-effort.
fn stop_worker(name: &str) {
    let _ = Command::new("pkill")
        .args(["-TERM", "-f", &format!("__worker {}", name)])
        .output();
    std::thread::sleep(Duration::from_secs(3));
}

fn cleanup(name: &str) {
    stop_worker(name);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = format!("{}/.local/share/dome/sandboxes", home);
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    let _ = std::fs::remove_file(format!("{}/{}.lock", dir, name));
}

/// Two terminals against one sandbox share one live, writable VM: a write in the first is
/// visible in the second, `ls` shows the live attached count, and the VM survives both
/// closing.
#[test]
#[ignore]
fn second_terminal_attaches_to_the_same_live_vm_and_sees_its_writes() {
    let name = sandbox_name("share");
    cleanup(&name);

    // Terminal A cold-boots the VM, writes a shared file, then stays attached (sleep) so a
    // concurrent terminal can observe it as a live, attached session.
    let mut a = Command::new(dome_bin())
        .args([
            "sandbox",
            "run",
            &name,
            "--",
            "sh",
            "-c",
            "echo from-terminal-a > /root/shared.txt; sleep 8",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn terminal A");

    // Give A time to cold-boot the VM and write the file before B attaches.
    std::thread::sleep(Duration::from_secs(6));

    // While A is still attached, `ls` must show the sandbox running with >=1 terminal.
    assert_eq!(
        ls_state(&name).as_deref(),
        Some("running"),
        "the sandbox should be running while terminal A is attached"
    );
    assert!(
        ls_attached(&name).unwrap_or(0) >= 1,
        "ls should report at least one attached terminal while A is live; got {:?}",
        ls_attached(&name)
    );

    // Terminal B attaches to the SAME live VM (A is still running, so this is not a cold
    // boot, not a read-only fork) and reads the file A wrote — cross-visible write.
    let b = sandbox_run(&name, "cat /root/shared.txt");
    assert!(
        b.status.success(),
        "terminal B should attach to the running VM; stderr: {}",
        String::from_utf8_lossy(&b.stderr)
    );
    assert!(
        String::from_utf8_lossy(&b.stdout).contains("from-terminal-a"),
        "B must see A's write live on the shared filesystem; stdout: {}",
        String::from_utf8_lossy(&b.stdout)
    );

    // Let terminal A finish; both terminals are now closed.
    let _ = a.wait();
    std::thread::sleep(Duration::from_secs(1));

    // The VM stays up after every terminal closes: still running, zero attached.
    assert_eq!(
        ls_state(&name).as_deref(),
        Some("running"),
        "closing all terminals must leave the VM running"
    );
    assert_eq!(
        ls_attached(&name),
        Some(0),
        "ls should report 0 attached once every terminal has detached"
    );

    // And a later attach still hits that same live VM.
    let c = sandbox_run(&name, "cat /root/shared.txt");
    assert!(
        String::from_utf8_lossy(&c.stdout).contains("from-terminal-a"),
        "a later attach must still reach the same live VM; stdout: {}",
        String::from_utf8_lossy(&c.stdout)
    );

    cleanup(&name);
}
