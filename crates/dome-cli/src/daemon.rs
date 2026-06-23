//! The `domed` supervisor: the control plane for persistent sandboxes.
//!
//! domed is the same signed `dome` binary re-executed via the hidden `__domed`
//! subcommand. It owns a single, user-private (0600) unix domain socket and speaks the
//! newline-delimited JSON protocol in [`dome_proto::control`]: request/response for
//! commands plus a `subscribe` verb that streams async events.
//!
//! This slice (the skeleton) ships only a [`FakeLauncher`] — no hypervisor — so the
//! registry and control behaviours are fully exercisable in normal `cargo test`. The
//! single new seam is the [`WorkerLauncher`] trait: domed creates/manages workers
//! through it, and a later slice swaps in the real VM-backed launcher without touching
//! the control plane.
//!
//! Invariant: **domed startup boots zero VMs.** Nothing in `run_supervisor` calls
//! [`WorkerLauncher::launch`], so auto-spawn-on-`ls` stays cheap (tens of ms).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use dome_proto::control::{
    AttachResult, Command, Event, ListResult, Request, Response, SandboxInfo, StatusResult,
    PROTOCOL_VERSION,
};

use crate::lock::{self, Lock};
use crate::sandbox;
use crate::worker;

/// How long the CLI waits for an auto-spawned domed to come up before giving up.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval while waiting for the control socket to appear / disappear.
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Accept-loop poll cadence (the listener is non-blocking so it can notice shutdown).
const ACCEPT_POLL: Duration = Duration::from_millis(50);
/// How long domed stays resident with no workers AND no clients before shutting itself
/// down. Long enough that a burst of one-off commands keeps reusing one daemon, short
/// enough that a truly idle daemon does not linger. Re-armed on any client connection or
/// while any worker runs.
const IDLE_GRACE: Duration = Duration::from_secs(300);

/// Pure idle-shutdown predicate: domed should exit itself only when nothing is using it —
/// no client connection is held AND no worker VM is running — and it has been so for at
/// least the grace period. Factored out so the timing is unit-testable without a daemon.
fn should_shut_down_idle(
    clients: usize,
    workers: usize,
    idle_for: Duration,
    grace: Duration,
) -> bool {
    clients == 0 && workers == 0 && idle_for >= grace
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// The daemon's private state directory: `{data_dir}/daemon`.
fn daemon_dir(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join("daemon")
}

/// The control socket path: `{data_dir}/daemon/domed.sock`.
fn socket_path(data_dir: &str) -> PathBuf {
    daemon_dir(data_dir).join("domed.sock")
}

/// The domed-singleton lock path: `{data_dir}/daemon/domed.lock`.
fn lock_path(data_dir: &str) -> PathBuf {
    daemon_dir(data_dir).join("domed.lock")
}

/// The domed log file path: `{data_dir}/daemon/domed.log`.
fn log_path(data_dir: &str) -> PathBuf {
    daemon_dir(data_dir).join("domed.log")
}

// ---------------------------------------------------------------------------
// WorkerLauncher seam
// ---------------------------------------------------------------------------

/// A live worker domed is supervising: its identity, OS pid (if a real process), start
/// time, and the user-private data-plane socket clients connect to directly.
#[allow(dead_code)] // `started` drives future uptime/age reporting.
pub(crate) struct WorkerHandle {
    /// Sandbox name this worker serves.
    pub name: String,
    /// Worker process id, if it is a real OS process (None for the fake / re-adopted).
    pub pid: Option<u32>,
    /// When the worker was launched (drives uptime/age reporting).
    pub started: Instant,
    /// Absolute path of the worker's data-plane socket.
    pub socket_path: PathBuf,
}

/// Whether a worker identified by `pid`/`socket` is still alive. Prefer the OS pid
/// (`kill(pid, 0)`) when we have it (a real launched worker); otherwise fall back to
/// whether its data-plane socket still accepts a connection (a re-adopted worker, or the
/// test fake whose liveness is its socket). This is the signal the crash reaper uses to
/// spot an unexpected exit; it is free-standing so the reaper can probe a snapshot of a
/// worker's identity without holding the registry lock across the (blocking) probe.
fn worker_is_alive(pid: Option<u32>, socket: &Path) -> bool {
    match pid {
        Some(pid) => pid_alive(pid),
        None => UnixStream::connect(socket).is_ok(),
    }
}

/// The single seam between domed and the VM backend. domed cold-boots and manages
/// workers exclusively through this trait, so the control plane stays testable against a
/// fake while the real hypervisor-backed launcher ([`VmWorkerLauncher`]) is dropped in
/// for production.
pub(crate) trait WorkerLauncher: Send + Sync {
    /// Ensure a worker process for sandbox `name` is running and reachable, cold-booting
    /// it with `boot` if needed. Returns a handle domed tracks in its registry.
    ///
    /// domed *startup* never calls this — only an explicit attach does — so the daemon
    /// boots zero VMs on its own.
    fn launch(&self, name: &str, boot: &serde_json::Value, data_dir: &str) -> Result<WorkerHandle>;
}

/// How long to wait for a freshly launched worker to cold-boot its VM and bind its
/// socket. A real VM boot takes seconds, so this is generous.
const WORKER_BOOT_TIMEOUT: Duration = Duration::from_secs(60);

/// The production launcher: re-execs the signed binary as a detached `dome __worker
/// <name>`, then waits for it to either become reachable or report a boot error.
pub(crate) struct VmWorkerLauncher;

impl WorkerLauncher for VmWorkerLauncher {
    fn launch(&self, name: &str, boot: &serde_json::Value, data_dir: &str) -> Result<WorkerHandle> {
        // Hand the worker its boot spec, then spawn it detached.
        worker::write_boot_spec(data_dir, name, boot)?;
        let pid = spawn_worker(name)?;
        let socket = worker::worker_socket_path(data_dir, name);

        // Wait until the worker is reachable, or it records a boot error, or it dies.
        let deadline = Instant::now() + WORKER_BOOT_TIMEOUT;
        loop {
            if UnixStream::connect(&socket).is_ok() {
                return Ok(WorkerHandle {
                    name: name.to_string(),
                    pid: Some(pid),
                    started: Instant::now(),
                    socket_path: socket,
                });
            }
            if let Some(err) = worker::take_worker_error(data_dir, name) {
                bail!("sandbox '{}' failed to boot: {}", name, err);
            }
            if !pid_alive(pid) {
                let err = worker::take_worker_error(data_dir, name)
                    .unwrap_or_else(|| "worker exited during boot (see its log)".to_string());
                bail!("sandbox '{}' failed to boot: {}", name, err);
            }
            if Instant::now() > deadline {
                bail!(
                    "sandbox '{}' worker did not become reachable within {WORKER_BOOT_TIMEOUT:?}",
                    name
                );
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

/// Re-exec this binary as a detached `dome __worker <name>` in its own session, so it
/// outlives both the spawning CLI and domed. Returns the worker pid.
fn spawn_worker(name: &str) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("locating current executable")?;
    let mut cmd = Command::new(exe);
    cmd.arg("__worker")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().context("spawning worker")?;
    Ok(child.id())
}

/// Whether `pid` is a live process (POSIX `kill(pid, 0)`).
fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// A hypervisor-free launcher used in tests. It boots no VM; it only records the worker
/// and writes a per-sandbox log line, and reports a socket path under the workers dir.
#[cfg(test)]
pub(crate) struct FakeLauncher;

#[cfg(test)]
impl WorkerLauncher for FakeLauncher {
    fn launch(
        &self,
        name: &str,
        _boot: &serde_json::Value,
        data_dir: &str,
    ) -> Result<WorkerHandle> {
        let log_dir = worker::workers_dir(data_dir);
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("creating worker log dir {}", log_dir.display()))?;
        let log = log_dir.join(format!("{name}.log"));
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
        {
            let _ = writeln!(f, "{} fake worker launched for '{}'", now_unix(), name);
        }
        Ok(WorkerHandle {
            name: name.to_string(),
            pid: None,
            started: Instant::now(),
            socket_path: worker::worker_socket_path(data_dir, name),
        })
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// A registry entry wrapping the worker handle. The attached-terminal count is NOT held
/// here: domed is not in the byte path, so it never observes session start/end. The
/// worker owns that count and domed queries it on demand (see [`worker::attached_count`]).
struct WorkerEntry {
    handle: WorkerHandle,
    /// Set when domed has asked this worker to stop, so the crash reaper treats its
    /// subsequent disappearance as an expected shutdown (a clean `sandbox.stopped`) rather
    /// than an unexpected exit (`sandbox.crashed`).
    stopping: bool,
}

/// domed's in-memory registry of live workers, keyed by sandbox name. Enforces
/// one-worker-per-name by construction.
struct Registry {
    workers: HashMap<String, WorkerEntry>,
}

impl Registry {
    fn new() -> Self {
        Self {
            workers: HashMap::new(),
        }
    }

    /// Register a freshly launched worker. Errors if a worker already exists for the
    /// name — domed never runs two workers for one sandbox.
    #[allow(dead_code)] // Driven by the worker-boot slice (#24); covered by tests here.
    fn insert(&mut self, handle: WorkerHandle) -> Result<()> {
        if self.workers.contains_key(&handle.name) {
            bail!("a worker is already running for sandbox '{}'", handle.name);
        }
        self.workers.insert(
            handle.name.clone(),
            WorkerEntry {
                handle,
                stopping: false,
            },
        );
        Ok(())
    }

    /// Remove a worker (e.g. on stop or crash), returning it if present.
    #[allow(dead_code)] // Driven by the worker-lifecycle slice (#27); covered by tests.
    fn remove(&mut self, name: &str) -> Option<WorkerEntry> {
        self.workers.remove(name)
    }

    fn len(&self) -> usize {
        self.workers.len()
    }

    /// Overlay live-worker state onto disk-derived rows: a sandbox with a live worker
    /// reads as `running`, and its `ATTACHED` count comes from `counts` — the live values
    /// domed queried from each worker (the source of truth). A running worker missing from
    /// `counts` (its count query failed) still reads as `running` with 0 attached.
    fn overlay(&self, infos: &mut [SandboxInfo], counts: &HashMap<String, usize>) {
        for info in infos.iter_mut() {
            if self.workers.contains_key(&info.name) {
                info.state = "running".to_string();
                info.attached = counts.get(&info.name).copied().unwrap_or(0);
            }
        }
    }

    /// Snapshot each live worker's name + data-plane socket, so the caller can query their
    /// attached counts without holding the registry lock across the socket round-trips.
    fn worker_sockets(&self) -> Vec<(String, PathBuf)> {
        self.workers
            .iter()
            .map(|(name, entry)| (name.clone(), entry.handle.socket_path.clone()))
            .collect()
    }

    /// Snapshot each worker's identity (name, pid, socket, whether a stop is in progress)
    /// so the crash reaper can probe liveness without holding the lock across the probe.
    fn reaper_snapshot(&self) -> Vec<(String, Option<u32>, PathBuf, bool)> {
        self.workers
            .iter()
            .map(|(name, e)| {
                (
                    name.clone(),
                    e.handle.pid,
                    e.handle.socket_path.clone(),
                    e.stopping,
                )
            })
            .collect()
    }

    /// Mark a worker as stopping so the reaper attributes its exit to a clean stop (a no-op
    /// if no worker is tracked for `name`).
    fn mark_stopping(&mut self, name: &str) {
        if let Some(entry) = self.workers.get_mut(name) {
            entry.stopping = true;
        }
    }

    /// Undo [`Registry::mark_stopping`] — used when a worker refuses a stop (terminals still
    /// attached), so a later unexpected exit is still surfaced as a crash, not swallowed as
    /// an expected stop.
    fn unmark_stopping(&mut self, name: &str) {
        if let Some(entry) = self.workers.get_mut(name) {
            entry.stopping = false;
        }
    }
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// The running supervisor: registry, launcher seam, event subscribers, and shutdown
/// flag. Shared across connection threads behind an [`Arc`].
struct Supervisor {
    data_dir: String,
    socket_path: PathBuf,
    log_path: PathBuf,
    started: Instant,
    pid: u32,
    registry: Mutex<Registry>,
    #[allow(dead_code)] // Wired now; first used to cold-boot workers in a later slice.
    launcher: Box<dyn WorkerLauncher>,
    /// Write-halves of connections that issued `subscribe`, for pushing events.
    subscribers: Mutex<Vec<UnixStream>>,
    shutdown: AtomicBool,
    /// Number of currently-open client connections. domed stays resident while any client
    /// holds a connection (e.g. a subscribed UI), and the idle monitor only considers
    /// shutting down when this is zero.
    clients: AtomicUsize,
    /// When domed last saw activity (a client connection, or while a worker ran). The idle
    /// grace period is measured from here, so a one-off command re-arms it.
    last_active: Mutex<Instant>,
    /// How long to stay resident while fully idle before self-shutdown (see [`IDLE_GRACE`];
    /// shortened in tests).
    idle_grace: Duration,
}

impl Supervisor {
    fn new(data_dir: String, launcher: Box<dyn WorkerLauncher>) -> Self {
        let socket_path = socket_path(&data_dir);
        let log_path = log_path(&data_dir);
        Self {
            data_dir,
            socket_path,
            log_path,
            started: Instant::now(),
            pid: std::process::id(),
            registry: Mutex::new(Registry::new()),
            launcher,
            subscribers: Mutex::new(Vec::new()),
            shutdown: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            idle_grace: IDLE_GRACE,
        }
    }

    /// Builder override for the idle-shutdown grace period (tests use a short one so the
    /// idle monitor fires promptly).
    #[cfg(test)]
    fn with_idle_grace(mut self, grace: Duration) -> Self {
        self.idle_grace = grace;
        self
    }

    /// Append a timestamped line to the domed log (best-effort).
    fn log(&self, msg: &str) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(f, "{} {}", now_unix(), msg);
        }
    }

    /// Build the current sandbox listing: disk scan overlaid with live-worker state. The
    /// attached-terminal count for each running worker is queried from the worker itself
    /// (domed is not in the byte path), so `ls` reflects attaches/detaches live.
    fn list(&self) -> Result<Vec<SandboxInfo>> {
        let mut infos = sandbox::collect_sandbox_infos(&self.data_dir)?;
        // Snapshot live workers under the lock, then query their counts WITHOUT holding it
        // (each query is a socket round-trip) so concurrent attaches/status stay responsive.
        let sockets = self.registry.lock().unwrap().worker_sockets();
        let mut counts = HashMap::new();
        for (name, socket) in sockets {
            if let Ok(n) = worker::attached_count(&socket) {
                counts.insert(name, n);
            }
        }
        self.registry.lock().unwrap().overlay(&mut infos, &counts);
        // A sandbox with no live worker but a crash marker reads as `failed` (the overlay
        // only ever promotes a row to `running`, so it never clobbers this).
        for info in infos.iter_mut() {
            if info.state != "running" && worker::is_failed(&self.data_dir, &info.name) {
                info.state = "failed".to_string();
            }
        }
        Ok(infos)
    }

    fn status(&self) -> StatusResult {
        StatusResult {
            protocol_version: PROTOCOL_VERSION,
            pid: self.pid,
            uptime_secs: self.started.elapsed().as_secs(),
            worker_count: self.registry.lock().unwrap().len(),
            socket_path: self.socket_path.to_string_lossy().to_string(),
        }
    }

    /// Attach to a sandbox: reuse its live worker if one is reachable, otherwise
    /// cold-boot one through the launcher (with the client-supplied `boot` spec). Mints a
    /// one-time token from the worker and returns where the client connects. domed is
    /// never in the resulting byte path.
    fn attach(&self, name: &str, boot: Option<serde_json::Value>) -> Result<AttachResult> {
        // Fast path: an already-running, reachable worker — just mint a token.
        if let Some((socket, pid)) = self.live_worker_socket(name) {
            let token = worker::mint_token(&socket)?;
            self.broadcast(&Event::bare("sandbox.attached"));
            return Ok(AttachResult {
                name: name.to_string(),
                worker_socket: socket.to_string_lossy().to_string(),
                token,
                worker_pid: pid,
                cold_booted: false,
            });
        }

        // Cold boot. The launcher blocks until the worker is reachable or fails; we hold
        // no registry lock across it, so concurrent status/list stay responsive.
        let boot = boot.ok_or_else(|| {
            anyhow::anyhow!("no boot spec supplied to cold-boot sandbox '{name}'")
        })?;
        self.log(&format!("cold-booting worker for '{name}'"));
        let handle = self.launcher.launch(name, &boot, &self.data_dir)?;
        // A fresh boot supersedes any prior crash: clear the failed marker so the sandbox
        // no longer reads as `failed`.
        worker::clear_failed_marker(&self.data_dir, name);
        let socket = handle.socket_path.clone();
        let pid = handle.pid.unwrap_or(0);
        // Register it (ignore a duplicate from a concurrent attach that raced us).
        let _ = self.registry.lock().unwrap().insert(handle);
        let token = worker::mint_token(&socket)?;
        self.broadcast(&Event::bare("sandbox.started"));
        Ok(AttachResult {
            name: name.to_string(),
            worker_socket: socket.to_string_lossy().to_string(),
            token,
            worker_pid: pid,
            cold_booted: true,
        })
    }

    /// Force a durable flush+save of a running sandbox. domed forwards the request to the
    /// sandbox's live worker (the only holder of the in-memory dirty buffer); the worker
    /// flushes+saves and then notifies domed, which broadcasts `sandbox.saved`. An idle
    /// sandbox has no unsaved state (its on-disk index already is the durable state), so
    /// this errors rather than silently no-op'ing — there is nothing to flush without a
    /// running worker.
    fn save(&self, name: &str) -> Result<()> {
        let (socket, _pid) = self.live_worker_socket(name).ok_or_else(|| {
            anyhow::anyhow!(
                "sandbox '{name}' is not running; nothing to save (it is already durable on disk)"
            )
        })?;
        worker::save_via_worker(&socket)
    }

    /// Stop a running sandbox: flush+save and shut its VM down. Refuses (naming the
    /// attached-terminal count) when terminals are still attached unless `force` is set —
    /// the worker is the source of truth for that count, so domed queries it first. With
    /// `force`, the worker tears the VM down anyway, which closes the in-flight guest
    /// sessions so attached clients detach. Errors if the sandbox is not running (there is
    /// no live worker to stop — an idle sandbox is already down). On success domed removes
    /// the worker from its registry and broadcasts `sandbox.stopped`.
    fn stop(&self, name: &str, force: bool) -> Result<()> {
        let (socket, _pid) = self
            .live_worker_socket(name)
            .ok_or_else(|| anyhow::anyhow!("sandbox '{name}' is not running"))?;

        // Mark the worker stopping FIRST so the reaper attributes its exit to this clean
        // stop (not a crash) for the whole stop window. The worker is the sole decider of
        // the attached-terminal guard — it owns the count and commits the stop atomically
        // with session start — so domed just forwards `force` and relays the verdict.
        self.registry.lock().unwrap().mark_stopping(name);
        self.log(&format!("stopping worker for '{name}' (force={force})"));
        match worker::stop_via_worker(&socket, force)? {
            worker::StopOutcome::Refused { attached } => {
                // The worker is still running; undo the stopping mark so a later
                // unexpected exit is still surfaced as a crash.
                self.registry.lock().unwrap().unmark_stopping(name);
                bail!(
                    "sandbox '{name}' has {attached} attached terminal(s); \
                     detach them first or stop with --force"
                );
            }
            worker::StopOutcome::Stopped => {}
        }

        // Wait for the worker to drain (save + VM teardown removes its socket), so the stop
        // is durable by the time the command returns.
        let deadline = Instant::now() + WORKER_BOOT_TIMEOUT;
        while Instant::now() < deadline {
            // The worker removes its socket only after the save + VM teardown completes, so
            // an unreachable socket means the stop is durable.
            if UnixStream::connect(&socket).is_err() {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }

        self.registry.lock().unwrap().remove(name);
        // A clean stop supersedes any prior crash state.
        worker::clear_failed_marker(&self.data_dir, name);
        self.broadcast(&Event {
            event: "sandbox.stopped".to_string(),
            data: Some(serde_json::json!({ "name": name })),
        });
        Ok(())
    }

    /// The data-plane socket of a live worker for `name`, if one is reachable. Drops a
    /// registry entry whose worker has died, and re-adopts a worker whose socket is live
    /// on disk but missing from the registry (e.g. after a domed restart).
    fn live_worker_socket(&self, name: &str) -> Option<(PathBuf, u32)> {
        let mut reg = self.registry.lock().unwrap();
        if let Some(entry) = reg.workers.get(name) {
            if UnixStream::connect(&entry.handle.socket_path).is_ok() {
                return Some((
                    entry.handle.socket_path.clone(),
                    entry.handle.pid.unwrap_or(0),
                ));
            }
            // The worker is gone; forget it so the caller cold-boots a fresh one.
            reg.remove(name);
            return None;
        }
        // Not tracked, but a worker socket may be live on disk — re-adopt it.
        let socket = worker::worker_socket_path(&self.data_dir, name);
        if UnixStream::connect(&socket).is_ok() {
            let handle = WorkerHandle {
                name: name.to_string(),
                pid: None,
                started: Instant::now(),
                socket_path: socket.clone(),
            };
            let _ = reg.insert(handle);
            return Some((socket, 0));
        }
        None
    }

    /// Push an event to every subscriber, dropping any whose connection has died.
    fn broadcast(&self, event: &Event) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain_mut(|stream| write_line(stream, event).is_ok());
    }

    /// Crash supervision: find any tracked worker that has exited, and classify it. A
    /// worker marked `stopping` exited because domed asked it to (a clean stop — already
    /// surfaced by [`Supervisor::stop`]); any other exit is unexpected, so domed marks the
    /// sandbox `failed`, stashes the worker's last log lines, and emits `sandbox.crashed`.
    /// domed never auto-restarts — a subsequent `shell` cold-boots from the last save.
    fn reap_dead_workers(&self) {
        let snapshot = self.registry.lock().unwrap().reaper_snapshot();
        for (name, pid, socket, stopping) in snapshot {
            if worker_is_alive(pid, &socket) {
                continue;
            }
            // Forget the dead worker. If it was already removed (e.g. by a concurrent
            // stop() or live_worker_socket probe), there is nothing left to report.
            if self.registry.lock().unwrap().remove(&name).is_none() {
                continue;
            }
            if stopping {
                continue; // expected exit; stop() owns the sandbox.stopped event
            }
            let tail = worker::read_last_log_lines(&self.data_dir, &name, 20);
            worker::write_failed_marker(&self.data_dir, &name, &tail);
            self.log(&format!(
                "worker for '{name}' exited unexpectedly; marked failed (no auto-restart)"
            ));
            self.broadcast(&Event {
                event: "sandbox.crashed".to_string(),
                data: Some(serde_json::json!({ "name": name, "log": tail })),
            });
        }
    }

    /// Discover workers that survived a previous domed (a crash, a restart, or simply a
    /// `daemon stop` that left the VMs running) and re-adopt them into the registry, so
    /// `ls`/attach see them immediately and sessions remain usable. Workers are independent
    /// processes keyed by their on-disk socket: a socket that answers the wire protocol is a
    /// live worker (re-adopted with `pid: None`, so the reaper falls back to socket liveness
    /// — see [`worker_is_alive`]); a socket file that no longer has a listener is stale from
    /// a crash and is reconciled away so a future cold boot can bind cleanly. Called once on
    /// startup, before serving.
    fn readopt_workers(&self) {
        for (name, socket) in worker::scan_worker_sockets(&self.data_dir) {
            // Liveness handshake: a real round-trip (not just a connect) confirms the worker
            // is actually serving, not a half-open socket.
            if worker::attached_count(&socket).is_ok() {
                let handle = WorkerHandle {
                    name: name.clone(),
                    pid: None,
                    started: Instant::now(),
                    socket_path: socket,
                };
                if self.registry.lock().unwrap().insert(handle).is_ok() {
                    self.log(&format!("re-adopted running worker for '{name}'"));
                }
            } else {
                // Stale socket from a crashed worker: reconcile it (same liveness-then-reclaim
                // pattern domed uses for its own socket and the lock module uses for locks).
                let _ = std::fs::remove_file(&socket);
                self.log(&format!("reconciled stale worker socket for '{name}'"));
            }
        }
    }

    /// Idle self-shutdown check: if no client connection is held and no worker is running,
    /// and that has been true for the whole grace period, domed shuts itself down so a
    /// daemon spun up for a one-off `ls` does not linger. While anything is using domed the
    /// idle timer is continually re-armed, so a busy or subscribed daemon never trips this.
    fn maybe_idle_shutdown(&self) {
        let clients = self.clients.load(Ordering::SeqCst);
        let workers = self.registry.lock().unwrap().len();
        if clients > 0 || workers > 0 {
            // Busy: re-arm the idle timer so the grace period only ever counts true idleness.
            *self.last_active.lock().unwrap() = Instant::now();
            return;
        }
        let idle_for = self.last_active.lock().unwrap().elapsed();
        if should_shut_down_idle(clients, workers, idle_for, self.idle_grace) {
            self.log(&format!(
                "idle for {:?} with no workers and no clients; shutting down",
                self.idle_grace
            ));
            self.broadcast(&Event::bare("daemon.stopping"));
            self.shutdown.store(true, Ordering::SeqCst);
        }
    }
}

/// Crash-reaper poll cadence: how often domed scans its registry for workers that have
/// exited unexpectedly. Brisk enough to surface a crash promptly without busy-spinning.
const REAPER_POLL: Duration = Duration::from_millis(500);

/// Spawn the background crash reaper. It polls [`Supervisor::reap_dead_workers`] every
/// [`REAPER_POLL`] until shutdown, so an unexpectedly-exited worker is surfaced as a
/// `sandbox.crashed` event even when no client is actively touching the sandbox.
fn spawn_reaper(sup: &Arc<Supervisor>) -> std::thread::JoinHandle<()> {
    let sup = Arc::clone(sup);
    std::thread::spawn(move || loop {
        if sup.shutdown.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(REAPER_POLL);
        if sup.shutdown.load(Ordering::SeqCst) {
            break;
        }
        sup.reap_dead_workers();
    })
}

/// Spawn the background idle monitor. It periodically evaluates
/// [`Supervisor::maybe_idle_shutdown`] and exits on shutdown. The tick is derived from the
/// grace period — frequent enough to notice an explicit shutdown promptly (so joining it on
/// stop is quick) and to fire soon after the grace elapses, capped so a long grace doesn't
/// busy-spin.
fn spawn_idle_monitor(sup: &Arc<Supervisor>) -> std::thread::JoinHandle<()> {
    let sup = Arc::clone(sup);
    let tick = (sup.idle_grace / 10).clamp(Duration::from_millis(25), Duration::from_millis(500));
    std::thread::spawn(move || loop {
        if sup.shutdown.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(tick);
        if sup.shutdown.load(Ordering::SeqCst) {
            break;
        }
        sup.maybe_idle_shutdown();
    })
}

/// Run the supervisor accept loop on `listener` until a `shutdown` is requested. The
/// listener must be non-blocking so the loop can observe the shutdown flag promptly.
fn serve(sup: &Arc<Supervisor>, listener: UnixListener) {
    sup.log(&format!("domed up (pid {})", sup.pid));
    loop {
        if sup.shutdown.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                // The non-blocking flag we set on the listener (so it can poll the
                // shutdown flag) is inherited by accepted connections; per-connection
                // reads must block, so clear it before handing the stream off.
                let _ = stream.set_nonblocking(false);
                let sup = Arc::clone(sup);
                std::thread::spawn(move || handle_conn(&sup, stream));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(e) => {
                // A transient accept error (e.g. a client that connected then dropped
                // before we accepted → ECONNABORTED, or an interrupted syscall) must not
                // tear down the whole control plane. Log it and keep serving; the poll
                // sleep also bounds the spin if some error were to persist.
                sup.log(&format!("accept error (continuing): {e}"));
                std::thread::sleep(ACCEPT_POLL);
            }
        }
    }
    sup.log("domed shutting down");
}

/// Decrements the live client count (and re-arms the idle timer) when a connection handler
/// returns — by normal close, error, or panic — so the count can never be left inflated.
struct ClientGuard<'a>(&'a Supervisor);

impl Drop for ClientGuard<'_> {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::SeqCst);
        // Start the idle grace from the moment the last client left.
        *self.0.last_active.lock().unwrap() = Instant::now();
    }
}

/// Handle one client connection: read newline-JSON requests, reply per request, and —
/// after a `subscribe` — keep the connection registered as an event subscriber until it
/// closes.
fn handle_conn(sup: &Arc<Supervisor>, stream: UnixStream) {
    // Count this connection so the idle monitor keeps domed resident while it is held, and
    // re-arm the idle timer (so even a brief one-off command resets the grace window). The
    // guard restores both on return, including the early try_clone failure below.
    sup.clients.fetch_add(1, Ordering::SeqCst);
    *sup.last_active.lock().unwrap() = Instant::now();
    let _client = ClientGuard(sup);

    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // client closed
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let _ = write_line(
                    &mut writer,
                    &Response::err(None, format!("invalid request: {e}")),
                );
                continue;
            }
        };

        if req.protocol_version != PROTOCOL_VERSION {
            let _ = write_line(
                &mut writer,
                &Response::err(
                    req.id,
                    format!(
                        "unsupported protocol version {} (domed speaks {})",
                        req.protocol_version, PROTOCOL_VERSION
                    ),
                ),
            );
            continue;
        }

        match req.command {
            Command::Status => {
                let result = serde_json::to_value(sup.status()).unwrap();
                let _ = write_line(&mut writer, &Response::ok(req.id, result));
            }
            Command::List => match sup.list() {
                Ok(sandboxes) => {
                    let result = serde_json::to_value(ListResult { sandboxes }).unwrap();
                    let _ = write_line(&mut writer, &Response::ok(req.id, result));
                }
                Err(e) => {
                    let _ = write_line(&mut writer, &Response::err(req.id, format!("{e:#}")));
                }
            },
            Command::Subscribe => {
                // Acknowledge, then register this connection's write-half so future
                // events are streamed to it. We keep reading to detect close.
                let _ = write_line(
                    &mut writer,
                    &Response::ok(req.id, serde_json::json!({ "subscribed": true })),
                );
                if let Ok(sub) = writer.try_clone() {
                    sup.subscribers.lock().unwrap().push(sub);
                }
            }
            Command::Shutdown => {
                let _ = write_line(
                    &mut writer,
                    &Response::ok(req.id, serde_json::json!({ "stopping": true })),
                );
                sup.broadcast(&Event::bare("daemon.stopping"));
                sup.shutdown.store(true, Ordering::SeqCst);
                break;
            }
            Command::Attach { name, boot } => match sup.attach(&name, boot) {
                Ok(result) => {
                    let result = serde_json::to_value(result).unwrap();
                    let _ = write_line(&mut writer, &Response::ok(req.id, result));
                }
                Err(e) => {
                    let _ = write_line(&mut writer, &Response::err(req.id, format!("{e:#}")));
                }
            },
            Command::Save { name } => match sup.save(&name) {
                // The worker emits the `sandbox.saved` event (via WorkerSaved) once it has
                // actually flushed, so we just report success/failure here.
                Ok(()) => {
                    let _ = write_line(
                        &mut writer,
                        &Response::ok(req.id, serde_json::json!({ "saved": true })),
                    );
                }
                Err(e) => {
                    let _ = write_line(&mut writer, &Response::err(req.id, format!("{e:#}")));
                }
            },
            Command::Stop { name, force } => match sup.stop(&name, force) {
                Ok(()) => {
                    let _ = write_line(
                        &mut writer,
                        &Response::ok(req.id, serde_json::json!({ "stopped": true })),
                    );
                }
                Err(e) => {
                    let _ = write_line(&mut writer, &Response::err(req.id, format!("{e:#}")));
                }
            },
            Command::WorkerSaved { name } => {
                // A worker reports that it flushed+saved (auto-flush, explicit, or stop).
                // Rebroadcast to subscribers, then acknowledge the worker.
                sup.broadcast(&Event {
                    event: "sandbox.saved".to_string(),
                    data: Some(serde_json::json!({ "name": name })),
                });
                let _ = write_line(
                    &mut writer,
                    &Response::ok(req.id, serde_json::json!({ "ok": true })),
                );
            }
        }
    }
}

/// Write `value` as a single JSON line and flush.
fn write_line(w: &mut impl Write, value: &impl serde::Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).map_err(std::io::Error::other)?;
    line.push('\n');
    w.write_all(line.as_bytes())?;
    w.flush()
}

/// Seconds since the unix epoch (best-effort; 0 if the clock is before the epoch).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Supervisor entry point (`dome __domed`)
// ---------------------------------------------------------------------------

/// Entry point for the hidden `dome __domed` subcommand: become the singleton
/// supervisor and serve the control socket until shutdown. If another live domed already
/// holds the singleton lock, this returns immediately (the other one wins).
pub(crate) fn run_supervisor(data_dir: &str) -> Result<()> {
    let dir = daemon_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating daemon dir {}", dir.display()))?;
    // The daemon dir is single-user private.
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));

    // Singleton lock: Owner → we are domed; Fork → another live domed already runs.
    let guard = match lock::acquire(&lock_path(data_dir))? {
        Lock::Owner(g) => g,
        Lock::Fork => return Ok(()),
    };

    let sock = socket_path(data_dir);
    // A stale socket from a crashed domed would make bind fail; the singleton lock just
    // proved no live domed owns it, so reclaiming it is safe.
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("binding control socket {}", sock.display()))?;
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    listener
        .set_nonblocking(true)
        .context("setting control socket non-blocking")?;

    let sup = Arc::new(Supervisor::new(
        data_dir.to_string(),
        Box::new(VmWorkerLauncher),
    ));
    // Re-adopt any workers that survived a previous domed (and reconcile stale sockets)
    // BEFORE serving, so the first `ls`/attach already reflects them. Workers are
    // independent processes and are unaffected by domed stopping/crashing.
    sup.readopt_workers();
    let reaper = spawn_reaper(&sup);
    let idle = spawn_idle_monitor(&sup);
    serve(&sup, listener);
    let _ = reaper.join();
    let _ = idle.join();

    let _ = std::fs::remove_file(&sock);
    drop(guard); // releases the singleton lock
    Ok(())
}

// ---------------------------------------------------------------------------
// Client helpers (CLI side)
// ---------------------------------------------------------------------------

/// Connect to a running domed, or `None` if none is reachable (no socket, or a stale
/// socket with no listener).
fn try_connect(data_dir: &str) -> Option<UnixStream> {
    UnixStream::connect(socket_path(data_dir)).ok()
}

/// Send one request and read the single-line response.
fn request(stream: &mut UnixStream, command: Command) -> Result<Response> {
    write_line(stream, &Request::new(Some(1), command)).context("sending request to domed")?;
    let mut reader = BufReader::new(stream.try_clone().context("cloning daemon socket")?);
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .context("reading daemon response")?;
    if n == 0 {
        bail!("domed closed the connection without responding");
    }
    let resp: Response = serde_json::from_str(line.trim())
        .with_context(|| format!("parsing daemon response: {}", line.trim()))?;
    Ok(resp)
}

/// Re-exec this binary as `dome __domed`, detached into its own session so it outlives
/// the spawning CLI process. Reuses the signed binary (entitlement-bound) per the PRD.
fn spawn_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("locating current executable")?;
    let mut cmd = Command::new(exe);
    cmd.arg("__domed")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Detach into a new session so closing the parent terminal does not signal domed.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("spawning domed")?;
    Ok(())
}

/// Ensure a domed is running and return a connection to it, auto-spawning one if needed.
fn ensure_daemon(data_dir: &str) -> Result<UnixStream> {
    if let Some(s) = try_connect(data_dir) {
        return Ok(s);
    }
    spawn_daemon()?;
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(s) = try_connect(data_dir) {
            return Ok(s);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    bail!("domed did not become reachable within {:?}", SPAWN_TIMEOUT);
}

/// Decode a typed result payload from a successful response.
fn decode_result<T: serde::de::DeserializeOwned>(resp: Response) -> Result<T> {
    if !resp.ok {
        bail!(
            "domed error: {}",
            resp.error.unwrap_or_else(|| "unknown error".to_string())
        );
    }
    let value = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("domed returned no result payload"))?;
    serde_json::from_value(value).context("decoding daemon result")
}

/// `dome sandbox ls` data path: route through domed (auto-spawning it) and return the
/// listing. Keeps a single code path and uniform output whether or not domed was already
/// up.
pub(crate) fn list_via_daemon(data_dir: &str) -> Result<Vec<SandboxInfo>> {
    let mut stream = ensure_daemon(data_dir)?;
    let resp = request(&mut stream, Command::List)?;
    let result: ListResult = decode_result(resp)?;
    Ok(result.sandboxes)
}

/// `dome sandbox shell`/`run` data path: route an attach through domed (auto-spawning
/// it), returning where to connect to the worker plus a one-time token. The actual
/// session bytes flow directly CLI↔worker afterwards — domed is not in that path.
pub(crate) fn attach_via_daemon(
    data_dir: &str,
    name: &str,
    boot: serde_json::Value,
) -> Result<AttachResult> {
    let mut stream = ensure_daemon(data_dir)?;
    let resp = request(
        &mut stream,
        Command::Attach {
            name: name.to_string(),
            boot: Some(boot),
        },
    )?;
    decode_result(resp)
}

/// `dome sandbox save <name>` data path: route a save through domed (auto-spawning it).
/// domed forwards it to the sandbox's worker, which performs the durable flush+save.
pub(crate) fn save_via_daemon(data_dir: &str, name: &str) -> Result<()> {
    let mut stream = ensure_daemon(data_dir)?;
    let resp = request(
        &mut stream,
        Command::Save {
            name: name.to_string(),
        },
    )?;
    if !resp.ok {
        bail!(
            "domed error: {}",
            resp.error.unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(())
}

/// `dome sandbox stop [--force] <name>` data path: route a stop through domed (auto-
/// spawning it). domed enforces the attached-terminal guard, then tells the worker to
/// flush+save and shut the VM down.
pub(crate) fn stop_via_daemon(data_dir: &str, name: &str, force: bool) -> Result<()> {
    let mut stream = ensure_daemon(data_dir)?;
    let resp = request(
        &mut stream,
        Command::Stop {
            name: name.to_string(),
            force,
        },
    )?;
    if !resp.ok {
        bail!(
            "domed error: {}",
            resp.error.unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(())
}

/// Worker → domed: report that a sandbox was saved, so domed broadcasts `sandbox.saved`
/// to subscribers. Best-effort and non-fatal — a worker outlives domed, so a save still
/// succeeds when no domed is up to notify (there are no subscribers then anyway). Connects
/// only if a domed is already reachable; it never spawns one.
pub(crate) fn notify_saved(data_dir: &str, name: &str) {
    if let Some(mut stream) = try_connect(data_dir) {
        let _ = request(
            &mut stream,
            Command::WorkerSaved {
                name: name.to_string(),
            },
        );
    }
}

// ---------------------------------------------------------------------------
// `dome daemon start|stop|status`
// ---------------------------------------------------------------------------

/// `dome daemon start`: pre-warm the control plane. No-op (with a notice) if already up.
pub(crate) fn start(data_dir: &str) -> Result<()> {
    if let Some(mut s) = try_connect(data_dir) {
        let st: StatusResult = decode_result(request(&mut s, Command::Status)?)?;
        println!(
            "dome: daemon already running (pid {}, uptime {}s)",
            st.pid, st.uptime_secs
        );
        return Ok(());
    }
    let mut s = ensure_daemon(data_dir)?;
    let st: StatusResult = decode_result(request(&mut s, Command::Status)?)?;
    println!(
        "dome: daemon started (pid {}, socket {})",
        st.pid, st.socket_path
    );
    Ok(())
}

/// `dome daemon status`: report up/down with pid, uptime, worker count, and socket path.
pub(crate) fn status(data_dir: &str) -> Result<()> {
    match try_connect(data_dir) {
        Some(mut s) => {
            let st: StatusResult = decode_result(request(&mut s, Command::Status)?)?;
            println!("dome: daemon is up");
            println!("  pid:     {}", st.pid);
            println!("  uptime:  {}s", st.uptime_secs);
            println!("  workers: {}", st.worker_count);
            println!("  socket:  {}", st.socket_path);
        }
        None => println!("dome: daemon is down"),
    }
    Ok(())
}

/// `dome daemon stop`: shut the control plane down. Running sandboxes are unaffected.
pub(crate) fn stop(data_dir: &str) -> Result<()> {
    let mut s = match try_connect(data_dir) {
        Some(s) => s,
        None => {
            println!("dome: daemon is not running");
            return Ok(());
        }
    };
    let resp = request(&mut s, Command::Shutdown)?;
    if !resp.ok {
        bail!(
            "domed refused to stop: {}",
            resp.error.unwrap_or_else(|| "unknown error".to_string())
        );
    }
    // Wait for the socket to disappear so the command is durable.
    let sock = socket_path(data_dir);
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    while Instant::now() < deadline {
        if try_connect(data_dir).is_none() && !sock.exists() {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    println!("dome: daemon stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — drive the control socket against the FakeLauncher (hypervisor-free).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// A test harness: a supervisor serving on a temp socket in a background thread.
    struct TestDaemon {
        data_dir: tempfile::TempDir,
        handle: Option<std::thread::JoinHandle<()>>,
        reaper: Option<std::thread::JoinHandle<()>>,
    }

    impl TestDaemon {
        fn start() -> Self {
            let data_dir = tempfile::tempdir().unwrap();
            let dir = daemon_dir(data_dir.path().to_str().unwrap());
            std::fs::create_dir_all(&dir).unwrap();
            let sock = socket_path(data_dir.path().to_str().unwrap());
            let listener = UnixListener::bind(&sock).unwrap();
            listener.set_nonblocking(true).unwrap();
            let sup = Arc::new(Supervisor::new(
                data_dir.path().to_str().unwrap().to_string(),
                Box::new(FakeLauncher),
            ));
            let reaper = spawn_reaper(&sup);
            let handle = std::thread::spawn(move || serve(&sup, listener));
            Self {
                data_dir,
                handle: Some(handle),
                reaper: Some(reaper),
            }
        }

        fn data_dir(&self) -> &str {
            self.data_dir.path().to_str().unwrap()
        }

        fn connect(&self) -> UnixStream {
            // The accept loop binds before the thread spawns, so connect succeeds at once.
            UnixStream::connect(socket_path(self.data_dir())).unwrap()
        }
    }

    impl Drop for TestDaemon {
        fn drop(&mut self) {
            // Ask the serve loop to exit, then join so no thread leaks across tests.
            if let Some(mut s) = try_connect(self.data_dir()) {
                let _ = request(&mut s, Command::Shutdown);
            }
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            if let Some(r) = self.reaper.take() {
                let _ = r.join();
            }
        }
    }

    fn write_sandbox_idx(data_dir: &str, name: &str, written_chunks: usize) {
        let dir = Path::new(data_dir).join("sandboxes");
        std::fs::create_dir_all(&dir).unwrap();
        let mut idx = dome_store::ChunkIndex::new(64 * 1024 * 1024);
        idx.fallback_path = Some(
            Path::new(data_dir)
                .join("rootfs-1.2.3.ext4")
                .to_string_lossy()
                .to_string(),
        );
        for i in 0..written_chunks {
            idx.set_hash(i, format!("hash{i:08x}"));
        }
        idx.save(dir.join(format!("{name}.idx")).to_str().unwrap())
            .unwrap();
    }

    #[test]
    fn status_reports_protocol_version_pid_and_zero_workers() {
        let d = TestDaemon::start();
        let mut s = d.connect();
        let st: StatusResult = decode_result(request(&mut s, Command::Status).unwrap()).unwrap();
        assert_eq!(st.protocol_version, PROTOCOL_VERSION);
        assert_eq!(st.worker_count, 0, "domed boots zero workers");
        assert_eq!(st.pid, std::process::id());
        assert!(st.socket_path.ends_with("domed.sock"));
    }

    #[test]
    fn list_is_empty_with_no_sandboxes_on_disk() {
        let d = TestDaemon::start();
        let mut s = d.connect();
        let result: ListResult = decode_result(request(&mut s, Command::List).unwrap()).unwrap();
        assert!(result.sandboxes.is_empty());
    }

    #[test]
    fn list_reflects_disk_sandboxes_as_idle() {
        let d = TestDaemon::start();
        write_sandbox_idx(d.data_dir(), "web", 1);
        let mut s = d.connect();
        let result: ListResult = decode_result(request(&mut s, Command::List).unwrap()).unwrap();
        let row = result
            .sandboxes
            .iter()
            .find(|r| r.name == "web")
            .expect("web sandbox should be listed");
        assert_eq!(row.state, "idle", "no live worker → idle");
        assert_eq!(row.attached, 0);
        assert_eq!(row.size_bytes, 64 * 1024, "one written chunk → 64 KiB");
        assert_eq!(row.base, "1.2.3");
    }

    #[test]
    fn unsupported_protocol_version_is_rejected() {
        let d = TestDaemon::start();
        let mut s = d.connect();
        // Hand-craft a request with a bogus protocol version.
        write_line(
            &mut s,
            &serde_json::json!({ "protocol_version": 9999, "id": 1, "verb": "status" }),
        )
        .unwrap();
        let mut reader = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let resp: Response = serde_json::from_str(line.trim()).unwrap();
        assert!(!resp.ok, "a version mismatch must be rejected");
        assert!(resp.error.unwrap().contains("protocol version"));
    }

    #[test]
    fn a_malformed_request_gets_an_error_not_a_crash() {
        let d = TestDaemon::start();
        let mut s = d.connect();
        s.write_all(b"this is not json\n").unwrap();
        s.flush().unwrap();
        let mut reader = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let resp: Response = serde_json::from_str(line.trim()).unwrap();
        assert!(!resp.ok);
        // The connection is still usable for a well-formed request afterwards.
        let st: StatusResult = decode_result(request(&mut s, Command::Status).unwrap()).unwrap();
        assert_eq!(st.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn subscribers_receive_the_shutdown_event_and_unknown_events_are_ignored() {
        let d = TestDaemon::start();

        // Connection A subscribes and reads the ack.
        let mut sub = d.connect();
        write_line(&mut sub, &Request::new(Some(1), Command::Subscribe)).unwrap();
        let mut sub_reader = BufReader::new(sub.try_clone().unwrap());
        let mut ack = String::new();
        sub_reader.read_line(&mut ack).unwrap();
        let ack: Response = serde_json::from_str(ack.trim()).unwrap();
        assert!(ack.ok, "subscribe should be acknowledged");

        // Connection B triggers shutdown, which broadcasts `daemon.stopping`.
        let mut ctl = d.connect();
        let _ = request(&mut ctl, Command::Shutdown);

        // A consumer that only knows `daemon.stopping` skips any other event it sees —
        // demonstrating forward-compatible "ignore unknown events" behaviour.
        let mut saw_stopping = false;
        let mut line = String::new();
        while sub_reader.read_line(&mut line).unwrap_or(0) != 0 {
            if let Ok(ev) = serde_json::from_str::<Event>(line.trim()) {
                match ev.event.as_str() {
                    "daemon.stopping" => {
                        saw_stopping = true;
                        break;
                    }
                    _ => { /* unknown event: ignore and keep reading */ }
                }
            }
            line.clear();
        }
        assert!(saw_stopping, "subscriber must receive daemon.stopping");
    }

    #[test]
    fn worker_saved_is_rebroadcast_to_subscribers_as_sandbox_saved() {
        // A worker reports a completed save via WorkerSaved; domed must rebroadcast it to
        // subscribers as a `sandbox.saved` event carrying the sandbox name. This is the
        // path that surfaces auto-flush/explicit saves to UI subscribers.
        let d = TestDaemon::start();

        // Connection A subscribes and reads the ack.
        let mut sub = d.connect();
        write_line(&mut sub, &Request::new(Some(1), Command::Subscribe)).unwrap();
        let mut sub_reader = BufReader::new(sub.try_clone().unwrap());
        let mut ack = String::new();
        sub_reader.read_line(&mut ack).unwrap();
        assert!(serde_json::from_str::<Response>(ack.trim()).unwrap().ok);

        // Connection B (standing in for the worker) reports a save.
        let mut worker = d.connect();
        let resp = request(
            &mut worker,
            Command::WorkerSaved {
                name: "web".to_string(),
            },
        )
        .unwrap();
        assert!(resp.ok, "domed should acknowledge the worker's save report");

        // The subscriber receives sandbox.saved with the sandbox name.
        let mut saw = false;
        let mut line = String::new();
        while sub_reader.read_line(&mut line).unwrap_or(0) != 0 {
            if let Ok(ev) = serde_json::from_str::<Event>(line.trim()) {
                if ev.event == "sandbox.saved" {
                    let name = ev.data.as_ref().and_then(|d| d["name"].as_str());
                    assert_eq!(name, Some("web"), "event must carry the sandbox name");
                    saw = true;
                    break;
                }
            }
            line.clear();
        }
        assert!(saw, "subscriber must receive sandbox.saved");
    }

    // --- Registry unit tests (one-worker-per-name + overlay) ---

    /// A throwaway data dir for launcher tests.
    fn tmp_data_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn registry_enforces_one_worker_per_name() {
        let mut reg = Registry::new();
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        let h = FakeLauncher
            .launch("web", &serde_json::json!({}), dd)
            .unwrap();
        reg.insert(h).unwrap();
        assert_eq!(reg.len(), 1);

        let dup = FakeLauncher
            .launch("web", &serde_json::json!({}), dd)
            .unwrap();
        let err = reg.insert(dup).unwrap_err();
        assert!(
            err.to_string().contains("already running"),
            "duplicate insert must be rejected: {err}"
        );
        assert_eq!(reg.len(), 1);

        assert!(reg.remove("web").is_some());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn registry_overlay_marks_a_live_worker_running() {
        let mut reg = Registry::new();
        let d = tmp_data_dir();
        let h = FakeLauncher
            .launch("api", &serde_json::json!({}), d.path().to_str().unwrap())
            .unwrap();
        reg.insert(h).unwrap();
        // Simulate two attached terminals: the count comes from the (queried) worker, not
        // from the registry entry, so we pass it in the live-counts map.
        let counts = HashMap::from([("api".to_string(), 2usize)]);

        let mut infos = vec![
            SandboxInfo {
                name: "api".to_string(),
                size_bytes: 0,
                base: "1.2.3".to_string(),
                state: "idle".to_string(),
                attached: 0,
                created_unix: 0,
            },
            SandboxInfo {
                name: "other".to_string(),
                size_bytes: 0,
                base: "1.2.3".to_string(),
                state: "idle".to_string(),
                attached: 0,
                created_unix: 0,
            },
        ];
        reg.overlay(&mut infos, &counts);
        assert_eq!(infos[0].state, "running");
        assert_eq!(infos[0].attached, 2);
        assert_eq!(
            infos[1].state, "idle",
            "a sandbox with no worker stays idle"
        );
    }

    #[test]
    fn overlay_keeps_a_worker_running_with_zero_attached_when_all_terminals_close() {
        // After the last terminal detaches the worker reports 0 attached but the VM stays
        // up: the sandbox must still read as `running` (count 0), not `idle`.
        let mut reg = Registry::new();
        let d = tmp_data_dir();
        let h = FakeLauncher
            .launch("api", &serde_json::json!({}), d.path().to_str().unwrap())
            .unwrap();
        reg.insert(h).unwrap();

        let mut infos = vec![SandboxInfo {
            name: "api".to_string(),
            size_bytes: 0,
            base: "1.2.3".to_string(),
            state: "idle".to_string(),
            attached: 7, // stale value that overlay must overwrite
            created_unix: 0,
        }];
        // An empty counts map mirrors "worker live, zero terminals attached".
        reg.overlay(&mut infos, &HashMap::new());
        assert_eq!(
            infos[0].state, "running",
            "the VM is still up after all detaches"
        );
        assert_eq!(infos[0].attached, 0, "no terminals attached → count 0");
    }

    // --- Lifecycle guards + crash supervision (#27) ---

    use std::sync::atomic::AtomicUsize;

    /// A minimal stand-in for a worker process: it listens on the worker data-plane socket
    /// and answers the worker wire protocol (`mint`/`count`/`stop`) with raw JSON, so
    /// domed's stop guard and attach/mint paths can be exercised without a hypervisor. Its
    /// liveness is the socket itself; `stop`/`crash`/drop take it down (so a probing
    /// `worker_is_alive(None, socket)` then fails, as it would for a real exited worker).
    struct FakeWorker {
        alive: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeWorker {
        fn start(socket: PathBuf, attached: usize) -> Self {
            if let Some(p) = socket.parent() {
                std::fs::create_dir_all(p).unwrap();
            }
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let alive = Arc::new(AtomicBool::new(true));
            let attached = Arc::new(AtomicUsize::new(attached));
            let al = Arc::clone(&alive);
            let at = Arc::clone(&attached);
            let sock = socket.clone();
            let handle = std::thread::spawn(move || {
                while al.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut s, _)) => {
                            let _ = s.set_nonblocking(false);
                            // Read one newline-terminated request line (unbuffered).
                            let mut buf = Vec::new();
                            let mut byte = [0u8; 1];
                            while let Ok(n) = s.read(&mut byte) {
                                if n == 0 || byte[0] == b'\n' {
                                    break;
                                }
                                buf.push(byte[0]);
                            }
                            let line = String::from_utf8_lossy(&buf);
                            let v: serde_json::Value =
                                serde_json::from_str(line.trim()).unwrap_or(serde_json::json!({}));
                            let resp = match v["op"].as_str().unwrap_or("") {
                                "mint" => serde_json::json!({ "token": "faketoken" }),
                                "count" => {
                                    serde_json::json!({ "attached": at.load(Ordering::SeqCst) })
                                }
                                "stop" => {
                                    // The worker is the sole decider of the attached guard:
                                    // refuse (naming the count) unless forced or idle.
                                    let force = v["force"].as_bool().unwrap_or(false);
                                    let attached = at.load(Ordering::SeqCst);
                                    if !force && attached > 0 {
                                        serde_json::json!({ "ok": false, "attached": attached })
                                    } else {
                                        al.store(false, Ordering::SeqCst);
                                        serde_json::json!({ "ok": true, "attached": 0 })
                                    }
                                }
                                _ => serde_json::json!({ "ok": false, "error": "unsupported" }),
                            };
                            let mut out = serde_json::to_string(&resp).unwrap();
                            out.push('\n');
                            let _ = s.write_all(out.as_bytes());
                            let _ = s.flush();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
                let _ = std::fs::remove_file(&sock);
            });
            Self {
                alive,
                handle: Some(handle),
            }
        }
    }

    impl Drop for FakeWorker {
        fn drop(&mut self) {
            self.alive.store(false, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// A launcher that boots a [`FakeWorker`] (listening on the real worker socket path)
    /// instead of a hypervisor, so cold-boot/attach succeeds end-to-end in tests.
    struct FakeWorkerLauncher {
        attached: usize,
        started: Mutex<Vec<FakeWorker>>,
    }

    impl WorkerLauncher for FakeWorkerLauncher {
        fn launch(
            &self,
            name: &str,
            _boot: &serde_json::Value,
            data_dir: &str,
        ) -> Result<WorkerHandle> {
            let socket = worker::worker_socket_path(data_dir, name);
            let fw = FakeWorker::start(socket.clone(), self.attached);
            self.started.lock().unwrap().push(fw);
            Ok(WorkerHandle {
                name: name.to_string(),
                pid: None,
                started: Instant::now(),
                socket_path: socket,
            })
        }
    }

    /// Subscribe to a supervisor's event stream via an in-process socket pair, returning the
    /// read half. Pushing the write half registers it as a subscriber, so a subsequent
    /// `broadcast` is readable here.
    fn subscribe(sup: &Supervisor) -> UnixStream {
        let (rx, tx) = UnixStream::pair().unwrap();
        sup.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Collect every event a subscriber received. Drops the supervisor's write halves first
    /// so the read side sees EOF and never blocks (events broadcast before the call are
    /// already buffered in the socket, so they are still read out).
    fn drain_events(sup: &Supervisor, rx: UnixStream) -> Vec<Event> {
        sup.subscribers.lock().unwrap().clear();
        let mut reader = BufReader::new(rx);
        let mut line = String::new();
        let mut events = Vec::new();
        while reader.read_line(&mut line).unwrap_or(0) != 0 {
            if let Ok(ev) = serde_json::from_str::<Event>(line.trim()) {
                events.push(ev);
            }
            line.clear();
        }
        events
    }

    #[test]
    fn an_unexpected_worker_exit_marks_failed_emits_crashed_and_does_not_restart() {
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        let sup = Arc::new(Supervisor::new(dd.to_string(), Box::new(FakeLauncher)));

        // Pre-seed a worker log so the crash report can surface its last lines.
        let logs = worker::workers_dir(dd);
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("web.log"), "boot ok\nlast gasp before dying\n").unwrap();

        // Register a live worker, then crash it (stop listening) so the reaper sees an
        // unexpected exit it was never told about.
        let socket = worker::worker_socket_path(dd, "web");
        let fw = FakeWorker::start(socket.clone(), 0);
        sup.registry
            .lock()
            .unwrap()
            .insert(WorkerHandle {
                name: "web".to_string(),
                pid: None,
                started: Instant::now(),
                socket_path: socket.clone(),
            })
            .unwrap();
        // While alive, the reaper leaves it be.
        sup.reap_dead_workers();
        assert_eq!(
            sup.registry.lock().unwrap().len(),
            1,
            "a live worker is not reaped"
        );

        drop(fw); // crash: the listener stops and the socket goes away
        while UnixStream::connect(&socket).is_ok() {
            std::thread::sleep(Duration::from_millis(5));
        }

        let rx = subscribe(&sup);
        sup.reap_dead_workers();

        // sandbox.crashed is emitted, carrying the name and the worker's last log lines.
        let events = drain_events(&sup, rx);
        let ev = events
            .iter()
            .find(|e| e.event == "sandbox.crashed")
            .expect("must emit sandbox.crashed");
        let data = ev.data.clone().unwrap();
        assert_eq!(data["name"], "web");
        assert!(
            data["log"].as_str().unwrap().contains("last gasp"),
            "crash event must surface the last log lines: {data}"
        );
        // The sandbox is marked failed (retrievable after the worker is gone)…
        assert!(
            worker::is_failed(dd, "web"),
            "a crashed sandbox is marked failed"
        );
        // …the dead worker is forgotten (no auto-restart)…
        assert_eq!(sup.registry.lock().unwrap().len(), 0, "no auto-restart");
        // …and reaping again does not re-emit (idempotent once removed).
        let rx2 = subscribe(&sup);
        sup.reap_dead_workers();
        assert!(
            !drain_events(&sup, rx2)
                .iter()
                .any(|e| e.event == "sandbox.crashed"),
            "an already-reaped crash must not re-fire"
        );
    }

    #[test]
    fn a_worker_asked_to_stop_is_not_reported_as_a_crash() {
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        let sup = Arc::new(Supervisor::new(dd.to_string(), Box::new(FakeLauncher)));

        let handle = WorkerHandle {
            name: "web".to_string(),
            pid: None,
            started: Instant::now(),
            socket_path: worker::worker_socket_path(dd, "web"),
        };
        sup.registry.lock().unwrap().insert(handle).unwrap();
        // Mark it stopping (as Supervisor::stop would) before it disappears.
        sup.registry.lock().unwrap().mark_stopping("web");

        let rx = subscribe(&sup);
        sup.reap_dead_workers();

        assert!(
            !drain_events(&sup, rx)
                .iter()
                .any(|e| e.event == "sandbox.crashed"),
            "an expected stop must not be reported as a crash"
        );
        assert!(
            !worker::is_failed(dd, "web"),
            "a clean stop is not a failure"
        );
        assert_eq!(
            sup.registry.lock().unwrap().len(),
            0,
            "the entry is still removed"
        );
    }

    #[test]
    fn stop_refuses_attached_terminals_unless_forced_and_emits_stopped() {
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        // 2 terminals attached; the fake worker reports the count over its socket.
        let sup = Arc::new(Supervisor::new(dd.to_string(), Box::new(FakeLauncher)));
        let socket = worker::worker_socket_path(dd, "web");
        let _fw = FakeWorker::start(socket.clone(), 2);
        sup.registry
            .lock()
            .unwrap()
            .insert(WorkerHandle {
                name: "web".to_string(),
                pid: None,
                started: Instant::now(),
                socket_path: socket.clone(),
            })
            .unwrap();

        // Non-force stop is refused and names the attached count.
        let err = sup.stop("web", false).unwrap_err();
        assert!(
            err.to_string().contains("2 attached"),
            "refusal must name the attached count: {err}"
        );
        assert_eq!(
            sup.registry.lock().unwrap().len(),
            1,
            "a refused stop leaves the worker running"
        );

        // Force stop detaches + tears down, and broadcasts sandbox.stopped.
        let rx = subscribe(&sup);
        sup.stop("web", true).unwrap();
        let events = drain_events(&sup, rx);
        let ev = events
            .iter()
            .find(|e| e.event == "sandbox.stopped")
            .expect("force stop must emit sandbox.stopped");
        assert_eq!(ev.data.clone().unwrap()["name"], "web");
        assert_eq!(
            sup.registry.lock().unwrap().len(),
            0,
            "the worker is gone after stop"
        );
        assert!(
            UnixStream::connect(&socket).is_err(),
            "the worker socket is torn down on stop"
        );
    }

    #[test]
    fn stop_with_no_attached_terminals_succeeds_without_force() {
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        let sup = Arc::new(Supervisor::new(dd.to_string(), Box::new(FakeLauncher)));
        let socket = worker::worker_socket_path(dd, "web");
        let _fw = FakeWorker::start(socket.clone(), 0); // no terminals attached
        sup.registry
            .lock()
            .unwrap()
            .insert(WorkerHandle {
                name: "web".to_string(),
                pid: None,
                started: Instant::now(),
                socket_path: socket,
            })
            .unwrap();

        sup.stop("web", false).unwrap();
        assert_eq!(sup.registry.lock().unwrap().len(), 0);
    }

    #[test]
    fn stopping_a_sandbox_that_is_not_running_errors_clearly() {
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        let sup = Arc::new(Supervisor::new(dd.to_string(), Box::new(FakeLauncher)));
        let err = sup.stop("ghost", false).unwrap_err();
        assert!(
            err.to_string().contains("not running"),
            "stopping an idle sandbox must say it is not running: {err}"
        );
    }

    #[test]
    fn list_surfaces_a_failed_sandbox_and_a_cold_boot_clears_it() {
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        write_sandbox_idx(dd, "web", 1);
        worker::write_failed_marker(dd, "web", "crashed here");

        // With a crash marker and no live worker, the sandbox lists as `failed`.
        let sup = Arc::new(Supervisor::new(
            dd.to_string(),
            Box::new(FakeWorkerLauncher {
                attached: 0,
                started: Mutex::new(Vec::new()),
            }),
        ));
        let infos = sup.list().unwrap();
        let row = infos.iter().find(|r| r.name == "web").unwrap();
        assert_eq!(
            row.state, "failed",
            "a crash marker with no worker reads as failed"
        );

        // Re-attaching cold-boots a fresh worker, which supersedes the failure.
        let boot = serde_json::json!({ "name": "web", "cwd": "/x", "vm_args": {} });
        sup.attach("web", Some(boot)).unwrap();
        assert!(
            !worker::is_failed(dd, "web"),
            "a cold boot clears the failed marker"
        );
        let infos = sup.list().unwrap();
        let row = infos.iter().find(|r| r.name == "web").unwrap();
        assert_eq!(
            row.state, "running",
            "after a cold boot the sandbox is running again"
        );
    }

    // --- Idle auto-shutdown + worker re-adoption (#29) ---

    #[test]
    fn idle_shutdown_predicate_requires_no_clients_no_workers_and_the_full_grace() {
        let grace = Duration::from_secs(300);
        // Fully idle past the grace → shut down.
        assert!(should_shut_down_idle(0, 0, grace, grace));
        assert!(should_shut_down_idle(
            0,
            0,
            grace + Duration::from_secs(1),
            grace
        ));
        // Any held client or running worker keeps it resident, no matter how long idle.
        assert!(!should_shut_down_idle(1, 0, grace * 10, grace));
        assert!(!should_shut_down_idle(0, 1, grace * 10, grace));
        // Idle, but not yet for the full grace → stay up.
        assert!(!should_shut_down_idle(0, 0, grace / 2, grace));
    }

    #[test]
    fn idle_monitor_shuts_down_when_there_are_no_workers_and_no_clients() {
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        let sup = Arc::new(
            Supervisor::new(dd.to_string(), Box::new(FakeLauncher))
                .with_idle_grace(Duration::from_millis(120)),
        );
        let monitor = spawn_idle_monitor(&sup);
        // No clients, no workers: after the grace elapses the monitor self-shuts.
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            sup.shutdown.load(Ordering::SeqCst),
            "an idle daemon must shut itself down after the grace period"
        );
        let _ = monitor.join();
    }

    #[test]
    fn idle_monitor_stays_resident_while_a_worker_is_running() {
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        let sup = Arc::new(
            Supervisor::new(dd.to_string(), Box::new(FakeLauncher))
                .with_idle_grace(Duration::from_millis(120)),
        );
        // A registered worker keeps domed resident even with no clients attached.
        sup.registry
            .lock()
            .unwrap()
            .insert(WorkerHandle {
                name: "web".to_string(),
                pid: None,
                started: Instant::now(),
                socket_path: worker::worker_socket_path(dd, "web"),
            })
            .unwrap();
        let monitor = spawn_idle_monitor(&sup);
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !sup.shutdown.load(Ordering::SeqCst),
            "domed must stay resident while a worker is running"
        );
        // Stop the monitor thread.
        sup.shutdown.store(true, Ordering::SeqCst);
        let _ = monitor.join();
    }

    #[test]
    fn idle_monitor_stays_resident_while_a_client_is_connected_then_shuts_after_it_leaves() {
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        let sup = Arc::new(
            Supervisor::new(dd.to_string(), Box::new(FakeLauncher))
                .with_idle_grace(Duration::from_millis(120)),
        );
        // A held client connection (e.g. a subscribed UI) keeps the timer re-armed.
        sup.clients.fetch_add(1, Ordering::SeqCst);
        let monitor = spawn_idle_monitor(&sup);
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !sup.shutdown.load(Ordering::SeqCst),
            "domed must stay resident while a client connection is held"
        );
        // The client leaves; after the grace from its departure domed shuts down.
        sup.clients.fetch_sub(1, Ordering::SeqCst);
        *sup.last_active.lock().unwrap() = Instant::now();
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            sup.shutdown.load(Ordering::SeqCst),
            "once the last client leaves, the idle grace elapses and domed shuts down"
        );
        let _ = monitor.join();
    }

    #[test]
    fn readopt_adopts_a_live_worker_and_reconciles_a_stale_socket() {
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        let sup = Arc::new(Supervisor::new(dd.to_string(), Box::new(FakeLauncher)));

        // A live worker (answers the wire protocol on its socket) must be re-adopted.
        let live = worker::worker_socket_path(dd, "web");
        let _fw = FakeWorker::start(live.clone(), 0);

        // A stale socket file with no listener (a crashed worker's leftover) must be
        // reconciled away. Binding then dropping a listener leaves the path on disk but
        // with nothing accepting, mirroring a crash.
        let stale = worker::worker_socket_path(dd, "ghost");
        {
            let l = UnixListener::bind(&stale).unwrap();
            drop(l);
        }
        assert!(
            stale.exists(),
            "precondition: the stale socket file is present"
        );

        sup.readopt_workers();

        // The live worker is now tracked; the sandbox reads as running and is attachable.
        assert!(
            sup.live_worker_socket("web").is_some(),
            "a live worker must be re-adopted into the registry"
        );
        assert_eq!(
            sup.registry.lock().unwrap().len(),
            1,
            "only the live worker is adopted"
        );
        // The stale socket is gone, so a future cold boot can bind cleanly.
        assert!(
            !stale.exists(),
            "a stale worker socket must be reconciled away"
        );
    }

    #[test]
    fn fake_launcher_writes_a_per_sandbox_log() {
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        FakeLauncher
            .launch("web", &serde_json::json!({}), dd)
            .unwrap();
        let log = worker::workers_dir(dd).join("web.log");
        assert!(log.exists(), "per-sandbox log must be written");
        let mut s = String::new();
        std::fs::File::open(&log)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert!(s.contains("web"), "log should mention the sandbox name");
    }
}
