//! Unified session core. `run`, `checkpoint create`, and `sandbox` all boot a VM,
//! run a command, and then differ only in what they do with the resulting disk
//! state on teardown. That variation is captured by [`SaveTarget`]; the booting
//! and running is shared via [`run_session`].

use anyhow::Result;

use crate::vm::{self, PreparedVm, RunResult};

/// Describes what to do with the VM's disk state once the command exits.
pub(crate) enum SaveTarget {
    /// Ephemeral: discard the instance, save nothing (the `dome run` default).
    None,
    /// Atomically save the disk state as a checkpoint at `checkpoints/<name>.idx`.
    Checkpoint { name: String },
    /// Flatten + atomically save the disk state as a persistent sandbox at
    /// `sandboxes/<name>.idx`.
    Sandbox { name: String },
}

/// Boot the prepared VM, run the command, then persist (or not) per `save`.
///
/// The save runs on any clean teardown regardless of the command's exit code, so a
/// failed build still leaves the disk state behind. On a host crash this function
/// never returns, no save is attempted, and the last good index survives.
pub(crate) fn run_session(
    prepared: &PreparedVm,
    command: &[String],
    save: &SaveTarget,
) -> Result<i32> {
    let result = vm::run_command(prepared, command)?;
    persist(prepared, &result, save)?;
    // Drop the NBD handle (flushes + shuts down the server) before removing the
    // instance dir that holds its socket.
    drop(result.nbd_handle);
    let _ = std::fs::remove_dir_all(&prepared.instance_dir);
    Ok(result.exit_code)
}

fn persist(prepared: &PreparedVm, result: &RunResult, save: &SaveTarget) -> Result<()> {
    match save {
        SaveTarget::None => Ok(()),
        SaveTarget::Checkpoint { name } => save_checkpoint(prepared, result, name),
        SaveTarget::Sandbox { name } => save_sandbox(result, name),
    }
}

fn save_checkpoint(prepared: &PreparedVm, result: &RunResult, name: &str) -> Result<()> {
    let data_dir = dome_vm::default_data_dir();
    let checkpoints_dir = format!("{}/checkpoints", data_dir);
    std::fs::create_dir_all(&checkpoints_dir)?;
    eprintln!("dome: saving checkpoint '{}'...", name);

    if let Some(ref nbd_handle) = result.nbd_handle {
        let index_path = format!("{}/{}.idx", checkpoints_dir, name);
        nbd_handle.save_checkpoint(&index_path)?;
    } else {
        // DOME_STORAGE=direct: no CAS index, fall back to cloning the flat work copy.
        let ext4_path = format!("{}/{}.ext4", checkpoints_dir, name);
        vm::clone_file(&prepared.work_rootfs, &ext4_path)?;
    }
    eprintln!("dome: checkpoint '{}' saved", name);
    Ok(())
}

fn save_sandbox(result: &RunResult, name: &str) -> Result<()> {
    let data_dir = dome_vm::default_data_dir();
    let sandboxes_dir = format!("{}/sandboxes", data_dir);
    std::fs::create_dir_all(&sandboxes_dir)?;
    let index_path = format!("{}/{}.idx", sandboxes_dir, name);

    // Persistence requires CAS — the caller rejects DOME_STORAGE=direct before we
    // get here, so a missing handle is a real error rather than a graceful fallback.
    let nbd_handle = result.nbd_handle.as_ref().ok_or_else(|| {
        anyhow::anyhow!("persistent sandbox requires CAS storage but no CAS backend was started")
    })?;

    eprintln!("dome: saving sandbox '{}'...", name);
    nbd_handle.save_sandbox(&index_path)?;
    eprintln!("dome: sandbox '{}' saved", name);
    Ok(())
}
