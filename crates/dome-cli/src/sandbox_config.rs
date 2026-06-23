//! Per-sandbox persisted configuration metadata.
//!
//! A persistent sandbox owns a small JSON sidecar — `{data_dir}/sandboxes/{name}.config.json`
//! — recording the boot-affecting config the user chose (cpus, memory, mounts, ports,
//! network policy, secrets). It is written by `dome sandbox create` (without booting) and
//! by the first lazy cold boot, and **every later cold boot reproduces from it** rather than
//! from whatever per-invocation flags the attaching client happened to pass. This makes a
//! sandbox a reproducible artifact: its shape is pinned at creation and edited explicitly
//! via `dome sandbox config`, not silently mutated by an unrelated `shell` invocation.
//!
//! Environment/session fields (kernel/rootfs/initrd, the `dome.json` path, verbose) are
//! deliberately NOT persisted — they describe the host and the session, not the sandbox.
//! Disk size is stored for the create-from-base path but is otherwise pinned by the index's
//! fixed chunk count (see [`crate::vm::pin_sandbox_disk_size`]), so editing it never resizes
//! a materialized sandbox.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::VmArgs;
use crate::worker;

/// Boot-affecting configuration persisted per sandbox so every cold boot reproduces the
/// same VM shape regardless of the per-invocation flags the attaching client passed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SandboxConfig {
    pub cpus: Option<usize>,
    pub memory: Option<u64>,
    pub disk_size: Option<u64>,
    pub allow_net: bool,
    pub allow_host_writes: bool,
    pub ports: Vec<String>,
    pub mounts: Vec<String>,
    pub secrets: Vec<String>,
    pub allow_host: Vec<String>,
    pub expose_host: Vec<String>,
}

impl SandboxConfig {
    /// Capture the boot-affecting subset of a `dome sandbox` invocation's flags.
    pub(crate) fn from_vm_args(vm: &VmArgs) -> Self {
        Self {
            cpus: vm.cpus,
            memory: vm.memory,
            disk_size: vm.disk_size,
            allow_net: vm.allow_net,
            allow_host_writes: vm.allow_host_writes,
            ports: vm.port.clone(),
            mounts: vm.mount.clone(),
            secrets: vm.secret.clone(),
            allow_host: vm.allow_host.clone(),
            expose_host: vm.expose_host.clone(),
        }
    }

    /// Project this persisted config onto a [`VmArgs`] suitable for booting, carrying the
    /// session/environment fields (kernel/rootfs/initrd, config path, verbose) from `base`
    /// (the originating invocation) but taking every boot-affecting field from `self`. So a
    /// cold boot uses the sandbox's pinned config, not the attaching client's flags.
    pub(crate) fn apply_to_vm_args(&self, base: &VmArgs) -> VmArgs {
        VmArgs {
            cpus: self.cpus,
            memory: self.memory,
            disk_size: self.disk_size,
            kernel: base.kernel.clone(),
            rootfs: base.rootfs.clone(),
            initrd: base.initrd.clone(),
            allow_net: self.allow_net,
            allow_host_writes: self.allow_host_writes,
            port: self.ports.clone(),
            mount: self.mounts.clone(),
            secret: self.secrets.clone(),
            allow_host: self.allow_host.clone(),
            expose_host: self.expose_host.clone(),
            config: base.config.clone(),
            verbose: base.verbose,
        }
    }

    /// Apply a `dome sandbox config` edit: a set `Option` overrides, a non-empty list
    /// replaces, and a bool flag turns the policy on (clap bools can't express "off", so a
    /// network/host-writes policy is only ever enabled here — disabling is recreate-time).
    /// The edit takes effect on the next cold boot, never on a running VM.
    pub(crate) fn merge_update(&mut self, vm: &VmArgs) {
        if vm.cpus.is_some() {
            self.cpus = vm.cpus;
        }
        if vm.memory.is_some() {
            self.memory = vm.memory;
        }
        if vm.disk_size.is_some() {
            self.disk_size = vm.disk_size;
        }
        if vm.allow_net {
            self.allow_net = true;
        }
        if vm.allow_host_writes {
            self.allow_host_writes = true;
        }
        if !vm.port.is_empty() {
            self.ports = vm.port.clone();
        }
        if !vm.mount.is_empty() {
            self.mounts = vm.mount.clone();
        }
        if !vm.secret.is_empty() {
            self.secrets = vm.secret.clone();
        }
        if !vm.allow_host.is_empty() {
            self.allow_host = vm.allow_host.clone();
        }
        if !vm.expose_host.is_empty() {
            self.expose_host = vm.expose_host.clone();
        }
    }

    /// Human-readable lines describing where the per-invocation `requested` flags conflict
    /// with `self` (the live, already-booted config). Used to warn — naming the live value
    /// — that flags passed to a sandbox that is already running are ignored. Only fields the
    /// user actually requested are reported; an absent flag is never a conflict.
    pub(crate) fn conflicts(&self, requested: &VmArgs) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(c) = requested.cpus {
            if Some(c) != self.cpus {
                out.push(format!("--cpus {c} (live: {})", show_opt(self.cpus)));
            }
        }
        if let Some(m) = requested.memory {
            if Some(m) != self.memory {
                out.push(format!("--memory {m} (live: {})", show_opt(self.memory)));
            }
        }
        if let Some(d) = requested.disk_size {
            if Some(d) != self.disk_size {
                out.push(format!(
                    "--disk-size {d} (live: {})",
                    show_opt(self.disk_size)
                ));
            }
        }
        if requested.allow_net && !self.allow_net {
            out.push("--allow-net (live: network disabled)".to_string());
        }
        if requested.allow_host_writes && !self.allow_host_writes {
            out.push("--allow-host-writes (live: host writes disabled)".to_string());
        }
        list_conflict(&mut out, "--port", &requested.port, &self.ports);
        list_conflict(&mut out, "--mount", &requested.mount, &self.mounts);
        list_conflict(&mut out, "--secret", &requested.secret, &self.secrets);
        list_conflict(
            &mut out,
            "--allow-host",
            &requested.allow_host,
            &self.allow_host,
        );
        list_conflict(
            &mut out,
            "--expose-host",
            &requested.expose_host,
            &self.expose_host,
        );
        out
    }

    /// The persisted config sidecar: `{data_dir}/sandboxes/{name}.config.json`.
    pub(crate) fn path(data_dir: &str, name: &str) -> PathBuf {
        Path::new(data_dir)
            .join("sandboxes")
            .join(format!("{name}.config.json"))
    }

    /// The live config a running worker booted with: `{workers}/{name}.live.json`. Written
    /// when the worker cold-boots and cleared on stop, so the CLI can name the live value
    /// when warning that flags passed to an already-running sandbox are ignored.
    fn live_path(data_dir: &str, name: &str) -> PathBuf {
        worker::workers_dir(data_dir).join(format!("{name}.live.json"))
    }

    /// Load the persisted config for `name`, or `None` if the sandbox has none yet.
    pub(crate) fn load(data_dir: &str, name: &str) -> Result<Option<Self>> {
        Self::load_path(&Self::path(data_dir, name))
    }

    /// Load the live config a running worker recorded, or `None` if no worker is up.
    pub(crate) fn load_live(data_dir: &str, name: &str) -> Option<Self> {
        Self::load_path(&Self::live_path(data_dir, name))
            .ok()
            .flatten()
    }

    fn load_path(path: &Path) -> Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let cfg = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing sandbox config {}", path.display()))?;
                Ok(Some(cfg))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading sandbox config {}", path.display())),
        }
    }

    /// Persist this config atomically (temp + rename) to the sandbox's sidecar.
    pub(crate) fn save(&self, data_dir: &str, name: &str) -> Result<()> {
        Self::save_path(self, &Self::path(data_dir, name))
    }

    /// Record this config as the live config of a running worker (best-effort).
    pub(crate) fn save_live(&self, data_dir: &str, name: &str) {
        let _ = std::fs::create_dir_all(worker::workers_dir(data_dir));
        let _ = Self::save_path(self, &Self::live_path(data_dir, name));
    }

    fn save_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, &bytes)
            .with_context(|| format!("writing sandbox config {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming sandbox config into place {}", path.display()))?;
        Ok(())
    }

    /// Remove the persisted config sidecar (best-effort), used by `rm`.
    pub(crate) fn remove(data_dir: &str, name: &str) {
        let _ = std::fs::remove_file(Self::path(data_dir, name));
    }

    /// Clear the live config marker (best-effort), used by a worker on shutdown.
    pub(crate) fn clear_live(data_dir: &str, name: &str) {
        let _ = std::fs::remove_file(Self::live_path(data_dir, name));
    }
}

/// Render an optional scalar for a warning: the value, or `default` when unset.
fn show_opt<T: std::fmt::Display>(v: Option<T>) -> String {
    match v {
        Some(v) => v.to_string(),
        None => "default".to_string(),
    }
}

/// Push a conflict line for a repeatable list flag when the user requested a non-empty
/// list that differs from the live one.
fn list_conflict(out: &mut Vec<String>, flag: &str, requested: &[String], live: &[String]) {
    if !requested.is_empty() && requested != live {
        let live_desc = if live.is_empty() {
            "none".to_string()
        } else {
            live.join(", ")
        };
        out.push(format!(
            "{flag} {} (live: {live_desc})",
            requested.join(", ")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(mut f: impl FnMut(&mut VmArgs)) -> VmArgs {
        let mut v = VmArgs::default();
        f(&mut v);
        v
    }

    #[test]
    fn captures_boot_affecting_flags_and_roundtrips_through_json() {
        let args = vm(|v| {
            v.cpus = Some(4);
            v.memory = Some(8192);
            v.allow_net = true;
            v.port = vec!["8080:80".to_string()];
            v.mount = vec!["./src:/work:rw".to_string()];
            // Session/environment fields must NOT be persisted.
            v.kernel = Some("/custom/Image".to_string());
            v.verbose = true;
        });
        let cfg = SandboxConfig::from_vm_args(&args);
        assert_eq!(cfg.cpus, Some(4));
        assert_eq!(cfg.memory, Some(8192));
        assert!(cfg.allow_net);
        assert_eq!(cfg.ports, vec!["8080:80".to_string()]);

        let json = serde_json::to_string(&cfg).unwrap();
        let back: SandboxConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn apply_overlays_boot_fields_but_keeps_session_fields_from_base() {
        let cfg = SandboxConfig {
            cpus: Some(4),
            memory: Some(8192),
            allow_net: true,
            mounts: vec!["./data:/data".to_string()],
            ..Default::default()
        };
        // The attaching invocation carries different boot flags (which must be ignored)
        // but supplies the session/env fields.
        let base = vm(|v| {
            v.cpus = Some(1);
            v.memory = Some(512);
            v.kernel = Some("/custom/Image".to_string());
            v.config = Some("./dome.json".to_string());
            v.verbose = true;
        });
        let effective = cfg.apply_to_vm_args(&base);
        // Boot fields come from the persisted config, not the invocation.
        assert_eq!(effective.cpus, Some(4));
        assert_eq!(effective.memory, Some(8192));
        assert!(effective.allow_net);
        assert_eq!(effective.mount, vec!["./data:/data".to_string()]);
        // Session/env fields come from the invocation.
        assert_eq!(effective.kernel.as_deref(), Some("/custom/Image"));
        assert_eq!(effective.config.as_deref(), Some("./dome.json"));
        assert!(effective.verbose);
    }

    #[test]
    fn merge_update_overrides_options_replaces_lists_and_enables_bools() {
        let mut cfg = SandboxConfig {
            cpus: Some(2),
            memory: Some(2048),
            ports: vec!["1:1".to_string()],
            allow_net: false,
            ..Default::default()
        };
        let edit = vm(|v| {
            v.cpus = Some(8); // override
            v.port = vec!["8080:80".to_string(), "443:443".to_string()]; // replace
            v.allow_net = true; // enable
                                // memory left unset → unchanged
        });
        cfg.merge_update(&edit);
        assert_eq!(cfg.cpus, Some(8));
        assert_eq!(
            cfg.memory,
            Some(2048),
            "an unset flag must not clear a field"
        );
        assert_eq!(
            cfg.ports,
            vec!["8080:80".to_string(), "443:443".to_string()]
        );
        assert!(cfg.allow_net);
    }

    #[test]
    fn conflicts_names_the_live_value_for_changed_flags_only() {
        let live = SandboxConfig {
            cpus: Some(2),
            memory: Some(2048),
            allow_net: false,
            ports: vec!["80:80".to_string()],
            ..Default::default()
        };
        // Request: a differing cpus, the same memory, allow-net (off live), and a new port.
        let requested = vm(|v| {
            v.cpus = Some(8);
            v.memory = Some(2048);
            v.allow_net = true;
            v.port = vec!["8080:80".to_string()];
        });
        let conflicts = live.conflicts(&requested);
        let joined = conflicts.join("\n");
        assert!(
            joined.contains("--cpus 8") && joined.contains("live: 2"),
            "{joined}"
        );
        assert!(
            !joined.contains("--memory"),
            "a matching flag is not a conflict: {joined}"
        );
        assert!(
            joined.contains("--allow-net") && joined.contains("disabled"),
            "{joined}"
        );
        assert!(
            joined.contains("--port 8080:80") && joined.contains("live: 80:80"),
            "{joined}"
        );
    }

    #[test]
    fn no_requested_flags_means_no_conflicts() {
        let live = SandboxConfig {
            cpus: Some(2),
            allow_net: true,
            ..Default::default()
        };
        assert!(live.conflicts(&VmArgs::default()).is_empty());
    }

    #[test]
    fn save_then_load_roundtrips_and_absent_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();

        assert!(
            SandboxConfig::load(data_dir, "web").unwrap().is_none(),
            "a sandbox with no sidecar loads as None"
        );

        let cfg = SandboxConfig {
            cpus: Some(4),
            memory: Some(8192),
            allow_net: true,
            ..Default::default()
        };
        cfg.save(data_dir, "web").unwrap();
        let loaded = SandboxConfig::load(data_dir, "web").unwrap().unwrap();
        assert_eq!(loaded, cfg);

        SandboxConfig::remove(data_dir, "web");
        assert!(SandboxConfig::load(data_dir, "web").unwrap().is_none());
    }

    #[test]
    fn live_config_roundtrips_and_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();

        assert!(SandboxConfig::load_live(data_dir, "web").is_none());
        let cfg = SandboxConfig {
            cpus: Some(3),
            ..Default::default()
        };
        cfg.save_live(data_dir, "web");
        assert_eq!(SandboxConfig::load_live(data_dir, "web"), Some(cfg));
        SandboxConfig::clear_live(data_dir, "web");
        assert!(SandboxConfig::load_live(data_dir, "web").is_none());
    }
}
