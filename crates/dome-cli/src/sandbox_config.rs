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

/// A resolved provisioning spec: the ordered toolchain steps and the provision-time
/// network allow-list, both fully resolved from `dome.json`. Persisted in the sidecar so a
/// reload/heal carries it forward; the cache key for the provisioned layer is derived from
/// it (see [`crate::provision::cache_key`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ProvisionSpec {
    pub steps: Vec<String>,
    pub allow: Vec<String>,
    /// Provision-time secrets (mapping only — values are never persisted, same as runtime).
    /// Injected via the egress proxy during the build; their `hosts` are auto-whitelisted
    /// into the build network (see [`Self::effective_allow`]).
    pub secrets: Vec<SecretSpec>,
}

impl ProvisionSpec {
    /// The build-time network allow-list: the declared `allow` plus every secret's `hosts`
    /// auto-whitelisted, so a corp mirror you authenticate to need not be declared twice.
    /// De-duplicated while preserving first-seen order.
    ///
    /// An empty `allow` means "all allowed" — there is nothing to whitelist *into*, and
    /// folding the secret hosts in would wrongly narrow an open build network down to just
    /// them. So an empty allow-list stays empty (all allowed) regardless of secrets. The
    /// auto-whitelist applies to the provision allow-list only — runtime is untouched.
    pub(crate) fn effective_allow(&self) -> Vec<String> {
        if self.allow.is_empty() {
            return Vec::new();
        }
        let mut out = self.allow.clone();
        for s in &self.secrets {
            for host in &s.hosts {
                if !out.contains(host) {
                    out.push(host.clone());
                }
            }
        }
        out
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
    /// Declarative toolchain provisioning, resolved from `dome.json`. `None` when the
    /// project declares no `provision` block (the common case — boot from the bare base).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<ProvisionSpec>,
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
            provision: None,
        }
    }
}

impl ResolvedConfig {
    /// Resolve a config with layered precedence `flags > dome.json > base/default`.
    ///
    /// This is the single place the three inputs merge. `base` carries previously-resolved
    /// values (an empty default for `create`/`run`; the stored sidecar for a heal). Scalars
    /// take the highest set layer; tri-state booleans take the highest *set* layer
    /// (`--allow-net`/`--no-allow-net` and the host-writes pair override `dome.json`, which
    /// overrides the base/default) so a policy can be turned off, not only on; lists take the
    /// highest *set* layer too (a non-empty `--port`/etc. replaces the lower layer, `--no-port`
    /// clears it to empty, an omitted flag inherits `dome.json` then the base) — one consistent
    /// rule with scalars and booleans. A malformed `--secret` fails here, so the error surfaces
    /// at `create`/`reload` rather than on a future boot.
    pub(crate) fn resolve(
        base: &ResolvedConfig,
        dome: &DomeConfig,
        flags: &VmArgs,
    ) -> Result<Self> {
        // Scalars: highest set layer wins; an unset value inherits, defaults applied at boot.
        let cpus = flags.cpus.or(dome.cpus).or(base.cpus);
        let memory = flags.memory.or(dome.memory).or(base.memory);
        let disk_size = flags.disk_size.or(dome.disk_size).or(base.disk_size);

        // Tri-state booleans: highest set layer wins (flag > dome.json > base/default), so a
        // policy can be disabled via `--no-X`, not only enabled. An omitted flag inherits.
        let allow_net = flags
            .allow_net_flag()
            .or(dome.allow_net)
            .unwrap_or(base.allow_net);
        let allow_host_writes = flags
            .allow_host_writes_flag()
            .or(dome.allow_host_writes)
            .unwrap_or(base.allow_host_writes);

        // Lists: highest set layer replaces (flag > dome.json > base/default). A non-empty
        // flag list replaces, `--no-<list>` clears to empty, an omitted flag inherits the
        // next-lower layer that is set.
        let ports = flags
            .port_flag()
            .or_else(|| dome.ports.clone())
            .unwrap_or_else(|| base.ports.clone());

        let mounts = flags
            .mount_flag()
            .or_else(|| dome.mounts.clone())
            .unwrap_or_else(|| base.mounts.clone());

        // `--allow-host` and `dome.json` `network.allow` unify into one allow-list field.
        let allow = flags
            .allow_host_flag()
            .or_else(|| dome.network.as_ref().and_then(|n| n.allow.clone()))
            .unwrap_or_else(|| base.proxy.allow.clone());

        let expose_host = flags
            .expose_host_flag()
            .or_else(|| dome.expose_host.clone())
            .unwrap_or_else(|| base.proxy.expose_host.clone());

        // Secrets follow the same replace rule, but the layers carry different shapes: flag
        // strings are parsed here (a malformed one fails), `dome.json` entries are already
        // structured, and the base is the previously-resolved sidecar. The value is never
        // captured — only the {name, from, hosts} mapping.
        let secrets = if let Some(secret_flags) = flags.secret_flag() {
            parse_secret_specs(&secret_flags)?
        } else if let Some(dome_secrets) = &dome.secrets {
            secret_specs_from_map(dome_secrets)
        } else {
            base.proxy.secrets.clone()
        };

        // Provisioning has no flag layer in this slice: it is resolved straight from
        // `dome.json` (a declared `provision` block wins), falling back to the base/sidecar
        // so a heal/reload carries a previously-resolved spec forward. A block with no
        // steps resolves to `None` — there is nothing to provision.
        let provision = match &dome.provision {
            Some(p) if !p.steps.is_empty() => Some(ProvisionSpec {
                steps: p.steps.clone(),
                allow: p.allow.clone().unwrap_or_default(),
                secrets: p
                    .secrets
                    .as_ref()
                    .map(secret_specs_from_map)
                    .unwrap_or_default(),
            }),
            Some(_) => None,
            None => base.provision.clone(),
        };

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
            provision,
        })
    }

    /// Apply a `dome sandbox config` edit in place: a set scalar overrides, a tri-state bool
    /// flag turns the policy on (`--allow-net`) or off (`--no-allow-net`) while an omitted one
    /// leaves it untouched, a non-empty list replaces, and `--no-<list>` clears it to empty
    /// (an omitted list flag leaves it untouched).
    /// A malformed `--secret` errors so the edit fails rather than corrupting the sidecar.
    /// The edit takes effect on the next cold boot, never on a running VM.
    pub(crate) fn merge_update(&mut self, vm: &VmArgs) -> Result<()> {
        if vm.cpus.is_some() {
            self.cpus = vm.cpus;
        }
        if vm.memory.is_some() {
            self.memory = vm.memory;
        }
        // disk_size is intentionally NOT applied here: it is create-only (the disk is
        // physically pinned), so the sidecar's value is never mutated after creation. The
        // CLI hard-errors on `--disk-size` for an existing sandbox before reaching this point.
        if let Some(v) = vm.allow_net_flag() {
            self.allow_net = v;
        }
        if let Some(v) = vm.allow_host_writes_flag() {
            self.allow_host_writes = v;
        }
        if let Some(v) = vm.port_flag() {
            self.ports = v;
        }
        if let Some(v) = vm.mount_flag() {
            self.mounts = v;
        }
        if let Some(v) = vm.allow_host_flag() {
            self.proxy.allow = v;
        }
        if let Some(v) = vm.expose_host_flag() {
            self.proxy.expose_host = v;
        }
        if let Some(v) = vm.secret_flag() {
            self.proxy.secrets = parse_secret_specs(&v)?;
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
        // disk_size is create-only and hard-errors on an existing sandbox, so it can never be
        // a pending next-boot change — it is intentionally not reported here.
        if let Some(want) = requested.allow_net_flag() {
            if want != self.allow_net {
                let flag = if want {
                    "--allow-net"
                } else {
                    "--no-allow-net"
                };
                let live = if self.allow_net {
                    "network enabled"
                } else {
                    "network disabled"
                };
                out.push(format!("{flag} (live: {live})"));
            }
        }
        if let Some(want) = requested.allow_host_writes_flag() {
            if want != self.allow_host_writes {
                let flag = if want {
                    "--allow-host-writes"
                } else {
                    "--no-allow-host-writes"
                };
                let live = if self.allow_host_writes {
                    "host writes enabled"
                } else {
                    "host writes disabled"
                };
                out.push(format!("{flag} (live: {live})"));
            }
        }
        list_conflict(
            &mut out,
            "--port",
            "--no-port",
            requested.port_flag(),
            &self.ports,
        );
        list_conflict(
            &mut out,
            "--mount",
            "--no-mount",
            requested.mount_flag(),
            &self.mounts,
        );
        let live_secrets: Vec<String> =
            self.proxy.secrets.iter().map(SecretSpec::to_flag).collect();
        list_conflict(
            &mut out,
            "--secret",
            "--no-secret",
            requested.secret_flag(),
            &live_secrets,
        );
        list_conflict(
            &mut out,
            "--allow-host",
            "--no-allow-host",
            requested.allow_host_flag(),
            &self.proxy.allow,
        );
        list_conflict(
            &mut out,
            "--expose-host",
            "--no-expose-host",
            requested.expose_host_flag(),
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

/// Insert `spec` into `secrets`, replacing any existing entry with the same name (a later
/// duplicate wins) while preserving the first-seen position.
fn upsert_secret(secrets: &mut Vec<SecretSpec>, spec: SecretSpec) {
    if let Some(existing) = secrets.iter_mut().find(|s| s.name == spec.name) {
        *existing = spec;
    } else {
        secrets.push(spec);
    }
}

/// Convert a `dome.json` secret map (`{name: {from, hosts}}`) into structured specs, sorted by
/// name for a deterministic result regardless of the map's iteration order. Shared by the
/// runtime `secrets` and the `provision.secrets` resolve paths (same shape, same machinery).
fn secret_specs_from_map(
    map: &std::collections::HashMap<String, config::SecretEntry>,
) -> Vec<SecretSpec> {
    let mut secrets: Vec<SecretSpec> = map
        .iter()
        .map(|(name, entry)| SecretSpec {
            name: name.clone(),
            from: entry.from.clone(),
            hosts: entry.hosts.clone(),
        })
        .collect();
    secrets.sort_by(|a, b| a.name.cmp(&b.name));
    secrets
}

/// Parse `NAME=ENV@host1,host2` secret flag strings into structured specs, de-duplicating by
/// name (a later occurrence wins) and sorting by name for a deterministic sidecar. A malformed
/// entry errors so the edit/resolve fails rather than baking a bad config.
fn parse_secret_specs(flags: &[String]) -> Result<Vec<SecretSpec>> {
    let mut secrets = Vec::new();
    for s in flags {
        let (name, from, hosts) = crate::vm::parse_secret_flag(s).with_context(|| {
            format!("invalid --secret: '{}' (expected NAME=ENV@host1,host2)", s)
        })?;
        upsert_secret(&mut secrets, SecretSpec { name, from, hosts });
    }
    secrets.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(secrets)
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

/// Push a conflict line for a repeatable list flag when the user requested a list (via the
/// setter or its `--no-` clearing counterpart) that differs from the live one. `requested` is
/// the flag tri-state: `None` means the user did not touch this list; `Some(empty)` is a
/// `--no-<list>` clear; `Some(values)` is a replace.
fn list_conflict(
    out: &mut Vec<String>,
    set_flag: &str,
    clear_flag: &str,
    requested: Option<Vec<String>>,
    live: &[String],
) {
    let Some(req) = requested else { return };
    if req == live {
        return;
    }
    let live_desc = if live.is_empty() {
        "none".to_string()
    } else {
        live.join(", ")
    };
    if req.is_empty() {
        out.push(format!("{clear_flag} (live: {live_desc})"));
    } else {
        out.push(format!("{set_flag} {} (live: {live_desc})", req.join(", ")));
    }
}

/// Warning shown when a network allow-list is configured but networking is
/// disabled. The allow-list is then inert: the egress proxy is never started, so
/// the guest has no DNS resolver and every lookup fails with `EAI_AGAIN`. This
/// is an easy footgun — declaring `network.allow` reads as "I want the network"
/// but does not by itself enable it. Returns `None` when there is nothing to warn about.
pub(crate) fn network_disabled_warning(allow_net: bool, allow: &[String]) -> Option<String> {
    if allow_net || allow.is_empty() {
        return None;
    }
    let plural = if allow.len() == 1 { "y" } else { "ies" };
    Some(format!(
        "network allow-list has {} entr{plural} but networking is disabled \
         (allow_net=false); the allow-list is ignored and DNS in the sandbox \
         will fail with EAI_AGAIN. Enable it with \"allow_net\": true in dome.json \
         (or --allow-net), then restart the sandbox.",
        allow.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{NetworkEntry, ProvisionEntry, SecretEntry};
    use std::collections::HashMap;

    #[test]
    fn warns_when_allowlist_set_but_networking_disabled() {
        let allow = vec!["registry.npmjs.org".to_string(), "github.com".to_string()];
        let warning = network_disabled_warning(false, &allow)
            .expect("should warn: allow entries are inert with networking off");
        // Names the real switch so the fix is obvious.
        assert!(warning.contains("allow_net"));
    }

    #[test]
    fn no_warning_when_networking_enabled() {
        let allow = vec!["registry.npmjs.org".to_string()];
        assert!(network_disabled_warning(true, &allow).is_none());
    }

    #[test]
    fn no_warning_when_allowlist_empty() {
        assert!(network_disabled_warning(false, &[]).is_none());
    }

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
    fn resolve_booleans_take_highest_set_layer() {
        // dome.json enables; an omitted flag inherits it.
        let dome = DomeConfig {
            allow_net: Some(true),
            ..Default::default()
        };
        let r =
            ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &VmArgs::default()).unwrap();
        assert!(r.allow_net, "dome.json enables, flag omitted inherits");
        assert!(!r.allow_host_writes);

        // A flag enables on its own.
        let flags = vm(|v| v.allow_host_writes = true);
        let r = ResolvedConfig::resolve(&ResolvedConfig::default(), &DomeConfig::default(), &flags)
            .unwrap();
        assert!(r.allow_host_writes, "flag enables");
    }

    #[test]
    fn resolve_no_flag_disables_a_lower_layer() {
        // `--no-allow-net` overrides a dome.json that enabled it.
        let dome = DomeConfig {
            allow_net: Some(true),
            ..Default::default()
        };
        let flags = vm(|v| v.no_allow_net = true);
        let r = ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &flags).unwrap();
        assert!(!r.allow_net, "--no-allow-net overrides dome.json");

        // `--no-allow-host-writes` overrides a base sidecar that had it enabled.
        let base = ResolvedConfig {
            allow_host_writes: true,
            ..Default::default()
        };
        let flags = vm(|v| v.no_allow_host_writes = true);
        let r = ResolvedConfig::resolve(&base, &DomeConfig::default(), &flags).unwrap();
        assert!(
            !r.allow_host_writes,
            "--no-X turns off a previously-enabled sandbox"
        );
    }

    #[test]
    fn resolve_omitted_flag_inherits_base() {
        // With no flag and no dome.json opinion, the base/sidecar value carries through.
        let base = ResolvedConfig {
            allow_net: true,
            ..Default::default()
        };
        let r = ResolvedConfig::resolve(&base, &DomeConfig::default(), &VmArgs::default()).unwrap();
        assert!(
            r.allow_net,
            "omitted flag + silent dome.json inherits the base"
        );
    }

    #[test]
    fn vm_args_tri_state_flag_helpers() {
        assert_eq!(
            VmArgs::default().allow_net_flag(),
            None,
            "neither flag → inherit"
        );
        assert_eq!(vm(|v| v.allow_net = true).allow_net_flag(), Some(true));
        assert_eq!(vm(|v| v.no_allow_net = true).allow_net_flag(), Some(false));
        assert_eq!(
            vm(|v| v.no_allow_host_writes = true).allow_host_writes_flag(),
            Some(false)
        );
    }

    #[test]
    fn resolve_lists_replace_on_set() {
        let dome = DomeConfig {
            ports: Some(vec!["8080:80".into(), "443:443".into()]),
            ..Default::default()
        };
        // A non-empty flag list replaces dome.json's list entirely (not an additive merge).
        let flags = vm(|v| v.port = vec!["9000:9000".into()]);
        let r = ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &flags).unwrap();
        assert_eq!(
            r.ports,
            vec!["9000:9000".to_string()],
            "flag replaces dome.json"
        );

        // An omitted flag inherits dome.json's list.
        let r =
            ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &VmArgs::default()).unwrap();
        assert_eq!(
            r.ports,
            vec!["8080:80".to_string(), "443:443".to_string()],
            "omitted flag inherits dome.json"
        );
    }

    #[test]
    fn resolve_no_list_flag_clears_a_lower_layer() {
        // `--no-port` clears a dome.json list; `--no-allow-host` clears the unified allow-list.
        let dome = DomeConfig {
            ports: Some(vec!["8080:80".into()]),
            network: Some(NetworkEntry {
                allow: Some(vec!["api.openai.com".into()]),
            }),
            ..Default::default()
        };
        let flags = vm(|v| {
            v.no_port = true;
            v.no_allow_host = true;
        });
        let r = ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &flags).unwrap();
        assert!(r.ports.is_empty(), "--no-port clears dome.json's ports");
        assert!(
            r.proxy.allow.is_empty(),
            "--no-allow-host clears the unified allow-list"
        );

        // `--no-secret` clears dome.json's secrets.
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
        let flags = vm(|v| v.no_secret = true);
        let r = ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &flags).unwrap();
        assert!(
            r.proxy.secrets.is_empty(),
            "--no-secret clears dome.json's secrets"
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
        // `--allow-host` and `network.allow` feed one unified field; with replace semantics a
        // passed `--allow-host` replaces `network.allow` rather than merging with it.
        let flags = vm(|v| v.allow_host = vec!["*.github.com".into()]);
        let r = ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &flags).unwrap();
        assert_eq!(
            r.proxy.allow,
            vec!["*.github.com".to_string()],
            "--allow-host replaces network.allow in the unified list"
        );

        // With no flag, the unified list inherits dome.json's network.allow.
        let r =
            ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &VmArgs::default()).unwrap();
        assert_eq!(r.proxy.allow, vec!["api.openai.com".to_string()]);
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
        // no higher layer is set. A dome.json list (a higher set layer) replaces the base's.
        let base = ResolvedConfig {
            cpus: Some(2),
            allow_net: true,
            ports: vec!["1:1".into()],
            mounts: vec!["/a:/a".into()],
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
            vec!["2:2".to_string()],
            "a dome.json list replaces the base's"
        );
        assert_eq!(
            r.mounts,
            vec!["/a:/a".to_string()],
            "a base list with no higher layer is inherited"
        );
    }

    #[test]
    fn resolve_provision_from_dome_json_and_empty_steps_is_none() {
        // A declared provision block with steps + allow resolves into the sidecar.
        let dome = DomeConfig {
            provision: Some(ProvisionEntry {
                steps: vec![
                    "apt-get update && apt-get install -y nodejs".into(),
                    "curl -fsSL https://get.pnpm.io/install.sh | sh -".into(),
                ],
                allow: Some(vec!["deb.debian.org".into(), "get.pnpm.io".into()]),
                secrets: None,
            }),
            ..Default::default()
        };
        let r =
            ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &VmArgs::default()).unwrap();
        let p = r.provision.expect("a declared provision block resolves");
        assert_eq!(p.steps.len(), 2);
        assert_eq!(
            p.allow,
            vec!["deb.debian.org".to_string(), "get.pnpm.io".into()]
        );

        // A block with no steps is nothing to provision → None.
        let dome = DomeConfig {
            provision: Some(ProvisionEntry {
                steps: vec![],
                allow: Some(vec!["x".into()]),
                secrets: None,
            }),
            ..Default::default()
        };
        let r =
            ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &VmArgs::default()).unwrap();
        assert!(r.provision.is_none(), "empty steps resolve to no provision");

        // With no dome.json opinion, a base-carried spec is inherited (heal/reload path).
        let base = ResolvedConfig {
            provision: Some(ProvisionSpec {
                steps: vec!["echo hi".into()],
                allow: vec![],
                secrets: vec![],
            }),
            ..Default::default()
        };
        let r = ResolvedConfig::resolve(&base, &DomeConfig::default(), &VmArgs::default()).unwrap();
        assert_eq!(
            r.provision.unwrap().steps,
            vec!["echo hi".to_string()],
            "an omitted dome.json provision inherits the base spec"
        );
    }

    #[test]
    fn resolve_provision_secrets_parse_and_auto_whitelist_hosts() {
        // provision.secrets parse via the same resolve() path as runtime secrets (mapping only),
        // and their hosts are auto-whitelisted into the provision allow-list.
        let mut secrets = HashMap::new();
        secrets.insert(
            "npm".to_string(),
            SecretEntry {
                from: "NPM_TOKEN".into(),
                hosts: vec!["registry.corp.internal".into()],
            },
        );
        let dome = DomeConfig {
            provision: Some(ProvisionEntry {
                steps: vec!["npm ci".into()],
                allow: Some(vec!["deb.debian.org".into()]),
                secrets: Some(secrets),
            }),
            ..Default::default()
        };
        let r =
            ResolvedConfig::resolve(&ResolvedConfig::default(), &dome, &VmArgs::default()).unwrap();
        let p = r
            .provision
            .as_ref()
            .expect("a declared provision block resolves");
        assert_eq!(p.secrets.len(), 1);
        assert_eq!(p.secrets[0].name, "npm");
        assert_eq!(p.secrets[0].from, "NPM_TOKEN");
        assert_eq!(
            p.secrets[0].hosts,
            vec!["registry.corp.internal".to_string()]
        );

        // The declared `allow` is kept as-is on the spec; the secret host is added only in the
        // effective (build-time) allow-list — no double-declaration required.
        assert_eq!(p.allow, vec!["deb.debian.org".to_string()]);
        assert_eq!(
            p.effective_allow(),
            vec![
                "deb.debian.org".to_string(),
                "registry.corp.internal".to_string()
            ],
            "a secret's hosts are auto-whitelisted into the provision allow-list"
        );

        // The serialized sidecar carries the mapping but never a secret *value* field.
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("NPM_TOKEN") && !json.contains("\"value\""));
    }

    #[test]
    fn effective_allow_keeps_open_network_open_and_dedupes() {
        // An empty allow = all allowed: secrets must NOT narrow it down to just their hosts.
        let open = ProvisionSpec {
            steps: vec!["s".into()],
            allow: vec![],
            secrets: vec![SecretSpec {
                name: "npm".into(),
                from: "NPM_TOKEN".into(),
                hosts: vec!["registry.corp.internal".into()],
            }],
        };
        assert!(
            open.effective_allow().is_empty(),
            "an empty allow-list stays open (all allowed) regardless of secrets"
        );

        // A host already declared in allow is not duplicated by the auto-whitelist.
        let dup = ProvisionSpec {
            steps: vec!["s".into()],
            allow: vec!["registry.corp.internal".into(), "deb.debian.org".into()],
            secrets: vec![SecretSpec {
                name: "npm".into(),
                from: "NPM_TOKEN".into(),
                hosts: vec!["registry.corp.internal".into()],
            }],
        };
        assert_eq!(
            dup.effective_allow(),
            vec![
                "registry.corp.internal".to_string(),
                "deb.debian.org".to_string()
            ],
            "an already-declared host is not whitelisted twice"
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
    fn merge_update_can_disable_a_previously_enabled_bool() {
        let mut cfg = ResolvedConfig {
            allow_net: true,
            allow_host_writes: true,
            ..Default::default()
        };
        // `--no-allow-net` turns it off; an omitted host-writes flag is left untouched.
        cfg.merge_update(&vm(|v| v.no_allow_net = true)).unwrap();
        assert!(!cfg.allow_net, "--no-allow-net disables via config");
        assert!(
            cfg.allow_host_writes,
            "an omitted flag must not clear the policy"
        );
    }

    #[test]
    fn merge_update_clears_lists_with_no_flag_and_leaves_omitted_lists() {
        let mut cfg = ResolvedConfig {
            ports: vec!["8080:80".into()],
            mounts: vec!["/a:/a".into()],
            ..Default::default()
        };
        cfg.proxy.secrets = vec![SecretSpec {
            name: "openai".into(),
            from: "OPENAI_API_KEY".into(),
            hosts: vec!["api.openai.com".into()],
        }];
        // `--no-port` clears the ports; an omitted mount/secret flag is left untouched.
        cfg.merge_update(&vm(|v| v.no_port = true)).unwrap();
        assert!(
            cfg.ports.is_empty(),
            "--no-port clears the ports via config"
        );
        assert_eq!(
            cfg.mounts,
            vec!["/a:/a".to_string()],
            "an omitted --mount must not clear the list"
        );
        assert_eq!(
            cfg.proxy.secrets.len(),
            1,
            "an omitted --secret is untouched"
        );

        // A non-empty list still replaces.
        cfg.merge_update(&vm(|v| v.mount = vec!["/b:/b".into()]))
            .unwrap();
        assert_eq!(cfg.mounts, vec!["/b:/b".to_string()], "--mount replaces");
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
