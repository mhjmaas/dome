//! Per-directory trust gating for directory auto-activation.
//!
//! Auto-activation boots a VM and drops you into it the moment you `cd` into a project,
//! so it must never fire on a `dome.json` the developer has not explicitly vouched for —
//! a hostile `dome.json` in a cloned repo could otherwise mount paths or open ports the
//! instant you enter the directory. The gate is an explicit one-time `dome allow` per
//! project, recorded as a trust record keyed to the canonical project directory and a
//! hash of the whole `dome.json`. Any later edit to `dome.json` changes the hash and
//! re-locks the project until it is re-approved.

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

/// Hex sha256 of the whole `dome.json` file bytes. Hashing the raw bytes means ANY edit —
/// even whitespace — changes the hash and re-locks the project, which is the safe default:
/// re-approval is cheap, and a silently-accepted edit is exactly the risk the gate exists
/// to prevent.
pub(crate) fn config_hash(dome_json_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(dome_json_bytes);
    format!("{:x}", hasher.finalize())
}

/// The on-disk trust record: the canonical project directory and the `dome.json` hash that
/// was approved. Both must match for a project to count as trusted, so moving a project
/// (different path) or editing its `dome.json` (different hash) both re-lock it.
#[derive(serde::Serialize, serde::Deserialize)]
struct TrustRecord {
    dir: String,
    hash: String,
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
    };
    let path = dir.join(format!("{}.json", dir_key(&canonical)));
    std::fs::write(&path, serde_json::to_vec(&record)?)
        .with_context(|| format!("writing trust record {}", path.display()))?;
    let _ = set_private(&path);
    Ok(())
}

/// Whether `project_dir` is trusted at its current `dome.json` content. True only when a
/// record exists for the canonical directory AND its stored hash matches the current
/// `dome.json` bytes. A missing record (never approved) or a hash mismatch (edited since
/// approval) both return false, so the caller stays on the host.
pub(crate) fn is_trusted(data_dir: &str, project_dir: &Path, dome_json_bytes: &[u8]) -> bool {
    let Ok(canonical) = std::fs::canonicalize(project_dir) else {
        return false;
    };
    let path = allowed_dir(data_dir).join(format!("{}.json", dir_key(&canonical)));
    let Ok(bytes) = std::fs::read(&path) else {
        return false;
    };
    let Ok(record) = serde_json::from_slice::<TrustRecord>(&bytes) else {
        return false;
    };
    record.dir == canonical.to_string_lossy() && record.hash == config_hash(dome_json_bytes)
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
    fn re_approval_after_an_edit_restores_trust() {
        let (project, data, bytes) = fixture(r#"{"sandbox":"web"}"#);
        let dd = data.path().to_str().unwrap();
        record_trust(dd, project.path(), &bytes).unwrap();

        let edited = br#"{"sandbox":"web","allow_net":true}"#;
        record_trust(dd, project.path(), edited).unwrap();
        assert!(is_trusted(dd, project.path(), edited));
    }

    #[test]
    fn config_hash_is_stable_and_content_sensitive() {
        assert_eq!(config_hash(b"{}"), config_hash(b"{}"));
        assert_ne!(config_hash(b"{}"), config_hash(b"{ }"));
    }
}
