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
