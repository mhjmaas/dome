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
use std::sync::atomic::{AtomicU64, Ordering};
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
/// Roll to a fresh segment once the current one reaches this many bytes (~64 MB), so files
/// stay tail-able and the reaper can trim oldest segments of even a still-running session.
const DEFAULT_SEGMENT_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Roll at this many events even before the byte cap, whichever comes first (~100k).
const DEFAULT_SEGMENT_MAX_EVENTS: u64 = 100_000;
/// Always-on per-sandbox safety bound: after each rotation, oldest segments across the
/// sandbox's sessions are unlinked until the sandbox audit dir is back under this (~512 MB).
/// Holds even if `dome prune` is never run; can trim oldest segments of a live session.
const DEFAULT_SANDBOX_MAX_BYTES: u64 = 512 * 1024 * 1024;

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
    /// Roll to a new segment once the current one reaches this many bytes.
    pub segment_max_bytes: u64,
    /// Roll to a new segment once the current one reaches this many events.
    pub segment_max_events: u64,
    /// Per-sandbox total-size ceiling enforced inline at each rotation.
    pub sandbox_max_bytes: u64,
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
            segment_max_bytes: DEFAULT_SEGMENT_MAX_BYTES,
            segment_max_events: DEFAULT_SEGMENT_MAX_EVENTS,
            sandbox_max_bytes: DEFAULT_SANDBOX_MAX_BYTES,
        }
    }

    /// Override the rotation caps and the per-sandbox size ceiling. Intended for tests, which
    /// drive rotation and trimming with small inputs.
    pub fn with_caps(
        mut self,
        segment_max_bytes: u64,
        segment_max_events: u64,
        sandbox_max_bytes: u64,
    ) -> Self {
        self.segment_max_bytes = segment_max_bytes;
        self.segment_max_events = segment_max_events;
        self.sandbox_max_bytes = sandbox_max_bytes;
        self
    }

    /// Override the bounded channel depth. A tiny depth lets a test (or an ops knob) make the
    /// channel saturate deterministically so drop accounting is exercised.
    pub fn with_channel_capacity(mut self, channel_capacity: usize) -> Self {
        self.channel_capacity = channel_capacity.max(1);
        self
    }
}

/// A cloneable, fail-open handle the proxy sends [`AuditEvent`]s through. Wraps the bounded
/// channel sender plus a shared drop counter: [`AuditSink::try_send`] never blocks egress —
/// on a full channel it drops the event and bumps the counter, which the writer task reads to
/// materialize `dropped` gap markers. The whole hot path is one atomic increment in the worst
/// case.
#[derive(Clone)]
pub struct AuditSink {
    tx: mpsc::Sender<AuditEvent>,
    /// Events lost to a full channel, monotonically increasing. Shared with the writer task,
    /// which converts increments into `dropped` markers.
    drops: Arc<AtomicU64>,
}

impl AuditSink {
    /// Try to enqueue an event without ever blocking. If the channel is full the event is
    /// dropped and the shared drop counter is incremented so the writer can label the gap.
    /// A closed channel (writer gone) is a silent no-op — there is nothing left to record to.
    pub fn try_send(&self, event: AuditEvent) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(event) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Total events dropped so far due to a full channel. Exposed for visibility/diagnostics.
    pub fn dropped(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }
}

/// Owns the current on-disk segment for one `{sandbox, session}` and the rotation +
/// size-ceiling policy. Rows are appended through [`SegmentSink::write_line`], which rolls to
/// `events-NNNN.jsonl` once the segment hits a byte or event cap and, immediately after each
/// roll, trims the oldest segments across the whole sandbox until under the size ceiling.
struct SegmentSink {
    /// `<audit_dir>/<sandbox>/<session>` — where this session's segments live.
    session_dir: PathBuf,
    /// `<audit_dir>/<sandbox>` — the trim scope: all sessions of this sandbox.
    sandbox_dir: PathBuf,
    /// Current segment number (1-based), formatted as `events-NNNN.jsonl`.
    seg_no: u32,
    /// Path of the currently-open segment; never trimmed while active.
    path: PathBuf,
    out: BufWriter<std::fs::File>,
    /// Bytes written to the current segment so far.
    bytes: u64,
    /// Events written to the current segment so far.
    events: u64,
    max_bytes: u64,
    max_events: u64,
    sandbox_max_bytes: u64,
}

impl SegmentSink {
    fn segment_name(seg_no: u32) -> String {
        format!("events-{seg_no:04}.jsonl")
    }

    /// Open the first segment (`events-0001.jsonl`) under `session_dir`.
    fn open(
        session_dir: PathBuf,
        sandbox_dir: PathBuf,
        max_bytes: u64,
        max_events: u64,
        sandbox_max_bytes: u64,
    ) -> std::io::Result<Self> {
        let seg_no = 1;
        let path = session_dir.join(Self::segment_name(seg_no));
        let out = BufWriter::new(open_segment(&path)?);
        Ok(SegmentSink {
            session_dir,
            sandbox_dir,
            seg_no,
            path,
            out,
            bytes: 0,
            events: 0,
            max_bytes,
            max_events,
            sandbox_max_bytes,
        })
    }

    /// Append one already-serialized, newline-terminated row, then roll if the segment is now
    /// at or past a cap. A row may push the segment slightly past the byte cap before the roll;
    /// that bounded overshoot keeps each row whole.
    fn write_line(&mut self, line: &[u8]) {
        if self.out.write_all(line).is_ok() {
            self.bytes += line.len() as u64;
            self.events += 1;
        }
        if self.bytes >= self.max_bytes || self.events >= self.max_events {
            self.rotate();
        }
    }

    /// Finalize the current segment, open the next one, then enforce the size ceiling. Best
    /// effort: if the next segment cannot be opened we keep writing to the current one rather
    /// than lose the writer.
    fn rotate(&mut self) {
        let _ = self.out.flush();
        if let Ok(file) = self.out.get_ref().try_clone() {
            let _ = file.sync_all();
        }
        let next_no = self.seg_no + 1;
        let next_path = self.session_dir.join(Self::segment_name(next_no));
        match open_segment(&next_path) {
            Ok(file) => {
                self.out = BufWriter::new(file);
                self.seg_no = next_no;
                self.path = next_path;
                self.bytes = 0;
                self.events = 0;
                self.enforce_ceiling();
            }
            Err(_) => {
                // Couldn't roll: stay on the current segment (it has been flushed) and keep
                // accepting rows. The byte/event counters keep climbing, so we retry the roll
                // on the next write — never wedging the writer.
            }
        }
    }

    /// Sum the sandbox's audit-dir size and unlink oldest segments until under the ceiling.
    /// The currently-active segment is never removed, so a still-running session's writer is
    /// undisturbed even as its older segments (and older sessions' segments) are reaped.
    fn enforce_ceiling(&mut self) {
        if self.sandbox_max_bytes == u64::MAX {
            return;
        }
        // Oldest-first across the whole sandbox: session dir names are timestamp-led and
        // segment names are zero-padded, so a lexical path sort is an age sort.
        let mut segments = collect_segments(&self.sandbox_dir);
        segments.sort_by(|a, b| a.0.cmp(&b.0));
        let mut total: u64 = segments.iter().map(|(_, size)| size).sum();
        for (path, size) in segments {
            if total <= self.sandbox_max_bytes {
                break;
            }
            if path == self.path {
                continue; // never unlink the segment we're actively writing
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }

    /// Flush and fsync the current segment on shutdown.
    fn finalize(mut self) {
        let _ = self.out.flush();
        if let Ok(file) = self.out.into_inner() {
            let _ = file.sync_all();
        }
    }
}

/// Open a segment file for append-only writing, creating it if absent.
fn open_segment(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// All `events-NNNN.jsonl` segments under a sandbox dir (across every session), each paired
/// with its byte size. Returns `(path, size)` pairs; missing/unreadable dirs yield nothing.
fn collect_segments(sandbox_dir: &std::path::Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let Ok(sessions) = std::fs::read_dir(sandbox_dir) else {
        return out;
    };
    for session in sessions.filter_map(|e| e.ok()) {
        let Ok(segments) = std::fs::read_dir(session.path()) else {
            continue;
        };
        for seg in segments.filter_map(|e| e.ok()) {
            let path = seg.path();
            let is_segment = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("events-") && n.ends_with(".jsonl"));
            if !is_segment {
                continue;
            }
            let size = seg.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((path, size));
        }
    }
    out
}

/// Constructs writers. Stateless; the entry point is [`AuditWriter::spawn`].
pub struct AuditWriter;

impl AuditWriter {
    /// Create the session directory, open the first segment, and spawn the writer thread.
    /// Returns a handle whose [`AuditHandle::sender`] feeds the proxy and whose drop drains
    /// the channel and flushes.
    pub fn spawn(config: WriterConfig) -> std::io::Result<AuditHandle> {
        let sandbox_dir = config.audit_dir.join(&config.sandbox);
        let session_dir = sandbox_dir.join(&config.session);
        std::fs::create_dir_all(&session_dir)?;
        let sink = SegmentSink::open(
            session_dir,
            sandbox_dir,
            config.segment_max_bytes,
            config.segment_max_events,
            config.sandbox_max_bytes,
        )?;

        let (tx, rx) = mpsc::channel::<AuditEvent>(config.channel_capacity);
        let shutdown = Arc::new(Notify::new());
        let drops = Arc::new(AtomicU64::new(0));

        let sandbox = config.sandbox.clone();
        let session = config.session.clone();
        let flush_every = config.flush_every;
        let flush_interval = config.flush_interval;
        let task_shutdown = shutdown.clone();
        let task_drops = drops.clone();

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
                    task_drops,
                    sink,
                    sandbox,
                    session,
                    flush_every,
                    flush_interval,
                ));
            })?;

        Ok(AuditHandle {
            tx,
            drops,
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
    /// Shared drop counter handed to every [`AuditSink`]; the writer task reads it.
    drops: Arc<AtomicU64>,
    shutdown: Arc<Notify>,
    thread: Option<JoinHandle<()>>,
}

impl AuditHandle {
    /// A cloneable, fail-open sink for the proxy. The proxy `try_send`s into it; a full
    /// channel drops the event and bumps the shared counter the writer turns into a `dropped`
    /// marker.
    pub fn sink(&self) -> AuditSink {
        AuditSink {
            tx: self.tx.clone(),
            drops: self.drops.clone(),
        }
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
// The writer's inputs are a flat list of independent channels/handles/tuning knobs; bundling
// them into a struct would add a type without making any callsite clearer.
#[allow(clippy::too_many_arguments)]
async fn run_writer(
    mut rx: mpsc::Receiver<AuditEvent>,
    shutdown: Arc<Notify>,
    drops: Arc<AtomicU64>,
    mut sink: SegmentSink,
    sandbox: String,
    session: String,
    flush_every: usize,
    flush_interval: Duration,
) {
    let mut interval = tokio::time::interval(flush_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut since_flush = 0usize;
    // Drops already materialized into markers; the next marker's `count` is the delta from
    // here to the live atomic, so the markers partition the drop stream with no double-counting.
    let mut recorded_drops = 0u64;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => {
                // Drain everything already queued, then label any final gap, flush+fsync, exit.
                while let Ok(event) = rx.try_recv() {
                    write_row(&mut sink, &event, &sandbox, &session);
                }
                record_drops(&mut sink, &drops, &mut recorded_drops, &sandbox, &session);
                sink.finalize();
                return;
            }
            event = rx.recv() => {
                match event {
                    Some(event) => {
                        // The channel just freed a slot, so any drop happened *before* this
                        // event regained admission: emit the gap marker ahead of the row it
                        // precedes, so the log reads "<N dropped> … resumed-event".
                        record_drops(&mut sink, &drops, &mut recorded_drops, &sandbox, &session);
                        write_row(&mut sink, &event, &sandbox, &session);
                        since_flush += 1;
                        if since_flush >= flush_every {
                            sink.flush();
                            since_flush = 0;
                        }
                    }
                    // All senders dropped: no more events will ever arrive.
                    None => {
                        record_drops(&mut sink, &drops, &mut recorded_drops, &sandbox, &session);
                        sink.finalize();
                        return;
                    }
                }
            }
            _ = interval.tick() => {
                // Catch a trailing drop burst that overload left behind when the senders fell
                // quiet without dropping the handle — the recv arm above would otherwise block.
                record_drops(&mut sink, &drops, &mut recorded_drops, &sandbox, &session);
                if since_flush > 0 {
                    sink.flush();
                    since_flush = 0;
                }
            }
        }
    }
}

/// Materialize a `dropped` gap marker directly to the file if the live drop counter has moved
/// past what we have already recorded. The marker's `count` is exactly the number lost since
/// the previous marker, so concatenating every marker's count reconstructs the total drops
/// with no gap and no overlap. Bypasses the channel by construction (it is the channel that
/// overflowed). A no-op when no new drops have accrued, so a healthy run is never polluted.
fn record_drops(
    sink: &mut SegmentSink,
    drops: &AtomicU64,
    recorded: &mut u64,
    sandbox: &str,
    session: &str,
) {
    let total = drops.load(Ordering::Relaxed);
    if total > *recorded {
        let count = total - *recorded;
        *recorded = total;
        write_row(
            sink,
            &AuditEvent::Dropped {
                count,
                ts_ms: now_ms(),
            },
            sandbox,
            session,
        );
    }
}

/// Current wall-clock time in milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Serialize one event into a self-describing JSONL row, stamping `{sandbox, session}`, and
/// append it through the sink (which handles rotation + ceiling). Serialization cannot fail
/// for our event types, so a failure here is dropped rather than disturbing the writer.
fn write_row(sink: &mut SegmentSink, event: &AuditEvent, sandbox: &str, session: &str) {
    let mut value = match serde_json::to_value(event) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => return,
    };
    value.insert("sandbox".to_string(), json!(sandbox));
    value.insert("session".to_string(), json!(session));
    let mut line = serde_json::Value::Object(value).to_string();
    line.push('\n');
    sink.write_line(line.as_bytes());
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

    /// The `events-NNNN.jsonl` segment files currently present in a session dir, sorted by
    /// name (which, being zero-padded, is oldest-first).
    fn segments(audit_dir: &std::path::Path, sandbox: &str, session: &str) -> Vec<PathBuf> {
        let dir = audit_dir.join(sandbox).join(session);
        let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("events-") && n.ends_with(".jsonl"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    /// Push a `conn_open` event whose `dst` is padded so each serialized row is comfortably
    /// large, making byte-cap rotation easy to drive with a handful of events.
    fn send_padded(tx: &AuditSink, conn_id: u64) {
        tx.try_send(AuditEvent::ConnOpen {
            conn_id,
            dst: "1.2.3.4:443".into(),
            sni: Some("x".repeat(200)),
            conn_kind: ConnKind::Mitm,
            ts_ms: 1_717_000_000_000,
        });
    }

    #[test]
    fn rolls_to_a_new_segment_at_the_byte_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_dir = tmp.path().to_path_buf();
        // Tiny byte cap: each padded row is ~250 bytes, so every couple of events rolls a
        // segment. Generous event/sandbox caps so only the byte cap fires here.
        let mut handle = AuditWriter::spawn(
            WriterConfig::new(&audit_dir, "web", "sess-rot").with_caps(300, 1_000_000, u64::MAX),
        )
        .unwrap();
        let tx = handle.sink();
        for id in 0..6 {
            send_padded(&tx, id);
        }
        handle.shutdown();

        let segs = segments(&audit_dir, "web", "sess-rot");
        assert!(
            segs.len() >= 2,
            "byte cap should have rolled multiple segments, got: {segs:#?}"
        );
        assert_eq!(
            segs[0].file_name().unwrap().to_str().unwrap(),
            "events-0001.jsonl",
            "first segment keeps the 0001 name"
        );
        assert_eq!(
            segs[1].file_name().unwrap().to_str().unwrap(),
            "events-0002.jsonl",
            "rotation increments the segment number"
        );
        // Every event survives across the roll: total rows == events sent.
        let total: usize = segs.iter().map(|p| read_rows(p).len()).sum();
        assert_eq!(total, 6, "no events lost across rotation");
    }

    #[test]
    fn rolls_to_a_new_segment_at_the_event_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_dir = tmp.path().to_path_buf();
        // Cap at 2 events/segment; huge byte/sandbox caps so only the event cap fires.
        let mut handle = AuditWriter::spawn(
            WriterConfig::new(&audit_dir, "web", "sess-evt").with_caps(u64::MAX, 2, u64::MAX),
        )
        .unwrap();
        let tx = handle.sink();
        for id in 0..5 {
            send_padded(&tx, id);
        }
        handle.shutdown();

        let segs = segments(&audit_dir, "web", "sess-evt");
        // 5 events at 2/segment -> events-0001 (2), 0002 (2), 0003 (1).
        assert_eq!(
            segs.len(),
            3,
            "event cap should roll every 2 events: {segs:#?}"
        );
        assert_eq!(read_rows(&segs[0]).len(), 2, "first segment holds the cap");
        assert_eq!(read_rows(&segs[1]).len(), 2, "second segment holds the cap");
        assert_eq!(
            read_rows(&segs[2]).len(),
            1,
            "remainder lands in the last segment"
        );
    }

    /// Pull the `conn_id` out of every row across all of a session's segments, oldest-first.
    fn conn_ids(audit_dir: &std::path::Path, sandbox: &str, session: &str) -> Vec<u64> {
        segments(audit_dir, sandbox, session)
            .iter()
            .flat_map(|p| read_rows(p))
            .map(|r| r["conn_id"].as_u64().unwrap())
            .collect()
    }

    #[test]
    fn trims_oldest_segments_once_over_the_sandbox_ceiling() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_dir = tmp.path().to_path_buf();
        // Roll on (almost) every row, and a ceiling that fits only a few segments. Sending far
        // more events than the ceiling holds forces oldest segments to be unlinked while the
        // writer keeps running.
        let mut handle = AuditWriter::spawn(
            WriterConfig::new(&audit_dir, "web", "sess-trim").with_caps(200, u64::MAX, 900),
        )
        .unwrap();
        let tx = handle.sink();
        for id in 0..20 {
            send_padded(&tx, id);
        }
        handle.shutdown();

        let segs = segments(&audit_dir, "web", "sess-trim");
        // The ceiling bounds total on-disk size: surviving segments stay within the ceiling
        // plus at most the active (post-trim) segment's growth — never the full 20-event run.
        let total_bytes: u64 = segs
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().len())
            .sum();
        assert!(
            total_bytes <= 900 + 400,
            "size ceiling must bound the sandbox dir; got {total_bytes} bytes over {segs:#?}"
        );

        // The oldest segment was unlinked: events-0001 is gone, the run did not just keep
        // appending unboundedly.
        assert!(
            !audit_dir
                .join("web")
                .join("sess-trim")
                .join("events-0001.jsonl")
                .exists(),
            "the oldest segment should have been trimmed"
        );

        // The active writer was never disrupted: the most recent events survive, and the rows
        // that remain are the newest contiguous suffix (oldest were dropped, not a hole).
        let ids = conn_ids(&audit_dir, "web", "sess-trim");
        assert!(!ids.is_empty(), "recent events must survive the trim");
        assert_eq!(*ids.last().unwrap(), 19, "the latest event must be present");
        assert!(
            ids[0] > 0,
            "the earliest events were trimmed, not the latest"
        );
        let contiguous = ids.windows(2).all(|w| w[1] == w[0] + 1);
        assert!(
            contiguous,
            "survivors are a contiguous newest suffix: {ids:?}"
        );
    }

    #[test]
    fn writes_self_describing_jsonl_under_sandbox_session_path() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_dir = tmp.path().to_path_buf();
        let mut handle =
            AuditWriter::spawn(WriterConfig::new(&audit_dir, "web", "sess-1")).unwrap();

        handle.sink().try_send(AuditEvent::ConnOpen {
            conn_id: 7,
            dst: "1.2.3.4:443".into(),
            sni: Some("api.github.com".into()),
            conn_kind: ConnKind::Mitm,
            ts_ms: 1_717_000_000_000,
        });
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
        let tx = handle.sink();

        tx.try_send(AuditEvent::ConnOpen {
            conn_id: 1,
            dst: "10.0.0.9:80".into(),
            sni: None,
            conn_kind: ConnKind::PlainTcp,
            ts_ms: 1_000,
        });
        tx.try_send(AuditEvent::ConnClose {
            conn_id: 1,
            bytes_tx: 42,
            bytes_rx: 1024,
            duration_ms: 5,
            ts_ms: 1_005,
        });
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
            handle.sink().try_send(AuditEvent::ConnOpen {
                conn_id: 99,
                dst: "8.8.8.8:443".into(),
                sni: Some("dns.google".into()),
                conn_kind: ConnKind::BlindTunnel,
                ts_ms: 2_000,
            });
        } // handle dropped here

        let rows = read_rows(&segment(&audit_dir, "eph", "sess-3"));
        assert_eq!(rows.len(), 1, "Drop-guard must flush the tail event");
        assert_eq!(rows[0]["conn_kind"], "blind_tunnel");
        assert_eq!(rows[0]["conn_id"], 99);
    }

    /// Every row across all of a session's segments, oldest-first.
    fn all_rows(audit_dir: &std::path::Path, sandbox: &str, session: &str) -> Vec<serde_json::Value> {
        segments(audit_dir, sandbox, session)
            .iter()
            .flat_map(|p| read_rows(p))
            .collect()
    }

    #[test]
    fn saturating_the_channel_records_dropped_gap_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_dir = tmp.path().to_path_buf();
        // Depth-1 channel: a tight burst of try_sends outpaces the writer's per-event disk
        // write, so the channel goes Full and the sink counts the drops. Generous rotation caps
        // so only the channel saturates here, never a segment roll.
        let mut handle = AuditWriter::spawn(
            WriterConfig::new(&audit_dir, "web", "sess-drop").with_channel_capacity(1),
        )
        .unwrap();
        let sink = handle.sink();
        for id in 0..5000 {
            sink.try_send(AuditEvent::ConnOpen {
                conn_id: id,
                dst: "1.2.3.4:443".into(),
                sni: Some("x".repeat(64)),
                conn_kind: ConnKind::Mitm,
                ts_ms: 1_717_000_000_000,
            });
        }
        // All sends are done: the counter is now final and the test reads a stable value.
        let dropped_total = sink.dropped();
        assert!(
            dropped_total > 0,
            "a tight burst into a depth-1 channel must drop events; dropped {dropped_total}"
        );
        handle.shutdown();

        let rows = all_rows(&audit_dir, "web", "sess-drop");
        let markers: Vec<&serde_json::Value> =
            rows.iter().filter(|r| r["kind"] == "dropped").collect();
        assert!(
            !markers.is_empty(),
            "a saturated channel must produce at least one dropped marker"
        );
        // Gaps are labeled exactly once: the markers' counts sum to every dropped event, with
        // no double-counting and none missed.
        let recorded: u64 = markers.iter().map(|r| r["count"].as_u64().unwrap()).sum();
        assert_eq!(
            recorded, dropped_total,
            "dropped markers must account for every dropped event exactly once"
        );
        // Markers are self-describing like every other row, and carry a positive count.
        assert!(
            markers
                .iter()
                .all(|r| r["sandbox"] == "web" && r["session"] == "sess-drop"),
            "dropped markers are stamped with identity"
        );
        assert!(
            markers.iter().all(|r| r["count"].as_u64().unwrap() > 0),
            "a dropped marker is only written for a real gap"
        );
        // Conservation: the rows that survived plus the labeled drops account for every event
        // the sink accepted — the log is complete or it tells you where it is not.
        let survived = rows.iter().filter(|r| r["kind"] == "conn_open").count() as u64;
        assert_eq!(
            survived + dropped_total,
            5000,
            "every event is either persisted or labeled as dropped"
        );
    }

    #[test]
    fn a_healthy_run_writes_no_dropped_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_dir = tmp.path().to_path_buf();
        // Default (generous) channel depth and a trickle of events: nothing is ever dropped, so
        // no marker must pollute a healthy log.
        let mut handle =
            AuditWriter::spawn(WriterConfig::new(&audit_dir, "web", "sess-clean")).unwrap();
        let sink = handle.sink();
        for id in 0..10 {
            sink.try_send(AuditEvent::ConnOpen {
                conn_id: id,
                dst: "1.2.3.4:443".into(),
                sni: None,
                conn_kind: ConnKind::PlainTcp,
                ts_ms: 1_717_000_000_000,
            });
        }
        assert_eq!(sink.dropped(), 0, "a depth-4096 channel never drops a trickle");
        handle.shutdown();

        let rows = all_rows(&audit_dir, "web", "sess-clean");
        assert_eq!(rows.len(), 10, "all events persisted");
        assert!(
            rows.iter().all(|r| r["kind"] != "dropped"),
            "a healthy run must contain no dropped markers; rows: {rows:#?}"
        );
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
