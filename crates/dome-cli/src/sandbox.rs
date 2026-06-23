//! Persistent developer sandboxes. A sandbox is named, durable disk state layered
//! on the existing CAS engine: it boots from its own index (or lazily from a pinned
//! base on first use), runs interactively or one-off, and flatten-saves on clean
//! exit so the next invocation resumes exactly where it left off.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use std::io::IsTerminal;

use crate::assets;
use crate::checkpoint;
use crate::cli::VmArgs;
use crate::config::{load_config, DomeConfig};
use crate::lock::{self, Lock};
use crate::sandbox_config::SandboxConfig;
use crate::vm::{self, SandboxSource};
use crate::worker;

/// Entry point for `dome sandbox shell` and `dome sandbox run`. An empty `command`
/// (the `shell` case) defaults to an interactive `/bin/sh`.
///
/// Unlike the old in-process model, the VM is owned by a persistent per-sandbox worker:
/// this routes through domed (auto-spawned) to ensure the worker exists — cold-booting
/// the VM from its last saved index if it is stopped — then streams the interactive
/// session **directly** to the worker (domed is not in the byte path). The VM stays
/// alive after this returns, so a later `shell`/`run` attaches to the same live VM.
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

    // A TTY on stdin means an interactive PTY session (raw mode, resize, signals);
    // otherwise the command is piped non-interactively. Mirrors the original
    // `run_command` choice so behaviour is unchanged from the user's point of view.
    let tty = std::io::stdin().is_terminal();

    let data_dir = dome_vm::default_data_dir();

    // The boot spec is consumed by the worker only on a cold boot; it captures exactly
    // what this invocation would have booted (resolved name, seed, cwd, and VM flags).
    let boot = worker::BootSpec::new(&name, from, &cwd, vm_args)?;
    let attach = crate::daemon::attach_via_daemon(&data_dir, &name, boot.to_value()?)?;

    // Config flags only take effect on a cold boot; warn (don't silently mislead) when
    // they were passed but the VM was already running and kept its original config. When
    // the live config is readable, name the specific conflicting live values; otherwise
    // fall back to a generic notice.
    if !attach.cold_booted && vm_flags_specified(vm_args, from) {
        warn_ignored_flags(&data_dir, &name, vm_args, from);
    }

    worker::attach_and_relay(&attach, &command, tty)
}

/// Whether the user passed any boot-affecting flag, used to decide if attaching to an
/// already-running VM should warn that those flags are being ignored.
fn vm_flags_specified(vm_args: &VmArgs, from: Option<&str>) -> bool {
    from.is_some()
        || vm_args.cpus.is_some()
        || vm_args.memory.is_some()
        || vm_args.disk_size.is_some()
        || vm_args.allow_net
        || vm_args.allow_host_writes
        || !vm_args.port.is_empty()
        || !vm_args.mount.is_empty()
        || !vm_args.secret.is_empty()
        || !vm_args.allow_host.is_empty()
        || !vm_args.expose_host.is_empty()
}

/// Warn that boot flags passed to an already-running sandbox are ignored. When the worker's
/// live config is readable, name the specific conflicting live values (e.g. `--cpus 8 (live:
/// 2)`) so the user sees exactly what was kept; otherwise emit a generic notice. `--from` is
/// always a creation-time-only seed, so it is called out separately when present.
fn warn_ignored_flags(data_dir: &str, name: &str, vm_args: &VmArgs, from: Option<&str>) {
    let conflicts = SandboxConfig::load_live(data_dir, name)
        .map(|live| live.conflicts(vm_args))
        .unwrap_or_default();

    if conflicts.is_empty() && from.is_none() {
        eprintln!(
            "dome: sandbox '{}' is already running — its existing config is kept; the \
             flags passed here are ignored. Stop it first to boot with new settings.",
            name
        );
        return;
    }

    eprintln!(
        "dome: sandbox '{}' is already running — these flags are ignored (its live config \
         is kept; stop it first, or use `dome sandbox config {}`, to change it):",
        name, name
    );
    for line in &conflicts {
        eprintln!("  {line}");
    }
    if from.is_some() {
        eprintln!("  --from (only seeds a brand-new sandbox; never re-seeds a running one)");
    }
}

/// Worker-side: resolve (creating, seeding, or pinning as needed) the CAS source a
/// persistent sandbox cold-boots from. The worker is always the persistence owner — it
/// holds the sandbox lock for the VM's whole lifetime — so `--from` is gated as
/// owner-of-a-possibly-absent sandbox: honored only for a brand-new sandbox, and refused
/// (never silently dropped) on one that already exists. An existing sandbox stays pinned
/// to the immutable base recorded in its index; a fresh one is pinned to the current OS
/// base. Mirrors the owner branch of [`run_sandbox`], minus the user-facing messaging
/// (the detached worker logs instead of printing).
pub(crate) fn prepare_sandbox_source(
    name: &str,
    data_dir: &str,
    vm_args: &VmArgs,
    from: Option<&str>,
) -> Result<SandboxSource> {
    dome_vm::validate_checkpoint_name(name).map_err(|e| anyhow::anyhow!(e))?;
    let index_path = format!("{}/sandboxes/{}.idx", data_dir, name);

    let existed = Path::new(&index_path).exists();
    match gate_from(from, existed, true) {
        FromGate::NoSeed => {}
        FromGate::Seed(seed_name) => {
            let seed_idx = resolve_seed_index(seed_name, data_dir)?;
            seed_sandbox_index(&seed_idx, &index_path)?;
        }
        FromGate::Refused(reason) => return Err(collision_error(name, reason, true)),
    }

    let now_exists = Path::new(&index_path).exists();
    let base_path = if now_exists {
        pinned_base_for_existing(&index_path)?
    } else {
        ensure_current_base(data_dir, vm_args)?
    };

    Ok(SandboxSource {
        index_path,
        base_path,
    })
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

    // Persist the per-sandbox config so every cold boot reproduces this VM shape, rather
    // than depending on whatever flags a later `shell`/`run` invocation happens to pass.
    SandboxConfig::from_vm_args(vm_args).save(&data_dir, &name)?;
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

/// Entry point for `dome sandbox rm`. Resolves the sandbox name the same way the rest
/// of the command group does (explicit → `dome.json` → cwd slug) and unlinks its index.
/// Chunk reclamation is deliberately deferred to `dome prune` (mark-and-sweep), so `rm`
/// is fast and safe: it never walks or rewrites the shared chunk pool.
pub(crate) fn remove_sandbox(name_arg: Option<String>, config_path: Option<&str>) -> Result<()> {
    reject_direct_storage()?;
    let cfg = load_config(config_path)?;
    let cwd = std::env::current_dir()?;
    let name = resolve_name(name_arg.as_deref(), &cfg, &cwd)?;
    dome_vm::validate_checkpoint_name(&name).map_err(|e| anyhow::anyhow!(e))?;

    let data_dir = dome_vm::default_data_dir();
    delete_sandbox_index(&data_dir, &name)?;
    eprintln!(
        "dome: sandbox '{}' removed. Run `dome prune` to reclaim its disk space.",
        name
    );
    Ok(())
}

/// `dome sandbox save <name>`: force a durable flush+save of a running sandbox. Resolves
/// the name the same way the rest of the command group does (explicit → `dome.json` → cwd
/// slug), then routes through domed, which tells the sandbox's worker to flush its dirty
/// chunks and atomically rewrite the index. Errors clearly if the sandbox is not running
/// (an idle sandbox is already durable on disk — there is nothing buffered to flush).
pub(crate) fn save_sandbox(name_arg: Option<String>, config_path: Option<&str>) -> Result<()> {
    reject_direct_storage()?;
    let cfg = load_config(config_path)?;
    let cwd = std::env::current_dir()?;
    let name = resolve_name(name_arg.as_deref(), &cfg, &cwd)?;
    dome_vm::validate_checkpoint_name(&name).map_err(|e| anyhow::anyhow!(e))?;

    let data_dir = dome_vm::default_data_dir();
    crate::daemon::save_via_daemon(&data_dir, &name)?;
    eprintln!("dome: sandbox '{}' saved.", name);
    Ok(())
}

/// `dome sandbox config <name> [flags]`: view or edit a sandbox's persisted config. With no
/// boot flags it prints the current config; with flags it merges them into the persisted
/// metadata (atomically) and reports that the change applies on the **next** cold boot, not
/// to a running VM. The sandbox must already exist (created lazily by `shell`/`run` or
/// explicitly by `create`). Disk size remains pinned by the index guardrail regardless.
pub(crate) fn config_sandbox(name_arg: Option<String>, vm_args: &VmArgs) -> Result<()> {
    reject_direct_storage()?;
    let cfg = load_config(vm_args.config.as_deref())?;
    let cwd = std::env::current_dir()?;
    let name = resolve_name(name_arg.as_deref(), &cfg, &cwd)?;
    dome_vm::validate_checkpoint_name(&name).map_err(|e| anyhow::anyhow!(e))?;

    let data_dir = dome_vm::default_data_dir();
    let index_path = format!("{}/sandboxes/{}.idx", data_dir, name);
    if !Path::new(&index_path).exists() {
        bail!(
            "sandbox '{}' not found. Create it first with `dome sandbox create {}` \
             (or `dome sandbox shell {}`).",
            name,
            name,
            name
        );
    }

    // An existing sandbox created before config persistence has no sidecar yet; treat a
    // missing sidecar as an empty config so editing it still works.
    let mut config = SandboxConfig::load(&data_dir, &name)?.unwrap_or_default();

    // No boot-affecting flags ⇒ a read-only view; otherwise merge the edit.
    if !vm_flags_specified(vm_args, None) {
        print_sandbox_config(&name, &config);
        return Ok(());
    }

    config.merge_update(vm_args);
    config.save(&data_dir, &name)?;
    eprintln!(
        "dome: updated config for sandbox '{}'. It applies on the next cold boot; a \
         running VM keeps its current config until stopped.",
        name
    );
    // Disk size is pinned to the materialized index at creation: the cold-boot guardrail
    // (`vm::prepare_vm`) always re-pins an existing sandbox to its index's chunk count and
    // prints an "ignoring --disk-size" notice. So unlike the other fields, a `--disk-size`
    // edit here never takes effect — say so explicitly rather than letting the generic
    // "applies on the next cold boot" line above contradict that notice.
    if vm_args.disk_size.is_some() {
        eprintln!(
            "dome: note — disk size is fixed when a sandbox is created and cannot be changed \
             by `config`; this value is ignored on cold boot. Recreate the sandbox \
             (`dome sandbox rm {}` then `create`) to change its disk size.",
            name
        );
    }
    print_sandbox_config(&name, &config);
    Ok(())
}

/// Pretty-print a sandbox's persisted config, showing unset scalars as `default` and empty
/// lists as `none`, so `dome sandbox config <name>` reads clearly.
fn print_sandbox_config(name: &str, c: &SandboxConfig) {
    let scalar = |v: Option<u64>| v.map(|n| n.to_string()).unwrap_or_else(|| "default".into());
    let list = |v: &[String]| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.join(", ")
        }
    };
    eprintln!("config for sandbox '{}':", name);
    eprintln!(
        "  cpus:             {}",
        c.cpus.map(|n| n.to_string()).unwrap_or_else(|| "default".into())
    );
    eprintln!("  memory (MB):      {}", scalar(c.memory));
    eprintln!(
        "  disk_size (MB):   {} (pinned at creation)",
        scalar(c.disk_size)
    );
    eprintln!("  allow_net:        {}", c.allow_net);
    eprintln!("  allow_host_writes:{}", c.allow_host_writes);
    eprintln!("  ports:            {}", list(&c.ports));
    eprintln!("  mounts:           {}", list(&c.mounts));
    eprintln!("  secrets:          {}", list(&c.secrets));
    eprintln!("  allow_host:       {}", list(&c.allow_host));
    eprintln!("  expose_host:      {}", list(&c.expose_host));
}

/// `dome sandbox stop [--force] <name>`: stop a running sandbox — flush+save and shut its
/// VM down. Resolves the name the same way the rest of the command group does, then routes
/// through domed, which refuses (naming the count) when terminals are still attached unless
/// `--force` is given. Errors clearly if the sandbox is not running.
pub(crate) fn stop_sandbox(
    name_arg: Option<String>,
    force: bool,
    config_path: Option<&str>,
) -> Result<()> {
    reject_direct_storage()?;
    let cfg = load_config(config_path)?;
    let cwd = std::env::current_dir()?;
    let name = resolve_name(name_arg.as_deref(), &cfg, &cwd)?;
    dome_vm::validate_checkpoint_name(&name).map_err(|e| anyhow::anyhow!(e))?;

    let data_dir = dome_vm::default_data_dir();
    crate::daemon::stop_via_daemon(&data_dir, &name, force)?;
    eprintln!("dome: sandbox '{}' stopped.", name);
    Ok(())
}

/// Unlink a sandbox's index (and any lock left behind by a crashed session). Errors
/// clearly if the sandbox does not exist, and refuses to remove one that is currently
/// open by a live session — deleting a running sandbox's index would just be recreated
/// by that session's save on exit, so it is rejected rather than silently lost.
/// Chunks are intentionally left untouched; `dome prune` reclaims them later.
pub(crate) fn delete_sandbox_index(data_dir: &str, name: &str) -> Result<()> {
    let index_path = format!("{}/sandboxes/{}.idx", data_dir, name);
    let lock_path = PathBuf::from(format!("{}/sandboxes/{}.lock", data_dir, name));

    if !Path::new(&index_path).exists() {
        bail!("sandbox '{}' not found", name);
    }

    // Take the persistence lock for the whole removal rather than merely *checking*
    // liveness — a bare `is_held_live` check-then-act leaves a window in which a
    // concurrent `shell`/`run` cold-boots a worker (taking the lock) between the check
    // and the unlink, so `rm` deletes the index out from under a live worker that then
    // recreates it on its next save — silently losing the `rm`. Acquiring the lock makes
    // the guard atomic: `Fork` means a live worker already owns it (refuse), and holding
    // `Owner` for the unlink means no worker can boot mid-delete. This mirrors how
    // `create_sandbox` guards its own index write. Reclaiming a stale lock from a crashed
    // owner is part of `acquire`, so it subsumes the old explicit stale-lock cleanup; the
    // guard's drop removes the lock file on every exit path.
    let _guard = match lock::acquire(&lock_path)? {
        Lock::Owner(g) => g,
        Lock::Fork => bail!(
            "sandbox '{}' is running. Stop it first with `dome sandbox stop {}`, \
             then run `dome sandbox rm {}`.",
            name,
            name,
            name
        ),
    };

    std::fs::remove_file(&index_path)
        .with_context(|| format!("failed to remove sandbox index '{}'", index_path))?;
    // Drop any crash marker too, so a sandbox later reusing this name does not inherit a
    // stale `failed` state in `ls` before its first cold boot. Best-effort.
    worker::clear_failed_marker(data_dir, name);
    // Drop the persisted config sidecar (and any live marker) so a sandbox later reusing
    // this name starts from a clean slate rather than inheriting the removed one's config.
    SandboxConfig::remove(data_dir, name);
    SandboxConfig::clear_live(data_dir, name);
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

/// Entry point for `dome sandbox ls`. Always routes through domed (auto-spawning it if
/// needed) for a single code path and uniform rich output: NAME, SIZE (CAS delta), BASE
/// (pinned OS version), STATE (`running` with attached-terminal count when a live worker
/// owns it, else `idle`), and CREATED age. Output stays flat: flatten-in-place leaves no
/// parent lineage to show.
pub(crate) fn list_sandboxes() -> Result<()> {
    let data_dir = dome_vm::default_data_dir();
    let infos = crate::daemon::list_via_daemon(&data_dir)?;

    if infos.is_empty() {
        eprintln!("No sandboxes found.");
        return Ok(());
    }

    let header = ["NAME", "SIZE", "BASE", "STATE", "ATTACHED", "CREATED"];
    let cells: Vec<Vec<String>> = infos
        .iter()
        .map(|info| {
            let created = std::time::UNIX_EPOCH + std::time::Duration::from_secs(info.created_unix);
            vec![
                info.name.clone(),
                checkpoint::format_cas_size(info.size_bytes),
                info.base.clone(),
                info.state.clone(),
                info.attached.to_string(),
                checkpoint::format_age(created),
            ]
        })
        .collect();

    print!("{}", checkpoint::render_table(&header, &cells));
    Ok(())
}

/// Disk-scan side of `dome sandbox ls`, used by domed to build a [`SandboxInfo`] for
/// every sandbox index under `{data_dir}/sandboxes`. Live-worker state (running/attached)
/// is overlaid by the registry on top of this; here every sandbox is reported with its
/// on-disk `idle`/lock-based status. Reuses the same robust walker as the listing logic.
pub(crate) fn collect_sandbox_infos(data_dir: &str) -> Result<Vec<dome_proto::control::SandboxInfo>> {
    Ok(collect_sandbox_rows(data_dir)?
        .into_iter()
        .map(|row| {
            let created_unix = row
                .mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            dome_proto::control::SandboxInfo {
                name: row.name,
                size_bytes: row.size_bytes,
                base: row.base,
                state: if row.running { "running" } else { "idle" }.to_string(),
                attached: 0,
                created_unix,
            }
        })
        .collect())
}

/// One row of `dome sandbox ls`: a sandbox's name, CAS delta size, pinned base
/// version, running/idle status, and last-modified time (used for the age column and
/// for ordering). Kept as plain data so the listing logic can be tested over a temp
/// data dir without rendering.
struct SandboxRow {
    name: String,
    size_bytes: u64,
    base: String,
    running: bool,
    mtime: std::time::SystemTime,
}

/// Gather a [`SandboxRow`] for every sandbox index under `{data_dir}/sandboxes`, sorted
/// oldest-first to match `checkpoint list`. SIZE is the CAS delta (non-ZERO chunks ×
/// 64 KiB), identical to how checkpoints are measured; BASE is the pinned OS version
/// read from the index's fallback; STATUS is `running` when a live session holds the
/// persistence lock. A missing `sandboxes/` directory is not an error — it yields no
/// rows so the caller can print a clear "no sandboxes" message.
fn collect_sandbox_rows(data_dir: &str) -> Result<Vec<SandboxRow>> {
    let sandboxes_dir = format!("{}/sandboxes", data_dir);
    let entries = match std::fs::read_dir(&sandboxes_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => bail!("failed to read sandboxes directory: {}", e),
    };

    let mut rows = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("idx") {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Non-UTF-8 paths are impossible for dome-created sandboxes (names are
        // ASCII-only), but guard explicitly so the error is intelligible rather than
        // a spurious "failed to open index: " from an empty-string fallback.
        let path_str = match path.to_str() {
            Some(s) => s,
            None => {
                eprintln!("dome: skipping sandbox with non-UTF-8 path: {:?}", path);
                continue;
            }
        };

        // A corrupt or unreadable index must not abort the entire listing — other
        // sandboxes are still healthy. Emit a warning and skip the bad entry.
        let idx = match dome_store::ChunkIndex::load(path_str) {
            Ok(idx) => idx,
            Err(e) => {
                eprintln!("dome: skipping sandbox '{}': {:#}", name, e);
                continue;
            }
        };

        // CAS delta size: count non-ZERO chunks × 64 KiB, matching `checkpoint list`.
        let non_zero = (0..idx.num_chunks())
            .filter(|&i| idx.get_hash(i).map(|h| h != "ZERO").unwrap_or(false))
            .count();
        let size_bytes = (non_zero as u64) * 64 * 1024;
        let base = idx
            .fallback_path
            .as_deref()
            .map(base_version_from_fallback)
            .unwrap_or_else(|| "?".to_string());

        let lock_path = path.with_extension("lock");
        let running = lock::is_held_live(&lock_path);

        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("dome: skipping sandbox '{}': {}", name, e);
                continue;
            }
        };

        rows.push(SandboxRow {
            name,
            size_bytes,
            base,
            running,
            mtime,
        });
    }

    rows.sort_by_key(|r| r.mtime);
    Ok(rows)
}

/// Pair every sandbox under `{data_dir}/sandboxes` with the OS base version it is
/// pinned to, reusing the same robust walker as `dome sandbox ls` (corrupt indexes are
/// skipped with a warning rather than aborting). Used by the latest-only retention
/// policy to find sandboxes still pinned to a now-superseded base after an upgrade.
pub(crate) fn collect_sandbox_base_versions(data_dir: &str) -> Result<Vec<(String, String)>> {
    Ok(collect_sandbox_rows(data_dir)?
        .into_iter()
        .map(|row| (row.name, row.base))
        .collect())
}

/// Extract the OS base version a sandbox is pinned to from its recorded base image
/// path. Base images are stored under versioned `rootfs-<version>.ext4` filenames, so
/// the version is the filename with that prefix and suffix stripped. A non-standard
/// base path (e.g. an explicit `--rootfs`) has no embedded version, so its bare
/// filename is shown as a best-effort label rather than a misleading version.
fn base_version_from_fallback(fallback_path: &str) -> String {
    let file = Path::new(fallback_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(fallback_path);
    file.strip_prefix("rootfs-")
        .and_then(|s| s.strip_suffix(".ext4"))
        .unwrap_or(file)
        .to_string()
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

    #[test]
    fn base_version_is_parsed_from_a_versioned_rootfs_path() {
        // A sandbox pins its base via the versioned rootfs filename written at
        // creation; `ls` shows just the version, not the whole path.
        assert_eq!(
            base_version_from_fallback("/Users/dev/.dome/rootfs-1.2.3.ext4"),
            "1.2.3"
        );
    }

    #[test]
    fn base_version_falls_back_to_filename_for_a_nonstandard_path() {
        // An explicit `--rootfs` base has no embedded version; show its filename as a
        // best-effort label rather than a misleading or empty version.
        assert_eq!(
            base_version_from_fallback("/opt/images/custom-base.img"),
            "custom-base.img"
        );
    }

    #[test]
    fn collect_rows_reports_size_base_and_status_per_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let sb_dir = format!("{}/sandboxes", data_dir);
        std::fs::create_dir_all(&sb_dir).unwrap();

        // A sandbox with one written (non-ZERO) chunk, pinned to a versioned base.
        let mut web = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        web.fallback_path = Some(format!("{}/rootfs-1.2.3.ext4", data_dir));
        web.set_hash(0, "deadbeef".to_string());
        web.save(&format!("{}/web.idx", sb_dir)).unwrap();
        // It is currently open: its lock records our (live) PID → running.
        std::fs::write(
            format!("{}/web.lock", sb_dir),
            std::process::id().to_string(),
        )
        .unwrap();

        // A fresh, never-written sandbox pinned to a different base, no lock → idle.
        let mut api = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        api.fallback_path = Some(format!("{}/rootfs-2.0.0.ext4", data_dir));
        api.save(&format!("{}/api.idx", sb_dir)).unwrap();

        let rows = collect_sandbox_rows(data_dir).unwrap();
        let by_name = |n: &str| rows.iter().find(|r| r.name == n).unwrap();

        let web_row = by_name("web");
        assert_eq!(web_row.size_bytes, 64 * 1024, "one chunk = 64 KiB delta");
        assert_eq!(web_row.base, "1.2.3");
        assert!(web_row.running, "a sandbox with a live lock is running");

        let api_row = by_name("api");
        assert_eq!(api_row.size_bytes, 0, "an unwritten sandbox has zero delta");
        assert_eq!(api_row.base, "2.0.0");
        assert!(!api_row.running, "a sandbox with no lock is idle");
    }

    #[test]
    fn collect_rows_is_empty_when_there_are_no_sandboxes() {
        // A data dir without a sandboxes/ directory yields no rows, so `ls` can print
        // a clear "no sandboxes" message rather than erroring.
        let tmp = tempfile::tempdir().unwrap();
        let rows = collect_sandbox_rows(tmp.path().to_str().unwrap()).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn collect_rows_skips_a_corrupt_index_and_returns_healthy_ones() {
        // A corrupt .idx alongside a valid one must not abort the listing — the valid
        // sandbox should still appear and the corrupt one is skipped with a warning.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let sb_dir = format!("{}/sandboxes", data_dir);
        std::fs::create_dir_all(&sb_dir).unwrap();

        // Valid sandbox.
        let mut good = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        good.fallback_path = Some(format!("{}/rootfs-1.0.0.ext4", data_dir));
        good.save(&format!("{}/good.idx", sb_dir)).unwrap();

        // Corrupt .idx: valid extension, but garbage bytes.
        std::fs::write(format!("{}/corrupt.idx", sb_dir), b"not-an-index").unwrap();

        let rows = collect_sandbox_rows(data_dir).unwrap();
        assert_eq!(rows.len(), 1, "only the healthy sandbox should appear");
        assert_eq!(rows[0].name, "good");
    }

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
    fn rm_removes_the_sandbox_index() {
        // `rm` unlinks the sandbox's index quickly — the externally observable effect
        // is that the .idx file is gone afterward.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let sb_dir = format!("{}/sandboxes", data_dir);
        std::fs::create_dir_all(&sb_dir).unwrap();
        let idx = format!("{}/web.idx", sb_dir);
        dome_store::ChunkIndex::new(64 * 1024).save(&idx).unwrap();

        // A leftover crash marker from a prior failed run of this name must not survive rm,
        // or a sandbox later reusing the name would wrongly read as `failed` before its
        // first cold boot.
        worker::write_failed_marker(data_dir, "web", "crashed earlier");

        delete_sandbox_index(data_dir, "web").unwrap();
        assert!(!Path::new(&idx).exists(), "rm should unlink the index");
        assert!(
            !worker::is_failed(data_dir, "web"),
            "rm should clear any stale crash marker"
        );
        // rm now takes the persistence lock for the unlink (closing the check-then-act
        // window a bare liveness check left open); its guard must release on the way out,
        // leaving no lock behind to wedge a future sandbox reusing the name.
        assert!(
            !Path::new(&format!("{}/web.lock", sb_dir)).exists(),
            "a clean rm must not leave its persistence lock behind"
        );
    }

    #[test]
    fn rm_errors_clearly_when_the_sandbox_does_not_exist() {
        // Removing a name with no index on disk is a clear error naming the sandbox,
        // not a silent success.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(format!("{}/sandboxes", data_dir)).unwrap();

        let err = delete_sandbox_index(data_dir, "ghost").unwrap_err();
        assert!(
            err.to_string().contains("ghost") && err.to_string().contains("not found"),
            "error should name the missing sandbox; got: {}",
            err
        );
    }

    #[test]
    fn rm_refuses_a_running_sandbox_and_leaves_its_index_intact() {
        // A sandbox open by a live session must not be removed: its owner would just
        // recreate the index on save, silently losing the rm. Refuse and keep the index.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let sb_dir = format!("{}/sandboxes", data_dir);
        std::fs::create_dir_all(&sb_dir).unwrap();
        let idx = format!("{}/web.idx", sb_dir);
        dome_store::ChunkIndex::new(64 * 1024).save(&idx).unwrap();
        // A live lock recording our own (running) PID marks the sandbox as in use.
        std::fs::write(
            format!("{}/web.lock", sb_dir),
            std::process::id().to_string(),
        )
        .unwrap();

        let err = delete_sandbox_index(data_dir, "web").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("running") && msg.contains("stop"),
            "error should explain the sandbox is running and to stop it first; got: {}",
            err
        );
        assert!(
            Path::new(&idx).exists(),
            "a refused rm must leave the index intact"
        );
    }

    #[test]
    fn rm_clears_a_stale_lock_left_by_a_crashed_session() {
        // A lock recording a dead PID is stale; rm should remove both the index and the
        // stale lock so a future sandbox reusing the name is not wedged.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let sb_dir = format!("{}/sandboxes", data_dir);
        std::fs::create_dir_all(&sb_dir).unwrap();
        let idx = format!("{}/web.idx", sb_dir);
        let lock = format!("{}/web.lock", sb_dir);
        dome_store::ChunkIndex::new(64 * 1024).save(&idx).unwrap();
        // PID 0 is never a live process for the liveness check, so this lock is stale.
        std::fs::write(&lock, "0").unwrap();

        delete_sandbox_index(data_dir, "web").unwrap();
        assert!(!Path::new(&idx).exists(), "index should be removed");
        assert!(!Path::new(&lock).exists(), "stale lock should be cleared");
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

    #[test]
    fn rm_when_sandboxes_dir_is_absent_errors_clearly() {
        // `delete_sandbox_index` must produce a clear "not found" error even when the
        // sandboxes/ directory itself has never been created — not a misleading
        // permissions or I/O error.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        // No sandboxes/ subdir created — the directory does not exist at all.

        let err = delete_sandbox_index(data_dir, "ghost").unwrap_err();
        assert!(
            err.to_string().contains("ghost") && err.to_string().contains("not found"),
            "error should name the missing sandbox even when sandboxes/ is absent; got: {}",
            err
        );
    }

    #[test]
    fn rm_rejects_direct_storage_with_a_clear_message() {
        // `remove_sandbox` must call `reject_direct_storage()` first, so users running
        // with DOME_STORAGE=direct get a clear explanation instead of a confusing
        // "sandbox not found" error. Since the guard fires before any filesystem access
        // it is safe to call `remove_sandbox` with an arbitrary name here.
        let prev = std::env::var("DOME_STORAGE").ok();
        std::env::set_var("DOME_STORAGE", "direct");

        let err = remove_sandbox(Some("web".to_string()), None).unwrap_err();
        assert!(
            err.to_string().contains("DOME_STORAGE=direct"),
            "remove_sandbox should explain direct-storage rejection; got: {}",
            err
        );

        match prev {
            Some(v) => std::env::set_var("DOME_STORAGE", v),
            None => std::env::remove_var("DOME_STORAGE"),
        }
    }
}
