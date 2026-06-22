# Networking

Networking is **off by default**. Pass `--allow-net` to enable it.

## How It Works

All guest network traffic goes through a userspace proxy on the host (no NAT, no direct internet access). The proxy:

- Resolves DNS on the host and relays responses
- Tunnels TCP connections (HTTP and HTTPS) to the real internet
- Optionally performs MITM on HTTPS to inject secrets (only when `secrets` are configured)
- Enforces domain allowlists when `network.allow` is set

ICMP (ping) is not supported — only TCP traffic is proxied.

## Enabling Network Access

```bash
dome run --allow-net -- sh -c 'apt-get install -y curl && curl https://example.com'
```

Or set it in `dome.json`:

```json
{
  "allow_net": true
}
```

## Domain Allowlist

Restrict which domains the guest can reach:

```json
{
  "allow_net": true,
  "network": {
    "allow": ["api.openai.com", "registry.npmjs.org", "*.github.com"]
  }
}
```

DNS queries for blocked domains return REFUSED. Omit `network.allow` to allow all domains.

## Secret Injection

See [config.md](config.md#secrets) for details on injecting API keys via the proxy.

## Port Forwarding

Forward host ports to guest ports with `-p HOST:GUEST`. Port forwarding uses vsock and works **without** `--allow-net`:

```bash
# Forward host 8080 to guest 80
dome run -p 8080:80 -- python3 -m http.server 80

# With networking too
dome run --allow-net -p 3000:3000 -p 5432:5432 -- sh -c 'start-services.sh'
```

Access forwarded services at `localhost:HOST_PORT` on the host machine.

Port forwards can also be set in `dome.json`:

```json
{
  "ports": ["8080:80", "3000:3000"]
}
```

CLI `-p` flags are merged with config ports (not replaced).

## Without Networking

When `--allow-net` is not set, the VM has no network device. DNS resolution, HTTP requests, and package installs will fail. This is the intended default for maximum isolation.

To install packages, either:
1. Use `--allow-net` during the run
2. Create a checkpoint with packages pre-installed, then run without networking:

```bash
dome checkpoint create with-tools --allow-net -- apt-get install -y curl jq python3
dome run --from with-tools -- python3 script.py   # no --allow-net needed
```
