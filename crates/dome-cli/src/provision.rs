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
use crate::sandbox_config::ProvisionSpec;

/// Compute the content hash that keys a provisioned layer:
/// `sha256(base identity ‖ normalized steps ‖ provision.allow)`.
///
/// The base identity is the CLI VERSION string (the bare base image is version-pinned), so a
/// new dome release rebuilds layers rather than serving one baked against an older base.
/// Steps are framed length-prefixed and hashed in order, so the key is **order-sensitive** on
/// steps; the allow-list is framed the same way, so the key is **sensitive to `allow`** too.
/// (Seed identity for `--from` composition is #E; the secret mapping joins the key in #C.)
pub(crate) fn cache_key(version: &str, steps: &[String], allow: &[String]) -> String {
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
    format!("{:x}", hasher.finalize())
}

/// Runs the provisioning steps and writes the resulting CAS index to `out_index`. Injected so
/// the orchestration (key/lookup/lock/publish) is testable without a hypervisor.
pub(crate) trait StepRunner {
    /// Build a provisioned layer: run `spec.steps` (as root, sequentially, stop-on-first-
    /// failure, each via `sh -c`, project dir NOT mounted, network narrowed by `spec.allow`)
    /// starting from the bare base, and write the resulting CAS index to `out_index`.
    fn build(
        &self,
        spec: &ProvisionSpec,
        disk_size_mb: u64,
        env: &VmArgs,
        out_index: &str,
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
    ) -> Result<()> {
        crate::vm::build_provision_layer(spec, disk_size_mb, env, out_index)
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

    let hash = cache_key(version, &spec.steps, &spec.allow);
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
    let _ = std::fs::remove_file(&tmp_path);

    eprintln!("dome: provisioning (first run for this spec)…");
    let build = runner.build(spec, disk_size_mb, env, &tmp);
    if let Err(e) = build {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e).context("provisioning failed");
    }

    std::fs::rename(&tmp_path, &idx_path).with_context(|| {
        format!(
            "publishing provisioned layer {} -> {}",
            tmp_path.display(),
            idx_path.display()
        )
    })?;
    eprintln!(
        "dome: provisioned, cached ({}…)",
        &hash[..hash.len().min(12)]
    );
    Ok(Some(path_string(&idx_path)?))
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
        }
    }

    #[test]
    fn cache_key_is_deterministic() {
        let s = vec!["apt-get install -y nodejs".to_string()];
        let a = vec!["deb.debian.org".to_string()];
        assert_eq!(cache_key("0.6.3", &s, &a), cache_key("0.6.3", &s, &a));
    }

    #[test]
    fn cache_key_is_order_sensitive_on_steps() {
        let a = ["one".to_string(), "two".to_string()];
        let b = ["two".to_string(), "one".to_string()];
        assert_ne!(
            cache_key("v", &a, &[]),
            cache_key("v", &b, &[]),
            "reordering steps must change the key (order matters for a build)"
        );
    }

    #[test]
    fn cache_key_is_sensitive_to_allow_and_to_framing() {
        let steps = ["s".to_string()];
        assert_ne!(
            cache_key("v", &steps, &["a.com".to_string()]),
            cache_key("v", &steps, &["b.com".to_string()]),
            "changing the allow-list must change the key"
        );
        // Length-prefix framing means a moved boundary cannot collide: ["ab","c"] != ["a","bc"].
        let x = ["ab".to_string(), "c".to_string()];
        let y = ["a".to_string(), "bc".to_string()];
        assert_ne!(cache_key("v", &x, &[]), cache_key("v", &y, &[]));
    }

    #[test]
    fn cache_key_is_sensitive_to_base_version() {
        let s = ["s".to_string()];
        assert_ne!(
            cache_key("0.6.3", &s, &[]),
            cache_key("0.7.0", &s, &[]),
            "the CLI VERSION is the base identity; bumping it must rebuild the layer"
        );
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
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(out_index, self.contents).unwrap();
            Ok(())
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
        let hash = cache_key("v", &s.steps, &s.allow);
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
}
