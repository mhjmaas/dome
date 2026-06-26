//! Age-based audit retention, invoked on-demand from `dome prune`.
//!
//! The writer enforces the always-on *size* ceiling inline at segment rotation (see
//! [`crate::writer`]). This module is the complementary *age* housekeeping: drop whole
//! sessions that have seen no activity for ~30 days. It is deliberately a pure filesystem
//! function with no background timer — `dome prune` calls it alongside the chunk
//! mark-and-sweep, consistent with the project's on-demand-prune philosophy.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Default age threshold for `dome prune` audit reaping: a session untouched for ~30 days
/// is reclaimed. Baked in (user-configurable retention is out of scope for v1).
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// What an age-based audit reap reclaimed, for the `dome prune` summary.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReapStats {
    /// Number of session directories removed.
    pub sessions_removed: u64,
    /// Total size of the removed sessions' files, in bytes.
    pub bytes_removed: u64,
}

/// Reap every audit session whose most recent activity is older than `max_age`, across all
/// sandboxes under `audit_dir` (`<audit_dir>/<sandbox>/<session>/`).
///
/// A session's age is its newest contained file's mtime — the last time anything was written
/// — so a long-lived, recently-active session is never reaped while a stale one is. Ephemeral
/// and persistent sandbox logs live under the same root and are reaped identically. Removal
/// is whole-session: the directory and all its segments go together. Missing/unreadable
/// `audit_dir` is not an error — there is simply nothing to reap.
pub fn reap_aged_sessions(audit_dir: &Path, max_age: Duration) -> ReapStats {
    let now = SystemTime::now();
    let mut stats = ReapStats::default();

    let Ok(sandboxes) = std::fs::read_dir(audit_dir) else {
        return stats;
    };
    for sandbox in sandboxes.filter_map(|e| e.ok()) {
        let Ok(sessions) = std::fs::read_dir(sandbox.path()) else {
            continue;
        };
        for session in sessions.filter_map(|e| e.ok()) {
            let dir = session.path();
            if !dir.is_dir() {
                continue;
            }
            let (last_activity, size) = scan_session(&dir);
            let age = now.duration_since(last_activity).unwrap_or_default();
            if age <= max_age {
                continue;
            }
            if std::fs::remove_dir_all(&dir).is_ok() {
                stats.sessions_removed += 1;
                stats.bytes_removed += size;
            }
        }
    }
    stats
}

/// The newest mtime among a session dir's files (its last-activity instant) and the total
/// byte size of those files. An empty session falls back to the directory's own mtime.
fn scan_session(dir: &Path) -> (SystemTime, u64) {
    let mut newest: Option<SystemTime> = None;
    let mut size = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            size += meta.len();
            if let Ok(modified) = meta.modified() {
                if newest.is_none_or(|n| modified > n) {
                    newest = Some(modified);
                }
            }
        }
    }
    // No segments (or unreadable mtimes): fall back to the session dir's own mtime so an
    // empty stale directory is still reclaimable.
    let last_activity = newest.unwrap_or_else(|| {
        std::fs::metadata(dir)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });
    (last_activity, size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    /// Write a one-segment session under `<audit_dir>/<sandbox>/<session>` and stamp the
    /// segment's mtime to `now - age` so the reaper sees a session of the given age.
    fn seed_session(audit_dir: &Path, sandbox: &str, session: &str, age: Duration) -> u64 {
        let dir = audit_dir.join(sandbox).join(session);
        fs::create_dir_all(&dir).unwrap();
        let seg = dir.join("events-0001.jsonl");
        let body = b"{\"kind\":\"conn_open\"}\n";
        let mut f = fs::File::create(&seg).unwrap();
        f.write_all(body).unwrap();
        f.sync_all().unwrap();
        let when = SystemTime::now() - age;
        fs::File::options()
            .write(true)
            .open(&seg)
            .unwrap()
            .set_modified(when)
            .unwrap();
        body.len() as u64
    }

    /// Stamp an extra segment with the given age into an existing session.
    fn add_segment(audit_dir: &Path, sandbox: &str, session: &str, name: &str, age: Duration) {
        let seg = audit_dir.join(sandbox).join(session).join(name);
        let mut f = fs::File::create(&seg).unwrap();
        f.write_all(b"{\"kind\":\"http_request\"}\n").unwrap();
        f.sync_all().unwrap();
        fs::File::options()
            .write(true)
            .open(&seg)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    #[test]
    fn keeps_long_lived_session_with_recent_segment() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // An old first segment but a fresh rotated one — a live, busy session.
        seed_session(root, "web", "busy", Duration::from_secs(45 * 86_400));
        add_segment(root, "web", "busy", "events-0002.jsonl", Duration::from_secs(30));

        let stats = reap_aged_sessions(root, DEFAULT_MAX_AGE);

        assert_eq!(stats.sessions_removed, 0, "newest segment keeps it alive");
        assert!(root.join("web").join("busy").exists());
    }

    #[test]
    fn reaps_ephemeral_and_persistent_under_same_policy_and_sums_segments() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let aged = Duration::from_secs(40 * 86_400);
        // An ephemeral sandbox (single aged session) and a persistent one (an aged session
        // with two segments) — both reaped; a fresh session under the persistent one stays.
        let eph = seed_session(root, "eph-run", "sess-a", aged);
        let per1 = seed_session(root, "persist", "sess-old", aged);
        add_segment(root, "persist", "sess-old", "events-0002.jsonl", aged);
        let per2 = std::fs::metadata(
            root.join("persist").join("sess-old").join("events-0002.jsonl"),
        )
        .unwrap()
        .len();
        seed_session(root, "persist", "sess-new", Duration::from_secs(120));

        let stats = reap_aged_sessions(root, DEFAULT_MAX_AGE);

        assert_eq!(stats.sessions_removed, 2, "both aged sessions reaped");
        assert_eq!(
            stats.bytes_removed,
            eph + per1 + per2,
            "all segments of every reaped session summed"
        );
        assert!(!root.join("eph-run").join("sess-a").exists());
        assert!(!root.join("persist").join("sess-old").exists());
        assert!(root.join("persist").join("sess-new").exists());
    }

    #[test]
    fn missing_audit_dir_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let absent = tmp.path().join("audit-does-not-exist");
        let stats = reap_aged_sessions(&absent, DEFAULT_MAX_AGE);
        assert_eq!(stats, ReapStats::default());
    }

    #[test]
    fn reaps_aged_session_reports_bytes_and_keeps_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let old_bytes = seed_session(root, "web", "old-sess", Duration::from_secs(40 * 86_400));
        seed_session(root, "web", "fresh-sess", Duration::from_secs(60));

        let stats = reap_aged_sessions(root, DEFAULT_MAX_AGE);

        assert_eq!(stats.sessions_removed, 1, "only the aged session is reaped");
        assert_eq!(stats.bytes_removed, old_bytes, "reclaimed bytes reported");
        assert!(
            !root.join("web").join("old-sess").exists(),
            "aged session removed"
        );
        assert!(
            root.join("web").join("fresh-sess").exists(),
            "fresh session kept"
        );
    }
}
