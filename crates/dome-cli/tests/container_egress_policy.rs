//! Integration test for issue #116 (Slice 2 of PRD #100): a container runtime's egress must be
//! bound by dome's `network.allow` policy **exactly** like VM-local traffic, applied
//! unconditionally at boot.
//!
//! ARCHITECTURE (verify-first). There is no guest-side iptables interception. All guest traffic
//! leaves `eth0` (10.0.0.2) into a host-side smoltcp userspace gateway (10.0.0.1) that:
//!   * answers DNS on 10.0.0.1:53, REFUSEs any name not in `network.allow`, and pins the A-record
//!     IPs of allowed names into an allowed-IP set (crates/dome-proxy/src/dns.rs);
//!   * proxies every TCP connection, rejecting a destination IP that was never DNS-pinned and a
//!     TLS SNI not in the allow-list (crates/dome-proxy/src/proxy.rs).
//!
//! A container runtime MASQUERADEs container traffic to `eth0`'s source IP (10.0.0.2) and Docker's
//! embedded resolver forwards upstream from the root netns, so container DNS + TCP *should* already
//! traverse this same gateway and be policed identically. This test PROVES that end to end.
//!
//! It needs a codesigned binary and a base image built from the container-capable `dome_defconfig`
//! (#115). `#[ignore]`d by default; run with:
//!   just test-vm container_egress
//!
//! Cost: this is a heavy test. It cold-builds a provision layer (full egress) that installs
//! `docker.io` and pre-pulls the `curlimages/curl` image — so the *runtime* boot needs no registry
//! access and the allow-list can be exactly `["example.com"]`, isolating the security property.
//! Container images are large and `disk_size` is create-only, so the box is sized up via dome.json.

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

/// Best-effort removal of the provision layer indexes this test may have published, so a rerun
/// cold-builds afresh and the global cache doesn't accumulate. Mirrors provision.rs's cleanup.
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

/// Provision step (full egress): install Docker, start dockerd, and pre-pull the curl image so the
/// runtime boot needs no registry access. Runs as root via `sh -c`; the VM stays up across the
/// step, and `sync` flushes /var/lib/docker into the snapshotted disk before the build VM stops.
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

/// Runtime script (tight allow-list = ["example.com"]). dockerd restarts against the pre-pulled
/// image store, then three containers probe egress:
///   * HTTPS to example.com  — ALLOWED: exercises container DNS pinning + IP allow + TLS SNI.
///   * HTTP  to example.com  — ALLOWED: same path without TLS.
///   * HTTPS to example.org  — NOT in the list: the gateway REFUSEs its DNS, so the container can't
///     resolve/connect. A leak (`LEAK 200`) would be a security regression.
///
/// Each marker is printed on its own line so a single VM boot reports every outcome.
const RUNTIME_SCRIPT: &str = r#"
set -e
grep -qw cgroup2 /proc/mounts || { echo NO_CGROUP2; exit 1; }
dockerd >/tmp/dockerd.log 2>&1 &
for i in $(seq 1 40); do docker info >/dev/null 2>&1 && break; sleep 1; done
docker info >/dev/null 2>&1 || { echo DOCKERD_DOWN; tail -20 /tmp/dockerd.log; exit 1; }
docker image inspect curlimages/curl:latest >/dev/null 2>&1 || { echo NO_IMAGE; exit 1; }

# ALLOWED host over HTTPS — container DNS resolves example.com via the gateway (pinning its IP),
# the proxy permits the pinned IP, and the TLS SNI matches the allow-list.
docker run --rm curlimages/curl:latest -sS --max-time 30 -o /dev/null \
    -w "HTTPS_ALLOWED %{http_code}\n" https://example.com || echo HTTPS_ALLOWED_FAILED

# ALLOWED host over HTTP — same DNS/IP path without TLS.
docker run --rm curlimages/curl:latest -sS --max-time 30 -o /dev/null \
    -w "HTTP_ALLOWED %{http_code}\n" http://example.com || echo HTTP_ALLOWED_FAILED

# NON-ALLOWED host — must be blocked identically to VM-local: the gateway REFUSEs the DNS query so
# the container never resolves it. Any 2xx here ("LEAK") is a container-egress allow-list bypass.
if docker run --rm curlimages/curl:latest -sS --max-time 30 -o /dev/null \
    -w "LEAK %{http_code}\n" https://example.org; then
    echo BLOCK_FAILED
else
    echo BLOCKED_OK
fi
"#;

/// Boot a real box whose runtime `network.allow` is exactly `["example.com"]`, with Docker and the
/// curl image pre-provisioned, and assert that container egress is policed identically to VM-local:
/// the allowed host is reachable over HTTP and HTTPS from inside a container, and a non-allowed host
/// is blocked. This is the security-critical acceptance for #116 — RED if container traffic could
/// bypass the gateway/DNS allow-list, GREEN once it is provably bound by it.
#[test]
#[ignore]
fn container_egress_is_bound_by_network_allowlist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dome_json = serde_json::json!({
        // Images are large and disk_size is create-only — size the box up exactly as the docs tell
        // users to. memory keeps dockerd + the build comfortable.
        "memory": 4096,
        "disk_size": 8192,
        // Networking must be ON for the allow-list to be enforced — with allow_net=false the box
        // gets no NIC at all and even VM-local DNS fails (the allow-list is silently ignored).
        "allow_net": true,
        // Provision (full egress, allow unset) bakes Docker + the test image into the layer so the
        // runtime boot below needs no registry and the allow-list can stay minimal.
        "provision": { "steps": [ PROVISION_STEP ] },
        // The security contract under test: container egress must obey exactly this list.
        "network": { "allow": ["example.com"] }
    });
    std::fs::write(
        dir.path().join("dome.json"),
        serde_json::to_string_pretty(&dome_json).unwrap(),
    )
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
        "the container-egress run should boot and exit cleanly; stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // A container reached the ALLOWED host over HTTPS — container-initiated DNS was answered and its
    // IP pinned by the gateway, the proxy permitted the pinned IP, and the TLS SNI was accepted.
    assert!(
        stdout.lines().any(|l| l == "HTTPS_ALLOWED 200"),
        "a container must reach the allowed host over HTTPS (DNS pin + IP allow + SNI); \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // ...and over plain HTTP.
    assert!(
        stdout.lines().any(|l| l == "HTTP_ALLOWED 200"),
        "a container must reach the allowed host over HTTP; stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // The non-allowed host was blocked — identical to VM-local behavior. No `LEAK` 2xx and no
    // `BLOCK_FAILED` marker may appear: either would be a container-egress allow-list bypass.
    assert!(
        stdout.lines().any(|l| l == "BLOCKED_OK"),
        "a container reaching a non-allowed host must be blocked; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("BLOCK_FAILED") && !stdout.lines().any(|l| l.starts_with("LEAK 2")),
        "container egress to a non-allowed host leaked past the allow-list (security regression); \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
