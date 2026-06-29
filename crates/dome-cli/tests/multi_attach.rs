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

use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
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

/// The whitespace-split columns of the `ls` row for `name`, if listed. Column indices are
/// NOT fixed: the SIZE field is rendered as `<n> <unit> (cas)` (three whitespace tokens) and
/// CREATED as `57m ago` (two), so the STATE/ATTACHED columns can't be read by a constant
/// offset — locate STATE by its known value instead.
fn ls_cols(name: &str) -> Option<Vec<String>> {
    let out = Command::new(dome_bin())
        .args(["sandbox", "ls"])
        .output()
        .expect("failed to spawn dome");
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .map(|l| l.split_whitespace().map(str::to_string).collect::<Vec<_>>())
        .find(|cols| cols.first().map(String::as_str) == Some(name))
}

/// The `STATE` `ls` reports for `name` (the one token from the known state set).
fn ls_state(name: &str) -> Option<String> {
    let cols = ls_cols(name)?;
    cols.into_iter()
        .find(|c| matches!(c.as_str(), "running" | "idle" | "failed"))
}

/// The `ATTACHED` count `ls` reports for `name`: the integer column immediately after STATE.
fn ls_attached(name: &str) -> Option<usize> {
    let cols = ls_cols(name)?;
    let state_idx = cols
        .iter()
        .position(|c| matches!(c.as_str(), "running" | "idle" | "failed"))?;
    cols.get(state_idx + 1)?.parse::<usize>().ok()
}

/// Poll `cond` every 250ms until it returns true or `secs` elapses; returns whether it became
/// true. Cold-boot latency varies (and rises when other suite VMs are live), so a fixed sleep
/// before reading `ls` is flaky — poll for the state we expect instead.
fn wait_until(secs: u64, cond: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    cond()
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

/// Run `dome sandbox shell <name>` over a REAL pseudo-terminal so the CLI sees an interactive
/// tty (`stdin().is_terminal()` → `tty=true`), exercising the guest's TTY/PTY exec path — the
/// one the non-TTY `run` tests never touch. Sends `exit\n`, then drains the pty until the CLI
/// exits (the slave fds close → master read returns 0/EIO), with a timeout backstop so a stuck
/// session fails the test rather than hanging. `secs` must be generous enough for a cold boot.
fn shell_exit_over_pty(name: &str, secs: u64) {
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty failed");

    let mut cmd = Command::new(dome_bin());
    cmd.args(["sandbox", "shell", name]);
    let slave_fd = slave;
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(libc::dup(slave_fd)));
        cmd.stdout(Stdio::from_raw_fd(libc::dup(slave_fd)));
        cmd.stderr(Stdio::from_raw_fd(libc::dup(slave_fd)));
        cmd.pre_exec(move || {
            libc::setsid();
            let _ = libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .expect("failed to spawn `dome sandbox shell` under a pty");
    unsafe {
        libc::close(slave);
    }

    // Ask the interactive shell to exit. The line waits in the tty buffer until the guest's
    // login shell reads it, so writing immediately (before the prompt) is fine.
    let mut writer = unsafe { std::fs::File::from_raw_fd(libc::dup(master)) };
    let _ = writer.write_all(b"exit\n");
    let _ = writer.flush();

    let mut reader = unsafe { std::fs::File::from_raw_fd(master) };
    let start = std::time::Instant::now();
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if start.elapsed() > Duration::from_secs(secs) {
            break;
        }
    }
    let _ = child.wait();
}

/// Issue #82: after an INTERACTIVE (tty) shell exits, the worker must drain the attached count
/// back to 0. The guest's TTY exec path leaked a duplicate vsock fd, so the connection stayed
/// half-open, the host relay never tore down, and the count stuck above 0 — which forced a
/// non-forced stop to be refused. The non-TTY `run` path above never caught this; this drives a
/// real pty so `tty=true`.
#[test]
#[ignore]
fn an_interactive_shell_drains_the_attached_count_after_it_exits() {
    let name = sandbox_name("tty");
    cleanup(&name);

    // First interactive shell cold-boots the VM (generous timeout), then exits.
    shell_exit_over_pty(&name, 90);
    assert!(
        wait_until(20, || ls_attached(&name) == Some(0)),
        "ls should report 0 attached after the interactive shell exits; got {:?}, state {:?}",
        ls_attached(&name),
        ls_state(&name)
    );
    assert_eq!(
        ls_state(&name).as_deref(),
        Some("running"),
        "the VM must stay running after the interactive shell exits"
    );

    // A second interactive shell against the now-warm VM must also drain to 0 — proving the
    // count balances per session, not just once.
    shell_exit_over_pty(&name, 30);
    assert!(
        wait_until(20, || ls_attached(&name) == Some(0)),
        "a second interactive shell must also drain the count to 0; got {:?}",
        ls_attached(&name)
    );

    cleanup(&name);
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
            // The sleep keeps A attached through B's checks. It runs in GUEST time *after* boot,
            // so it gives a fixed attached window regardless of how long the cold boot took. A
            // generous window keeps the poll below non-flaky even under full-suite load. A ends
            // when this command exits — we wait it out below, which is what drains the attached
            // count back to 0 (killing the host client would leave the guest sleep running, so
            // the session — and the count — would linger).
            "echo from-terminal-a > /root/shared.txt; sleep 20",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn terminal A");

    // Wait (polling) for A to cold-boot and attach: `ls` must show the sandbox running with at
    // least one attached terminal. Cold boots aren't instant — especially with other suite VMs
    // live — so poll rather than sleep a fixed interval.
    assert!(
        wait_until(60, || ls_attached(&name).unwrap_or(0) >= 1),
        "ls should report at least one attached terminal while A is live; got {:?}, state {:?}",
        ls_attached(&name),
        ls_state(&name)
    );
    assert_eq!(
        ls_state(&name).as_deref(),
        Some("running"),
        "the sandbox should be running while terminal A is attached"
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

    // Let terminal A's command finish so its session ends cleanly; both terminals are now
    // closed. (B already detached when its `cat` returned.)
    let _ = a.wait();

    // The VM stays up after every terminal closes: still running, and the attached count
    // drains back to 0 (poll — the worker observes the session end asynchronously).
    assert!(
        wait_until(15, || ls_attached(&name) == Some(0)),
        "ls should report 0 attached once every terminal has detached; got {:?}",
        ls_attached(&name)
    );
    assert_eq!(
        ls_state(&name).as_deref(),
        Some("running"),
        "closing all terminals must leave the VM running"
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
