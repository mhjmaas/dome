//! Integration test for issue #59: guest shell upgrade — the interactive sandbox shell
//! is a real `bash` login shell with a sandbox-labeled prompt, not a bare dash `#`.
//!
//! Boots a REAL persistent VM through a worker, so it requires a codesigned binary AND a
//! rootfs that has `bash` + `/etc/profile.d/dome.sh` installed (rebuild with `just
//! build-image`). `#[ignore]`d by default; run with:
//!   just test-vm guest_shell
//!
//! The default interactive command (`dome sandbox shell` with no args) is `bash -l`. We
//! drive it with piped stdin (a non-TTY session runs that same default command), so the
//! login shell sources `/etc/profile.d/dome.sh` and we can observe `$BASH_VERSION`, the
//! sandbox-labeled `$PS1`, and `$HOME` in its output.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. `just build`)")
}

fn sandbox_name(suffix: &str) -> String {
    format!("itest-shell-{}-{}", std::process::id(), suffix)
}

fn dome(args: &[&str]) -> std::process::Output {
    Command::new(dome_bin())
        .args(args)
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

fn cleanup(name: &str) {
    let _ = dome(&["sandbox", "stop", "--force", name]);
    std::thread::sleep(Duration::from_secs(2));
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = format!("{}/.local/share/dome/sandboxes", home);
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    let _ = std::fs::remove_file(format!("{}/{}.lock", dir, name));
}

/// Run `dome sandbox shell <name>` with `script` piped to its stdin and return the
/// captured stdout. A piped (non-TTY) session runs the SAME default command an
/// interactive drop-in would (`bash -l`), so the login profile is sourced identically.
fn shell_with_stdin(name: &str, script: &str) -> String {
    let mut child = Command::new(dome_bin())
        .args(["sandbox", "shell", name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn `dome sandbox shell`");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(script.as_bytes())
        .expect("write script to shell stdin");
    let out = child.wait_with_output().expect("wait for shell");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Run an explicit command in the sandbox over piped (non-TTY) stdin and return captured
/// stdout. `sandbox run <cmd>` bypasses the default `bash -l`, so `/etc/profile.d/dome.sh`
/// is NOT sourced — this exercises the same bare `handle_piped_exec` path provision steps
/// use, where `HOME` can only come from the guest init environment.
fn run_with_stdin(name: &str, command: &[&str]) -> String {
    let mut child = Command::new(dome_bin())
        .args(["sandbox", "run", name])
        .args(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn `dome sandbox run <cmd>`");
    // Close stdin immediately (empty) so the session is detected as non-TTY.
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("wait for command");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Regression for issue #90: provision steps (and any non-login piped exec) must run with a
/// real `HOME`, not an empty one. A bare `sh -c` does not source `/etc/profile.d/dome.sh`,
/// so `$HOME` can only come from the guest init environment — which must default it to
/// `/root`. With `HOME` unset, `$HOME`-relative installers (e.g. bun) land under `/` and
/// fail silently at runtime.
#[test]
#[ignore]
fn piped_exec_runs_with_home_set() {
    let name = sandbox_name("home");
    cleanup(&name);

    let stdout = run_with_stdin(&name, &["sh", "-c", r#"printf 'HOME=%s\n' "$HOME""#]);

    assert!(
        stdout.lines().any(|l| l == "HOME=/root"),
        "non-login piped exec must see HOME=/root (issue #90); stdout:\n{stdout}"
    );

    cleanup(&name);
}

/// The default sandbox shell is a `bash` login shell whose prompt carries the running
/// sandbox's name, and `$HOME` is set — proving the bash + profile.d upgrade end to end.
#[test]
#[ignore]
fn default_shell_is_bash_with_sandbox_labeled_prompt() {
    let name = sandbox_name("bash");
    cleanup(&name);

    let script = "echo VER=$BASH_VERSION\necho PS1=\"$PS1\"\necho HOME=$HOME\nexit\n";
    let stdout = shell_with_stdin(&name, script);

    // `$BASH_VERSION` is non-empty only under bash — proves the launched shell is bash,
    // not the old bare dash `/bin/sh`.
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("VER=") && l.trim_start_matches("VER=").contains('.')),
        "default shell must be bash ($BASH_VERSION set); stdout:\n{stdout}"
    );

    // The prompt is sandbox-labeled and the label matches THIS sandbox's name.
    assert!(
        stdout.contains(&format!("PS1=[sandbox:{name}]")),
        "prompt must carry the running sandbox's label `[sandbox:{name}]`; stdout:\n{stdout}"
    );

    // A real HOME is set so `~` and login shells behave.
    assert!(
        stdout.lines().any(|l| l == "HOME=/root"),
        "HOME must be set in the guest shell; stdout:\n{stdout}"
    );

    cleanup(&name);
}
