//! Opt-in "latest-only" base retention policy for `dome upgrade`.
//!
//! By default sandboxes are pinned to their OS base version forever and old bases are
//! reclaimed by `dome prune` only once nothing references them (pin-forever + GC). This
//! module implements the opt-in alternative: when enabled (via the `latest_only` config
//! field or the `--latest-only` upgrade flag), an upgrade lists every sandbox still
//! pinned to a now-superseded base version, asks for explicit confirmation, and — only
//! if confirmed — deletes those sandboxes and runs the mark-and-sweep so just the latest
//! base remains. There is no block-level migration (it would corrupt the filesystem);
//! the policy deletes superseded-version sandboxes rather than migrating them.

use anyhow::Result;

use crate::gc::{self, SweepStats};
use crate::sandbox;

/// A sandbox still pinned to an OS base version that the upgrade has superseded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SupersededSandbox {
    pub name: String,
    pub base_version: String,
}

/// Enumerate sandboxes whose pinned base version is not `current_version` — i.e. every
/// sandbox left on a superseded base after an upgrade. Sandboxes already on the current
/// base are left out (they are exactly what the policy keeps).
pub(crate) fn superseded_sandboxes(
    data_dir: &str,
    current_version: &str,
) -> Result<Vec<SupersededSandbox>> {
    Ok(sandbox::collect_sandbox_base_versions(data_dir)?
        .into_iter()
        .filter(|(_, base_version)| base_version != current_version)
        .map(|(name, base_version)| SupersededSandbox { name, base_version })
        .collect())
}

/// Whether the latest-only retention policy is in effect for this upgrade. The policy
/// is opt-in and off by default: it is enabled by either the `--latest-only` upgrade
/// flag or a `"latest_only": true` field in `dome.json`.
pub(crate) fn policy_enabled(flag: bool, config_field: Option<bool>) -> bool {
    flag || config_field.unwrap_or(false)
}

/// What applying the latest-only policy did, for reporting back to the user.
#[derive(Debug)]
pub(crate) enum RetentionOutcome {
    /// No sandbox was pinned to a superseded base — nothing to reclaim, no prompt shown.
    NothingToReclaim,
    /// Superseded sandboxes existed but the user declined; everything left intact.
    Declined,
    /// The user confirmed: `deleted` sandboxes were removed and the sweep reclaimed
    /// their now-orphaned chunks and superseded base images.
    Reclaimed { deleted: usize, sweep: SweepStats },
}

/// Apply the opt-in latest-only retention policy after upgrading to `current_version`.
///
/// Enumerates every sandbox still pinned to a superseded base; if there are none, it is
/// a no-op (`confirm` is never called). Otherwise it asks `confirm` for an explicit
/// yes/no — declining leaves every sandbox and base image untouched. On confirmation it
/// deletes the superseded sandboxes' indexes and runs the mark-and-sweep so only the
/// latest base remains. There is no migration: a superseded sandbox is deleted, never
/// silently rebased.
pub(crate) fn apply_latest_only(
    data_dir: &str,
    current_version: &str,
    confirm: impl FnOnce(&[SupersededSandbox]) -> Result<bool>,
) -> Result<RetentionOutcome> {
    let superseded = superseded_sandboxes(data_dir, current_version)?;
    if superseded.is_empty() {
        return Ok(RetentionOutcome::NothingToReclaim);
    }

    if !confirm(&superseded)? {
        return Ok(RetentionOutcome::Declined);
    }

    let mut deleted = 0;
    for sb in &superseded {
        match sandbox::delete_sandbox_index(data_dir, &sb.name) {
            Ok(()) => deleted += 1,
            // A sandbox open in a live session (or otherwise un-removable) must not abort
            // the whole policy: warn and leave it pinned rather than failing the upgrade.
            Err(e) => eprintln!("dome: could not remove sandbox '{}': {:#}", sb.name, e),
        }
    }

    // Reclaim the chunks and now-unreferenced superseded base images the deletions freed.
    let sweep = gc::sweep(data_dir)?;
    Ok(RetentionOutcome::Reclaimed { deleted, sweep })
}

/// Interpret a typed confirmation line. Deleting sandboxes is destructive, so only an
/// explicit `y`/`yes` (case-insensitive, surrounding whitespace ignored) confirms;
/// every other response — including a bare Enter — declines.
fn parse_confirm_response(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The real terminal confirmation used by `dome upgrade`: list the superseded sandboxes
/// that would be deleted and read an explicit yes/no from stdin. Wired as the `confirm`
/// callback of [`apply_latest_only`]; the orchestration itself is exercised in tests
/// with stub closures so this terminal I/O stays out of the test path.
pub(crate) fn interactive_confirm(superseded: &[SupersededSandbox]) -> Result<bool> {
    use std::io::Write;

    eprintln!(
        "dome: latest-only retention would DELETE {} sandbox(es) pinned to a superseded \
         OS base:",
        superseded.len()
    );
    for sb in superseded {
        eprintln!("  - {} (base {})", sb.name, sb.base_version);
    }
    eprint!("dome: delete these sandboxes and reclaim their disk space? [y/N]: ");
    std::io::stderr().flush().ok();

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(parse_confirm_response(&line))
}

/// Print a one-line summary of what the policy did, for the end of `dome upgrade`.
pub(crate) fn report_outcome(outcome: &RetentionOutcome) {
    match outcome {
        RetentionOutcome::NothingToReclaim => {
            eprintln!("dome: latest-only — no sandbox is on a superseded base; nothing to reclaim.")
        }
        RetentionOutcome::Declined => {
            eprintln!("dome: latest-only — declined; all sandboxes and base images left intact.")
        }
        RetentionOutcome::Reclaimed { deleted, sweep } => eprintln!(
            "dome: latest-only — deleted {} sandbox(es); reclaimed {} chunk(s) ({}) and {} base \
             image(s).",
            deleted,
            sweep.chunks_removed,
            gc::format_bytes(sweep.bytes_removed),
            sweep.bases_removed
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a sandbox index pinned to the given OS base version.
    fn write_sandbox(data_dir: &str, name: &str, base_version: &str) {
        let sb_dir = format!("{}/sandboxes", data_dir);
        std::fs::create_dir_all(&sb_dir).unwrap();
        let mut idx = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        idx.fallback_path = Some(crate::assets::versioned_rootfs_path(data_dir, base_version));
        idx.save(&format!("{}/{}.idx", sb_dir, name)).unwrap();
    }

    /// Write a versioned base image file on disk.
    fn write_base(data_dir: &str, version: &str) {
        std::fs::write(
            crate::assets::versioned_rootfs_path(data_dir, version),
            b"base",
        )
        .unwrap();
    }

    fn base_exists(data_dir: &str, version: &str) -> bool {
        std::path::Path::new(&crate::assets::versioned_rootfs_path(data_dir, version)).exists()
    }

    fn sandbox_exists(data_dir: &str, name: &str) -> bool {
        std::path::Path::new(&format!("{}/sandboxes/{}.idx", data_dir, name)).exists()
    }

    #[test]
    fn confirmed_policy_deletes_superseded_sandboxes_and_sweeps_their_base() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();

        // Upgraded to 2.0.0: both base images are on disk for now.
        std::fs::write(format!("{}/VERSION", data_dir), "2.0.0\n").unwrap();
        write_base(data_dir, "1.0.0");
        write_base(data_dir, "2.0.0");
        write_sandbox(data_dir, "web", "1.0.0"); // superseded
        write_sandbox(data_dir, "fresh", "2.0.0"); // current

        let outcome = apply_latest_only(data_dir, "2.0.0", |_| Ok(true)).unwrap();

        match outcome {
            RetentionOutcome::Reclaimed { deleted, .. } => assert_eq!(deleted, 1),
            other => panic!("expected Reclaimed, got {:?}", other),
        }
        assert!(
            !sandbox_exists(data_dir, "web"),
            "superseded sandbox deleted"
        );
        assert!(sandbox_exists(data_dir, "fresh"), "current sandbox kept");
        assert!(!base_exists(data_dir, "1.0.0"), "superseded base swept");
        assert!(base_exists(data_dir, "2.0.0"), "latest base kept");
    }

    #[test]
    fn policy_is_off_by_default_and_opt_in_via_flag_or_config() {
        // Off by default: neither the flag nor the config field enables it.
        assert!(!policy_enabled(false, None));
        assert!(!policy_enabled(false, Some(false)));
        // Either the `--latest-only` flag or the config field opts in.
        assert!(policy_enabled(true, None));
        assert!(policy_enabled(false, Some(true)));
        assert!(policy_enabled(true, Some(false)));
    }

    #[test]
    fn declined_policy_leaves_every_sandbox_and_base_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();

        std::fs::write(format!("{}/VERSION", data_dir), "2.0.0\n").unwrap();
        write_base(data_dir, "1.0.0");
        write_base(data_dir, "2.0.0");
        write_sandbox(data_dir, "web", "1.0.0");
        write_sandbox(data_dir, "fresh", "2.0.0");

        let outcome = apply_latest_only(data_dir, "2.0.0", |_| Ok(false)).unwrap();

        assert!(
            matches!(outcome, RetentionOutcome::Declined),
            "declining yields Declined"
        );
        // Nothing is deleted: every sandbox and every base survives untouched.
        assert!(sandbox_exists(data_dir, "web"), "superseded sandbox kept");
        assert!(sandbox_exists(data_dir, "fresh"));
        assert!(base_exists(data_dir, "1.0.0"), "superseded base kept");
        assert!(base_exists(data_dir, "2.0.0"));
    }

    #[test]
    fn nothing_to_reclaim_never_prompts_for_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();

        std::fs::write(format!("{}/VERSION", data_dir), "2.0.0\n").unwrap();
        write_base(data_dir, "2.0.0");
        // Every sandbox is already on the current base.
        write_sandbox(data_dir, "fresh", "2.0.0");

        // The confirm closure must never run when there is nothing to reclaim.
        let outcome = apply_latest_only(data_dir, "2.0.0", |_| {
            panic!("confirmation must not be requested when nothing is superseded")
        })
        .unwrap();

        assert!(matches!(outcome, RetentionOutcome::NothingToReclaim));
        assert!(sandbox_exists(data_dir, "fresh"));
    }

    #[test]
    fn confirm_response_requires_an_explicit_yes() {
        // Deleting sandboxes is destructive, so only an explicit yes proceeds; anything
        // else (including a bare Enter) is treated as a decline.
        for yes in ["y", "Y", "yes", "YES", "  yes  ", "Yes"] {
            assert!(parse_confirm_response(yes), "{:?} should confirm", yes);
        }
        for no in ["", "n", "N", "no", "nope", "\n", "  ", "q", "yeah"] {
            assert!(!parse_confirm_response(no), "{:?} should decline", no);
        }
    }

    #[test]
    fn superseded_lists_only_sandboxes_off_the_current_base() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();

        // Two sandboxes on an old base, one already on the new base.
        write_sandbox(data_dir, "web", "1.0.0");
        write_sandbox(data_dir, "api", "1.0.0");
        write_sandbox(data_dir, "fresh", "2.0.0");

        let mut superseded = superseded_sandboxes(data_dir, "2.0.0").unwrap();
        superseded.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(
            superseded,
            vec![
                SupersededSandbox {
                    name: "api".to_string(),
                    base_version: "1.0.0".to_string(),
                },
                SupersededSandbox {
                    name: "web".to_string(),
                    base_version: "1.0.0".to_string(),
                },
            ],
            "only the sandboxes pinned to the superseded 1.0.0 base are listed"
        );
    }
}
