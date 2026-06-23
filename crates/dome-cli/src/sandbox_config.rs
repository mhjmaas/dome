//! The structured, versioned per-sandbox resolved config — the single source of truth
//! for an existing sandbox's VM shape.
//!
//! A persistent sandbox owns a small JSON sidecar — `{data_dir}/sandboxes/{name}.config.json`
//! — holding its boot-affecting configuration *already resolved* from `defaults <- dome.json
//! <- flags` at creation time (see [`ResolvedConfig::resolve`]). From then on the sidecar is
//! authoritative: every cold boot reproduces from it and does **not** re-read `dome.json` for
//! VM-shape fields. This makes a sandbox a reproducible artifact whose shape is pinned at
//! creation and edited explicitly via `dome sandbox config`.
//!
//! Session/environment fields (kernel/rootfs/initrd, the `dome.json` path, verbose) and
//! project fields (`command`, the sandbox name, `latest_only`) are deliberately NOT
//! persisted — they describe the host and the session, not the sandbox, and are read fresh
//! each invocation.
//!
//! Secret *values* are never written to disk: only the `{name, from, hosts}` mapping is
//! stored, and the real value is read from the host env var named by `from` at boot.
//!
//! Sidecars written before this model carry no `version` field. They are detected as legacy
//! on load and **healed in place** (re-resolved against the current `dome.json`, then written
//! back as a structured, versioned sidecar) — see [`load_or_heal`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::VmArgs;
use crate::config::{self, DomeConfig};
use crate::worker;

/// Current sidecar schema version. A sidecar with no `version` field is a legacy
/// (pre-resolution-model) sidecar and is healed on first load.
pub(crate) const SIDECAR_VERSION: u32 = 1;

/// A secret injected via the egress proxy. Only the *mapping* is persisted — the value is
/// read from the host env var named by `from` at boot and is never written to disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SecretSpec {
    /// Logical secret name (the proxy's key).
    pub name: String,
    /// Host environment variable holding the real value.
    pub from: String,
    /// Domains the secret may be sent to.
    pub hosts: Vec<String>,
}

impl SecretSpec {
    /// Render this mapping back into the `NAME=ENV@host1,host2` flag form, used only to
    /// compare a running sandbox's live secrets against requested `--secret` flags.
    fn to_flag(&self) -> String {
        format!("{}={}@{}", self.name, self.from, self.hosts.join(","))
    }
}

/// The proxy-facing slice of a resolved config: secrets (by mapping), the unified network
/// allow-list (merging `--allow-host` and `dome.json` `network.allow` into one field), and
/// the host ports exposed to the guest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ProxyResolved {
    pub secrets: Vec<SecretSpec>,
    pub allow: Vec<String>,
    pub expose_host: Vec<String>,
}

impl ProxyResolved {
    /// Build the dome-proxy config from this resolved slice. Secret values are left `None`
    /// (resolved from the host env at boot); expose-host mappings are parsed here so a
    /// malformed one fails the boot loudly rather than being silently dropped.
    pub(crate) fn to_proxy_config(&self) -> Result<dome_proxy::config::ProxyConfig> {
        let mut proxy = dome_proxy::config::ProxyConfig::default();
        for s in &self.secrets {
            proxy.secrets.insert(
                s.name.clone(),
                dome_proxy::config::SecretConfig {
                    from: s.from.clone(),
                    hosts: s.hosts.clone(),
                    value: None,
                },
            );
        }
        proxy.network.allow = self.allow.clone();
        for s in &self.expose_host {
            let mapping = config::parse_expose_host(s)
                .with_context(|| format!("invalid --expose-host: '{}'", s))?;
            proxy.expose_host.push(mapping);
        }
        Ok(proxy)
    }
}

/// The single structured, versioned resolved config: both the sidecar schema and the input
/// to VM preparation. Holds parsed, validated VM-shape values resolved once from
/// `defaults <- dome.json <- flags`. Scalars stay `Option` (an unset scalar means "use the
/// built-in default", applied at boot); booleans and lists are fully resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ResolvedConfig {
    /// Sidecar schema version. Always [`SIDECAR_VERSION`] when written by this code.
    pub version: u32,
    pub cpus: Option<usize>,
    pub memory: Option<u64>,
    pub disk_size: Option<u64>,
    pub allow_net: bool,
    pub allow_host_writes: bool,
    pub ports: Vec<String>,
    pub mounts: Vec<String>,
    pub proxy: ProxyResolved,
}

impl Default for ResolvedConfig {
    fn default() -> Self {
        Self {
            version: SIDECAR_VERSION,
            cpus: None,
            memory: None,
            disk_size: None,
            allow_net: false,
            allow_host_writes: false,
            ports: Vec::new(),
            mounts: Vec::new(),
            proxy: ProxyResolved::default(),
        }
    }
}

impl ResolvedConfig {
    /// Resolve a config with layered precedence `flags > dome.json > base/default`.
    ///
    /// This is the single place the three inputs merge. `base` carries previously-resolved
    /// values (an empty default for `create`/`run`; the stored sidecar for a heal). Scalars
    /// take the highest set layer; booleans are enabled by any layer (this slice keeps the
    /// current on-only grammar — `--no-X` lands in a follow-up); lists are merged additively
    /// (`base ++ dome.json ++ flags`, de-duplicated) so creating a sandbox folds `dome.json`'s
    /// ports/mounts/secrets/allow-list into the sidecar. A malformed `--secret` fails here, so
    /// the error surfaces at `create`/`reload` rather than on a future boot.
    pub(crate) fn resolve(
        base: &ResolvedConfig,
        dome: &DomeConfig,
        flags: &VmArgs,
    ) -> Result<Self> {
        // Scalars: highest set layer wins; an unset value inherits, defaults applied at boot.
        let cpus = flags.cpus.or(dome.cpus).or(base.cpus);
        let memory = flags.memory.or(dome.memory).or(base.memory);
        let disk_size = flags.disk_size.or(dome.disk_size).or(base.disk_size);

        // Booleans (this slice: on-only, matching the current flag grammar).
        let allow_net = flags.allow_net || dome.allow_net.unwrap_or(false) || base.allow_net;
        let allow_host_writes = flags.allow_host_writes
            || dome.allow_host_writes.unwrap_or(false)
            || base.allow_host_writes;

        // Lists: additive merge base ++ dome.json ++ flags (de-duplicated).
        let mut ports = base.ports.clone();
        extend_dedup(&mut ports, dome.ports.as_deref().unwrap_or(&[]));
        extend_dedup(&mut ports, &flags.port);

        let mut mounts = base.mounts.clone();
        extend_dedup(&mut mounts, dome.mounts.as_deref().unwrap_or(&[]));
        extend_dedup(&mut mounts, &flags.mount);

        let mut allow = base.proxy.allow.clone();
        if let Some(net) = &dome.network {
            if let Some(a) = &net.allow {
                extend_dedup(&mut allow, a);
            }
        }
        extend_dedup(&mut allow, &flags.allow_host);

        let mut expose_host = base.proxy.expose_host.clone();
        extend_dedup(&mut expose_host, dome.expose_host.as_deref().unwrap_or(&[]));
        extend_dedup(&mut expose_host, &flags.expose_host);

        // Secrets: base, then dome.json, then flags; a later layer overrides by name. The
        // value is never captured — only the {name, from, hosts} mapping.
        let mut secrets = base.proxy.secrets.clone();
        if let Some(dome_secrets) = &dome.secrets {
            for (name, entry) in dome_secrets {
                upsert_secret(
                    &mut secrets,
                    SecretSpec {
                        name: name.clone(),
                        from: entry.from.clone(),
                        hosts: entry.hosts.clone(),
                    },
                );
            }
        }
        for s in &flags.secret {
            let (name, from, hosts) = crate::vm::parse_secret_flag(s).with_context(|| {
                format!("invalid --secret: '{}' (expected NAME=ENV@host1,host2)", s)
            })?;
            upsert_secret(&mut secrets, SecretSpec { name, from, hosts });
        }
        // Deterministic order regardless of dome.json's HashMap iteration order.
        secrets.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(Self {
            version: SIDECAR_VERSION,
            cpus,
            memory,
            disk_size,
            allow_net,
            allow_host_writes,
            ports,
            mounts,
            proxy: ProxyResolved {
                secrets,
                allow,
                expose_host,
            },
        })
    }

    /// Apply a `dome sandbox config` edit in place: a set scalar overrides, a bool flag turns
    /// the policy on (this slice keeps the on-only grammar), and a non-empty list replaces.
    /// A malformed `--secret` errors so the edit fails rather than corrupting the sidecar.
    /// The edit takes effect on the next cold boot, never on a running VM.
    pub(crate) fn merge_update(&mut self, vm: &VmArgs) -> Result<()> {
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
        if !vm.allow_host.is_empty() {
            self.proxy.allow = vm.allow_host.clone();
        }
        if !vm.expose_host.is_empty() {
            self.proxy.expose_host = vm.expose_host.clone();
        }
        if !vm.secret.is_empty() {
            let mut secrets = Vec::new();
            for s in &vm.secret {
                let (name, from, hosts) = crate::vm::parse_secret_flag(s).with_context(|| {
                    format!("invalid --secret: '{}' (expected NAME=ENV@host1,host2)", s)
                })?;
                upsert_secret(&mut secrets, SecretSpec { name, from, hosts });
            }
            secrets.sort_by(|a, b| a.name.cmp(&b.name));
            self.proxy.secrets = secrets;
        }
        Ok(())
    }

    /// Human-readable lines describing where the per-invocation `requested` flags differ
    /// from `self` (the live, already-booted config). Used to warn — naming the live value —
    /// that flags passed to a sandbox that is already running take effect only on the next
    /// boot. Only fields the user actually requested are reported.
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
        let live_secrets: Vec<String> =
            self.proxy.secrets.iter().map(SecretSpec::to_flag).collect();
        list_conflict(&mut out, "--secret", &requested.secret, &live_secrets);
        list_conflict(
            &mut out,
            "--allow-host",
            &requested.allow_host,
            &self.proxy.allow,
        );
        list_conflict(
            &mut out,
            "--expose-host",
            &requested.expose_host,
            &self.proxy.expose_host,
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
    /// when warning that flags passed to an already-running sandbox apply only on next boot.
    fn live_path(data_dir: &str, name: &str) -> PathBuf {
        worker::workers_dir(data_dir).join(format!("{name}.live.json"))
    }

    /// Load the live config a running worker recorded, or `None` if no worker is up (or the
    /// marker is unreadable — it is best-effort).
    pub(crate) fn load_live(data_dir: &str, name: &str) -> Option<Self> {
        let bytes = std::fs::read(Self::live_path(data_dir, name)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Persist this config atomically (temp + rename) to the sandbox's sidecar.
    pub(crate) fn save(&self, data_dir: &str, name: &str) -> Result<()> {
        save_path(self, &Self::path(data_dir, name))
    }

    /// Record this config as the live config of a running worker (best-effort).
    pub(crate) fn save_live(&self, data_dir: &str, name: &str) {
        let _ = std::fs::create_dir_all(worker::workers_dir(data_dir));
        let _ = save_path(self, &Self::live_path(data_dir, name));
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

/// A legacy (pre-resolution-model) sidecar: the flat, unversioned shape captured by the
/// previous `from_vm_args`. Deserialize-only; loaded solely to heal into a [`ResolvedConfig`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct LegacySidecar {
    cpus: Option<usize>,
    memory: Option<u64>,
    disk_size: Option<u64>,
    allow_net: bool,
    allow_host_writes: bool,
    ports: Vec<String>,
    mounts: Vec<String>,
    secrets: Vec<String>,
    allow_host: Vec<String>,
    expose_host: Vec<String>,
}

impl LegacySidecar {
    /// Project the legacy sidecar back onto the flag values it recorded, so it can be
    /// re-resolved against the current `dome.json` during a heal.
    fn as_vm_args(&self) -> VmArgs {
        VmArgs {
            cpus: self.cpus,
            memory: self.memory,
            disk_size: self.disk_size,
            allow_net: self.allow_net,
            allow_host_writes: self.allow_host_writes,
            port: self.ports.clone(),
            mount: self.mounts.clone(),
            secret: self.secrets.clone(),
            allow_host: self.allow_host.clone(),
            expose_host: self.expose_host.clone(),
            ..Default::default()
        }
    }
}

/// What a sidecar on disk turned out to be: an already-versioned resolved config, or a
/// legacy one needing a heal.
enum Loaded {
    Resolved(ResolvedConfig),
    Legacy(LegacySidecar),
}

/// Read and classify the sidecar for `name`. A sidecar carrying a `version` field is a
/// current resolved config; one without is legacy. Returns `None` if the sandbox has no
/// sidecar yet.
fn load(data_dir: &str, name: &str) -> Result<Option<Loaded>> {
    let path = ResolvedConfig::path(data_dir, name);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("reading sandbox config {}", path.display()))
        }
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing sandbox config {}", path.display()))?;
    if value.get("version").is_some() {
        let cfg = serde_json::from_value(value)
            .with_context(|| format!("parsing sandbox config {}", path.display()))?;
        Ok(Some(Loaded::Resolved(cfg)))
    } else {
        let legacy = serde_json::from_value(value)
            .with_context(|| format!("parsing legacy sandbox config {}", path.display()))?;
        Ok(Some(Loaded::Legacy(legacy)))
    }
}

/// Load a sandbox's resolved config, healing a legacy sidecar in place if needed. Returns
/// `None` when the sandbox has no sidecar yet (a lazy first boot, or one created before
/// config persistence) so the caller can decide how to seed it.
///
/// Healing re-resolves the legacy sidecar's recorded flags against the current `dome.json`
/// (which legacy boots re-read every time) so nothing it was already booting with — secrets,
/// ports, the network allow-list — is lost, then writes back the structured, versioned
/// sidecar. If `dome.json` is absent, the heal proceeds from the stored flags only and warns;
/// if it is present but invalid, the boot fails loudly with the path (via [`config::load_config`]).
pub(crate) fn load_or_heal(
    data_dir: &str,
    name: &str,
    config_flag: Option<&str>,
) -> Result<Option<ResolvedConfig>> {
    match load(data_dir, name)? {
        None => Ok(None),
        Some(Loaded::Resolved(cfg)) => Ok(Some(cfg)),
        Some(Loaded::Legacy(legacy)) => {
            let flags = legacy.as_vm_args();
            let dome = if config::config_path(config_flag).exists() {
                config::load_config(config_flag)?
            } else {
                eprintln!(
                    "dome: dome.json not found at {}; healing sandbox '{}' config from its \
                     stored flags only",
                    config::config_path(config_flag).display(),
                    name
                );
                DomeConfig::default()
            };
            let healed = ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &flags)?;
            healed.save(data_dir, name)?;
            Ok(Some(healed))
        }
    }
}

/// Append items from `extra` to `target`, skipping any already present so an additive merge
/// across layers does not duplicate a forward/mount/host the user listed twice.
fn extend_dedup(target: &mut Vec<String>, extra: &[String]) {
    for s in extra {
        if !target.iter().any(|existing| existing == s) {
            target.push(s.clone());
        }
    }
}

/// Insert `spec` into `secrets`, replacing any existing entry with the same name (a later
/// layer wins) while preserving the first-seen position.
fn upsert_secret(secrets: &mut Vec<SecretSpec>, spec: SecretSpec) {
    if let Some(existing) = secrets.iter_mut().find(|s| s.name == spec.name) {
        *existing = spec;
    } else {
        secrets.push(spec);
    }
}

/// Atomically write `cfg` to `path` (temp + rename).
fn save_path(cfg: &ResolvedConfig, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(cfg)?;
    std::fs::write(&tmp, &bytes)
        .with_context(|| format!("writing sandbox config {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming sandbox config into place {}", path.display()))?;
    Ok(())
}

/// Render an optional scalar for a warning: the value, or `default` when unset.
fn show_opt<T: std::fmt::Display>(v: Option<T>) -> String {
    match v {
        Some(v) => v.to_string(),
        None => "default".to_string(),
    }
}

/// Push a conflict line for a repeatable list flag when the user requested a non-empty list
/// that differs from the live one.
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
    use crate::config::{NetworkEntry, SecretEntry};
    use std::collections::HashMap;

    fn vm(mut f: impl FnMut(&mut VmArgs)) -> VmArgs {
        let mut v = VmArgs::default();
        f(&mut v);
        v
    }

    #[test]
    fn resolve_precedence_flag_over_dome_over_default() {
        let dome = DomeConfig {
            cpus: Some(4),
            memory: Some(2048),
            ..Default::default()
        };
        // Flag wins over dome.json; dome.json wins over default (unset).
        let flags = vm(|v| v.cpus = Some(8));
        let r = ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &flags).unwrap();
        assert_eq!(r.cpus, Some(8), "flag wins");
        assert_eq!(r.memory, Some(2048), "dome.json wins when no flag");
        assert_eq!(
            r.disk_size, None,
            "unset stays None (default applied at boot)"
        );
        assert_eq!(r.version, SIDECAR_VERSION);
    }

    #[test]
    fn resolve_booleans_enabled_by_any_layer() {
        let dome = DomeConfig {
            allow_net: Some(true),
            ..Default::default()
        };
        let r =
            ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &VmArgs::default()).unwrap();
        assert!(r.allow_net, "dome.json enables");
        assert!(!r.allow_host_writes);

        let flags = vm(|v| v.allow_host_writes = true);
        let r = ResolvedConfig::resolve(&ResolvedConfig::default(), &DomeConfig::default(), &flags)
            .unwrap();
        assert!(r.allow_host_writes, "flag enables");
    }

    #[test]
    fn resolve_lists_merge_additively_and_dedup() {
        let dome = DomeConfig {
            ports: Some(vec!["8080:80".into(), "443:443".into()]),
            ..Default::default()
        };
        let flags = vm(|v| v.port = vec!["443:443".into(), "9000:9000".into()]);
        let r = ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &flags).unwrap();
        // dome.json ports first, then the flag's new port; the duplicate 443:443 appears once.
        assert_eq!(
            r.ports,
            vec![
                "8080:80".to_string(),
                "443:443".to_string(),
                "9000:9000".to_string()
            ]
        );
    }

    #[test]
    fn resolve_unifies_allow_host_and_network_allow() {
        let dome = DomeConfig {
            network: Some(NetworkEntry {
                allow: Some(vec!["api.openai.com".into()]),
            }),
            ..Default::default()
        };
        let flags = vm(|v| v.allow_host = vec!["*.github.com".into()]);
        let r = ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &flags).unwrap();
        assert_eq!(
            r.proxy.allow,
            vec!["api.openai.com".to_string(), "*.github.com".to_string()],
            "network.allow and --allow-host feed one unified list"
        );
    }

    #[test]
    fn resolve_secrets_store_mapping_only_with_flag_overriding_dome() {
        let mut dome_secrets = HashMap::new();
        dome_secrets.insert(
            "openai".to_string(),
            SecretEntry {
                from: "OPENAI_API_KEY".into(),
                hosts: vec!["api.openai.com".into()],
            },
        );
        let dome = DomeConfig {
            secrets: Some(dome_secrets),
            ..Default::default()
        };
        // A flag re-defines `openai` and adds `gh`. The flag's `openai` must win.
        let flags = vm(|v| {
            v.secret = vec![
                "openai=OPENAI_TOKEN@api.openai.com".into(),
                "gh=GH_TOKEN@api.github.com".into(),
            ]
        });
        let r = ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &flags).unwrap();
        assert_eq!(r.proxy.secrets.len(), 2);
        let openai = r.proxy.secrets.iter().find(|s| s.name == "openai").unwrap();
        assert_eq!(
            openai.from, "OPENAI_TOKEN",
            "flag overrides dome.json by name"
        );
        let gh = r.proxy.secrets.iter().find(|s| s.name == "gh").unwrap();
        assert_eq!(gh.hosts, vec!["api.github.com".to_string()]);

        // The serialized sidecar must never contain a secret *value*, only the mapping.
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("OPENAI_TOKEN") && !json.contains("\"value\""));
    }

    #[test]
    fn resolve_rejects_a_malformed_secret() {
        let flags = vm(|v| v.secret = vec!["no-at-sign".into()]);
        let err =
            ResolvedConfig::resolve(&ResolvedConfig::default(), &DomeConfig::default(), &flags)
                .unwrap_err();
        assert!(format!("{err:#}").contains("--secret"));
    }

    #[test]
    fn resolve_base_carries_previously_resolved_values() {
        // A heal re-resolves a base against dome.json; the base's values are inherited when
        // no higher layer overrides, and lists fold together.
        let base = ResolvedConfig {
            cpus: Some(2),
            allow_net: true,
            ports: vec!["1:1".into()],
            ..Default::default()
        };
        let dome = DomeConfig {
            ports: Some(vec!["2:2".into()]),
            ..Default::default()
        };
        let r = ResolvedConfig::resolve(&base, &dome, &VmArgs::default()).unwrap();
        assert_eq!(r.cpus, Some(2), "base cpus inherited");
        assert!(r.allow_net, "base allow_net inherited");
        assert_eq!(
            r.ports,
            vec!["1:1".to_string(), "2:2".to_string()],
            "base ++ dome"
        );
    }

    #[test]
    fn save_then_load_roundtrips_a_versioned_sidecar_and_absent_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();

        assert!(
            load_or_heal(data_dir, "web", None).unwrap().is_none(),
            "a sandbox with no sidecar loads as None"
        );

        let cfg = ResolvedConfig {
            cpus: Some(4),
            memory: Some(8192),
            allow_net: true,
            ..Default::default()
        };
        cfg.save(data_dir, "web").unwrap();
        let loaded = load_or_heal(data_dir, "web", None).unwrap().unwrap();
        assert_eq!(loaded, cfg);

        ResolvedConfig::remove(data_dir, "web");
        assert!(load_or_heal(data_dir, "web", None).unwrap().is_none());
    }

    #[test]
    fn legacy_sidecar_heals_on_load_and_writes_back_versioned() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let path = ResolvedConfig::path(data_dir, "web");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A legacy sidecar: no `version`, secrets as raw flag strings, top-level allow_host.
        let legacy = serde_json::json!({
            "cpus": 4,
            "allow_net": true,
            "ports": ["8080:80"],
            "secrets": ["openai=OPENAI_API_KEY@api.openai.com"],
            "allow_host": ["api.openai.com"]
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        // No dome.json present here, so the heal proceeds from stored flags only.
        let healed = load_or_heal(data_dir, "web", None).unwrap().unwrap();
        assert_eq!(healed.version, SIDECAR_VERSION);
        assert_eq!(healed.cpus, Some(4));
        assert!(healed.allow_net);
        assert_eq!(healed.ports, vec!["8080:80".to_string()]);
        assert_eq!(healed.proxy.secrets.len(), 1);
        assert_eq!(healed.proxy.secrets[0].name, "openai");
        assert_eq!(healed.proxy.allow, vec!["api.openai.com".to_string()]);

        // The sidecar on disk is now versioned (healed in place).
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("\"version\""),
            "heal writes back a versioned sidecar"
        );

        // A second load is a no-op (already resolved) and returns the same config.
        let again = load_or_heal(data_dir, "web", None).unwrap().unwrap();
        assert_eq!(again, healed);
    }

    #[test]
    fn legacy_heal_with_invalid_dome_json_fails_loudly_with_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let path = ResolvedConfig::path(data_dir, "web");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A legacy sidecar (no `version`) forces a heal, which re-reads dome.json.
        let legacy = serde_json::json!({ "cpus": 2 });
        std::fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        // An explicit, present-but-malformed dome.json must fail the boot, not heal silently.
        let bad = tmp.path().join("dome.json");
        std::fs::write(&bad, b"{ not valid json").unwrap();
        let err = load_or_heal(data_dir, "web", bad.to_str()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("dome.json"), "error must name the path: {msg}");

        // The sidecar is left untouched (still legacy) so a fixed dome.json can heal it later.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("\"version\""),
            "a failed heal must not write back: {on_disk}"
        );
    }

    #[test]
    fn merge_update_overrides_scalars_replaces_lists_and_enables_bools() {
        let mut cfg = ResolvedConfig {
            cpus: Some(2),
            memory: Some(2048),
            ports: vec!["1:1".into()],
            ..Default::default()
        };
        let edit = vm(|v| {
            v.cpus = Some(8);
            v.port = vec!["8080:80".into(), "443:443".into()];
            v.allow_net = true;
        });
        cfg.merge_update(&edit).unwrap();
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
        let live = ResolvedConfig {
            cpus: Some(2),
            memory: Some(2048),
            ports: vec!["80:80".into()],
            ..Default::default()
        };
        let requested = vm(|v| {
            v.cpus = Some(8);
            v.memory = Some(2048);
            v.allow_net = true;
            v.port = vec!["8080:80".into()];
        });
        let joined = live.conflicts(&requested).join("\n");
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
    fn live_config_roundtrips_and_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();

        assert!(ResolvedConfig::load_live(data_dir, "web").is_none());
        let cfg = ResolvedConfig {
            cpus: Some(3),
            ..Default::default()
        };
        cfg.save_live(data_dir, "web");
        assert_eq!(ResolvedConfig::load_live(data_dir, "web"), Some(cfg));
        ResolvedConfig::clear_live(data_dir, "web");
        assert!(ResolvedConfig::load_live(data_dir, "web").is_none());
    }
}
