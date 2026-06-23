use clap::Parser;
use serde::{Deserialize, Serialize};

// `VmArgs` is parsed by clap on the CLI, but it is also serialized into the worker boot
// spec (see `worker::BootSpec`) so a detached `dome __worker` re-exec can reconstruct the
// exact session config the user asked for. Every field is a plain Option/Vec/bool/scalar,
// so the serde derives are mechanical; `#[serde(default)]` keeps older boot specs
// forward-compatible if a field is added later.
#[derive(clap::Args, Serialize, Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub(crate) struct VmArgs {
    /// Number of CPU cores
    #[arg(long)]
    pub cpus: Option<usize>,

    /// Memory in MB
    #[arg(long)]
    pub memory: Option<u64>,

    /// Disk size in MB (default: 4096)
    #[arg(long)]
    pub disk_size: Option<u64>,

    /// Path to kernel
    #[arg(long, env = "DOME_KERNEL")]
    pub kernel: Option<String>,

    /// Path to rootfs image
    #[arg(long, env = "DOME_ROOTFS")]
    pub rootfs: Option<String>,

    /// Path to initramfs (for loading VirtIO modules)
    #[arg(long, env = "DOME_INITRD")]
    pub initrd: Option<String>,

    /// Allow network access
    #[arg(long = "allow-net", conflicts_with = "no_allow_net")]
    pub allow_net: bool,

    /// Disable network access (overrides dome.json / a previously-enabled sandbox)
    #[arg(long = "no-allow-net")]
    pub no_allow_net: bool,

    /// Allow mounts to write to host filesystem (required for :rw mounts)
    #[arg(long = "allow-host-writes", conflicts_with = "no_allow_host_writes")]
    pub allow_host_writes: bool,

    /// Disable host-filesystem writes (overrides dome.json / a previously-enabled sandbox)
    #[arg(long = "no-allow-host-writes")]
    pub no_allow_host_writes: bool,

    /// Forward a host port to a guest port (HOST:GUEST, e.g. 8080:80)
    #[arg(short = 'p', long = "port", value_name = "HOST:GUEST")]
    pub port: Vec<String>,

    /// Mount a host directory into the VM (HOST:GUEST[:ro|:rw], default ro)
    #[arg(long = "mount", value_name = "HOST:GUEST[:ro|:rw]")]
    pub mount: Vec<String>,

    /// Inject a secret via proxy (NAME=ENV_VAR@host1,host2)
    #[arg(long = "secret", value_name = "NAME=ENV@HOSTS")]
    pub secret: Vec<String>,

    /// Restrict network to specific hosts (repeatable)
    #[arg(long = "allow-host", value_name = "PATTERN")]
    pub allow_host: Vec<String>,

    /// Expose a host port to the guest via host.dome.internal (HOST:GUEST or PORT)
    #[arg(long = "expose-host", value_name = "HOST:GUEST", hide = true)]
    pub expose_host: Vec<String>,

    /// Path to config file (default: ./dome.json)
    #[arg(long)]
    pub config: Option<String>,

    /// Show verbose output (kernel boot, init messages)
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

impl VmArgs {
    /// The network policy the user requested as a tri-state: `Some(true)` from `--allow-net`,
    /// `Some(false)` from `--no-allow-net`, `None` when neither is passed (inherit the lower
    /// layer). clap rejects passing both, so at most one of the pair is ever set.
    pub(crate) fn allow_net_flag(&self) -> Option<bool> {
        tri_state(self.allow_net, self.no_allow_net)
    }

    /// The host-writes policy the user requested as a tri-state, mirroring [`allow_net_flag`].
    pub(crate) fn allow_host_writes_flag(&self) -> Option<bool> {
        tri_state(self.allow_host_writes, self.no_allow_host_writes)
    }
}

/// Collapse a paired on/off flag into a tri-state. `(on, off)` are mutually exclusive at the
/// CLI layer (clap `conflicts_with`), so `on` winning the tie is just defensive.
fn tri_state(on: bool, off: bool) -> Option<bool> {
    match (on, off) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

#[derive(Parser)]
#[command(name = "dome", about = "microVM sandbox for AI agents", version)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(clap::Subcommand)]
pub(crate) enum Commands {
    /// Boot a VM and run a command inside it
    Run {
        #[command(flatten)]
        vm: VmArgs,

        /// Start from a named checkpoint instead of the base image
        #[arg(long)]
        from: Option<String>,

        /// Attach to raw serial console instead of running a command
        #[arg(long)]
        console: bool,

        /// Run in stdio mode (JSON-lines protocol over stdin/stdout)
        #[arg(long, hide = true)]
        stdio: bool,

        /// Command and arguments to run inside the VM
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    /// Download or update OS image assets
    Init {
        /// Force re-download even if assets exist
        #[arg(long)]
        force: bool,
    },

    /// Upgrade dome to the latest release (CLI + OS image)
    Upgrade {
        /// Opt in to latest-only retention: after upgrading, offer to delete sandboxes
        /// pinned to the superseded OS base (after confirmation) so only the latest base
        /// remains. Overrides `latest_only` in dome.json for this run.
        #[arg(long)]
        latest_only: bool,
    },

    /// Manage disk checkpoints
    Checkpoint {
        #[command(subcommand)]
        action: CheckpointCommands,
    },

    /// Manage persistent developer sandboxes
    Sandbox {
        #[command(subcommand)]
        action: SandboxCommands,
    },

    /// Manage the domed control-plane daemon (start, stop, status)
    Daemon {
        #[command(subcommand)]
        action: DaemonCommands,
    },

    /// Remove leftover instance data from crashed VMs
    Prune,

    /// Internal: run as the domed supervisor (re-exec target; not for direct use)
    #[command(name = "__domed", hide = true)]
    Domed,

    /// Internal: run as a per-sandbox worker that owns one persistent VM (re-exec
    /// target; not for direct use). domed launches this; it reads its boot spec from
    /// the daemon dir and serves the sandbox's data-plane socket until stopped.
    #[command(name = "__worker", hide = true)]
    Worker {
        /// Sandbox name this worker serves.
        name: String,
    },
}

#[derive(clap::Subcommand)]
pub(crate) enum DaemonCommands {
    /// Start the daemon (pre-warm the control plane); no-op if already running
    Start,
    /// Stop the daemon; running sandboxes are left untouched
    Stop,
    /// Report whether the daemon is up, with pid, uptime, worker count, and socket path
    Status,
}

#[derive(clap::Subcommand)]
pub(crate) enum SandboxCommands {
    /// Open an interactive shell in a persistent sandbox (lazily created on first use)
    Shell {
        /// Sandbox name (defaults to the `sandbox` field in dome.json, else a cwd slug)
        name: Option<String>,

        #[command(flatten)]
        vm: VmArgs,

        /// Seed a new sandbox from a checkpoint or another sandbox (only when creating it)
        #[arg(long)]
        from: Option<String>,
    },

    /// Run a command in a persistent sandbox (lazily created on first use)
    Run {
        /// Sandbox name (defaults to the `sandbox` field in dome.json, else a cwd slug)
        name: Option<String>,

        #[command(flatten)]
        vm: VmArgs,

        /// Seed a new sandbox from a checkpoint or another sandbox (only when creating it)
        #[arg(long)]
        from: Option<String>,

        /// Command and arguments to run inside the sandbox
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    /// Materialize a sandbox without running it (lazily created, optionally seeded)
    Create {
        /// Sandbox name (defaults to the `sandbox` field in dome.json, else a cwd slug)
        name: Option<String>,

        #[command(flatten)]
        vm: VmArgs,

        /// Seed the new sandbox from a checkpoint or another sandbox
        #[arg(long)]
        from: Option<String>,
    },

    /// View or edit a sandbox's persisted config (applied on the next cold boot)
    Config {
        /// Sandbox name (defaults to the `sandbox` field in dome.json, else a cwd slug)
        name: Option<String>,

        #[command(flatten)]
        vm: VmArgs,
    },

    /// Force a durable flush+save of a running sandbox's disk state to its index
    Save {
        /// Sandbox name (defaults to the `sandbox` field in dome.json, else a cwd slug)
        name: Option<String>,

        /// Path to config file (default: ./dome.json)
        #[arg(long)]
        config: Option<String>,
    },

    /// Stop a running sandbox (flush+save, then shut its VM down)
    Stop {
        /// Sandbox name (defaults to the `sandbox` field in dome.json, else a cwd slug)
        name: Option<String>,

        /// Detach any attached terminals and stop anyway (otherwise stop refuses while
        /// terminals are attached)
        #[arg(long)]
        force: bool,

        /// Path to config file (default: ./dome.json)
        #[arg(long)]
        config: Option<String>,
    },

    /// List persistent sandboxes (size, pinned base version, running/idle status)
    Ls,

    /// Remove a sandbox's index (fast; chunk reclamation is deferred to `dome prune`)
    Rm {
        /// Sandbox name (defaults to the `sandbox` field in dome.json, else a cwd slug)
        name: Option<String>,

        /// Path to config file (default: ./dome.json)
        #[arg(long)]
        config: Option<String>,
    },
}

// Variants flatten the shared `VmArgs` struct, which clap requires by value (a
// `Box<VmArgs>` can't be `#[command(flatten)]`d), so the size spread between `Create`
// and the lightweight variants is inherent. These enums are parsed once at startup, so
// the size is irrelevant.
#[allow(clippy::large_enum_variant)]
#[derive(clap::Subcommand)]
pub(crate) enum CheckpointCommands {
    /// Run a command and save the resulting disk state as a checkpoint
    Create {
        /// Checkpoint name
        name: String,

        #[command(flatten)]
        vm: VmArgs,

        /// Start from an existing checkpoint instead of the base image
        #[arg(long)]
        from: Option<String>,

        /// Command and arguments to run inside the VM
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    /// List all checkpoints
    List,

    /// Delete a checkpoint
    Delete {
        /// Checkpoint name
        name: String,
    },

    /// Push a checkpoint to a remote store
    Push {
        /// Checkpoint name
        name: String,
    },

    /// Pull a checkpoint from a remote store
    Pull {
        /// Checkpoint name
        name: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a `dome run` invocation, returning the flattened `VmArgs`.
    fn parse_run(args: &[&str]) -> Result<VmArgs, clap::Error> {
        let mut full = vec!["dome", "run"];
        full.extend_from_slice(args);
        let cli = Cli::try_parse_from(full)?;
        match cli.command {
            Commands::Run { vm, .. } => Ok(vm),
            _ => unreachable!("parsed a non-run command"),
        }
    }

    #[test]
    fn paired_net_flags_resolve_to_a_tri_state() {
        assert_eq!(parse_run(&[]).unwrap().allow_net_flag(), None);
        assert_eq!(
            parse_run(&["--allow-net"]).unwrap().allow_net_flag(),
            Some(true)
        );
        assert_eq!(
            parse_run(&["--no-allow-net"]).unwrap().allow_net_flag(),
            Some(false)
        );
        assert_eq!(
            parse_run(&["--no-allow-host-writes"])
                .unwrap()
                .allow_host_writes_flag(),
            Some(false)
        );
    }

    #[test]
    fn passing_a_flag_and_its_negation_is_an_error() {
        let err = parse_run(&["--allow-net", "--no-allow-net"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);

        let err = parse_run(&["--allow-host-writes", "--no-allow-host-writes"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
