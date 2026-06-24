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

/// The number of preserved failure ("debug") disks currently on disk.
fn failed_disks() -> usize {
    std::fs::read_dir(provision_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("failed"))
                .count()
        })
        .unwrap_or(0)
}

/// A failing step fails the create, publishes nothing, and preserves a half-provisioned debug
/// disk that `dome provision debug` can boot — without re-running the steps. The marker the
/// first (succeeding) step wrote must be present on that debug disk.
#[test]
#[ignore]
fn failing_step_preserves_a_bootable_debug_disk() {
    let marker = format!("prov-fail-{}", std::process::id());
    let dir = tempfile::tempdir().expect("tempdir");
    // First step succeeds and leaves a marker; the second fails. The debug disk must carry the
    // marker (the half-provisioned state) and the second step's effect must be absent.
    let dome_json =
        format!(r#"{{ "provision": {{ "steps": ["echo ok > /opt/{marker}", "exit 7"] }} }}"#);
    std::fs::write(dir.path().join("dome.json"), dome_json).expect("write dome.json");

    let failed_before = failed_disks();
    let layers_before = published_layers();

    let run = run_in(dir.path(), "true");
    assert!(
        !run.status.success(),
        "a failing provision step must fail the create"
    );
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("exit 7"),
        "the failure must surface the step's exit code; stderr: {stderr}"
    );
    assert!(
        stderr.contains("dome provision debug"),
        "the failure must print the opt-in debug-shell hint; stderr: {stderr}"
    );

    assert_eq!(
        published_layers(),
        layers_before,
        "a failed build must publish nothing under the success hash"
    );
    assert_eq!(
        failed_disks(),
        failed_before + 1,
        "the half-provisioned disk must be preserved as <hash>.failed"
    );

    // Boot the debug disk without re-running steps. `provision debug` opens an interactive
    // `/bin/sh`, so drive it by piping a command to stdin: the first step's marker must be
    // present (the preserved half-provisioned state), proving the disk booted and no steps
    // re-ran.
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(dome_bin())
        .current_dir(dir.path())
        .args(["provision", "debug"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn dome provision debug");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("cat /opt/{marker}\nexit\n").as_bytes())
        .unwrap();
    let debug = child.wait_with_output().expect("debug shell output");
    assert!(
        String::from_utf8_lossy(&debug.stdout).contains("ok"),
        "the debug shell must boot the preserved half-provisioned disk and see the marker; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&debug.stdout),
        String::from_utf8_lossy(&debug.stderr),
    );

    // Best-effort cleanup of the failure disk this test produced.
    if let Ok(rd) = std::fs::read_dir(provision_dir()) {
        for e in rd.filter_map(|e| e.ok()) {
            if e.path().extension().and_then(|x| x.to_str()) == Some("failed") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Provision-time secrets (#68): the build can authenticate to a secret's matched host while
/// the guest only ever sees a placeholder, the allow-list (auto-whitelisting the secret host)
/// blocks a non-listed domain, and the placeholder is useless against an off-target host.
///
/// This needs egress to a real HTTPS host that echoes the request, so it talks to
/// `https://postman-echo.com/get` (which reflects request headers as JSON). The secret rides
/// an `Authorization` header: the proxy substitutes the real value only on egress to the
/// secret's matched host, so the echo from the matched host shows the real value while the
/// guest's own view (and any other host) shows only the `dome_tok_…` placeholder.
#[test]
#[ignore]
fn provision_secret_is_substituted_only_for_the_matched_host() {
    let marker = format!("prov-secret-{}", std::process::id());
    let dir = tempfile::tempdir().expect("tempdir");
    // allow lists only the echo host; the secret's host (also the echo host) is auto-whitelisted.
    // Step 1: the guest's own env must show the placeholder, never the real token.
    // Step 2: egress to the matched host echoes back the substituted (real) Authorization value.
    // Step 3: a curl to a NON-listed domain must be blocked by the provision allow-list.
    let dome_json = format!(
        r#"{{
  "provision": {{
    "allow": ["postman-echo.com"],
    "secrets": {{ "echo": {{ "from": "ECHO_TOKEN", "hosts": ["postman-echo.com"] }} }},
    "steps": [
      "test \"$ECHO_TOKEN\" != real-secret-value && echo placeholder-only > /opt/{marker}.guest",
      "curl -sS -H \"Authorization: $ECHO_TOKEN\" https://postman-echo.com/get > /opt/{marker}.echo",
      "curl -sS --max-time 10 https://example.com/ && echo REACHED > /opt/{marker}.blocked || echo BLOCKED > /opt/{marker}.blocked"
    ]
  }}
}}"#
    );
    std::fs::write(dir.path().join("dome.json"), dome_json).expect("write dome.json");

    let out = Command::new(dome_bin())
        .current_dir(dir.path())
        .env("ECHO_TOKEN", "real-secret-value")
        .args([
            "run",
            "--",
            "sh",
            "-c",
            &format!(
                "cat /opt/{marker}.guest; echo ---; cat /opt/{marker}.echo; echo ---; cat /opt/{marker}.blocked"
            ),
        ])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the provisioned run should succeed; stdout: {stdout}, stderr: {stderr}"
    );

    // The guest's own $ECHO_TOKEN was the placeholder, not the real value.
    assert!(
        stdout.contains("placeholder-only"),
        "the guest must see a placeholder, never the real token; stdout: {stdout}"
    );
    // The echo from the matched host reflects the *real* value (proxy substituted it on egress).
    assert!(
        stdout.contains("real-secret-value"),
        "egress to the matched host must carry the substituted real value; stdout: {stdout}"
    );
    // The placeholder must never appear on the wire to the matched host.
    assert!(
        !stdout.contains("dome_tok_"),
        "the placeholder must be substituted out on egress to the matched host; stdout: {stdout}"
    );
    // A non-listed domain was blocked by the provision allow-list.
    assert!(
        stdout.contains("BLOCKED") && !stdout.contains("REACHED"),
        "a non-listed domain must be blocked by the provision allow-list; stdout: {stdout}"
    );

    // Best-effort cleanup of the layer this test published.
    if let Ok(rd) = std::fs::read_dir(provision_dir()) {
        for e in rd.filter_map(|e| e.ok()) {
            let ext = e
                .path()
                .extension()
                .and_then(|x| x.to_str())
                .map(str::to_string);
            if matches!(ext.as_deref(), Some("idx") | Some("failed")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}
