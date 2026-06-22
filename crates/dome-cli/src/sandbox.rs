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

    let existed = Path::new(&index_path).exists();
    // A new sandbox pins to the currently installed OS version; an existing one stays
    // pinned to the immutable base recorded in its index, regardless of any later
    // upgrade — so an OS upgrade never silently rebases (and corrupts) it.
    let base_path = if existed {
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
