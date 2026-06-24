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
    // DOME_HOOK_INSTALLED is scrubbed too so the one-time install tip behaves as on a fresh
    // (un-hooked) machine regardless of the host shell.
    for guard in ["CI", "DOME_SANDBOX", "DOME_NO_AUTO", "DOME_HOOK_INSTALLED"] {
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

/// Collision-proof naming (#62): a project with NO `sandbox` field auto-activates into a
/// `<slug>-<pathhash>` sandbox (not the bare cwd-slug), so two same-basename directories never
/// silently share one VM. Drives the real drop-in and reads `$DOME_SANDBOX` from inside the guest.
#[test]
#[ignore]
fn auto_activation_derives_a_pathhash_sandbox_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Basename "api" → the slug is a known prefix; the hash suffix is derived from the abs path.
    let project = tmp.path().join("api");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("dome.json"), "{}").unwrap();

    // Trust it via a piped (non-interactive) `dome allow`: no pin prompt fires, and trust is
    // recorded against the bare `{}` so auto-activation derives the path-hashed name.
    let allow = dome_in(&project, &["allow"]);
    assert!(
        allow.status.success(),
        "dome allow failed: {}",
        String::from_utf8_lossy(&allow.stderr)
    );

    // Auto-activate: the drop-in boots the guest; the guest reports the resolved DOME_SANDBOX.
    let (stdout, code) = hook_activate(&project, &project, "echo SANDBOX=$DOME_SANDBOX\nexit\n");
    let name = stdout
        .lines()
        .find_map(|l| l.strip_prefix("SANDBOX="))
        .map(str::to_string);
    // Always reclaim whatever VM we booted, even if an assertion below fails.
    if let Some(ref n) = name {
        cleanup(n);
    }

    assert_eq!(
        code, 0,
        "a trusted drop-in must drop in (exit 0); stdout:\n{stdout}"
    );
    let name = name.expect("the guest must print its DOME_SANDBOX");
    let (slug, hash) = name.split_once('-').expect("the name is <slug>-<hash>");
    assert_eq!(
        slug, "api",
        "the slug prefix is the cwd basename; got {name}"
    );
    assert_eq!(hash.len(), 8, "8 hex chars of path hash; got {name}");
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "the suffix is a hex path hash; got {name}"
    );
}

/// Offer-to-pin (#62): `dome allow` under a TTY on a project with no `sandbox` field offers to
/// pin a stable name. Accepting writes `sandbox: "<slug>"` into the dome.json, and a later
/// auto-activation then uses that plain pinned name (no path-hash) — manual and auto converge.
#[test]
#[ignore]
fn pin_offer_writes_a_stable_name_and_converges_auto_activation() {
    // A unique basename per test process: the pinned name equals the slug, so this keeps reruns
    // from colliding on one sandbox.
    let base = format!("pin{}", std::process::id());
    cleanup(&base);

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join(&base);
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("dome.json"), "{}\n").unwrap();

    // `dome allow` under a pty, answering `y` to the pin offer.
    let out = dome_in_pty(&project, &["allow"], "y\n");
    assert!(
        out.contains("pin a stable sandbox name"),
        "the pin offer must appear on a TTY for an unpinned project; output:\n{out}"
    );

    // The dome.json now carries the pinned `sandbox` field.
    let written = std::fs::read_to_string(project.join("dome.json")).unwrap();
    assert!(
        written.contains(&format!("\"sandbox\": \"{base}\"")),
        "dome.json must be pinned with the stable name; got:\n{written}"
    );

    // Auto-activation now resolves to the plain pinned name (no hash suffix).
    let (stdout, code) = hook_activate(&project, &project, "echo SANDBOX=$DOME_SANDBOX\nexit\n");
    cleanup(&base);
    assert_eq!(
        code, 0,
        "the pinned, trusted project must drop in; stdout:\n{stdout}"
    );
    assert!(
        stdout.lines().any(|l| l == format!("SANDBOX={base}")),
        "auto-activation must use the pinned name (no path-hash); stdout:\n{stdout}"
    );
}

/// Discoverability (#63): a manual `dome sandbox run` with the shell hook NOT installed prints
/// the one-time install tip (showing the exact `eval "$(dome hook zsh)"` rc line), then drops a
/// marker so the tip never shows again on subsequent manual runs. Driven under a pty so the tip's
/// interactive-TTY guard is satisfied, in a project with no dome.json so the inline trust prompt
/// stays silent and the tip is the only nudge.
#[test]
#[ignore]
fn install_tip_shows_once_then_the_marker_suppresses_it() {
    let name = sandbox_name("tip");
    cleanup(&name);

    // The tip's "shown once" marker lives in the global dome data dir; clear it so this run
    // starts as on a fresh machine, and reclaim it afterward so it can't leak into other tests.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let marker = format!("{home}/.local/share/dome/hook-tip-shown");
    let _ = std::fs::remove_file(&marker);

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path(); // no dome.json → the inline trust prompt stays silent

    // First manual run: the tip appears with the exact rc line and the convenience installer.
    let first = dome_in_pty(project, &["sandbox", "run", &name, "--", "true"], "");
    assert!(
        first.contains("eval \"$(dome hook zsh)\""),
        "the install tip must show the exact rc line on a first un-hooked run; output:\n{first}"
    );
    assert!(
        first.contains("dome hook --install"),
        "the tip must mention the convenience installer; output:\n{first}"
    );
    assert!(
        std::path::Path::new(&marker).exists(),
        "showing the tip must drop the marker so it never nags again"
    );

    // Second manual run: the marker now suppresses the tip entirely.
    let second = dome_in_pty(project, &["sandbox", "run", &name, "--", "true"], "");
    assert!(
        !second.contains("eval \"$(dome hook zsh)\""),
        "the tip must not reappear once the marker exists; output:\n{second}"
    );

    let _ = std::fs::remove_file(&marker);
    cleanup(&name);
}

/// Is `shell` runnable on this host? Real-shell parity tests self-skip when it is not.
fn have(shell: &str) -> bool {
    Command::new(shell)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Drive a REAL `shell` (argv) under a pty so it sources the emitted hook and, on `cd`, boots a
/// real guest — then return the pty output as soon as `marker` appears in it (or a VM-boot-sized
/// timeout elapses). The marker is the guest's own sandbox-labeled prompt: seeing it proves the
/// real shell hook reached the real guest. We deliberately do NOT feed the guest any stdin and
/// stop reading the moment the marker lands — driving a nested interactive guest shell over the
/// same pty (typing `exit` to unwind it) deadlocks, because the host shell's line reader buffers
/// the post-`cd` input before the guest can read it. The child (and its session) is killed after,
/// and the caller force-stops the sandbox.
///
/// The pty gets a non-zero window size (fish bails otherwise) and the hook's guard env vars are
/// scrubbed so it behaves as on a developer's machine.
fn boot_guest_via_shell(argv: &[&str], input: &str, marker: &str) -> String {
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

    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..]);
    cmd.env("TERM", "xterm");
    for guard in ["CI", "DOME_SANDBOX", "DOME_NO_AUTO", "DOME_HOOK_INSTALLED"] {
        cmd.env_remove(guard);
    }
    let slave_fd = slave;
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(libc::dup(slave_fd)));
        cmd.stdout(Stdio::from_raw_fd(libc::dup(slave_fd)));
        cmd.stderr(Stdio::from_raw_fd(libc::dup(slave_fd)));
        cmd.pre_exec(move || {
            libc::setsid();
            let ws = libc::winsize {
                ws_row: 40,
                ws_col: 120,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            libc::ioctl(slave_fd, libc::TIOCSWINSZ as _, &ws);
            let _ = libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn shell under pty");
    let child_pid = child.id() as libc::pid_t;
    unsafe {
        libc::close(slave);
    }

    if !input.is_empty() {
        let mut writer = unsafe { std::fs::File::from_raw_fd(libc::dup(master)) };
        writer.write_all(input.as_bytes()).ok();
        writer.flush().ok();
    }

    let mut reader = unsafe { std::fs::File::from_raw_fd(master) };
    let start = Instant::now();
    let mut buf = [0u8; 4096];
    let mut out = String::new();
    loop {
        if out.contains(marker) {
            break; // the guest prompt rendered — the hook reached the real guest.
        }
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
        }
        if start.elapsed() > Duration::from_secs(90) {
            break;
        }
    }
    // Tear down the whole session (host shell + the guest shell it is blocked on); the caller's
    // `cleanup` then force-stops the sandbox VM itself.
    unsafe {
        libc::killpg(child_pid, libc::SIGKILL);
    }
    let _ = child.wait();
    out
}

/// Hook parity, real VM (#64): a REAL bash shell that sources `dome hook bash` boots a real guest
/// on `cd` into a trusted project. Bash's hook fires via `PROMPT_COMMAND`, so the shell is driven
/// interactively over a pty. We assert on the guest's sandbox-labeled prompt `[sandbox:<name>]`,
/// which only renders once the hook has dropped into the named guest VM.
#[test]
#[ignore]
fn bash_hook_drops_into_real_guest() {
    if !have("bash") {
        eprintln!("skipping: bash not available");
        return;
    }
    let name = sandbox_name("bash");
    cleanup(&name);

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    std::fs::write(
        project.join("dome.json"),
        format!("{{\"sandbox\":\"{name}\"}}"),
    )
    .unwrap();

    // Trust it (piped, non-interactive → no pin prompt) so the drop-in actually boots a VM.
    let allow = dome_in(project, &["allow"]);
    assert!(allow.status.success(), "dome allow failed");

    // Emit the real bash hook to a file the script sources, wiring DOME_HOOK_CMD at the real
    // codesigned binary so the drop-in boots an actual guest.
    let hook = project.join("hook.bash");
    let out = Command::new(dome_bin())
        .args(["hook", "bash"])
        .output()
        .expect("dome hook bash");
    std::fs::write(&hook, out.stdout).unwrap();

    // Source the hook, then `cd` in → PROMPT_COMMAND fires the hook → it boots the named guest.
    let input = format!(
        "export DOME_HOOK_CMD='{dome}'\nsource {hook}\ncd {proj}\n",
        dome = dome_bin(),
        hook = hook.display(),
        proj = project.display(),
    );
    let marker = format!("[sandbox:{name}]");
    let output = boot_guest_via_shell(&["bash", "--norc", "--noprofile", "-i"], &input, &marker);
    cleanup(&name);

    assert!(
        output.contains(&marker),
        "the bash hook must drop into the named guest (prompt `{marker}`); output:\n{output}"
    );
}

/// Hook parity, real VM (#64): a REAL fish shell that sources `dome hook fish` boots a real guest
/// on `cd` into a trusted project. Fish fires `--on-variable PWD` synchronously, so a single
/// `fish -i -c` script suffices. We assert on the guest's sandbox-labeled prompt `[sandbox:<name>]`.
#[test]
#[ignore]
fn fish_hook_drops_into_real_guest() {
    if !have("fish") {
        eprintln!("skipping: fish not available");
        return;
    }
    let name = sandbox_name("fish");
    cleanup(&name);

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    std::fs::write(
        project.join("dome.json"),
        format!("{{\"sandbox\":\"{name}\"}}"),
    )
    .unwrap();

    let allow = dome_in(project, &["allow"]);
    assert!(allow.status.success(), "dome allow failed");

    let hook = project.join("hook.fish");
    let out = Command::new(dome_bin())
        .args(["hook", "fish"])
        .output()
        .expect("dome hook fish");
    std::fs::write(&hook, out.stdout).unwrap();

    // fish wires DOME_HOOK_CMD with `set -gx`; the `cd` fires the PWD event, which boots the guest.
    let script = format!(
        "set -gx DOME_HOOK_CMD '{dome}'\nsource {hook}\ncd {proj}",
        dome = dome_bin(),
        hook = hook.display(),
        proj = project.display(),
    );
    let marker = format!("[sandbox:{name}]");
    let output = boot_guest_via_shell(&["fish", "--no-config", "-i", "-c", &script], "", &marker);
    cleanup(&name);

    assert!(
        output.contains(&marker),
        "the fish hook must drop into the named guest (prompt `{marker}`); output:\n{output}"
    );
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
