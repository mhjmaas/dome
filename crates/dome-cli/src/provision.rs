//! Declarative sandbox provisioning: cached toolchain layer.
//!
//! A project declares its toolchain prerequisites in a `provision` block in `dome.json`
//! (see [`crate::config::ProvisionEntry`]). The first sandbox/`run` creation on that spec
//! runs the steps once inside a build VM, snapshots the result as a hidden checkpoint keyed
//! by a hash of the spec, and seeds every later creation `--from` that cached layer — so the
//! provisioning cost is paid exactly once. Change the spec, the hash changes, the layer
//! rebuilds; the cache never silently serves a stale toolchain.
//!
//! ## Layering
//!
//! ```text
//! bare base image → provisioned checkpoint (hash-keyed, cached) → sandbox (mutable work)
//!    (shared)          {data_dir}/provision/<hash>.idx               (per-project)
//! ```
//!
//! ## Testability
//!
//! The key/parse/lookup/lock/publish logic is exercised without a hypervisor by injecting a
//! [`StepRunner`]: the real [`VmStepRunner`] boots a VM and runs the steps, while tests pass a
//! fake runner that just writes an index file. This module owns the cache key, the per-hash
//! lock, and the atomic publish; the VM build itself lives in [`crate::vm`].

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::cli::VmArgs;
use crate::sandbox_config::{ProvisionSpec, ResolvedConfig, SecretSpec};

/// Compute the content hash that keys a provisioned layer:
/// `sha256(base identity ‖ normalized steps ‖ provision.allow ‖ secret mappings)`.
///
/// The base identity is the CLI VERSION string (the bare base image is version-pinned), so a
/// new dome release rebuilds layers rather than serving one baked against an older base.
/// Steps are framed length-prefixed and hashed in order, so the key is **order-sensitive** on
/// steps; the allow-list is framed the same way, so the key is **sensitive to `allow`** too.
/// Each secret's *mapping* (`name + from + hosts`) joins the key — so re-pointing a secret to a
/// different env var or host rebuilds — but the secret **value never enters the key** (it isn't
/// even available here: a [`SecretSpec`] carries the mapping only).
///
/// `identity` is the base the layer composes on: at steady state the CLI VERSION (the bare base
/// image is version-pinned), or — when a creation seeds `--from <checkpoint/sandbox>` — the
/// seed's content identity from [`seed_identity`]. Either way it pins the layer to the disk it
/// was built on top of, so changing the base (a new release, or a different seed) rebuilds.
pub(crate) fn cache_key(
    identity: &str,
    steps: &[String],
    allow: &[String],
    secrets: &[SecretSpec],
) -> String {
    let mut hasher = Sha256::new();
    // Length-prefix every field so no concatenation of inputs can collide with another
    // (e.g. steps ["ab","c"] must not hash the same as ["a","bc"]).
    hasher.update((identity.len() as u64).to_le_bytes());
    hasher.update(identity.as_bytes());
    hasher.update(b"steps");
    hasher.update((steps.len() as u64).to_le_bytes());
    for s in steps {
        hasher.update((s.len() as u64).to_le_bytes());
        hasher.update(s.as_bytes());
    }
    hasher.update(b"allow");
    hasher.update((allow.len() as u64).to_le_bytes());
    for a in allow {
        hasher.update((a.len() as u64).to_le_bytes());
        hasher.update(a.as_bytes());
    }
    // Secret mappings: name + from + hosts, framed so re-pointing a secret rebuilds the layer.
    // The value is deliberately absent — only the mapping affects the key.
    hasher.update(b"secrets");
    hasher.update((secrets.len() as u64).to_le_bytes());
    for s in secrets {
        for field in [&s.name, &s.from] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }
        hasher.update((s.hosts.len() as u64).to_le_bytes());
        for h in &s.hosts {
            hasher.update((h.len() as u64).to_le_bytes());
            hasher.update(h.as_bytes());
        }
    }
    format!("{:x}", hasher.finalize())
}

/// Compute the content identity of a `--from` seed index — the stable fingerprint of the disk
/// state provisioning will compose on top of. Used in place of the CLI VERSION base identity in
/// [`cache_key`] when a creation seeds `--from`: the same seed + spec resolves to the same hash
/// (cache hit), and a different seed changes the hash (rebuild on top of the new content).
///
/// The parent chain is flattened first so the identity reflects the *resolved* content rather
/// than which parent index a chunk happens to live in (so two seeds with identical content but
/// different chain shapes share a hash). The fingerprint covers the pinned base
/// (`fallback_path`, which decides what never-written chunks resolve to) and every chunk hash in
/// order — never any chunk *data*, so it stays cheap. Returned prefixed so a seed identity can
/// never collide with a bare VERSION string in the key's identity slot.
pub(crate) fn seed_identity(seed_index: &str) -> Result<String> {
    let flat = dome_store::ChunkIndex::flatten_chain(seed_index)
        .with_context(|| format!("fingerprinting seed index {seed_index}"))?;
    let mut hasher = Sha256::new();
    let fallback = flat.fallback_path.as_deref().unwrap_or("");
    hasher.update((fallback.len() as u64).to_le_bytes());
    hasher.update(fallback.as_bytes());
    let n = flat.num_chunks();
    hasher.update((n as u64).to_le_bytes());
    for i in 0..n {
        let h = flat.get_hash(i).unwrap_or("ZERO");
        hasher.update((h.len() as u64).to_le_bytes());
        hasher.update(h.as_bytes());
    }
    Ok(format!("seed:{:x}", hasher.finalize()))
}

/// Runs the provisioning steps and writes the resulting CAS index to `out_index`. Injected so
/// the orchestration (key/lookup/lock/publish) is testable without a hypervisor.
pub(crate) trait StepRunner {
    /// Build a provisioned layer: run `spec.steps` (as root, sequentially, stop-on-first-
    /// failure, each via `sh -c`, project dir NOT mounted, network narrowed by `spec.allow`)
    /// and write the resulting CAS index to `out_index`. The steps start from the bare base, or
    /// — when `seed` is set (a `--from` composition) — from that seed's CAS index, so the
    /// toolchain is layered on top of the seeded disk rather than a fresh one.
    ///
    /// On failure, the half-provisioned disk is saved to `failed_index` (when set) so the
    /// developer can shell into it without re-running steps; nothing is ever written to
    /// `out_index` from a failed build, so the success hash is never published partially.
    fn build(
        &self,
        spec: &ProvisionSpec,
        disk_size_mb: u64,
        env: &VmArgs,
        out_index: &str,
        failed_index: Option<&str>,
        seed: Option<&str>,
    ) -> Result<()>;
}

/// The production step-runner: boots a build VM and runs the steps via [`crate::vm`].
pub(crate) struct VmStepRunner;

impl StepRunner for VmStepRunner {
    fn build(
        &self,
        spec: &ProvisionSpec,
        disk_size_mb: u64,
        env: &VmArgs,
        out_index: &str,
        failed_index: Option<&str>,
        seed: Option<&str>,
    ) -> Result<()> {
        crate::vm::build_provision_layer(spec, disk_size_mb, env, out_index, failed_index, seed)
    }
}

/// The directory holding cached provisioned layers: `{data_dir}/provision`.
fn provision_dir(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join("provision")
}

/// The cached layer index path for a given hash: `{data_dir}/provision/<hash>.idx`.
pub(crate) fn layer_path(data_dir: &str, hash: &str) -> PathBuf {
    provision_dir(data_dir).join(format!("{hash}.idx"))
}

/// The preserved half-provisioned ("debug") disk path for a given hash:
/// `{data_dir}/provision/<hash>.failed`. Parked when a build fails so the developer can
/// shell into it without re-running steps; overwritten by the next successful build and
/// reclaimed by `dome prune`.
pub(crate) fn failed_layer_path(data_dir: &str, hash: &str) -> PathBuf {
    provision_dir(data_dir).join(format!("{hash}.failed"))
}

/// Resolve the provisioned layer for `spec`, building it once if uncached, and return its CAS
/// index path. Returns `Ok(None)` when there is nothing to provision (no steps).
///
/// The first creation on an uncached spec acquires the per-hash lock, builds into a temp file,
/// and atomically renames it into `{data_dir}/provision/<hash>.idx`; concurrent creations block
/// on the lock then cache-hit, so exactly one build runs and a reader never observes a partial
/// `.idx`. A cached spec returns instantly with no lock, no network, and no rebuild.
///
/// When `rebuild` is set, the cache is bypassed on both the fast path and the post-lock
/// re-check: the layer is built fresh and atomically replaces any layer already on disk for
/// this exact hash (the rename overwrites in place), so `--rebuild` forces a clean toolchain
/// even when a cached one would otherwise be served. The build still runs under the per-hash
/// lock, so a concurrent plain creation never observes a partial `.idx`.
///
/// When `seed` is set (a `--from` composition), the layer's cache key swaps the CLI VERSION
/// base identity for the seed's content identity ([`seed_identity`]) and the build runs the
/// steps on top of that seed's disk rather than the bare base — so the same seed + spec
/// cache-hits and a changed seed rebuilds.
#[allow(clippy::too_many_arguments)] // cohesive build inputs; bundling them would not aid clarity
pub(crate) fn ensure_layer(
    data_dir: &str,
    version: &str,
    spec: &ProvisionSpec,
    disk_size_mb: u64,
    env: &VmArgs,
    runner: &dyn StepRunner,
    rebuild: bool,
    seed: Option<&str>,
) -> Result<Option<String>> {
    if spec.steps.is_empty() {
        return Ok(None);
    }

    // Base identity: the seed's content fingerprint when composing `--from`, else the CLI
    // VERSION. Everything else about the key (steps, allow, secrets) is identical either way.
    let identity = match seed {
        Some(seed_index) => seed_identity(seed_index)?,
        None => version.to_string(),
    };
    let hash = cache_key(&identity, &spec.steps, &spec.allow, &spec.secrets);
    let idx_path = layer_path(data_dir, &hash);

    // Fast path: a published layer is served instantly, without taking the lock. `--rebuild`
    // skips it so the layer is always rebuilt fresh.
    if !rebuild && idx_path.exists() {
        return Ok(Some(path_string(&idx_path)?));
    }

    let dir = provision_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating provision dir {}", dir.display()))?;

    // Serialize builders of this exact spec: the loser blocks here until the winner publishes,
    // then falls through to the post-lock cache-hit check below.
    let _lock = HashLock::acquire(&dir.join(format!("{hash}.lock")))?;

    // Re-check under the lock: a concurrent builder may have published while we blocked.
    // `--rebuild` ignores this too — it always rebuilds, then overwrites the cached layer.
    if !rebuild && idx_path.exists() {
        return Ok(Some(path_string(&idx_path)?));
    }

    // Build into a per-process temp file, then publish atomically so a reader never sees a
    // partial `.idx`. A failed build leaves no temp behind to be mistaken for a layer.
    let tmp_path = dir.join(format!("{hash}.{}.tmp.idx", std::process::id()));
    let tmp = path_string(&tmp_path)?;
    let failed_path = failed_layer_path(data_dir, &hash);
    let failed = path_string(&failed_path)?;
    let _ = std::fs::remove_file(&tmp_path);

    if rebuild {
        eprintln!("dome: re-provisioning (--rebuild: forcing a fresh build of this spec)…");
    } else {
        eprintln!("dome: provisioning (first run for this spec)…");
    }
    let build = runner.build(spec, disk_size_mb, env, &tmp, Some(&failed), seed);
    if let Err(e) = build {
        let _ = std::fs::remove_file(&tmp_path);
        // The build parks the half-provisioned disk at `<hash>.failed` on failure. If it's
        // there, point the developer at the opt-in debug shell — booting that disk without
        // re-running steps. Nothing was published under the success hash.
        if failed_path.exists() {
            let short = &hash[..hash.len().min(12)];
            eprintln!("dome:");
            eprintln!("dome: the half-provisioned disk was preserved for inspection.");
            eprintln!("dome: open a debug shell on it (provision steps are NOT re-run) with:");
            eprintln!("dome:     dome provision debug {short}");
        }
        return Err(e).context("provisioning failed");
    }

    std::fs::rename(&tmp_path, &idx_path).with_context(|| {
        format!(
            "publishing provisioned layer {} -> {}",
            tmp_path.display(),
            idx_path.display()
        )
    })?;
    // A clean build supersedes any preserved failure disk for this exact spec; drop it so a
    // stale debug disk can't linger (and its chunks become reclaimable by `dome prune`).
    let _ = std::fs::remove_file(&failed_path);
    eprintln!(
        "dome: {}, cached ({}…)",
        if rebuild {
            "re-provisioned"
        } else {
            "provisioned"
        },
        &hash[..hash.len().min(12)]
    );
    Ok(Some(path_string(&idx_path)?))
}

/// Boot the preserved half-provisioned disk for a failed build into an interactive shell,
/// **without re-running any provision steps**, so the developer can investigate why a step
/// died (a missing package, a wrong path, …). The disk is ridden read-only-ish like any seed:
/// this is an ephemeral session that saves nothing, so poking around never mutates the cache.
///
/// `hash` is the value (or a unique prefix of it) printed by the failing build; when omitted
/// and exactly one failure disk is present, that one is used.
pub(crate) fn debug_shell(hash: Option<&str>, env: &VmArgs) -> Result<i32> {
    let data_dir = dome_vm::default_data_dir();
    let (full_hash, failed) = resolve_failed_layer(&data_dir, hash)?;
    let short = &full_hash[..full_hash.len().min(12)];
    eprintln!("dome: opening a shell on the half-provisioned disk for {short}…");
    eprintln!("dome: (provision steps are NOT re-run; nothing here is saved back to the cache)");

    // Pin the disk size to whatever the preserved index was built with: the index encodes a
    // fixed chunk count, so booting it at a different size would corrupt the filesystem. (Same
    // invariant the persistent-sandbox boot path enforces.)
    let stored = dome_store::ChunkIndex::load(&failed)
        .with_context(|| format!("loading preserved provisioning disk {failed}"))?
        .disk_size()
        / (1024 * 1024);
    let cfg = ResolvedConfig {
        disk_size: Some(stored),
        ..Default::default()
    };

    // Ride the failure disk as a provision seed (an absolute CAS index path), exactly as a
    // normal provisioned layer is ridden — reads resolve through it and its pinned base.
    let prepared = crate::vm::prepare_vm(&cfg, env, None, Some(&failed), None)?;
    crate::session::run_session(
        &prepared,
        &["/bin/sh".to_string()],
        &crate::session::SaveTarget::None,
    )
}

/// Resolve a preserved failure disk under `{data_dir}/provision` by full hash or unique
/// prefix; with no selector, succeed only when exactly one is present. Returns
/// `(full_hash, index_path)`.
fn resolve_failed_layer(data_dir: &str, selector: Option<&str>) -> Result<(String, String)> {
    let dir = provision_dir(data_dir);
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("failed") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                found.push((stem.to_string(), path));
            }
        }
    }

    let matches: Vec<(String, PathBuf)> = match selector {
        Some(sel) => found
            .into_iter()
            .filter(|(stem, _)| stem.starts_with(sel))
            .collect(),
        None => found,
    };

    match matches.len() {
        0 => match selector {
            Some(sel) => {
                anyhow::bail!("no preserved provisioning disk matches '{sel}' under {}/provision", data_dir)
            }
            None => anyhow::bail!(
                "no preserved provisioning disk found under {}/provision (a build must fail to leave one)",
                data_dir
            ),
        },
        1 => {
            let (stem, path) = matches.into_iter().next().unwrap();
            Ok((stem, path_string(&path)?))
        }
        _ => {
            let shorts: Vec<String> = matches
                .iter()
                .map(|(stem, _)| stem[..stem.len().min(12)].to_string())
                .collect();
            anyhow::bail!(
                "multiple preserved provisioning disks; pick one: {}",
                shorts.join(", ")
            )
        }
    }
}

/// Remove every preserved failure disk under `{data_dir}/provision`, returning how many were
/// removed. Their now-unreferenced chunks are reclaimed by the CAS sweep that follows in
/// `dome prune`. Called by `dome prune`; missing dir is treated as "nothing to do".
pub(crate) fn prune_failed_layers(data_dir: &str) -> Result<usize> {
    let dir = provision_dir(data_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("failed") {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing preserved disk {}", path.display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// The OS base version a layer is pinned to, parsed from the versioned base image its index
/// falls back to (`rootfs-<version>.ext4`). The layer's cache key folds in the CLI VERSION the
/// layer was built against — which equals this base version at steady state — so this is how a
/// later release recognises a layer keyed against a superseded base. `None` when the index
/// records no pinned base (corrupt) or a non-standard one (e.g. an explicit `--rootfs`), in
/// which case the layer's version can't be determined and reclamation leaves it alone.
fn layer_base_version(idx: &dome_store::ChunkIndex) -> Option<String> {
    idx.fallback_path.as_deref().and_then(|p| {
        let file = Path::new(p).file_name().and_then(|s| s.to_str())?;
        file.strip_prefix("rootfs-")
            .and_then(|s| s.strip_suffix(".ext4"))
            .map(|s| s.to_string())
    })
}

/// Iterate the published layer indexes (`<hash>.idx`) under `{data_dir}/provision`, yielding
/// `(hash, path)` for each. Skips the per-process build temp (`<hash>.<pid>.tmp.idx`, whose
/// stem carries dots a real hex hash never does) so an in-flight build is never mistaken for a
/// published layer. A missing dir yields nothing.
fn published_layers(data_dir: &str) -> Result<Vec<(String, PathBuf)>> {
    let dir = provision_dir(data_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("idx") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.contains('.') {
            continue; // a build temp, not a published layer
        }
        out.push((stem.to_string(), path));
    }
    Ok(out)
}

/// List the cached provisioned layers with their delta size, pinned OS base, staleness, and
/// age — the dedicated view that keeps these hidden checkpoints off `dome checkpoint list`
/// while still making the cache inspectable. A layer is **stale** when its base version no
/// longer matches the installed OS: its hash can never cache-hit again, so it is dead weight
/// that `dome upgrade` / `dome prune` will reclaim.
pub(crate) fn list() -> Result<()> {
    let data_dir = dome_vm::default_data_dir();
    let current = crate::assets::installed_version(&data_dir);

    let mut layers: Vec<(String, u64, String, bool, std::time::SystemTime)> = Vec::new();
    for (hash, path) in published_layers(&data_dir)? {
        let Some(path_str) = path.to_str() else {
            continue;
        };
        let idx = match dome_store::ChunkIndex::load(path_str) {
            Ok(idx) => idx,
            Err(e) => {
                eprintln!(
                    "dome: skipping unreadable provisioned layer '{}': {:#}",
                    hash, e
                );
                continue;
            }
        };
        // CAS delta size: non-ZERO chunks × 64 KiB, matching `checkpoint list` / `sandbox ls`.
        let non_zero = (0..idx.num_chunks())
            .filter(|&i| idx.get_hash(i).map(|h| h != "ZERO").unwrap_or(false))
            .count();
        let size_bytes = (non_zero as u64) * 64 * 1024;
        let base = layer_base_version(&idx).unwrap_or_else(|| "?".to_string());
        // Stale iff we know the installed version and this layer's base differs from it.
        let stale = current.as_deref().map(|c| c != base).unwrap_or(false);
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified())?;
        layers.push((hash, size_bytes, base, stale, mtime));
    }

    if layers.is_empty() {
        eprintln!("No provisioned layers found.");
        return Ok(());
    }

    layers.sort_by_key(|(_, _, _, _, t)| *t);

    let header = ["HASH", "SIZE", "BASE", "STATUS", "CREATED"];
    let rows: Vec<Vec<String>> = layers
        .iter()
        .map(|(hash, size, base, stale, mtime)| {
            vec![
                hash[..hash.len().min(12)].to_string(),
                crate::checkpoint::format_cas_size(*size),
                base.clone(),
                if *stale { "stale" } else { "current" }.to_string(),
                crate::checkpoint::format_age(*mtime),
            ]
        })
        .collect();

    print!("{}", crate::checkpoint::render_table(&header, &rows));
    Ok(())
}

/// Reclaim every published layer whose pinned OS base no longer matches `current_version`: a
/// stale layer's hash folds in a superseded base identity, so it can never cache-hit again and
/// is pure dead weight. Only the `.idx` is removed here (fast, like `sandbox rm`); the now-
/// orphaned chunks are swept by the CAS mark-and-sweep that `dome prune` runs next. A layer
/// whose base version can't be determined is left untouched (never reclaim on a guess).
/// Returns how many layers were removed. Called by `dome upgrade` and `dome prune`.
pub(crate) fn reclaim_stale_layers(data_dir: &str, current_version: &str) -> Result<usize> {
    let mut removed = 0;
    for (hash, path) in published_layers(data_dir)? {
        let Some(path_str) = path.to_str() else {
            continue;
        };
        let idx = match dome_store::ChunkIndex::load(path_str) {
            Ok(idx) => idx,
            Err(e) => {
                eprintln!(
                    "dome: skipping unreadable provisioned layer '{}': {:#}",
                    hash, e
                );
                continue;
            }
        };
        match layer_base_version(&idx) {
            Some(base) if base != current_version => {
                std::fs::remove_file(&path).with_context(|| {
                    format!("removing stale provisioned layer {}", path.display())
                })?;
                removed += 1;
            }
            _ => {}
        }
    }
    Ok(removed)
}

/// Reclaim **every** published layer wholesale, regardless of base version — the force-clear
/// the `--provision` flag on `dome prune` selects when a user wants a clean cache (every layer
/// rebuilds on its next creation). Like [`reclaim_stale_layers`], this removes only the `.idx`;
/// the orphaned chunks are swept by the CAS sweep that follows. Returns the count removed.
pub(crate) fn prune_all_layers(data_dir: &str) -> Result<usize> {
    let mut removed = 0;
    for (_hash, path) in published_layers(data_dir)? {
        std::fs::remove_file(&path)
            .with_context(|| format!("removing provisioned layer {}", path.display()))?;
        removed += 1;
    }
    Ok(removed)
}

/// A blocking, advisory per-hash build lock. `acquire` blocks until the lock is held (so a
/// second concurrent builder waits rather than racing), and the guard releases it on drop by
/// closing the descriptor. The lock file itself is left in place — it is just a mutex handle,
/// never read for content — so there is no unlink/recreate race between builders.
struct HashLock {
    _file: std::fs::File,
}

impl HashLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening provision lock {}", path.display()))?;
        // flock(LOCK_EX) blocks until exclusive access is granted; it is released when the
        // descriptor is closed (guard drop) or the process dies, so a crashed builder never
        // wedges the lock.
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("locking provision lock {}", path.display()));
        }
        Ok(Self { _file: file })
    }
}

/// Render a path as an owned `String`, erroring on the (dome-impossible) non-UTF-8 case rather
/// than silently lossy-converting a path the rest of the storage layer takes as `&str`.
fn path_string(p: &Path) -> Result<String> {
    p.to_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path: {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn spec(steps: &[&str], allow: &[&str]) -> ProvisionSpec {
        ProvisionSpec {
            steps: steps.iter().map(|s| s.to_string()).collect(),
            allow: allow.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn secret(name: &str, from: &str, hosts: &[&str]) -> SecretSpec {
        SecretSpec {
            name: name.to_string(),
            from: from.to_string(),
            hosts: hosts.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn cache_key_is_deterministic() {
        let s = vec!["apt-get install -y nodejs".to_string()];
        let a = vec!["deb.debian.org".to_string()];
        assert_eq!(
            cache_key("0.6.3", &s, &a, &[]),
            cache_key("0.6.3", &s, &a, &[])
        );
    }

    #[test]
    fn cache_key_is_order_sensitive_on_steps() {
        let a = ["one".to_string(), "two".to_string()];
        let b = ["two".to_string(), "one".to_string()];
        assert_ne!(
            cache_key("v", &a, &[], &[]),
            cache_key("v", &b, &[], &[]),
            "reordering steps must change the key (order matters for a build)"
        );
    }

    #[test]
    fn cache_key_is_sensitive_to_allow_and_to_framing() {
        let steps = ["s".to_string()];
        assert_ne!(
            cache_key("v", &steps, &["a.com".to_string()], &[]),
            cache_key("v", &steps, &["b.com".to_string()], &[]),
            "changing the allow-list must change the key"
        );
        // Length-prefix framing means a moved boundary cannot collide: ["ab","c"] != ["a","bc"].
        let x = ["ab".to_string(), "c".to_string()];
        let y = ["a".to_string(), "bc".to_string()];
        assert_ne!(cache_key("v", &x, &[], &[]), cache_key("v", &y, &[], &[]));
    }

    #[test]
    fn cache_key_is_sensitive_to_base_version() {
        let s = ["s".to_string()];
        assert_ne!(
            cache_key("0.6.3", &s, &[], &[]),
            cache_key("0.7.0", &s, &[], &[]),
            "the CLI VERSION is the base identity; bumping it must rebuild the layer"
        );
    }

    #[test]
    fn cache_key_is_sensitive_to_the_secret_mapping() {
        let s = ["s".to_string()];
        // Re-pointing a secret to a different env var or host changes the key, so the cache
        // never serves a layer built against a different secret wiring.
        let base = vec![secret("npm", "NPM_TOKEN", &["registry.corp.internal"])];
        let diff_from = vec![secret("npm", "OTHER_TOKEN", &["registry.corp.internal"])];
        let diff_host = vec![secret("npm", "NPM_TOKEN", &["registry.other.internal"])];
        let diff_name = vec![secret("pip", "NPM_TOKEN", &["registry.corp.internal"])];
        assert_ne!(
            cache_key("v", &s, &[], &base),
            cache_key("v", &s, &[], &[]),
            "adding a secret must change the key"
        );
        assert_ne!(
            cache_key("v", &s, &[], &base),
            cache_key("v", &s, &[], &diff_from)
        );
        assert_ne!(
            cache_key("v", &s, &[], &base),
            cache_key("v", &s, &[], &diff_host)
        );
        assert_ne!(
            cache_key("v", &s, &[], &base),
            cache_key("v", &s, &[], &diff_name)
        );
        // The same mapping yields the same key (determinism).
        assert_eq!(
            cache_key("v", &s, &[], &base),
            cache_key(
                "v",
                &s,
                &[],
                &[secret("npm", "NPM_TOKEN", &["registry.corp.internal"])]
            )
        );
    }

    #[test]
    fn cache_key_ignores_the_secret_value() {
        // The key is computed from the {name, from, hosts} mapping only — a SecretSpec carries
        // no value, so the real token's bytes can never enter the key. Setting/changing the
        // env var the secret reads from must not change the layer hash.
        let s = ["s".to_string()];
        let secrets = vec![secret("npm", "NPM_TOKEN", &["registry.corp.internal"])];
        std::env::set_var("NPM_TOKEN", "value-one");
        let k1 = cache_key("v", &s, &[], &secrets);
        std::env::set_var("NPM_TOKEN", "a-completely-different-value");
        let k2 = cache_key("v", &s, &[], &secrets);
        std::env::remove_var("NPM_TOKEN");
        assert_eq!(k1, k2, "the secret value must never affect the cache key");
    }

    /// A fake runner that records its build calls and writes a sentinel index file, so the
    /// orchestration (lookup/lock/publish) can be driven without a hypervisor.
    struct FakeRunner {
        calls: Arc<AtomicUsize>,
        contents: &'static str,
    }

    impl StepRunner for FakeRunner {
        fn build(
            &self,
            _spec: &ProvisionSpec,
            _disk_size_mb: u64,
            _env: &VmArgs,
            out_index: &str,
            _failed_index: Option<&str>,
            _seed: Option<&str>,
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(out_index, self.contents).unwrap();
            Ok(())
        }
    }

    /// A runner that always fails after parking a half-provisioned disk at `failed_index`,
    /// mirroring the real build: nothing is written to `out_index` on failure.
    struct FailingRunner {
        calls: Arc<AtomicUsize>,
    }

    impl StepRunner for FailingRunner {
        fn build(
            &self,
            _spec: &ProvisionSpec,
            _disk_size_mb: u64,
            _env: &VmArgs,
            _out_index: &str,
            failed_index: Option<&str>,
            _seed: Option<&str>,
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(failed) = failed_index {
                std::fs::write(failed, "half-provisioned").unwrap();
            }
            Err(anyhow::anyhow!(
                "provision step failed (exit 1): apt-get install nope"
            ))
        }
    }

    #[test]
    fn no_steps_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = FakeRunner {
            calls: calls.clone(),
            contents: "x",
        };
        let layer = ensure_layer(
            tmp.path().to_str().unwrap(),
            "v",
            &spec(&[], &[]),
            4096,
            &VmArgs::default(),
            &runner,
            false,
            None,
        )
        .unwrap();
        assert!(layer.is_none(), "a spec with no steps provisions nothing");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no build runs");
    }

    #[test]
    fn cold_build_then_cache_hit_runs_exactly_one_build() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = FakeRunner {
            calls: calls.clone(),
            contents: "layer-bytes",
        };
        let s = spec(&["apt-get install -y nodejs"], &["deb.debian.org"]);

        // First creation: cold build + publish.
        let first = ensure_layer(
            data_dir,
            "v",
            &s,
            4096,
            &VmArgs::default(),
            &runner,
            false,
            None,
        )
        .unwrap()
        .expect("a spec with steps yields a layer");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "cold build ran once");
        assert!(Path::new(&first).exists(), "the layer was published");
        // Published atomically into the per-hash path (no temp left behind).
        let hash = cache_key("v", &s.steps, &s.allow, &s.secrets);
        assert_eq!(first, layer_path(data_dir, &hash).to_str().unwrap());
        let leftovers: Vec<_> = std::fs::read_dir(provision_dir(data_dir).as_path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no temp file survives a publish");

        // Second creation on the same spec: cache hit, no rebuild.
        let second = ensure_layer(
            data_dir,
            "v",
            &s,
            4096,
            &VmArgs::default(),
            &runner,
            false,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(second, first, "the same spec resolves to the same layer");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a cache hit must not rebuild"
        );
    }

    #[test]
    fn editing_the_spec_changes_the_hash_and_rebuilds() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = FakeRunner {
            calls: calls.clone(),
            contents: "x",
        };

        let a = ensure_layer(
            data_dir,
            "v",
            &spec(&["install A"], &[]),
            4096,
            &VmArgs::default(),
            &runner,
            false,
            None,
        )
        .unwrap()
        .unwrap();
        let b = ensure_layer(
            data_dir,
            "v",
            &spec(&["install B"], &[]),
            4096,
            &VmArgs::default(),
            &runner,
            false,
            None,
        )
        .unwrap()
        .unwrap();

        assert_ne!(a, b, "a changed spec resolves to a different layer path");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "each distinct spec triggers its own build"
        );
    }

    /// A runner that sleeps before writing, to widen the race window so the per-hash lock is
    /// genuinely exercised by concurrent builders.
    struct SlowRunner {
        calls: Arc<AtomicUsize>,
    }

    impl StepRunner for SlowRunner {
        fn build(
            &self,
            _spec: &ProvisionSpec,
            _disk_size_mb: u64,
            _env: &VmArgs,
            out_index: &str,
            _failed_index: Option<&str>,
            _seed: Option<&str>,
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(80));
            std::fs::write(out_index, "layer").unwrap();
            Ok(())
        }
    }

    #[test]
    fn concurrent_creations_run_exactly_one_build() {
        // Two creations on the same uncached spec must run exactly one build: the per-hash
        // lock makes the second block until the first publishes, then it cache-hits.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap().to_string();
        let calls = Arc::new(AtomicUsize::new(0));
        let s = spec(&["install everything"], &["deb.debian.org"]);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let data_dir = data_dir.clone();
                let calls = calls.clone();
                let s = s.clone();
                std::thread::spawn(move || {
                    let runner = SlowRunner { calls };
                    ensure_layer(
                        &data_dir,
                        "v",
                        &s,
                        4096,
                        &VmArgs::default(),
                        &runner,
                        false,
                        None,
                    )
                    .unwrap()
                    .unwrap()
                })
            })
            .collect();

        let paths: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the per-hash lock must collapse concurrent builds to exactly one"
        );
        assert!(
            paths.iter().all(|p| *p == paths[0]),
            "every concurrent creation resolves to the same published layer"
        );
    }

    #[test]
    fn published_layer_has_the_built_contents() {
        // The publish renames the temp the runner wrote, so the on-disk layer is exactly the
        // built bytes — a reader never observes a partial or empty index.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let runner = FakeRunner {
            calls: Arc::new(AtomicUsize::new(0)),
            contents: "exact-layer-bytes",
        };
        let layer = ensure_layer(
            data_dir,
            "v",
            &spec(&["s"], &[]),
            4096,
            &VmArgs::default(),
            &runner,
            false,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&layer).unwrap(),
            "exact-layer-bytes"
        );
    }

    #[test]
    fn a_failed_build_publishes_nothing_and_parks_a_debug_disk() {
        // The headline #67 invariant: a failing step fails the create, nothing is published
        // under the success hash, and the half-provisioned disk is parked at `<hash>.failed`.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = FailingRunner {
            calls: calls.clone(),
        };
        let s = spec(&["apt-get install nope"], &[]);

        let err = ensure_layer(
            data_dir,
            "v",
            &s,
            4096,
            &VmArgs::default(),
            &runner,
            false,
            None,
        )
        .expect_err("a failed build must propagate the error");
        // The failure surfaces the failing step's command + exit code (carried in the error).
        let msg = format!("{err:#}");
        assert!(
            msg.contains("apt-get install nope"),
            "error names the step: {msg}"
        );
        assert!(msg.contains("exit 1"), "error carries the exit code: {msg}");

        let hash = cache_key("v", &s.steps, &s.allow, &s.secrets);
        assert!(
            !layer_path(data_dir, &hash).exists(),
            "nothing is published under the success hash after a failure"
        );
        assert!(
            failed_layer_path(data_dir, &hash).exists(),
            "the half-provisioned disk is parked at <hash>.failed"
        );
        // No temp index survives a failed build either.
        let leftovers: Vec<_> = std::fs::read_dir(provision_dir(data_dir).as_path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp index survives a failed build"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the build ran once");
    }

    #[test]
    fn a_subsequent_successful_build_overwrites_the_failed_disk() {
        // Lifecycle: once the spec builds cleanly, the stale `.failed` disk is dropped and the
        // success layer is published.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let s = spec(&["install toolchain"], &[]);
        let hash = cache_key("v", &s.steps, &s.allow, &s.secrets);

        // First: fail → debug disk parked, nothing published.
        let fail = FailingRunner {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        ensure_layer(
            data_dir,
            "v",
            &s,
            4096,
            &VmArgs::default(),
            &fail,
            false,
            None,
        )
        .unwrap_err();
        assert!(failed_layer_path(data_dir, &hash).exists());

        // Then: succeed → success layer published, stale `.failed` removed.
        let ok = FakeRunner {
            calls: Arc::new(AtomicUsize::new(0)),
            contents: "clean-layer",
        };
        let layer = ensure_layer(
            data_dir,
            "v",
            &s,
            4096,
            &VmArgs::default(),
            &ok,
            false,
            None,
        )
        .unwrap()
        .unwrap();
        assert!(Path::new(&layer).exists(), "the clean layer is published");
        assert!(
            !failed_layer_path(data_dir, &hash).exists(),
            "a successful build supersedes (removes) the stale failure disk"
        );
    }

    #[test]
    fn prune_failed_layers_removes_only_failure_disks() {
        // `dome prune` reclaims preserved failure disks but leaves published layers and locks.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let dir = provision_dir(data_dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("aaaa.failed"), "x").unwrap();
        std::fs::write(dir.join("bbbb.failed"), "y").unwrap();
        std::fs::write(dir.join("cccc.idx"), "live").unwrap();
        std::fs::write(dir.join("cccc.lock"), "").unwrap();

        let removed = prune_failed_layers(data_dir).unwrap();
        assert_eq!(removed, 2, "both failure disks are reclaimed");
        assert!(!dir.join("aaaa.failed").exists());
        assert!(!dir.join("bbbb.failed").exists());
        assert!(
            dir.join("cccc.idx").exists(),
            "published layers are left alone"
        );
        assert!(dir.join("cccc.lock").exists(), "locks are left alone");

        // Idempotent: a second prune with nothing to do reclaims zero.
        assert_eq!(prune_failed_layers(data_dir).unwrap(), 0);
    }

    #[test]
    fn resolve_failed_layer_handles_prefix_unique_and_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let dir = provision_dir(data_dir);
        std::fs::create_dir_all(&dir).unwrap();

        // None present → a clear error.
        assert!(resolve_failed_layer(data_dir, None).is_err());

        std::fs::write(dir.join("abc123.failed"), "x").unwrap();
        // Exactly one present, no selector → that one.
        let (h, path) = resolve_failed_layer(data_dir, None).unwrap();
        assert_eq!(h, "abc123");
        assert!(path.ends_with("abc123.failed"));
        // Prefix selects it.
        assert_eq!(
            resolve_failed_layer(data_dir, Some("abc")).unwrap().0,
            "abc123"
        );
        // A non-matching selector errors.
        assert!(resolve_failed_layer(data_dir, Some("zzz")).is_err());

        // Two present + no selector → ambiguous error; a disambiguating prefix still resolves.
        std::fs::write(dir.join("abd999.failed"), "y").unwrap();
        assert!(resolve_failed_layer(data_dir, None).is_err());
        assert_eq!(
            resolve_failed_layer(data_dir, Some("abc")).unwrap().0,
            "abc123"
        );
    }

    /// Write a published layer index (`<hash>.idx`) pinned to the given OS base version, so
    /// the lifecycle helpers (list/reclaim/prune) can be driven over a temp data_dir without a
    /// hypervisor. `base` of `None` writes a layer with no pinned base (version undeterminable).
    fn write_layer(data_dir: &str, hash: &str, base: Option<&str>) {
        let dir = provision_dir(data_dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut idx = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        idx.set_hash(0, "chunk".to_string());
        idx.fallback_path = base.map(|v| format!("{}/rootfs-{}.ext4", data_dir, v));
        idx.save(dir.join(format!("{hash}.idx")).to_str().unwrap())
            .unwrap();
    }

    #[test]
    fn rebuild_forces_a_fresh_build_overwriting_the_cached_layer() {
        // `--rebuild` (#69): a cached layer is normally served without rebuilding, but with
        // rebuild=true the build runs again and the new bytes atomically replace the cached
        // layer *in place* (same hash path), so a stale toolchain can be force-refreshed.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let s = spec(&["install toolchain"], &[]);

        let calls = Arc::new(AtomicUsize::new(0));
        let v1 = FakeRunner {
            calls: calls.clone(),
            contents: "toolchain-v1",
        };
        let first = ensure_layer(
            data_dir,
            "v",
            &s,
            4096,
            &VmArgs::default(),
            &v1,
            false,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "cold build ran once");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "toolchain-v1");

        // Without --rebuild, the cached layer is served and the build does NOT run again.
        let v_cached = FakeRunner {
            calls: calls.clone(),
            contents: "should-not-be-written",
        };
        let cached = ensure_layer(
            data_dir,
            "v",
            &s,
            4096,
            &VmArgs::default(),
            &v_cached,
            false,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(cached, first, "a cache hit resolves to the same path");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no rebuild on a plain hit");
        assert_eq!(std::fs::read_to_string(&cached).unwrap(), "toolchain-v1");

        // With --rebuild, the build runs again and overwrites the layer in place.
        let v2 = FakeRunner {
            calls: calls.clone(),
            contents: "toolchain-v2",
        };
        let rebuilt = ensure_layer(data_dir, "v", &s, 4096, &VmArgs::default(), &v2, true, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            rebuilt, first,
            "--rebuild overwrites the SAME hash path in place"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "--rebuild forces a fresh build"
        );
        assert_eq!(
            std::fs::read_to_string(&rebuilt).unwrap(),
            "toolchain-v2",
            "the cached layer now holds the freshly built bytes"
        );
        // No temp file survives the in-place overwrite.
        let leftovers: Vec<_> = std::fs::read_dir(provision_dir(data_dir).as_path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp file survives a --rebuild publish"
        );
    }

    #[test]
    fn layer_base_version_parses_the_pinned_rootfs_and_handles_unknowns() {
        let mut idx = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        // A standard versioned base resolves to its version.
        idx.fallback_path = Some("/data/rootfs-1.2.3.ext4".to_string());
        assert_eq!(layer_base_version(&idx), Some("1.2.3".to_string()));
        // No pinned base → undeterminable (None), so reclamation leaves it alone.
        idx.fallback_path = None;
        assert_eq!(layer_base_version(&idx), None);
        // A non-standard base path (e.g. an explicit --rootfs) is undeterminable too.
        idx.fallback_path = Some("/data/custom.ext4".to_string());
        assert_eq!(layer_base_version(&idx), None);
    }

    #[test]
    fn reclaim_stale_layers_removes_only_version_mismatched_layers() {
        // `dome upgrade` / `dome prune` reclaim (#69): a layer pinned to a base version other
        // than the installed one can never cache-hit again, so it is reclaimed; the matching
        // layer survives, and a layer whose version is undeterminable is never reclaimed on a
        // guess.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        write_layer(data_dir, "stale111", Some("1.0.0"));
        write_layer(data_dir, "current2", Some("2.0.0"));
        write_layer(data_dir, "unknown3", None);

        let removed = reclaim_stale_layers(data_dir, "2.0.0").unwrap();
        assert_eq!(removed, 1, "only the version-mismatched layer is reclaimed");
        assert!(
            !layer_path(data_dir, "stale111").exists(),
            "the stale layer is gone"
        );
        assert!(
            layer_path(data_dir, "current2").exists(),
            "the current-version layer survives"
        );
        assert!(
            layer_path(data_dir, "unknown3").exists(),
            "a layer with no determinable base version is never reclaimed on a guess"
        );

        // Idempotent: re-running against the same version reclaims nothing more.
        assert_eq!(reclaim_stale_layers(data_dir, "2.0.0").unwrap(), 0);
        // A missing provision dir is not an error.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            reclaim_stale_layers(empty.path().to_str().unwrap(), "2.0.0").unwrap(),
            0
        );
    }

    #[test]
    fn prune_all_layers_clears_every_published_layer_but_skips_temps_and_failed() {
        // `dome prune --provision` wholesale-clears the cache regardless of version, but must
        // not touch an in-flight build temp (`<hash>.<pid>.tmp.idx`) or a `.failed` debug disk
        // (reclaimed separately by prune_failed_layers).
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        write_layer(data_dir, "aaaa1111", Some("1.0.0"));
        write_layer(data_dir, "bbbb2222", Some("2.0.0"));
        let dir = provision_dir(data_dir);
        // An in-flight build temp and a parked failure disk must survive a wholesale clear.
        std::fs::write(dir.join("cccc3333.99999.tmp.idx"), "building").unwrap();
        std::fs::write(dir.join("dddd4444.failed"), "halfbuilt").unwrap();

        let removed = prune_all_layers(data_dir).unwrap();
        assert_eq!(removed, 2, "both published layers are cleared");
        assert!(!layer_path(data_dir, "aaaa1111").exists());
        assert!(!layer_path(data_dir, "bbbb2222").exists());
        assert!(
            dir.join("cccc3333.99999.tmp.idx").exists(),
            "an in-flight build temp is not mistaken for a published layer"
        );
        assert!(
            dir.join("dddd4444.failed").exists(),
            "a .failed debug disk is left for prune_failed_layers"
        );

        // Idempotent: nothing published remains (the temp and .failed are not published layers).
        assert_eq!(prune_all_layers(data_dir).unwrap(), 0);
    }

    #[test]
    fn published_layers_skips_build_temps() {
        // The cache view and reclamation must enumerate only real published layers, never an
        // in-flight `<hash>.<pid>.tmp.idx` (whose stem carries dots a hex hash never does).
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        write_layer(data_dir, "feedface", Some("1.0.0"));
        let dir = provision_dir(data_dir);
        std::fs::write(dir.join("feedface.12345.tmp.idx"), "building").unwrap();
        std::fs::write(dir.join("feedface.lock"), "").unwrap();
        std::fs::write(dir.join("feedface.failed"), "x").unwrap();

        let found = published_layers(data_dir).unwrap();
        let hashes: Vec<&str> = found.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(
            hashes,
            vec!["feedface"],
            "only the published layer is listed"
        );
    }

    /// Write a self-contained (depth-1) seed index at `path` with the given chunk hashes and
    /// pinned base, so the `--from` composition helpers (#70) can be driven without a hypervisor.
    fn write_seed_index(path: &str, chunks: &[(usize, &str)], base: Option<&str>) {
        let mut idx = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        for (i, h) in chunks {
            idx.set_hash(*i, h.to_string());
        }
        idx.fallback_path = base.map(|s| s.to_string());
        idx.save(path).unwrap();
    }

    #[test]
    fn seed_identity_fingerprints_resolved_content() {
        // The seed identity (#70) is a stable fingerprint of the seed's resolved content: the
        // same content yields the same identity, and changing any chunk OR the pinned base
        // (which decides what never-written chunks resolve to) changes it.
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.idx");
        let a = a.to_str().unwrap();
        let b = tmp.path().join("b.idx");
        let b = b.to_str().unwrap();

        write_seed_index(a, &[(0, "h0"), (1, "h1")], Some("/data/rootfs-1.0.0.ext4"));
        write_seed_index(b, &[(0, "h0"), (1, "h1")], Some("/data/rootfs-1.0.0.ext4"));
        assert_eq!(
            seed_identity(a).unwrap(),
            seed_identity(b).unwrap(),
            "identical content yields an identical identity"
        );
        assert!(
            seed_identity(a).unwrap().starts_with("seed:"),
            "a seed identity is prefixed so it can't collide with a bare VERSION string"
        );

        // A changed chunk changes the identity.
        write_seed_index(
            b,
            &[(0, "h0"), (1, "DIFFERENT")],
            Some("/data/rootfs-1.0.0.ext4"),
        );
        assert_ne!(seed_identity(a).unwrap(), seed_identity(b).unwrap());

        // A changed pinned base changes the identity too.
        write_seed_index(b, &[(0, "h0"), (1, "h1")], Some("/data/rootfs-2.0.0.ext4"));
        assert_ne!(seed_identity(a).unwrap(), seed_identity(b).unwrap());
    }

    #[test]
    fn cache_key_swaps_base_identity_for_the_seed() {
        // With `--from`, the layer keys against the seed's content identity in place of the CLI
        // VERSION — so a seeded build and a bare-base build of the same spec never collide.
        let tmp = tempfile::tempdir().unwrap();
        let seed = tmp.path().join("seed.idx");
        let seed = seed.to_str().unwrap();
        write_seed_index(seed, &[(0, "c0")], Some("/data/rootfs-1.0.0.ext4"));

        let steps = ["install".to_string()];
        let version_key = cache_key("0.6.3", &steps, &[], &[]);
        let seed_key = cache_key(&seed_identity(seed).unwrap(), &steps, &[], &[]);
        assert_ne!(
            version_key, seed_key,
            "a --from seed keys against the seed content, not the CLI VERSION"
        );
    }

    /// A runner that records the `seed` it was handed (the composition seam) and writes a
    /// sentinel layer, so the `--from` orchestration can be driven without a hypervisor.
    struct SeedRecordingRunner {
        calls: Arc<AtomicUsize>,
        seen: Arc<std::sync::Mutex<Vec<Option<String>>>>,
        contents: &'static str,
    }

    impl StepRunner for SeedRecordingRunner {
        fn build(
            &self,
            _spec: &ProvisionSpec,
            _disk_size_mb: u64,
            _env: &VmArgs,
            out_index: &str,
            _failed_index: Option<&str>,
            seed: Option<&str>,
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(seed.map(|s| s.to_string()));
            std::fs::write(out_index, self.contents).unwrap();
            Ok(())
        }
    }

    #[test]
    fn composing_on_a_seed_hits_on_same_seed_and_rebuilds_on_a_changed_seed() {
        // The #70 acceptance criteria end-to-end through `ensure_layer`: composing `--from` a
        // seed threads the seed to the build, same-seed-same-spec cache-hits, a changed seed
        // rebuilds, and a seeded layer is keyed apart from the bare-base build.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let seed1 = tmp.path().join("seed1.idx");
        let seed1 = seed1.to_str().unwrap();
        write_seed_index(seed1, &[(0, "c0")], Some("/data/rootfs-1.0.0.ext4"));

        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
        let runner = SeedRecordingRunner {
            calls: calls.clone(),
            seen: seen.clone(),
            contents: "composed-layer",
        };
        let s = spec(&["install"], &[]);

        // Cold build composing on seed1: the build runs once and is handed the seed.
        let first = ensure_layer(
            data_dir,
            "v",
            &s,
            4096,
            &VmArgs::default(),
            &runner,
            false,
            Some(seed1),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "cold composed build ran once"
        );
        assert_eq!(
            seen.lock().unwrap().last().unwrap().as_deref(),
            Some(seed1),
            "the seed is threaded to the build so steps compose on top of it"
        );

        // Same seed + spec → cache hit, no rebuild.
        let again = ensure_layer(
            data_dir,
            "v",
            &s,
            4096,
            &VmArgs::default(),
            &runner,
            false,
            Some(seed1),
        )
        .unwrap()
        .unwrap();
        assert_eq!(again, first, "same seed + spec resolves to the same layer");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "same seed + spec must not rebuild"
        );

        // A seed with different content → different hash → rebuild.
        let seed2 = tmp.path().join("seed2.idx");
        let seed2 = seed2.to_str().unwrap();
        write_seed_index(seed2, &[(0, "DIFFERENT")], Some("/data/rootfs-1.0.0.ext4"));
        let other = ensure_layer(
            data_dir,
            "v",
            &s,
            4096,
            &VmArgs::default(),
            &runner,
            false,
            Some(seed2),
        )
        .unwrap()
        .unwrap();
        assert_ne!(other, first, "a changed seed resolves to a different layer");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a changed seed rebuilds");

        // Composing on a seed is keyed apart from the bare-base (no-seed) build of the same spec.
        let bare = ensure_layer(
            data_dir,
            "v",
            &s,
            4096,
            &VmArgs::default(),
            &runner,
            false,
            None,
        )
        .unwrap()
        .unwrap();
        assert_ne!(
            bare, first,
            "a seeded layer is keyed differently from a bare-base layer"
        );
    }
}
