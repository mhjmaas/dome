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

## Build from source

By default the CLI downloads a prebuilt OS image (kernel + rootfs + initramfs) from GitHub Releases on first run. Developers who want full control can build the entire image locally instead — useful for customizing the kernel ([`kernel/dome_defconfig`](kernel/dome_defconfig)) or rootfs.

Requirements: a Rust toolchain with the `aarch64-unknown-linux-musl` target, [`just`](https://github.com/casey/just), and **Docker** (macOS uses it to compile the kernel and format the ext4 rootfs).

```sh
# Build the OS image locally (kernel + rootfs + initramfs) and stamp the
# VERSION file so the CLI uses your local build instead of downloading
just build-image

# Build everything from scratch: local OS image + CLI binary (codesigned)
just setup
```

The image is written to `~/.local/share/dome/` (`Image`, `rootfs.ext4`, `initramfs.cpio.gz`, `VERSION`). Once present and matching the CLI version, `dome run` uses it and skips the download. The kernel step is skipped if `Image` already exists; to rebuild just the kernel — optionally pinning a version — run:

```sh
KERNEL_VERSION=6.12.17 ./scripts/build-kernel.sh
```

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

### Persistent sandboxes

Where `dome run` is ephemeral — a fresh disk every time, everything discarded on exit — a **sandbox** is a long-lived named VM whose disk persists across runs. Use it as a stable environment you return to.

```sh
# Open a shell (the sandbox is created lazily on first use)
dome sandbox shell myenv

# Run a command in it
dome sandbox run myenv -- python3 script.py

# Seed a new sandbox from a checkpoint or another sandbox
dome sandbox create myenv --from python-env

# List sandboxes (size, pinned base version, running/idle status)
dome sandbox ls

# Stop a running sandbox (flush + save, then shut the VM down)
dome sandbox stop myenv

# Force a durable save without stopping
dome sandbox save myenv

# Remove a sandbox
dome sandbox rm myenv
```

The name is optional — it defaults to the `sandbox` field in `dome.json`, otherwise a slug of the current directory. A sandbox resolves its config **once, at creation**, and stores it in a sidecar (see [Config file](#config-file)). Multiple terminals can attach to the same running sandbox at once; a fully idle sandbox shuts its VM down on its own and reboots on next use.

### Directory auto-activation

Install a shell hook and dome will drop you straight into a project's sandbox whenever you `cd` into a directory that contains a `dome.json` — no need to type `dome sandbox shell`. When you exit the guest, you're back on the host; you stay there until you leave the project and `cd` back in.

```sh
# Install the hook into your shell rc ($SHELL is auto-detected; zsh, bash, fish)
dome hook --install

# ...or print it and wire it up yourself
eval "$(dome hook zsh)"      # add to ~/.zshrc
eval "$(dome hook bash)"     # add to ~/.bashrc
dome hook fish | source      # add to ~/.config/fish/config.fish
```

**Activation requires explicit trust.** A directory does nothing until you allow it — the hook just prints a one-line hint. Trust is recorded for that exact directory and the current `dome.json` content:

```sh
# From inside the project, grant trust (records the dir + a hash of its dome.json)
dome allow
```

Editing `dome.json` **re-locks** the project: the next time you enter it the hook reminds you it changed, and you re-run `dome allow` to review the diff and re-grant. This keeps a pulled or edited config from silently changing what boots.

The hook only acts on an interactive terminal you're driving. It stays out of the way when any of these hold: you're already inside a dome guest (`$DOME_SANDBOX`), `$CI` is set, or you export `DOME_NO_AUTO=1`. To disable auto-activation for one project while keeping it trusted, set `"activate": "off"` in its `dome.json` (`dome sandbox shell` still works manually).

The sandbox a project auto-activates into is the `sandbox` field from its `dome.json`, otherwise a `<slug>-<pathhash>` name — the path hash means two different directories that share a basename never collide on the same VM. If you `cd` into a subdirectory of the project, you land at the matching path under `/workspace`.

### Provisioning

The base image is intentionally bare. Declare your project's **toolchain** in a `provision` block in `dome.json` and dome installs it once, snapshots the result as a hidden cache layer, and seeds every later sandbox or `dome run` from that layer — so the cost is paid exactly once.

```json
{
  "provision": {
    "steps": [
      "apt-get update && apt-get install -y nodejs",
      "curl -fsSL https://get.pnpm.io/install.sh | sh -"
    ],
    "allow": ["deb.debian.org", "get.pnpm.io"],
    "secrets": {
      "NPM_TOKEN": { "from": "NPM_TOKEN", "hosts": ["registry.corp.internal"] }
    }
  }
}
```

- **`steps`** — ordered shell commands run as root inside a build VM, sequentially, stop-on-first-failure. Toolchain/prerequisites only (node, gcc, python3). Project-dependency installs (`pnpm install`, `pip install -r …`) belong in the live sandbox, where its persistence captures them — not here.
- **`allow`** — the *provision-time* network allow-list, separate from runtime `network.allow`. Empty/unset = all hosts allowed (the build has network by default so `apt-get`/`curl` just work).
- **`secrets`** — same shape as runtime `secrets`; injected through the egress proxy so the real value never enters the build VM. A secret's `hosts` are auto-added to the provision allow-list.

The first creation on a given spec runs a **cold build** with a live banner and streamed step output:

```sh
dome sandbox shell                  # no cached layer yet
# dome: ── provisioning toolchain (cold build) ──
# dome:   → apt-get update && apt-get install -y nodejs
# dome:   → curl -fsSL https://get.pnpm.io/install.sh | sh -

dome sandbox shell                  # later, same spec — instant (reused layer)
```

The layer is keyed by a hash of the spec (steps + `allow` + secret mappings + the base it composes on). Edit the spec and the next creation rebuilds automatically; the cache never serves a stale toolchain. A failed step caches nothing — it surfaces the failing command, exit code, and captured output, and preserves the half-provisioned disk so you can shell in without re-running steps. During the build the project directory is **not** mounted (nothing to exfiltrate over the open-network build window); at runtime it auto-mounts at `/workspace`.

```sh
# Force a fresh build without editing the spec (also on sandbox create/run)
dome sandbox shell --rebuild

# Inspect cached layers (hidden from `checkpoint list`): delta size, base, staleness, age
dome provision list

# Shell into the preserved disk from the most recent failed build to debug it
dome provision debug [hash]

# Clear the whole provision cache (layers rebuild on next use)
dome prune --provision
```

Provisioning composes on top of a `--from` seed, so you can layer a toolchain onto a pre-baked checkpoint or sandbox. Stale-base layers (built against an older OS version) are reclaimed by `dome upgrade` and `dome prune`.

### Daemon

A background control plane (`domed`) supervises running sandboxes and their VMs. It starts automatically on first use and shuts itself down once idle, so you normally never invoke it directly. Manage it explicitly when you need to:

```sh
dome daemon status   # report pid, uptime, worker count, socket path
dome daemon start    # pre-warm the control plane (optional)
dome daemon stop     # stop the daemon; running sandboxes are left untouched
```

Use `dome prune` to reclaim disk from removed sandboxes and clear leftover data from crashed VMs.

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

Dome loads `dome.json` from the current directory (or `--config PATH`). All fields are optional.

```json
{
  "sandbox": "myenv",
  "cpus": 4,
  "memory": 4096,
  "disk_size": 8192,
  "allow_net": true,
  "activate": "shell",
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
  },
  "provision": {
    "steps": ["apt-get update && apt-get install -y nodejs"],
    "allow": ["deb.debian.org"]
  }
}
```

The `network.allow` list restricts which hosts the guest can reach. Omit it to allow all hosts. The `provision` block declares a cached toolchain layer built once per spec (see [Provisioning](#provisioning)). The `sandbox` field pins the persistent sandbox name (otherwise a slug of the directory is used), and `activate` controls [directory auto-activation](#directory-auto-activation) — `"shell"` (the default) drops you into the sandbox on `cd`, `"off"` disables it.

#### One resolution rule

Every config field follows the same rule:

- **`--X` sets** the value (a scalar/boolean sets it; a list flag like `--port` **replaces** the lower layer — it is not additive).
- **`--no-X` clears or disables** it (`--no-allow-net`, `--no-allow-host-writes`, `--no-port`, `--no-mount`, `--no-secret`, `--no-allow-host`, `--no-expose-host`).
- **Omit the flag to inherit** the lower layer.
- **`dome.json` only fills the fields you didn't set on the command line** — it never overrides a flag, and for a persistent sandbox that fill-in happens **once, at creation**.

So the precedence is always **flag > `dome.json` > built-in default**, with the highest *set* layer winning.

#### Ephemeral runs vs. persistent sandboxes

- **`dome run` is ephemeral.** It resolves config from `dome.json` + flags on every invocation, boots a fresh disk clone, and discards everything on exit. There is no persisted config to drift from.
- **`dome sandbox …` is persistent.** A sandbox resolves its config **once, at creation**, and stores it in a sidecar that is the single source of truth for every subsequent boot. After creation, **editing `dome.json` and restarting does *not* change an existing sandbox** — the sidecar is authoritative. To change a persistent sandbox's config:
  - Pass flags to `dome sandbox run`/`shell`/`config` — **flags always win** and update the sidecar (applied on the next cold boot; a running VM keeps its current config until stopped).
  - Run `dome sandbox config --reload <name>` to re-apply the current `dome.json` (plus any flags) to the sandbox. This is the only supported way to pick up `dome.json` edits without recreating.

```sh
# Disable a previously-enabled policy on an existing sandbox
dome sandbox config myenv --no-allow-net

# Replace the port list (not additive); clear it entirely
dome sandbox config myenv --port 9090:80
dome sandbox config myenv --no-port

# Re-apply an edited dome.json to an existing sandbox
dome sandbox config --reload myenv

# View the full effective resolved config (resources, secrets by name, allow-list, exposed ports)
dome sandbox config myenv
```

#### `disk_size` is create-only

`--disk-size` (and the `dome.json` `disk_size` field) is honored **only when a sandbox is created**. Passing `--disk-size` to an existing sandbox — via `run`, `shell`, or `config` — is a hard error, and `--reload` ignores any `disk_size` in `dome.json` (the disk stays pinned). To change a sandbox's disk size, recreate it: `dome sandbox rm <name>` then `dome sandbox create <name> --disk-size <MB>`.

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

## Acknowledgements

Dome started as a fork of [shuru](https://github.com/superhq-ai/shuru) and has been substantially built upon since.

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
