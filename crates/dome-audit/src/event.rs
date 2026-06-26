//! The lean, identity-free event model the proxy emits.
//!
//! The proxy knows nothing about which sandbox/session it serves — it emits these
//! bare events through a channel and the [`crate::writer`] stamps `{sandbox, session}`
//! onto each row as it serializes, so every persisted record is self-describing.

use serde::Serialize;

/// How the proxy handled a connection. Determines how rich the rows can be: only
/// `Mitm` connections are decrypted, so later slices attach request/response rows
/// to them; everything else is metadata-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnKind {
    /// TLS to a secret-bound host: the proxy terminates TLS on both sides.
    Mitm,
    /// TLS to a host with no matching secret: forwarded opaquely, never decrypted.
    BlindTunnel,
    /// Non-TLS TCP: forwarded opaquely.
    PlainTcp,
    /// A connection to an exposed host port (`host.dome.internal`), relayed to localhost.
    ExposeHost,
}

/// A single audit event. Flat and append-only: the connection→close relationship is
/// reconstructed at read time by grouping on `conn_id` within a `{sandbox, session}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditEvent {
    /// A new egress connection was opened. Carries the destination and, where known
    /// (TLS), the SNI domain.
    ConnOpen {
        /// Per-session connection id, unambiguous within `{sandbox, session}`.
        conn_id: u64,
        /// Destination socket address (`ip:port`).
        dst: String,
        /// SNI / domain when known (TLS connections); `None` for plain TCP.
        #[serde(skip_serializing_if = "Option::is_none")]
        sni: Option<String>,
        /// How the proxy handled the connection.
        conn_kind: ConnKind,
        /// Wall-clock open time, milliseconds since the Unix epoch.
        ts_ms: u64,
    },
    /// A connection closed. Carries byte counts and how long it was open.
    ConnClose {
        conn_id: u64,
        /// Bytes sent guest → upstream.
        bytes_tx: u64,
        /// Bytes received upstream → guest.
        bytes_rx: u64,
        /// How long the connection was open, in milliseconds.
        duration_ms: u64,
        /// Wall-clock close time, milliseconds since the Unix epoch.
        ts_ms: u64,
    },
    /// One HTTP request seen on a MITM connection. Emitted the moment the request
    /// head is fully parsed (before the body is consumed), so it suits a live tail.
    /// Carries the request line and sizes only — no headers (deferred to the
    /// redaction slice). The bytes are captured guest-side, pre-substitution, so the
    /// real secret value can never appear here.
    HttpRequest {
        conn_id: u64,
        /// Request method, e.g. `GET`, `POST`.
        method: String,
        /// Request target (path + query) exactly as sent.
        path: String,
        /// HTTP minor version (`1` for HTTP/1.1, `0` for HTTP/1.0).
        http_minor: u8,
        /// Bytes of the request head (request line + headers + terminating CRLFCRLF).
        head_bytes: u64,
        /// Declared body length from `Content-Length`; `None` when the body is
        /// `chunked` or otherwise indeterminate at head-parse time.
        #[serde(skip_serializing_if = "Option::is_none")]
        body_bytes: Option<u64>,
        /// Wall-clock time the head was parsed, milliseconds since the Unix epoch.
        ts_ms: u64,
    },
    /// One HTTP response seen on a MITM connection. Emitted when the status line +
    /// headers are parsed. Pairs with the [`AuditEvent::HttpRequest`] of the same
    /// `conn_id` by order (HTTP/1.1 preserves request/response ordering).
    HttpResponse {
        conn_id: u64,
        /// Status code, e.g. `200`, `404`.
        status: u16,
        /// Reason phrase, e.g. `OK`; empty when absent.
        #[serde(skip_serializing_if = "String::is_empty")]
        reason: String,
        /// HTTP minor version.
        http_minor: u8,
        /// Bytes of the response head (status line + headers + terminating CRLFCRLF).
        head_bytes: u64,
        /// Declared body length from `Content-Length`; `None` when `chunked` or
        /// read-until-close.
        #[serde(skip_serializing_if = "Option::is_none")]
        body_bytes: Option<u64>,
        ts_ms: u64,
    },
    /// The framer hit something it cannot parse on a MITM connection (an unexpected
    /// encoding, a protocol upgrade, HTTP/2 over the tunnel, or a desync) and has
    /// stopped framing that connection. Emitted once per affected direction; the
    /// connection's traffic is never affected — the log degrades, not the network.
    Unparsed {
        conn_id: u64,
        /// Which side desynced: `"request"` or `"response"`.
        direction: &'static str,
        /// Short, non-sensitive reason, e.g. `"chunked size"` or `"http2 preface"`.
        reason: &'static str,
        ts_ms: u64,
    },
}
