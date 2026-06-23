//! Per-sandbox worker: the process that owns one persistent microVM.
//!
//! A worker is the same signed `dome` binary re-executed as the hidden `__worker <name>`
//! subcommand (the macOS Virtualization entitlement is tied to the signed binary, so it
//! must be reused — never a separate helper). domed launches one worker per running
//! sandbox; each worker owns its VM + CAS NBD server + egress proxy and **stays alive
//! after an interactive session exits**, until it is explicitly stopped (SIGTERM, or a
//! `stop` request). It is never idle-reaped — that is a later slice.
//!
//! ## Data plane (direct CLI ↔ worker)
//!
//! domed is deliberately **not** in the byte path. To open a session the client asks
//! domed to [`Attach`](dome_proto::control::Command::Attach); domed ensures the worker
//! exists (cold-booting it if needed), mints a one-time token from it, and returns the
//! worker's socket path + token. The client then connects **directly** to the worker
//! socket, presents the token, and the worker splices the raw interactive byte stream
//! straight through to the guest over vsock. The frame protocol (STDIN/STDOUT/RESIZE/…)
//! is identical to the in-process path — the worker is a transparent pipe, so terminal
//! resize and Ctrl-C/signals flow through unchanged.
//!
//! The worker socket is user-private (0600), and the token is single-use, so a leaked
//! socket path alone cannot open a session.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use dome_proto::control::AttachResult;

use crate::cli::VmArgs;
use crate::config::load_config;
use crate::lock::{self, Lock};
use crate::sandbox;
use crate::vm;

/// Accept-loop poll cadence (the listener is non-blocking so it can notice shutdown).
const ACCEPT_POLL: Duration = Duration::from_millis(50);
/// How long a minted token stays valid before it is pruned unused.
const TOKEN_TTL: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// The workers directory: `{data_dir}/daemon/workers`. One socket/log/boot-spec per
/// sandbox lives here.
pub(crate) fn workers_dir(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join("daemon").join("workers")
}

/// A worker's data-plane socket: `{workers}/{name}.sock`.
pub(crate) fn worker_socket_path(data_dir: &str, name: &str) -> PathBuf {
    workers_dir(data_dir).join(format!("{name}.sock"))
}

/// A worker's persistence lock (held for the VM's whole lifetime): the existing
/// per-sandbox lock under `sandboxes/`, so the worker is the sandbox's single writer.
fn sandbox_lock_path(data_dir: &str, name: &str) -> PathBuf {
    Path::new(data_dir)
        .join("sandboxes")
        .join(format!("{name}.lock"))
}

/// The boot spec domed drops for a worker to read on cold boot: `{workers}/{name}.boot.json`.
fn boot_spec_path(data_dir: &str, name: &str) -> PathBuf {
    workers_dir(data_dir).join(format!("{name}.boot.json"))
}

/// Where a worker records a boot failure so domed (and the user) see it instead of a
/// silent exit: `{workers}/{name}.err`.
fn err_path(data_dir: &str, name: &str) -> PathBuf {
    workers_dir(data_dir).join(format!("{name}.err"))
}

/// A worker's on-disk log: `{workers}/{name}.log`.
fn log_path(data_dir: &str, name: &str) -> PathBuf {
    workers_dir(data_dir).join(format!("{name}.log"))
}

// ---------------------------------------------------------------------------
// Boot spec
// ---------------------------------------------------------------------------

/// Everything a worker needs to cold-boot a sandbox, captured by the CLI at attach time
/// and consumed by the worker only when it actually cold-boots (ignored when joining an
/// already-running VM). It carries the resolved name, the optional `--from` seed, the
/// originating cwd (so relative `dome.json` / mount paths resolve as the user intended),
/// and the verbatim VM flags.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BootSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub cwd: String,
    pub vm_args: VmArgs,
}

impl BootSpec {
    /// Build a boot spec from a `dome sandbox shell/run` invocation.
    pub(crate) fn new(
        name: &str,
        from: Option<&str>,
        cwd: &Path,
        vm_args: &VmArgs,
    ) -> Result<Self> {
        Ok(Self {
            name: name.to_string(),
            from: from.map(|s| s.to_string()),
            cwd: cwd.to_string_lossy().to_string(),
            // VmArgs is cheap, plain data; serialize via its serde derives by cloning
            // through JSON so we don't have to thread a borrow through the wire types.
            vm_args: serde_json::from_value(serde_json::to_value(vm_args)?)?,
        })
    }

    /// Serialize for transport inside the [`Attach`](dome_proto::control::Command::Attach)
    /// command.
    pub(crate) fn to_value(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self)?)
    }
}

/// domed-side: persist a boot spec where the worker will read it, before launching the
/// worker. Returns the workers dir (created, 0700) so the caller can spawn into it.
pub(crate) fn write_boot_spec(data_dir: &str, name: &str, boot: &serde_json::Value) -> Result<()> {
    let dir = workers_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating workers dir {}", dir.display()))?;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    // Clear any stale error from a previous failed/crashed boot. Otherwise the launcher's
    // poll loop could read it before the fresh worker's socket comes up and abort a
    // perfectly healthy boot (the error is keyed only by name, not by boot attempt).
    let _ = std::fs::remove_file(err_path(data_dir, name));
    let path = boot_spec_path(data_dir, name);
    std::fs::write(&path, serde_json::to_vec(boot)?)
        .with_context(|| format!("writing boot spec {}", path.display()))?;
    Ok(())
}

/// domed-side: read the error a failed worker recorded, if any (cleared after reading).
pub(crate) fn take_worker_error(data_dir: &str, name: &str) -> Option<String> {
    let path = err_path(data_dir, name);
    let msg = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let msg = msg.trim();
    if msg.is_empty() {
        None
    } else {
        Some(msg.to_string())
    }
}

// ---------------------------------------------------------------------------
// Worker wire protocol (CLI/domed ↔ worker, internal to the dome binary)
// ---------------------------------------------------------------------------

/// A request on the worker socket. The first newline-JSON line selects the operation;
/// an `attach` then switches the connection into a raw byte splice to the guest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum WorkerRequest {
    /// domed asks the worker for a fresh one-time token to hand to a client.
    Mint,
    /// A client opens a session, authorized by a previously minted token.
    Attach {
        token: String,
        tty: bool,
        argv: Vec<String>,
        rows: u16,
        cols: u16,
    },
    /// domed asks the worker how many terminals are currently attached. The worker is
    /// the source of truth for this count: it owns the byte path, so it (not domed) sees
    /// every session start and end. Used to drive the `ATTACHED` column in `ls`.
    Count,
    /// Stop the worker: save the sandbox and shut the VM down.
    Stop,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct MintResponse {
    token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CountResponse {
    attached: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AttachAck {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// One-time token store
// ---------------------------------------------------------------------------

/// Single-use tokens authorizing a client to open a session on this worker. domed mints
/// one per attach and the worker consumes it on first use, so a leaked socket path alone
/// is not enough to attach. Tokens expire after [`TOKEN_TTL`] so an unredeemed mint does
/// not accumulate.
struct TokenStore {
    pending: HashMap<String, Instant>,
    ttl: Duration,
}

impl TokenStore {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
            ttl: TOKEN_TTL,
        }
    }

    /// Mint, store, and return a fresh random token.
    fn mint(&mut self) -> String {
        self.prune();
        let token = random_token();
        self.pending.insert(token.clone(), Instant::now());
        token
    }

    /// Consume `token`: returns true exactly once for a valid, unexpired token, then
    /// never again (one-time use). Unknown or expired tokens return false.
    fn take(&mut self, token: &str) -> bool {
        self.prune();
        self.pending.remove(token).is_some()
    }

    /// Drop expired tokens so an unredeemed mint cannot linger past its TTL.
    fn prune(&mut self) {
        let ttl = self.ttl;
        self.pending.retain(|_, minted| minted.elapsed() < ttl);
    }
}

/// A random hex token sourced from the OS CSPRNG (`/dev/urandom`).
fn random_token() -> String {
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    let mut s = String::with_capacity(32);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// Worker supervisor state
// ---------------------------------------------------------------------------

/// The running worker: the booted VM (shared with session threads) plus its token store,
/// attached-session counter, and shutdown flag.
struct Worker {
    name: String,
    data_dir: String,
    socket_path: PathBuf,
    /// Shared with every session thread; opening a session calls `open_shell`/`open_exec`.
    sandbox: Arc<dome_vm::Sandbox>,
    /// Proxy secret placeholders injected into every guest session.
    env: HashMap<String, String>,
    tokens: Mutex<TokenStore>,
    attached: AtomicUsize,
    shutdown: AtomicBool,
}

impl Worker {
    fn log(&self, msg: &str) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path(&self.data_dir, &self.name))
        {
            let _ = writeln!(f, "{} {}", now_unix(), msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Worker entry point (`dome __worker <name>`)
// ---------------------------------------------------------------------------

/// Entry point for the hidden `dome __worker <name>` subcommand. Cold-boots the
/// sandbox's VM, serves its data-plane socket until stopped, then saves and shuts down.
/// A boot failure is recorded to `{name}.err` (so domed surfaces it to the user) and
/// returned.
pub(crate) fn run_worker(name: &str, data_dir: &str) -> Result<()> {
    match boot_and_serve(name, data_dir) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Record where domed polls for it, so an auto-spawned cold boot that fails
            // reports a real error instead of a "worker never came up" timeout.
            let dir = workers_dir(data_dir);
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(err_path(data_dir, name), format!("{e:#}"));
            Err(e)
        }
    }
}

fn boot_and_serve(name: &str, data_dir: &str) -> Result<()> {
    dome_vm::validate_checkpoint_name(name).map_err(|e| anyhow::anyhow!(e))?;
    let dir = workers_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating workers dir {}", dir.display()))?;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));

    // Read (and clear) the boot spec domed left for us.
    let boot = read_boot_spec(data_dir, name)?;

    // Resolve relative `dome.json` / mount host paths against the originating cwd.
    if let Err(e) = std::env::set_current_dir(&boot.cwd) {
        // Not fatal: absolute paths still work; log and continue.
        let _ = e;
    }
    let cfg = load_config(boot.vm_args.config.as_deref())?;

    // Become the sandbox's sole persistence owner for the VM's whole lifetime. A Fork
    // outcome means another live owner already holds it (a stale worker, or a legacy
    // session): refuse rather than run a second writer.
    let lock_path = sandbox_lock_path(data_dir, name);
    let _lock_guard = match lock::acquire(&lock_path)? {
        Lock::Owner(g) => g,
        Lock::Fork => bail!(
            "sandbox '{}' is already owned by another live session; cannot boot a worker",
            name
        ),
    };

    // Resolve (creating/seeding/pinning) the CAS source, then prepare and boot the VM.
    let source = sandbox::prepare_sandbox_source(name, data_dir, &boot.vm_args, boot.from.as_deref())?;
    let prepared = vm::prepare_vm(&boot.vm_args, &cfg, None, Some(&source))?;
    let instance_dir = prepared.instance_dir.clone();

    let booted = vm::boot_vm(&prepared)?;
    // Share only the Sandbox (Send+Sync) with session threads. The NBD handle (not Sync)
    // and the proxy/forward handles stay in this main thread: `nbd_handle` for the save
    // on shutdown, and the rest held in `booted` (partially moved, never dropped early)
    // so the VM's support services stay up for its whole lifetime.
    let nbd_handle = booted.nbd_handle;
    let env = booted.env;
    let sandbox = Arc::new(booted.sandbox);
    let worker = Arc::new(Worker {
        name: name.to_string(),
        data_dir: data_dir.to_string(),
        socket_path: worker_socket_path(data_dir, name),
        sandbox: Arc::clone(&sandbox),
        env,
        tokens: Mutex::new(TokenStore::new()),
        attached: AtomicUsize::new(0),
        shutdown: AtomicBool::new(false),
    });

    // Bind the data-plane socket (reclaim any stale one — the lock proved no live worker
    // owns it) and lock it down to the current user.
    let _ = std::fs::remove_file(&worker.socket_path);
    let listener = UnixListener::bind(&worker.socket_path)
        .with_context(|| format!("binding worker socket {}", worker.socket_path.display()))?;
    let _ = std::fs::set_permissions(&worker.socket_path, std::fs::Permissions::from_mode(0o600));
    listener
        .set_nonblocking(true)
        .context("setting worker socket non-blocking")?;

    install_signal_handlers();
    worker.log(&format!("worker up for '{name}' (pid {})", std::process::id()));

    serve(&worker, listener);

    // --- Shutdown: save, then tear the VM down. ---
    worker.log("worker stopping: saving sandbox");
    if let Some(ref nbd) = nbd_handle {
        let index_path = format!("{}/sandboxes/{}.idx", data_dir, name);
        if let Err(e) = nbd.save_sandbox(&index_path) {
            worker.log(&format!("save failed: {e:#}"));
        }
    }
    let _ = sandbox.stop();
    // Dropping the NBD handle flushes + shuts down the server before we remove the
    // instance dir that holds its socket. The proxy/forward handles still held by
    // `booted` drop when this function returns.
    drop(nbd_handle);
    let _ = std::fs::remove_file(&worker.socket_path);
    let _ = std::fs::remove_dir_all(&instance_dir);
    worker.log("worker stopped");
    Ok(())
}

/// Read and remove the boot spec domed wrote for this worker.
fn read_boot_spec(data_dir: &str, name: &str) -> Result<BootSpec> {
    let path = boot_spec_path(data_dir, name);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading boot spec {} (was domed supposed to write it?)", path.display()))?;
    let boot: BootSpec = serde_json::from_slice(&bytes).context("parsing boot spec")?;
    let _ = std::fs::remove_file(&path);
    Ok(boot)
}

/// Accept loop: serve worker requests until shutdown is requested (a `stop` request or a
/// caught signal). Mirrors domed's non-blocking accept pattern.
fn serve(worker: &Arc<Worker>, listener: UnixListener) {
    loop {
        if worker.shutdown.load(Ordering::SeqCst) || signal_shutdown_requested() {
            break;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                let _ = stream.set_nonblocking(false);
                let worker = Arc::clone(worker);
                std::thread::spawn(move || handle_conn(&worker, stream));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(e) => {
                // A single aborted/interrupted connection must not take the worker down.
                worker.log(&format!("accept error (continuing): {e}"));
                std::thread::sleep(ACCEPT_POLL);
            }
        }
    }
}

/// Handle one worker-socket connection: read the request line, then dispatch.
fn handle_conn(worker: &Arc<Worker>, mut stream: UnixStream) {
    let line = match read_request_line(&mut stream) {
        Ok(l) if !l.trim().is_empty() => l,
        _ => return,
    };
    let req: WorkerRequest = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            let _ = write_line(&mut stream, &AttachAck { ok: false, error: Some(format!("invalid request: {e}")) });
            return;
        }
    };

    match req {
        WorkerRequest::Mint => {
            let token = worker.tokens.lock().unwrap().mint();
            let _ = write_line(&mut stream, &MintResponse { token });
        }
        WorkerRequest::Count => {
            let attached = worker.attached.load(Ordering::SeqCst);
            let _ = write_line(&mut stream, &CountResponse { attached });
        }
        WorkerRequest::Stop => {
            worker.shutdown.store(true, Ordering::SeqCst);
            let _ = write_line(&mut stream, &AttachAck { ok: true, error: None });
        }
        WorkerRequest::Attach {
            token,
            tty,
            argv,
            rows,
            cols,
        } => handle_attach(worker, stream, &token, tty, &argv, rows, cols),
    }
}

/// Authorize and open one session, then splice the client byte stream straight to the
/// guest. The worker is a transparent pipe from here on — it never reinterprets frames.
fn handle_attach(
    worker: &Arc<Worker>,
    mut stream: UnixStream,
    token: &str,
    tty: bool,
    argv: &[String],
    rows: u16,
    cols: u16,
) {
    if !worker.tokens.lock().unwrap().take(token) {
        let _ = write_line(&mut stream, &AttachAck { ok: false, error: Some("invalid or expired token".to_string()) });
        return;
    }

    // Open the guest session (mounts + ExecRequest sent on the vsock side here, so the
    // client's relay only deals with terminal frames).
    let guest = if tty {
        worker.sandbox.open_shell(argv, &worker.env, rows, cols)
    } else {
        worker.sandbox.open_exec(argv, &worker.env, None)
    };
    let guest = match guest {
        Ok(g) => g,
        Err(e) => {
            worker.log(&format!("attach: opening session failed: {e:#}"));
            let _ = write_line(&mut stream, &AttachAck { ok: false, error: Some(format!("{e:#}")) });
            return;
        }
    };

    // Acknowledge before the splice so the client knows the session is live before it
    // enters raw mode. The ack read is unbuffered on the client side, so no guest output
    // is lost between the ack and the relay starting.
    if write_line(&mut stream, &AttachAck { ok: true, error: None }).is_err() {
        return;
    }

    worker.attached.fetch_add(1, Ordering::SeqCst);
    worker.log("session attached");
    splice(stream, guest);
    worker.attached.fetch_sub(1, Ordering::SeqCst);
    worker.log("session detached (VM stays running)");
}

/// Bidirectional raw byte copy between the client (unix socket) and the guest (vsock).
/// Each half shuts the other's write side on EOF so a closed session tears down cleanly.
fn splice(cli: UnixStream, guest: TcpStream) {
    let mut cli_read = match cli.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut guest_write = match guest.try_clone() {
        Ok(g) => g,
        Err(_) => return,
    };
    let mut guest_read = guest;
    let mut cli_write = cli;

    let t1 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut cli_read, &mut guest_write);
        let _ = guest_write.shutdown(Shutdown::Write);
    });
    let t2 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut guest_read, &mut cli_write);
        let _ = cli_write.shutdown(Shutdown::Write);
    });
    let _ = t1.join();
    let _ = t2.join();
}

// ---------------------------------------------------------------------------
// Client side: domed mint + CLI attach/relay
// ---------------------------------------------------------------------------

/// domed-side: ask a running worker for a fresh one-time token over its socket.
pub(crate) fn mint_token(socket_path: &Path) -> Result<String> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connecting to worker socket {}", socket_path.display()))?;
    write_line(&mut stream, &WorkerRequest::Mint).context("requesting token from worker")?;
    let resp: MintResponse = read_line_json(&mut stream).context("reading worker token")?;
    Ok(resp.token)
}

/// domed-side: ask a running worker how many terminals are currently attached. domed is
/// not in the byte path, so it cannot count attaches itself — the worker, which owns the
/// data plane, is the source of truth. Drives the `ATTACHED` column in `ls`.
pub(crate) fn attached_count(socket_path: &Path) -> Result<usize> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connecting to worker socket {}", socket_path.display()))?;
    write_line(&mut stream, &WorkerRequest::Count).context("requesting attached count from worker")?;
    let resp: CountResponse = read_line_json(&mut stream).context("reading worker attached count")?;
    Ok(resp.attached)
}

/// CLI-side: connect directly to the worker, authorize with the one-time token, and run
/// the interactive (or piped) relay to completion. Returns the guest exit code.
pub(crate) fn attach_and_relay(
    attach: &AttachResult,
    command: &[String],
    tty: bool,
) -> Result<i32> {
    let stdin_fd = std::io::stdin().as_raw_fd();
    let (rows, cols) = if tty {
        dome_vm::client::terminal_size(stdin_fd)
    } else {
        (24, 80)
    };

    let mut stream = UnixStream::connect(&attach.worker_socket)
        .with_context(|| format!("connecting to worker socket {}", attach.worker_socket))?;
    write_line(
        &mut stream,
        &WorkerRequest::Attach {
            token: attach.token.clone(),
            tty,
            argv: command.to_vec(),
            rows,
            cols,
        },
    )
    .context("sending attach request to worker")?;

    let ack: AttachAck = read_line_json(&mut stream).context("reading worker attach ack")?;
    if !ack.ok {
        bail!(
            "worker refused the session: {}",
            ack.error.unwrap_or_else(|| "unknown error".to_string())
        );
    }

    if tty {
        let writer = stream.try_clone().context("cloning worker socket")?;
        Ok(dome_vm::client::run_pty_client(writer, stream, stdin_fd))
    } else {
        Ok(dome_vm::client::run_piped_client(
            stream,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Small wire helpers + signals
// ---------------------------------------------------------------------------

/// Write `value` as a single JSON line and flush.
fn write_line(w: &mut impl Write, value: &impl Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).map_err(std::io::Error::other)?;
    line.push('\n');
    w.write_all(line.as_bytes())?;
    w.flush()
}

/// Read a single newline-terminated line one byte at a time (no buffering), so any bytes
/// that follow the line on the same stream stay unread for the subsequent raw splice.
fn read_request_line(stream: &mut UnixStream) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte)?;
        if n == 0 || byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request line too long",
            ));
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Read and parse one unbuffered JSON line.
fn read_line_json<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let line = read_request_line(stream).context("reading line")?;
    if line.trim().is_empty() {
        bail!("peer closed the connection without responding");
    }
    serde_json::from_str(line.trim()).with_context(|| format!("parsing line: {}", line.trim()))
}

/// Seconds since the unix epoch (best-effort).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Set by SIGTERM/SIGINT so the (detached) worker shuts down gracefully — saving the
/// sandbox — instead of being killed mid-write.
static SIGNAL_SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term_signal(_sig: libc::c_int) {
    SIGNAL_SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTERM, on_term_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term_signal as *const () as libc::sighandler_t);
    }
}

fn signal_shutdown_requested() -> bool {
    SIGNAL_SHUTDOWN.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Tests (hypervisor-free: token store, wire protocol, boot spec, paths)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_single_use() {
        let mut store = TokenStore::new();
        let token = store.mint();
        assert!(!token.is_empty(), "a minted token is non-empty");
        assert!(store.take(&token), "a freshly minted token is accepted once");
        assert!(
            !store.take(&token),
            "the same token must not be accepted twice (one-time use)"
        );
    }

    #[test]
    fn unknown_tokens_are_rejected() {
        let mut store = TokenStore::new();
        let _ = store.mint();
        assert!(
            !store.take("not-a-real-token"),
            "a token that was never minted must be rejected"
        );
    }

    #[test]
    fn minted_tokens_are_distinct() {
        let mut store = TokenStore::new();
        let a = store.mint();
        let b = store.mint();
        assert_ne!(a, b, "each mint yields a distinct token");
        assert!(store.take(&a) && store.take(&b), "both remain independently valid");
    }

    #[test]
    fn expired_tokens_are_rejected() {
        let mut store = TokenStore::new();
        store.ttl = Duration::from_millis(1);
        let token = store.mint();
        std::thread::sleep(Duration::from_millis(5));
        assert!(
            !store.take(&token),
            "a token past its TTL must be rejected, not accepted"
        );
    }

    #[test]
    fn attach_request_roundtrips_with_a_flat_op_tag() {
        let req = WorkerRequest::Attach {
            token: "tok".to_string(),
            tty: true,
            argv: vec!["/bin/sh".to_string()],
            rows: 40,
            cols: 120,
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"op\":\"attach\""), "op tag must be flat: {line}");
        let back: WorkerRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn every_worker_request_variant_roundtrips() {
        for req in [
            WorkerRequest::Mint,
            WorkerRequest::Count,
            WorkerRequest::Stop,
            WorkerRequest::Attach {
                token: "t".to_string(),
                tty: false,
                argv: vec!["ls".to_string(), "-la".to_string()],
                rows: 24,
                cols: 80,
            },
        ] {
            let back: WorkerRequest =
                serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
            assert_eq!(back, req);
        }
    }

    #[test]
    fn boot_spec_roundtrips_through_a_value() {
        let vm = VmArgs {
            memory: Some(4096),
            allow_net: true,
            mount: vec!["./src:/work".to_string()],
            ..Default::default()
        };
        let spec = BootSpec::new("web", Some("base"), Path::new("/home/dev/proj"), &vm).unwrap();
        let value = spec.to_value().unwrap();
        let back: BootSpec = serde_json::from_value(value).unwrap();
        assert_eq!(back.name, "web");
        assert_eq!(back.from.as_deref(), Some("base"));
        assert_eq!(back.cwd, "/home/dev/proj");
        assert_eq!(back.vm_args.memory, Some(4096));
        assert!(back.vm_args.allow_net);
        assert_eq!(back.vm_args.mount, vec!["./src:/work".to_string()]);
    }

    #[test]
    fn worker_socket_path_is_under_the_daemon_workers_dir() {
        let p = worker_socket_path("/data", "web");
        assert!(p.ends_with("daemon/workers/web.sock"), "got {}", p.display());
    }

    #[test]
    fn boot_spec_write_then_take_error_roundtrip() {
        // domed writes a boot spec; an absent error reads as None; a written one is
        // surfaced exactly once and then cleared.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        let value = serde_json::json!({ "name": "web", "cwd": "/x", "vm_args": {} });
        write_boot_spec(data_dir, "web", &value).unwrap();
        assert!(boot_spec_path(data_dir, "web").exists());

        assert_eq!(take_worker_error(data_dir, "web"), None, "no error yet");
        std::fs::write(err_path(data_dir, "web"), "boot blew up").unwrap();
        assert_eq!(
            take_worker_error(data_dir, "web").as_deref(),
            Some("boot blew up")
        );
        assert_eq!(take_worker_error(data_dir, "web"), None, "error is cleared after reading");

        // A stale error from a previous failed boot must not survive into the next boot:
        // writing a fresh boot spec clears it, so the launcher's poll loop can't read a
        // leftover error and abort a healthy boot.
        std::fs::write(err_path(data_dir, "web"), "old failure").unwrap();
        write_boot_spec(data_dir, "web", &value).unwrap();
        assert_eq!(
            take_worker_error(data_dir, "web"),
            None,
            "writing a fresh boot spec clears a stale error"
        );
    }

    #[test]
    fn a_count_request_and_response_roundtrip() {
        let line = serde_json::to_string(&WorkerRequest::Count).unwrap();
        assert!(line.contains("\"op\":\"count\""), "op tag must be flat: {line}");
        let back: WorkerRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(back, WorkerRequest::Count);

        let resp = CountResponse { attached: 3 };
        let back: CountResponse =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(back.attached, 3);
    }

    #[test]
    fn an_attach_ack_carries_an_error_only_on_failure() {
        let ok = serde_json::to_string(&AttachAck { ok: true, error: None }).unwrap();
        assert!(!ok.contains("error"), "ok ack omits error: {ok}");
        let bad = serde_json::to_string(&AttachAck {
            ok: false,
            error: Some("nope".to_string()),
        })
        .unwrap();
        assert!(bad.contains("nope"));
    }
}
