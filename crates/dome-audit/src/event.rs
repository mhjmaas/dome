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
}
