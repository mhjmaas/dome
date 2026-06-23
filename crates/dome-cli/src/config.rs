use std::collections::HashMap;

use anyhow::{bail, Result};
use serde::Deserialize;

#[derive(Default, Deserialize)]
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
}

/// A secret to inject via the proxy.
/// Example: `{ "from": "OPENAI_API_KEY", "hosts": ["api.openai.com"] }`
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SecretEntry {
    /// Host environment variable containing the real value.
    pub from: String,
    /// Domains where this secret may be sent.
    pub hosts: Vec<String>,
}

/// Network access policy.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct NetworkEntry {
    /// Allowed domain patterns. Empty or absent = allow all.
    pub allow: Option<Vec<String>>,
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
