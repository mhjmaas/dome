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

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

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
