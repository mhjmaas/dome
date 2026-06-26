//! The JSONL writer task: owns the segment file, stamps identity, and guarantees the
//! tail of a run is flushed on shutdown.
//!
//! The writer runs on a dedicated thread with a current-thread tokio runtime so a
//! synchronous callsite (the CLI boot path) can construct and spawn it without a runtime
//! of its own. The proxy sends [`AuditEvent`]s into a bounded channel with `try_send`
//! (fail-open); the writer drains the channel, serializes each event into a self-describing
//! row stamped with `{sandbox, session}`, and flushes on a short cadence. On shutdown — an
//! explicit [`AuditHandle::shutdown`] or merely dropping the handle — it drains the channel,
//! flushes, and fsyncs within a bounded timeout so a stuck disk degrades to "lost a few tail
//! events" rather than "the process won't exit".

use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::Notify;

use crate::event::AuditEvent;

/// Bounded channel depth. Generous, since events are small; a saturated channel means the
/// guest is flooding faster than disk can absorb, and drop accounting (a later slice) makes
/// the gap visible. Until then `try_send` simply drops on a full channel — never blocking.
const DEFAULT_CHANNEL_CAPACITY: usize = 4096;
/// Flush after this many buffered events even if the cadence timer has not fired.
const DEFAULT_FLUSH_EVERY: usize = 256;
/// Flush at least this often so a low-traffic run's rows reach disk promptly.
const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
/// Upper bound on the shutdown drain+flush so a stuck disk cannot wedge process exit.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Identity and tuning for a writer bound to one `{sandbox, session}`.
pub struct WriterConfig {
    /// Central audit root (e.g. `<data_dir>/audit`). Rows land under
    /// `<audit_dir>/<sandbox>/<session>/events-0001.jsonl`.
    pub audit_dir: PathBuf,
    /// Sandbox name. Stamped onto every row.
    pub sandbox: String,
    /// Per-boot session id. Stamped onto every row.
    pub session: String,
    /// Bounded channel depth.
    pub channel_capacity: usize,
    /// Flush after this many buffered events.
    pub flush_every: usize,
    /// Flush at least this often.
    pub flush_interval: Duration,
}

impl WriterConfig {
    /// A config with the default cadence/capacity, bound to `{sandbox, session}` under
    /// `audit_dir`.
    pub fn new(
        audit_dir: impl Into<PathBuf>,
        sandbox: impl Into<String>,
        session: impl Into<String>,
    ) -> Self {
        WriterConfig {
            audit_dir: audit_dir.into(),
            sandbox: sandbox.into(),
            session: session.into(),
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            flush_every: DEFAULT_FLUSH_EVERY,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
        }
    }
}

/// Constructs writers. Stateless; the entry point is [`AuditWriter::spawn`].
pub struct AuditWriter;

impl AuditWriter {
    /// Create the session directory, open the first segment, and spawn the writer thread.
    /// Returns a handle whose [`AuditHandle::sender`] feeds the proxy and whose drop drains
    /// the channel and flushes.
    pub fn spawn(config: WriterConfig) -> std::io::Result<AuditHandle> {
        let session_dir = config
            .audit_dir
            .join(&config.sandbox)
            .join(&config.session);
        std::fs::create_dir_all(&session_dir)?;
        let segment_path = session_dir.join("events-0001.jsonl");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&segment_path)?;

        let (tx, rx) = mpsc::channel::<AuditEvent>(config.channel_capacity);
        let shutdown = Arc::new(Notify::new());

        let sandbox = config.sandbox.clone();
        let session = config.session.clone();
        let flush_every = config.flush_every;
        let flush_interval = config.flush_interval;
        let task_shutdown = shutdown.clone();

        let thread = std::thread::Builder::new()
            .name("dome-audit-writer".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => return,
                };
                rt.block_on(run_writer(
                    rx,
                    task_shutdown,
                    BufWriter::new(file),
                    sandbox,
                    session,
                    flush_every,
                    flush_interval,
                ));
            })?;

        Ok(AuditHandle {
            tx,
            shutdown,
            thread: Some(thread),
        })
    }
}

/// A live writer. Holding it keeps the writer thread running; [`AuditHandle::sender`] hands
/// the proxy a cloneable channel sender. Dropping the handle drains and flushes (the
/// `Drop`-guard backstop), so a callsite that merely drops it still gets the tail on disk.
pub struct AuditHandle {
    tx: mpsc::Sender<AuditEvent>,
    shutdown: Arc<Notify>,
    thread: Option<JoinHandle<()>>,
}

impl AuditHandle {
    /// A cloneable sender for the proxy. The proxy `try_send`s into it (fail-open).
    pub fn sender(&self) -> mpsc::Sender<AuditEvent> {
        self.tx.clone()
    }

    /// Explicitly drain and flush, joining the writer within the bounded timeout. Idempotent
    /// with the `Drop` backstop.
    pub fn shutdown(&mut self) {
        self.shutdown.notify_one();
        if let Some(thread) = self.thread.take() {
            join_bounded(thread, SHUTDOWN_TIMEOUT);
        }
    }
}

impl Drop for AuditHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Join a thread but never block longer than `timeout`: a watchdog thread performs the
/// (potentially blocking) join and signals completion. On timeout we proceed and leave the
/// watchdog to finish on its own, so a stuck disk cannot wedge process exit.
fn join_bounded(thread: JoinHandle<()>, timeout: Duration) {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = thread.join();
        let _ = done_tx.send(());
    });
    let _ = done_rx.recv_timeout(timeout);
}

/// The writer event loop. Serializes each event into a stamped JSONL row, flushing on a
/// short cadence or after `flush_every` events. Exits — flushing and fsyncing — on a
/// shutdown signal or when every sender has been dropped.
async fn run_writer(
    mut rx: mpsc::Receiver<AuditEvent>,
    shutdown: Arc<Notify>,
    mut out: BufWriter<std::fs::File>,
    sandbox: String,
    session: String,
    flush_every: usize,
    flush_interval: Duration,
) {
    let mut interval = tokio::time::interval(flush_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut since_flush = 0usize;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => {
                // Drain everything already queued, then flush+fsync and exit.
                while let Ok(event) = rx.try_recv() {
                    write_row(&mut out, &event, &sandbox, &session);
                }
                finalize(out);
                return;
            }
            event = rx.recv() => {
                match event {
                    Some(event) => {
                        write_row(&mut out, &event, &sandbox, &session);
                        since_flush += 1;
                        if since_flush >= flush_every {
                            let _ = out.flush();
                            since_flush = 0;
                        }
                    }
                    // All senders dropped: no more events will ever arrive.
                    None => {
                        finalize(out);
                        return;
                    }
                }
            }
            _ = interval.tick() => {
                if since_flush > 0 {
                    let _ = out.flush();
                    since_flush = 0;
                }
            }
        }
    }
}

/// Flush the buffer and fsync the file. Best-effort: failures degrade the log, never the
/// caller.
fn finalize(mut out: BufWriter<std::fs::File>) {
    let _ = out.flush();
    if let Ok(file) = out.into_inner() {
        let _ = file.sync_all();
    }
}

/// Serialize one event into a self-describing JSONL row, stamping `{sandbox, session}`, and
/// append it (newline-terminated). Serialization cannot fail for our event types, so a
/// failure here is dropped rather than disturbing the writer.
fn write_row(
    out: &mut BufWriter<std::fs::File>,
    event: &AuditEvent,
    sandbox: &str,
    session: &str,
) {
    let mut value = match serde_json::to_value(event) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => return,
    };
    value.insert("sandbox".to_string(), json!(sandbox));
    value.insert("session".to_string(), json!(session));
    let mut line = serde_json::Value::Object(value).to_string();
    line.push('\n');
    let _ = out.write_all(line.as_bytes());
}

/// Mint a per-boot session id: a millisecond Unix timestamp (sortable) plus a short nonce,
/// e.g. `0001717000000000-1f3a-0000`. The nonce is `pid`-low-bits + a process-wide counter,
/// so sessions are collision-safe both across rapid restarts of *different* processes
/// (distinct pids) and within one process (distinct counter), and lexically sortable by boot
/// time (the millisecond timestamp leads).
pub fn mint_session() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let ms = now.as_millis() as u64;
    let pid = std::process::id() as u64;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{:016}-{:04x}-{:04x}", ms, pid & 0xffff, seq & 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ConnKind;

    fn read_rows(path: &std::path::Path) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(path).expect("read segment");
        text.lines()
            .map(|l| serde_json::from_str(l).expect("valid json row"))
            .collect()
    }

    fn segment(audit_dir: &std::path::Path, sandbox: &str, session: &str) -> PathBuf {
        audit_dir
            .join(sandbox)
            .join(session)
            .join("events-0001.jsonl")
    }

    #[test]
    fn writes_self_describing_jsonl_under_sandbox_session_path() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_dir = tmp.path().to_path_buf();
        let mut handle =
            AuditWriter::spawn(WriterConfig::new(&audit_dir, "web", "sess-1")).unwrap();

        handle
            .sender()
            .try_send(AuditEvent::ConnOpen {
                conn_id: 7,
                dst: "1.2.3.4:443".into(),
                sni: Some("api.github.com".into()),
                conn_kind: ConnKind::Mitm,
                ts_ms: 1_717_000_000_000,
            })
            .unwrap();
        handle.shutdown();

        let rows = read_rows(&segment(&audit_dir, "web", "sess-1"));
        assert_eq!(rows.len(), 1, "exactly one row written");
        let row = &rows[0];
        assert_eq!(row["kind"], "conn_open");
        assert_eq!(row["conn_id"], 7);
        assert_eq!(row["dst"], "1.2.3.4:443");
        assert_eq!(row["sni"], "api.github.com");
        assert_eq!(row["conn_kind"], "mitm");
        // Self-describing: identity stamped by the writer, not the proxy.
        assert_eq!(row["sandbox"], "web");
        assert_eq!(row["session"], "sess-1");
    }

    #[test]
    fn writes_open_and_close_rows_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_dir = tmp.path().to_path_buf();
        let mut handle =
            AuditWriter::spawn(WriterConfig::new(&audit_dir, "web", "sess-2")).unwrap();
        let tx = handle.sender();

        tx.try_send(AuditEvent::ConnOpen {
            conn_id: 1,
            dst: "10.0.0.9:80".into(),
            sni: None,
            conn_kind: ConnKind::PlainTcp,
            ts_ms: 1_000,
        })
        .unwrap();
        tx.try_send(AuditEvent::ConnClose {
            conn_id: 1,
            bytes_tx: 42,
            bytes_rx: 1024,
            duration_ms: 5,
            ts_ms: 1_005,
        })
        .unwrap();
        handle.shutdown();

        let rows = read_rows(&segment(&audit_dir, "web", "sess-2"));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["kind"], "conn_open");
        // Plain TCP has no SNI: the field is omitted entirely rather than null.
        assert!(rows[0].get("sni").is_none());
        assert_eq!(rows[0]["conn_kind"], "plain_tcp");
        assert_eq!(rows[1]["kind"], "conn_close");
        assert_eq!(rows[1]["conn_id"], 1);
        assert_eq!(rows[1]["bytes_tx"], 42);
        assert_eq!(rows[1]["bytes_rx"], 1024);
        assert_eq!(rows[1]["duration_ms"], 5);
    }

    #[test]
    fn dropping_the_handle_flushes_the_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_dir = tmp.path().to_path_buf();
        // No explicit shutdown(): only the Drop-guard runs.
        {
            let handle =
                AuditWriter::spawn(WriterConfig::new(&audit_dir, "eph", "sess-3")).unwrap();
            handle
                .sender()
                .try_send(AuditEvent::ConnOpen {
                    conn_id: 99,
                    dst: "8.8.8.8:443".into(),
                    sni: Some("dns.google".into()),
                    conn_kind: ConnKind::BlindTunnel,
                    ts_ms: 2_000,
                })
                .unwrap();
        } // handle dropped here

        let rows = read_rows(&segment(&audit_dir, "eph", "sess-3"));
        assert_eq!(rows.len(), 1, "Drop-guard must flush the tail event");
        assert_eq!(rows[0]["conn_kind"], "blind_tunnel");
        assert_eq!(rows[0]["conn_id"], 99);
    }

    #[test]
    fn sessions_are_unique_and_sortable() {
        let a = mint_session();
        let b = mint_session();
        assert_ne!(a, b, "rapid mints must not collide");
        // Lexical order tracks time: a was minted first.
        assert!(a <= b, "sessions are lexically sortable by boot time");
    }
}
