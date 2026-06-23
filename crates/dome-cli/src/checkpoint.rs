use std::os::unix::fs::MetadataExt;

use anyhow::{bail, Result};

use dome_vm::default_data_dir;

use crate::cli::VmArgs;
use crate::config::load_config;
use crate::sandbox_config::ResolvedConfig;
use crate::session::{run_session, SaveTarget};
use crate::vm;

pub(crate) fn create(
    name: String,
    vm_args: &VmArgs,
    from: Option<&str>,
    command: Vec<String>,
) -> Result<i32> {
    let cfg = load_config(vm_args.config.as_deref())?;

    let command = if !command.is_empty() {
        command
    } else {
        vec!["/bin/sh".to_string()]
    };

    dome_vm::validate_checkpoint_name(&name).map_err(|e| anyhow::anyhow!(e))?;

    let data_dir = dome_vm::default_data_dir();
    let checkpoints_dir = format!("{}/checkpoints", data_dir);
    // Check both formats
    if checkpoint_exists(&checkpoints_dir, &name) {
        bail!("checkpoint '{}' already exists, delete it first", name);
    }

    // A checkpoint run is ephemeral in spirit: resolve `dome.json` + flags once, persist nothing.
    let resolved = ResolvedConfig::resolve(&ResolvedConfig::default(), &cfg, vm_args)?;
    let prepared = vm::prepare_vm(&resolved, vm_args, from, None)?;
    run_session(&prepared, &command, &SaveTarget::Checkpoint { name })
}

pub(crate) fn list() -> Result<()> {
    let data_dir = default_data_dir();
    let checkpoints_dir = format!("{}/checkpoints", data_dir);

    let entries = match std::fs::read_dir(&checkpoints_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("No checkpoints found.");
            return Ok(());
        }
        Err(e) => bail!("Failed to read checkpoints directory: {}", e),
    };

    let mut checkpoints: Vec<(String, u64, std::time::SystemTime, bool)> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        let is_cas = ext == Some("idx");
        if !is_cas && ext != Some("ext4") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let meta = entry.metadata()?;
        let disk_usage = if is_cas {
            // Count non-ZERO chunks × 64KB for actual referenced data size
            dome_store::ChunkIndex::load(path.to_str().unwrap_or("")).map(|idx| {
                let non_zero = (0..idx.num_chunks())
                    .filter(|&i| idx.get_hash(i).map(|h| h != "ZERO").unwrap_or(false))
                    .count();
                (non_zero as u64) * 64 * 1024
            })?
        } else {
            meta.blocks() * 512
        };
        checkpoints.push((name, disk_usage, meta.modified()?, is_cas));
    }

    if checkpoints.is_empty() {
        eprintln!("No checkpoints found.");
        return Ok(());
    }

    checkpoints.sort_by_key(|(_, _, t, _)| *t);

    let header = ["NAME", "SIZE", "CREATED"];
    let rows: Vec<Vec<String>> = checkpoints
        .iter()
        .map(|(name, size, mtime, is_cas)| {
            let size_str = if *is_cas {
                format_cas_size(*size)
            } else if *size >= 1024 * 1024 * 1024 {
                format!("{:.1} GB", *size as f64 / (1024.0 * 1024.0 * 1024.0))
            } else {
                format!("{} MB", size / (1024 * 1024))
            };
            vec![name.clone(), size_str, format_age(*mtime)]
        })
        .collect();

    print!("{}", render_table(&header, &rows));
    Ok(())
}

/// Render a left-aligned table whose columns are sized to their widest cell, so they
/// line up no matter how long any name or size is — fixed-width columns break the
/// instant a cell overflows. Columns are separated by two spaces and the trailing
/// column is not padded. Returns the whole table, one line per row, each newline-
/// terminated. Shared by `checkpoint list` and `sandbox ls`.
pub(crate) fn render_table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }

    let format_line = |cells: &[&str]| -> String {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            // Pad every column but the last so the next column starts at a fixed offset.
            if i + 1 < cells.len() {
                for _ in 0..widths[i].saturating_sub(cell.chars().count()) {
                    line.push(' ');
                }
            }
        }
        line
    };

    let mut out = String::new();
    out.push_str(&format_line(header));
    out.push('\n');
    for row in rows {
        let cells: Vec<&str> = row.iter().map(|s| s.as_str()).collect();
        out.push_str(&format_line(&cells));
        out.push('\n');
    }
    out
}

/// Render a CAS delta size: MB once it reaches a megabyte, otherwise KB, tagged
/// `(cas)` to signal it is deduplicated delta size rather than a full disk image.
/// Shared by `checkpoint list` and `sandbox ls` so both read identically.
pub(crate) fn format_cas_size(size: u64) -> String {
    if size >= 1024 * 1024 {
        format!("{} MB (cas)", size / (1024 * 1024))
    } else {
        format!("{} KB (cas)", size / 1024)
    }
}

/// Render a last-modified time as a coarse human age ("just now", "5m ago", "3h ago",
/// "2d ago"). Shared by `checkpoint list` and `sandbox ls`.
pub(crate) fn format_age(mtime: std::time::SystemTime) -> String {
    let elapsed = mtime
        .elapsed()
        .unwrap_or(std::time::Duration::ZERO)
        .as_secs();
    if elapsed < 60 {
        "just now".to_string()
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86400)
    }
}

pub(crate) fn delete(name: &str) -> Result<()> {
    dome_vm::validate_checkpoint_name(name).map_err(|e| anyhow::anyhow!(e))?;

    let data_dir = default_data_dir();
    let checkpoints_dir = format!("{}/checkpoints", data_dir);

    // Try .idx (CAS) first, then .ext4 (legacy)
    let idx_path = format!("{}/{}.idx", checkpoints_dir, name);
    let ext4_path = format!("{}/{}.ext4", checkpoints_dir, name);

    if std::path::Path::new(&idx_path).exists() {
        std::fs::remove_file(&idx_path)?;
    } else if std::path::Path::new(&ext4_path).exists() {
        std::fs::remove_file(&ext4_path)?;
    } else {
        bail!("Checkpoint '{}' not found", name);
    }

    eprintln!("dome: checkpoint '{}' deleted", name);
    Ok(())
}

/// Check if a checkpoint exists in either format.
fn checkpoint_exists(checkpoints_dir: &str, name: &str) -> bool {
    std::path::Path::new(&format!("{}/{}.idx", checkpoints_dir, name)).exists()
        || std::path::Path::new(&format!("{}/{}.ext4", checkpoints_dir, name)).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_table_aligns_columns_even_when_a_name_overflows() {
        // A name longer than the others must not shove later columns out of
        // alignment: every column starts at the same offset on every line. This is the
        // bug fixed-width columns caused for both `checkpoint list` and `sandbox ls`.
        let s = |x: &str| x.to_string();
        let header = ["NAME", "SIZE", "BASE", "STATUS", "CREATED"];
        let rows = vec![
            vec![
                s("itest-81330-create-base"),
                s("0 KB (cas)"),
                s("0.6.3"),
                s("idle"),
                s("1h ago"),
            ],
            vec![
                s("web"),
                s("55 MB (cas)"),
                s("0.6.3"),
                s("idle"),
                s("just now"),
            ],
        ];

        let out = render_table(&header, &rows);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "header + two rows");

        let off = |line: &str, tok: &str| line.find(tok).unwrap();
        // BASE values line up with each other and under the BASE header.
        assert_eq!(off(lines[1], "0.6.3"), off(lines[2], "0.6.3"));
        assert_eq!(off(lines[0], "BASE"), off(lines[1], "0.6.3"));
        // STATUS likewise.
        assert_eq!(off(lines[1], "idle"), off(lines[2], "idle"));
        assert_eq!(off(lines[0], "STATUS"), off(lines[1], "idle"));
        // The wide name is not truncated.
        assert!(lines[1].starts_with("itest-81330-create-base"));
    }

    #[test]
    fn render_table_sizes_columns_to_the_header_when_cells_are_narrower() {
        // With only short cells, columns are still at least as wide as their headers,
        // so the SIZE/CREATED headers never collide with the data.
        let header = ["NAME", "SIZE", "CREATED"];
        let rows = vec![vec![
            "a".to_string(),
            "1 MB".to_string(),
            "1d ago".to_string(),
        ]];
        let out = render_table(&header, &rows);
        let lines: Vec<&str> = out.lines().collect();
        // CREATED header and its value share an offset; SIZE column is header-width.
        assert_eq!(lines[0].find("CREATED"), lines[1].find("1d ago"));
        assert_eq!(lines[0].find("SIZE"), lines[1].find("1 MB"));
    }
}
