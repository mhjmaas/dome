//! Per-directory trust gating for directory auto-activation.
//!
//! Auto-activation boots a VM and drops you into it the moment you `cd` into a project,
//! so it must never fire on a `dome.json` the developer has not explicitly vouched for —
//! a hostile `dome.json` in a cloned repo could otherwise mount paths or open ports the
//! instant you enter the directory. The gate is an explicit one-time `dome allow` per
//! project, recorded as a trust record keyed to the canonical project directory and a
//! hash of the *normalized* `dome.json` (see [`config_hash`]). Any later edit that changes
//! what dome acts on re-locks the project until it is re-approved; cosmetic churn that does
//! not (reformatting, key reordering, an unknown key) leaves trust intact.

use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// The directory holding one trust record per allowed project, created 0700 on first write.
fn allowed_dir(data_dir: &str) -> std::path::PathBuf {
    Path::new(data_dir).join("allowed")
}

/// Hex sha256 of a project's canonical directory path — a filesystem-safe, collision-proof
/// filename for that project's trust record (the raw path has slashes and is unbounded).
fn dir_key(canonical_dir: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_dir.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Hex sha256 of a project's *normalized* `dome.json` — the parsed [`DomeConfig`] re-serialized
/// to canonical (sorted-key) JSON, then hashed. Hashing the meaning rather than the bytes means
/// cosmetic churn that dome does not act on — reformatting, key reordering, an unrecognized key
/// an editor adds or drops — no longer re-locks a trusted project, while any change to a field
/// dome *does* act on (mounts, ports, network, provision, command, secrets, sandbox, activate…)
/// still changes the hash and re-locks until re-approved. That preserves the security purpose of
/// the gate — a silently-accepted *semantic* edit is the risk it exists to prevent — without the
/// brittleness that made a project permanently un-activatable after a benign rewrite.
///
/// A `dome.json` that does not parse falls back to a raw-byte hash: it cannot be loaded or
/// activated anyway, so there is no normalized form to compare, and a stable fingerprint still
/// lets `dome allow` re-record it.
pub(crate) fn config_hash(dome_json_bytes: &[u8]) -> String {
    let canonical = match serde_json::from_slice::<crate::config::DomeConfig>(dome_json_bytes)
        .ok()
        .and_then(|cfg| serde_json::to_value(cfg).ok())
    {
        Some(value) => canonical_json(&value),
        None => String::from_utf8_lossy(dome_json_bytes).into_owned(),
    };
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Serialize a JSON value to a deterministic canonical string: object keys sorted, no incidental
/// whitespace. Independent of serde_json's map ordering (which the `preserve_order` feature can
/// flip), so a config whose `secrets` map is written in a different key order still hashes the
/// same.
fn canonical_json(value: &serde_json::Value) -> String {
    use std::fmt::Write;
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                // A key is a JSON string; serde_json renders the escaping for us.
                let _ = write!(out, "{}:", serde_json::Value::String((*k).clone()));
                out.push_str(&canonical_json(&map[*k]));
            }
            out.push('}');
            out
        }
        serde_json::Value::Array(items) => {
            let mut out = String::from("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&canonical_json(item));
            }
            out.push(']');
            out
        }
        // Scalars: serde_json's own rendering is already canonical.
        other => other.to_string(),
    }
}

/// The on-disk trust record: the canonical project directory and the `dome.json` hash that
/// was approved. Both must match for a project to count as trusted, so moving a project
/// (different path) or editing its `dome.json` (different hash) both re-lock it.
#[derive(serde::Serialize, serde::Deserialize)]
struct TrustRecord {
    dir: String,
    hash: String,
    /// The approved `dome.json` content at the time of approval. Stored so a later
    /// `dome allow` can show what changed (a diff) before re-recording trust. Optional so
    /// legacy records written before this field (dir + hash only) still deserialize.
    #[serde(default)]
    config: Option<String>,
}

/// A previously-recorded trust grant for a project: the approved `dome.json` hash and (when
/// the record is recent enough to have it) the approved content, used by `dome allow` to show
/// an informed re-approval diff. Returned by [`prior_trust`].
pub(crate) struct PriorTrust {
    pub hash: String,
    pub config: Option<String>,
}

/// The previously-recorded trust grant for `project_dir`, or `None` if the project was never
/// allowed. Unlike [`is_trusted`] this returns the stored record regardless of whether the
/// current `dome.json` still matches, so `dome allow` can compare the approved content against
/// the edited one and show the developer what changed.
pub(crate) fn prior_trust(data_dir: &str, project_dir: &Path) -> Option<PriorTrust> {
    let canonical = std::fs::canonicalize(project_dir).ok()?;
    let path = allowed_dir(data_dir).join(format!("{}.json", dir_key(&canonical)));
    let bytes = std::fs::read(&path).ok()?;
    let record = serde_json::from_slice::<TrustRecord>(&bytes).ok()?;
    Some(PriorTrust {
        hash: record.hash,
        config: record.config,
    })
}

/// Record trust for `project_dir` pinned to the current `dome.json` content. Overwrites any
/// previous record for the same directory (re-approval after an edit). `project_dir` is
/// canonicalized so the record matches what [`is_trusted`] later looks up.
pub(crate) fn record_trust(
    data_dir: &str,
    project_dir: &Path,
    dome_json_bytes: &[u8],
) -> Result<()> {
    let canonical = std::fs::canonicalize(project_dir)
        .with_context(|| format!("resolving project dir {}", project_dir.display()))?;
    let dir = allowed_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating trust store {}", dir.display()))?;
    let _ = set_private(&dir);
    let record = TrustRecord {
        dir: canonical.to_string_lossy().to_string(),
        hash: config_hash(dome_json_bytes),
        // Store the approved content (lossy is fine — dome.json is UTF-8 text) so a later edit
        // can be diffed against what was actually approved.
        config: Some(String::from_utf8_lossy(dome_json_bytes).into_owned()),
    };
    let path = dir.join(format!("{}.json", dir_key(&canonical)));
    std::fs::write(&path, serde_json::to_vec(&record)?)
        .with_context(|| format!("writing trust record {}", path.display()))?;
    let _ = set_private(&path);
    Ok(())
}

/// Where `project_dir` stands against its trust record at the current `dome.json` content.
/// The three states the auto-activation gate must tell apart: a re-lock after an edit is a
/// stale approval the developer can refresh, which is a different message than a project that
/// was never vouched for at all.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TrustState {
    /// A record exists for this directory and its hash matches the current `dome.json`.
    Trusted,
    /// A record exists but the current `dome.json` no longer matches it (edited since approval).
    Changed,
    /// No trust record for this directory — the project was never allowed.
    NeverAllowed,
}

/// Classify `project_dir` against its stored trust record at the current `dome.json` content.
/// `Trusted` only when a record exists for the canonical directory AND its stored hash matches;
/// `Changed` when a record exists but the hash differs (edited since approval); `NeverAllowed`
/// when there is no record (or it is unreadable, which is treated as absent so the caller
/// re-locks rather than trusting a corrupt store).
pub(crate) fn trust_state(
    data_dir: &str,
    project_dir: &Path,
    dome_json_bytes: &[u8],
) -> TrustState {
    let Ok(canonical) = std::fs::canonicalize(project_dir) else {
        return TrustState::NeverAllowed;
    };
    let path = allowed_dir(data_dir).join(format!("{}.json", dir_key(&canonical)));
    let Ok(bytes) = std::fs::read(&path) else {
        return TrustState::NeverAllowed;
    };
    let Ok(record) = serde_json::from_slice::<TrustRecord>(&bytes) else {
        return TrustState::NeverAllowed;
    };
    if record.dir != canonical.to_string_lossy() {
        return TrustState::NeverAllowed;
    }
    if record.hash == config_hash(dome_json_bytes) {
        TrustState::Trusted
    } else {
        TrustState::Changed
    }
}

/// Whether `project_dir` is trusted at its current `dome.json` content. A convenience over
/// [`trust_state`] for callers that only need the yes/no gate, not which kind of "no".
pub(crate) fn is_trusted(data_dir: &str, project_dir: &Path, dome_json_bytes: &[u8]) -> bool {
    trust_state(data_dir, project_dir, dome_json_bytes) == TrustState::Trusted
}

#[cfg(unix)]
fn set_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway project dir with a `dome.json`, plus an isolated data dir for the trust
    /// store, so each test reads and writes its own store with no global state.
    fn fixture(contents: &str) -> (tempfile::TempDir, tempfile::TempDir, Vec<u8>) {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("dome.json"), contents).unwrap();
        let data = tempfile::tempdir().unwrap();
        (project, data, contents.as_bytes().to_vec())
    }

    #[test]
    fn unrecorded_project_is_untrusted() {
        let (project, data, bytes) = fixture("{}");
        assert!(!is_trusted(
            data.path().to_str().unwrap(),
            project.path(),
            &bytes
        ));
    }

    #[test]
    fn recorded_project_is_trusted() {
        let (project, data, bytes) = fixture(r#"{"sandbox":"web"}"#);
        let dd = data.path().to_str().unwrap();
        record_trust(dd, project.path(), &bytes).unwrap();
        assert!(is_trusted(dd, project.path(), &bytes));
    }

    #[test]
    fn editing_dome_json_relocks_the_project() {
        let (project, data, bytes) = fixture(r#"{"sandbox":"web"}"#);
        let dd = data.path().to_str().unwrap();
        record_trust(dd, project.path(), &bytes).unwrap();

        // Simulate an edit: the new bytes hash differently, so the record no longer matches.
        let edited = br#"{"sandbox":"web","allow_net":true}"#;
        assert!(
            !is_trusted(dd, project.path(), edited),
            "an edited dome.json must re-lock the project"
        );
        // The original content is still trusted (the record itself was not touched).
        assert!(is_trusted(dd, project.path(), &bytes));
    }

    #[test]
    fn trust_state_distinguishes_changed_from_never_allowed() {
        // The three-way contract the auto-activation warning depends on: an unrecorded project
        // is NeverAllowed, a recorded one at its approved content is Trusted, and a semantic edit
        // since approval is Changed (not silently lumped in with NeverAllowed).
        let (project, data, bytes) = fixture(r#"{"sandbox":"web"}"#);
        let dd = data.path().to_str().unwrap();
        assert_eq!(
            trust_state(dd, project.path(), &bytes),
            TrustState::NeverAllowed
        );
        record_trust(dd, project.path(), &bytes).unwrap();
        assert_eq!(trust_state(dd, project.path(), &bytes), TrustState::Trusted);
        let edited = br#"{"sandbox":"web","allow_net":true}"#;
        assert_eq!(trust_state(dd, project.path(), edited), TrustState::Changed);
    }

    #[test]
    fn re_approval_after_an_edit_restores_trust() {
        let (project, data, bytes) = fixture(r#"{"sandbox":"web"}"#);
        let dd = data.path().to_str().unwrap();
        record_trust(dd, project.path(), &bytes).unwrap();

        let edited = br#"{"sandbox":"web","allow_net":true}"#;
        record_trust(dd, project.path(), edited).unwrap();
        assert!(is_trusted(dd, project.path(), edited));
    }

    #[test]
    fn prior_trust_returns_the_approved_config_for_diffing() {
        let (project, data, bytes) = fixture(r#"{"sandbox":"web"}"#);
        let dd = data.path().to_str().unwrap();
        record_trust(dd, project.path(), &bytes).unwrap();

        let prior = prior_trust(dd, project.path()).expect("a record was just written");
        assert_eq!(prior.hash, config_hash(&bytes));
        assert_eq!(
            prior.config.as_deref(),
            Some(r#"{"sandbox":"web"}"#),
            "the approved dome.json content is stored so `dome allow` can diff a later edit"
        );
    }

    #[test]
    fn prior_trust_is_none_for_an_unrecorded_project() {
        let (project, data, _bytes) = fixture("{}");
        assert!(prior_trust(data.path().to_str().unwrap(), project.path()).is_none());
    }

    #[test]
    fn config_hash_is_invariant_to_formatting_and_key_order() {
        // Reformatting (whitespace) and reordering keys do not change what dome acts on, so the
        // fingerprint is stable across them — an editor's save no longer re-locks the project.
        let a = config_hash(br#"{"sandbox":"web","allow_net":true}"#);
        let b = config_hash(b"{\n  \"allow_net\": true,\n  \"sandbox\": \"web\"\n}\n");
        assert_eq!(a, b);
    }

    #[test]
    fn config_hash_is_sensitive_to_semantic_changes() {
        // A change to a field dome acts on must still re-lock: this is the security purpose of
        // the gate. Adding `allow_net` is a real capability change, so the hash must differ.
        assert_ne!(
            config_hash(br#"{"sandbox":"web"}"#),
            config_hash(br#"{"sandbox":"web","allow_net":true}"#),
        );
    }

    #[test]
    fn config_hash_is_invariant_to_secret_key_order() {
        // `secrets` is a map; its serialized key order must not leak into the fingerprint, or a
        // re-serialization with a different iteration order would spuriously re-lock the project.
        let a = config_hash(
            br#"{"secrets":{"A":{"from":"A","hosts":["a.com"]},"B":{"from":"B","hosts":["b.com"]}}}"#,
        );
        let b = config_hash(
            br#"{"secrets":{"B":{"from":"B","hosts":["b.com"]},"A":{"from":"A","hosts":["a.com"]}}}"#,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn config_hash_ignores_unknown_keys() {
        // The incident: `dome allow` recorded trust over a dome.json carrying an unrecognized
        // top-level `name` key; a later rewrite dropped it, changing the raw bytes and silently
        // re-locking the project forever. Unknown keys are not part of the config dome acts on,
        // so they must not affect the trust fingerprint.
        assert_eq!(
            config_hash(br#"{"sandbox":"web"}"#),
            config_hash(br#"{"sandbox":"web","name":"web"}"#),
        );
    }
}
