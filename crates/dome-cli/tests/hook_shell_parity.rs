//! Shell-level tests for the bash and fish auto-activation hooks (issue #64), bringing them to
//! parity with the zsh hook covered by `hook_shell.rs`.
//!
//! Like the zsh tests, these drive a REAL shell under a pseudo-terminal (so the hook's
//! interactive / tty guards pass) with a FAKE `dome` shim wired in via `DOME_HOOK_CMD`. The shim
//! records every `__hook-activate` call and returns a controllable exit code, so we can observe
//! exactly when the hook does and does not invoke the binary — proving the guards, the
//! per-terminal-session suppression, and `DOME_NO_AUTO` behave as specified. No VM is involved,
//! so these run under `just test` (no `#[ignore]`).
//!
//! Two harnesses are needed because the shells differ in how their directory hook fires:
//!   * bash wires the hook into `PROMPT_COMMAND`, which only runs in the interactive prompt loop
//!     (NOT under `bash -c`), so the script is fed to an interactive bash over the pty's stdin.
//!   * fish fires `--on-variable PWD` synchronously on `cd`, so `fish -i -c '<script>'` works
//!     (like zsh's `-ifc`) — but fish needs a non-zero terminal window size or it bails early,
//!     so the pty is sized before exec.
//!
//! Unix-only: they use `openpty`. On a host without the shell the matching test self-skips.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The dome binary built for this crate's tests (cargo sets this env var).
fn dome_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dome")
}

fn have(shell: &str) -> bool {
    Command::new(shell)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Write the fake `dome` shim to `shim_path`: it appends each invocation's arguments to
/// `log_path` and exits with `$DOME_FAKE_RC` (default 0), letting a test simulate trusted (0),
/// untrusted (10), or skip (11) outcomes. Returns the `export DOME_HOOK_CMD=...` line that wires
/// the emitted hook to it.
fn write_shim(shim_path: &Path, log_path: &Path) -> String {
    let shim = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit ${{DOME_FAKE_RC:-0}}\n",
        log_path.display()
    );
    std::fs::write(shim_path, shim).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(shim_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(shim_path, perms).unwrap();
    format!("export DOME_HOOK_CMD='{}'", shim_path.display())
}

/// Emit the hook for `shell` from the real `dome` binary and write it to a file the test scripts
/// `source`. Returns the path to the hook file.
fn emit_hook_file(shell: &str, dir: &Path) -> std::path::PathBuf {
    let out = Command::new(dome_bin())
        .args(["hook", shell])
        .output()
        .unwrap_or_else(|_| panic!("dome hook {shell}"));
    assert!(out.status.success(), "dome hook {shell} failed");
    let path = dir.join(format!("hook.{shell}"));
    std::fs::write(&path, out.stdout).unwrap();
    path
}

/// Set the pty slave to a sane size (fish bails on a zero window size) and make it the
/// controlling terminal, run inside the child via `pre_exec`.
fn setup_pty_child(slave_fd: libc::c_int) {
    unsafe {
        libc::setsid();
        let ws = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(slave_fd, libc::TIOCSWINSZ as _, &ws);
        let _ = libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
    }
}

/// Drain the pty master until the child closes the slave (read returns 0/EIO), with a timeout
/// backstop so a stuck shell fails the test rather than hanging forever.
fn drain(master_fd: libc::c_int) -> String {
    let mut reader = unsafe { std::fs::File::from_raw_fd(master_fd) };
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
    out
}

/// Apply each test's explicit env on top of a scrubbed base. The hook's own guards key off
/// `$CI`/`$DOME_SANDBOX`/`$DOME_NO_AUTO`; a runner that sets `$CI` (every GitHub Actions job
/// does) would trip the guard and suppress every drop-in, so scrub them first and let each test
/// re-add exactly what it wants. `$DOME_HOOK_INSTALLED` is scrubbed too so the shell starts as
/// if the hook were not yet installed.
fn apply_envs(cmd: &mut Command, envs: &[(&str, &str)]) {
    for guard in ["CI", "DOME_SANDBOX", "DOME_NO_AUTO", "DOME_HOOK_INSTALLED"] {
        cmd.env_remove(guard);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
}

/// Open a pty and return `(master_fd, slave_fd)`.
fn openpty() -> (libc::c_int, libc::c_int) {
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
    (master, slave)
}

/// Run an interactive bash over a pty, feeding `script` to its stdin. Bash's `PROMPT_COMMAND`
/// hook only fires in the interactive prompt loop, so the script is typed in (terminated with
/// `exit`) rather than passed via `-c`. Returns everything bash wrote to the terminal.
fn run_bash(script: &str, envs: &[(&str, &str)]) -> String {
    let (master, slave) = openpty();
    let mut cmd = Command::new("bash");
    // --norc/--noprofile: a clean shell with no user rc; -i: interactive (drives PROMPT_COMMAND).
    cmd.args(["--norc", "--noprofile", "-i"]);
    apply_envs(&mut cmd, envs);
    let slave_fd = slave;
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(libc::dup(slave_fd)));
        cmd.stdout(Stdio::from_raw_fd(libc::dup(slave_fd)));
        cmd.stderr(Stdio::from_raw_fd(libc::dup(slave_fd)));
        cmd.pre_exec(move || {
            setup_pty_child(slave_fd);
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn bash");
    unsafe {
        libc::close(slave);
    }
    // Type the script into the interactive shell, then `exit` to end the session.
    {
        let mut writer = unsafe { std::fs::File::from_raw_fd(libc::dup(master)) };
        write!(writer, "{script}\nexit\n").expect("write to pty");
        writer.flush().ok();
    }
    let out = drain(master);
    let _ = child.wait();
    out
}

/// Run `fish -i -c '<script>'` over a pty. Fish fires `--on-variable PWD` synchronously on `cd`,
/// so a single `-c` script suffices (no interactive-stdin typing needed); the pty just needs a
/// non-zero window size, set in `pre_exec`. Returns everything fish wrote to the terminal.
fn run_fish(script: &str, envs: &[(&str, &str)]) -> String {
    let (master, slave) = openpty();
    let mut cmd = Command::new("fish");
    // --no-config: skip user config; -i: interactive (so `status is-interactive` is true).
    cmd.args(["--no-config", "-i", "-c", script]);
    cmd.env("TERM", "xterm");
    apply_envs(&mut cmd, envs);
    let slave_fd = slave;
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(libc::dup(slave_fd)));
        cmd.stdout(Stdio::from_raw_fd(libc::dup(slave_fd)));
        cmd.stderr(Stdio::from_raw_fd(libc::dup(slave_fd)));
        cmd.pre_exec(move || {
            setup_pty_child(slave_fd);
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn fish");
    unsafe {
        libc::close(slave);
    }
    let out = drain(master);
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

// ---------------------------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------------------------

#[test]
fn bash_cd_into_a_project_invokes_the_drop_in() {
    if !have("bash") {
        eprintln!("skipping: bash not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("calls.log");
    let shim_env = write_shim(&tmp.path().join("dome-shim"), &log);
    let hook = emit_hook_file("bash", tmp.path());
    let project = make_project(&tmp.path().join("proj"));

    let script = format!(
        "cd {tmp}\n{shim_env}\nsource {hook}\ncd {proj}",
        tmp = tmp.path().display(),
        hook = hook.display(),
        proj = project.display(),
    );
    run_bash(&script, &[]);

    let calls = log_lines(&log);
    assert_eq!(calls.len(), 1, "expected one drop-in call, got: {calls:?}");
    assert!(
        calls[0].contains("__hook-activate") && calls[0].contains(project.to_str().unwrap()),
        "drop-in must target the project dir; got {calls:?}"
    );
}

#[test]
fn bash_dome_no_auto_suppresses_activation() {
    if !have("bash") {
        eprintln!("skipping: bash not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("calls.log");
    let shim_env = write_shim(&tmp.path().join("dome-shim"), &log);
    let hook = emit_hook_file("bash", tmp.path());
    let project = make_project(&tmp.path().join("proj"));

    let script = format!(
        "cd {tmp}\n{shim_env}\nsource {hook}\ncd {proj}",
        tmp = tmp.path().display(),
        hook = hook.display(),
        proj = project.display(),
    );
    run_bash(&script, &[("DOME_NO_AUTO", "1")]);

    assert!(
        log_lines(&log).is_empty(),
        "DOME_NO_AUTO=1 must hard-skip auto-activation"
    );
}

#[test]
fn bash_inside_a_guest_does_not_re_activate() {
    if !have("bash") {
        eprintln!("skipping: bash not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("calls.log");
    let shim_env = write_shim(&tmp.path().join("dome-shim"), &log);
    let hook = emit_hook_file("bash", tmp.path());
    let project = make_project(&tmp.path().join("proj"));

    let script = format!(
        "cd {tmp}\n{shim_env}\nsource {hook}\ncd {proj}",
        tmp = tmp.path().display(),
        hook = hook.display(),
        proj = project.display(),
    );
    run_bash(&script, &[("DOME_SANDBOX", "web")]);

    assert!(
        log_lines(&log).is_empty(),
        "being inside a guest (DOME_SANDBOX set) must prevent nested activation"
    );
}

#[test]
fn bash_suppression_prevents_re_drop_until_you_leave_and_return() {
    if !have("bash") {
        eprintln!("skipping: bash not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("calls.log");
    let shim_env = write_shim(&tmp.path().join("dome-shim"), &log);
    let hook = emit_hook_file("bash", tmp.path());
    let project = make_project(&tmp.path().join("proj"));
    let sub = project.join("src");
    std::fs::create_dir_all(&sub).unwrap();

    // Enter (1 call), cd into a subdir of the SAME project (no re-drop: still suppressed), leave
    // to tmp (clears suppression), then re-enter (a 2nd call).
    let script = format!(
        "cd {tmp}\n{shim_env}\nsource {hook}\ncd {proj}\ncd {sub}\ncd {tmp}\ncd {proj}",
        tmp = tmp.path().display(),
        hook = hook.display(),
        proj = project.display(),
        sub = sub.display(),
    );
    run_bash(&script, &[]);

    let calls = log_lines(&log);
    assert_eq!(
        calls.len(),
        2,
        "expected one drop-in on first entry and one after leaving+re-entering; got {calls:?}"
    );
}

#[test]
fn bash_untrusted_hint_prints_at_most_once_per_session() {
    if !have("bash") {
        eprintln!("skipping: bash not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("calls.log");
    let shim_env = write_shim(&tmp.path().join("dome-shim"), &log);
    let hook = emit_hook_file("bash", tmp.path());
    let project = make_project(&tmp.path().join("proj"));

    // The shim simulates an untrusted verdict (exit 10) on every call. Enter, leave, re-enter:
    // the binary is consulted each entry, but the `dome allow` hint must appear only ONCE.
    let script = format!(
        "cd {tmp}\n{shim_env}\nsource {hook}\ncd {proj}\ncd {tmp}\ncd {proj}",
        tmp = tmp.path().display(),
        hook = hook.display(),
        proj = project.display(),
    );
    let out = run_bash(&script, &[("DOME_FAKE_RC", "10")]);

    assert_eq!(
        out.matches("dome allow").count(),
        1,
        "the untrusted hint must print exactly once per session; full output:\n{out}"
    );
}

// ---------------------------------------------------------------------------------------------
// fish
// ---------------------------------------------------------------------------------------------

#[test]
fn fish_cd_into_a_project_invokes_the_drop_in() {
    if !have("fish") {
        eprintln!("skipping: fish not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("calls.log");
    let shim_env = write_shim(&tmp.path().join("dome-shim"), &log);
    let hook = emit_hook_file("fish", tmp.path());
    let project = make_project(&tmp.path().join("proj"));

    // fish wires DOME_HOOK_CMD with `set -gx` (the shim_env `export` line is sh syntax).
    let script = format!(
        "cd {tmp}\nset -gx DOME_HOOK_CMD '{shim}'\nsource {hook}\ncd {proj}",
        tmp = tmp.path().display(),
        shim = tmp.path().join("dome-shim").display(),
        hook = hook.display(),
        proj = project.display(),
    );
    let _ = shim_env; // the env var is set in-script for fish
    run_fish(&script, &[]);

    let calls = log_lines(&log);
    assert_eq!(calls.len(), 1, "expected one drop-in call, got: {calls:?}");
    assert!(
        calls[0].contains("__hook-activate") && calls[0].contains(project.to_str().unwrap()),
        "drop-in must target the project dir; got {calls:?}"
    );
}

#[test]
fn fish_dome_no_auto_suppresses_activation() {
    if !have("fish") {
        eprintln!("skipping: fish not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("calls.log");
    write_shim(&tmp.path().join("dome-shim"), &log);
    let hook = emit_hook_file("fish", tmp.path());
    let project = make_project(&tmp.path().join("proj"));

    let script = format!(
        "cd {tmp}\nset -gx DOME_HOOK_CMD '{shim}'\nsource {hook}\ncd {proj}",
        tmp = tmp.path().display(),
        shim = tmp.path().join("dome-shim").display(),
        hook = hook.display(),
        proj = project.display(),
    );
    run_fish(&script, &[("DOME_NO_AUTO", "1")]);

    assert!(
        log_lines(&log).is_empty(),
        "DOME_NO_AUTO=1 must hard-skip auto-activation"
    );
}

#[test]
fn fish_inside_a_guest_does_not_re_activate() {
    if !have("fish") {
        eprintln!("skipping: fish not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("calls.log");
    write_shim(&tmp.path().join("dome-shim"), &log);
    let hook = emit_hook_file("fish", tmp.path());
    let project = make_project(&tmp.path().join("proj"));

    let script = format!(
        "cd {tmp}\nset -gx DOME_HOOK_CMD '{shim}'\nsource {hook}\ncd {proj}",
        tmp = tmp.path().display(),
        shim = tmp.path().join("dome-shim").display(),
        hook = hook.display(),
        proj = project.display(),
    );
    run_fish(&script, &[("DOME_SANDBOX", "web")]);

    assert!(
        log_lines(&log).is_empty(),
        "being inside a guest (DOME_SANDBOX set) must prevent nested activation"
    );
}

#[test]
fn fish_suppression_prevents_re_drop_until_you_leave_and_return() {
    if !have("fish") {
        eprintln!("skipping: fish not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("calls.log");
    write_shim(&tmp.path().join("dome-shim"), &log);
    let hook = emit_hook_file("fish", tmp.path());
    let project = make_project(&tmp.path().join("proj"));
    let sub = project.join("src");
    std::fs::create_dir_all(&sub).unwrap();

    let script = format!(
        "cd {tmp}\nset -gx DOME_HOOK_CMD '{shim}'\nsource {hook}\ncd {proj}\ncd {sub}\ncd {tmp}\ncd {proj}",
        tmp = tmp.path().display(),
        shim = tmp.path().join("dome-shim").display(),
        hook = hook.display(),
        proj = project.display(),
        sub = sub.display(),
    );
    run_fish(&script, &[]);

    let calls = log_lines(&log);
    assert_eq!(
        calls.len(),
        2,
        "expected one drop-in on first entry and one after leaving+re-entering; got {calls:?}"
    );
}

#[test]
fn fish_untrusted_hint_prints_at_most_once_per_session() {
    if !have("fish") {
        eprintln!("skipping: fish not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("calls.log");
    write_shim(&tmp.path().join("dome-shim"), &log);
    let hook = emit_hook_file("fish", tmp.path());
    let project = make_project(&tmp.path().join("proj"));

    let script = format!(
        "cd {tmp}\nset -gx DOME_HOOK_CMD '{shim}'\nsource {hook}\ncd {proj}\ncd {tmp}\ncd {proj}",
        tmp = tmp.path().display(),
        shim = tmp.path().join("dome-shim").display(),
        hook = hook.display(),
        proj = project.display(),
    );
    let out = run_fish(&script, &[("DOME_FAKE_RC", "10")]);

    assert_eq!(
        out.matches("dome allow").count(),
        1,
        "the untrusted hint must print exactly once per session; full output:\n{out}"
    );
}
