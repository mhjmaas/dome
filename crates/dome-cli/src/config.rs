use std::collections::HashMap;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct DomeConfig {
    pub cpus: Option<usize>,
    pub memory: Option<u64>,
    pub disk_size: Option<u64>,
    pub allow_net: Option<bool>,
    pub allow_host_writes: Option<bool>,
    pub ports: Option<Vec<String>>,
    pub mounts: Option<Vec<String>>,
    pub command: Option<Vec<String>>,
    pub secrets: Option<HashMap<String, SecretEntry>>,
    pub network: Option<NetworkEntry>,
    /// Declarative toolchain provisioning: steps run once in a build VM and cached as a
    /// hidden, hash-keyed checkpoint layer that later sandbox/`run` creations seed from.
    pub provision: Option<ProvisionEntry>,
    /// Host ports to expose to the guest (e.g. "3000:8080" or "5432").
    pub expose_host: Option<Vec<String>>,
    /// Persistent sandbox name for this project. Used by `dome sandbox` when no
    /// explicit name is given, before falling back to a cwd-derived slug.
    pub sandbox: Option<String>,
    /// Opt-in latest-only base retention. When true, `dome upgrade` offers to delete
    /// sandboxes pinned to a superseded OS base (after confirmation) so only the latest
    /// base remains. Off by default: sandboxes are pinned forever and old bases are
    /// reclaimed by `dome prune` once unreferenced.
    pub latest_only: Option<bool>,
    /// Directory auto-activation policy for the shell hook (`dome hook zsh`). `shell`
    /// (the default when omitted) drops you into the sandbox on `cd`; `off` disables
    /// auto-activation entirely (manual `dome sandbox shell` still works).
    pub activate: Option<ActivateMode>,
}

/// What the directory auto-activation hook does when it enters a trusted project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ActivateMode {
    /// Drop into the sandbox shell. The default when `activate` is omitted.
    Shell,
    /// Do nothing automatically; manual `dome sandbox shell` still works.
    Off,
}

impl DomeConfig {
    /// The effective auto-activation policy: the explicit `activate` field, else the
    /// default (`shell`). An omitted field auto-drops so the hook works out of the box.
    pub(crate) fn activate(&self) -> ActivateMode {
        self.activate.unwrap_or(ActivateMode::Shell)
    }
}

/// A secret to inject via the proxy.
/// Example: `{ "from": "OPENAI_API_KEY", "hosts": ["api.openai.com"] }`
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct SecretEntry {
    /// Host environment variable containing the real value.
    pub from: String,
    /// Domains where this secret may be sent.
    pub hosts: Vec<String>,
}

/// Network access policy.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct NetworkEntry {
    /// Allowed domain patterns. Empty or absent = allow all.
    pub allow: Option<Vec<String>>,
}

/// Declarative toolchain provisioning. The `steps` run once (as root, sequentially,
/// stop-on-first-failure) inside a build VM whose result is snapshotted as a hidden
/// checkpoint keyed by a hash of the spec; later sandbox/`run` creations seed from that
/// cached layer. `allow` is the *provision-time* network allow-list, separate from the
/// runtime `network.allow` (empty/unset = all allowed). Installs the toolchain only
/// (node, pnpm, gcc, python3) — project-dependency installs belong in the live sandbox.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct ProvisionEntry {
    /// Ordered shell commands run as root inside the build VM, each via `sh -c`.
    #[serde(default)]
    pub steps: Vec<String>,
    /// Provision-time network allow-list. Empty/absent = all allowed.
    pub allow: Option<Vec<String>>,
    /// Provision-time secrets, same shape as the runtime [`SecretEntry`] map. Injected via
    /// the egress proxy during the build: the guest only ever sees a random placeholder and
    /// the real value is substituted on egress to the secret's matched `hosts`, which are
    /// auto-whitelisted into the provision allow-list. Provision-only — never affects runtime.
    pub secrets: Option<HashMap<String, SecretEntry>>,
}

/// Parse "HOST_PORT:GUEST_PORT" or "PORT" into an ExposeHostMapping.
pub(crate) fn parse_expose_host(s: &str) -> Result<dome_proxy::config::ExposeHostMapping> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => {
            let port: u16 = parts[0]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid port: '{}'", parts[0]))?;
            Ok(dome_proxy::config::ExposeHostMapping {
                host_port: port,
                guest_port: port,
            })
        }
        2 => {
            let host_port: u16 = parts[0]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid host port: '{}'", parts[0]))?;
            let guest_port: u16 = parts[1]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid guest port: '{}'", parts[1]))?;
            Ok(dome_proxy::config::ExposeHostMapping {
                host_port,
                guest_port,
            })
        }
        _ => bail!("expected HOST_PORT:GUEST_PORT or PORT format"),
    }
}

/// The `dome.json` path a config flag resolves to: the explicit `--config` value, else
/// `./dome.json`. Exposed so the heal path can test for the file's presence and report it.
pub(crate) fn config_path(config_flag: Option<&str>) -> std::path::PathBuf {
    match config_flag {
        Some(p) => std::path::PathBuf::from(p),
        None => std::path::PathBuf::from("dome.json"),
    }
}

/// The project root: the canonical directory containing the `dome.json` in use, or `None`
/// when no `dome.json` is present (an absent default `./dome.json` is not an error — `dome run`
/// still boots; there is simply no project to mount). The RUNTIME guest auto-mounts this
/// directory at [`crate::vm::GUEST_PROJECT_ROOT`]; the provision BUILD phase deliberately does
/// not (see [`crate::vm::build_provision_layer`]). Resolves relative to the current working
/// directory, matching [`config_path`] — callers that need it rooted elsewhere (the worker)
/// `set_current_dir` to the originating cwd first.
pub(crate) fn project_root(config_flag: Option<&str>) -> Option<std::path::PathBuf> {
    let path = config_path(config_flag);
    let canonical = std::fs::canonicalize(&path).ok()?;
    canonical.parent().map(|p| p.to_path_buf())
}

/// Walk up from `start` to the nearest ancestor directory containing a `dome.json`,
/// returning that directory canonicalized. Nearest/deepest wins: a `dome.json` in `start`
/// itself shadows one further up. This is the authoritative Rust counterpart to the hook's
/// pure-shell walk — `dome allow` and `dome __hook-activate` use it so the trust record and
/// the drop-in always agree on which project the cwd belongs to. Returns `None` when no
/// `dome.json` exists from `start` up to the filesystem root.
pub(crate) fn find_nearest_dome_json(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dir = std::fs::canonicalize(start).ok()?;
    loop {
        if dir.join("dome.json").is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub(crate) fn load_config(config_flag: Option<&str>) -> Result<DomeConfig> {
    let path = config_path(config_flag);

    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let cfg: DomeConfig = serde_json::from_str(&contents)
                .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;
            Ok(cfg)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if config_flag.is_some() {
                bail!("Config file not found: {}", path.display());
            }
            Ok(DomeConfig::default())
        }
        Err(e) => bail!("Failed to read {}: {}", path.display(), e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activate_defaults_to_shell_when_omitted() {
        // No `activate` key → auto-activation drops in (the default).
        let cfg: DomeConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.activate(), ActivateMode::Shell);
    }

    #[test]
    fn activate_off_disables_auto_activation() {
        let cfg: DomeConfig = serde_json::from_str(r#"{"activate":"off"}"#).unwrap();
        assert_eq!(cfg.activate(), ActivateMode::Off);
    }

    #[test]
    fn activate_shell_is_explicit_drop_in() {
        let cfg: DomeConfig = serde_json::from_str(r#"{"activate":"shell"}"#).unwrap();
        assert_eq!(cfg.activate(), ActivateMode::Shell);
    }

    #[test]
    fn unknown_activate_value_is_a_parse_error() {
        // A typo like `"on"` must fail loudly rather than silently auto-activating.
        assert!(serde_json::from_str::<DomeConfig>(r#"{"activate":"on"}"#).is_err());
    }

    #[test]
    fn walk_up_finds_dome_json_in_the_start_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dome.json"), "{}").unwrap();
        let found = find_nearest_dome_json(dir.path()).unwrap();
        assert_eq!(found, std::fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn walk_up_finds_dome_json_in_an_ancestor() {
        // A subdirectory with no dome.json resolves to the nearest ancestor that has one.
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("dome.json"), "{}").unwrap();
        let sub = root.path().join("src").join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        let found = find_nearest_dome_json(&sub).unwrap();
        assert_eq!(found, std::fs::canonicalize(root.path()).unwrap());
    }

    #[test]
    fn walk_up_picks_the_nearest_deepest_dome_json() {
        // dome.json at both root and an inner dir: the inner (deepest/nearest) one wins.
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("dome.json"), "{}").unwrap();
        let inner = root.path().join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("dome.json"), "{}").unwrap();
        let found = find_nearest_dome_json(&inner).unwrap();
        assert_eq!(found, std::fs::canonicalize(&inner).unwrap());
    }

    #[test]
    fn walk_up_returns_none_when_no_dome_json_anywhere() {
        // A temp dir under the system temp root has no dome.json up the chain to "/".
        let dir = tempfile::tempdir().unwrap();
        assert!(find_nearest_dome_json(dir.path()).is_none());
    }

    #[test]
    fn project_root_is_the_dir_containing_an_explicit_dome_json() {
        // With an explicit (existing) --config, the project root is that file's directory —
        // the dir the runtime auto-mounts into the guest at GUEST_PROJECT_ROOT.
        let dir = tempfile::tempdir().expect("tempdir");
        let dome_json = dir.path().join("dome.json");
        std::fs::write(&dome_json, "{}").expect("write dome.json");

        let root = project_root(Some(dome_json.to_str().unwrap())).expect("root present");
        // Canonicalize the expected dir too: tempdir() may sit under a symlinked /tmp.
        assert_eq!(root, std::fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn project_root_is_none_when_dome_json_is_absent() {
        // No dome.json → no project to mount (a plain `dome run` in a bare dir is unaffected).
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("dome.json");
        assert!(project_root(Some(missing.to_str().unwrap())).is_none());
    }
}
