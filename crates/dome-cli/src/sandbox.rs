//! Persistent developer sandboxes. A sandbox is named, durable disk state layered
//! on the existing CAS engine: it boots from its own index (or lazily from a pinned
//! base on first use), runs interactively or one-off, and flatten-saves on clean
//! exit so the next invocation resumes exactly where it left off.

use std::path::Path;

use anyhow::{bail, Result};

use crate::assets;
use crate::cli::VmArgs;
use crate::config::{load_config, DomeConfig};
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
    let base_path = ensure_versioned_base(&data_dir, vm_args)?;

    let existed = Path::new(&index_path).exists();
    let source = SandboxSource {
        index_path,
        base_path,
    };
    let prepared = vm::prepare_vm(vm_args, &cfg, None, Some(&source))?;

    if existed {
        eprintln!("dome: resuming sandbox '{}'", name);
    } else {
        eprintln!("dome: creating sandbox '{}'", name);
    }

    run_session(&prepared, &command, &SaveTarget::Sandbox { name })
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
pub(crate) fn resolve_name(
    explicit: Option<&str>,
    cfg: &DomeConfig,
    cwd: &Path,
) -> Result<String> {
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
    let base = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
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

/// Ensure an immutable, version-pinned copy of the base image exists and return its
/// path. The sandbox's never-written chunks resolve through this file, so it must
/// not change underneath a sandbox even when the live rootfs is upgraded.
///
/// On APFS the clone is copy-on-write (essentially free); on Linux it is a full copy.
fn ensure_versioned_base(data_dir: &str, vm_args: &VmArgs) -> Result<String> {
    // Make sure the live OS image is present before we pin a copy of it.
    if vm_args.kernel.is_none()
        && vm_args.rootfs.is_none()
        && vm_args.initrd.is_none()
        && !assets::assets_ready(data_dir)
    {
        assets::download_os_image(data_dir)?;
    }

    let rootfs_path = vm_args
        .rootfs
        .clone()
        .unwrap_or_else(|| format!("{}/rootfs.ext4", data_dir));
    if !Path::new(&rootfs_path).exists() {
        bail!(
            "Rootfs not found at {}. Run `dome init` to download.",
            rootfs_path
        );
    }

    let version = base_version(data_dir);
    let bases_dir = format!("{}/bases", data_dir);
    std::fs::create_dir_all(&bases_dir)?;
    let base_path = format!("{}/rootfs-{}.ext4", bases_dir, version);
    if !Path::new(&base_path).exists() {
        vm::clone_file(&rootfs_path, &base_path)?;
    }
    Ok(base_path)
}

/// The installed OS image version (the `VERSION` file written by asset download),
/// falling back to the CLI's build version.
fn base_version(data_dir: &str) -> String {
    std::fs::read_to_string(format!("{}/VERSION", data_dir))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| assets::CURRENT_VERSION.to_string())
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
