mod assets;
mod checkpoint;
mod cli;
mod config;
mod daemon;
mod gc;
mod lock;
mod provision;
mod retention;
mod sandbox;
mod sandbox_config;
mod session;
mod stdio;
mod vm;
mod worker;

use std::process;

use anyhow::Result;
use clap::Parser;

use dome_vm::{default_data_dir, VmState};

use cli::{CheckpointCommands, Cli, Commands, DaemonCommands, ProvisionCommands, SandboxCommands};
use config::load_config;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            vm,
            from,
            rebuild,
            console,
            stdio,
            command,
        } => {
            let cfg = load_config(vm.config.as_deref())?;

            // Command resolution: CLI args > config > default /bin/sh
            let command = if !command.is_empty() {
                command
            } else if let Some(cfg_cmd) = cfg.command.clone() {
                cfg_cmd
            } else {
                vec!["/bin/sh".to_string()]
            };

            // Ephemeral runs resolve `dome.json` + flags per invocation and persist nothing.
            let resolved = sandbox_config::ResolvedConfig::resolve(
                &sandbox_config::ResolvedConfig::default(),
                &cfg,
                &vm,
            )?;

            // Declarative provisioning: when the project declares a `provision` block and no
            // explicit `--from` was given, resolve (building once if uncached) the cached
            // toolchain layer and seed this ephemeral run from it. `--from` composition with a
            // provisioned layer is a later slice, so an explicit seed wins here.
            let provision_seed = match (from.as_deref(), &resolved.provision) {
                (None, Some(spec)) => provision::ensure_layer(
                    &default_data_dir(),
                    assets::CURRENT_VERSION,
                    spec,
                    resolved.disk_size.unwrap_or(4096),
                    &vm,
                    &provision::VmStepRunner,
                    rebuild,
                )?,
                _ => None,
            };
            let prepared = vm::prepare_vm(
                &resolved,
                &vm,
                from.as_deref(),
                provision_seed.as_deref(),
                None,
            )?;

            let result = if stdio {
                let r = stdio::run_stdio(&prepared);
                let _ = std::fs::remove_dir_all(&prepared.instance_dir);
                r
            } else if console {
                let r = run_console(&prepared);
                let _ = std::fs::remove_dir_all(&prepared.instance_dir);
                r
            } else {
                // Ephemeral run: run_session handles instance cleanup and saves nothing.
                session::run_session(&prepared, &command, &session::SaveTarget::None)
            };

            process::exit(result?);
        }
        Commands::Init { force } => {
            let data_dir = default_data_dir();
            if force {
                let _ = std::fs::remove_file(format!("{}/VERSION", data_dir));
            }
            if assets::assets_ready(&data_dir) {
                eprintln!(
                    "dome: OS image already up to date ({})",
                    assets::CURRENT_VERSION
                );
            } else {
                assets::download_os_image(&data_dir)?;
            }
        }
        Commands::Upgrade { latest_only } => {
            let data_dir = default_data_dir();
            let cfg = load_config(None)?;
            let policy_enabled = retention::policy_enabled(latest_only, cfg.latest_only);

            // Upgrade first; it reports the version it moved to (None if already latest).
            if let Some(new_version) = assets::upgrade(&data_dir)? {
                // A provisioned layer's hash folds in the base OS identity, so every layer
                // built against the superseded version can never cache-hit again — reclaim
                // them unconditionally (they are pure cache, unlike a sandbox's work). Their
                // orphaned chunks are swept by the next `dome prune`.
                let stale = provision::reclaim_stale_layers(&data_dir, &new_version)?;
                if stale > 0 {
                    eprintln!(
                        "dome: reclaimed {} provisioned layer(s) stale against {}",
                        stale, new_version
                    );
                }

                // Apply the opt-in latest-only retention against the version just
                // installed. Pin-forever + GC is the default, so this is a no-op unless
                // explicitly enabled.
                if policy_enabled {
                    let outcome = retention::apply_latest_only(
                        &data_dir,
                        &new_version,
                        retention::interactive_confirm,
                    )?;
                    retention::report_outcome(&outcome);
                }
            }
        }
        Commands::Prune { provision } => {
            let data_dir = default_data_dir();

            // 1. Reclaim instance directories left by crashed ephemeral VMs.
            let instances = gc::prune_instances(&data_dir)?;
            if instances == 0 {
                eprintln!("dome: no orphaned instances found");
            } else {
                eprintln!("dome: removed {} orphaned instance(s)", instances);
            }

            // 2. Reclaim preserved half-provisioned ("debug") disks left by failed provision
            // builds. Their now-unreferenced chunks are swept in step 4.
            let failed = provision::prune_failed_layers(&data_dir)?;
            if failed > 0 {
                eprintln!(
                    "dome: removed {} preserved provisioning debug disk(s)",
                    failed
                );
            }

            // 3. Reclaim cached provisioned layers. By default this is stale-only: layers
            // whose pinned OS base no longer matches the installed version (they can never
            // cache-hit again). `--provision` force-clears the whole cache. The removed
            // layers' now-orphaned chunks are reclaimed by the sweep in step 4.
            let layers = if provision {
                provision::prune_all_layers(&data_dir)?
            } else {
                match assets::installed_version(&data_dir) {
                    Some(v) => provision::reclaim_stale_layers(&data_dir, &v)?,
                    None => 0,
                }
            };
            if layers > 0 {
                eprintln!(
                    "dome: reclaimed {} provisioned layer(s){}",
                    layers,
                    if provision {
                        " (full cache cleared)"
                    } else {
                        " stale against the installed OS"
                    }
                );
            }

            // 4. Mark-and-sweep the CAS store: reclaim chunks and superseded base images
            // no live sandbox, checkpoint, or cached provision layer references (deferred
            // from `sandbox rm`).
            let stats = gc::sweep(&data_dir)?;
            if stats.chunks_removed == 0 && stats.bases_removed == 0 {
                eprintln!("dome: no unreferenced chunks or base images to reclaim");
            } else {
                eprintln!(
                    "dome: reclaimed {} chunk(s) ({}) and {} base image(s)",
                    stats.chunks_removed,
                    gc::format_bytes(stats.bytes_removed),
                    stats.bases_removed
                );
            }
        }
        Commands::Provision { action } => match action {
            ProvisionCommands::List => provision::list()?,
            ProvisionCommands::Debug { hash, vm } => {
                let exit_code = provision::debug_shell(hash.as_deref(), &vm)?;
                process::exit(exit_code);
            }
        },
        Commands::Checkpoint { action } => match action {
            CheckpointCommands::Create {
                name,
                vm,
                from,
                command,
            } => {
                let exit_code = checkpoint::create(name, &vm, from.as_deref(), command)?;
                process::exit(exit_code);
            }
            CheckpointCommands::List => checkpoint::list()?,
            CheckpointCommands::Delete { name } => checkpoint::delete(&name)?,
            CheckpointCommands::Push { name: _ } => {
                anyhow::bail!("checkpoint push is not yet implemented")
            }
            CheckpointCommands::Pull { name: _ } => {
                anyhow::bail!("checkpoint pull is not yet implemented")
            }
        },
        Commands::Sandbox { action } => match action {
            SandboxCommands::Shell {
                name,
                vm,
                from,
                rebuild,
            } => {
                let exit_code =
                    sandbox::run_sandbox(name, &vm, Vec::new(), from.as_deref(), rebuild)?;
                process::exit(exit_code);
            }
            SandboxCommands::Run {
                name,
                vm,
                from,
                rebuild,
                command,
            } => {
                let exit_code = sandbox::run_sandbox(name, &vm, command, from.as_deref(), rebuild)?;
                process::exit(exit_code);
            }
            SandboxCommands::Create {
                name,
                vm,
                from,
                rebuild,
            } => {
                sandbox::create_sandbox(name, &vm, from.as_deref(), rebuild)?;
            }
            SandboxCommands::Config { name, reload, vm } => {
                sandbox::config_sandbox(name, &vm, reload)?;
            }
            SandboxCommands::Save { name, config } => {
                sandbox::save_sandbox(name, config.as_deref())?;
            }
            SandboxCommands::Stop {
                name,
                force,
                config,
            } => {
                sandbox::stop_sandbox(name, force, config.as_deref())?;
            }
            SandboxCommands::Ls => sandbox::list_sandboxes()?,
            SandboxCommands::Rm { name, config } => {
                sandbox::remove_sandbox(name, config.as_deref())?;
            }
        },
        Commands::Daemon { action } => {
            let data_dir = default_data_dir();
            match action {
                DaemonCommands::Start => daemon::start(&data_dir)?,
                DaemonCommands::Stop => daemon::stop(&data_dir)?,
                DaemonCommands::Status => daemon::status(&data_dir)?,
            }
        }
        Commands::Domed => {
            daemon::run_supervisor(&default_data_dir())?;
        }
        Commands::Worker { name } => {
            worker::run_worker(&name, &default_data_dir())?;
        }
    }

    Ok(())
}

/// Run the VM in raw serial console mode (for debugging).
fn run_console(prepared: &vm::PreparedVm) -> Result<i32> {
    eprintln!("dome: kernel={}", prepared.kernel_path);
    eprintln!("dome: rootfs={} (work copy)", prepared.work_rootfs);
    eprintln!(
        "dome: booting VM ({}cpus, {}MB RAM, {}MB disk)...",
        prepared.cpus, prepared.memory, prepared.disk_size
    );

    let sandbox = vm::build_sandbox(prepared, true, None, None)?;
    eprintln!("dome: VM created and validated successfully");

    let state_rx = sandbox.state_channel();

    eprintln!("dome: starting VM...");
    sandbox.start()?;
    eprintln!("dome: VM started");

    eprintln!("dome: running in console mode (Ctrl+C to stop)");
    let mut exit_code = 0;
    loop {
        match state_rx.recv() {
            Ok(VmState::Stopped) => {
                eprintln!("dome: VM stopped");
                break;
            }
            Ok(VmState::Error) => {
                eprintln!("dome: VM encountered an error");
                exit_code = 1;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    Ok(exit_code)
}
