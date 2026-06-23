# Changelog

## 0.6.3

### Sandbox config: sidecar-as-truth with one resolution rule

Persistent sandbox config is reworked around a single, predictable model. Every
field — resources, booleans, and lists — now follows **one rule**: `--X` sets it,
`--no-X` clears or disables it, and omitting the flag inherits the lower layer, so
precedence is always **flag > `dome.json` > default** (the highest *set* layer wins).
List flags (`--port`, `--mount`, `--secret`, `--allow-host`, `--expose-host`)
**replace** the lower layer rather than appending.

A persistent sandbox resolves its config **once, at creation**, and stores it in a
versioned sidecar that is the single source of truth for every later boot
(sidecar-as-truth). Ephemeral `dome run` is unchanged — it re-resolves
`dome.json` + flags on every invocation.

- **New tri-state flags** to turn a setting off without recreating the sandbox:
  `--no-allow-net`, `--no-allow-host-writes`, `--no-port`, `--no-mount`,
  `--no-secret`, `--no-allow-host`, `--no-expose-host`. Passing a flag together
  with its negation is rejected.
- **Flags always win on an existing sandbox.** A config flag passed to
  `dome sandbox run`/`shell`/`config` now updates the sidecar (applied on the next
  cold boot; a running VM keeps its current config until stopped) instead of being
  warned about and ignored.
- **`dome sandbox config --reload <name>`** re-applies the current `dome.json`
  (plus any flags) to an existing sandbox — the supported way to pick up
  `dome.json` edits without recreating.
- **`--disk-size` is now create-only.** Passing it to an existing sandbox
  (`run`, `shell`, or `config`) is a hard error, and `--reload` ignores any
  `disk_size` in `dome.json`; the disk stays pinned. Recreate the sandbox to
  resize it.
- **`create --from` clones disk only.** A seeded sandbox takes its config from
  `dome.json` + flags (never the seed's config), and `disk_size` follows the
  cloned disk so the sidecar stays truthful.
- `dome sandbox config` (no flags) prints the full effective resolved config —
  resources, secrets by name, the unified network allow-list, and exposed host
  ports.

#### Behavior change

**Editing `dome.json` and restarting no longer auto-applies to an existing
sandbox.** Previously every cold boot re-read `dome.json`; now the sidecar is
authoritative. To apply `dome.json` edits to an existing sandbox, run
`dome sandbox config --reload <name>` (or pass the specific flags, which always
win), or recreate the sandbox. Ephemeral `dome run` is unaffected.

#### Migration

Legacy (unversioned) sidecars heal transparently on first boot: they are
re-resolved against the current `dome.json` and written back versioned. The one
exception is a **list** field set in *both* a CLI flag and `dome.json` on a
pre-0.6.3 sandbox — under the new replace-on-set rule the CLI value wins and the
`dome.json` entries are dropped on first boot (previously the two were merged).
Re-add them with `dome sandbox config --port …` (etc.) or
`dome sandbox config --reload`. Scalars and booleans are unaffected.

## 0.6.1

### Linux backend: correct wall clock and working directory mounts

Two gaps in the experimental Linux KVM backend are closed. Both were verified
live on an AWS `a1.metal` ARM64 instance.

### Linux backend (`dome-linux` 0.2.0)

- **PL031 RTC.** Exposes host wall clock to the guest via a minimal PL031
  MMIO device + FDT node. Fixes guests booting with clock pinned at
  1970-01-01, which previously broke every TLS handshake inside the sandbox
  ("certificate not yet valid"). Kernel already had `CONFIG_RTC_DRV_PL031`
  and `CONFIG_RTC_HCTOSYS`, so the system clock is seeded from `/dev/rtc0`
  at boot.
- **In-process virtio-fs passthrough.** `--mount` now works on Linux. The
  VMM speaks the FUSE wire protocol directly over the virtio-fs queue and
  forwards ops to the host filesystem via libc/std::fs. No external
  `virtiofsd` daemon or extra crate required. Covers INIT, LOOKUP, FORGET
  (single + batch), GETATTR, SETATTR, OPEN/READ/WRITE/RELEASE, OPENDIR and
  READDIR(PLUS), CREATE, UNLINK/MKDIR/RMDIR/RENAME(2), STATFS, FLUSH,
  FSYNC(DIR), ACCESS, and xattr stubs. Read-only mounts reject writes with
  EROFS at the op layer.

### VM (`dome-vm` 0.3.6)

- `VmCreateConfig` gains a `mounts: Vec<(tag, host_path, read_only)>` field
  wired through from the existing `set_directory_sharing_devices()` plumbing.

### CLI (`dome-cli` 0.6.1)

- Picks up the Linux backend fixes; no CLI-facing changes.

## 0.6.0

### Experimental Linux ARM64 support

Dome now ships Linux ARM64 CLI builds using a KVM-based backend, alongside the
existing macOS builds. Setup guide: https://dome.run/linux

Linux support is experimental, not production-ready yet. Homebrew remains
macOS-only; on Linux, use the install script.

### Linux backend (`dome-linux` 0.1.0, new)

- Initial release of the KVM-based backend crate, published to crates.io
- Mirrors the `dome-darwin` API surface (`VirtualMachine`, `VmState`, `VzError`,
  network attachment, terminal)

### VM (`dome-vm` 0.3.5)

- `dome-linux` dependency wired in unconditionally on Linux targets (no longer
  behind a `dome-linux` feature flag)

### CLI (`dome-cli` 0.6.0)

- `dome upgrade` selects the correct tarball per host (`darwin-aarch64` or
  `linux-aarch64`) instead of hard-coding darwin

### Installer and CI

- `install.sh` detects Linux ARM64 and installs the matching tarball; prints an
  experimental-support warning on Linux
- Release workflow builds both `darwin-aarch64` and `linux-aarch64` CLI tarballs
  (adds `ubuntu-24.04-arm` runner for the Linux job)
- Crates-publish workflow includes `dome-linux`

## 0.5.5

### Store (`dome-store` 0.1.1)

- Content-addressable chunk store with BLAKE3 hashing and local filesystem backend
- NBD (Network Block Device) server for serving VM disks from the chunk store
- `ChunkIndex` with parent-chain resolution for delta-only checkpoints
- Lazy ingestion: chunks read from flat rootfs on first access, no upfront conversion
- S3 chunk store backend

### CLI (`dome-cli` 0.5.5)

- CAS-backed VM disks via NBD: `dome run` now uses the chunk store by default
- `DOME_STORAGE=direct` env var to fall back to flat file mode
- Checkpoints saved as `.idx` (CAS delta index) when CAS is active, `.ext4` otherwise
- `checkpoint list` shows storage type and size for CAS checkpoints
- `--expose-host` flag for forwarding host ports to the guest via `host.dome.internal`
- `--disk-size` flag to set the VM disk size

### VM (`dome-vm` 0.3.3)

- NBD storage support: `SandboxBuilder::nbd_uri()` for attaching NBD-backed block devices
- `download()` method on `Sandbox` for downloading and extracting archives inside the guest
- Port forwarding for host-exposed ports via vsock

### Rust SDK (`dome-sdk` 0.3.3)

- CAS storage support behind the `cas` feature flag
- `StorageMode` enum: `Direct` (default, flat file with CoW) or `Cas { cas_dir }` for chunk store
- Checkpoints saved as `.idx` when CAS is active, `.ext4` otherwise
- `download()` method with progress reporting
- `open_watch()` for inotify-backed filesystem change events
- `discard_overlay()` to revert file changes in overlay mounts
- File management: `read_dir()`, `mkdir()`, `rename()`, `chmod()`, `remove()`
- `expose_host` config for forwarding host ports to the guest
- `open_shell()` gains `cwd` and `extra_env` parameters

### Guest (`dome-guest` 0.3.2)

- Download handler: fetch URLs, optionally extract `.tar.gz` archives, with progress reporting
- File management ops: `mkdir`, `read_dir`, `stat`, `remove`, `rename`, `copy`, `chmod`
- Filesystem watching via `inotify` with recursive directory support
- Overlay discard support for reverting file changes

### Protocol (`dome-proto` 0.3.2)

- `Download`, `DownloadProgress` types for in-guest downloads
- `ReadDir`, `Mkdir`, `Rename`, `Chmod`, `Remove`, `DiscardOverlay` request/response types
- `DOWNLOAD_REQ`, `DOWNLOAD_PROGRESS` frame types

### Proxy (`dome-proxy` 0.2.3)

- `ExposeHostMapping` config and DNS interception for `host.dome.internal`
- Exposed host ports resolved to `127.0.0.1` on the host side

### Darwin (`dome-darwin` 0.1.1)

- `VZNetworkBlockDeviceStorageDeviceAttachment` support for NBD-backed disks

### TypeScript SDK (`@superhq/dome` 0.4.2)

- `exposeHost` option in `StartOptions` for forwarding host ports to the guest

## 0.5.4

### CLI (`dome-cli` 0.5.4)

- Read-write mount support: `--mount ./src:/workspace:rw` (default remains read-only overlay)
- `--allow-host-writes` flag required for `:rw` mounts (same opt-in pattern as `--allow-net`)
- Mount host paths restricted to current working directory; `/` rejected as mount source or CWD
- BoringSSL for upstream TLS in proxy (Chrome-like TLS fingerprint passes Cloudflare)
- ALPN aligned to HTTP/1.1 only on MITM connections
- DNS AAAA queries return empty NOERROR (IPv4-only VM, fixes musl getaddrinfo)
- Removed rewrite rules from proxy

### VM (`dome-vm` 0.3.2)

- `MountConfig` gains `read_only: bool` field. Consumers must add this field (`true` preserves existing behavior).
- `validate_checkpoint_name()` rejects path traversal in checkpoint names (fixes #17)

### Guest (`dome-guest` 0.3.1)

- Direct VirtioFS mount for `:rw` mounts (skips overlay)

### Protocol (`dome-proto` 0.3.1)

- `MountRequest.read_only` field with `serde(default = true)` for backward compatibility

### Proxy (`dome-proxy` 0.2.2)

- Upstream TLS switched from rustls to BoringSSL (Cloudflare JA3/JA4 fingerprint compatibility)
- Guest-side ALPN set to HTTP/1.1 only (matches upstream, prevents protocol mismatch)
- DNS: empty NOERROR for AAAA queries instead of forwarding (IPv4-only stack)

### SDK (`dome-sdk` 0.3.2)

- Re-exports updated `MountConfig` with `read_only` field
- Checkpoint name validation on boot and save

### TypeScript SDK (`@superhq/dome` 0.4.1)

- `allowHostWrites` option in `StartOptions`
- `mounts` values support `:rw` suffix (e.g. `{ "./src": "/workspace:rw" }`)

## 0.4.1

### CLI (`dome-cli` 0.4.1)

- Fixed `--allow-net` having no effect in `--stdio` mode. Proxy networking now works via the SDK.
- Secret environment variables are now injected into exec/spawn calls in stdio mode
- CA certificate installation for MITM proxying in stdio mode

### SDK (`@superhq/dome` 0.3.1)

- `exec()` and `spawn()` now accept `string | string[]`. Array form passes argv directly with no shell interpretation.
- Added `shell` option to `ExecOptions` and `SpawnOptions` to override the default shell (e.g. `/bin/bash` instead of `sh`)
- New exported type: `ExecOptions`

## 0.4.0

### Streaming spawn, kill, and file watching

Full streaming I/O across the guest, CLI, and SDK - spawn long-running processes, stream stdout/stderr in real-time, kill processes, write to stdin, and watch files for changes.

#### Guest (`dome-guest` 0.2.0)

- Streaming piped exec: dedicated threads for stdout, stderr, and stdin relay with mpsc channel for frame serialization (no interleaved writes)
- `cwd` support in both piped and TTY exec modes
- Guest-side file watching via raw `libc::inotify` with recursive directory traversal, auto-watching new subdirectories, and `poll(2)` for clean shutdown on vsock hangup
- New frame types: `KILL`, `WATCH_REQ`, `WATCH_EVENT`

#### CLI (`dome-cli` 0.4.0)

- Rewrote `stdio.rs` from synchronous request-response to concurrent multiplexed architecture
- Main thread reads stdin JSON-RPC, dedicated event thread writes notifications to stdout
- Per-process std::threads relay vsock frames as JSON-RPC `output`/`exit` notifications
- New methods: `spawn` (returns pid, streams in background), `kill`, `input` (stdin forwarding), `watch` (file change events)
- `SharedWriter` (`Arc<Mutex<Stdout>>`) for thread-safe output from multiple process threads
- `ProcessHandle` with `mpsc::Sender<ProcessInput>` for stdin/kill forwarding to the correct vsock connection
- Backward-compatible: `exec`, `read_file`, `write_file`, `checkpoint` unchanged

#### Protocol (`dome-proto` 0.2.0)

- Added `KILL` (0x07), `WATCH_REQ` (0x30), `WATCH_EVENT` (0x31) frame types
- Added `cwd` field to `ExecRequest` (backward-compatible `Option`)
- Added `WatchRequest` and `WatchEvent` types

#### VM (`dome-vm` 0.2.0)

- `open_exec()`: connect vsock for streaming, returns raw `TcpStream` for caller-managed I/O
- `open_watch()`: connect vsock for file watching, returns stream emitting `WATCH_EVENT` frames

#### SDK (`@superhq/dome` 0.3.0)

- `sandbox.spawn(command, opts?)` — real-time stdout/stderr streaming via `SandboxProcess` handle
- `sandbox.watch(path, handler, opts?)` — guest-side inotify file change events
- `SandboxProcess`: `.on("stdout" | "stderr" | "exit")`, `.write()`, `.kill()`, `.exited`, `.pid`
- `SpawnOptions` (`cwd`, `env`), `WatchOptions` (`recursive`), `FileChangeEvent` type
- JSON-RPC notification dispatch for `output`, `exit`, `file_change` in `DomeProcess`
- Unit tests (13) with mock dome binary: spawn streaming, kill, watch, concurrent operations
- Integration tests (12) against real VM: streaming, stdin, kill, file creation/modification/deletion, recursive watch, concurrent watch+spawn

## 0.3.3

- Added `--secret` and `--allow-host` CLI flags for inline proxy config (no `dome.json` required)
- Replaced `dome.epoch` cmdline hack with proper PL031 RTC, now, the kernel sets wall clock at boot automatically
- Added `libatomic1` to rootfs
- SDK: `secrets` and `network` options now map to CLI flags directly (no temp config files)

## 0.3.2

- Fixed proxy corrupting large HTTP responses (e.g. `apt-get update`) due to dropped bytes when smoltcp TX buffer was full

## 0.3.1

- Fixed TLS certificate validation failures by syncing guest clock from host via kernel cmdline

## 0.3.0

### Custom minimal kernel, faster boot

Boot time reduced from ~5s to ~1s by replacing the Debian cloud kernel with a custom minimal Linux 6.12.x kernel.

- Custom kernel built from `kernel/dome_defconfig` with all VirtIO drivers built-in (~8MB, no loadable modules)
- Simplified initramfs with no module loading, no DHCP, no /dev/vda polling
- Quiet boot by default, use `--verbose` to see kernel output

### Proxy-based networking

All guest network traffic now flows through a userspace proxy on the host. No NAT device, no direct internet access.

- Domain allowlists via `dome.json`
- Secret injection: API keys stay on host, placeholder tokens swapped at proxy
- MITM TLS only when secrets need to be injected; blind-tunneled otherwise
- Fixed placeholder token collision with atomic counter
- Instance directory cleanup on error and PID reuse

**Note:** Existing checkpoints created with 0.2.x will continue to work.

## 0.2.0

### Breaking: Guest OS migrated from Alpine Linux to Debian

The guest VM now runs **Debian 13 (trixie)** instead of Alpine Linux 3.21. This is a breaking change for existing checkpoints and workflows that use `apk`.

**Why:** Alpine's musl libc is incompatible with many tools that assume glibc (e.g., Claude Code, VS Code server, many pre-built binaries). Debian's glibc resolves this and aligns with the standard environment developers expect.

**What changed:**

- **Package manager:** `apk add` -> `apt-get install -y`
- **Package names:** Some differ between Alpine and Debian (e.g., `build-base` → `build-essential`, `py3-pip` → `python3-pip`)
- **Kernel:** Alpine `linux-virt` -> Debian `linux-image-cloud-arm64`
- **Pre-installed tools:** `curl`, `git`, `jq`, `less`, `procps`, `openssh-client`, `iproute2`, `xz-utils`

**Migration guide:**

1. Run `dome upgrade` to get the new CLI and OS image.
2. Recreate any checkpoints using `apt-get` instead of `apk`:

```bash
# Before (Alpine)
dome checkpoint create myenv --allow-net -- apk add nodejs npm

# After (Debian)
dome checkpoint create myenv --allow-net -- apt-get install -y nodejs npm
```

3. Existing Alpine checkpoints will continue to boot (same kernel architecture, same init path), but new VMs start from Debian.
