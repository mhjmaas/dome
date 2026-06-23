---
name: dome
description: Run commands in an isolated Linux microVM sandbox using the dome CLI. Use when the user asks to execute untrusted code, install packages safely, test in a clean environment, or needs Linux-specific tooling on macOS.
---

# Sandboxed Execution with Dome

Dome boots an ephemeral Linux microVM (Debian, ARM64) on macOS. Each `dome run` gets a fresh disk clone - all changes are discarded on exit. Use it whenever you need to run commands in isolation from the host.

## Core Workflow

The pattern is: **run in sandbox, mount to share files, checkpoint to persist state**.

```bash
# 1. Run a command in a fresh VM
dome run -- echo "hello from the sandbox"

# 2. Mount the project directory so the VM can access host files
dome run --mount ./src:/workspace -- ls /workspace

# 3. If the command needs network access (install packages, fetch data)
dome run --allow-net -- sh -c 'apt-get install -y curl && curl https://example.com'

# 4. If setup is expensive, save a checkpoint and reuse it
dome checkpoint create node-env --allow-net -- apt-get install -y nodejs npm
dome run --from node-env --mount .:/workspace -- node /workspace/app.js
```

## Command Chaining

Chain commands with `sh -c` when you need multiple steps:

```bash
dome run --allow-net -- sh -c 'apt-get install -y python3 python3-pip && python3 -c "print(1+1)"'

dome run --mount .:/workspace -- sh -c 'cd /workspace && ls -la && cat README.md'
```

## Essential Commands

### Run

```bash
dome run [flags] [-- command...]

# Interactive shell (default when no command given)
dome run

# Run a single command
dome run -- whoami

# With resources
dome run --cpus 4 --memory 4096 --disk-size 8192 -- make -j4

# With networking + port forwarding
dome run --allow-net -p 8080:80 -- nginx -g 'daemon off;'

# Multiple mounts
dome run --mount ./src:/src --mount ./data:/data -- ls /src /data

# From a checkpoint
dome run --from myenv -- npm test
```

### Checkpoints

```bash
# Create: boots VM, runs command, saves disk on exit
dome checkpoint create <name> [flags] [-- command...]

# Stack: create from an existing checkpoint
dome checkpoint create with-deps --from base-env --allow-net -- npm install

# List all checkpoints (shows actual disk usage)
dome checkpoint list

# Delete
dome checkpoint delete <name>
```

Checkpoint names must be unique - delete the old one before re-creating with the same name.

### Other Commands

```bash
# Download/update OS image
dome init
dome init --force    # re-download even if up to date

# Upgrade CLI + OS image
dome upgrade

# Clean up leftover data from crashed VMs
dome prune
```

## Common Patterns

### Dev Environment Setup

Create a checkpoint with all dependencies pre-installed, then use it for fast runs:

```bash
# One-time setup
dome checkpoint create python-dev --allow-net -- sh -c 'apt-get install -y python3 python3-pip && pip install pytest requests'

# Fast subsequent runs
dome run --from python-dev --mount .:/workspace -- sh -c 'cd /workspace && pytest'
```

### Testing Untrusted Code

Run untrusted scripts with no network access and no host filesystem access:

```bash
# Fully isolated — no --allow-net, no --mount
dome run -- sh -c 'echo "malicious script here" && rm -rf / 2>/dev/null; echo "host is safe"'
```

### Build and Test

Mount source, build inside the VM, results appear on host via the mount:

```bash
dome run --mount .:/workspace --cpus 4 --memory 4096 -- sh -c '
  cd /workspace
  apt-get install -y build-essential
  make -j4
  make test
'
```

### Port Forwarding for Web Servers

```bash
dome run --allow-net --from node-env -p 3000:3000 --mount .:/app -- sh -c '
  cd /app && node server.js
'
# Access at http://localhost:3000 on the host
```

### Stacking Checkpoints

Build environments incrementally:

```bash
dome checkpoint create base --allow-net -- apt-get install -y build-essential git curl
dome checkpoint create node --from base --allow-net -- apt-get install -y nodejs npm
dome checkpoint create project --from node --allow-net --mount .:/app -- sh -c 'cd /app && npm install'
# Now "project" has OS deps + Node + node_modules baked in
dome run --from project --mount .:/app -- sh -c 'cd /app && npm test'
```

## Project Config (dome.json)

Place `dome.json` in the project root to avoid repeating flags:

```json
{
  "cpus": 2,
  "memory": 2048,
  "disk_size": 4096,
  "allow_net": true,
  "ports": ["8080:80"],
  "mounts": ["./src:/workspace"],
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

One rule resolves every field: `--X` sets it, `--no-X` clears/disables it, omit to inherit — so flags win, then `dome.json`, then the default. For an ephemeral `dome run` this is re-resolved every invocation. A persistent `dome sandbox` resolves config **once at creation** and stores it; editing `dome.json` afterwards does not affect an existing sandbox — pass flags (they always win) or run `dome sandbox config --reload <name>` to re-apply it. When `secrets` are configured, the guest receives placeholder tokens and the proxy substitutes real values on HTTPS requests to allowed hosts. See [references/config.md](references/config.md) for all fields.

## Important Constraints

- **Networking is off by default.** You must pass `--allow-net` to install packages or make HTTP requests.
- **The guest is Debian Linux (aarch64).** Use `apt-get install` for packages.
- **Ephemeral by default.** Everything is discarded on exit unless you checkpoint.
- **Mounts are read-only by default.** Guest writes go to a tmpfs overlay and are discarded on exit. Use `:rw` suffix + `--allow-host-writes` to write to the host.
- **macOS only** (Apple Silicon). Uses Apple Virtualization.framework.
- **Default resources:** 2 CPUs, 2048 MB RAM, 4096 MB disk. Override with `--cpus`, `--memory`, `--disk-size`.

## Deep-Dive Documentation

- [references/checkpoints.md](references/checkpoints.md) — checkpoint lifecycle, stacking, disk usage
- [references/config.md](references/config.md) — dome.json fields and resolution order
- [references/networking.md](references/networking.md) — allow-net, port forwarding, proxy behavior
