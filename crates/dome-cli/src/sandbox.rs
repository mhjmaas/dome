//! Persistent developer sandboxes. A sandbox is named, durable disk state layered
//! on the existing CAS engine: it boots from its own index (or lazily from a pinned
//! base on first use), runs interactively or one-off, and flatten-saves on clean
//! exit so the next invocation resumes exactly where it left off.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::assets;
use crate::cli::VmArgs;
use crate::config::{load_config, DomeConfig};
use crate::lock::{self, Lock};
use crate::session::{run_session, SaveTarget};
use crate::vm::{self, SandboxSource};

/// Entry point for `dome sandbox shell` and `dome sandbox run`. An empty `command`
/// (the `shell` case) defaults to an interactive `/bin/sh`.
pub(crate) fn run_sandbox(
    name_arg: Option<String>,
    vm_args: &VmArgs,
    command: Vec<String>,
    from: Option<&str>,
) -> Result<i32> {
    reject_direct_storage()?;

    let cfg = load_config(vm_args.config.as_deref())?;
    let cwd = std::env::current_dir()?;
    let name = resolve_name(name_arg.as_deref(), &cfg, &cwd)?;
    dome_vm::validate_checkpoint_name(&name).map_err(|e| anyhow::anyhow!(e))?;

    // Command resolution mirrors `dome run`: CLI args > config > default /bin/sh.
    let command = if !command.is_empty() {
        command
    } else if let Some(cfg_cmd) = cfg.command.clone() {
        cfg_cmd
    } else {
        vec!["/bin/sh".to_string()]
    };

    let data_dir = dome_vm::default_data_dir();
    let index_path = format!("{}/sandboxes/{}.idx", data_dir, name);
    let lock_path = PathBuf::from(format!("{}/sandboxes/{}.lock", data_dir, name));

    // Acquire the persistence lock. The first session to open a sandbox owns
    // persistence (read-write, saves on exit); any concurrent session boots as a
    // silent ephemeral fork that never saves back. A lock left by a crashed owner is
    // reclaimed automatically via a process-liveness check.
    let acquired = lock::acquire(&lock_path)?;
    let is_owner = matches!(acquired, Lock::Owner(_));

    let existed = Path::new(&index_path).exists();
    // `--from` is a creation-time seed: honored only when we own persistence and the
    // sandbox is absent. If it already exists, or a live session is already
    // creating/using it (we are a fork), seeding would clobber or race it — a hard
    // error, never a silent miss.
    let seed = match gate_from(from, existed, is_owner) {
        FromGate::NoSeed => None,
        FromGate::Seed(s) => Some(s),
        FromGate::Refused(reason) => return Err(collision_error(&name, reason, true)),
    };

    // Only the persistence owner may create or seed the index. A concurrent fork never
    // writes it (its changes are discarded), so it just boots from the current state.
    if is_owner {
        if let Some(seed_name) = seed {
            let seed_idx = resolve_seed_index(seed_name, &data_dir)?;
            seed_sandbox_index(&seed_idx, &index_path)?;
        }
    }

    // A freshly created (or seeded) sandbox now has an index; an existing one stays
    // pinned to the immutable base recorded in its index, regardless of any later
    // upgrade — so an OS upgrade never silently rebases (and corrupts) it.
    let now_exists = Path::new(&index_path).exists();
    let base_path = if now_exists {
        pinned_base_for_existing(&index_path)?
    } else {
        ensure_current_base(&data_dir, vm_args)?
    };

    let source = SandboxSource {
        index_path,
        base_path,
    };
    let prepared = vm::prepare_vm(vm_args, &cfg, None, Some(&source))?;

    match acquired {
        Lock::Owner(guard) => {
            if existed {
                eprintln!("dome: resuming sandbox '{}'", name);
            } else if let Some(seed_name) = seed {
                eprintln!("dome: creating sandbox '{}' from '{}'", name, seed_name);
            } else {
                eprintln!("dome: creating sandbox '{}'", name);
            }
            let result = run_session(&prepared, &command, &SaveTarget::Sandbox { name });
            // Hold the lock until the session (and its save) is fully done, then
            // release it explicitly so a crashed owner is distinguishable from a
            // clean exit (the latter leaves no lock behind).
            drop(guard);
            result
        }
        Lock::Fork => {
            eprintln!(
                "dome: sandbox '{}' is already open in another session — running as an \
                 ephemeral fork; changes made here will NOT be saved.",
                name
            );
            // No save target: the fork boots from the current saved state (or the base
            // image if the owner has not saved yet) and is fully functional, but it
            // never writes back to the sandbox index.
            run_session(&prepared, &command, &SaveTarget::None)
        }
    }
}

/// Entry point for `dome sandbox create`: materialize a sandbox's index without
/// booting a VM. With `--from`, the index is seeded from a checkpoint or another
/// sandbox; without it, a fresh index pinned to the current OS base is written. An
/// existing sandbox is never overwritten — `create` always hard-errors on collision.
pub(crate) fn create_sandbox(
    name_arg: Option<String>,
    vm_args: &VmArgs,
    from: Option<&str>,
) -> Result<()> {
    reject_direct_storage()?;

    let cfg = load_config(vm_args.config.as_deref())?;
    let cwd = std::env::current_dir()?;
    let name = resolve_name(name_arg.as_deref(), &cfg, &cwd)?;
    dome_vm::validate_checkpoint_name(&name).map_err(|e| anyhow::anyhow!(e))?;

    let data_dir = dome_vm::default_data_dir();
    let index_path = format!("{}/sandboxes/{}.idx", data_dir, name);
    let lock_path = PathBuf::from(format!("{}/sandboxes/{}.lock", data_dir, name));

    // Materializing the index is a persistence write, so take the persistence lock —
    // the same one-writer rule `shell`/`run` follow. A fork outcome means a live
    // session already owns this name (it may be lazily creating the index right now):
    // refuse rather than race its save and silently lose one of the two writes. The
    // guard is held until this function returns and released on any exit path, so a
    // freshly created sandbox is left idle (unlocked), not wedged.
    let _guard = match lock::acquire(&lock_path)? {
        Lock::Owner(g) => g,
        Lock::Fork => return Err(collision_error(&name, Collision::InUse, from.is_some())),
    };

    if Path::new(&index_path).exists() {
        // `create` never resumes — an on-disk index is a hard collision.
        return Err(collision_error(&name, Collision::Exists, from.is_some()));
    }

    match from {
        Some(seed_name) => {
            let seed_idx = resolve_seed_index(seed_name, &data_dir)?;
            seed_sandbox_index(&seed_idx, &index_path)?;
            eprintln!("dome: created sandbox '{}' from '{}'", name, seed_name);
        }
        None => {
            let base_path = ensure_current_base(&data_dir, vm_args)?;
            let disk_size_mb = vm::resolve_session(vm_args.disk_size, cfg.disk_size, 4096);
            materialize_from_base(&index_path, &base_path, disk_size_mb)?;
            eprintln!("dome: created sandbox '{}'", name);
        }
    }
    Ok(())
}

/// Why a sandbox cannot be created or seeded right now. The two states are kept
/// distinct because they have different remedies, but both are reported through
/// [`collision_error`] so every command phrases them identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Collision {
    /// A persisted index is already on disk. Remedy: `dome sandbox rm`, then recreate.
    Exists,
    /// A live session owns persistence for this name (it may be lazily creating the
    /// index right now). Remedy: wait for that session to finish, then retry.
    InUse,
}

/// The outcome of gating `--from` for a session: a no-op, a seed to materialize from,
/// or a refusal carrying the reason. Pure (no name, no message) so the decision can be
/// tested in isolation; the caller attaches the sandbox name via [`collision_error`].
#[derive(Debug, PartialEq, Eq)]
enum FromGate<'a> {
    /// No `--from` was given.
    NoSeed,
    /// Materialize the new sandbox from this seed name.
    Seed(&'a str),
    /// `--from` cannot be honored; report it with this reason.
    Refused(Collision),
}

/// Build the canonical collision error for a sandbox, shared by every command so the
/// phrasing, the included name, and the remedy are identical everywhere. When `--from`
/// was involved, a clause clarifying what `--from` does is appended uniformly.
fn collision_error(name: &str, reason: Collision, had_from: bool) -> anyhow::Error {
    let (state, remedy) = match reason {
        Collision::Exists => (
            "already exists",
            format!("Remove it first with `dome sandbox rm {name}` to recreate it."),
        ),
        Collision::InUse => (
            "is currently in use by another session",
            "Wait for it to finish, then try again.".to_string(),
        ),
    };
    let from_clause = if had_from {
        " --from only seeds a brand-new sandbox; it will not re-seed, clobber, or race \
         an existing or in-use one."
    } else {
        ""
    };
    anyhow::anyhow!("sandbox '{name}' {state}.{from_clause} {remedy}")
}

/// Decide what `--from` means for this session, given whether the named sandbox's
/// index already exists and whether this session owns persistence (vs. running as a
/// fork). `--from` is a *creation-time* seed honored ONLY by the persistence owner of
/// a not-yet-existing sandbox. Re-seeding an existing sandbox — or a sandbox a live
/// session is already creating/using (we are a fork) — would clobber or race it, so
/// both are refused rather than silently missed.
fn gate_from(from: Option<&str>, sandbox_exists: bool, is_owner: bool) -> FromGate<'_> {
    match from {
        None => FromGate::NoSeed,
        Some(_) if sandbox_exists => FromGate::Refused(Collision::Exists),
        Some(_) if !is_owner => FromGate::Refused(Collision::InUse),
        Some(seed) => FromGate::Seed(seed),
    }
}

/// Resolve a `--from` seed name to the CAS index it should be materialized from.
/// A seed may be a checkpoint or another sandbox; checkpoints are checked first
/// (mirroring how `--from` resolves for `dome run`/`checkpoint create`), then
/// sandboxes. Errors clearly when neither exists rather than silently falling back
/// to the base image. Legacy `.ext4` checkpoints have no index to seed from and are
/// not a valid CAS seed source.
fn resolve_seed_index(seed: &str, data_dir: &str) -> Result<String> {
    dome_vm::validate_checkpoint_name(seed).map_err(|e| anyhow::anyhow!(e))?;
    let candidates = [
        format!("{}/checkpoints/{}.idx", data_dir, seed),
        format!("{}/sandboxes/{}.idx", data_dir, seed),
    ];
    for candidate in &candidates {
        if Path::new(candidate).exists() {
            return Ok(candidate.clone());
        }
    }
    bail!(
        "seed '{}' not found: no checkpoint or sandbox index exists to seed from \
         (looked in checkpoints/ and sandboxes/). Persistent sandboxes require a CAS \
         index, so legacy .ext4 checkpoints cannot be used as a seed.",
        seed
    )
}

/// Write a fresh, depth-1 sandbox index pinned to `base_path`, without booting a VM.
/// Every chunk starts ZERO and resolves through the base — exactly the disk state a
/// first boot-from-base would observe before any write — so a later `shell`/`run`
/// resumes it correctly. The index is written atomically (temp + rename).
fn materialize_from_base(index_path: &str, base_path: &str, disk_size_mb: u64) -> Result<()> {
    let mut idx = dome_store::ChunkIndex::new(disk_size_mb * 1024 * 1024);
    idx.fallback_path = Some(base_path.to_string());
    idx.save_atomic(index_path)?;
    Ok(())
}

/// Seed a new sandbox index from the source's CAS index. Because chunks live in a
/// single global deduplicated pool, copying only the (resolved) hash references hands
/// the new sandbox the source's exact content for free — no chunk data is moved. The
/// source's pinned base (`fallback_path`) is inherited, so a fork of an older-OS
/// sandbox keeps resolving its never-written chunks through the base it was actually
/// built on.
///
/// The source's parent chain is flattened into a self-contained, depth-1 index so the
/// new sandbox never depends on the source's parent index files: removing the source
/// (or its parents) later cannot silently corrupt the seeded sandbox, and its resume
/// reads stay shallow. The source index is left untouched; the write is atomic.
fn seed_sandbox_index(source_index: &str, dest_index: &str) -> Result<()> {
    let flat = dome_store::ChunkIndex::flatten_chain(source_index)?;
    flat.save_atomic(dest_index)?;
    Ok(())
}

/// Persistence requires CAS — there is no index to save in direct mode.
fn reject_direct_storage() -> Result<()> {
    if std::env::var("DOME_STORAGE").unwrap_or_default() == "direct" {
        bail!(
            "persistent sandboxes require CAS storage, but DOME_STORAGE=direct is set. \
             Unset it to use `dome sandbox` (there is no index to save in direct mode)."
        );
    }
    Ok(())
}

/// Resolve the sandbox name: explicit argument → `dome.json` `sandbox` field →
/// a slug derived from the current working directory, in that order.
pub(crate) fn resolve_name(explicit: Option<&str>, cfg: &DomeConfig, cwd: &Path) -> Result<String> {
    if let Some(name) = explicit {
        if name.is_empty() {
            bail!("sandbox name cannot be empty");
        }
        return Ok(name.to_string());
    }
    if let Some(name) = cfg.sandbox.as_deref() {
        if !name.is_empty() {
            return Ok(name.to_string());
        }
    }
    let base = cwd.file_name().and_then(|s| s.to_str()).unwrap_or_default();
    let slug = slugify(base);
    if slug.is_empty() {
        bail!(
            "could not derive a sandbox name from the current directory '{}'. \
             Pass an explicit name or set \"sandbox\" in dome.json.",
            cwd.display()
        );
    }
    Ok(slug)
}

/// Lowercase, replace runs of non-alphanumeric characters with a single '-', and
/// trim leading/trailing '-'.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Resolve the immutable, version-addressed base image for a brand-new sandbox: the
/// rootfs of the currently installed OS version. The sandbox's never-written chunks
/// resolve through this file, and because it is stored under a per-version filename
/// it is never overwritten by a later upgrade.
fn ensure_current_base(data_dir: &str, vm_args: &VmArgs) -> Result<String> {
    // Make sure the OS image is present before we pin to it.
    if vm_args.kernel.is_none()
        && vm_args.rootfs.is_none()
        && vm_args.initrd.is_none()
        && !assets::assets_ready(data_dir)
    {
        assets::download_os_image(data_dir)?;
    }

    let base_path = vm_args.rootfs.clone().unwrap_or_else(|| {
        let version = assets::installed_version(data_dir)
            .unwrap_or_else(|| assets::CURRENT_VERSION.to_string());
        assets::versioned_rootfs_path(data_dir, &version)
    });
    if !Path::new(&base_path).exists() {
        bail!(
            "Rootfs not found at {}. Run `dome init` to download.",
            base_path
        );
    }
    Ok(base_path)
}

/// Resolve the pinned base image for an existing sandbox from the base recorded in
/// its index. If that base image is no longer available (e.g. its OS version was
/// reclaimed), error clearly rather than silently rebasing onto a different version,
/// which would corrupt the filesystem.
fn pinned_base_for_existing(index_path: &str) -> Result<String> {
    let idx = dome_store::ChunkIndex::load(index_path)?;
    let base = idx.fallback_path.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "sandbox index '{}' records no pinned base image; it may be corrupt",
            index_path
        )
    })?;
    if !Path::new(&base).exists() {
        bail!(
            "this sandbox's pinned OS base image is no longer available: {}\n\
             The OS version it was created on has been removed. dome will not silently \
             migrate it to a different base (that would corrupt the filesystem). \
             Restore that base image or recreate the sandbox.",
            base
        );
    }
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(sandbox: Option<&str>) -> DomeConfig {
        DomeConfig {
            sandbox: sandbox.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn explicit_name_wins() {
        let cfg = cfg_with(Some("from-config"));
        let cwd = Path::new("/Users/dev/my-project");
        let name = resolve_name(Some("explicit"), &cfg, cwd).unwrap();
        assert_eq!(name, "explicit");
    }

    #[test]
    fn config_field_beats_cwd_slug() {
        let cfg = cfg_with(Some("from-config"));
        let cwd = Path::new("/Users/dev/my-project");
        let name = resolve_name(None, &cfg, cwd).unwrap();
        assert_eq!(name, "from-config");
    }

    #[test]
    fn falls_back_to_cwd_slug() {
        let cfg = cfg_with(None);
        let cwd = Path::new("/Users/dev/My Project");
        let name = resolve_name(None, &cfg, cwd).unwrap();
        assert_eq!(name, "my-project");
    }

    #[test]
    fn empty_config_sandbox_field_falls_through_to_slug() {
        let cfg = cfg_with(Some(""));
        let cwd = Path::new("/Users/dev/webapp");
        let name = resolve_name(None, &cfg, cwd).unwrap();
        assert_eq!(name, "webapp");
    }

    #[test]
    fn slugify_collapses_and_trims() {
        assert_eq!(slugify("My Project"), "my-project");
        assert_eq!(slugify("--weird__name!!"), "weird-name");
        assert_eq!(slugify("CamelCase"), "camelcase");
        assert_eq!(slugify("a.b.c"), "a-b-c");
    }

    #[test]
    fn existing_sandbox_resolves_its_recorded_pinned_base() {
        let tmp = tempfile::tempdir().unwrap();
        // A base image file for the version this sandbox was created on.
        let base = tmp.path().join("rootfs-1.0.0.ext4");
        std::fs::write(&base, b"base").unwrap();

        // An index recording that base as its fallback (as flatten-save does).
        let idx_path = tmp.path().join("foo.idx");
        let mut idx = dome_store::ChunkIndex::new(256 * 1024);
        idx.fallback_path = Some(base.to_string_lossy().to_string());
        idx.save(idx_path.to_str().unwrap()).unwrap();

        let resolved = pinned_base_for_existing(idx_path.to_str().unwrap()).unwrap();
        assert_eq!(resolved, base.to_string_lossy());
    }

    #[test]
    fn existing_sandbox_with_missing_base_errors_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let idx_path = tmp.path().join("foo.idx");
        let mut idx = dome_store::ChunkIndex::new(256 * 1024);
        idx.fallback_path = Some(
            tmp.path()
                .join("rootfs-9.9.9.ext4")
                .to_string_lossy()
                .to_string(),
        );
        idx.save(idx_path.to_str().unwrap()).unwrap();

        let err = pinned_base_for_existing(idx_path.to_str().unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("rootfs-9.9.9.ext4") && msg.to_lowercase().contains("base"),
            "error should name the unavailable base and not silently migrate; got: {}",
            msg
        );
    }

    #[test]
    fn from_is_honored_when_sandbox_is_absent() {
        // A brand-new sandbox we own: --from names the seed to materialize it from.
        assert_eq!(
            gate_from(Some("base-ckpt"), false, true),
            FromGate::Seed("base-ckpt")
        );
    }

    #[test]
    fn no_from_is_a_no_op_regardless_of_existence_or_ownership() {
        for &exists in &[false, true] {
            for &owner in &[false, true] {
                assert_eq!(
                    gate_from(None, exists, owner),
                    FromGate::NoSeed,
                    "no --from must be a no-op (exists={}, owner={})",
                    exists,
                    owner
                );
            }
        }
    }

    #[test]
    fn from_on_an_existing_sandbox_is_refused_as_exists() {
        // Re-seeding a live sandbox would clobber it — must be refused, not silent,
        // whether we'd own it or are a fork.
        for &owner in &[false, true] {
            assert_eq!(
                gate_from(Some("base-ckpt"), true, owner),
                FromGate::Refused(Collision::Exists),
                "an on-disk index must refuse --from as Exists (owner={})",
                owner
            );
        }
    }

    #[test]
    fn from_on_a_forked_sandbox_is_refused_as_in_use() {
        // A live session is already creating/using this name (we are a fork) and the
        // index does not exist yet. `--from` must NOT be silently dropped (which would
        // boot from base, ignoring the user's seed): it is refused as in-use.
        assert_eq!(
            gate_from(Some("base-ckpt"), false, false),
            FromGate::Refused(Collision::InUse)
        );
    }

    #[test]
    fn collision_error_is_consistent_across_states_and_from() {
        // Every collision reads as: sandbox '<name>' <state>. [<--from clause>] <remedy>.
        // The name and a remedy are always present; the --from clause appears only when
        // --from was involved; the two states stay distinguishable by their wording.
        let exists = collision_error("web", Collision::Exists, false).to_string();
        assert!(exists.contains("'web'") && exists.contains("already exists"));
        assert!(
            exists.contains("dome sandbox rm web"),
            "missing remedy: {}",
            exists
        );
        assert!(
            !exists.contains("--from"),
            "no --from clause expected: {}",
            exists
        );

        let exists_from = collision_error("web", Collision::Exists, true).to_string();
        assert!(exists_from.contains("already exists") && exists_from.contains("--from"));
        assert!(exists_from.contains("dome sandbox rm web"));

        let in_use = collision_error("web", Collision::InUse, false).to_string();
        assert!(in_use.contains("'web'") && in_use.contains("in use"));
        assert!(in_use.contains("Wait"), "missing remedy: {}", in_use);
        assert!(
            !in_use.contains("--from"),
            "no --from clause expected: {}",
            in_use
        );

        let in_use_from = collision_error("web", Collision::InUse, true).to_string();
        assert!(in_use_from.contains("in use") && in_use_from.contains("--from"));
    }

    #[test]
    fn seed_resolves_a_checkpoint_index() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let ckpt = format!("{}/checkpoints/base.idx", data_dir);
        std::fs::create_dir_all(format!("{}/checkpoints", data_dir)).unwrap();
        std::fs::write(&ckpt, b"idx").unwrap();

        let resolved = resolve_seed_index("base", data_dir).unwrap();
        assert_eq!(resolved, ckpt);
    }

    #[test]
    fn seed_resolves_a_sandbox_index() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let other = format!("{}/sandboxes/other.idx", data_dir);
        std::fs::create_dir_all(format!("{}/sandboxes", data_dir)).unwrap();
        std::fs::write(&other, b"idx").unwrap();

        let resolved = resolve_seed_index("other", data_dir).unwrap();
        assert_eq!(resolved, other);
    }

    #[test]
    fn seed_that_does_not_exist_errors_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let err = resolve_seed_index("ghost", data_dir).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("ghost"),
            "error should name the missing seed; got: {}",
            msg
        );
    }

    #[test]
    fn materialize_from_base_writes_a_fresh_pinned_index() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("rootfs-1.0.0.ext4");
        std::fs::write(&base, vec![0u8; 1024]).unwrap();
        let idx_path = tmp.path().join("nested/sb.idx");

        materialize_from_base(idx_path.to_str().unwrap(), base.to_str().unwrap(), 64).unwrap();

        let idx = dome_store::ChunkIndex::load(idx_path.to_str().unwrap()).unwrap();
        // Pinned to the base it was created on, with the requested disk size.
        assert_eq!(idx.fallback_path.as_deref(), Some(base.to_str().unwrap()));
        assert_eq!(idx.disk_size(), 64 * 1024 * 1024);
        // Depth-1: a freshly materialized sandbox has no parent chain.
        assert!(idx.parent_path.is_none());
        // Nothing written yet: every chunk resolves through the base (all ZERO).
        let non_zero = (0..idx.num_chunks())
            .filter(|&i| idx.get_hash(i).map(|h| h != "ZERO").unwrap_or(false))
            .count();
        assert_eq!(non_zero, 0);
    }

    #[test]
    fn seeding_inherits_the_source_content_and_pinned_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("rootfs-1.0.0.ext4");
        std::fs::write(&base, vec![0u8; 1024]).unwrap();

        // A seed index pinned to `base` with one written (non-ZERO) chunk.
        let seed_path = tmp.path().join("seed.idx");
        let mut seed = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        seed.fallback_path = Some(base.to_string_lossy().to_string());
        seed.set_hash(2, "deadbeef".to_string());
        seed.save(seed_path.to_str().unwrap()).unwrap();

        // Seeding a new sandbox from it.
        let dst = tmp.path().join("nested/sb.idx");
        seed_sandbox_index(seed_path.to_str().unwrap(), dst.to_str().unwrap()).unwrap();

        let copied = dome_store::ChunkIndex::load(dst.to_str().unwrap()).unwrap();
        // Same content (chunk references), same disk size, same pinned base.
        assert_eq!(copied.get_hash(2), Some("deadbeef"));
        assert_eq!(copied.disk_size(), seed.disk_size());
        assert_eq!(copied.fallback_path.as_deref(), base.to_str());
        // The original seed is untouched.
        assert!(seed_path.exists());
    }

    #[test]
    fn seeding_flattens_a_chained_source_into_a_self_contained_index() {
        // Seeding from a *chained* checkpoint (created with `--from`) must not leave the
        // sandbox depending on the source's parent files: that dependency would silently
        // corrupt reads if the source were removed before the sandbox's first save.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("rootfs-1.0.0.ext4");
        std::fs::write(&base, vec![0u8; 1024]).unwrap();

        // A parent checkpoint (one written chunk), pinned to the base.
        let parent_path = tmp.path().join("parent.idx");
        let mut parent = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        parent.fallback_path = Some(base.to_string_lossy().to_string());
        parent.set_hash(1, "parentchunk".to_string());
        parent.save(parent_path.to_str().unwrap()).unwrap();

        // A child checkpoint chained onto it, adding its own chunk.
        let child_path = tmp.path().join("child.idx");
        let mut child = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        child.parent_path = Some(parent_path.to_string_lossy().to_string());
        child.fallback_path = Some(base.to_string_lossy().to_string());
        child.set_hash(2, "childchunk".to_string());
        child.save(child_path.to_str().unwrap()).unwrap();

        let dst = tmp.path().join("nested/sb.idx");
        seed_sandbox_index(child_path.to_str().unwrap(), dst.to_str().unwrap()).unwrap();

        // Remove the entire source chain: the seeded sandbox must remain intact.
        std::fs::remove_file(&child_path).unwrap();
        std::fs::remove_file(&parent_path).unwrap();

        let copied = dome_store::ChunkIndex::load(dst.to_str().unwrap()).unwrap();
        // Depth-1: no surviving parent dependency.
        assert!(copied.parent_path.is_none());
        // Both the child's own chunk and the inherited parent chunk resolve in place.
        assert_eq!(copied.get_hash(2), Some("childchunk"));
        assert_eq!(copied.get_hash(1), Some("parentchunk"));
        // Pinned base preserved.
        assert_eq!(copied.fallback_path.as_deref(), base.to_str());
    }

    #[test]
    fn seeding_from_a_corrupt_chain_errors_rather_than_silently_dropping_content() {
        // A source whose parent link is missing is corrupt; seeding must hard-error
        // instead of silently producing an index that resolves through the base.
        let tmp = tempfile::tempdir().unwrap();
        let child_path = tmp.path().join("child.idx");
        let mut child = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        child.parent_path = Some(
            tmp.path()
                .join("vanished-parent.idx")
                .to_string_lossy()
                .to_string(),
        );
        child.set_hash(2, "childchunk".to_string());
        child.save(child_path.to_str().unwrap()).unwrap();

        let dst = tmp.path().join("sb.idx");
        let err =
            seed_sandbox_index(child_path.to_str().unwrap(), dst.to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("parent"),
            "error should name the missing parent; got: {}",
            err
        );
        assert!(
            !dst.exists(),
            "a failed seed must not leave an index behind"
        );
    }

    #[test]
    fn direct_storage_is_rejected() {
        // Guard against leaking env state across tests in the same process.
        let prev = std::env::var("DOME_STORAGE").ok();
        std::env::set_var("DOME_STORAGE", "direct");
        let err = reject_direct_storage().unwrap_err();
        assert!(err.to_string().contains("DOME_STORAGE=direct"));
        match prev {
            Some(v) => std::env::set_var("DOME_STORAGE", v),
            None => std::env::remove_var("DOME_STORAGE"),
        }
    }

    #[test]
    fn cas_storage_is_accepted() {
        let prev = std::env::var("DOME_STORAGE").ok();
        std::env::remove_var("DOME_STORAGE");
        assert!(reject_direct_storage().is_ok());
        if let Some(v) = prev {
            std::env::set_var("DOME_STORAGE", v);
        }
    }
}
