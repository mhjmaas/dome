//! Persistence lock for sandboxes.
//!
//! This is a *persistence* lock, not a *boot* lock: any number of sessions may boot
//! the same sandbox concurrently, but exactly one of them — the lock owner — runs
//! read-write and flatten-saves on exit. Every additional concurrent session boots
//! as a silent ephemeral fork from the current saved index: fully functional, but it
//! never saves back, so the owner's persisted disk state has exactly one writer.
//!
//! The lock is a file at `sandboxes/<name>.lock` recording the owner's PID. Stale
//! locks left by a crashed owner are reclaimed automatically via the same
//! `kill(pid, 0)` process-liveness check that `dome prune` uses to reap orphaned
//! instance directories.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Outcome of trying to acquire a sandbox's persistence lock.
pub(crate) enum Lock {
    /// We acquired persistence: this session runs read-write and saves on exit. The
    /// guard releases the lock when dropped, so it must be held for the whole session.
    Owner(LockGuard),
    /// A live session already owns persistence. This session runs as a silent
    /// ephemeral fork: it boots from the current saved index and is fully functional,
    /// but it never saves back.
    Fork,
}

/// RAII guard that releases the persistence lock on drop — but only if the lock file
/// still records our PID, so we never delete a lock another session legitimately
/// reclaimed after wrongly judging us dead. A host crash skips Drop entirely, leaving
/// a stale lock that the next boot reclaims via the liveness check.
pub(crate) struct LockGuard {
    path: PathBuf,
    pid: u32,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if read_lock_pid(&self.path) == Some(self.pid as i32) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// RAII cleanup for the staged temp file we hard-link the lock from. Once the lock
/// file exists it is an independent hard link to the same inode, so removing the temp
/// link leaves the lock intact; we just don't want the temp file accumulating.
struct StagedFile(PathBuf);

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Path for the per-process staged lock file, alongside the lock so the hard link
/// stays on the same filesystem. The PID keeps it unique across concurrent acquirers.
fn staged_path(lock_path: &Path, pid: u32) -> PathBuf {
    let mut name = lock_path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".stage.{}", pid));
    lock_path.with_file_name(name)
}

/// How many times to retry the create-after-reclaim race before giving up and forking.
/// Each loss means another session won the lock; after a few losses that session is
/// the live owner and forking is correct.
const MAX_RECLAIM_ATTEMPTS: usize = 8;

/// Try to acquire the persistence lock at `lock_path`. Returns [`Lock::Owner`] if we
/// took ownership (creating or reclaiming a stale lock), or [`Lock::Fork`] if a live
/// session already holds it.
pub(crate) fn acquire(lock_path: &Path) -> Result<Lock> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating lock directory {}", parent.display()))?;
    }

    let me = std::process::id();

    // Stage the lock contents (our PID) in a temp file, then publish it by atomically
    // hard-linking it into place. `link(2)` fails if the target already exists, giving
    // us mutual exclusion AND making the lock file appear at its path *already
    // populated* with our PID. A plain `create_new` would instead leave a brief window
    // where the file exists but is still empty — a concurrent session reading it in
    // that window would parse no PID, judge the lock garbage, and delete the live
    // owner's lock while taking ownership itself, yielding two writers. The link
    // closes that window.
    let staged = staged_path(lock_path, me);
    std::fs::write(&staged, me.to_string())
        .with_context(|| format!("writing staged lock {}", staged.display()))?;
    let _staged_guard = StagedFile(staged.clone());

    for _ in 0..MAX_RECLAIM_ATTEMPTS {
        match std::fs::hard_link(&staged, lock_path) {
            Ok(()) => {
                return Ok(Lock::Owner(LockGuard {
                    path: lock_path.to_path_buf(),
                    pid: me,
                }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match read_lock_pid(lock_path) {
                    // Held by a live process: run as an ephemeral fork.
                    Some(pid) if is_alive(pid) => return Ok(Lock::Fork),
                    // Stale (dead PID) or empty/garbage lock: reclaim it and retry the
                    // atomic link. If another session reclaims first, the next
                    // iteration sees its live PID and forks.
                    _ => {
                        let _ = std::fs::remove_file(lock_path);
                        continue;
                    }
                }
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("linking lock file {}", lock_path.display()));
            }
        }
    }

    // Lost every reclaim race to other starting sessions; one of them is the live
    // owner now, so forking is the correct outcome.
    Ok(Lock::Fork)
}

/// Read the PID recorded in a lock file, or `None` if it is missing, unreadable, or
/// does not contain a valid PID.
fn read_lock_pid(path: &Path) -> Option<i32> {
    let mut s = String::new();
    File::open(path).ok()?.read_to_string(&mut s).ok()?;
    s.trim().parse::<i32>().ok()
}

/// Process-liveness check — the same `kill(pid, 0)` pattern `dome prune` uses to
/// decide whether an orphaned instance's owner is still running.
fn is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("sandboxes").join("demo.lock")
    }

    #[test]
    fn acquires_on_a_fresh_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = lock_path(&tmp);
        match acquire(&path).unwrap() {
            Lock::Owner(_guard) => {
                assert!(path.exists(), "owning the lock should create the lock file");
                assert_eq!(read_lock_pid(&path), Some(std::process::id() as i32));
            }
            Lock::Fork => panic!("a fresh lock path must be acquirable, not a fork"),
        }
    }

    #[test]
    fn second_concurrent_acquire_is_a_fork() {
        let tmp = tempfile::tempdir().unwrap();
        let path = lock_path(&tmp);
        let _owner = match acquire(&path).unwrap() {
            Lock::Owner(g) => g,
            Lock::Fork => panic!("first acquire should own"),
        };
        // The owner's guard is still alive, so a second acquire must fork.
        assert!(matches!(acquire(&path).unwrap(), Lock::Fork));
    }

    #[test]
    fn releases_on_drop_and_can_be_reacquired() {
        let tmp = tempfile::tempdir().unwrap();
        let path = lock_path(&tmp);
        match acquire(&path).unwrap() {
            Lock::Owner(guard) => drop(guard),
            Lock::Fork => panic!("first acquire should own"),
        }
        assert!(
            !path.exists(),
            "dropping the guard should release (remove) the lock file"
        );
        assert!(
            matches!(acquire(&path).unwrap(), Lock::Owner(_)),
            "a released lock must be reacquirable as owner"
        );
    }

    #[test]
    fn reclaims_a_stale_lock_from_a_dead_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = lock_path(&tmp);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A PID that is essentially never alive — the owner crashed without releasing.
        std::fs::write(&path, "2147483646").unwrap();

        let acquired = acquire(&path).unwrap();
        assert!(
            matches!(acquired, Lock::Owner(_)),
            "a lock held by a dead PID must be reclaimed automatically"
        );
        // Keep the guard alive while we assert the file now records our PID.
        assert_eq!(read_lock_pid(&path), Some(std::process::id() as i32));
        drop(acquired);
    }

    #[test]
    fn reclaims_a_garbage_lock_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = lock_path(&tmp);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not-a-pid").unwrap();

        assert!(
            matches!(acquire(&path).unwrap(), Lock::Owner(_)),
            "an unreadable/garbage lock should be reclaimed rather than wedging the sandbox"
        );
    }

    #[test]
    fn reclaims_an_empty_lock_file() {
        // An empty lock file means a previous owner created it but died (or was killed)
        // before recording its PID. There is no live owner to protect, so it must be
        // reclaimed rather than wedging the sandbox into permanent fork mode.
        let tmp = tempfile::tempdir().unwrap();
        let path = lock_path(&tmp);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();

        assert!(
            matches!(acquire(&path).unwrap(), Lock::Owner(_)),
            "an empty (never-populated) lock should be reclaimed"
        );
    }

    #[test]
    fn owning_the_lock_leaves_no_staged_temp_file() {
        // The staged temp file used to publish the lock atomically must not survive a
        // successful acquire — otherwise temp files would accumulate per acquisition.
        let tmp = tempfile::tempdir().unwrap();
        let path = lock_path(&tmp);
        let guard = match acquire(&path).unwrap() {
            Lock::Owner(g) => g,
            Lock::Fork => panic!("fresh path should own"),
        };
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".stage."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staged temp files left behind: {:?}",
            leftovers
        );
        drop(guard);
    }

    #[test]
    fn does_not_reclaim_a_live_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = lock_path(&tmp);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Our own PID is, by definition, alive.
        std::fs::write(&path, std::process::id().to_string()).unwrap();

        assert!(
            matches!(acquire(&path).unwrap(), Lock::Fork),
            "a lock held by a live PID must not be reclaimed"
        );
    }
}
