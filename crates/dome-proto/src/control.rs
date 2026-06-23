//! Control-plane protocol for the `domed` supervisor.
//!
//! Newline-delimited JSON over a local, user-private unix domain socket. Every request
//! carries a [`PROTOCOL_VERSION`] so the CLI and a future native (Swift/Electron) UI
//! can detect a wire mismatch early instead of misparsing. Commands are
//! request/response; a [`Command::Subscribe`] request additionally turns the connection
//! into an async event stream. Clients ignore [`Event`] types they do not recognise, so
//! adding a new event never breaks an older client.
//!
//! These types live in `dome-proto` (not the CLI) precisely so the future UI can depend
//! on the same definitions the CLI speaks.

use serde::{Deserialize, Serialize};

/// Wire-format version. Bump on any breaking change to the request/response/event
/// shapes. domed rejects a request whose `protocol_version` it does not understand.
pub const PROTOCOL_VERSION: u32 = 1;

/// A request from a client (CLI or UI) to domed — one JSON object per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Wire-format version the client speaks.
    pub protocol_version: u32,
    /// Optional correlation id echoed back in the matching [`Response`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// The command to run (flattened, so the `verb` tag sits at the top level).
    #[serde(flatten)]
    pub command: Command,
}

impl Request {
    /// Build a request at the current [`PROTOCOL_VERSION`].
    pub fn new(id: Option<u64>, command: Command) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            id,
            command,
        }
    }
}

/// A control command. The `verb` field tags the variant on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum Command {
    /// Report daemon health: pid, uptime, worker count, socket path, protocol version.
    Status,
    /// List every sandbox (on-disk plus any live workers) with state, attached-terminal
    /// count, size, pinned base, and age.
    List,
    /// Turn this connection into an event subscriber. domed acknowledges with a
    /// [`Response`], then streams [`Event`]s on the same connection until it closes.
    Subscribe,
    /// Ask domed to shut down. Running workers are NOT affected — they outlive domed and
    /// are re-adopted on the next start.
    Shutdown,
}

/// domed's reply to a [`Request`] — one JSON object per line. Distinguished from an
/// [`Event`] on the wire by the presence of the `ok` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Correlation id copied from the request, when it carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// Whether the command succeeded.
    pub ok: bool,
    /// Command-specific payload on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Human-readable message on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    /// A successful response carrying `result`.
    pub fn ok(id: Option<u64>, result: serde_json::Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// A failed response carrying an error message.
    pub fn err(id: Option<u64>, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
}

/// An asynchronous event pushed to subscribers. `event` is a dotted name (e.g.
/// `sandbox.started`, `daemon.stopping`); a client ignores names it does not handle.
/// Distinguished from a [`Response`] on the wire by the presence of the `event` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Dotted event name.
    pub event: String,
    /// Optional event-specific payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Event {
    /// Build an event with no payload.
    pub fn bare(event: impl Into<String>) -> Self {
        Self {
            event: event.into(),
            data: None,
        }
    }
}

/// Result payload for [`Command::Status`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResult {
    /// Protocol version domed speaks.
    pub protocol_version: u32,
    /// domed's process id.
    pub pid: u32,
    /// Seconds since domed started.
    pub uptime_secs: u64,
    /// Number of live workers domed is supervising.
    pub worker_count: usize,
    /// Absolute path of the control socket.
    pub socket_path: String,
}

/// Result payload for [`Command::List`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListResult {
    /// All sandboxes, oldest-first.
    pub sandboxes: Vec<SandboxInfo>,
}

/// One sandbox as reported by `dome sandbox ls`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxInfo {
    /// Sandbox name.
    pub name: String,
    /// CAS delta size in bytes (non-ZERO chunks × chunk size).
    pub size_bytes: u64,
    /// Pinned OS base version (or a best-effort label for a non-standard base).
    pub base: String,
    /// `running` when a live worker owns it, else `idle`.
    pub state: String,
    /// Number of attached terminals (0 unless a live worker has sessions).
    pub attached: usize,
    /// Sandbox index mtime as seconds since the unix epoch (drives the age column).
    pub created_unix: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request round-trips through JSON with the `verb` tag flattened to the top level
    /// (no nested `command` object), so the wire form stays flat for the UI.
    #[test]
    fn request_roundtrips_with_a_flat_verb_tag() {
        let req = Request::new(Some(7), Command::List);
        let line = serde_json::to_string(&req).unwrap();
        assert!(
            line.contains("\"verb\":\"list\""),
            "verb tag must be flattened to the top level: {line}"
        );
        assert!(
            !line.contains("command"),
            "the command must not appear as a nested object: {line}"
        );
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);
    }

    /// Every command variant survives a JSON round-trip.
    #[test]
    fn every_command_variant_roundtrips() {
        for cmd in [
            Command::Status,
            Command::List,
            Command::Subscribe,
            Command::Shutdown,
        ] {
            let req = Request::new(None, cmd.clone());
            let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
            assert_eq!(back.command, cmd);
            assert_eq!(back.protocol_version, PROTOCOL_VERSION);
        }
    }

    /// `id` is omitted from the wire when absent and preserved when present.
    #[test]
    fn request_id_is_optional_on_the_wire() {
        let without = serde_json::to_string(&Request::new(None, Command::Status)).unwrap();
        assert!(!without.contains("\"id\""), "id must be omitted: {without}");
        let with = serde_json::to_string(&Request::new(Some(42), Command::Status)).unwrap();
        assert!(with.contains("\"id\":42"), "id must be present: {with}");
    }

    /// A success response carries `ok:true` and a `result`, no `error`.
    #[test]
    fn ok_response_roundtrips() {
        let resp = Response::ok(Some(1), serde_json::json!({"pid": 99}));
        let line = serde_json::to_string(&resp).unwrap();
        assert!(!line.contains("error"), "ok response must omit error: {line}");
        let back: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(back, resp);
        assert!(back.ok);
    }

    /// An error response carries `ok:false` and an `error`, no `result`.
    #[test]
    fn err_response_roundtrips() {
        let resp = Response::err(None, "boom");
        let line = serde_json::to_string(&resp).unwrap();
        assert!(!line.contains("result"), "err response must omit result: {line}");
        let back: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(back, resp);
        assert!(!back.ok);
        assert_eq!(back.error.as_deref(), Some("boom"));
    }

    /// Events round-trip, including event names the current client does not know about —
    /// proving an older client can parse (and then ignore) a future event type.
    #[test]
    fn events_roundtrip_including_unknown_names() {
        let known = Event::bare("daemon.stopping");
        let unknown = Event {
            event: "sandbox.some_future_thing".to_string(),
            data: Some(serde_json::json!({"x": 1})),
        };
        for ev in [known, unknown] {
            let back: Event = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
            assert_eq!(back, ev);
        }
    }

    /// Status and list result payloads round-trip.
    #[test]
    fn result_payloads_roundtrip() {
        let status = StatusResult {
            protocol_version: PROTOCOL_VERSION,
            pid: 1234,
            uptime_secs: 5,
            worker_count: 0,
            socket_path: "/tmp/domed.sock".to_string(),
        };
        let back: StatusResult =
            serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
        assert_eq!(back, status);

        let list = ListResult {
            sandboxes: vec![SandboxInfo {
                name: "web".to_string(),
                size_bytes: 65536,
                base: "1.2.3".to_string(),
                state: "idle".to_string(),
                attached: 0,
                created_unix: 1_700_000_000,
            }],
        };
        let back: ListResult =
            serde_json::from_str(&serde_json::to_string(&list).unwrap()).unwrap();
        assert_eq!(back, list);
    }
}
