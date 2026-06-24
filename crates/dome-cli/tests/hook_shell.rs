//! Shell-level tests for the zsh auto-activation hook (issue #60).
//!
//! These drive a REAL zsh under a pseudo-terminal (so the hook's `[[ -o interactive ]]` and
//! `[[ -t 0 && -t 1 ]]` guards pass) with a FAKE `dome` shim wired in via `DOME_HOOK_CMD`. The
//! shim records every `__hook-activate` call and returns a controllable exit code, so we can
//! observe exactly when the hook does and does not invoke the binary — proving the guards, the
//! per-terminal-session suppression, and `DOME_NO_AUTO` behave as specified. No VM is involved,
//! so these run under `just test` (no `#[ignore]`).
//!
//! Unix-only: they use `openpty`. On a host without `zsh` the test self-skips.

#![cfg(unix)]

use std::io::Read;
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The dome binary built for this crate's tests (cargo sets this env var).
fn dome_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dome")
}

fn have_zsh() -> bool {
    Command::new("zsh")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The emitted hook script with `DOME_HOOK_CMD` pointed at a fake shim. The shim appends each
/// invocation's arguments to `log_path` and exits with the code in `$DOME_FAKE_RC` (default 0),
/// letting a test simulate trusted (0), untrusted (10), or skip (11) outcomes.
fn hook_with_shim(shim_path: &Path, log_path: &Path) -> (String, String) {
    let shim = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit ${{DOME_FAKE_RC:-0}}\n",
        log_path.display()
    );
    std::fs::write(shim_path, shim).unwrap();
    let mut perms = std::fs::metadata(shim_path).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(shim_path, perms).unwrap();

    let out = Command::new(dome_bin())
        .args(["hook", "zsh"])
        .output()
        .expect("dome hook zsh");
    let hook = String::from_utf8_lossy(&out.stdout).to_string();
    (
        hook,
        format!("export DOME_HOOK_CMD='{}'\n", shim_path.display()),
    )
}

/// Run a zsh script under a pty (so the hook's `-o interactive` and `-t 0/-t 1` guards pass)
/// with the given extra environment. Uses `zsh -ic '<script>'`: `-i` makes the shell
/// interactive, the pty makes the tty checks true, and `-c` runs the script and exits without
/// engaging the line editor (which would block waiting for keystrokes). The hook's `chpwd`
/// runs on every `cd`, so a script that sources the hook then `cd`s drives activation. The
/// assertions read the shim log afterwards.
fn run_in_pty(script: &str, envs: &[(&str, &str)]) -> String {
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

    // `-f` skips user rc files for a clean shell; `-i` forces interactive; `-c` runs the
    // script string and exits.
    let mut cmd = Command::new("zsh");
    cmd.args(["-ifc", script]);
    // Scrub the hook's environment guards from whatever the host process inherited so the spawned
    // shell starts from a known-clean state. Without this, a runner that sets `$CI` (every GitHub
    // Actions job does) would trip the hook's `[[ -n "$CI" ]]` guard and silently suppress every
    // drop-in — making the positive-activation tests pass locally but fail in CI. Each test then
    // re-adds exactly the guard it wants to exercise via `envs`, applied on top below.
    for guard in ["CI", "DOME_SANDBOX", "DOME_NO_AUTO"] {
        cmd.env_remove(guard);
    }
    for (k, v) in envs {
        cmd.env(k, v);
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
    let mut child = cmd.spawn().expect("spawn zsh");
    unsafe {
        libc::close(slave);
    }

    let mut reader = unsafe { std::fs::File::from_raw_fd(master) };
    // Drain the pty until the child exits (read returns 0/EIO once the slave fds close), with
    // a timeout backstop so a stuck shell fails the test rather than hanging forever.
    let start = Instant::now();
    let mut buf = [0u8; 4096];
    let mut out = String::new();
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
        }
        if start.elapsed() > Duration::from_secs(20) {
            break;
        }
    }
    let _ = child.wait();
    out
}

/// Read the shim log lines (each line is one `__hook-activate` invocation's args), or empty.
fn log_lines(log_path: &Path) -> Vec<String> {
    std::fs::read_to_string(log_path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Make a project dir containing a dome.json and return its canonical path.
fn make_project(dir: &Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("dome.json"), "{}").unwrap();
    std::fs::canonicalize(dir).unwrap()
}

#[test]
fn cd_into_a_project_invokes_the_drop_in() {
    if !have_zsh() {
        eprintln!("skipping: zsh not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let shim = tmp.path().join("dome-shim");
    let log = tmp.path().join("calls.log");
    let (hook, shim_env) = hook_with_shim(&shim, &log);
    let project = make_project(&tmp.path().join("proj"));

    // Start outside the project, source the hook, then cd in. The hook should fire once.
    let script = format!(
        "cd {tmp}\n{hook}\ncd {proj}\n",
        tmp = tmp.path().display(),
        hook = hook,
        proj = project.display(),
    );
    run_in_pty(&format!("{shim_env}{script}"), &[]);

    let calls = log_lines(&log);
    assert_eq!(
        calls.len(),
        1,
        "expected exactly one drop-in call, got: {calls:?}"
    );
    assert!(
        calls[0].contains("__hook-activate") && calls[0].contains(project.to_str().unwrap()),
        "drop-in must target the project dir; got {calls:?}"
    );
}

#[test]
fn dome_no_auto_suppresses_activation() {
    if !have_zsh() {
        eprintln!("skipping: zsh not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let shim = tmp.path().join("dome-shim");
    let log = tmp.path().join("calls.log");
    let (hook, shim_env) = hook_with_shim(&shim, &log);
    let project = make_project(&tmp.path().join("proj"));

    let script = format!(
        "cd {tmp}\n{hook}\ncd {proj}\n",
        tmp = tmp.path().display(),
        hook = hook,
        proj = project.display(),
    );
    run_in_pty(&format!("{shim_env}{script}"), &[("DOME_NO_AUTO", "1")]);

    assert!(
        log_lines(&log).is_empty(),
        "DOME_NO_AUTO=1 must hard-skip auto-activation"
    );
}

#[test]
fn inside_a_guest_does_not_re_activate() {
    if !have_zsh() {
        eprintln!("skipping: zsh not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let shim = tmp.path().join("dome-shim");
    let log = tmp.path().join("calls.log");
    let (hook, shim_env) = hook_with_shim(&shim, &log);
    let project = make_project(&tmp.path().join("proj"));

    let script = format!(
        "cd {tmp}\n{hook}\ncd {proj}\n",
        tmp = tmp.path().display(),
        hook = hook,
        proj = project.display(),
    );
    // DOME_SANDBOX set means we are already inside a dome guest → never nest.
    run_in_pty(&format!("{shim_env}{script}"), &[("DOME_SANDBOX", "web")]);

    assert!(
        log_lines(&log).is_empty(),
        "being inside a guest (DOME_SANDBOX set) must prevent nested activation"
    );
}

#[test]
fn untrusted_hint_prints_at_most_once_per_session() {
    if !have_zsh() {
        eprintln!("skipping: zsh not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let shim = tmp.path().join("dome-shim");
    let log = tmp.path().join("calls.log");
    let (hook, shim_env) = hook_with_shim(&shim, &log);
    let project = make_project(&tmp.path().join("proj"));

    // The shim simulates an untrusted verdict (exit 10) on every call. Enter the project, leave,
    // and re-enter: the binary is consulted each entry, but the `dome allow` hint must appear
    // only ONCE across the whole session.
    let script = format!(
        "cd {tmp}\n{hook}\ncd {proj}\ncd {tmp}\ncd {proj}\n",
        tmp = tmp.path().display(),
        hook = hook,
        proj = project.display(),
    );
    let out = run_in_pty(&format!("{shim_env}{script}"), &[("DOME_FAKE_RC", "10")]);

    let hint_count = out.matches("dome allow").count();
    assert_eq!(
        hint_count, 1,
        "the untrusted hint must print exactly once per session; full output:\n{out}"
    );
}

#[test]
fn suppression_prevents_re_drop_until_you_leave_and_return() {
    if !have_zsh() {
        eprintln!("skipping: zsh not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let shim = tmp.path().join("dome-shim");
    let log = tmp.path().join("calls.log");
    let (hook, shim_env) = hook_with_shim(&shim, &log);
    let project = make_project(&tmp.path().join("proj"));
    let sub = project.join("src");
    std::fs::create_dir_all(&sub).unwrap();

    // Enter the project (1 call), cd into a subdir of the SAME project (no re-drop: still
    // suppressed), leave to the parent tmp (clears suppression), then re-enter (a 2nd call).
    let script = format!(
        "cd {tmp}\n{hook}\ncd {proj}\ncd {sub}\ncd {tmp}\ncd {proj}\n",
        tmp = tmp.path().display(),
        hook = hook,
        proj = project.display(),
        sub = sub.display(),
    );
    run_in_pty(&format!("{shim_env}{script}"), &[]);

    let calls = log_lines(&log);
    assert_eq!(
        calls.len(),
        2,
        "expected one drop-in on first entry and one after leaving+re-entering; got {calls:?}"
    );
}
