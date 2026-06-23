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
use std::sync::atomic::{AtomicBool, Ordering};
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
                bail!("sandbox '{}' worker did not become reachable within {WORKER_BOOT_TIMEOUT:?}", name);
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
    fn launch(&self, name: &str, _boot: &serde_json::Value, data_dir: &str) -> Result<WorkerHandle> {
        let log_dir = worker::workers_dir(data_dir);
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("creating worker log dir {}", log_dir.display()))?;
        let log = log_dir.join(format!("{name}.log"));
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
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
        self.workers
            .insert(handle.name.clone(), WorkerEntry { handle });
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
        }
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

    /// The data-plane socket of a live worker for `name`, if one is reachable. Drops a
    /// registry entry whose worker has died, and re-adopts a worker whose socket is live
    /// on disk but missing from the registry (e.g. after a domed restart).
    fn live_worker_socket(&self, name: &str) -> Option<(PathBuf, u32)> {
        let mut reg = self.registry.lock().unwrap();
        if let Some(entry) = reg.workers.get(name) {
            if UnixStream::connect(&entry.handle.socket_path).is_ok() {
                return Some((entry.handle.socket_path.clone(), entry.handle.pid.unwrap_or(0)));
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

/// Handle one client connection: read newline-JSON requests, reply per request, and —
/// after a `subscribe` — keep the connection registered as an event subscriber until it
/// closes.
fn handle_conn(sup: &Arc<Supervisor>, stream: UnixStream) {
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
                let _ = write_line(&mut writer, &Response::err(None, format!("invalid request: {e}")));
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
    let listener =
        UnixListener::bind(&sock).with_context(|| format!("binding control socket {}", sock.display()))?;
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    listener
        .set_nonblocking(true)
        .context("setting control socket non-blocking")?;

    let sup = Arc::new(Supervisor::new(data_dir.to_string(), Box::new(VmWorkerLauncher)));
    serve(&sup, listener);

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
    let n = reader.read_line(&mut line).context("reading daemon response")?;
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
            let handle = std::thread::spawn(move || serve(&sup, listener));
            Self {
                data_dir,
                handle: Some(handle),
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
        assert_eq!(infos[1].state, "idle", "a sandbox with no worker stays idle");
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
        assert_eq!(infos[0].state, "running", "the VM is still up after all detaches");
        assert_eq!(infos[0].attached, 0, "no terminals attached → count 0");
    }

    #[test]
    fn fake_launcher_writes_a_per_sandbox_log() {
        let d = tmp_data_dir();
        let dd = d.path().to_str().unwrap();
        FakeLauncher.launch("web", &serde_json::json!({}), dd).unwrap();
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
