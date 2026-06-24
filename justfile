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
    DOME_BIN={{ justfile_directory() }}/{{ binary }}-signed cargo test -p dome-cli --tests --no-fail-fast -- --ignored {{ filter }}

# Install the binary to ~/.local/bin with codesign
install: build-guest
    cargo build -p dome-cli --release
    codesign --entitlements dome.entitlements --force -s - target/release/dome
    mkdir -p ~/.local/bin
    cp target/release/dome ~/.local/bin/dome
    mkdir -p ~/.local/share/dome
    cargo pkgid -p dome-cli | sed 's/.*#//' > ~/.local/share/dome/VERSION

# Tag and push a release (triggers GitHub Actions)
release version:
    git tag -a "v{{ version }}" -m "Release v{{ version }}"
    git push origin "v{{ version }}"
