# Project Config (dome.json)

Place `dome.json` in the project root (or pass `--config <path>`). All fields are optional.

## Fields

VM-shape fields define how the VM boots. For a persistent sandbox they are resolved **once at creation** and stored in the sidecar (the single source of truth for every later boot).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `cpus` | number | 2 | Number of CPU cores |
| `memory` | number | 2048 | Memory in MB |
| `disk_size` | number | 4096 | Disk size in MB — **create-only** (see below) |
| `allow_net` | boolean | false | Enable networking |
| `allow_host_writes` | boolean | false | Allow `:rw` mounts to write to host filesystem |
| `ports` | string[] | [] | Port forwards, `"HOST:GUEST"` format |
| `mounts` | string[] | [] | Directory mounts, `"HOST:GUEST[:ro\|:rw]"` format (default: ro) |
| `secrets` | object | {} | Secrets to inject via proxy (see below) |
| `network` | object | {} | Network access policy (see below) |

Session/project keys are *not* part of the VM shape and are never persisted to the sidecar — they are read live on each invocation:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `command` | string[] | ["/bin/sh"] | Default command to run (live; not persisted) |
| `sandbox` | string | cwd slug | Default sandbox name for `dome sandbox …` (live; not persisted) |

## Project root auto-mount

At **runtime** (`dome run`, `dome sandbox shell/run`), the project root — the directory containing the `dome.json` in use — is automatically mounted into the guest at the standard path **`/workspace`**, so a developer working inside the sandbox sees their project. It honors `allow_host_writes`: read-write when set, read-only otherwise (the same semantics as an explicit mount). If you already declare a mount targeting `/workspace`, your explicit mapping wins and the auto-mount is skipped (no double-mount). When there is no `dome.json`, there is no project root to mount.

**Build vs. runtime — an intentional asymmetry.** The `provision` build phase runs in a **hermetic, unmounted** VM: the project directory is never exposed to provisioning, so the cache key cannot depend on project contents and a build step cannot read or exfiltrate them. The project-root mount described above is **runtime-only**. (Directory auto-activation lands you at the matching subdirectory under `/workspace`, computed as `/workspace` + (host cwd − project root).)

## Resolution Order

Every field follows one rule. The highest **set** layer wins:

```
CLI flag > dome.json > default
```

- **`--X` sets** the value. A scalar/boolean sets it; a **list flag** (`--port`, `--mount`, `--secret`, `--allow-host`, `--expose-host`) **replaces** the lower layer — it is not additive.
- **`--no-X` clears or disables** the value: `--no-allow-net`, `--no-allow-host-writes`, `--no-port`, `--no-mount`, `--no-secret`, `--no-allow-host`, `--no-expose-host`. Passing both `--X` and `--no-X` in one command is an error.
- **Omit the flag to inherit** the lower layer (`dome.json`, then the default).
- **`dome.json` only fills fields you didn't set on the command line.** It never overrides a flag.

For example, `dome run --cpus 4` with `{"cpus": 2}` in dome.json uses 4 CPUs; `--no-allow-net` with `{"allow_net": true}` disables networking.

## Ephemeral runs vs. persistent sandboxes

- **`dome run` is ephemeral.** Config is resolved from `dome.json` + flags on every invocation, the VM boots a fresh disk clone, and everything is discarded on exit. Nothing is persisted, so there is no drift.
- **`dome sandbox …` is persistent.** A sandbox resolves its config **once at creation** and writes a versioned sidecar that is the single source of truth (sidecar-as-truth) for every later boot. After creation, **editing `dome.json` and restarting does not change an existing sandbox.** To change a persistent sandbox:
  - **Flags always win.** Passing a config flag to `dome sandbox run`/`shell`/`config` re-resolves and updates the sidecar (applied on the next cold boot; a running VM keeps its current config until stopped).
  - **`dome sandbox config --reload <name>`** re-applies the current `dome.json` (plus any flags) to the sandbox — the only supported way to pick up `dome.json` edits without recreating. `disk_size` stays pinned (create-only) even if `dome.json` carries a new value.

```bash
dome sandbox config myenv --no-allow-net    # disable a policy on an existing sandbox
dome sandbox config myenv --port 9090:80    # replace the port list
dome sandbox config myenv --no-port         # clear the port list
dome sandbox config --reload myenv          # re-apply edited dome.json
dome sandbox config myenv                   # print the full effective resolved config
```

## `disk_size` is create-only

`--disk-size` (and `dome.json`'s `disk_size`) is honored **only when a sandbox is created**. Passing `--disk-size` to an existing sandbox (`run`, `shell`, or `config`) is a hard error, and `--reload` ignores any `disk_size` in `dome.json`. To resize, recreate: `dome sandbox rm <name>` then `dome sandbox create <name> --disk-size <MB>`.

## Secrets

Secrets let the guest use API keys without exposing the real values. The guest receives a random placeholder token; the proxy substitutes the real value only on HTTPS requests to allowed hosts.

```json
{
  "allow_net": true,
  "secrets": {
    "API_KEY": {
      "from": "OPENAI_API_KEY",
      "hosts": ["api.openai.com"]
    }
  }
}
```

- `from`: host environment variable containing the real value
- `hosts`: domains where the proxy will substitute the placeholder with the real value

The guest sees `$API_KEY=dome_tok_...`. The real secret never enters the VM.

## Network Policy

Restrict which domains the guest can reach:

```json
{
  "allow_net": true,
  "network": {
    "allow": ["api.openai.com", "registry.npmjs.org", "*.github.com"]
  }
}
```

- Empty or absent `allow` list means all domains are allowed.
- Supports wildcards: `*.example.com` matches `api.example.com` but not `example.com`.
- DNS queries for blocked domains return REFUSED.

## Example

```json
{
  "cpus": 4,
  "memory": 4096,
  "disk_size": 8192,
  "allow_net": true,
  "ports": ["3000:3000", "8080:80"],
  "mounts": [".:/workspace"],
  "command": ["/bin/sh", "-c", "cd /workspace && sh"],
  "secrets": {
    "API_KEY": {
      "from": "OPENAI_API_KEY",
      "hosts": ["api.openai.com"]
    }
  },
  "network": {
    "allow": ["api.openai.com", "registry.npmjs.org"]
  }
}
```

With this config, `dome run` boots a VM that can only reach `api.openai.com` and `registry.npmjs.org`, with the OpenAI API key injected securely via the proxy.
