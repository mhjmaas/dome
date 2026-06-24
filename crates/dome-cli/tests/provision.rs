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
    layer_paths().len()
}

/// The set of published layer index paths currently on disk. Used to pin down the layer a
/// given run published by diffing before/after: the provision cache is global and accumulates
/// layers from other tests and prior runs, so picking "the first `.idx`" would grab an
/// arbitrary unrelated layer.
fn layer_paths() -> std::collections::HashSet<std::path::PathBuf> {
    std::fs::read_dir(provision_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("idx"))
                .collect()
        })
        .unwrap_or_default()
}

/// The single layer published between `before` and now (panics if not exactly one appeared).
fn newly_published_layer(before: &std::collections::HashSet<std::path::PathBuf>) -> std::path::PathBuf {
    let after = layer_paths();
    let mut new_layers = after.difference(before);
    let layer = new_layers
        .next()
        .cloned()
        .expect("a newly published layer index");
    assert!(
        new_layers.next().is_none(),
        "expected exactly one newly published layer for this spec"
    );
    layer
}

/// Cold build on first `run`, cache-hit on the second: the toolchain is present both times,
/// and the published layer index is not rebuilt the second time.
#[test]
#[ignore]
fn cold_build_then_second_run_is_a_cache_hit() {
    let marker = format!("prov-{}", std::process::id());
    let project = project_with_provision(&marker);
    let layers_before = layer_paths();

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

    // The cold build published exactly one new layer under provision/. Capture it (and its
    // mtime) so we can prove the second run does not rebuild it.
    let layer = newly_published_layer(&layers_before);
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
    "secrets": {{ "ECHO_TOKEN": {{ "from": "ECHO_TOKEN", "hosts": ["postman-echo.com"] }} }},
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

/// `--rebuild` (#69) forces a fresh provision even when a cached layer would otherwise be
/// served: the published layer is rewritten in place (its mtime advances), `dome provision
/// list` shows it, and the toolchain marker is still present after the rebuild.
#[test]
#[ignore]
fn rebuild_forces_a_fresh_build_and_list_shows_the_layer() {
    let marker = format!("prov-rebuild-{}", std::process::id());
    let project = project_with_provision(&marker);
    let layers_before = layer_paths();

    // First run: cold build publishes the layer.
    let first = run_in(project.path(), &format!("cat /opt/{marker}"));
    assert!(
        first.status.success(),
        "first run should succeed; stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let layer = newly_published_layer(&layers_before);
    let mtime_before = std::fs::metadata(&layer).unwrap().modified().unwrap();

    // `dome provision list` shows the cached layer (and not via `checkpoint list`).
    let list = Command::new(dome_bin())
        .args(["provision", "list"])
        .output()
        .expect("failed to spawn dome provision list");
    let list_out = format!(
        "{}{}",
        String::from_utf8_lossy(&list.stdout),
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(
        list_out.contains("HASH") && list_out.contains("current"),
        "provision list must show the cached layer with a status; out: {list_out}"
    );

    // `--rebuild` rewrites the SAME layer in place: the toolchain is still present and the
    // layer file's mtime advanced (a plain cache hit would have left it untouched).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let rebuilt = Command::new(dome_bin())
        .current_dir(project.path())
        .args([
            "run",
            "--rebuild",
            "--",
            "sh",
            "-c",
            &format!("cat /opt/{marker}"),
        ])
        .output()
        .expect("failed to spawn dome run --rebuild");
    assert!(
        rebuilt.status.success(),
        "rebuild run should succeed; stderr: {}",
        String::from_utf8_lossy(&rebuilt.stderr)
    );
    assert!(
        String::from_utf8_lossy(&rebuilt.stdout).contains("ok"),
        "the freshly rebuilt layer must still carry the toolchain; stdout: {}",
        String::from_utf8_lossy(&rebuilt.stdout)
    );
    let mtime_after = std::fs::metadata(&layer).unwrap().modified().unwrap();
    assert!(
        mtime_after > mtime_before,
        "--rebuild must rewrite the published layer in place (mtime should advance)"
    );

    // Best-effort cleanup of the layer this test published.
    let _ = std::fs::remove_file(&layer);
}

/// Compose (#70): when a creation both declares a `provision` block AND seeds `--from <seed>`,
/// the steps run on top of the seeded disk (not the bare base). This creates a seed checkpoint
/// carrying its own marker, then `dome sandbox create --from <seed>` in a project whose
/// `provision` block writes a second marker — and asserts BOTH markers are present in the
/// resulting sandbox (seed content survived; provisioning layered on top).
#[test]
#[ignore]
fn provision_composes_on_top_of_a_seeded_checkpoint() {
    let id = std::process::id();
    let seed_marker = format!("seed-{id}");
    let prov_marker = format!("prov-compose-{id}");
    let ckpt = format!("compose-seed-{id}");
    let sandbox = format!("compose-sb-{id}");

    // 1. Build a seed checkpoint that carries its own marker. Created from a project with NO
    //    provision block, so the checkpoint is just the bare base + the seed marker.
    let seed_project = tempfile::tempdir().expect("tempdir");
    let ckpt_out = Command::new(dome_bin())
        .current_dir(seed_project.path())
        .args([
            "checkpoint",
            "create",
            &ckpt,
            "--",
            "sh",
            "-c",
            &format!("mkdir -p /opt && echo seedok > /opt/{seed_marker}"),
        ])
        .output()
        .expect("failed to spawn dome checkpoint create");
    assert!(
        ckpt_out.status.success(),
        "seed checkpoint create should succeed; stderr: {}",
        String::from_utf8_lossy(&ckpt_out.stderr)
    );

    // 2. A project whose provision block writes a SECOND marker on top of whatever it seeds.
    let project = project_with_provision(&prov_marker);

    // 3. Create the sandbox composing the provision steps on top of the seed checkpoint.
    let layers_before = layer_paths();
    let create = Command::new(dome_bin())
        .current_dir(project.path())
        .args(["sandbox", "create", &sandbox, "--from", &ckpt])
        .output()
        .expect("failed to spawn dome sandbox create --from");
    assert!(
        create.status.success(),
        "composed sandbox create should succeed; stderr: {}",
        String::from_utf8_lossy(&create.stderr)
    );
    // The composed layer this create published — identified so the regression check below can
    // scope its assertion to OUR layer rather than any pre-existing broken layer in the shared
    // provision cache (the cache accumulates across runs, and a pre-flatten-fix layer would
    // otherwise make the check spuriously fail).
    let our_layer = newly_published_layer(&layers_before);
    let our_layer_file = our_layer
        .file_name()
        .and_then(|s| s.to_str())
        .expect("layer file name")
        .to_string();

    // 4. Both markers must be present: the seed's content survived AND provisioning layered the
    //    toolchain marker on top.
    let run = Command::new(dome_bin())
        .current_dir(project.path())
        .args([
            "sandbox",
            "run",
            &sandbox,
            "--",
            "sh",
            "-c",
            &format!("cat /opt/{seed_marker} /opt/{prov_marker}"),
        ])
        .output()
        .expect("failed to spawn dome sandbox run");
    let out = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "composed sandbox run should succeed; stderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        out.contains("seedok"),
        "the seed's content must survive the compose; stdout: {out}"
    );
    assert!(
        out.contains("ok"),
        "provisioning must layer its marker on top of the seed; stdout: {out}"
    );

    // 5. Regression: the composed layer must be SELF-CONTAINED, not chained to the seed it was
    //    built `--from`. Delete the seed checkpoint, then `dome prune`: the cached
    //    provision/<hash>.idx must not dangle. A chained layer (the bug) would record
    //    parent_path = <seed index>; once the seed is gone, prune's mark phase would fail to
    //    load that parent and skip the layer with a "skipping unreadable index" warning,
    //    stranding the layer's own chunks. A flattened (parent-less) layer prunes cleanly.
    let del = Command::new(dome_bin())
        .args(["checkpoint", "delete", &ckpt])
        .output()
        .expect("failed to spawn dome checkpoint delete");
    assert!(
        del.status.success(),
        "deleting the seed checkpoint should succeed; stderr: {}",
        String::from_utf8_lossy(&del.stderr)
    );
    let prune = Command::new(dome_bin())
        .arg("prune")
        .output()
        .expect("failed to spawn dome prune");
    let prune_err = String::from_utf8_lossy(&prune.stderr);
    assert!(
        prune.status.success(),
        "prune should succeed after the seed is gone; stderr: {prune_err}"
    );
    // Scope the check to OUR layer: prune may warn about unrelated pre-existing broken layers
    // in the shared cache, but it must NOT report ours as dangling once its seed is deleted.
    assert!(
        !prune_err
            .lines()
            .any(|l| l.contains("skipping unreadable index") && l.contains(&our_layer_file)),
        "the composed layer must be self-contained: prune found a dangling parent for our layer \
         ({our_layer_file}) after the seed checkpoint was deleted; stderr: {prune_err}"
    );
    // The composed sandbox (which flattened the layer at seed time) still runs after the prune.
    let after = Command::new(dome_bin())
        .current_dir(project.path())
        .args([
            "sandbox",
            "run",
            &sandbox,
            "--",
            "sh",
            "-c",
            &format!("cat /opt/{seed_marker} /opt/{prov_marker}"),
        ])
        .output()
        .expect("failed to spawn dome sandbox run after prune");
    assert!(
        after.status.success() && String::from_utf8_lossy(&after.stdout).contains("seedok"),
        "the composed sandbox must still run after the seed is deleted and pruned; stderr: {}",
        String::from_utf8_lossy(&after.stderr)
    );

    // Best-effort cleanup.
    let _ = Command::new(dome_bin())
        .args(["sandbox", "rm", &sandbox])
        .output();
}

/// Runtime project-root mount (#71): a project with a `dome.json` boots a runtime guest where
/// the project root (the dir containing `dome.json`) is mounted at the standard guest path
/// `/workspace`, honoring `allow_host_writes`. A host file under the project is visible inside
/// the guest at `/workspace/...`, and — because `allow_host_writes` is set — a file the guest
/// writes under `/workspace` lands back on the host. The provision BUILD phase stays unmounted
/// (covered by the other tests, which never mount the project dir into the build VM).
#[test]
#[ignore]
fn runtime_mounts_the_project_root_at_workspace_writable() {
    let id = std::process::id();
    let project = tempfile::tempdir().expect("tempdir");
    // A writable project: allow_host_writes makes the auto-mount read-write.
    std::fs::write(
        project.path().join("dome.json"),
        r#"{ "allow_host_writes": true }"#,
    )
    .expect("write dome.json");

    // A host file under the project root must be visible inside the guest at /workspace.
    let host_marker = format!("host-{id}");
    std::fs::write(project.path().join(&host_marker), "hostok\n").expect("write host marker");

    let guest_marker = format!("guest-{id}");
    let out = run_in(
        project.path(),
        &format!("cat /workspace/{host_marker} && echo guestok > /workspace/{guest_marker}"),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "runtime run should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("hostok"),
        "the host project file must be visible inside the guest at /workspace; stdout: {stdout}"
    );

    // The guest's write under /workspace must appear on the host (writable mount).
    let written = std::fs::read_to_string(project.path().join(&guest_marker))
        .expect("guest write should land on the host under the project root");
    assert!(
        written.contains("guestok"),
        "the guest's /workspace write must reach the host; got: {written}"
    );
}
