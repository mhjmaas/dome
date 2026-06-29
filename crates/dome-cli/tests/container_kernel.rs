//! Integration test for issue #115 (Slice 1 of PRD #100): the dome base kernel must be
//! capable of hosting a container runtime's bridge networking and cgroup/BPF machinery.
//!
//! This boots a REAL ephemeral VM and exercises — as root, inside the guest — the kernel
//! primitives a Docker/Podman runtime relies on:
//!   * `CONFIG_VETH`         — a veth pair (critical: no container bridge networking without it)
//!   * `CONFIG_BRIDGE`       — a bridge with a veth enslaved (the docker0 datapath)
//!   * `CONFIG_NET_NS`       — moving the peer into a fresh netns (the container's side)
//!   * `CONFIG_MACVLAN`      — a macvlan device (compose multi-network)
//!   * `CONFIG_VXLAN`        — a vxlan device (compose overlay multi-network)
//!   * `CONFIG_BRIDGE_NETFILTER` — the br_netfilter sysctls docker toggles for bridge iptables
//!   * `CONFIG_BPF_SYSCALL`  — a mountable bpffs (the bpf() syscall machinery)
//!   * `CONFIG_BLK_CGROUP`   — the `io` controller in the cgroup v2 hierarchy
//!
//! It needs a codesigned binary and a base image built from the updated `dome_defconfig`.
//! `#[ignore]`d by default; run with:
//!   just test-vm container_kernel
//!
//! This slice has NO allow-list active (pass-through): a container only needs to *run* here,
//! not be policed — egress policing and CA propagation are follow-up slices (#116, #117).

use std::process::Command;

fn dome_bin() -> String {
    std::env::var("DOME_BIN")
        .expect("DOME_BIN not set — point it at a codesigned dome binary (e.g. `just build`)")
}

/// The guest script probes each capability independently and prints one `CAP <name> <ok|FAIL>`
/// line per primitive, so a single VM boot reports every flag and a failure pinpoints exactly
/// which one is missing. Each probe is guarded so one missing capability never aborts the rest.
const PROBE: &str = r#"
log() { echo "CAP $1 $2"; }

# CONFIG_VETH (critical) — a veth pair is the container<->host link.
if ip link add dome_v0 type veth peer name dome_v1 2>/dev/null; then
    ip link set dome_v0 up 2>/dev/null; ip link set dome_v1 up 2>/dev/null
    log VETH ok
else
    log VETH FAIL
fi

# CONFIG_BRIDGE — enslave one veth end to a bridge (the docker0 pattern).
if ip link add dome_br0 type bridge 2>/dev/null && ip link set dome_v0 master dome_br0 2>/dev/null; then
    log BRIDGE ok
else
    log BRIDGE FAIL
fi

# CONFIG_MACVLAN — compose multi-network. Parented on the free veth end (dome_v1) rather than
# a physical NIC, so this probes the kernel flag itself, not whether an eth0 happens to exist
# (a plain `dome run` with no allow-list attaches no NIC). dome_v0 is now a bridge port and a
# macvlan cannot stack on an enslaved device, so dome_v1 is the valid parent here.
if ip link add dome_mv0 link dome_v1 type macvlan mode bridge 2>/dev/null; then log MACVLAN ok; else log MACVLAN FAIL; fi

# CONFIG_VXLAN — compose overlay multi-network. Likewise parented on the free veth end.
if ip link add dome_vx0 type vxlan id 42 dev dome_v1 dstport 4789 2>/dev/null; then log VXLAN ok; else log VXLAN FAIL; fi

# CONFIG_NET_NS — push the peer into a fresh netns: the container's network side.
if ip netns add dome_ns0 2>/dev/null && ip link set dome_v1 netns dome_ns0 2>/dev/null; then
    log NETNS ok
else
    log NETNS FAIL
fi

# CONFIG_BRIDGE_NETFILTER — br_netfilter sysctls docker toggles for bridge iptables.
if [ -e /proc/sys/net/bridge/bridge-nf-call-iptables ]; then log BRIDGE_NF ok; else log BRIDGE_NF FAIL; fi

# CONFIG_BPF_SYSCALL — a mountable bpffs implies the bpf() syscall is wired up.
mkdir -p /tmp/dome_bpf 2>/dev/null
if mount -t bpf bpf /tmp/dome_bpf 2>/dev/null; then log BPF ok; umount /tmp/dome_bpf 2>/dev/null; else log BPF FAIL; fi

# CONFIG_BLK_CGROUP — the io controller in the cgroup v2 hierarchy (block-io accounting).
mkdir -p /sys/fs/cgroup 2>/dev/null
mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null || true
if grep -qw io /sys/fs/cgroup/cgroup.controllers 2>/dev/null; then log BLK_CGROUP ok; else log BLK_CGROUP FAIL; fi
"#;

/// Boot an ephemeral VM with no `network.allow` and assert every container-runtime kernel
/// primitive is present. The critical one is `CONFIG_VETH`: without it `ip link add … type
/// veth` fails and no container bridge networking is possible — so this test is RED on a kernel
/// built before the defconfig flags were added, and GREEN once the base image is rebuilt.
#[test]
#[ignore]
fn kernel_exposes_container_runtime_primitives() {
    let out = Command::new(dome_bin())
        .args(["run", "--", "sh", "-c", PROBE])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the probe run should boot and exit cleanly; stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Every capability the slice unlocks must report `ok`. VETH is listed first because it is
    // the load-bearing one — the rest are meaningless if the veth datapath is unavailable.
    for cap in [
        "VETH",
        "BRIDGE",
        "NETNS",
        "MACVLAN",
        "VXLAN",
        "BRIDGE_NF",
        "BPF",
        "BLK_CGROUP",
    ] {
        assert!(
            stdout.lines().any(|l| l == format!("CAP {cap} ok")),
            "kernel must expose the `{cap}` container-runtime primitive; \
             a `CAP {cap} FAIL` (or missing line) means the defconfig flag is absent. \
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

/// The end-to-end acceptance demo: a real Docker runtime, installed inside a box at runtime,
/// just works — a container runs and reaches the network through the veth bridge + MASQUERADE.
///
/// This is the heaviest test in the suite: it boots a box with networking on (no `network.allow`
/// = pass-through), `apt install`s `docker.io`, starts `dockerd`, and runs two containers,
/// pulling images from Docker Hub. It therefore needs egress and a few minutes. Container images
/// are large and `disk_size` is create-only, so the box is sized up at boot (`--disk-size`,
/// `--memory`) exactly as the docs will tell users to do.
///
/// It proves the slice end to end:
///   * `dockerd` starts unaided — the guest mounts cgroup v2 at boot (no systemd here), and the
///     kernel carries the nftables/xt netfilter targets docker's bridge NAT needs.
///   * `docker run hello-world` succeeds (the container runs).
///   * a container doing outbound HTTP gets a 200 — veth bridge + docker0 MASQUERADE function.
#[test]
#[ignore]
fn docker_runtime_runs_and_egresses_through_veth_bridge() {
    // Each network-bound step is bounded by `timeout` so a dead mirror/registry fails the test
    // rather than hanging it. dockerd is polled until its socket answers (or we give up and dump
    // its log). The two `docker run`s exercise (a) a container that just runs and (b) a container
    // that egresses — the heart of acceptance criteria #3 and #4.
    let script = r#"
set -e
export DEBIAN_FRONTEND=noninteractive
grep -qw cgroup2 /proc/mounts || { echo "NO_CGROUP2"; exit 1; }
timeout 120 apt-get update -qq >/dev/null 2>&1
timeout 240 apt-get install -y -qq docker.io >/dev/null 2>&1
dockerd >/tmp/dockerd.log 2>&1 &
for i in $(seq 1 40); do docker info >/dev/null 2>&1 && break; sleep 1; done
if ! docker info >/dev/null 2>&1; then echo "DOCKERD_DOWN"; tail -15 /tmp/dockerd.log; exit 1; fi
timeout 120 docker run --rm hello-world 2>/dev/null | grep -qi "hello from docker" && echo "HELLO_OK"
timeout 120 docker run --rm curlimages/curl:latest -sS --max-time 30 -o /dev/null \
    -w "CURL_HTTP %{http_code}\n" http://example.com
"#;

    let out = Command::new(dome_bin())
        .args([
            "run",
            "--allow-net",
            "--memory",
            "4096",
            "--disk-size",
            "8192",
            "--",
            "sh",
            "-c",
            script,
        ])
        .output()
        .expect("failed to spawn dome — is DOME_BIN correct?");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the docker E2E run should exit cleanly; stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // dockerd started on its own — proving the guest's boot-time cgroup v2 mount and the kernel's
    // bridge-NAT netfilter flags are in place (no `NO_CGROUP2` / `DOCKERD_DOWN` bailout fired).
    assert!(
        stdout.lines().any(|l| l == "HELLO_OK"),
        "`docker run hello-world` must succeed (container runs); stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // A container's outbound request reached the internet through the veth bridge + docker0
    // MASQUERADE and came back 200.
    assert!(
        stdout.lines().any(|l| l == "CURL_HTTP 200"),
        "a container's outbound HTTP must succeed via veth bridge + MASQUERADE; \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
