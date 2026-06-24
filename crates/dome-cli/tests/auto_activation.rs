//! Real-VM integration test for issue #60: directory auto-activation end to end.
//!
//! Boots a REAL persistent VM through the worker, so it needs a codesigned binary AND a rootfs
//! with `bash` + `/etc/profile.d/dome.sh` (the profile now also honors `DOME_LAND_CWD` for
//! subdir landing — rebuild with `just build-image`). `#[ignore]`d by default; run with:
//!   just test-vm auto_activation
//!
//! Covers the headline tracer bullet: an untrusted project is NOT activated, `dome allow`
//! records trust, the hidden `dome __hook-activate` then drops into the guest landing at the
//! mapped subdirectory, and the drop-in returns exit code 0 (the signal the shell hook uses to
//! suppress the exit→re-drop loop).

use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. `just build`)")
}

fn sandbox_name(suffix: &str) -> String {
    format!("itest-auto-{}-{}", std::process::id(), suffix)
}

/// Run `dome` with the given args and cwd, returning the full output.
fn dome_in(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(dome_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

fn cleanup(name: &str) {
    let _ = Command::new(dome_bin())
        .args(["sandbox", "stop", "--force", name])
        .output();
    std::thread::sleep(Duration::from_secs(2));
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = format!("{}/.local/share/dome/sandboxes", home);
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    let _ = std::fs::remove_file(format!("{}/{}.lock", dir, name));
    let _ = std::fs::remove_file(format!("{}/{}.config.json", dir, name));
}

/// Drive `dome __hook-activate <project>` (the hook's drop-in) from `cwd`, with `script` piped
/// to the guest shell's stdin. Returns (stdout, exit_code). A piped session runs the same
/// `bash -l` an interactive drop-in would, so `/etc/profile.d/dome.sh` (and its `DOME_LAND_CWD`
/// cd) is sourced identically.
fn hook_activate(cwd: &Path, project: &Path, script: &str) -> (String, i32) {
    let mut child = Command::new(dome_bin())
        .args(["__hook-activate", project.to_str().unwrap()])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn `dome __hook-activate`");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait for drop-in");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// Run `dome` under a pseudo-terminal so `stdin().is_terminal()` is true (the inline trust
/// prompt only fires on an interactive TTY), feeding `input` to its stdin. Returns the combined
/// pty output. Used to exercise the inline `[y/N]` grant on a manual `dome sandbox run`.
fn dome_in_pty(cwd: &Path, args: &[&str], input: &str) -> String {
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
    cmd.args(args).current_dir(cwd);
    // Scrub guards the host may set (CI runners export $CI), so the inline-offer guards behave
    // as on a developer's machine rather than being suppressed by the test environment.
    for guard in ["CI", "DOME_SANDBOX", "DOME_NO_AUTO"] {
        cmd.env_remove(guard);
    }
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
    let mut child = cmd.spawn().expect("spawn dome under pty");
    unsafe {
        libc::close(slave);
    }

    let mut writer = unsafe { std::fs::File::from_raw_fd(libc::dup(master)) };
    writer.write_all(input.as_bytes()).ok();
    writer.flush().ok();

    let mut reader = unsafe { std::fs::File::from_raw_fd(master) };
    let start = Instant::now();
    let mut buf = [0u8; 4096];
    let mut out = String::new();
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
        }
        if start.elapsed() > Duration::from_secs(60) {
            break;
        }
    }
    let _ = child.wait();
    out
}

/// Inline trust UX (#61): answering `y` to the prompt on a first manual `dome sandbox run` in an
/// untrusted project records trust, so a later auto-activation drop-in fires (exit 0) without any
/// further `dome allow`. Answering `n` records nothing, so the project stays untrusted (exit 10).
#[test]
#[ignore]
fn inline_grant_on_manual_run_enables_auto_activation() {
    let name = sandbox_name("inline");
    cleanup(&name);

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    std::fs::write(
        project.join("dome.json"),
        format!("{{\"sandbox\":\"{name}\"}}"),
    )
    .unwrap();

    // Untrusted up front: the auto-activation drop-in refuses (exit 10).
    let (_out, code) = hook_activate(project, project, "exit\n");
    assert_eq!(code, 10, "project must start untrusted");

    // Answer `n`: the session runs once but trust is NOT recorded, so it stays untrusted.
    // The name is passed explicitly (it is the first positional, so `run -- true` would make
    // `true` the sandbox name) — `true` is the one-shot command run inside the sandbox.
    let out = dome_in_pty(project, &["sandbox", "run", &name, "--", "true"], "n\n");
    assert!(
        out.contains("auto-activate on entry?"),
        "the inline [y/N] prompt must appear in an untrusted dir; output:\n{out}"
    );
    let (_out, code) = hook_activate(project, project, "exit\n");
    assert_eq!(code, 10, "answering n must record no trust; output:\n{out}");

    // Answer `y`: trust is recorded keyed to dir + dome.json hash.
    let out = dome_in_pty(project, &["sandbox", "run", &name, "--", "true"], "y\n");
    assert!(
        out.contains("auto-activate on entry?"),
        "the prompt must still appear (still untrusted); output:\n{out}"
    );

    // A later auto-activation drop-in now fires (exit 0) with no `dome allow` in between.
    let script = "echo SANDBOX=$DOME_SANDBOX\nexit\n";
    let (stdout, code) = hook_activate(project, project, script);
    assert_eq!(
        code, 0,
        "after answering y, auto-activation must drop in (exit 0); stdout:\n{stdout}"
    );
    assert!(
        stdout.lines().any(|l| l == format!("SANDBOX={name}")),
        "the drop-in must run inside the named guest; stdout:\n{stdout}"
    );

    cleanup(&name);
}

/// The full tracer bullet: untrusted → no drop-in; `dome allow` → trusted; drop-in lands in the
/// guest at the mapped SUBDIRECTORY and returns the "dropped-in" exit code.
#[test]
#[ignore]
fn allow_then_auto_activate_drops_into_guest_at_subdir() {
    let name = sandbox_name("subdir");
    cleanup(&name);

    // A project with a pinned sandbox name and a subdirectory to land in.
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    std::fs::write(
        project.join("dome.json"),
        format!("{{\"sandbox\":\"{name}\"}}"),
    )
    .unwrap();
    let sub = project.join("src");
    std::fs::create_dir_all(&sub).unwrap();

    // 1. Untrusted: the drop-in must refuse (exit code 10) and NOT boot a VM.
    let (_out, code) = hook_activate(project, project, "echo SHOULD_NOT_RUN\nexit\n");
    assert_eq!(
        code, 10,
        "an untrusted project must not drop in (expected ACTIVATE_UNTRUSTED=10)"
    );

    // 2. Grant trust.
    let allow = dome_in(project, &["allow"]);
    assert!(
        allow.status.success(),
        "dome allow failed: {}",
        String::from_utf8_lossy(&allow.stderr)
    );

    // 3. From the subdirectory, the drop-in boots the guest, lands at /workspace/src, and
    //    returns exit code 0 (the hook's "dropped in" signal).
    let script = "echo SANDBOX=$DOME_SANDBOX\necho LANDED=$(pwd)\nexit\n";
    let (stdout, code) = hook_activate(&sub, project, script);

    assert_eq!(
        code, 0,
        "a trusted drop-in must return ACTIVATE_DROPPED_IN=0 after the guest exits; stdout:\n{stdout}"
    );
    assert!(
        stdout.lines().any(|l| l == format!("SANDBOX={name}")),
        "the drop-in must run inside the named guest; stdout:\n{stdout}"
    );
    assert!(
        stdout.lines().any(|l| l == "LANDED=/workspace/src"),
        "the drop-in must land at the mapped subdirectory /workspace/src; stdout:\n{stdout}"
    );

    cleanup(&name);
}
