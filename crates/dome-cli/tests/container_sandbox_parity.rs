//! Integration test for issue #118 (Slice 4 of PRD #100): the container story must behave
//! **identically for a persistent `dome sandbox` as for an ephemeral `dome run`** — the runtime
//! binary and pulled/built images survive across sessions via CAS, and egress policing (#116) plus
//! dome-CA propagation (#117) hold exactly the same in a later session as in the first.
//!
//! WHY A SEPARATE TEST. #116 proved egress policing and #117 proved CA propagation, but both ran
//! through ephemeral `dome run`. A sandbox cold-boots from a CAS-chunked disk that was saved when a
//! previous session's worker stopped, then re-applies the unconditional boot-time policy + CA
//! shim. This slice is the end-to-end proof that the persisted disk (which now contains a real
//! container runtime and its image store under `/var/lib/docker`) and the boot-time policy/CA
//! application compose correctly: nothing about persistence weakens or bypasses policing, and a
//! container started in session N is policed exactly like one in session 1.
//!
//! VERIFY-FIRST. dome saves the entire writable root filesystem as the sandbox checkpoint
//! (crates/dome-cli/src/session.rs `save_checkpoint` chunks the whole `work_rootfs`), so the docker
//! binary and `/var/lib/docker` persist by construction; egress policing and CA injection are
//! unconditional boot properties re-applied on every cold boot. This test asserts that contract
//! end to end rather than assuming it.
//!
//! HOW IT PROVES THE CONTRACT. One sandbox, created (lazily, on first `sandbox run`) from a
//! `provision` block that bakes `docker.io` and pre-pulls `curlimages/curl`. A `secret` binds
//! `postman-echo.com` (forcing the proxy to MITM that host and inject its per-boot CA), and the
//! allow-list is exactly `["postman-echo.com"]`.
//!
//!   * SESSION 1 (cold boot): start dockerd; `docker build` a NEW image `dome-persist:v1` from the
//!     pre-pulled base entirely offline (legacy builder, a `RUN echo` that needs no egress) — a
//!     session-time write into `/var/lib/docker`. Then, from inside containers: HTTPS to the
//!     MITM'd host returns 200 with no `-k` (CA trusted), a secret placeholder sent from inside the
//!     container is substituted upstream, and a non-allowed host (`example.org`) is BLOCKED.
//!   * worker is stopped → the disk (now holding the runtime + both images) is chunked to CAS.
//!   * SESSION 2 (cold boot from the saved disk): start dockerd; assert `dome-persist:v1` and
//!     `curlimages/curl` are STILL present with no rebuild/re-pull (persistence), and a marker file
//!     baked into the image at session-1 build time reads back (proves it is the same image, not a
//!     fresh build). Then re-run the exact same policing + CA probes and assert identical results —
//!     a container in a later session is policed exactly like one in the first.
//!
//! Needs a codesigned binary and a container-capable base image (#115). `#[ignore]`d by default;
//! run with:
//!   just test-vm container_sandbox_parity
//!
//! Cost: heavy (two cold boots + a CAS save of a multi-hundred-MB docker store). Depends on the
//! public `postman-echo.com` echo service, as do the MITM tests in sandbox_persist.rs and the
//! container CA test. Images are large and `disk_size` is create-only, so the box is sized up via
//! dome.json exactly as the docs tell users to.

use std::process::Command;
use std::time::{Duration, Instant};

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. `just build`)")
}

fn data_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/.local/share/dome", home)
}

/// A unique sandbox name per test run so repeated runs don't collide on the global namespace.
fn sandbox_name() -> String {
    format!("itest-{}-ctr-parity", std::process::id())
}

/// The real secret value. It is never written into dome.json — it lives only in the host env the
/// `secret`'s `from` reads — and must surface upstream (echoed by postman-echo) only because the
/// proxy substituted the placeholder on the container's request.
const REAL_SECRET: &str = "real-secret-value-118";

/// Run one `dome sandbox run <name> -- sh -c <script>` session in `cwd` with `ECHO_TOKEN` in the
/// env (read at boot to resolve the secret), waiting for it to exit.
fn sandbox_session(name: &str, cwd: &std::path::Path, script: &str) -> std::process::Output {
    Command::new(dome_bin())
        .current_dir(cwd)
        .env("ECHO_TOKEN", REAL_SECRET)
        .args(["sandbox", "run", name, "--", "sh", "-c", script])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?")
}

/// Stop a sandbox's persistent worker so it saves the disk to CAS and releases the lock, then the
/// next session cold-boots from that saved state. SIGTERM triggers a clean save+teardown. The disk
/// here holds a multi-hundred-MB docker store, so poll until the worker process is really gone
/// (chunking can take many seconds) rather than sleeping a fixed amount.
fn stop_worker(name: &str) {
    let _ = Command::new("pkill")
        .args(["-TERM", "-f", &format!("__worker {}", name)])
        .output();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let still = Command::new("pgrep")
            .args(["-f", &format!("__worker {}", name)])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !still || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // Small grace for the index rename to settle after the process exits.
    std::thread::sleep(Duration::from_secs(1));
}

/// Best-effort teardown independent of the binary under test: stop the worker, then drop the index
/// and any stale lock directly so a broken `rm` can never strand other tests.
fn rm_sandbox(name: &str) {
    stop_worker(name);
    let dir = format!("{}/sandboxes", data_dir());
    let _ = std::fs::remove_file(format!("{}/{}.idx", dir, name));
    let _ = std::fs::remove_file(format!("{}/{}.lock", dir, name));
}

/// Best-effort removal of any provision-layer indexes this test published, so a rerun cold-builds
/// afresh and the global cache doesn't accumulate. Mirrors container_ca_propagation.rs.
fn cleanup_published_layers() {
    let provision_dir = format!("{}/provision", data_dir());
    if let Ok(rd) = std::fs::read_dir(provision_dir) {
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

/// Provision step (full egress): install Docker, start dockerd, and pre-pull the curl image so the
/// policed runtime sessions need no registry access. Mirrors the container_*.rs tests.
const PROVISION_STEP: &str = r#"
set -e
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null 2>&1
apt-get install -y -qq docker.io >/dev/null 2>&1
dockerd >/tmp/dockerd-prov.log 2>&1 &
for i in $(seq 1 40); do docker info >/dev/null 2>&1 && break; sleep 1; done
docker info >/dev/null 2>&1 || { echo PROVISION_DOCKERD_DOWN; tail -20 /tmp/dockerd-prov.log; exit 1; }
docker pull curlimages/curl:latest >/dev/null 2>&1
sync
"#;

/// SESSION 1: build a session-time image offline from the provisioned base (a write into
/// /var/lib/docker that must persist), then exercise the policed container path (#116/#117).
const SESSION1_SCRIPT: &str = r#"
set -e
dockerd >/tmp/dockerd.log 2>&1 &
for i in $(seq 1 40); do docker info >/dev/null 2>&1 && break; sleep 1; done
docker info >/dev/null 2>&1 || { echo DOCKERD_DOWN; tail -20 /tmp/dockerd.log; exit 1; }
docker image inspect curlimages/curl:latest >/dev/null 2>&1 || { echo NO_BASE_IMAGE; exit 1; }

# Build a NEW image in THIS session, fully offline (legacy builder, RUN needs no egress), from the
# pre-pulled base. The marker file proves later that session 2 sees the same built image.
mkdir -p /tmp/img
printf 'FROM curlimages/curl:latest\nUSER root\nRUN echo persist-marker-118 > /persist-marker.txt\n' > /tmp/img/Dockerfile
DOCKER_BUILDKIT=0 docker build -q -t dome-persist:v1 /tmp/img >/tmp/build.log 2>&1 || { echo BUILD_FAILED; tail -30 /tmp/build.log; exit 1; }
docker image inspect dome-persist:v1 >/dev/null 2>&1 && echo SESSION1_BUILT_IMAGE || echo SESSION1_BUILD_MISSING

# (1) Container HTTPS to the MITM'd host with NO -k: 200 means the container trusted dome's CA.
docker run --rm dome-persist:v1 -sS --max-time 30 -o /dev/null \
    -w "S1_MITM_HTTPS %{http_code}\n" https://postman-echo.com/get || echo S1_MITM_HTTPS_FAILED

# (2) Secret placeholder sent from inside the container must be substituted upstream.
BODY=$(docker run --rm dome-persist:v1 -sS --max-time 30 \
    -H "Authorization: $ECHO_TOKEN" https://postman-echo.com/get 2>/dev/null || true)
case "$BODY" in *real-secret-value-118*) echo S1_SECRET_REACHED_UPSTREAM ;; *) echo "S1_SECRET_MISSING" ;; esac

# (3) A non-allowed host must be BLOCKED at the gateway DNS — no leak.
if docker run --rm dome-persist:v1 -sS --max-time 20 -o /dev/null -w "%{http_code}" \
    https://example.org/ 2>/dev/null | grep -q 200; then echo S1_LEAK_EXAMPLE_ORG; else echo S1_BLOCKED_EXAMPLE_ORG; fi
sync
"#;

/// SESSION 2 (cold boot from the CAS-saved disk): assert the runtime + both images persisted with
/// no rebuild/re-pull, then re-run the identical policing + CA probes.
const SESSION2_SCRIPT: &str = r#"
set -e
dockerd >/tmp/dockerd.log 2>&1 &
for i in $(seq 1 40); do docker info >/dev/null 2>&1 && break; sleep 1; done
docker info >/dev/null 2>&1 || { echo DOCKERD_DOWN; tail -20 /tmp/dockerd.log; exit 1; }

# Persistence: the session-1-built image and the provisioned base are still present (no re-pull /
# no rebuild — this boot has a tight allow-list and could not reach a registry anyway).
docker image inspect dome-persist:v1 >/dev/null 2>&1 && echo S2_IMAGE_PERSISTED || echo S2_IMAGE_MISSING
docker image inspect curlimages/curl:latest >/dev/null 2>&1 && echo S2_BASE_PERSISTED || echo S2_BASE_MISSING
# The marker baked at session-1 build time proves it is the SAME image, not a fresh build.
docker run --rm --entrypoint cat dome-persist:v1 /persist-marker.txt 2>/dev/null | grep -q persist-marker-118 \
    && echo S2_MARKER_OK || echo S2_MARKER_MISSING

# Policing + CA must be IDENTICAL to session 1 — a later-session container is policed the same way.
docker run --rm dome-persist:v1 -sS --max-time 30 -o /dev/null \
    -w "S2_MITM_HTTPS %{http_code}\n" https://postman-echo.com/get || echo S2_MITM_HTTPS_FAILED
BODY=$(docker run --rm dome-persist:v1 -sS --max-time 30 \
    -H "Authorization: $ECHO_TOKEN" https://postman-echo.com/get 2>/dev/null || true)
case "$BODY" in *real-secret-value-118*) echo S2_SECRET_REACHED_UPSTREAM ;; *) echo "S2_SECRET_MISSING" ;; esac
if docker run --rm dome-persist:v1 -sS --max-time 20 -o /dev/null -w "%{http_code}" \
    https://example.org/ 2>/dev/null | grep -q 200; then echo S2_LEAK_EXAMPLE_ORG; else echo S2_BLOCKED_EXAMPLE_ORG; fi
"#;

/// End-to-end sandbox parity for the container story (#118). Provisions a container-capable
/// sandbox, then across two cold-booted sessions proves: a session-built image persists via CAS,
/// and egress policing + CA propagation behave identically in the later session as in the first.
#[test]
#[ignore]
fn container_runtime_and_images_persist_and_stay_policed_across_sandbox_sessions() {
    let name = sandbox_name();
    rm_sandbox(&name);

    let dir = tempfile::tempdir().expect("tempdir");
    let dome_json = serde_json::json!({
        // Images are large and disk_size is create-only — size the box up as the docs tell users.
        "memory": 4096,
        "disk_size": 8192,
        // Networking on so the proxy (and thus MITM + CA injection + allow-list) is active.
        "allow_net": true,
        // Provision (full egress) bakes Docker + the base image into the disk at creation so the
        // policed runtime sessions need no registry and the allow-list can stay minimal.
        "provision": { "steps": [ PROVISION_STEP ] },
        // Tight allow-list: only the MITM'd host is reachable at runtime.
        "network": { "allow": ["postman-echo.com"] },
        // The secret binds postman-echo.com: this forces the proxy to MITM that host (present a
        // dome-CA-signed cert) and inject the CA into the VM trust store. `from` reads the real
        // value from the host env — it never appears in dome.json.
        "secrets": { "ECHO_TOKEN": { "from": "ECHO_TOKEN", "hosts": ["postman-echo.com"] } }
    });
    std::fs::write(
        dir.path().join("dome.json"),
        serde_json::to_string_pretty(&dome_json).unwrap(),
    )
    .expect("write dome.json");

    // SESSION 1: cold boot (lazily creates + provisions the sandbox), build an image, probe policing.
    let s1 = sandbox_session(&name, dir.path(), SESSION1_SCRIPT);
    let s1_out = String::from_utf8_lossy(&s1.stdout).to_string();
    let s1_err = String::from_utf8_lossy(&s1.stderr).to_string();

    // Persist the disk to CAS (worker save on stop), then SESSION 2 cold-boots from it.
    stop_worker(&name);
    let s2 = sandbox_session(&name, dir.path(), SESSION2_SCRIPT);
    let s2_out = String::from_utf8_lossy(&s2.stdout).to_string();
    let s2_err = String::from_utf8_lossy(&s2.stderr).to_string();

    // Cleanup before assertions so a failure doesn't strand the sandbox / leak layers.
    rm_sandbox(&name);
    cleanup_published_layers();

    let dump = || {
        format!(
            "--- session1 stdout ---\n{s1_out}\n--- session1 stderr ---\n{s1_err}\n\
             --- session2 stdout ---\n{s2_out}\n--- session2 stderr ---\n{s2_err}"
        )
    };

    assert!(
        s1.status.success(),
        "session 1 should boot and exit cleanly;\n{}",
        dump()
    );
    assert!(
        s2.status.success(),
        "session 2 should boot and exit cleanly;\n{}",
        dump()
    );

    // SESSION 1 — the runtime works and the policed container path matches ephemeral (#116/#117).
    assert!(
        s1_out.lines().any(|l| l == "SESSION1_BUILT_IMAGE"),
        "session 1 must build an image in-session;\n{}",
        dump()
    );
    assert!(
        s1_out.lines().any(|l| l == "S1_MITM_HTTPS 200"),
        "session 1 container must trust dome's CA and reach the MITM'd host over HTTPS (no -k);\n{}",
        dump()
    );
    assert!(
        s1_out.lines().any(|l| l == "S1_SECRET_REACHED_UPSTREAM"),
        "session 1 must substitute the secret on a container-originated request;\n{}",
        dump()
    );
    assert!(
        s1_out.lines().any(|l| l == "S1_BLOCKED_EXAMPLE_ORG")
            && !s1_out.contains("S1_LEAK_EXAMPLE_ORG"),
        "session 1 must BLOCK a non-allowed host (no leak);\n{}",
        dump()
    );

    // SESSION 2 — persistence: the runtime + both images survived the CAS round-trip.
    assert!(
        s2_out.lines().any(|l| l == "S2_IMAGE_PERSISTED"),
        "the session-1-built image must persist into session 2 (no rebuild);\n{}",
        dump()
    );
    assert!(
        s2_out.lines().any(|l| l == "S2_BASE_PERSISTED"),
        "the provisioned base image must persist into session 2 (no re-pull);\n{}",
        dump()
    );
    assert!(
        s2_out.lines().any(|l| l == "S2_MARKER_OK"),
        "the persisted image must be the SAME one built in session 1 (marker readback);\n{}",
        dump()
    );

    // SESSION 2 — policing + CA are IDENTICAL to session 1 for a later-session container.
    assert!(
        s2_out.lines().any(|l| l == "S2_MITM_HTTPS 200"),
        "a later-session container must trust dome's CA identically to the first session;\n{}",
        dump()
    );
    assert!(
        s2_out.lines().any(|l| l == "S2_SECRET_REACHED_UPSTREAM"),
        "secret injection must behave identically in a later sandbox session;\n{}",
        dump()
    );
    assert!(
        s2_out.lines().any(|l| l == "S2_BLOCKED_EXAMPLE_ORG")
            && !s2_out.contains("S2_LEAK_EXAMPLE_ORG"),
        "a later-session container must be policed exactly like the first (non-allowed host blocked);\n{}",
        dump()
    );
}
