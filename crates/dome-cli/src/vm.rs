use std::collections::HashMap;
use std::ffi::CString;
use std::io::IsTerminal;

use anyhow::{bail, Context, Result};

#[cfg(target_os = "macos")]
extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
}

#[cfg(target_os = "macos")]
pub(crate) fn clone_file(src: &str, dst: &str) -> Result<()> {
    let c_src = CString::new(src).context("invalid source path")?;
    let c_dst = CString::new(dst).context("invalid destination path")?;
    let ret = unsafe { clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        bail!("clonefile({} -> {}) failed: {}", src, dst, err);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn clone_file(src: &str, dst: &str) -> Result<()> {
    std::fs::copy(src, dst).with_context(|| format!("failed to copy {} -> {}", src, dst))?;
    Ok(())
}

use dome_vm::{MountConfig, PortMapping, Sandbox};

use crate::assets;
use crate::cli::VmArgs;
use crate::sandbox_config::{ProvisionSpec, ProxyResolved, ResolvedConfig};

/// A persistent sandbox's storage binding: where its CAS index lives and the
/// immutable, version-pinned base image its never-written chunks resolve through.
pub(crate) struct SandboxSource {
    /// Path to the sandbox's CAS index (`sandboxes/<name>.idx`). May not exist yet
    /// on first boot — it is created lazily from the base.
    pub index_path: String,
    /// Path to the immutable versioned base image (`bases/rootfs-<version>.ext4`).
    pub base_path: String,
}

pub(crate) struct PreparedVm {
    pub instance_dir: String,
    pub source_rootfs: String,
    pub work_rootfs: String,
    /// If restoring from a CAS checkpoint, the index path.
    pub cas_index: Option<String>,
    pub kernel_path: String,
    pub initrd_path: Option<String>,
    pub cpus: usize,
    pub memory: u64,
    pub disk_size: u64,
    pub proxy_config: Option<dome_proxy::config::ProxyConfig>,
    pub verbose: bool,
    pub forwards: Vec<PortMapping>,
    pub mounts: Vec<MountConfig>,
}

/// Resolve the disk size for booting an *existing* sandbox. Disk size is pinned at
/// creation: the index encodes a fixed chunk count, so booting at a different size
/// would corrupt the filesystem. The stored size therefore always wins. When the
/// caller passed `--disk-size` (`flag_mb`) and it differs from the pinned size, the
/// flag is ignored and the requested value is returned alongside so the caller can
/// emit a one-line notice. A matching or absent flag yields no notice. `stored_mb`
/// and the resolved size are both in MB.
pub(crate) fn pin_sandbox_disk_size(flag_mb: Option<u64>, stored_mb: u64) -> (u64, Option<u64>) {
    let ignored = match flag_mb {
        Some(requested) if requested != stored_mb => Some(requested),
        _ => None,
    };
    (stored_mb, ignored)
}

/// Prepare a VM from a single resolved config plus the session/environment fields the
/// resolved config deliberately does not carry. `cfg` holds the already-merged VM shape
/// (the `defaults <- dome.json <- flags` merge happened in [`ResolvedConfig::resolve`], so
/// this function no longer takes the raw `dome.json`); `env` supplies only the host/session
/// fields — kernel/rootfs/initrd paths and verbosity — read fresh each invocation.
pub(crate) fn prepare_vm(
    cfg: &ResolvedConfig,
    env: &VmArgs,
    from: Option<&str>,
    provision_seed: Option<&str>,
    sandbox: Option<&SandboxSource>,
) -> Result<PreparedVm> {
    let cpus = cfg.cpus.unwrap_or(2);
    let memory = cfg.memory.unwrap_or(2048);
    let mut disk_size = cfg.disk_size.unwrap_or(4096);
    let allow_net = cfg.allow_net;
    let allow_host_writes = cfg.allow_host_writes;
    let verbose = env.verbose;

    // Secrets, the unified allow-list, and expose-host mappings are all resolved already;
    // the proxy only exists when networking is enabled.
    let proxy_config = if allow_net {
        Some(cfg.proxy.to_proxy_config()?)
    } else {
        None
    };

    // Port forwards (already merged across layers in `resolve`).
    let mut forwards = Vec::new();
    for s in &cfg.ports {
        let mapping =
            parse_port_mapping(s).with_context(|| format!("invalid port mapping: '{}'", s))?;
        forwards.push(mapping);
    }

    // Mounts (already merged across layers in `resolve`).
    let mut mounts = Vec::new();
    for s in &cfg.mounts {
        let mc = parse_mount_spec(s).with_context(|| format!("invalid mount spec: '{}'", s))?;
        mounts.push(mc);
    }

    if !mounts.is_empty() {
        validate_mounts(&mounts, allow_host_writes)?;
    }

    let data_dir = dome_vm::default_data_dir();

    // Auto-download assets when using default paths
    if env.kernel.is_none()
        && env.rootfs.is_none()
        && env.initrd.is_none()
        && !assets::assets_ready(&data_dir)
    {
        assets::download_os_image(&data_dir)?;
    }

    let kernel_path = env
        .kernel
        .clone()
        .unwrap_or_else(|| format!("{}/Image", data_dir));
    // Ephemeral runs and checkpoints resolve the base via the immutable, versioned
    // rootfs path of the installed OS version — the same file sandboxes pin to.
    let rootfs_path = env.rootfs.clone().unwrap_or_else(|| {
        let version = assets::installed_version(&data_dir)
            .unwrap_or_else(|| assets::CURRENT_VERSION.to_string());
        assets::versioned_rootfs_path(&data_dir, &version)
    });
    let initrd_path_str = env
        .initrd
        .clone()
        .unwrap_or_else(|| format!("{}/initramfs.cpio.gz", data_dir));

    if !std::path::Path::new(&kernel_path).exists() {
        bail!(
            "Kernel not found at {}. Run `dome init` to download.",
            kernel_path
        );
    }

    // Determine source for working copy: persistent sandbox, checkpoint, or base rootfs.
    let checkpoints_dir = format!("{}/checkpoints", data_dir);
    let mut cas_index: Option<String> = None;
    let source = if let Some(sb) = sandbox {
        // A persistent sandbox always rides the CAS index it owns, falling back to
        // its pinned base image. The index may not exist yet on first boot.
        cas_index = Some(sb.index_path.clone());
        if std::path::Path::new(&sb.index_path).exists() {
            // Pin disk size to whatever the sandbox was created with; the index
            // encodes a fixed chunk count, so honoring a differing --disk-size here
            // would corrupt the filesystem.
            let stored = dome_store::ChunkIndex::load(&sb.index_path)?.disk_size() / (1024 * 1024);
            let (resolved, ignored) = pin_sandbox_disk_size(cfg.disk_size, stored);
            if let Some(requested) = ignored {
                eprintln!(
                    "dome: ignoring --disk-size {}MB; sandbox is pinned to {}MB",
                    requested, resolved
                );
            }
            disk_size = resolved;
        }
        sb.base_path.clone()
    } else {
        match from {
            Some(name) => {
                dome_vm::validate_checkpoint_name(name).map_err(|e| anyhow::anyhow!(e))?;
                // Check .idx (CAS) first, then .ext4 (legacy)
                let idx_path = format!("{}/{}.idx", checkpoints_dir, name);
                let ext4_path = format!("{}/{}.ext4", checkpoints_dir, name);
                if std::path::Path::new(&idx_path).exists() {
                    cas_index = Some(idx_path.clone());
                    idx_path
                } else if std::path::Path::new(&ext4_path).exists() {
                    ext4_path
                } else {
                    bail!("Checkpoint '{}' not found", name);
                }
            }
            None => {
                if let Some(seed) = provision_seed {
                    // A provisioned layer is a CAS index (an absolute path under
                    // `provision/`), not a name resolved under `checkpoints/`. Ride it
                    // directly like a checkpoint seed: reads resolve through it and its
                    // pinned base; an ephemeral run never writes back to the shared layer.
                    if !std::path::Path::new(seed).exists() {
                        bail!("provisioned layer not found: {}", seed);
                    }
                    cas_index = Some(seed.to_string());
                    seed.to_string()
                } else {
                    if !std::path::Path::new(&rootfs_path).exists() {
                        bail!(
                            "Rootfs not found at {}. Run `dome init` to download.",
                            rootfs_path
                        );
                    }
                    rootfs_path
                }
            }
        }
    };

    // Create per-instance working copy (clean any stale dir from PID reuse)
    let instance_dir = format!("{}/instances/{}", data_dir, std::process::id());
    let _ = std::fs::remove_dir_all(&instance_dir);
    std::fs::create_dir_all(&instance_dir)?;
    let work_rootfs = format!("{}/rootfs.ext4", instance_dir);

    // CAS checkpoints don't need a file copy — the NBD server reads from the chunk store.
    // We still need a rootfs file for the VM builder (kernel cmdline root=), but it can be
    // a dummy for CAS mode since I/O goes through NBD.
    if cas_index.is_none() {
        if verbose {
            eprintln!("dome: creating working copy...");
        }
        clone_file(&source, &work_rootfs)?;
    } else {
        // Create a minimal placeholder so the VM builder doesn't fail
        std::fs::File::create(&work_rootfs)?;
    }

    // Extend to requested disk size
    let f = std::fs::OpenOptions::new().write(true).open(&work_rootfs)?;
    let target = disk_size * 1024 * 1024;
    let current = f.metadata()?.len();
    if target < current {
        bail!(
            "--disk-size {}MB is smaller than the base image ({}MB)",
            disk_size,
            current / 1024 / 1024
        );
    }
    if target > current {
        f.set_len(target)?;
    }
    drop(f);

    let initrd_path = if std::path::Path::new(&initrd_path_str).exists() {
        Some(initrd_path_str)
    } else {
        eprintln!(
            "dome: warning: initramfs not found at {}, booting without it",
            initrd_path_str
        );
        None
    };

    Ok(PreparedVm {
        instance_dir,
        source_rootfs: source,
        work_rootfs,
        cas_index,
        kernel_path,
        initrd_path,
        cpus,
        memory,
        disk_size,
        proxy_config,
        verbose,
        forwards,
        mounts,
    })
}

pub(crate) fn build_sandbox(
    prepared: &PreparedVm,
    console: bool,
    network_fd: Option<i32>,
    nbd_uri: Option<&str>,
) -> Result<Sandbox> {
    let mut builder = Sandbox::builder()
        .kernel(&prepared.kernel_path)
        .rootfs(&prepared.work_rootfs)
        .cpus(prepared.cpus)
        .memory_mb(prepared.memory)
        .console(console)
        .verbose(prepared.verbose);

    if let Some(fd) = network_fd {
        builder = builder.network_fd(fd);
    }

    if let Some(uri) = nbd_uri {
        builder = builder.nbd_uri(uri);
    }

    if let Some(initrd) = &prepared.initrd_path {
        builder = builder.initrd(initrd);
    }

    for m in &prepared.mounts {
        builder = builder.mount(m.clone());
    }

    builder.build()
}

/// Start the CAS NBD server for a prepared VM, respecting DOME_STORAGE=direct fallback.
pub(crate) fn start_nbd(prepared: &PreparedVm) -> Result<Option<dome_store::NbdHandle>> {
    if std::env::var("DOME_STORAGE").unwrap_or_default() == "direct" {
        return Ok(None);
    }
    let socket_path = format!("{}/nbd.sock", prepared.instance_dir);
    let data_dir = dome_vm::default_data_dir();
    let cas_dir = format!("{}/cas", data_dir);
    let index_path = if let Some(ref idx) = prepared.cas_index {
        idx.clone()
    } else {
        let source_hash = blake3::hash(prepared.source_rootfs.as_bytes()).to_hex();
        format!("{}/cas/indexes/{}.idx", data_dir, &source_hash[..16])
    };
    let target_size = prepared.disk_size * 1024 * 1024;
    Ok(Some(dome_store::start_cas_nbd_server(
        &prepared.source_rootfs,
        &cas_dir,
        &index_path,
        &socket_path,
        target_size,
    )?))
}

pub(crate) struct RunResult {
    pub exit_code: i32,
    pub nbd_handle: Option<dome_store::NbdHandle>,
}

/// A booted, ready-to-serve VM with all its host-side support handles. Holding this
/// alive keeps the VM, its CAS NBD server, the egress proxy, and any port forwards
/// running; dropping it (in field order) tears them down. The one-shot `run_command`
/// path holds it only for the duration of a single command, while the persistent worker
/// holds it for the whole sandbox lifetime, opening many sessions against `sandbox`.
pub(crate) struct BootedVm {
    pub sandbox: Sandbox,
    /// Environment to inject into every guest session (proxy secret placeholders).
    pub env: HashMap<String, String>,
    /// CAS NBD server handle — also the save handle (`save_sandbox`). `None` only under
    /// `DOME_STORAGE=direct`.
    pub nbd_handle: Option<dome_store::NbdHandle>,
    /// Egress proxy handle; kept alive so MITM/secret injection keeps working.
    proxy_handle: Option<dome_proxy::ProxyHandle>,
    /// Port-forward listeners; kept alive so `-p` forwards keep serving.
    fwd_handle: Option<dome_vm::PortForwardHandle>,
}

/// Boot the prepared VM and bring up all support services (proxy, CAS NBD, port
/// forwards), injecting the proxy CA + secret placeholders when MITM is configured.
/// Returns once the guest is reachable; the caller decides how long to keep it alive and
/// what to run against it.
pub(crate) fn boot_vm(prepared: &PreparedVm) -> Result<BootedVm> {
    if prepared.verbose {
        eprintln!("dome: kernel={}", prepared.kernel_path);
        eprintln!("dome: rootfs={} (work copy)", prepared.work_rootfs);
    }
    eprintln!(
        "dome: booting VM ({}cpus, {}MB RAM, {}MB disk)...",
        prepared.cpus, prepared.memory, prepared.disk_size
    );

    // Set up proxy networking if --allow-net
    let (vm_fd, proxy_handle) = if let Some(ref proxy_config) = prepared.proxy_config {
        let (vm_fd, host_fd) = dome_proxy::create_socketpair()?;
        let handle = dome_proxy::start(host_fd, proxy_config.clone())?;

        if prepared.verbose {
            eprintln!("dome: proxy started");
        }

        (Some(vm_fd), Some(handle))
    } else {
        (None, None)
    };

    let nbd_handle = start_nbd(prepared)?;
    let nbd_uri = nbd_handle.as_ref().map(|h| h.uri());

    let sandbox = build_sandbox(prepared, false, vm_fd, nbd_uri.as_deref())?;
    if prepared.verbose {
        eprintln!("dome: VM created and validated successfully");
    }

    sandbox.start()?;
    if prepared.verbose {
        eprintln!("dome: VM started, waiting for guest...");
    }

    let fwd_handle = if !prepared.forwards.is_empty() {
        Some(sandbox.start_port_forwarding(&prepared.forwards)?)
    } else {
        None
    };

    // Inject CA cert and secret placeholders when MITM is needed
    let mut env = HashMap::new();
    if let Some(ref handle) = proxy_handle {
        if !handle.placeholders.is_empty() {
            sandbox.write_file(
                "/usr/local/share/ca-certificates/dome-proxy.crt",
                &handle.ca_cert_pem,
            )?;
            sandbox.exec(
                &["update-ca-certificates", "--fresh"],
                &mut std::io::sink(),
                &mut std::io::sink(),
            )?;
            if prepared.verbose {
                eprintln!("dome: proxy CA certificate injected");
            }
            for (name, placeholder) in &handle.placeholders {
                env.insert(name.clone(), placeholder.clone());
            }
        }
    }

    Ok(BootedVm {
        sandbox,
        env,
        nbd_handle,
        proxy_handle,
        fwd_handle,
    })
}

pub(crate) fn run_command(prepared: &PreparedVm, command: &[String]) -> Result<RunResult> {
    let booted = boot_vm(prepared)?;

    let exit_code = if std::io::stdin().is_terminal() {
        booted.sandbox.shell(command, &booted.env)?
    } else {
        booted.sandbox.exec_with_env(
            command,
            &booted.env,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        )?
    };

    let BootedVm {
        sandbox,
        proxy_handle,
        fwd_handle,
        nbd_handle,
        ..
    } = booted;
    drop(proxy_handle);
    drop(fwd_handle);
    let _ = sandbox.stop();
    Ok(RunResult {
        exit_code,
        nbd_handle,
    })
}

/// Run a project's provisioning steps in a one-shot build VM and save the resulting disk
/// state as the provisioned layer index at `out_index`.
///
/// The build boots from the bare base (project dir NOT mounted), with networking on and
/// narrowed by `spec.allow` (empty = all allowed). Steps run as **root**, **sequentially**,
/// **stop-on-first-failure**, each via its own `sh -c` so `cd`/`export` don't cross steps. A
/// banner and live per-step output make the cold run visible. On success the layer is saved to
/// `out_index`; on any step failure nothing is saved and the error propagates (failing the
/// create) — richer failure UX (debug disk) is a later slice. Lives here, not in
/// [`crate::provision`], so it can drive the private [`BootedVm`] teardown the same way
/// [`run_command`] does.
pub(crate) fn build_provision_layer(
    spec: &ProvisionSpec,
    disk_size_mb: u64,
    env: &VmArgs,
    out_index: &str,
    failed_index: Option<&str>,
) -> Result<()> {
    // Build-VM shape: default cpus/memory, the requested disk size, networking on and
    // narrowed by the provision-time allow-list. No mounts — the project dir is never exposed
    // to the build.
    let cfg = ResolvedConfig {
        disk_size: Some(disk_size_mb),
        allow_net: true,
        proxy: ProxyResolved {
            allow: spec.allow.clone(),
            ..Default::default()
        },
        ..Default::default()
    };

    let prepared = prepare_vm(&cfg, env, None, None, None)?;
    let booted = boot_vm(&prepared)?;

    eprintln!("dome: ── provisioning toolchain (cold build) ──");
    // Capture the running step's combined output (tail-bounded) so a failure can surface
    // exactly what the step printed before it died, while still streaming it live.
    let capture = std::cell::RefCell::new(Vec::<u8>::new());
    let mut failure: Option<anyhow::Error> = None;
    for step in &spec.steps {
        eprintln!("dome:   → {}", step);
        capture.borrow_mut().clear();
        let cmd = vec!["sh".to_string(), "-c".to_string(), step.clone()];
        let exec = {
            let mut out = CaptureTee::new(std::io::stdout(), &capture);
            let mut err = CaptureTee::new(std::io::stderr(), &capture);
            booted
                .sandbox
                .exec_with_env(&cmd, &booted.env, &mut out, &mut err)
        };
        match exec {
            Ok(0) => {}
            Ok(code) => {
                failure = Some(provision_step_error(step, Some(code), &capture.borrow()));
                break;
            }
            Err(e) => {
                // An exec transport error (not a step exit code): keep the captured tail too.
                failure = Some(
                    e.context(provision_step_error(step, None, &capture.borrow()).to_string()),
                );
                break;
            }
        }
    }

    // Tear down the support services and stop the VM (mirroring `run_command`), then save the
    // resulting disk state: on success to the caller's temp path (published atomically), on
    // failure — if a debug path was given — to that path so the developer can shell into the
    // half-provisioned disk without re-running steps. Nothing partial is ever written to
    // `out_index`, so the success hash is never published from a failed build.
    let BootedVm {
        sandbox,
        proxy_handle,
        fwd_handle,
        nbd_handle,
        ..
    } = booted;
    drop(proxy_handle);
    drop(fwd_handle);
    let _ = sandbox.stop();

    let result = match failure {
        Some(e) => {
            if let (Some(failed), Some(h)) = (failed_index, &nbd_handle) {
                match h.save_checkpoint(failed) {
                    Ok(_) => eprintln!("dome: preserved the half-provisioned disk for debugging"),
                    Err(se) => eprintln!(
                        "dome: warning: could not preserve the debug disk ({}): {:#}",
                        failed, se
                    ),
                }
            }
            Err(e)
        }
        None => match &nbd_handle {
            Some(h) => h
                .save_checkpoint(out_index)
                .context("saving provisioned layer"),
            None => Err(anyhow::anyhow!(
                "provisioning requires CAS storage; DOME_STORAGE=direct is not supported"
            )),
        },
    };
    drop(nbd_handle);
    let _ = std::fs::remove_dir_all(&prepared.instance_dir);
    result
}

/// Build the error for a failed provision step, surfacing the command, exit code, and the
/// captured tail of its output so the failure is self-explanatory without scrolling back.
fn provision_step_error(step: &str, code: Option<i32>, output: &[u8]) -> anyhow::Error {
    let code = match code {
        Some(c) => c.to_string(),
        None => "error".to_string(),
    };
    let tail = String::from_utf8_lossy(output);
    let tail = tail.trim_end();
    if tail.is_empty() {
        anyhow::anyhow!("provision step failed (exit {code}): {step}\n  (no output captured)")
    } else {
        anyhow::anyhow!(
            "provision step failed (exit {code}): {step}\n\
             --- captured output (tail) ---\n{tail}\n\
             ------------------------------"
        )
    }
}

/// A `Write` that streams bytes straight through to `inner` while also appending them to a
/// shared, tail-bounded capture buffer. Used to tee a provision step's live output into a
/// buffer the failure path can quote. Single-threaded (`exec_with_env` writes inline), so a
/// `RefCell` is sufficient — no locking.
struct CaptureTee<'a, W: std::io::Write> {
    inner: W,
    capture: &'a std::cell::RefCell<Vec<u8>>,
}

impl<'a, W: std::io::Write> CaptureTee<'a, W> {
    /// Cap the capture so a chatty step cannot balloon memory; the tail is what matters.
    const MAX_CAPTURE: usize = 64 * 1024;

    fn new(inner: W, capture: &'a std::cell::RefCell<Vec<u8>>) -> Self {
        Self { inner, capture }
    }
}

impl<W: std::io::Write> std::io::Write for CaptureTee<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        {
            let mut cap = self.capture.borrow_mut();
            cap.extend_from_slice(buf);
            if cap.len() > Self::MAX_CAPTURE {
                let drop = cap.len() - Self::MAX_CAPTURE;
                cap.drain(0..drop);
            }
        }
        self.inner.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Pure string validation — no filesystem access. Separated from `parse_mount_spec`
/// so unit tests can exercise mode/path logic without touching the filesystem.
fn parse_mount_parts(host: &str, guest: &str, mode: Option<&str>) -> Result<MountConfig> {
    if !guest.starts_with('/') {
        bail!("guest path must be absolute (start with /): '{}'", guest);
    }
    let read_only = match mode {
        None | Some("ro") => true,
        Some("rw") => false,
        Some(other) => bail!("invalid mount mode '{}': expected 'ro' or 'rw'", other),
    };
    Ok(MountConfig {
        host_path: host.to_string(),
        guest_path: guest.to_string(),
        read_only,
    })
}

fn parse_mount_spec(s: &str) -> Result<MountConfig> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    if parts.len() < 2 {
        bail!("expected HOST:GUEST[:ro|rw] format (e.g. ./src:/workspace:rw)");
    }

    let host_path = std::fs::canonicalize(parts[0])
        .with_context(|| format!("host path does not exist: '{}'", parts[0]))?
        .to_string_lossy()
        .to_string();

    let guest = parts[1];
    let mode = parts.get(2).copied();

    parse_mount_parts(&host_path, guest, mode)
}

fn validate_mounts(mounts: &[MountConfig], allow_host_writes: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to determine current working directory")?;
    let cwd =
        std::fs::canonicalize(&cwd).context("failed to canonicalize current working directory")?;
    validate_mounts_with_cwd(mounts, allow_host_writes, &cwd)
}

fn validate_mounts_with_cwd(
    mounts: &[MountConfig],
    allow_host_writes: bool,
    cwd: &std::path::Path,
) -> Result<()> {
    if cwd == std::path::Path::new("/") {
        bail!(
            "cannot use mounts when the current working directory is '/'. \
             Change to a project directory first."
        );
    }

    for mc in mounts {
        let host = std::path::Path::new(&mc.host_path);

        if host == std::path::Path::new("/") {
            bail!("mounting '/' as a host path is not allowed. Mount a specific subdirectory instead.");
        }

        if !host.starts_with(cwd) {
            bail!(
                "mount host path '{}' is outside the current working directory '{}'. \
                 Only paths within CWD can be mounted.",
                mc.host_path,
                cwd.display()
            );
        }

        if !mc.read_only && !allow_host_writes {
            bail!(
                "read-write mount '{}:{}:rw' requires --allow-host-writes flag \
                 (or \"allow_host_writes\": true in config).",
                mc.host_path,
                mc.guest_path
            );
        }
    }

    Ok(())
}

/// Parse `NAME=ENV_VAR@host1,host2` into (name, from, hosts). Used by config resolution
/// to turn `--secret` flags into the structured secret mapping stored in the sidecar.
pub(crate) fn parse_secret_flag(s: &str) -> Result<(String, String, Vec<String>)> {
    let (name, rest) = s
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("missing '=' separator"))?;
    let (from, hosts_str) = rest
        .split_once('@')
        .ok_or_else(|| anyhow::anyhow!("missing '@' separator for hosts"))?;
    let hosts: Vec<String> = hosts_str.split(',').map(|h| h.trim().to_string()).collect();
    if name.is_empty() || from.is_empty() || hosts.is_empty() {
        bail!("name, env var, and hosts must all be non-empty");
    }
    Ok((name.to_string(), from.to_string(), hosts))
}

fn parse_port_mapping(s: &str) -> Result<PortMapping> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        bail!("expected HOST:GUEST format (e.g. 8080:80)");
    }
    let host_port: u16 = parts[0]
        .parse()
        .with_context(|| format!("invalid host port: '{}'", parts[0]))?;
    let guest_port: u16 = parts[1]
        .parse()
        .with_context(|| format!("invalid guest port: '{}'", parts[1]))?;
    Ok(PortMapping {
        host_port,
        guest_port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_disk_size_wins_and_a_differing_flag_is_reported() {
        // A sandbox is pinned to its creation size. A boot-time `--disk-size` that
        // differs is ignored (the stored size is used) and reported back so the caller
        // can warn — a differing size would corrupt the fixed-chunk-count filesystem.
        let (resolved, ignored) = pin_sandbox_disk_size(Some(8192), 4096);
        assert_eq!(resolved, 4096, "the pinned size must win over the flag");
        assert_eq!(
            ignored,
            Some(8192),
            "the ignored flag is reported for the notice"
        );
    }

    #[test]
    fn a_matching_disk_size_flag_is_not_reported() {
        // Passing the same size the sandbox is pinned to is a no-op, not a warning.
        let (resolved, ignored) = pin_sandbox_disk_size(Some(4096), 4096);
        assert_eq!(resolved, 4096);
        assert_eq!(ignored, None, "a matching flag must not produce a notice");
    }

    #[test]
    fn an_absent_disk_size_flag_uses_the_pinned_size_silently() {
        // With no `--disk-size`, the pinned size is used and nothing is reported.
        let (resolved, ignored) = pin_sandbox_disk_size(None, 2048);
        assert_eq!(resolved, 2048);
        assert_eq!(ignored, None);
    }

    #[test]
    fn mount_defaults_to_read_only() {
        let mc = parse_mount_parts("/some/host", "/workspace", None).unwrap();
        assert!(mc.read_only);
        assert_eq!(mc.guest_path, "/workspace");
    }

    #[test]
    fn mount_ro_suffix() {
        let mc = parse_mount_parts("/some/host", "/workspace", Some("ro")).unwrap();
        assert!(mc.read_only);
    }

    #[test]
    fn mount_rw_suffix() {
        let mc = parse_mount_parts("/some/host", "/workspace", Some("rw")).unwrap();
        assert!(!mc.read_only);
    }

    #[test]
    fn mount_rejects_bad_mode() {
        assert!(parse_mount_parts("/some/host", "/workspace", Some("xx")).is_err());
    }

    #[test]
    fn mount_rejects_relative_guest() {
        assert!(parse_mount_parts("/some/host", "relative/path", None).is_err());
    }

    #[test]
    fn rw_mount_rejected_without_flag() {
        let cwd = std::env::current_dir().unwrap();
        let mounts = vec![MountConfig {
            host_path: cwd.to_string_lossy().to_string(),
            guest_path: "/workspace".to_string(),
            read_only: false,
        }];
        let err = validate_mounts_with_cwd(&mounts, false, &cwd).unwrap_err();
        assert!(err.to_string().contains("--allow-host-writes"));
    }

    #[test]
    fn rw_mount_accepted_with_flag() {
        let cwd = std::env::current_dir().unwrap();
        let mounts = vec![MountConfig {
            host_path: cwd.to_string_lossy().to_string(),
            guest_path: "/workspace".to_string(),
            read_only: false,
        }];
        assert!(validate_mounts_with_cwd(&mounts, true, &cwd).is_ok());
    }

    #[test]
    fn ro_mount_accepted_without_flag() {
        let cwd = std::env::current_dir().unwrap();
        let mounts = vec![MountConfig {
            host_path: cwd.to_string_lossy().to_string(),
            guest_path: "/workspace".to_string(),
            read_only: true,
        }];
        assert!(validate_mounts_with_cwd(&mounts, false, &cwd).is_ok());
    }

    #[test]
    fn mount_outside_cwd_rejected() {
        let cwd = std::path::Path::new("/Users/testuser/project");
        let mounts = vec![MountConfig {
            host_path: "/tmp".to_string(),
            guest_path: "/workspace".to_string(),
            read_only: true,
        }];
        let err = validate_mounts_with_cwd(&mounts, false, cwd).unwrap_err();
        assert!(err
            .to_string()
            .contains("outside the current working directory"));
    }

    #[test]
    fn root_host_path_rejected() {
        let cwd = std::path::Path::new("/Users/testuser/project");
        let mounts = vec![MountConfig {
            host_path: "/".to_string(),
            guest_path: "/workspace".to_string(),
            read_only: true,
        }];
        let err = validate_mounts_with_cwd(&mounts, false, cwd).unwrap_err();
        assert!(err.to_string().contains("mounting '/'"));
    }

    #[test]
    fn empty_mounts_passes() {
        let cwd = std::env::current_dir().unwrap();
        assert!(validate_mounts_with_cwd(&[], false, &cwd).is_ok());
    }

    #[test]
    fn cwd_root_rejected() {
        let mounts = vec![MountConfig {
            host_path: "/usr".to_string(),
            guest_path: "/workspace".to_string(),
            read_only: true,
        }];
        let err = validate_mounts_with_cwd(&mounts, false, std::path::Path::new("/")).unwrap_err();
        assert!(err.to_string().contains("current working directory is '/'"));
    }
}
