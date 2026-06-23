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
use std::sync::atomic::{AtomicBool, Ordering};
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
/// Auto-flush interval: if a sandbox has unsaved (dirty) writes, the worker flushes+saves
/// at least this often, bounding the crash-loss window for a long-lived session.
const FLUSH_INTERVAL: Duration = Duration::from_secs(60);
/// Auto-flush dirty-byte cap: a save is forced as soon as the in-memory dirty buffer
/// exceeds this, bounding worker memory under a write-heavy burst.
const DIRTY_CAP: u64 = 256 * 1024 * 1024;
/// How often the background flusher wakes to re-check the dirty buffer + interval. Small
/// enough to react promptly to a dirty-cap breach, and to notice shutdown.
const FLUSH_POLL: Duration = Duration::from_secs(1);

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

/// Where domed records that a worker died unexpectedly (a crash): `{workers}/{name}.failed`.
/// Its presence (with no live worker) is what surfaces a sandbox as `failed` in `ls`, and
/// it holds the worker's last log lines so the crash is diagnosable after the fact. Cleared
/// when the sandbox is next cold-booted (a fresh boot supersedes the failure).
pub(crate) fn failed_marker_path(data_dir: &str, name: &str) -> PathBuf {
    workers_dir(data_dir).join(format!("{name}.failed"))
}

/// domed-side: record an unexpected worker exit (a crash), stashing the last log lines so
/// they remain retrievable after the worker process is gone. Best-effort.
pub(crate) fn write_failed_marker(data_dir: &str, name: &str, log_tail: &str) {
    let dir = workers_dir(data_dir);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(failed_marker_path(data_dir, name), log_tail);
}

/// domed-side: whether a sandbox is in the failed (crashed) state — a marker exists from a
/// prior unexpected worker exit and has not yet been superseded by a fresh boot.
pub(crate) fn is_failed(data_dir: &str, name: &str) -> bool {
    failed_marker_path(data_dir, name).exists()
}

/// domed-side: clear a sandbox's failed marker (a fresh cold boot supersedes the crash).
pub(crate) fn clear_failed_marker(data_dir: &str, name: &str) {
    let _ = std::fs::remove_file(failed_marker_path(data_dir, name));
}

/// domed-side: read the last `n` lines of a worker's log (best-effort; empty if no log).
/// Used to surface what a crashed worker last did, both in the `sandbox.crashed` event and
/// in the persisted failed marker.
pub(crate) fn read_last_log_lines(data_dir: &str, name: &str, n: usize) -> String {
    let Ok(contents) = std::fs::read_to_string(log_path(data_dir, name)) else {
        return String::new();
    };
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
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
    /// Force a durable flush+save of the sandbox right now (drives `dome sandbox save`).
    /// The worker flushes its dirty chunks and atomically rewrites the index.
    Save,
    /// Stop the worker: save the sandbox and shut the VM down. The worker is the sole
    /// decider of the attached-terminal guard (it owns the count): unless `force` is set, a
    /// stop is REFUSED while terminals are attached. The check-and-commit is atomic with
    /// `attach` (both go through [`SessionState`]), so once a stop commits no new session
    /// can start — there is no check-then-act window.
    Stop {
        /// Detach attached terminals and stop anyway (default: refuse if any are attached).
        #[serde(default)]
        force: bool,
    },
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
struct SaveResponse {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The worker's verdict on a [`WorkerRequest::Stop`]. `ok` means the worker committed to
/// stopping (or was already stopping); otherwise `attached` carries the live count that
/// caused the refusal, so domed can name it to the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StopResponse {
    ok: bool,
    #[serde(default)]
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
    /// CAS backend behind the VM's NBD server (None in flat-file mode). The source of the
    /// dirty-byte count and the target of every flush+save. Shared with the NBD server and
    /// the background flusher.
    cas: Option<Arc<dome_store::CasBackend>>,
    /// Where a save writes the flattened sandbox index: `{data_dir}/sandboxes/{name}.idx`.
    index_path: String,
    /// Serializes saves so the auto-flush thread, an explicit `save`, and the shutdown
    /// save never run [`CasBackend::save_sandbox_index`] concurrently (its atomic temp
    /// file is keyed only by pid, so two in-flight saves in one worker would collide).
    save_lock: Mutex<()>,
    tokens: Mutex<TokenStore>,
    /// The attached-session count + a `stopping` flag, behind one lock. The lock makes the
    /// stop guard atomic with session start: `attach` checks `stopping` and bumps the count
    /// under it, and `stop` reads the count and commits `stopping` under it, so a stop and a
    /// new attach can never interleave to slip a session past a committed stop.
    sessions: Mutex<SessionState>,
    shutdown: AtomicBool,
}

/// The worker's session lifecycle state (see [`Worker::sessions`]). The attached-terminal
/// guard lives here, under one lock, so the check + commit are atomic with session start —
/// there is no check-then-act window between counting and stopping.
struct SessionState {
    /// Number of terminals currently spliced to the guest.
    attached: usize,
    /// Set once a stop has committed: no new session may start, so the worker drains to a
    /// quiescent state and shuts down.
    stopping: bool,
}

impl SessionState {
    fn new() -> Self {
        Self {
            attached: 0,
            stopping: false,
        }
    }

    /// Try to start a session. Returns false if a stop has committed (the caller refuses
    /// the attach), else bumps the count and returns true.
    fn begin(&mut self) -> bool {
        if self.stopping {
            return false;
        }
        self.attached += 1;
        true
    }

    /// End a session started by [`SessionState::begin`].
    fn end(&mut self) {
        self.attached = self.attached.saturating_sub(1);
    }

    /// Decide a stop. `Ok(())` (committing `stopping`) when idle, forced, or already
    /// stopping; `Err(attached)` (leaving the worker running) when terminals are attached
    /// and `force` is not set.
    fn commit_stop(&mut self, force: bool) -> std::result::Result<(), usize> {
        if self.stopping {
            return Ok(());
        }
        if !force && self.attached > 0 {
            return Err(self.attached);
        }
        self.stopping = true;
        Ok(())
    }
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

    /// Bytes currently buffered in the in-memory dirty map (0 in flat-file mode).
    fn dirty_bytes(&self) -> u64 {
        self.cas.as_ref().map(|c| c.dirty_bytes()).unwrap_or(0)
    }

    /// Number of terminals currently attached (drives the `ATTACHED` column in `ls`).
    fn attached_count(&self) -> usize {
        self.sessions.lock().unwrap().attached
    }

    /// Try to start a session (see [`SessionState::begin`]): false if the worker is
    /// stopping, so a late attach is refused cleanly rather than racing the teardown.
    fn begin_session(&self) -> bool {
        self.sessions.lock().unwrap().begin()
    }

    /// End a session started by [`Worker::begin_session`].
    fn end_session(&self) {
        self.sessions.lock().unwrap().end();
    }

    /// Decide + commit a stop (see [`SessionState::commit_stop`]). `Err(attached)` when
    /// terminals are attached and `force` is not set; `Ok(())` (with `stopping` committed)
    /// otherwise. Atomic with [`Worker::begin_session`] — once this returns `Ok`, no new
    /// session can start.
    fn commit_stop(&self, force: bool) -> std::result::Result<(), usize> {
        self.sessions.lock().unwrap().commit_stop(force)
    }

    /// Force a durable flush+save: write+hash the dirty chunks and atomically rewrite the
    /// sandbox index, so a later cold boot reflects the latest in-memory state. A no-op in
    /// flat-file mode (no index to save). On success, notifies domed so it can broadcast a
    /// `sandbox.saved` event to subscribers (best-effort: a save still succeeds if domed
    /// is down — workers outlive domed).
    fn save(&self) -> Result<()> {
        let Some(ref cas) = self.cas else {
            return Ok(());
        };
        // Hold the save lock across the whole flatten+atomic-write so two saves can't race.
        let _guard = self.save_lock.lock().unwrap();
        cas.save_sandbox_index(&self.index_path)
            .with_context(|| format!("saving sandbox index {}", self.index_path))?;
        self.log("sandbox saved");
        crate::daemon::notify_saved(&self.data_dir, &self.name);
        Ok(())
    }
}

/// The auto-flush trigger: a save is due once there are dirty (unsaved) bytes AND either
/// the flush interval has elapsed since the last save or the dirty buffer has exceeded the
/// cap. With no dirty bytes there is nothing to lose, so the interval alone never forces a
/// pointless save. Pure + side-effect-free so the policy is unit-testable without a VM.
fn flush_is_due(dirty_bytes: u64, since_last_save: Duration, interval: Duration, cap: u64) -> bool {
    dirty_bytes > 0 && (since_last_save >= interval || dirty_bytes >= cap)
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
    // Clone the CAS backend for the worker (and its background flusher) so they can save
    // independently of the main thread, which keeps the non-`Sync` NBD handle.
    let cas = nbd_handle.as_ref().and_then(|h| h.cas_backend());
    let index_path = format!("{}/sandboxes/{}.idx", data_dir, name);
    let worker = Arc::new(Worker {
        name: name.to_string(),
        data_dir: data_dir.to_string(),
        socket_path: worker_socket_path(data_dir, name),
        sandbox: Arc::clone(&sandbox),
        env,
        cas,
        index_path,
        save_lock: Mutex::new(()),
        tokens: Mutex::new(TokenStore::new()),
        sessions: Mutex::new(SessionState::new()),
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

    // Background auto-flush: bound worker memory + the crash-loss window by saving on an
    // interval and when the dirty buffer exceeds the cap. Joined before the shutdown save.
    let flusher = spawn_auto_flusher(Arc::clone(&worker));

    serve(&worker, listener);

    // Stop the flusher before the final save so they can't run concurrently.
    let _ = flusher.join();

    // --- Shutdown: save, then tear the VM down. ---
    worker.log("worker stopping: saving sandbox");
    if let Err(e) = worker.save() {
        worker.log(&format!("save failed: {e:#}"));
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

/// Spawn the background auto-flush thread. It wakes every [`FLUSH_POLL`] and, when a save
/// is [`flush_is_due`], flushes+saves the sandbox — bounding both worker memory (the dirty
/// buffer never grows past the cap before being drained) and the crash-loss window (unsaved
/// writes are at most one interval old). It exits when shutdown is requested, so the
/// caller can join it before the final shutdown save (no two saves ever overlap).
fn spawn_auto_flusher(worker: Arc<Worker>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut last_save = Instant::now();
        loop {
            if worker.shutdown.load(Ordering::SeqCst) || signal_shutdown_requested() {
                break;
            }
            std::thread::sleep(FLUSH_POLL);
            if worker.shutdown.load(Ordering::SeqCst) || signal_shutdown_requested() {
                break;
            }
            if flush_is_due(worker.dirty_bytes(), last_save.elapsed(), FLUSH_INTERVAL, DIRTY_CAP) {
                match worker.save() {
                    Ok(()) => last_save = Instant::now(),
                    Err(e) => worker.log(&format!("auto-flush save failed: {e:#}")),
                }
            }
        }
    })
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
            let attached = worker.attached_count();
            let _ = write_line(&mut stream, &CountResponse { attached });
        }
        WorkerRequest::Save => {
            let resp = match worker.save() {
                Ok(()) => SaveResponse { ok: true, error: None },
                Err(e) => {
                    worker.log(&format!("explicit save failed: {e:#}"));
                    SaveResponse { ok: false, error: Some(format!("{e:#}")) }
                }
            };
            let _ = write_line(&mut stream, &resp);
        }
        WorkerRequest::Stop { force } => {
            // The worker is the sole decider of the attached guard, atomically with attach.
            match worker.commit_stop(force) {
                Ok(()) => {
                    worker.shutdown.store(true, Ordering::SeqCst);
                    let _ = write_line(&mut stream, &StopResponse { ok: true, attached: 0 });
                }
                Err(attached) => {
                    worker.log(&format!(
                        "stop refused: {attached} attached terminal(s) (use --force)"
                    ));
                    let _ = write_line(&mut stream, &StopResponse { ok: false, attached });
                }
            }
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

    // Claim a session slot. This is atomic with a stop's commit, so a session can never
    // start once a stop has committed — closing the check-then-act window the old
    // count-then-stop had. Reserve BEFORE opening the guest session (and before the count is
    // observable), so a concurrent non-forced stop sees this attach.
    if !worker.begin_session() {
        let _ = write_line(&mut stream, &AttachAck { ok: false, error: Some("sandbox is stopping".to_string()) });
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
            worker.end_session();
            let _ = write_line(&mut stream, &AttachAck { ok: false, error: Some(format!("{e:#}")) });
            return;
        }
    };

    // Acknowledge before the splice so the client knows the session is live before it
    // enters raw mode. The ack read is unbuffered on the client side, so no guest output
    // is lost between the ack and the relay starting.
    if write_line(&mut stream, &AttachAck { ok: true, error: None }).is_err() {
        worker.end_session();
        return;
    }

    worker.log("session attached");
    splice(stream, guest);
    worker.end_session();
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

/// domed-side: tell a running worker to flush+save its sandbox now and wait for the save
/// to complete. Backs `dome sandbox save <name>` (domed forwards the request here).
pub(crate) fn save_via_worker(socket_path: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connecting to worker socket {}", socket_path.display()))?;
    write_line(&mut stream, &WorkerRequest::Save).context("requesting save from worker")?;
    let resp: SaveResponse = read_line_json(&mut stream).context("reading worker save response")?;
    if !resp.ok {
        bail!(
            "worker failed to save: {}",
            resp.error.unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(())
}

/// The result of a [`stop_via_worker`] request — the worker's verdict on the attached
/// guard. The worker (not domed) decides: it owns the count and commits the stop atomically
/// with session start.
pub(crate) enum StopOutcome {
    /// The worker committed to stopping and is draining (save + VM teardown).
    Stopped,
    /// The stop was refused because terminals are attached and `force` was not set; carries
    /// the live attached count so domed can name it to the user.
    Refused { attached: usize },
}

/// domed-side: ask a running worker to stop — flush+save its sandbox and tear the VM down.
/// The worker is the sole decider of the attached-terminal guard (it owns the count and
/// commits the stop atomically with session start), so domed forwards `force` and relays
/// the verdict: on [`StopOutcome::Stopped`] the worker drains (breaks its accept loop,
/// joins the flusher, saves, stops the VM — which closes in-flight guest sessions so
/// attached clients detach — and removes its socket), and the caller polls for the socket
/// to disappear to confirm durability; on [`StopOutcome::Refused`] the worker keeps
/// running. Backs `dome sandbox stop [--force]`.
pub(crate) fn stop_via_worker(socket_path: &Path, force: bool) -> Result<StopOutcome> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connecting to worker socket {}", socket_path.display()))?;
    write_line(&mut stream, &WorkerRequest::Stop { force })
        .context("requesting stop from worker")?;
    let resp: StopResponse = read_line_json(&mut stream).context("reading worker stop response")?;
    if resp.ok {
        Ok(StopOutcome::Stopped)
    } else {
        Ok(StopOutcome::Refused {
            attached: resp.attached,
        })
    }
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
            WorkerRequest::Stop { force: false },
            WorkerRequest::Stop { force: true },
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
    fn a_save_request_and_response_roundtrip() {
        let line = serde_json::to_string(&WorkerRequest::Save).unwrap();
        assert!(line.contains("\"op\":\"save\""), "op tag must be flat: {line}");
        let back: WorkerRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(back, WorkerRequest::Save);

        let ok = serde_json::to_string(&SaveResponse { ok: true, error: None }).unwrap();
        assert!(!ok.contains("error"), "ok save omits error: {ok}");
        let bad = SaveResponse { ok: false, error: Some("disk full".to_string()) };
        let back: SaveResponse =
            serde_json::from_str(&serde_json::to_string(&bad).unwrap()).unwrap();
        assert_eq!(back, bad);
    }

    #[test]
    fn flush_is_due_only_with_dirty_bytes() {
        let interval = Duration::from_secs(60);
        let cap = 256 * 1024 * 1024;

        // Nothing dirty → never due, no matter how long it has been.
        assert!(!flush_is_due(0, Duration::from_secs(3600), interval, cap));

        // Dirty + interval elapsed → due.
        assert!(flush_is_due(4096, interval, interval, cap));
        // Dirty but the interval has not elapsed and we are under the cap → not yet.
        assert!(!flush_is_due(4096, Duration::from_secs(1), interval, cap));

        // Dirty over the cap → due immediately, even early in the interval.
        assert!(flush_is_due(cap, Duration::from_secs(0), interval, cap));
        assert!(flush_is_due(cap + 1, Duration::from_secs(1), interval, cap));
        // Just under the cap, early in the interval → not yet (bounded buffer still OK).
        assert!(!flush_is_due(cap - 1, Duration::from_secs(1), interval, cap));
    }

    #[test]
    fn a_stop_request_roundtrips_with_a_flat_op_tag_and_defaulted_force() {
        let line = serde_json::to_string(&WorkerRequest::Stop { force: true }).unwrap();
        assert!(line.contains("\"op\":\"stop\""), "op tag must be flat: {line}");
        let back: WorkerRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(back, WorkerRequest::Stop { force: true });
        // A stop line omitting `force` defaults to false.
        let back: WorkerRequest = serde_json::from_str(r#"{"op":"stop"}"#).unwrap();
        assert_eq!(back, WorkerRequest::Stop { force: false });
    }

    #[test]
    fn a_stop_response_carries_the_count_only_on_refusal() {
        let refused = StopResponse { ok: false, attached: 3 };
        let back: StopResponse =
            serde_json::from_str(&serde_json::to_string(&refused).unwrap()).unwrap();
        assert_eq!(back, refused);
        // An accepted stop round-trips with a zero count.
        let ok: StopResponse =
            serde_json::from_str(&serde_json::to_string(&StopResponse { ok: true, attached: 0 }).unwrap())
                .unwrap();
        assert!(ok.ok);
    }

    #[test]
    fn the_stop_guard_is_atomic_with_session_start() {
        let mut s = SessionState::new();

        // Idle → a stop commits immediately, and then blocks any new session.
        let mut idle = SessionState::new();
        assert_eq!(idle.commit_stop(false), Ok(()));
        assert!(!idle.begin(), "no session may start once a stop has committed");

        // Attached + not forced → refused, naming the count, and the worker stays runnable.
        assert!(s.begin() && s.begin(), "two sessions start");
        assert_eq!(s.commit_stop(false), Err(2), "refusal carries the live count");
        assert!(s.begin(), "a refused stop leaves the worker accepting sessions");

        // Forced → commits regardless of the attached count; commit is idempotent.
        assert_eq!(s.commit_stop(true), Ok(()));
        assert_eq!(s.commit_stop(false), Ok(()), "commit_stop is idempotent");
        assert!(!s.begin(), "no new session after a forced stop commits");

        // end() never underflows.
        let mut z = SessionState::new();
        z.end();
        assert_eq!(z.attached, 0);
    }

    #[test]
    fn failed_marker_and_last_log_lines_roundtrip() {
        // A crash marker records the worker's last log lines, is observable via is_failed,
        // surfaces them via read_last_log_lines, and is cleared by a fresh boot.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        assert!(!is_failed(data_dir, "web"), "no marker initially");

        write_failed_marker(data_dir, "web", "line A\nline B");
        assert!(is_failed(data_dir, "web"), "marker present after a crash");

        clear_failed_marker(data_dir, "web");
        assert!(!is_failed(data_dir, "web"), "marker cleared on a fresh boot");

        // read_last_log_lines returns the tail of the worker log (and empty when absent).
        assert_eq!(read_last_log_lines(data_dir, "web", 5), "");
        let dir = workers_dir(data_dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("web.log"), "l1\nl2\nl3\nl4\n").unwrap();
        assert_eq!(read_last_log_lines(data_dir, "web", 2), "l3\nl4");
        assert_eq!(
            read_last_log_lines(data_dir, "web", 99),
            "l1\nl2\nl3\nl4",
            "asking for more lines than exist returns them all"
        );
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
