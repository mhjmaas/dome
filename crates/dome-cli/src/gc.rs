//! On-demand garbage collection for the content-addressable store, folded into
//! `dome prune`. `dome sandbox rm` (and `checkpoint delete`) only unlink an index, so
//! the chunks and base images those indexes referenced linger until a manual sweep.
//!
//! The sweep is mark-and-sweep: walk every live index (sandboxes and checkpoints,
//! following parent chains), collect the set of referenced chunk hashes and pinned base
//! images, then delete every chunk file and every superseded base image that nothing
//! live still references. The currently installed OS base is always retained — new
//! ephemeral runs and new sandboxes need it even when no existing index points at it.

use std::collections::HashSet;

use anyhow::Result;

use dome_store::ChunkIndex;

/// Render a byte count as a coarse human size for the `dome prune` summary: GB once it
/// reaches a gigabyte, then MB, then KB, then bytes. Reclaimed chunk data is whole 64
/// KiB multiples, so sub-KB only appears for an empty sweep.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// Remove leftover instance directories whose owning VM process is no longer alive.
/// Each `dome run` boots into `instances/<pid>/`; a clean teardown removes it, but a
/// crash leaves it behind. Directories named by a still-live PID are kept; everything
/// else under `instances/` that isn't a dead PID (e.g. a stray non-numeric entry) is
/// left untouched. Returns the number of directories reclaimed. A missing `instances/`
/// directory is not an error — there is simply nothing to prune.
pub(crate) fn prune_instances(data_dir: &str) -> Result<u32> {
    let instances_dir = format!("{}/instances", data_dir);
    let entries = match std::fs::read_dir(&instances_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };

    let mut removed = 0u32;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        // The same `kill(pid, 0)` liveness check used to reclaim stale sandbox locks.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            std::fs::remove_dir_all(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// What a mark-and-sweep over the CAS store reclaimed, for the `dome prune` summary.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SweepStats {
    /// Number of unreferenced chunk files deleted.
    pub chunks_removed: u64,
    /// Total size of the deleted chunk files, in bytes.
    pub bytes_removed: u64,
    /// Number of superseded, unreferenced base images deleted.
    pub bases_removed: u64,
}

/// Mark-and-sweep the CAS store: delete every chunk file no live index references, and
/// every superseded base image no live index is pinned to. The currently installed OS
/// base is always retained. Returns what was reclaimed for reporting. Missing `chunks/`
/// or data dir is not an error — there is simply nothing to sweep.
pub(crate) fn sweep(data_dir: &str) -> Result<SweepStats> {
    let (chunk_refs, base_refs) = collect_referenced(data_dir)?;
    let mut stats = SweepStats::default();

    let chunks_dir = format!("{}/chunks", data_dir);
    match std::fs::read_dir(&chunks_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                let Some(hash) = name.to_str() else { continue };
                if chunk_refs.contains(hash) {
                    continue;
                }
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                std::fs::remove_file(entry.path())?;
                stats.chunks_removed += 1;
                stats.bytes_removed += size;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::anyhow!("failed to read {}: {}", chunks_dir, e)),
    }

    sweep_bases(data_dir, &base_refs, &mut stats)?;
    Ok(stats)
}

/// Delete every versioned base image (`rootfs-<version>.ext4`) that no live index is
/// pinned to — except the currently installed OS version, which is always retained
/// because new ephemeral runs and new sandboxes resolve their never-written chunks
/// through it even when no existing index references it yet.
fn sweep_bases(data_dir: &str, base_refs: &HashSet<String>, stats: &mut SweepStats) -> Result<()> {
    // The active base: never reclaimed regardless of whether an index references it.
    let current_base = crate::assets::installed_version(data_dir)
        .map(|v| crate::assets::versioned_rootfs_path(data_dir, &v));

    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow::anyhow!("failed to read {}: {}", data_dir, e)),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Only versioned base images are candidates; the kernel/initramfs and index
        // directories are off-limits.
        if !(name.starts_with("rootfs-") && name.ends_with(".ext4")) {
            continue;
        }
        let Some(full) = entry.path().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if base_refs.contains(&full) || current_base.as_deref() == Some(full.as_str()) {
            continue;
        }
        std::fs::remove_file(entry.path())?;
        stats.bases_removed += 1;
    }
    Ok(())
}

/// Collect every chunk hash and pinned base image referenced by any live index. Walks
/// `sandboxes/`, `checkpoints/`, and `provision/`, following each index's parent chain so a
/// chained checkpoint's ancestors are marked too. A corrupt or unreadable index does
/// not abort the sweep — it is skipped with a warning, so one bad file never strands
/// the rest of the store as unreclaimable (and never risks deleting live chunks on the
/// basis of an index we failed to read).
///
/// `provision/` is included to exempt the provision cache from orphan reclamation: a
/// sandbox seeded from a layer **flattens** it (copies the index with no parent pointer
/// back to the layer), so following references alone would leave the layer's chunks
/// unreferenced the moment no seeded sandbox happens to share them — and the next sweep
/// would reclaim chunks a still-cached `<hash>.idx` depends on. Marking the layer indexes
/// keeps their chunks alive independently of any sandbox. Their `.failed` debug disks use a
/// different extension and are intentionally *not* marked, so `dome prune` still reclaims
/// them.
fn collect_referenced(data_dir: &str) -> Result<(HashSet<String>, HashSet<String>)> {
    let mut chunk_refs = HashSet::new();
    let mut base_refs = HashSet::new();

    for sub in ["sandboxes", "checkpoints", "provision"] {
        let dir = format!("{}/{}", data_dir, sub);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(anyhow::anyhow!("failed to read {}: {}", dir, e)),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("idx") {
                continue;
            }
            let Some(path_str) = path.to_str() else {
                continue;
            };
            if let Err(e) = collect_index_refs(path_str, &mut chunk_refs, &mut base_refs) {
                eprintln!(
                    "dome: skipping unreadable index '{}' during prune: {:#}",
                    path_str, e
                );
            }
        }
    }

    Ok((chunk_refs, base_refs))
}

/// Add every non-ZERO chunk hash and pinned base referenced by `index_path` and its
/// entire parent chain to the given sets. Follows `parent_path` links (guarding against
/// cycles); a missing parent is corruption and surfaces as an error so the caller can
/// skip this index rather than under-mark its chunks.
fn collect_index_refs(
    index_path: &str,
    chunk_refs: &mut HashSet<String>,
    base_refs: &mut HashSet<String>,
) -> Result<()> {
    let mut current = Some(index_path.to_string());
    let mut visited = HashSet::new();
    while let Some(path) = current {
        if !visited.insert(path.clone()) {
            break; // cycle guard — a self-referential chain would otherwise loop forever
        }
        let idx = ChunkIndex::load(&path)?;
        for i in 0..idx.num_chunks() {
            if let Some(h) = idx.get_hash(i) {
                if h != "ZERO" {
                    chunk_refs.insert(h.to_string());
                }
            }
        }
        if let Some(base) = &idx.fallback_path {
            base_refs.insert(base.clone());
        }
        current = idx.parent_path.clone();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// Write a CAS index with the given non-ZERO chunk hashes and optional base/parent.
    fn write_index(
        path: &str,
        chunks: &[(usize, &str)],
        fallback: Option<&str>,
        parent: Option<&str>,
    ) {
        let mut idx = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        for &(i, h) in chunks {
            idx.set_hash(i, h.to_string());
        }
        idx.fallback_path = fallback.map(|s| s.to_string());
        idx.parent_path = parent.map(|s| s.to_string());
        idx.save(path).unwrap();
    }

    #[test]
    fn collect_referenced_gathers_chunk_hashes_from_sandboxes_and_checkpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(format!("{}/sandboxes", data_dir)).unwrap();
        std::fs::create_dir_all(format!("{}/checkpoints", data_dir)).unwrap();

        write_index(
            &format!("{}/sandboxes/web.idx", data_dir),
            &[(0, "aaaa"), (1, "bbbb")],
            Some(&format!("{}/rootfs-1.0.0.ext4", data_dir)),
            None,
        );
        write_index(
            &format!("{}/checkpoints/base.idx", data_dir),
            &[(0, "cccc")],
            Some(&format!("{}/rootfs-1.0.0.ext4", data_dir)),
            None,
        );

        let (chunks, bases) = collect_referenced(data_dir).unwrap();
        let expected_chunks: HashSet<String> = ["aaaa", "bbbb", "cccc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(chunks, expected_chunks);
        assert!(bases.contains(&format!("{}/rootfs-1.0.0.ext4", data_dir)));
    }

    #[test]
    fn collect_referenced_follows_parent_chains() {
        // A chained checkpoint (created with `--from`) references chunks that live only
        // in its parent index. The mark phase must follow the chain, else those parent
        // chunks would be swept while still reachable.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(format!("{}/checkpoints", data_dir)).unwrap();

        let parent = format!("{}/checkpoints/parent.idx", data_dir);
        let child = format!("{}/checkpoints/child.idx", data_dir);
        write_index(&parent, &[(0, "parentchunk")], None, None);
        write_index(&child, &[(1, "childchunk")], None, Some(&parent));

        let (chunks, _bases) = collect_referenced(data_dir).unwrap();
        assert!(
            chunks.contains("parentchunk"),
            "a parent's chunk is still reachable through the child"
        );
        assert!(chunks.contains("childchunk"));
    }

    /// Write a fake chunk file (named by `hash`) with `len` bytes into the chunk store.
    fn write_chunk(data_dir: &str, hash: &str, len: usize) {
        let chunks_dir = format!("{}/chunks", data_dir);
        std::fs::create_dir_all(&chunks_dir).unwrap();
        std::fs::write(format!("{}/{}", chunks_dir, hash), vec![0u8; len]).unwrap();
    }

    fn chunk_exists(data_dir: &str, hash: &str) -> bool {
        std::path::Path::new(&format!("{}/chunks/{}", data_dir, hash)).exists()
    }

    #[test]
    fn sweep_deletes_unreferenced_chunks_and_keeps_referenced_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(format!("{}/sandboxes", data_dir)).unwrap();

        // One referenced chunk (held by a live sandbox) and one orphan.
        write_chunk(data_dir, "live", 64 * 1024);
        write_chunk(data_dir, "orphan", 64 * 1024);
        write_index(
            &format!("{}/sandboxes/web.idx", data_dir),
            &[(0, "live")],
            None,
            None,
        );

        let stats = sweep(data_dir).unwrap();

        assert!(chunk_exists(data_dir, "live"), "referenced chunk survives");
        assert!(!chunk_exists(data_dir, "orphan"), "orphan chunk reclaimed");
        assert_eq!(stats.chunks_removed, 1);
        assert_eq!(stats.bytes_removed, 64 * 1024);
    }

    fn base_exists(data_dir: &str, version: &str) -> bool {
        std::path::Path::new(&crate::assets::versioned_rootfs_path(data_dir, version)).exists()
    }

    #[test]
    fn sweep_reclaims_superseded_unreferenced_bases_but_keeps_referenced_and_current() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(format!("{}/sandboxes", data_dir)).unwrap();

        // The currently installed OS version is 2.0.0.
        std::fs::write(format!("{}/VERSION", data_dir), "2.0.0\n").unwrap();

        // Three base images on disk: the current one, an old one a sandbox is pinned to,
        // and an old one nothing references.
        for v in ["2.0.0", "1.0.0", "0.9.0"] {
            std::fs::write(crate::assets::versioned_rootfs_path(data_dir, v), b"base").unwrap();
        }
        // A sandbox pinned to the 1.0.0 base keeps it alive.
        write_index(
            &format!("{}/sandboxes/web.idx", data_dir),
            &[],
            Some(&crate::assets::versioned_rootfs_path(data_dir, "1.0.0")),
            None,
        );

        let stats = sweep(data_dir).unwrap();

        assert!(
            base_exists(data_dir, "2.0.0"),
            "the current installed base is always retained, even if unreferenced"
        );
        assert!(
            base_exists(data_dir, "1.0.0"),
            "a base a sandbox is pinned to is retained"
        );
        assert!(
            !base_exists(data_dir, "0.9.0"),
            "a superseded base nothing references is reclaimed"
        );
        assert_eq!(stats.bases_removed, 1);
    }

    #[test]
    fn prune_instances_removes_dead_pid_dirs_and_keeps_live_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let inst = format!("{}/instances", data_dir);
        std::fs::create_dir_all(&inst).unwrap();

        // A dead-PID instance dir (999999 exceeds the macOS/Linux PID range, so it can
        // never be live), a live one (our own PID), and a non-numeric stray.
        std::fs::create_dir_all(format!("{}/999999", inst)).unwrap();
        std::fs::create_dir_all(format!("{}/{}", inst, std::process::id())).unwrap();
        std::fs::create_dir_all(format!("{}/not-a-pid", inst)).unwrap();

        let removed = prune_instances(data_dir).unwrap();

        assert_eq!(removed, 1, "only the dead-PID instance is reclaimed");
        assert!(
            !std::path::Path::new(&format!("{}/999999", inst)).exists(),
            "the crashed instance dir is removed"
        );
        assert!(
            std::path::Path::new(&format!("{}/{}", inst, std::process::id())).exists(),
            "a live instance dir is kept"
        );
        assert!(
            std::path::Path::new(&format!("{}/not-a-pid", inst)).exists(),
            "a non-PID entry is left untouched"
        );
    }

    #[test]
    fn format_bytes_scales_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(64 * 1024), "64 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5 MB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn prune_instances_is_zero_when_there_is_no_instances_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(prune_instances(tmp.path().to_str().unwrap()).unwrap(), 0);
    }

    #[test]
    fn prune_instances_is_zero_on_an_empty_instances_dir() {
        // An empty (but present) instances/ dir is a normal post-clean-shutdown state:
        // every VM exited cleanly and removed its own instance dir. Must return 0, not
        // error, so `dome prune` can run safely immediately after a clean shutdown.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(format!("{}/instances", data_dir)).unwrap();
        assert_eq!(prune_instances(data_dir).unwrap(), 0);
    }

    #[test]
    fn sweep_is_a_noop_on_a_fresh_data_dir() {
        // `dome prune` may run before any sandbox or chunk exists; a missing chunks/
        // directory (and no indexes) must reclaim nothing rather than error.
        let tmp = tempfile::tempdir().unwrap();
        let stats = sweep(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(stats, SweepStats::default());
    }

    #[test]
    fn collect_referenced_cycle_guard_prevents_infinite_loop() {
        // A pathological (corrupt) index where A.parent = B and B.parent = A must not
        // hang `dome prune`. The visited-set cycle guard must detect the cycle and
        // terminate, collecting all reachable chunks from both indexes.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(format!("{}/checkpoints", data_dir)).unwrap();

        let a = format!("{}/checkpoints/a.idx", data_dir);
        let b = format!("{}/checkpoints/b.idx", data_dir);

        // Write A and B first with a placeholder parent path (required for the index
        // format), then overwrite with the real cross-references. We create B first
        // so A's parent path (which we embed in A) can reference the final path.
        write_index(&b, &[(0, "chunk-b")], None, Some(&a));
        write_index(&a, &[(1, "chunk-a")], None, Some(&b));

        // Should terminate without hanging and collect both chunks.
        let (chunks, _bases) = collect_referenced(data_dir).unwrap();
        assert!(chunks.contains("chunk-a"), "chunk from A must be collected");
        assert!(chunks.contains("chunk-b"), "chunk from B must be collected");
    }

    #[test]
    fn sweep_keeps_chunks_a_cached_provision_layer_references() {
        // GC exemption (#69): a provisioned layer that no sandbox currently seeds from must
        // keep its chunks. A seeded sandbox flattens the layer (no parent pointer back to it),
        // so without marking provision/ the layer's chunks would look orphaned and be swept —
        // stranding a still-cached `<hash>.idx` over dead chunks. Marking provision/ keeps the
        // chunk alive even with no sandbox referencing it; the `.idx` itself is never swept.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(format!("{}/provision", data_dir)).unwrap();

        // A provision layer references "provchunk"; no sandbox or checkpoint references it.
        write_chunk(data_dir, "provchunk", 64 * 1024);
        let layer = format!("{}/provision/abc123.idx", data_dir);
        write_index(&layer, &[(0, "provchunk")], None, None);

        let stats = sweep(data_dir).unwrap();

        assert!(
            chunk_exists(data_dir, "provchunk"),
            "a cached provision layer's chunk must survive the sweep"
        );
        assert!(
            std::path::Path::new(&layer).exists(),
            "the layer index itself is never swept (only chunks/bases are)"
        );
        assert_eq!(stats.chunks_removed, 0);
    }

    #[test]
    fn sweep_reclaims_chunks_of_a_failed_provision_disk() {
        // The flip side of the exemption: a `.failed` debug disk uses a different extension
        // and is intentionally NOT marked, so its chunks remain reclaimable — `dome prune`
        // removes the `.failed` file, then the sweep reclaims its now-orphaned chunks.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(format!("{}/provision", data_dir)).unwrap();

        write_chunk(data_dir, "failedchunk", 64 * 1024);
        // A `.failed` index (extension is not "idx", so collect_referenced skips it).
        write_index(
            &format!("{}/provision/abc123.failed", data_dir),
            &[(0, "failedchunk")],
            None,
            None,
        );

        let stats = sweep(data_dir).unwrap();

        assert!(
            !chunk_exists(data_dir, "failedchunk"),
            "a failed debug disk's chunks are not exempt and must be reclaimed"
        );
        assert_eq!(stats.chunks_removed, 1);
    }

    #[test]
    fn sweep_bases_with_no_installed_version_sweeps_unreferenced() {
        // When no VERSION file exists (e.g. before `dome init`), `installed_version`
        // returns None and no base is "protected" by the current-version guard.
        // Unreferenced bases must still be swept (the None path must not silently
        // retain all bases).
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        // No VERSION file written — installed_version returns None.

        // One unreferenced base image; no sandboxes or checkpoints reference it.
        let unreferenced = crate::assets::versioned_rootfs_path(data_dir, "1.0.0");
        std::fs::write(&unreferenced, b"base").unwrap();

        let stats = sweep(data_dir).unwrap();

        assert_eq!(stats.bases_removed, 1, "unreferenced base should be swept");
        assert!(
            !std::path::Path::new(&unreferenced).exists(),
            "the unreferenced base image should have been deleted"
        );
    }
}
