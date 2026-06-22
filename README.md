# dome

Local-first microVM sandbox for AI agents on macOS, with experimental Linux support.

Dome boots lightweight Linux VMs for AI agents. On macOS it uses Apple's Virtualization.framework. On Linux it uses a KVM backend that is now available as an experimental release build for ARM64 hosts. Every sandbox is ephemeral: the rootfs resets on every run, giving agents a disposable environment to execute code, install packages, and run tools without touching your host.

> [!WARNING]
> **Experimental Linux support.** Linux builds are available for testing, but they are not ready for production use yet. Expect rough edges, missing polish, and compatibility gaps.

## Requirements

- macOS 14 (Sonoma) or later on Apple Silicon
- Linux ARM64 with KVM access (`/dev/kvm`) for experimental testing only

## Install

```sh
brew tap mhjmaas/tap && brew install dome
```

Or via the install script:

```sh
curl -fsSL https://raw.githubusercontent.com/mhjmaas/dome/main/install.sh | sh
```

The install script supports macOS on Apple Silicon and experimental Linux ARM64. Linux users can also download the `linux-aarch64` release tarball manually from GitHub Releases if they prefer.

> [!NOTE]
> Homebrew remains macOS-only. Linux installs via the script are still experimental and not ready for production use.

## Usage

```sh
# Interactive shell
dome run

# Run a command
dome run -- echo hello

# With network access
dome run --allow-net

# Restrict to specific hosts
dome run --allow-net --allow-host api.openai.com --allow-host registry.npmjs.org

# Custom resources
dome run --cpus 4 --memory 4096 --disk-size 8192 -- make -j4
```

### Directory mounts

Share host directories into the VM using VirtioFS. By default the host directory is read-only; guest writes go to a tmpfs overlay layer (discarded when the VM exits). Append `:rw` to make the mount read-write — guest writes go directly to the host filesystem.

```sh
# Mount a directory (guest can read, writes go to overlay — host is untouched)
dome run --mount ./src:/workspace -- touch /workspace/test.txt
ls ./src/test.txt   # not found — write stayed in the overlay

# Read-write mount (guest writes land on host, requires --allow-host-writes)
dome run --allow-host-writes --mount ./src:/workspace:rw -- touch /workspace/test.txt
ls ./src/test.txt   # found — write went to host

# Multiple mounts
dome run --mount ./src:/workspace --mount ./data:/data -- sh
```

Mounts can also be set in `dome.json` (see [Config file](#config-file)).

> [!NOTE]
> Directory mounts require checkpoints created on v0.1.11+. Existing checkpoints work normally for all other features. Run `dome upgrade` to get the latest version.

### Port forwarding

Forward host ports to guest ports over vsock. Works without `--allow-net` — the guest needs no network device.

```sh
# Install python3 into a checkpoint, then serve with port forwarding
dome checkpoint create py --allow-net -- apt-get install -y python3
dome run --from py -p 8080:8000 -- python3 -m http.server 8000

# From the host (in another terminal)
curl http://127.0.0.1:8080/

# Multiple ports
dome run -p 8080:80 -p 8443:443 -- nginx
```

Port forwards can also be set in `dome.json` (see [Config file](#config-file)).

### Checkpoints

Checkpoints save the disk state so you can reuse an environment across runs.

```sh
# Set up an environment and save it
dome checkpoint create myenv --allow-net -- sh -c 'apt-get install -y python3 gcc'

# Run from a checkpoint (ephemeral -- changes are discarded)
dome run --from myenv -- python3 script.py

# Branch from an existing checkpoint
dome checkpoint create myenv2 --from myenv --allow-net -- sh -c 'pip install numpy'

# List and delete
dome checkpoint list
dome checkpoint delete myenv
```

### Secrets

Secrets keep API keys on the host. The guest receives a random placeholder token; the proxy substitutes the real value only on HTTPS requests to the specified hosts. The real secret never enters the VM.

```sh
# Inject a secret via CLI
dome run --allow-net --secret API_KEY=OPENAI_API_KEY@api.openai.com -- curl https://api.openai.com/v1/models

# Multiple secrets
dome run --allow-net \
  --secret API_KEY=OPENAI_API_KEY@api.openai.com \
  --secret GH_TOKEN=GITHUB_TOKEN@api.github.com \
  -- sh
```

Format: `NAME=ENV_VAR@host1,host2` — `NAME` is the env var the guest sees, `ENV_VAR` is the host env var with the real value, and hosts are where the proxy substitutes it.

Secrets can also be set in `dome.json` (see [Config file](#config-file)).

### Config file

Dome loads `dome.json` from the current directory (or `--config PATH`). All fields are optional; CLI flags take precedence.

```json
{
  "cpus": 4,
  "memory": 4096,
  "disk_size": 8192,
  "allow_net": true,
  "ports": ["8080:80"],
  "mounts": ["./src:/workspace", "./data:/data"],
  "command": ["python", "script.py"],
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

The `network.allow` list restricts which hosts the guest can reach. Omit it to allow all hosts.

## SDK

Use dome programmatically from TypeScript with the [`@superhq/dome`](https://www.npmjs.com/package/@superhq/dome) package.

```sh
bun add @superhq/dome
```

```ts
import { Sandbox } from "@superhq/dome";

const sb = await Sandbox.start({ from: "python-env" });

const result = await sb.exec("python3 -c 'print(1+1)'");
console.log(result.stdout); // "2\n"

await sb.checkpoint("after-run"); // saves disk state and stops the VM
```

See the [SDK README](packages/sdk/README.md) for full API docs.

## Agent Skill

Dome ships as an [agent skill](https://agentskills.io) so AI agents (Claude Code, Cursor, Copilot, etc.) can use it automatically.

```sh
# Install via Vercel's skills CLI
npx skills add mhjmaas/dome

# Or manually copy into your project
cp -r skills/dome .claude/skills/dome
```

Once installed, agents will use `dome run` whenever they need sandboxed execution.

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for release notes and breaking changes.

## Support

<a href="https://buymeacoffee.com/harshdoesdev" target="_blank"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-yellow.png" alt="Buy Me A Coffee" height="40"></a>

## Bugs

File issues at [github.com/mhjmaas/dome/issues](https://github.com/mhjmaas/dome/issues).

## Star History

<a href="https://www.star-history.com/?repos=mhjmaas%2Fdome&type=date&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/chart?repos=mhjmaas/dome&type=date&theme=dark&legend=top-left" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/chart?repos=mhjmaas/dome&type=date&legend=top-left" />
   <img alt="Star History Chart" src="https://api.star-history.com/chart?repos=mhjmaas/dome&type=date&legend=top-left" />
 </picture>
</a>
