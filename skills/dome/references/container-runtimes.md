# Container runtimes inside dome boxes

You can install and run a container runtime (Docker, Podman) **inside** a dome box. The VM is
the encapsulation boundary, so the containers you start run *within* it and stay subject to
dome's network policy — exactly like VM-local processes. This works the same for an ephemeral
`dome run` and a persistent `dome sandbox`, and no matter how the runtime is installed
(interactive shell, `provision` block, or the configured `command`).

dome does **not** bake or auto-cache images. If you want images pre-pulled or pre-built, do it
yourself (e.g. in a `provision` step).

The recommended default engine target is **`dockerd` + `docker compose`**. Running it rootful is
fine — the VM is already the isolation boundary, so there is no security benefit to rootless inside
the box. Podman (daemonless) works too; everything below applies to both.

## Sizing the box

Container images are large, and `disk_size` is **create-only** (it cannot be grown after the box
exists). Bump it at creation:

```json
{
  "memory": 4096,
  "disk_size": 8192,
  "allow_net": true
}
```

For an ephemeral run: `dome run --memory 4096 --disk-size 8192 --allow-net -- …`.

## Starting the daemon

The guest init is not systemd, so an installed `dockerd` does not auto-start — start it yourself:

```bash
apt-get update && apt-get install -y docker.io
dockerd >/tmp/dockerd.log 2>&1 &
docker run --rm hello-world
```

Podman is daemonless, so there is nothing to start.

After exiting and re-entering a sandbox you must start `dockerd` again — the daemon is a
per-session process, not persisted state. Your images and containers *are* persisted (see below);
only the running daemon needs restarting.

## The install path doesn't matter

Bringing a runtime in is the same `apt-get install -y docker.io` + `dockerd &` no matter how the
box is driven, and the kernel capability, egress policing, and CA trust are unconditional boot
properties — so a container behaves **identically** across all four entry points:

```bash
# 1. Interactive `dome run` shell
dome run --memory 4096 --disk-size 8192 --allow-net -- sh
#   then, inside: apt-get install -y docker.io && dockerd & … && docker run …

# 2. Interactive `dome sandbox shell` (persists — see below)
dome sandbox shell mybox          # same commands inside; runtime survives re-entry
```

**3. A `provision` block** installs (and optionally pre-pulls/builds) at *build* time. Provisioning
runs under full egress and is hermetic (no project mount), so it is the place to bake the runtime
and images **once** instead of on every boot — dome does not pre-bake images, so the pull is yours
to write:

```json
{
  "memory": 4096,
  "disk_size": 8192,
  "allow_net": true,
  "provision": {
    "steps": [
      "apt-get update && apt-get install -y docker.io",
      "dockerd >/tmp/d.log 2>&1 & for i in $(seq 1 40); do docker info >/dev/null 2>&1 && break; sleep 1; done; docker pull curlimages/curl:latest"
    ]
  }
}
```

**4. The configured `command`** starts the daemon and runs your workload non-interactively:

```json
{
  "command": ["/bin/sh", "-lc", "dockerd >/tmp/d.log 2>&1 & sleep 5; docker run --rm hello-world"]
}
```

Because policing and CA trust are applied at boot (not keyed off *how* the container is started), a
container launched interactively mid-shell is treated exactly like one started from `provision` or
`command`.

## Persistence in a sandbox

A `dome sandbox` persists its entire writable root filesystem across sessions via content-addressed
storage. The runtime binary (`/usr/bin/docker…`) and everything under `/var/lib/docker` — pulled
images, built images, container layers, volumes — are saved when the session's worker stops and are
present again, unchanged, on the next `dome sandbox shell`/`run`. So a runtime you install and
images you pull or build in one session do **not** need re-installing or re-pulling in the next.

What does **not** persist is ephemeral per-boot runtime state under `/run` (pidfiles, sockets): like
a normal Linux boot, `/run` is a fresh tmpfs each session. That is deliberate — it is what lets you
simply re-run `dockerd` after re-entering, rather than tripping over a stale `/var/run/docker.pid`
left by the previous session's daemon.

Egress policing and CA propagation are unconditional boot properties, re-applied on every cold boot,
so a container started in a later sandbox session is policed and trusts dome's CA *exactly* like one
in the first session. There is no ephemeral-vs-sandbox difference in behavior.

## Egress is policed identically to VM-local

Container traffic is bound by `network.allow` exactly like the VM's own traffic — a container
cannot reach a host the box itself is not allowed to reach. This is enforced at the host gateway
(container traffic is masqueraded onto the VM's interface and container DNS is answered by the
same gateway), so there is nothing to configure: it is on whenever networking is.

See [networking.md](networking.md) for the allow-list and [config.md](config.md#secrets) for
secrets.

### Allow the registry hosts you pull from

Because container egress obeys `network.allow` exactly like the VM's, **`docker pull` only works if
the registry's hosts are in the list.** If you restrict `network.allow`, add every host the pull
touches — the registry, its token/auth endpoint, and its blob CDN — or the pull's DNS is REFUSEd and
it cannot resolve. For **Docker Hub** that is:

```json
{
  "allow_net": true,
  "network": {
    "allow": [
      "registry-1.docker.io",
      "auth.docker.io",
      "*.docker.io",
      "*.docker.com",
      "*.cloudflarestorage.com"
    ]
  }
}
```

(The blobs are served from a Cloudflare/R2 CDN, hence the wildcards — a list with only
`registry-1.docker.io` resolves the manifest but stalls on the layer download.) Other registries
need their own hosts, e.g. `quay.io` + `*.quay.io`, or `ghcr.io` + `*.githubusercontent.com` +
`pkg-containers.githubusercontent.com`. A registry you do **not** list is blocked — there is no
container-egress bypass.

The simplest alternative is to pull during `provision`, which runs under full egress, so the runtime
boot can keep a tight `allow` list (or none) and never needs registry access at all.

## HTTPS and secret injection from containers

When you configure `secrets`, dome's proxy MITMs HTTPS to the secret's hosts to substitute the
real value, presenting certificates signed by a CA generated fresh each boot. dome makes that CA
**trusted inside containers automatically** — you do not copy anything in.

Concretely, every box installs a transparent `docker`/`podman` shim at boot. For `run` and
`create` it bind-mounts the VM's combined trust bundle (public roots **plus** the dome CA) into
the container and exports the common CA environment variables (`SSL_CERT_FILE`, `CURL_CA_BUNDLE`,
`REQUESTS_CA_BUNDLE`, `GIT_SSL_CAINFO`, `NODE_EXTRA_CA_CERTS`). So HTTPS and secret injection work
from a container exactly as they do VM-local:

```bash
# No -k needed: the container trusts dome's CA.
docker run --rm curlimages/curl https://api.example.com

# A placeholder passed into the container is substituted by the proxy on the way upstream.
docker run --rm -e API_TOKEN="$API_TOKEN" curlimages/curl \
    -H "Authorization: Bearer $API_TOKEN" https://api.example.com
```

The shim is a no-op when no MITM is active (no `secrets` configured), so default behavior is
unchanged.

### Limitations

The shim covers the common case (`docker run` / `docker create`, `podman run` / `podman create`)
and the standard CA paths. It does **not** reach:

- **`docker build` / BuildKit** — build steps run in their own containers without the run-time
  mount. If a build step makes HTTPS calls to a MITM'd host, add the CA in the Dockerfile (e.g.
  `COPY` the bundle and `update-ca-certificates`) or use build secrets.
- **`docker compose` and direct API/SDK callers** — Compose services and anything that talks to
  the daemon socket directly bypass the CLI shim. Mount the bundle yourself per service, e.g.
  `volumes: ["/etc/ssl/certs/ca-certificates.crt:/etc/ssl/certs/ca-certificates.crt:ro"]`.
- **Images that pin their own CA bundle** — anything that ignores the system trust store and the
  `SSL_CERT_*` env vars (e.g. Java's keystore, some distroless/static images, apps with a baked-in
  bundle) will not pick up the dome CA. Import it into that runtime's own trust store, or build the
  image with the CA included.
