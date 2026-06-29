# Container runtimes inside dome boxes

You can install and run a container runtime (Docker, Podman) **inside** a dome box. The VM is
the encapsulation boundary, so the containers you start run *within* it and stay subject to
dome's network policy — exactly like VM-local processes. This works the same for an ephemeral
`dome run` and a persistent `dome sandbox`, and no matter how the runtime is installed
(interactive shell, `provision` block, or the configured `command`).

dome does **not** bake or auto-cache images. If you want images pre-pulled or pre-built, do it
yourself (e.g. in a `provision` step).

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

## Egress is policed identically to VM-local

Container traffic is bound by `network.allow` exactly like the VM's own traffic — a container
cannot reach a host the box itself is not allowed to reach. This is enforced at the host gateway
(container traffic is masqueraded onto the VM's interface and container DNS is answered by the
same gateway), so there is nothing to configure: it is on whenever networking is.

See [networking.md](networking.md) for the allow-list and [config.md](config.md#secrets) for
secrets.

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
