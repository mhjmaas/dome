use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use boring::ssl::{SslConnector, SslMethod};
use dome_audit::{
    AuditEvent, AuditSink, BlockReason, ConnKind, Direction, HttpFramer, PlaceholderNames,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::config::ProxyConfig;
use crate::dns;
use crate::stack::{ConnectionId, StackCommand, StackEvent, TcpConnection};
use crate::stream::ChannelStream;
use crate::tls::CertificateAuthority;
use crate::AllowedIps;

/// The async proxy engine.
///
/// Receives events from the smoltcp NetworkStack and proxies TCP connections
/// to the real internet, with optional MITM for secret injection.
///
/// Uses BoringSSL (Chrome's TLS stack) for upstream connections so that
/// Cloudflare-protected sites accept the TLS fingerprint. The client-side
/// (guest <-> proxy) uses rustls with our generated CA cert.
pub struct ProxyEngine {
    config: Arc<ProxyConfig>,
    event_rx: mpsc::UnboundedReceiver<StackEvent>,
    cmd_tx: mpsc::UnboundedSender<StackCommand>,
    connections: HashMap<ConnectionId, mpsc::UnboundedSender<Vec<u8>>>,
    placeholders: Arc<HashMap<String, String>>,
    /// Inverse of `placeholders` (`placeholder → secret-name`), handed to the audit framer so
    /// it can attribute a placeholder seen in a sensitive header to the secret it stands for.
    placeholder_names: Arc<PlaceholderNames>,
    ca: Arc<tokio::sync::Mutex<CertificateAuthority>>,
    upstream_ssl: SslConnector,
    allowed_ips: AllowedIps,
    dns_cache: crate::dns::SharedDnsCache,
    audit_sink: Option<AuditSink>,
    /// Monotonic per-session connection id, assigned as each connection opens. Stable and
    /// unambiguous within a `{sandbox, session}`.
    next_conn_id: u64,
}

impl ProxyEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: ProxyConfig,
        event_rx: mpsc::UnboundedReceiver<StackEvent>,
        cmd_tx: mpsc::UnboundedSender<StackCommand>,
        ca: CertificateAuthority,
        placeholders: HashMap<String, String>,
        allowed_ips: AllowedIps,
        dns_cache: crate::dns::SharedDnsCache,
        audit_sink: Option<AuditSink>,
    ) -> Self {
        // BoringSSL upstream connector — Chrome's TLS stack so Cloudflare
        // doesn't reject our MITM connections based on JA3/JA4 fingerprint.
        let mut builder = SslConnector::builder(SslMethod::tls()).expect("SslConnector");
        builder.set_alpn_protos(b"\x08http/1.1").expect("ALPN");
        let upstream_ssl = builder.build();

        // Invert `name → placeholder` into `placeholder → name` once, up front, so the framer
        // can name the secret behind a placeholder it sees in a sensitive header.
        let placeholder_names: PlaceholderNames = placeholders
            .iter()
            .map(|(name, placeholder)| (placeholder.clone(), name.clone()))
            .collect();

        ProxyEngine {
            config: Arc::new(config),
            event_rx,
            cmd_tx,
            connections: HashMap::new(),
            placeholders: Arc::new(placeholders),
            placeholder_names: Arc::new(placeholder_names),
            ca: Arc::new(tokio::sync::Mutex::new(ca)),
            upstream_ssl,
            allowed_ips,
            dns_cache,
            audit_sink,
            next_conn_id: 0,
        }
    }

    /// Run the proxy event loop.
    pub async fn run(&mut self) {
        info!("proxy engine started");
        while let Some(event) = self.event_rx.recv().await {
            match event {
                StackEvent::NewConnection(conn) => {
                    self.handle_new_connection(conn);
                }
                StackEvent::Data { id, payload } => {
                    if let Some(tx) = self.connections.get(&id) {
                        if tx.send(payload).is_err() {
                            self.connections.remove(&id);
                        }
                    }
                }
                StackEvent::Closed { id } => {
                    self.connections.remove(&id);
                }
                StackEvent::DnsQuery { src, payload } => {
                    let cmd_tx = self.cmd_tx.clone();
                    let config = self.config.clone();
                    let allowed_ips = self.allowed_ips.clone();
                    let dns_cache = self.dns_cache.clone();
                    let audit_sink = self.audit_sink.clone();
                    tokio::spawn(async move {
                        dns::handle_dns_query(
                            src,
                            payload,
                            cmd_tx,
                            &config,
                            &allowed_ips,
                            &dns_cache,
                            audit_sink.as_ref(),
                        )
                        .await;
                    });
                }
            }
        }
    }

    fn handle_new_connection(&mut self, conn: TcpConnection) {
        let (data_tx, data_rx) = mpsc::unbounded_channel();
        self.connections.insert(conn.id, data_tx);

        let cmd_tx = self.cmd_tx.clone();
        let config = self.config.clone();
        let ca = self.ca.clone();
        let placeholders = self.placeholders.clone();
        let placeholder_names = self.placeholder_names.clone();
        let upstream_ssl = self.upstream_ssl.clone();
        let allowed_ips = self.allowed_ips.clone();

        let conn_id = self.next_conn_id;
        self.next_conn_id += 1;
        let audit = ConnAudit::new(self.audit_sink.clone(), conn_id);

        tokio::spawn(async move {
            if let Err(e) = handle_connection(
                conn.id,
                conn.dst,
                data_rx,
                cmd_tx,
                &config,
                ca,
                &placeholders,
                placeholder_names,
                upstream_ssl,
                &allowed_ips,
                audit,
            )
            .await
            {
                debug!("connection to {} ended: {e}", conn.dst);
            }
        });
    }
}

/// Current wall-clock time in milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The terminal outcome of one connection, tracked so the drop-guard knows whether a
/// `conn_close` is owed and so a second terminal transition is caught. Exactly one terminal
/// event is emitted per connection: it either `Opened` (and later closes) or was `Blocked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    /// No terminal event emitted yet — the path/policy decision is still pending.
    Pending,
    /// `conn_open` was emitted (allowed and established); a paired `conn_close` is owed on drop.
    Opened,
    /// `conn_blocked` was emitted (policy denial); terminal, no close to pair.
    Blocked,
}

/// Observe-and-emit audit helper for one connection. Tracks the open instant and byte
/// counters, and emits `conn_open`/`conn_close`/`conn_blocked` events fail-open. When the
/// proxy was started without an audit sink (`audit_tx == None`) every method is a no-op, so
/// the network paths carry it unconditionally without branching.
struct ConnAudit {
    tx: Option<AuditSink>,
    conn_id: u64,
    started: Instant,
    /// Bytes guest → upstream. Shared so the relay's directional tasks can increment it.
    bytes_tx: Arc<AtomicU64>,
    /// Bytes upstream → guest.
    bytes_rx: Arc<AtomicU64>,
    /// Which terminal event (if any) has been emitted. Enforces exactly-one-terminal.
    state: ConnState,
}

impl ConnAudit {
    fn new(tx: Option<AuditSink>, conn_id: u64) -> Self {
        ConnAudit {
            tx,
            conn_id,
            started: Instant::now(),
            bytes_tx: Arc::new(AtomicU64::new(0)),
            bytes_rx: Arc::new(AtomicU64::new(0)),
            state: ConnState::Pending,
        }
    }

    /// Emit `conn_open` once the path/kind (and SNI, where known) is decided. `conn_open`
    /// keeps meaning "allowed and established"; a blocked attempt never reaches here.
    fn open(&mut self, kind: ConnKind, dst: SocketAddr, sni: Option<&str>) {
        debug_assert_eq!(
            self.state,
            ConnState::Pending,
            "conn {} terminal transition twice (already {:?})",
            self.conn_id,
            self.state
        );
        self.state = ConnState::Opened;
        let Some(tx) = &self.tx else { return };
        tx.try_send(AuditEvent::ConnOpen {
            conn_id: self.conn_id,
            dst: dst.to_string(),
            sni: sni.map(str::to_string),
            conn_kind: kind,
            ts_ms: now_ms(),
        });
    }

    /// Emit `conn_blocked` for a connection-layer policy denial. Terminal and mutually
    /// exclusive with `open`: a blocked connection produces this row and no `conn_open`/
    /// `conn_close`. Rides the same fail-open path; one row per blocked attempt, retries
    /// included (no dedup). A hostile guest controls denial volume, so a flood could spill
    /// legitimate rows into a visible `dropped` gap — an accepted, revisit-if-observed
    /// trade-off. `sni` is populated only for [`BlockReason::SniNotAllowed`].
    fn blocked(&mut self, reason: BlockReason, dst: SocketAddr, sni: Option<&str>) {
        debug_assert_eq!(
            self.state,
            ConnState::Pending,
            "conn {} terminal transition twice (already {:?})",
            self.conn_id,
            self.state
        );
        self.state = ConnState::Blocked;
        let Some(tx) = &self.tx else { return };
        tx.try_send(AuditEvent::ConnBlocked {
            conn_id: self.conn_id,
            dst: dst.to_string(),
            sni: sni.map(str::to_string),
            reason,
            ts_ms: now_ms(),
        });
    }
}

/// Emit `conn_close` on drop with the observed byte counts and duration. Driving the close
/// from `Drop` (rather than an explicit call on each path) guarantees every `conn_open` is
/// paired with a `conn_close` on *all* exit paths — including an early `?` return when the
/// upstream `connect` fails after the open was emitted. Skipped unless the connection
/// reached `Opened` — a `Pending` connection (closed before a path was chosen) or a
/// `Blocked` one (its terminal row is `conn_blocked`) owes no close, so close rows always
/// pair with an open and opens always pair with a close.
impl Drop for ConnAudit {
    fn drop(&mut self) {
        if self.state != ConnState::Opened {
            return;
        }
        let Some(tx) = &self.tx else { return };
        tx.try_send(AuditEvent::ConnClose {
            conn_id: self.conn_id,
            bytes_tx: self.bytes_tx.load(Ordering::Relaxed),
            bytes_rx: self.bytes_rx.load(Ordering::Relaxed),
            duration_ms: self.started.elapsed().as_millis() as u64,
            ts_ms: now_ms(),
        });
    }
}

/// Handle a single proxied TCP connection.
// The per-connection context (ids, channels, TLS config, policy) is genuinely wide;
// bundling it into a struct would not make the plumbing clearer.
#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    id: ConnectionId,
    dst: SocketAddr,
    mut data_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cmd_tx: mpsc::UnboundedSender<StackCommand>,
    config: &ProxyConfig,
    ca: Arc<tokio::sync::Mutex<CertificateAuthority>>,
    placeholders: &HashMap<String, String>,
    placeholder_names: Arc<PlaceholderNames>,
    upstream_ssl: SslConnector,
    allowed_ips: &AllowedIps,
    mut audit: ConnAudit,
) -> anyhow::Result<()> {
    // Check if this is a connection to an exposed host port (host.dome.internal).
    if let std::net::IpAddr::V4(ipv4) = dst.ip() {
        if let Some(host_port) = config.exposed_host_port(ipv4, dst.port()) {
            debug!(
                "expose-host: guest :{} -> localhost:{}",
                dst.port(),
                host_port
            );
            let local_dst = SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                host_port,
            );
            audit.open(ConnKind::ExposeHost, dst, None);
            let upstream = TcpStream::connect(local_dst).await?;
            let (mut upstream_rd, mut upstream_wr) = upstream.into_split();
            // `audit` drops at function return, emitting the paired `conn_close`.
            return blind_relay(
                id,
                &mut upstream_rd,
                &mut upstream_wr,
                data_rx,
                cmd_tx,
                &audit,
            )
            .await;
        }
    }

    // When a domain allowlist is active, only allow connections to IPs that
    // were resolved via the DNS handler. This prevents bypass via hardcoded IPs.
    // Gateway IP is always allowed so the expose-host fallback path still works;
    // unexposed gateway ports will fail later at connect().
    //
    // Uses write lock + get() so the LRU bumps recency for active IPs, keeping
    // them warm against eviction when the cache is full.
    if config.network.has_allowlist() {
        if let std::net::IpAddr::V4(ipv4) = dst.ip() {
            const GATEWAY: std::net::Ipv4Addr = std::net::Ipv4Addr::new(10, 0, 0, 1);
            let is_allowed = ipv4 == GATEWAY
                || allowed_ips
                    .write()
                    .expect("allowed_ips lock poisoned")
                    .get(&ipv4)
                    .is_some();
            if !is_allowed {
                debug!("IP not in DNS-pinned set, rejecting: {dst}");
                let _ = cmd_tx.send(StackCommand::Close { id });
                // Terminal `conn_blocked`: a literal/hardcoded IP that bypasses the name
                // layer. No SNI to report — the denial is on the address itself.
                audit.blocked(BlockReason::IpNotAllowed, dst, None);
                return Err(anyhow::anyhow!(
                    "connection to {dst} blocked: IP not resolved via allowed DNS"
                ));
            }
        }
    }

    let is_tls = dst.port() == 443;

    if is_tls {
        // Buffer data until we have a complete TLS ClientHello record.
        // The ClientHello may span multiple TCP segments.
        let mut tls_buf = data_rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("connection closed before data"))?;

        // TLS record header: type(1) + version(2) + length(2) = 5 bytes
        // Keep reading until we have the full record
        while tls_buf.len() >= 5 {
            let record_len = u16::from_be_bytes([tls_buf[3], tls_buf[4]]) as usize;
            if tls_buf.len() >= 5 + record_len {
                break; // have the complete record
            }
            match data_rx.recv().await {
                Some(chunk) => tls_buf.extend_from_slice(&chunk),
                None => break, // connection closed
            }
        }

        let sni = extract_sni(&tls_buf);
        debug!("TLS to {dst}, SNI: {sni:?}");

        // When a domain allowlist is active, verify the SNI matches an
        // allowed domain. This prevents connecting to non-allowed services
        // that happen to share an IP with an allowed domain (e.g. CDN/Cloudflare).
        if config.network.has_allowlist() {
            if let Some(reason) = sni_block_reason(sni.as_deref(), |d| config.is_domain_allowed(d))
            {
                // The offending SNI is meaningful only when the name itself was rejected;
                // `no_sni` has none to report.
                let blocked_sni = match reason {
                    BlockReason::SniNotAllowed => sni.as_deref(),
                    _ => None,
                };
                debug!("TLS to {dst} blocked ({reason:?}), SNI: {sni:?}");
                let _ = cmd_tx.send(StackCommand::Close { id });
                audit.blocked(reason, dst, blocked_sni);
                return Err(anyhow::anyhow!("TLS to {dst} blocked: {reason:?}"));
            }
        }

        if let Some(domain) = &sni {
            let substitutions = config.secrets_for_domain(domain, placeholders);
            if !substitutions.is_empty() {
                debug!("MITM: {domain}");
                audit.open(ConnKind::Mitm, dst, Some(domain));
                // `audit` drops at function return, emitting the paired `conn_close`.
                return handle_mitm(
                    id,
                    dst,
                    domain.clone(),
                    tls_buf,
                    data_rx,
                    cmd_tx,
                    ca,
                    substitutions,
                    placeholder_names,
                    upstream_ssl,
                    &audit,
                )
                .await;
            }
        }

        // Blind tunnel: forward the buffered data and relay the rest
        debug!("blind tunnel to {dst}");
        audit.open(ConnKind::BlindTunnel, dst, sni.as_deref());
        let upstream = TcpStream::connect(dst).await?;
        let (mut upstream_rd, mut upstream_wr) = upstream.into_split();

        // Send the buffered TLS data
        upstream_wr.write_all(&tls_buf).await?;
        audit
            .bytes_tx
            .fetch_add(tls_buf.len() as u64, Ordering::Relaxed);

        // `audit` drops at function return, emitting the paired `conn_close`.
        return blind_relay(
            id,
            &mut upstream_rd,
            &mut upstream_wr,
            data_rx,
            cmd_tx,
            &audit,
        )
        .await;
    }

    // Non-TLS: blind tunnel
    debug!("TCP tunnel to {dst}");
    audit.open(ConnKind::PlainTcp, dst, None);
    let upstream = TcpStream::connect(dst).await?;
    let (mut upstream_rd, mut upstream_wr) = upstream.into_split();

    // `audit` drops at function return, emitting the paired `conn_close`.
    blind_relay(
        id,
        &mut upstream_rd,
        &mut upstream_wr,
        data_rx,
        cmd_tx,
        &audit,
    )
    .await
}

/// Blind bidirectional relay (no inspection). Counts bytes in each direction into the
/// audit counters; the actual relayed bytes are never inspected or recorded.
async fn blind_relay(
    id: ConnectionId,
    upstream_rd: &mut tokio::net::tcp::OwnedReadHalf,
    upstream_wr: &mut tokio::net::tcp::OwnedWriteHalf,
    mut data_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cmd_tx: mpsc::UnboundedSender<StackCommand>,
    audit: &ConnAudit,
) -> anyhow::Result<()> {
    let cmd_tx_clone = cmd_tx.clone();
    let bytes_rx = audit.bytes_rx.clone();
    let upstream_to_guest = async {
        let mut buf = vec![0u8; 65536];
        loop {
            match upstream_rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    bytes_rx.fetch_add(n as u64, Ordering::Relaxed);
                    if cmd_tx_clone
                        .send(StackCommand::Send {
                            id,
                            payload: buf[..n].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    };

    let bytes_tx = audit.bytes_tx.clone();
    let guest_to_upstream = async {
        while let Some(payload) = data_rx.recv().await {
            bytes_tx.fetch_add(payload.len() as u64, Ordering::Relaxed);
            if upstream_wr.write_all(&payload).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = upstream_to_guest => {},
        _ = guest_to_upstream => {},
    }

    let _ = cmd_tx.send(StackCommand::Close { id });
    Ok(())
}

/// MITM: terminate TLS on both sides, relay HTTP/1.1 with secret substitution.
// As with `handle_connection`, the per-connection context is inherently wide.
#[allow(clippy::too_many_arguments)]
async fn handle_mitm(
    id: ConnectionId,
    dst: SocketAddr,
    domain: String,
    first_chunk: Vec<u8>,
    data_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cmd_tx: mpsc::UnboundedSender<StackCommand>,
    ca: Arc<tokio::sync::Mutex<CertificateAuthority>>,
    substitutions: Vec<(String, String)>,
    placeholder_names: Arc<PlaceholderNames>,
    upstream_ssl: SslConnector,
    audit: &ConnAudit,
) -> anyhow::Result<()> {
    let acceptor = {
        let mut ca = ca.lock().await;
        ca.acceptor_for_domain(&domain)?
    };

    let mut guest_stream = ChannelStream::new(id, data_rx, cmd_tx.clone());
    guest_stream.prepend(first_chunk);

    // Client-side: rustls with our generated CA cert
    let guest_tls = acceptor.accept(guest_stream).await?;

    // Upstream: BoringSSL — Chrome's TLS fingerprint passes Cloudflare
    let upstream_tcp = TcpStream::connect(dst).await?;
    let upstream_tls = tokio_boring::connect(upstream_ssl.configure()?, &domain, upstream_tcp)
        .await
        .map_err(|e| anyhow::anyhow!("BoringSSL connect to {domain}: {e}"))?;

    // HTTP/1.1 text-based relay with secret substitution
    let (mut guest_rd, mut guest_wr) = tokio::io::split(guest_tls);
    let (mut upstream_rd, mut upstream_wr) = tokio::io::split(upstream_tls);

    // Identity + sink for the per-request framer events, captured before the relay tasks
    // move the byte counters. `None` (auditing disabled) makes the framer work a no-op.
    let conn_id = audit.conn_id;
    let audit_tx = audit.tx.clone();

    // Count the GUEST-side (pre-substitution) byte volume so the audit never reflects the
    // real secret's length. Substitution is left exactly as-is — capture is observe-only.
    let bytes_tx = audit.bytes_tx.clone();
    let req_tx = audit_tx.clone();
    let req_names = placeholder_names.clone();
    let guest_to_upstream = async move {
        // Tee a read-only HTTP framer over the pre-substitution guest bytes: the guest-side
        // view contains only placeholders, so the real secret can never enter the log. The
        // placeholder→name map lets the framer attribute a placeholder in a sensitive header.
        let mut framer = req_tx
            .as_ref()
            .map(|_| HttpFramer::with_names(Direction::Request, req_names));
        let mut buf = vec![0u8; 65536];
        loop {
            match guest_rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    bytes_tx.fetch_add(n as u64, Ordering::Relaxed);
                    if let (Some(tx), Some(framer)) = (&req_tx, framer.as_mut()) {
                        for ev in framer.push(&buf[..n]) {
                            tx.try_send(ev.into_audit(conn_id, Direction::Request, now_ms()));
                        }
                    }
                    let mut data = buf[..n].to_vec();
                    for (placeholder, real_value) in &substitutions {
                        data = replace_bytes(&data, placeholder.as_bytes(), real_value.as_bytes());
                    }
                    if upstream_wr.write_all(&data).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    let bytes_rx = audit.bytes_rx.clone();
    let resp_tx = audit_tx.clone();
    let resp_names = placeholder_names.clone();
    let upstream_to_guest = async move {
        let mut framer = resp_tx
            .as_ref()
            .map(|_| HttpFramer::with_names(Direction::Response, resp_names));
        let mut buf = vec![0u8; 65536];
        loop {
            match upstream_rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    bytes_rx.fetch_add(n as u64, Ordering::Relaxed);
                    if let (Some(tx), Some(framer)) = (&resp_tx, framer.as_mut()) {
                        for ev in framer.push(&buf[..n]) {
                            tx.try_send(ev.into_audit(conn_id, Direction::Response, now_ms()));
                        }
                    }
                    if guest_wr.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    tokio::select! {
        _ = guest_to_upstream => {},
        _ = upstream_to_guest => {},
    }

    let _ = cmd_tx.send(StackCommand::Close { id });
    Ok(())
}

/// Replace all occurrences of `from` with `to` in a byte slice.
fn replace_bytes(data: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() || data.len() < from.len() {
        return data.to_vec();
    }

    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;

    while i <= data.len() - from.len() {
        if &data[i..i + from.len()] == from {
            result.extend_from_slice(to);
            i += from.len();
        } else {
            result.push(data[i]);
            i += 1;
        }
    }

    // Append remaining bytes that can't contain the pattern
    result.extend_from_slice(&data[i..]);
    result
}

/// Map a TLS ClientHello's SNI to the connection-layer policy denial it triggers under an
/// active allowlist, or `None` when the connection is permitted to proceed. Kept a pure
/// function of `(sni, is_allowed)` so the reason mapping is unit-testable without standing
/// up a live TLS handshake. `is_allowed` is the allowlist membership predicate (i.e.
/// `config.is_domain_allowed`).
fn sni_block_reason(sni: Option<&str>, is_allowed: impl Fn(&str) -> bool) -> Option<BlockReason> {
    match sni {
        Some(domain) if !is_allowed(domain) => Some(BlockReason::SniNotAllowed),
        Some(_) => None,
        None => Some(BlockReason::NoSni),
    }
}

/// Extract SNI from a TLS ClientHello.
pub fn extract_sni(data: &[u8]) -> Option<String> {
    if data.len() < 5 || data[0] != 0x16 {
        return None;
    }

    let record_len = u16::from_be_bytes([data[3], data[4]]) as usize;
    if data.len() < 5 + record_len {
        return None;
    }

    let hs = &data[5..];
    if hs.is_empty() || hs[0] != 0x01 {
        return None;
    }

    if hs.len() < 38 {
        return None;
    }
    let mut pos = 38;

    // Session ID
    if pos >= hs.len() {
        return None;
    }
    let session_id_len = hs[pos] as usize;
    pos += 1 + session_id_len;

    // Cipher suites
    if pos + 2 > hs.len() {
        return None;
    }
    let cs_len = u16::from_be_bytes([hs[pos], hs[pos + 1]]) as usize;
    pos += 2 + cs_len;

    // Compression methods
    if pos >= hs.len() {
        return None;
    }
    let cm_len = hs[pos] as usize;
    pos += 1 + cm_len;

    // Extensions
    if pos + 2 > hs.len() {
        return None;
    }
    let ext_len = u16::from_be_bytes([hs[pos], hs[pos + 1]]) as usize;
    pos += 2;
    let ext_end = pos + ext_len;

    while pos + 4 <= ext_end && pos + 4 <= hs.len() {
        let ext_type = u16::from_be_bytes([hs[pos], hs[pos + 1]]);
        let ext_data_len = u16::from_be_bytes([hs[pos + 2], hs[pos + 3]]) as usize;
        pos += 4;

        if ext_type == 0x0000 {
            if ext_data_len >= 5 && pos + ext_data_len <= hs.len() {
                let name_type = hs[pos + 2];
                if name_type == 0x00 {
                    let name_len = u16::from_be_bytes([hs[pos + 3], hs[pos + 4]]) as usize;
                    if pos + 5 + name_len <= hs.len() {
                        return String::from_utf8(hs[pos + 5..pos + 5 + name_len].to_vec()).ok();
                    }
                }
            }
            return None;
        }

        pos += ext_data_len;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "terminal transition")]
    fn conn_audit_double_terminal_transition_trips_debug_assert() {
        // Exactly one terminal event per connection: once a connection has Opened it must
        // never also be Blocked (or vice versa). A no-op sink still drives the state machine,
        // so the second terminal transition trips the debug assertion in debug builds.
        let dst = "10.0.0.5:443".parse().unwrap();
        let mut audit = ConnAudit::new(None, 0);
        audit.open(ConnKind::PlainTcp, dst, None);
        audit.blocked(BlockReason::NoSni, dst, None);
    }

    #[test]
    fn sni_block_reason_maps_policy_decisions() {
        let allow = |d: &str| d == "example.com";
        // An allowlisted SNI is permitted: no denial.
        assert_eq!(sni_block_reason(Some("example.com"), allow), None);
        // A non-allowlisted SNI is the sni_not_allowed denial — the offending name is known.
        assert_eq!(
            sni_block_reason(Some("evil.example.com"), allow),
            Some(BlockReason::SniNotAllowed)
        );
        // No SNI at all cannot be checked against the allowlist: no_sni.
        assert_eq!(sni_block_reason(None, allow), Some(BlockReason::NoSni));
    }

    #[test]
    fn test_extract_sni_none_for_non_tls() {
        assert_eq!(extract_sni(b"GET / HTTP/1.1\r\n"), None);
        assert_eq!(extract_sni(&[]), None);
    }

    #[test]
    fn test_replace_bytes() {
        assert_eq!(
            replace_bytes(b"hello world", b"world", b"rust"),
            b"hello rust"
        );
        assert_eq!(
            replace_bytes(
                b"key=dome_tok_abc123&other=val",
                b"dome_tok_abc123",
                b"real_secret"
            ),
            b"key=real_secret&other=val"
        );
        assert_eq!(replace_bytes(b"no match", b"xyz", b"abc"), b"no match");
        assert_eq!(replace_bytes(b"", b"x", b"y"), b"");
    }
}
