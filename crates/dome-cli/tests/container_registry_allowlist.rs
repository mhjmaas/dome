//! Integration test for issue #119 (Slice 5 / docs capstone of PRD #100): the user-facing claim
//! that to `docker pull` from inside a box you must add the **registry hosts** to `network.allow`,
//! and that container egress is policed **exactly** like VM-local traffic — so an allowed registry
//! pulls and a non-allowed registry is blocked.
//!
//! WHY THIS IS NEW. The earlier security tests (#116/#117/#118) pre-pulled their images in a
//! `provision` step that ran under *full* egress (no `network.allow`), so they never exercised a
//! pull under a *restricted* allow-list and never had to enumerate registry hosts. The docs added
//! in this slice tell users which hosts to allow for Docker Hub; this test proves that guidance is
//! correct against a real VM and that the allow-list stays tight (a different registry is blocked).
//!
//! ARCHITECTURE (verify-first, unchanged from #116). All guest traffic — including masqueraded
//! container traffic and Docker's embedded resolver forwarding from the root netns — leaves `eth0`
//! into the host smoltcp gateway, which REFUSEs DNS for any name not in `network.allow` and pins the
//! A-records of allowed names. So a `docker pull` only succeeds when every host the registry uses
//! (registry + auth + blob CDN) is in the list.
//!
//! Needs a codesigned binary and the container-capable base image (#115). `#[ignore]`d by default:
//!   just test-vm container_pull_requires_registry_hosts
//!
//! Cost: heavy (~2 min). Provision installs `docker.io` under full egress but does NOT pre-pull —
//! the runtime boot must pull through the restricted allow-list, which is the whole point.

use std::process::Command;

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. `just build`)")
}

fn data_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/.local/share/dome", home)
}

fn provision_dir() -> String {
    format!("{}/provision", data_dir())
}

/// Best-effort removal of the provision layer indexes this test may publish, so a rerun cold-builds
/// afresh and the global cache doesn't accumulate. Mirrors the other container tests' cleanup.
fn cleanup_published_layers() {
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

/// Provision step (full egress): install Docker only — deliberately NO pre-pull, so the runtime
/// boot below must pull through the restricted allow-list. `sync` flushes the install into the
/// snapshotted disk before the build VM stops.
const PROVISION_STEP: &str = r#"
set -e
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null 2>&1
apt-get install -y -qq docker.io >/dev/null 2>&1
sync
"#;

/// Runtime script. dockerd starts, then we exercise the registry-host allow-list documented for
/// #119:
///   * `docker pull hello-world` from Docker Hub — every Docker Hub host (registry/auth/blob CDN)
///     is in `network.allow`, so the pull resolves and completes, and the image runs.
///   * `docker pull quay.io/...` — quay.io is NOT in the list, so the gateway REFUSEs its DNS and
///     the pull cannot even resolve. A success here (`REGISTRY_LEAK`) would mean container egress
///     escaped the allow-list — a security regression.
const RUNTIME_SCRIPT: &str = r#"
set -e
dockerd >/tmp/dockerd.log 2>&1 &
for i in $(seq 1 40); do docker info >/dev/null 2>&1 && break; sleep 1; done
docker info >/dev/null 2>&1 || { echo DOCKERD_DOWN; tail -20 /tmp/dockerd.log; exit 1; }

# ALLOWED registry: every Docker Hub host is in network.allow, so the pull succeeds end to end
# (manifest from registry-1.docker.io, token from auth.docker.io, blobs from the R2/cloudflare CDN).
if docker pull hello-world:latest >/tmp/pull.log 2>&1; then
    echo REGISTRY_PULL_OK
else
    echo REGISTRY_PULL_FAILED; tail -20 /tmp/pull.log
fi
docker run --rm hello-world 2>/dev/null | grep -q "Hello from Docker" && echo HELLO_RAN || echo HELLO_FAILED

# NON-ALLOWED registry: quay.io is not in the list. The gateway REFUSEs its DNS, so the pull must
# fail. Any success is an allow-list bypass.
if docker pull quay.io/podman/hello:latest >/tmp/quay.log 2>&1; then
    echo REGISTRY_LEAK
else
    echo REGISTRY_BLOCKED_OK
fi
"#;

/// Boot a real box whose runtime `network.allow` lists exactly the Docker Hub registry hosts (plus
/// the CDN/auth wildcards the docs recommend) and assert the documented registry workflow: an
/// allowed registry pulls and runs, while a registry that is NOT listed is blocked. This is the
/// real-VM verification behind the #119 "adding registry hosts to network.allow" docs.
#[test]
#[ignore]
fn container_pull_requires_registry_hosts_in_network_allow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dome_json = serde_json::json!({
        // Images are large and disk_size is create-only — size the box up exactly as the docs say.
        "memory": 4096,
        "disk_size": 8192,
        // Networking must be ON for the allow-list to be enforced.
        "allow_net": true,
        // Provision (full egress) bakes Docker into the layer but does NOT pre-pull, so the pull
        // under the restricted list below is the thing under test.
        "provision": { "steps": [ PROVISION_STEP ] },
        // The exact registry-host set the #119 docs tell users to add for Docker Hub: the registry
        // and auth endpoints plus the blob-CDN wildcards. quay.io is intentionally absent so the
        // negative probe is genuinely blocked.
        "network": { "allow": [
            "registry-1.docker.io",
            "auth.docker.io",
            "*.docker.io",
            "*.docker.com",
            "*.cloudflarestorage.com"
        ] }
    });
    std::fs::write(dir.path().join("dome.json"), serde_json::to_string_pretty(&dome_json).unwrap())
        .expect("write dome.json");

    let out = Command::new(dome_bin())
        .current_dir(dir.path())
        .args(["run", "--", "sh", "-c", RUNTIME_SCRIPT])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    cleanup_published_layers();

    assert!(
        out.status.success(),
        "the registry-allowlist run should boot and exit cleanly; stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // The allowed registry pulled and ran — proving that listing the registry hosts in
    // network.allow is sufficient to pull through the restricted allow-list.
    assert!(
        stdout.lines().any(|l| l == "REGISTRY_PULL_OK"),
        "a pull from an allowed registry should succeed under a restricted allow-list; \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.lines().any(|l| l == "HELLO_RAN"),
        "the pulled image should run; stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // A registry NOT in the list is blocked at the gateway DNS — identical to VM-local. No leak.
    assert!(
        stdout.lines().any(|l| l == "REGISTRY_BLOCKED_OK"),
        "a pull from a non-allowed registry must be blocked; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.lines().any(|l| l == "REGISTRY_LEAK"),
        "a pull from a non-allowed registry leaked past the allow-list (security regression); \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
