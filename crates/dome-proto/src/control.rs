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
    /// Attach to a sandbox: ensure its worker exists (cold-booting the VM from its last
    /// saved index if it is not already running) and return the worker's data-plane
    /// socket plus a one-time token. domed is NOT in the byte path — the client connects
    /// directly to the worker socket and presents the token. `boot` is an opaque,
    /// client-supplied boot spec used only when a cold boot is required (ignored, with a
    /// warning, when the worker is already running).
    Attach {
        /// Sandbox name to attach to.
        name: String,
        /// Opaque boot spec (serialized by the CLI) used only on cold boot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        boot: Option<serde_json::Value>,
    },
    /// Force a durable flush+save of a running sandbox: domed tells the sandbox's worker
    /// to write+hash its dirty chunks and atomically rewrite the index, so a subsequent
    /// cold boot reflects the latest in-memory state. Errors if the sandbox is not running
    /// (an idle sandbox's on-disk index is already its durable state). Backs
    /// `dome sandbox save <name>`.
    Save {
        /// Sandbox name to save.
        name: String,
    },
    /// Stop a running sandbox: flush+save and shut its VM down. Refuses (naming the count)
    /// when terminals are still attached, unless `force` is set — `force` detaches the
    /// attached terminals gracefully, saves, then stops. Errors if the sandbox is not
    /// running. On success domed emits a `sandbox.stopped` [`Event`]. Backs
    /// `dome sandbox stop [--force] <name>`.
    Stop {
        /// Sandbox name to stop.
        name: String,
        /// Detach attached terminals and stop anyway (default: refuse if any are attached).
        #[serde(default)]
        force: bool,
    },
    /// Worker → domed notification that a save completed (auto-flush interval, dirty-cap
    /// trigger, explicit save, or graceful stop). domed rebroadcasts it to subscribers as
    /// a `sandbox.saved` [`Event`]. Internal to the dome binary — the worker is the only
    /// sender; domed is not in the byte path, so it cannot observe a save otherwise.
    WorkerSaved {
        /// Sandbox that was saved.
        name: String,
    },
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
/// Result payload for [`Command::Attach`]: where and how the client connects to the
/// worker's data plane. The token is single-use — the worker consumes it on the first
/// successful attach so a leaked socket path alone cannot open a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachResult {
    /// Sandbox name attached to.
    pub name: String,
    /// Absolute path of the worker's user-private (0600) data-plane socket.
    pub worker_socket: String,
    /// One-time token the client presents to the worker to authorize the session.
    pub token: String,
    /// The worker process id (for diagnostics / `ls`).
    pub worker_pid: u32,
    /// True if this attach cold-booted the VM; false if it joined an already-running one.
    pub cold_booted: bool,
}

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
            Command::Attach {
                name: "web".to_string(),
                boot: None,
            },
            Command::Attach {
                name: "api".to_string(),
                boot: Some(serde_json::json!({ "cpus": 4 })),
            },
            Command::Save {
                name: "web".to_string(),
            },
            Command::Stop {
                name: "web".to_string(),
                force: false,
            },
            Command::Stop {
                name: "web".to_string(),
                force: true,
            },
            Command::WorkerSaved {
                name: "web".to_string(),
            },
        ] {
            let req = Request::new(None, cmd.clone());
            let back: Request =
                serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
            assert_eq!(back.command, cmd);
            assert_eq!(back.protocol_version, PROTOCOL_VERSION);
        }
    }

    /// `save` flattens its `verb` tag and carries the sandbox name at the top level.
    #[test]
    fn save_command_roundtrips_with_a_flat_verb_and_name() {
        let req = Request::new(
            Some(5),
            Command::Save {
                name: "web".to_string(),
            },
        );
        let line = serde_json::to_string(&req).unwrap();
        assert!(
            line.contains("\"verb\":\"save\"") && line.contains("\"name\":\"web\""),
            "save must carry a flat verb tag and name: {line}"
        );
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);
    }

    /// `stop` flattens its `verb` tag and carries the sandbox name; `force` defaults to
    /// false on the wire when absent (so an older `--force`-less client still parses).
    #[test]
    fn stop_command_roundtrips_with_a_flat_verb_and_defaulted_force() {
        let req = Request::new(
            Some(9),
            Command::Stop {
                name: "web".to_string(),
                force: false,
            },
        );
        let line = serde_json::to_string(&req).unwrap();
        assert!(
            line.contains("\"verb\":\"stop\"") && line.contains("\"name\":\"web\""),
            "stop must carry a flat verb tag and name: {line}"
        );
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);

        // A request omitting `force` entirely must still parse, defaulting to false.
        let without_force: Request =
            serde_json::from_str(r#"{"protocol_version":1,"verb":"stop","name":"web"}"#).unwrap();
        assert_eq!(
            without_force.command,
            Command::Stop {
                name: "web".to_string(),
                force: false,
            }
        );
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
        assert!(
            !line.contains("error"),
            "ok response must omit error: {line}"
        );
        let back: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(back, resp);
        assert!(back.ok);
    }

    /// An error response carries `ok:false` and an `error`, no `result`.
    #[test]
    fn err_response_roundtrips() {
        let resp = Response::err(None, "boom");
        let line = serde_json::to_string(&resp).unwrap();
        assert!(
            !line.contains("result"),
            "err response must omit result: {line}"
        );
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

    /// `attach` flattens its `verb` tag and carries the sandbox name (and optional boot
    /// spec) at the top level, like the other commands.
    #[test]
    fn attach_command_roundtrips_with_a_flat_verb_and_name() {
        let req = Request::new(
            Some(3),
            Command::Attach {
                name: "web".to_string(),
                boot: None,
            },
        );
        let line = serde_json::to_string(&req).unwrap();
        assert!(
            line.contains("\"verb\":\"attach\"") && line.contains("\"name\":\"web\""),
            "attach must carry a flat verb tag and name: {line}"
        );
        assert!(!line.contains("boot"), "absent boot is omitted: {line}");
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);
    }

    /// The attach result payload round-trips, preserving the one-time token and the
    /// cold-boot flag the client uses to decide whether to warn about ignored flags.
    #[test]
    fn attach_result_roundtrips() {
        let res = AttachResult {
            name: "web".to_string(),
            worker_socket: "/tmp/web.sock".to_string(),
            token: "deadbeef".to_string(),
            worker_pid: 4321,
            cold_booted: true,
        };
        let back: AttachResult =
            serde_json::from_str(&serde_json::to_string(&res).unwrap()).unwrap();
        assert_eq!(back, res);
    }
}
