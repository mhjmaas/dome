//! Integration test for issue #117 (Slice 3 of PRD #100): dome's per-boot MITM CA must be
//! trusted **inside containers**, so HTTPS and secret injection work from a container identically
//! to VM-local — unconditionally, with no manual cert copying.
//!
//! WHY THIS IS NEEDED. dome's transparent proxy MITMs TLS only for hosts that carry a `secret`
//! (crates/dome-proxy/src/proxy.rs `handle_mitm`): it presents a leaf certificate signed by a CA
//! generated fresh each boot (crates/dome-proxy/src/tls.rs). The host injects that CA into the
//! VM trust store (`/etc/ssl/certs/ca-certificates.crt` + `/usr/local/share/ca-certificates/
//! dome-proxy.crt`) so VM-local tools trust it. A freshly pulled container image does **not** —
//! it ships its own CA bundle — so an HTTPS request from a container to a MITM'd host fails the
//! handshake and the injected secret never reaches upstream.
//!
//! THE MECHANISM UNDER TEST. dome-guest writes a transparent `docker`/`podman` shim at boot
//! (crates/dome-guest/src/main.rs) that, for `run`/`create`, bind-mounts the VM's combined trust
//! bundle (public roots + dome CA) into the container and exports the common CA env vars. It is a
//! no-op when no dome CA is present (no MITM active), so default behavior is unchanged.
//!
//! HOW IT PROVES THE CONTRACT. A `secret` is bound to `postman-echo.com`, which forces the proxy
//! to MITM that host AND makes the host injects the CA. From inside a container, with **no `-k`**:
//!   * `https://postman-echo.com/get` returns 200 — the container trusted the dome-signed cert
//!     (acceptance: CA available to containers; `docker run … https://host` succeeds, no `-k`).
//!   * the request carries the secret *placeholder* in an `Authorization` header; postman-echo
//!     echoes the headers it received back in the JSON body, and that body contains the **real**
//!     secret value — proving the proxy substituted the placeholder on a *container-originated*
//!     request (acceptance: a secret placeholder reaches upstream from inside a container).
//! Before the shim this test is RED: the container's curl aborts with a certificate error, so no
//! 200 and no echoed secret. After it, GREEN.
//!
//! Needs a codesigned binary and a base image from the container-capable `dome_defconfig` (#115).
//! `#[ignore]`d by default; run with:
//!   just test-vm container_ca_propagation
//!
//! Cost: heavy. A provision layer (full egress) installs `docker.io` and pre-pulls
//! `curlimages/curl` so the policed runtime boot needs no registry access and the allow-list can
//! be exactly `["postman-echo.com"]`. Images are large and `disk_size` is create-only, so the box
//! is sized up via dome.json. Depends on the public `postman-echo.com` echo service (as do the
//! MITM audit tests in sandbox_persist.rs).

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
/// cold-builds afresh and the global cache doesn't accumulate. Mirrors container_egress_policy.rs.
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

/// The real secret value. It is never written into dome.json — it lives only in the host env the
/// `secret`'s `from` reads — and must surface upstream (echoed by postman-echo) only because the
/// proxy substituted the placeholder on the container's request.
const REAL_SECRET: &str = "real-secret-value-117";

/// Provision step (full egress): install Docker, start dockerd, and pre-pull the curl image so the
/// runtime boot needs no registry access. Mirrors container_egress_policy.rs's provision.
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

/// Runtime script. dockerd restarts against the pre-pulled image store, then containers probe a
/// MITM'd host. `$ECHO_TOKEN` is the proxy-injected placeholder in the VM env; the outer shell
/// expands it into the container's curl args, so the placeholder leaves the *container* in the
/// HTTPS request and the proxy substitutes it en route to upstream.
const RUNTIME_SCRIPT: &str = r#"
set -e
dockerd >/tmp/dockerd.log 2>&1 &
for i in $(seq 1 40); do docker info >/dev/null 2>&1 && break; sleep 1; done
docker info >/dev/null 2>&1 || { echo DOCKERD_DOWN; tail -20 /tmp/dockerd.log; exit 1; }
docker image inspect curlimages/curl:latest >/dev/null 2>&1 || { echo NO_IMAGE; exit 1; }

# (1) Container HTTPS to the MITM'd host with NO -k: a 200 means the container trusted the
# dome-signed leaf cert, i.e. dome's CA reached the container trust path.
docker run --rm curlimages/curl:latest -sS --max-time 30 -o /dev/null \
    -w "MITM_HTTPS %{http_code}\n" https://postman-echo.com/get || echo MITM_HTTPS_FAILED

# (2) Same MITM'd host, carrying the secret placeholder in an Authorization header. postman-echo
# echoes the headers it received in the JSON body; if the proxy substituted the placeholder on
# this container-originated request, the body contains the REAL secret value.
BODY=$(docker run --rm curlimages/curl:latest -sS --max-time 30 \
    -H "Authorization: $ECHO_TOKEN" https://postman-echo.com/get 2>/dev/null || true)
case "$BODY" in
    *real-secret-value-117*) echo SECRET_REACHED_UPSTREAM ;;
    *) echo "SECRET_MISSING: $BODY" ;;
esac
"#;

/// Boot a real box where a secret binds `postman-echo.com` (forcing MITM + CA injection), with
/// Docker and the curl image pre-provisioned, and assert that dome's CA is trusted inside
/// containers: an HTTPS request to the MITM'd host succeeds with no `-k`, and a secret placeholder
/// sent from inside a container is substituted before reaching upstream. This is the acceptance
/// for #117 — RED if container HTTPS fails the dome-CA handshake, GREEN once the CA is propagated.
#[test]
#[ignore]
fn container_trusts_dome_ca_for_mitm_https_and_secret_injection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dome_json = serde_json::json!({
        // Images are large and disk_size is create-only — size the box up as the docs tell users.
        "memory": 4096,
        "disk_size": 8192,
        // Networking on so the proxy (and thus MITM + CA injection) is active.
        "allow_net": true,
        // Provision (full egress) bakes Docker + the test image into the layer so the runtime boot
        // needs no registry and the allow-list can stay minimal.
        "provision": { "steps": [ PROVISION_STEP ] },
        // Tight allow-list: only the MITM'd host is reachable at runtime.
        "network": { "allow": ["postman-echo.com"] },
        // The secret binds postman-echo.com: this is what makes the proxy MITM that host (present a
        // dome-CA-signed cert) and inject the CA into the VM trust store. `from` reads the real
        // value from the host env below — it never appears in dome.json.
        "secrets": { "ECHO_TOKEN": { "from": "ECHO_TOKEN", "hosts": ["postman-echo.com"] } }
    });
    std::fs::write(
        dir.path().join("dome.json"),
        serde_json::to_string_pretty(&dome_json).unwrap(),
    )
    .expect("write dome.json");

    let out = Command::new(dome_bin())
        .current_dir(dir.path())
        .env("ECHO_TOKEN", REAL_SECRET)
        .args(["run", "--", "sh", "-c", RUNTIME_SCRIPT])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    cleanup_published_layers();

    assert!(
        out.status.success(),
        "the container-CA run should boot and exit cleanly; stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // The container trusted dome's MITM CA — HTTPS to the MITM'd host returned 200 with no `-k`.
    assert!(
        stdout.lines().any(|l| l == "MITM_HTTPS 200"),
        "a container must trust dome's CA and reach the MITM'd host over HTTPS without -k; \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // The secret placeholder sent from inside the container was substituted before reaching
    // upstream: postman-echo echoed back the REAL value, not the placeholder.
    assert!(
        stdout.lines().any(|l| l == "SECRET_REACHED_UPSTREAM"),
        "a secret placeholder sent from inside a container must reach upstream as the real value; \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Defense in depth: the placeholder itself must never be what upstream saw (would mean no
    // substitution), and the handshake must not have failed.
    assert!(
        !stdout.contains("MITM_HTTPS_FAILED") && !stdout.contains("SECRET_MISSING"),
        "container HTTPS to the MITM'd host must not fail the dome-CA handshake; \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
