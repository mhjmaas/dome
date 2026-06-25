guest_target := "aarch64-unknown-linux-musl"
binary := "target/debug/dome"

# List available recipes
default:
    @just --list

# Build the guest init binary (cross-compiled to aarch64 musl)
build-guest:
    cargo build -p dome-guest --target {{ guest_target }} --release

# Build the CLI binary (debug)
build-cli:
    cargo build -p dome-cli

# Codesign the CLI binary with virtualization entitlement
codesign:
    codesign --entitlements dome.entitlements --force -s - {{ binary }}

# Build everything: guest + CLI + codesign
build: build-guest build-cli codesign

# Prepare the rootfs, kernel, and initramfs (requires Docker)
prepare-rootfs:
    ./scripts/prepare-rootfs.sh

# Build a complete local OS image (guest + kernel + rootfs + initramfs) + VERSION; CLI uses it instead of downloading (Docker required on macOS)
build-image: build-guest
    ./scripts/prepare-rootfs.sh
    cargo pkgid -p dome-cli | sed 's/.*#//' > ~/.local/share/dome/VERSION
    # The CLI resolves the base via an immutable, version-addressed rootfs filename.
    # Materialize it from the freshly-built rootfs.ext4 (clones on APFS, copies elsewhere).
    cp -c ~/.local/share/dome/rootfs.ext4 ~/.local/share/dome/rootfs-$(cat ~/.local/share/dome/VERSION).ext4 2>/dev/null || cp ~/.local/share/dome/rootfs.ext4 ~/.local/share/dome/rootfs-$(cat ~/.local/share/dome/VERSION).ext4
    @echo "==> Local OS image ready ($(cat ~/.local/share/dome/VERSION)) — CLI will use it instead of downloading"

# Plain `build-image` skips an existing rootfs/initramfs (fast, but silently serves a STALE
# image when dome's own contents change — e.g. the rootfs landing profile or the guest binary).
# This sets FORCE=1, which prepare-rootfs.sh honors to regenerate them. The kernel stays cached;
# delete ~/.local/share/dome/Image to rebuild that too. To pick the new image up in an EXISTING
# sandbox, also drop its stale provision layer and recreate it: `dome sandbox rm <name>` + remove
# ~/.local/share/dome/provision/*.idx.

# Force a full local OS image rebuild, ignoring cached rootfs/initramfs (kernel stays cached)
rebuild-image:
    FORCE=1 just build-image

# Re-inject a freshly-built guest into the EXISTING boot image (Docker required). `build-image`
# and prepare-rootfs.sh SKIP existing artifacts, so a guest-only change (dome-guest) never reaches
# the VM without this. The guest that ACTUALLY runs is the copy baked into the initramfs: its
# `/init` overwrites the rootfs's `/usr/bin/dome-init` with its own copy on every boot, then
# `switch_root`s into it (see scripts/prepare-rootfs.sh). So this rebuilds the initramfs in place
# (replacing /bin/dome-init, preserving the init script + everything else). The rootfs copy is
# clobbered at boot, so it does not need swapping. A RUNNING sandbox keeps the old guest in
# memory — force-stop it afterward so it cold-boots the new one.
refresh-guest: build-guest
    #!/usr/bin/env bash
    set -euo pipefail
    data_dir="$HOME/.local/share/dome"
    guest="{{ justfile_directory() }}/target/{{ guest_target }}/release/dome-guest"
    initramfs="$data_dir/initramfs.cpio.gz"
    [ -f "$initramfs" ] || { echo "no $initramfs — run 'just build-image' first"; exit 1; }
    echo "==> rebuilding initramfs with the freshly-built guest"
    docker run --rm --platform linux/arm64/v8 \
        -v "$data_dir:/output" \
        -v "$guest:/tmp/dome-init:ro" \
        debian:trixie-slim /bin/sh -c '
            set -e
            apt-get update -qq >/dev/null 2>&1
            apt-get install -y -qq cpio gzip >/dev/null 2>&1
            mkdir -p /work && cd /work
            zcat /output/initramfs.cpio.gz | cpio -idm --quiet
            cp /tmp/dome-init bin/dome-init
            chmod 755 bin/dome-init
            find . | cpio -o -H newc --quiet | gzip > /output/initramfs.cpio.gz
        '
    echo "==> guest refreshed in $initramfs"
    echo "==> NOTE: 'dome sandbox stop --force <name>' any RUNNING sandbox so it cold-boots the new guest"

# Run a command inside the VM
run *args:
    {{ binary }} run -- {{ args }}

# Open an interactive shell in the VM
shell:
    {{ binary }} run -- sh

# Full setup from scratch: local OS image + CLI build
setup: build-image build

# Check all crates compile (host targets only)
check:
    cargo check

# Run clippy on all crates
clippy:
    cargo clippy --workspace

# Run the hypervisor-free unit + control-protocol tests (no VM, no codesign).
test:
    cargo test -p dome-cli

# Run the #[ignore]d real-VM integration tests. Needs macOS Virtualization.framework and a
# codesigned binary. Pass a substring to run a subset: `just test-vm runtime_mounts` runs one
# test; bare `just test-vm` runs them all.
test-vm filter="": build-guest build-cli
    # Build all test harnesses first. We can't sign target/debug/dome in place: every `cargo
    # test` re-hardlinks it from the cached deps/ artifact, reverting any signature. So sign a
    # private COPY that cargo never manages, and point the tests at it via DOME_BIN. A spawned
    # worker inherits the signature too (it re-execs current_exe, i.e. this signed copy).
    cargo test -p dome-cli --tests --no-run
    cp -f {{ binary }} {{ binary }}-signed
    codesign --entitlements dome.entitlements --force -s - {{ binary }}-signed
    # Run serially (`--test-threads=1`): every real-VM test shares one global domed +
    # data dir under ~/.local/share/dome, so libtest's default parallelism would race the
    # daemon and make the global `workers: N` assertions flaky. cargo already runs the test
    # binaries one at a time, so this serializes the whole suite end to end.
    DOME_BIN={{ justfile_directory() }}/{{ binary }}-signed cargo test -p dome-cli --tests --no-fail-fast -- --ignored --test-threads=1 {{ filter }}

# Install the binary to ~/.local/bin with codesign
install: build-guest
    cargo build -p dome-cli --release
    codesign --entitlements dome.entitlements --force -s - target/release/dome
    mkdir -p ~/.local/bin
    # Install atomically: copy to a temp path, then rename into place. An in-place
    # `cp` rewrites the same inode (O_TRUNC), so a `domed`/worker from a prior build
    # still mmap'ing it pins the old, differently-signed code pages — macOS then
    # SIGKILLs new execs on a code-signature page-hash mismatch (Killed: 9 / exit 137).
    # `mv` swaps in a fresh inode, leaving running processes on their old one.
    cp target/release/dome ~/.local/bin/dome.tmp
    mv -f ~/.local/bin/dome.tmp ~/.local/bin/dome
    mkdir -p ~/.local/share/dome
    cargo pkgid -p dome-cli | sed 's/.*#//' > ~/.local/share/dome/VERSION

# Tag and push a release (triggers GitHub Actions)
release version:
    git tag -a "v{{ version }}" -m "Release v{{ version }}"
    git push origin "v{{ version }}"
