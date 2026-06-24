//! Integration tests for mounts. Boots a real VM — requires codesigned binary.
//! All #[ignore]d by default. Run with:
//!   DOME_BIN=target/debug/dome cargo test -p dome-cli -- --ignored

use std::process::Command;
use tempfile::TempDir;

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. just build)")
}

/// A temp dir for a mount **inside the current working directory**. `dome` confines mounts
/// to paths within CWD ("Only paths within CWD can be mounted"), so a mount host dir under
/// the system temp dir (the default for `tempfile::tempdir()`) would be rejected before the
/// VM ever boots. Creating it under CWD mirrors real usage (you mount project-local paths).
fn host_tmp() -> TempDir {
    let cwd = std::env::current_dir().expect("cwd");
    TempDir::new_in(cwd).expect("failed to create temp mount dir under CWD")
}

fn run_in_vm(mount_spec: &str, guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args(["run", "--mount", mount_spec, "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

/// Like [`run_in_vm`], but grants `--allow-host-writes`. A `:rw` mount is gated behind this
/// flag (writes to the host are opt-in), so the read-write cases must pass it or the boot is
/// refused before it starts.
fn run_in_vm_rw(mount_spec: &str, guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .args([
            "run",
            "--allow-host-writes",
            "--mount",
            mount_spec,
            "--",
            "sh",
            "-c",
            guest_cmd,
        ])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

#[test]
#[ignore]
fn ro_mount_default_discards_guest_writes() {
    let tmp = host_tmp();
    let host_dir = tmp.path().to_str().unwrap();
    let spec = format!("{}:/workspace", host_dir);

    run_in_vm(&spec, "echo guest > /workspace/test.txt");

    assert!(
        !tmp.path().join("test.txt").exists(),
        "default mount should not write to host"
    );
}

#[test]
#[ignore]
fn rw_mount_writes_land_on_host() {
    let tmp = host_tmp();
    let host_dir = tmp.path().to_str().unwrap();
    let spec = format!("{}:/workspace:rw", host_dir);

    run_in_vm_rw(&spec, "echo guest > /workspace/test.txt");

    let written = std::fs::read_to_string(tmp.path().join("test.txt"))
        .expect("guest write should land on host");
    assert_eq!(written.trim(), "guest");
}

#[test]
#[ignore]
fn rw_mount_guest_reads_host_files() {
    let tmp = host_tmp();
    std::fs::write(tmp.path().join("original.txt"), "host\n").unwrap();
    let host_dir = tmp.path().to_str().unwrap();
    let spec = format!("{}:/workspace:rw", host_dir);

    run_in_vm_rw(&spec, "cp /workspace/original.txt /workspace/copy.txt");

    let copied = std::fs::read_to_string(tmp.path().join("copy.txt"))
        .expect("guest should be able to read and copy host files");
    assert_eq!(copied.trim(), "host");
}

#[test]
#[ignore]
fn explicit_ro_suffix_discards_guest_writes() {
    let tmp = host_tmp();
    let host_dir = tmp.path().to_str().unwrap();
    let spec = format!("{}:/workspace:ro", host_dir);

    run_in_vm(&spec, "echo ghost > /workspace/test.txt");

    assert!(
        !tmp.path().join("test.txt").exists(),
        ":ro mount should not write to host"
    );
}

#[test]
#[ignore]
fn rw_mount_creates_missing_guest_dirs() {
    let tmp = host_tmp();
    let host_dir = tmp.path().to_str().unwrap();
    let spec = format!("{}:/mnt/does/not/exist:rw", host_dir);

    let output = run_in_vm_rw(&spec, "echo ok > /mnt/does/not/exist/proof.txt");

    assert!(
        output.status.success(),
        "rw mount to non-existent guest path should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let written = std::fs::read_to_string(tmp.path().join("proof.txt"))
        .expect("write through rw mount at auto-created guest path should land on host");
    assert_eq!(written.trim(), "ok");
}

#[test]
#[ignore]
fn ro_mount_creates_missing_guest_dirs() {
    let tmp = host_tmp();
    std::fs::write(tmp.path().join("hello.txt"), "from-host\n").unwrap();
    let host_dir = tmp.path().to_str().unwrap();
    let spec = format!("{}:/mnt/does/not/exist:ro", host_dir);

    let output = run_in_vm(&spec, "cat /mnt/does/not/exist/hello.txt");

    assert!(
        output.status.success(),
        "ro mount to non-existent guest path should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("from-host"),
        "should read host file through overlay at auto-created guest path"
    );
}
