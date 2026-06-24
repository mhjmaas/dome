//! Integration tests for declarative provisioning (PRD #66). The cold-build test boots a
//! real VM and requires a codesigned binary; all #[ignore]d by default. Run with:
//!   DOME_BIN=target/debug/dome cargo test -p dome-cli --test provision -- --ignored
//!
//! The end-to-end demo this asserts: a `dome.json` declaring `provision.steps` →
//! first `dome run` cold-builds the layer, publishes `provision/<hash>.idx`, and boots with
//! the toolchain present → a second `dome run` on the same spec is a cache-hit (the layer
//! file is not rebuilt) and still sees the toolchain.

use std::path::Path;
use std::process::Command;

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. just build)")
}

fn data_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/.local/share/dome", home)
}

fn provision_dir() -> String {
    format!("{}/provision", data_dir())
}

/// Write a `dome.json` with a provision block into a fresh temp project dir, returning it.
/// The step touches a marker file with no network dependency, so the build is hermetic
/// beyond booting the VM. Each call uses a unique marker so repeated runs key to a fresh
/// layer hash (and don't collide with a previously cached one).
fn project_with_provision(marker: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let dome_json = format!(
        r#"{{ "provision": {{ "steps": ["mkdir -p /opt && echo ok > /opt/{marker}"] }} }}"#
    );
    std::fs::write(dir.path().join("dome.json"), dome_json).expect("write dome.json");
    dir
}

/// `dome run` in `project_dir`, executing `guest_cmd` non-interactively.
fn run_in(project_dir: &Path, guest_cmd: &str) -> std::process::Output {
    Command::new(dome_bin())
        .current_dir(project_dir)
        .args(["run", "--", "sh", "-c", guest_cmd])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

/// The number of published layer indexes currently on disk.
fn published_layers() -> usize {
    std::fs::read_dir(provision_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("idx"))
                .count()
        })
        .unwrap_or(0)
}

/// Cold build on first `run`, cache-hit on the second: the toolchain is present both times,
/// and the published layer index is not rebuilt the second time.
#[test]
#[ignore]
fn cold_build_then_second_run_is_a_cache_hit() {
    let marker = format!("prov-{}", std::process::id());
    let project = project_with_provision(&marker);

    // First run: cold build. The step ran in the build VM, so the marker is present.
    let first = run_in(project.path(), &format!("cat /opt/{marker}"));
    assert!(
        first.status.success(),
        "first run should succeed; stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("ok"),
        "the provisioned toolchain marker must be present after the cold build; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr),
    );

    // The layer was published as a hidden checkpoint under provision/.
    let layers_after_first = published_layers();
    assert!(
        layers_after_first >= 1,
        "the cold build must publish a provision/<hash>.idx layer"
    );

    // Find the layer this spec keyed to and capture its mtime, so we can prove the second
    // run does not rebuild it.
    let layer = std::fs::read_dir(provision_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("idx"))
        .expect("a published layer index");
    let mtime_before = std::fs::metadata(&layer).unwrap().modified().unwrap();

    // Second run on the same spec: cache hit. The marker is still present (booted from the
    // cached layer), and the layer file was not rebuilt.
    let second = run_in(project.path(), &format!("cat /opt/{marker}"));
    assert!(
        second.status.success(),
        "second run should succeed; stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("ok"),
        "the cached layer must still carry the toolchain; stdout: {}",
        String::from_utf8_lossy(&second.stdout)
    );
    let mtime_after = std::fs::metadata(&layer).unwrap().modified().unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "a cache hit must not rebuild (and thus not rewrite) the published layer"
    );

    // Best-effort cleanup of the layer this test published.
    let _ = std::fs::remove_file(&layer);
}
