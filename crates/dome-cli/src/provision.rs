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
/// even available here: a [`SecretSpec`] carries the mapping only). (Seed identity for `--from`
/// composition is #E.)
pub(crate) fn cache_key(
    version: &str,
    steps: &[String],
    allow: &[String],
    secrets: &[SecretSpec],
) -> String {
    let mut hasher = Sha256::new();
    // Length-prefix every field so no concatenation of inputs can collide with another
    // (e.g. steps ["ab","c"] must not hash the same as ["a","bc"]).
    hasher.update((version.len() as u64).to_le_bytes());
    hasher.update(version.as_bytes());
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

/// Runs the provisioning steps and writes the resulting CAS index to `out_index`. Injected so
/// the orchestration (key/lookup/lock/publish) is testable without a hypervisor.
pub(crate) trait StepRunner {
    /// Build a provisioned layer: run `spec.steps` (as root, sequentially, stop-on-first-
    /// failure, each via `sh -c`, project dir NOT mounted, network narrowed by `spec.allow`)
    /// starting from the bare base, and write the resulting CAS index to `out_index`.
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
    ) -> Result<()> {
        crate::vm::build_provision_layer(spec, disk_size_mb, env, out_index, failed_index)
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
pub(crate) fn ensure_layer(
    data_dir: &str,
    version: &str,
    spec: &ProvisionSpec,
    disk_size_mb: u64,
    env: &VmArgs,
    runner: &dyn StepRunner,
) -> Result<Option<String>> {
    if spec.steps.is_empty() {
        return Ok(None);
    }

    let hash = cache_key(version, &spec.steps, &spec.allow, &spec.secrets);
    let idx_path = layer_path(data_dir, &hash);

    // Fast path: a published layer is served instantly, without taking the lock.
    if idx_path.exists() {
        return Ok(Some(path_string(&idx_path)?));
    }

    let dir = provision_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating provision dir {}", dir.display()))?;

    // Serialize builders of this exact spec: the loser blocks here until the winner publishes,
    // then falls through to the post-lock cache-hit check below.
    let _lock = HashLock::acquire(&dir.join(format!("{hash}.lock")))?;

    // Re-check under the lock: a concurrent builder may have published while we blocked.
    if idx_path.exists() {
        return Ok(Some(path_string(&idx_path)?));
    }

    // Build into a per-process temp file, then publish atomically so a reader never sees a
    // partial `.idx`. A failed build leaves no temp behind to be mistaken for a layer.
    let tmp_path = dir.join(format!("{hash}.{}.tmp.idx", std::process::id()));
    let tmp = path_string(&tmp_path)?;
    let failed_path = failed_layer_path(data_dir, &hash);
    let failed = path_string(&failed_path)?;
    let _ = std::fs::remove_file(&tmp_path);

    eprintln!("dome: provisioning (first run for this spec)…");
    let build = runner.build(spec, disk_size_mb, env, &tmp, Some(&failed));
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
        "dome: provisioned, cached ({}…)",
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
        let first = ensure_layer(data_dir, "v", &s, 4096, &VmArgs::default(), &runner)
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
        let second = ensure_layer(data_dir, "v", &s, 4096, &VmArgs::default(), &runner)
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
                    ensure_layer(&data_dir, "v", &s, 4096, &VmArgs::default(), &runner)
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

        let err = ensure_layer(data_dir, "v", &s, 4096, &VmArgs::default(), &runner)
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
        ensure_layer(data_dir, "v", &s, 4096, &VmArgs::default(), &fail).unwrap_err();
        assert!(failed_layer_path(data_dir, &hash).exists());

        // Then: succeed → success layer published, stale `.failed` removed.
        let ok = FakeRunner {
            calls: Arc::new(AtomicUsize::new(0)),
            contents: "clean-layer",
        };
        let layer = ensure_layer(data_dir, "v", &s, 4096, &VmArgs::default(), &ok)
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
}
