//! Integration tests for mounts. Boots a real VM — requires codesigned binary.
//! All #[ignore]d by default. Run with:
//!   SHURU_BIN=target/debug/shuru cargo test -p shuru-cli -- --ignored

use std::process::Command;
use tempfile::tempdir;

fn shuru_bin() -> String {
    std::env::var("SHURU_BIN")
        .expect("SHURU_BIN not set — point it at a codesigned shuru binary (e.g. just build)")
}

fn run_in_vm(mount_spec: &str, guest_cmd: &str) -> std::process::Output {
    Command::new(shuru_bin())
        .args(["run", "--mount", mount_spec, "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn shuru — is SHURU_BIN correct?")
}

#[test]
#[ignore]
fn ro_mount_default_discards_guest_writes() {
    let tmp = tempdir().unwrap();
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
    let tmp = tempdir().unwrap();
    let host_dir = tmp.path().to_str().unwrap();
    let spec = format!("{}:/workspace:rw", host_dir);

    run_in_vm(&spec, "echo guest > /workspace/test.txt");

    let written = std::fs::read_to_string(tmp.path().join("test.txt"))
        .expect("guest write should land on host");
    assert_eq!(written.trim(), "guest");
}

#[test]
#[ignore]
fn rw_mount_guest_reads_host_files() {
    let tmp = tempdir().unwrap();
    std::fs::write(tmp.path().join("original.txt"), "host\n").unwrap();
    let host_dir = tmp.path().to_str().unwrap();
    let spec = format!("{}:/workspace:rw", host_dir);

    run_in_vm(&spec, "cp /workspace/original.txt /workspace/copy.txt");

    let copied = std::fs::read_to_string(tmp.path().join("copy.txt"))
        .expect("guest should be able to read and copy host files");
    assert_eq!(copied.trim(), "host");
}

#[test]
#[ignore]
fn explicit_ro_suffix_discards_guest_writes() {
    let tmp = tempdir().unwrap();
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
    let tmp = tempdir().unwrap();
    let host_dir = tmp.path().to_str().unwrap();
    let spec = format!("{}:/mnt/does/not/exist:rw", host_dir);

    let output = run_in_vm(&spec, "echo ok > /mnt/does/not/exist/proof.txt");

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
    let tmp = tempdir().unwrap();
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
