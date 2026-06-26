//! A read-only HTTP/1.x framer, tee'd alongside the proxy's byte-substitution relay.
//!
//! The framer consumes a **copy of the pre-substitution guest bytes** (request side) or
//! the upstream bytes (response side) and reconstructs per-message structure: it parses
//! each head with [`httparse`], runs a `Content-Length` / `Transfer-Encoding: chunked`
//! body-length state machine to skip body bytes, and reframes the next message on
//! keep-alive / pipelined connections.
//!
//! It is pure observation: it never touches, blocks, or reorders the relay. Its guiding
//! invariant is **when in doubt, emit [`FrameEvent::Unparsed`] and stop** — on anything it
//! cannot parse (unexpected encoding, protocol upgrade, HTTP/2 over the tunnel, desync) it
//! emits one note and ignores all further bytes on that connection. It never emits garbage.
//!
//! Each side of a connection gets its own [`HttpFramer`] ([`Direction::Request`] /
//! [`Direction::Response`]); the proxy stamps `conn_id` + `ts_ms` via
//! [`FrameEvent::into_audit`] and `try_send`s the result fail-open.

use std::sync::Arc;

use crate::redact::{scrub_header, PlaceholderNames};
use crate::AuditEvent;

/// Which side of a connection a framer observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Guest → upstream bytes (request heads). Captured pre-substitution.
    Request,
    /// Upstream → guest bytes (status lines).
    Response,
}

/// A lean, identity-free framing observation. The proxy turns this into an
/// [`AuditEvent`] via [`FrameEvent::into_audit`], stamping `conn_id` and `ts_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameEvent {
    /// A request head was parsed. `headers` are already redacted.
    Request {
        method: String,
        path: String,
        http_minor: u8,
        head_bytes: u64,
        body_bytes: Option<u64>,
        headers: Vec<(String, String)>,
    },
    /// A response head was parsed. `headers` are already redacted.
    Response {
        status: u16,
        reason: String,
        http_minor: u8,
        head_bytes: u64,
        body_bytes: Option<u64>,
        headers: Vec<(String, String)>,
    },
    /// The framer gave up on this connection's direction.
    Unparsed { reason: &'static str },
}

impl FrameEvent {
    /// Stamp this lean observation with connection identity and a timestamp, yielding the
    /// self-describing [`AuditEvent`] the writer persists. `direction` labels an
    /// [`FrameEvent::Unparsed`] note; it is ignored for request/response events.
    pub fn into_audit(self, conn_id: u64, direction: Direction, ts_ms: u64) -> AuditEvent {
        match self {
            FrameEvent::Request {
                method,
                path,
                http_minor,
                head_bytes,
                body_bytes,
                headers,
            } => AuditEvent::HttpRequest {
                conn_id,
                method,
                path,
                http_minor,
                head_bytes,
                body_bytes,
                headers,
                ts_ms,
            },
            FrameEvent::Response {
                status,
                reason,
                http_minor,
                head_bytes,
                body_bytes,
                headers,
            } => AuditEvent::HttpResponse {
                conn_id,
                status,
                reason,
                http_minor,
                head_bytes,
                body_bytes,
                headers,
                ts_ms,
            },
            FrameEvent::Unparsed { reason } => AuditEvent::Unparsed {
                conn_id,
                direction: match direction {
                    Direction::Request => "request",
                    Direction::Response => "response",
                },
                reason,
                ts_ms,
            },
        }
    }
}

/// Incremental HTTP/1.x framer for one direction of one connection.
///
/// Feed it bytes with [`HttpFramer::push`] as they arrive (in arbitrary chunk
/// boundaries); it returns the [`FrameEvent`]s newly completed by those bytes.
pub struct HttpFramer {
    direction: Direction,
    state: State,
    /// Unconsumed bytes: head bytes still accumulating, or carried-over pipeline bytes.
    buf: Vec<u8>,
    /// `placeholder → secret-name`, applied as headers are captured so a raw credential is
    /// never carried on an event. Shared across both directions of a connection.
    names: Arc<PlaceholderNames>,
}

enum State {
    /// Accumulating a message head until `\r\n\r\n`.
    Head,
    /// Skipping a fixed-length body; `remaining` bytes left.
    Body { remaining: u64 },
    /// Skipping a `chunked` body.
    Chunked(ChunkState),
    /// Response body runs until the connection closes; consume and ignore the rest.
    UntilClose,
    /// Terminal: emitted `Unparsed` (or done). Ignore all further bytes.
    Stopped,
}

enum ChunkState {
    /// Reading a chunk-size line.
    Size,
    /// Skipping chunk data + its trailing CRLF; `remaining` bytes left.
    Data { remaining: u64 },
    /// After the terminating `0` chunk, consuming optional trailers until a blank line.
    Trailer,
}

/// Cap on an un-terminated head. Beyond this we assume a desync rather than buffer forever.
const HEAD_LIMIT: usize = 256 * 1024;

impl HttpFramer {
    /// Create a framer for one direction of a connection with no placeholder map: sensitive
    /// header values are still redacted (to `<redacted len=N>`), they just cannot be attributed.
    pub fn new(direction: Direction) -> Self {
        Self::with_names(direction, Arc::new(PlaceholderNames::new()))
    }

    /// Create a framer that attributes known dome placeholders in sensitive headers to their
    /// secret name via `names` (`placeholder → secret-name`).
    pub fn with_names(direction: Direction, names: Arc<PlaceholderNames>) -> Self {
        HttpFramer {
            direction,
            state: State::Head,
            buf: Vec::new(),
            names,
        }
    }

    /// Feed the next slice of observed bytes; returns events newly completed.
    pub fn push(&mut self, data: &[u8]) -> Vec<FrameEvent> {
        let mut events = Vec::new();
        if matches!(self.state, State::Stopped | State::UntilClose) {
            // Terminal or read-until-close: nothing left to emit, just drop the bytes.
            return events;
        }
        self.buf.extend_from_slice(data);
        loop {
            match &mut self.state {
                State::Head => match self.parse_head() {
                    HeadParse::Incomplete => {
                        if self.buf.len() > HEAD_LIMIT {
                            events.push(FrameEvent::Unparsed { reason: "head too large" });
                            self.stop();
                        }
                        break;
                    }
                    HeadParse::Unparsed(reason) => {
                        events.push(FrameEvent::Unparsed { reason });
                        self.stop();
                        break;
                    }
                    HeadParse::Complete { head_len, event, next } => {
                        self.buf.drain(..head_len);
                        events.push(event);
                        self.state = next;
                        // Loop again to process any body / pipelined bytes already buffered.
                    }
                },
                State::Body { remaining } => {
                    let take = (*remaining).min(self.buf.len() as u64);
                    self.buf.drain(..take as usize);
                    *remaining -= take;
                    if *remaining == 0 {
                        self.state = State::Head;
                    } else {
                        break; // consumed all we have; await more body bytes
                    }
                }
                State::Chunked(cs) => match consume_chunked(&mut self.buf, cs) {
                    ChunkProgress::NeedMore => break,
                    ChunkProgress::Done => self.state = State::Head,
                    ChunkProgress::Error => {
                        events.push(FrameEvent::Unparsed { reason: "chunked framing" });
                        self.stop();
                        break;
                    }
                },
                State::UntilClose | State::Stopped => break,
            }
        }
        events
    }

    /// Enter the terminal state and release the buffer.
    fn stop(&mut self) {
        self.state = State::Stopped;
        self.buf.clear();
    }

    /// Attempt to parse a complete head from `self.buf`.
    fn parse_head(&self) -> HeadParse {
        match self.direction {
            Direction::Request => parse_request_head(&self.buf, &self.names),
            Direction::Response => parse_response_head(&self.buf, &self.names),
        }
    }
}

/// Capture each header as a `(name, redacted-value)` pair in wire order. Redaction happens
/// here, at capture, so a raw sensitive value is never carried on a [`FrameEvent`].
fn capture_headers(headers: &[httparse::Header], names: &PlaceholderNames) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|h| {
            let value = String::from_utf8_lossy(h.value);
            (h.name.to_string(), scrub_header(h.name, &value, names))
        })
        .collect()
}

/// Outcome of attempting to parse one message head.
enum HeadParse {
    /// Need more bytes.
    Incomplete,
    /// Cannot parse — emit a note and stop.
    Unparsed(&'static str),
    /// Parsed; `head_len` bytes consumed, `event` to emit, `next` body state.
    Complete {
        head_len: usize,
        event: FrameEvent,
        next: State,
    },
}

fn parse_request_head(buf: &[u8], names: &PlaceholderNames) -> HeadParse {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let head_len = match req.parse(buf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => return HeadParse::Incomplete,
        // Covers HTTP/2 preface ("PRI * HTTP/2.0"), too many headers, garbage, etc.
        Err(_) => return HeadParse::Unparsed("request head"),
    };
    let method = req.method.unwrap_or_default();
    // CONNECT establishes an opaque tunnel we cannot frame as HTTP.
    if method.eq_ignore_ascii_case("CONNECT") {
        return HeadParse::Unparsed("connect tunnel");
    }
    let next = match body_length(req.headers, BodyContext::Request) {
        Ok(b) => b.into_state(),
        Err(reason) => return HeadParse::Unparsed(reason),
    };
    let body_bytes = next_declared_len(&next);
    let captured = capture_headers(req.headers, names);
    HeadParse::Complete {
        head_len,
        event: FrameEvent::Request {
            method: method.to_string(),
            path: req.path.unwrap_or_default().to_string(),
            http_minor: req.version.unwrap_or(1),
            head_bytes: head_len as u64,
            body_bytes,
            headers: captured,
        },
        next,
    }
}

fn parse_response_head(buf: &[u8], names: &PlaceholderNames) -> HeadParse {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers);
    let head_len = match resp.parse(buf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => return HeadParse::Incomplete,
        Err(_) => return HeadParse::Unparsed("response head"),
    };
    let status = resp.code.unwrap_or_default();
    // 1xx informational, 204 No Content, and 304 Not Modified never carry a body
    // regardless of headers (RFC 9112 §6.3).
    let next = if (100..200).contains(&status) || status == 204 || status == 304 {
        State::Head
    } else {
        match body_length(resp.headers, BodyContext::Response) {
            Ok(b) => b.into_state(),
            Err(reason) => return HeadParse::Unparsed(reason),
        }
    };
    let body_bytes = next_declared_len(&next);
    let captured = capture_headers(resp.headers, names);
    HeadParse::Complete {
        head_len,
        event: FrameEvent::Response {
            status,
            reason: resp.reason.unwrap_or_default().to_string(),
            http_minor: resp.version.unwrap_or(1),
            head_bytes: head_len as u64,
            body_bytes,
            headers: captured,
        },
        next,
    }
}

/// The declared body length to report in the emitted event: the `Content-Length` for a
/// fixed-length body, `Some(0)` for no body, and `None` for chunked / read-until-close.
fn next_declared_len(next: &State) -> Option<u64> {
    match next {
        State::Head => Some(0),
        State::Body { remaining } => Some(*remaining),
        State::Chunked(_) | State::UntilClose => None,
        State::Stopped => None,
    }
}

/// Whether a parsed head is followed by a request body or a response body — they differ
/// in their default when neither `Content-Length` nor `Transfer-Encoding` is present.
#[derive(Clone, Copy)]
enum BodyContext {
    Request,
    Response,
}

/// How the body after a head is delimited.
enum Body {
    None,
    Fixed(u64),
    Chunked,
    UntilClose,
}

impl Body {
    fn into_state(self) -> State {
        match self {
            Body::None => State::Head,
            Body::Fixed(0) => State::Head,
            Body::Fixed(n) => State::Body { remaining: n },
            Body::Chunked => State::Chunked(ChunkState::Size),
            Body::UntilClose => State::UntilClose,
        }
    }
}

/// Determine how the body after a head is framed from its headers (RFC 9112 §6).
fn body_length(headers: &[httparse::Header], ctx: BodyContext) -> Result<Body, &'static str> {
    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    for h in headers {
        if h.name.eq_ignore_ascii_case("transfer-encoding") {
            // The final encoding being "chunked" makes the body self-delimiting.
            if let Ok(v) = std::str::from_utf8(h.value) {
                if v.split(',').any(|t| t.trim().eq_ignore_ascii_case("chunked")) {
                    chunked = true;
                }
            }
        } else if h.name.eq_ignore_ascii_case("content-length") {
            let v = std::str::from_utf8(h.value)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok());
            match v {
                Some(n) => content_length = Some(n),
                None => return Err("bad content-length"),
            }
        }
    }
    // Both present is a request-smuggling vector; refuse to guess rather than desync.
    if chunked && content_length.is_some() {
        return Err("conflicting length");
    }
    if chunked {
        return Ok(Body::Chunked);
    }
    if let Some(n) = content_length {
        return Ok(Body::Fixed(n));
    }
    // No framing headers: a request has no body; a response runs until the connection
    // closes (HTTP/1.0-style or `Connection: close`).
    Ok(match ctx {
        BodyContext::Request => Body::None,
        BodyContext::Response => Body::UntilClose,
    })
}

enum ChunkProgress {
    NeedMore,
    Done,
    Error,
}

/// Consume as much of a `chunked` body as `buf` allows, advancing `st`. Drains consumed
/// bytes from `buf`. Returns `Done` once the terminating `0`-chunk and trailers are seen.
fn consume_chunked(buf: &mut Vec<u8>, st: &mut ChunkState) -> ChunkProgress {
    loop {
        match st {
            ChunkState::Size => {
                let Some(pos) = find_crlf(buf) else {
                    return ChunkProgress::NeedMore;
                };
                // Strip any chunk extensions (";ext") before the size.
                let size_field = buf[..pos].split(|&b| b == b';').next().unwrap_or(&[]);
                let size = match parse_hex(size_field) {
                    Some(n) => n,
                    None => return ChunkProgress::Error,
                };
                buf.drain(..pos + 2);
                if size == 0 {
                    *st = ChunkState::Trailer;
                } else {
                    // +2 to also skip the CRLF that terminates the chunk data.
                    *st = ChunkState::Data { remaining: size + 2 };
                }
            }
            ChunkState::Data { remaining } => {
                let take = (*remaining).min(buf.len() as u64);
                buf.drain(..take as usize);
                *remaining -= take;
                if *remaining == 0 {
                    *st = ChunkState::Size;
                } else {
                    return ChunkProgress::NeedMore;
                }
            }
            ChunkState::Trailer => {
                let Some(pos) = find_crlf(buf) else {
                    return ChunkProgress::NeedMore;
                };
                if pos == 0 {
                    // Blank line terminates the trailer section.
                    buf.drain(..2);
                    return ChunkProgress::Done;
                }
                // A trailer header line; consume it and look for the next.
                buf.drain(..pos + 2);
            }
        }
    }
}

/// Index of the first `\r\n` in `buf`, if any.
fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Parse an ASCII-hex chunk size, ignoring surrounding whitespace. `None` on garbage.
fn parse_hex(bytes: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(bytes).ok()?.trim();
    if s.is_empty() {
        return None;
    }
    u64::from_str_radix(s, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: the method/path/sizes of every request event, in order.
    fn req_lines(events: &[FrameEvent]) -> Vec<(String, String, Option<u64>)> {
        events
            .iter()
            .filter_map(|e| match e {
                FrameEvent::Request {
                    method,
                    path,
                    body_bytes,
                    ..
                } => Some((method.clone(), path.clone(), *body_bytes)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn single_request_emits_one_event() {
        let mut f = HttpFramer::new(Direction::Request);
        let events = f.push(b"GET /hello HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert_eq!(
            events,
            vec![FrameEvent::Request {
                method: "GET".into(),
                path: "/hello".into(),
                http_minor: 1,
                head_bytes: 42,
                body_bytes: Some(0),
                headers: vec![("Host".into(), "example.com".into())],
            }]
        );
    }

    #[test]
    fn keep_alive_produces_one_event_per_request() {
        let mut f = HttpFramer::new(Direction::Request);
        let mut events = f.push(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n");
        events.extend(f.push(b"GET /b HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert_eq!(
            req_lines(&events),
            vec![
                ("GET".into(), "/a".into(), Some(0)),
                ("GET".into(), "/b".into(), Some(0)),
            ]
        );
    }

    #[test]
    fn pipelined_requests_in_one_read_each_framed() {
        let mut f = HttpFramer::new(Direction::Request);
        // Two requests arriving in a single read, the second with a body.
        let events = f.push(
            b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n\
              POST /b HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nabc\
              GET /c HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(
            req_lines(&events),
            vec![
                ("GET".into(), "/a".into(), Some(0)),
                ("POST".into(), "/b".into(), Some(3)),
                ("GET".into(), "/c".into(), Some(0)),
            ]
        );
    }

    #[test]
    fn content_length_body_is_skipped_before_next_request() {
        let mut f = HttpFramer::new(Direction::Request);
        let events = f.push(
            b"POST /up HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello\
              GET /next HTTP/1.1\r\n\r\n",
        );
        assert_eq!(
            req_lines(&events),
            vec![
                ("POST".into(), "/up".into(), Some(5)),
                ("GET".into(), "/next".into(), Some(0)),
            ]
        );
    }

    #[test]
    fn chunked_body_is_skipped_and_reframes() {
        let mut f = HttpFramer::new(Direction::Request);
        // "Wiki" + "pedia" chunks, then terminator, then a pipelined GET.
        let events = f.push(
            b"POST /c HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
              4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n\
              GET /after HTTP/1.1\r\n\r\n",
        );
        assert_eq!(
            req_lines(&events),
            vec![
                ("POST".into(), "/c".into(), None),
                ("GET".into(), "/after".into(), Some(0)),
            ]
        );
    }

    #[test]
    fn request_split_across_reads_is_framed() {
        let mut f = HttpFramer::new(Direction::Request);
        // Head split mid-line, body split from head.
        let mut events = f.push(b"POST /sp HTTP/1.1\r\nContent-Len");
        assert!(events.is_empty(), "incomplete head yields nothing yet");
        events.extend(f.push(b"gth: 4\r\n\r\nab"));
        events.extend(f.push(b"cd"));
        events.extend(f.push(b"GET /done HTTP/1.1\r\n\r\n"));
        assert_eq!(
            req_lines(&events),
            vec![
                ("POST".into(), "/sp".into(), Some(4)),
                ("GET".into(), "/done".into(), Some(0)),
            ]
        );
    }

    #[test]
    fn response_status_line_is_parsed() {
        let mut f = HttpFramer::new(Direction::Response);
        let events = f.push(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(
            events,
            vec![FrameEvent::Response {
                status: 200,
                reason: "OK".into(),
                http_minor: 1,
                head_bytes: 38,
                body_bytes: Some(0),
                headers: vec![("Content-Length".into(), "0".into())],
            }]
        );
    }

    #[test]
    fn response_no_body_statuses_reframe_immediately() {
        let mut f = HttpFramer::new(Direction::Response);
        // 304 carries no body even with a Content-Length present; the next status must frame.
        let events = f.push(
            b"HTTP/1.1 304 Not Modified\r\nContent-Length: 99\r\n\r\n\
              HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        );
        let statuses: Vec<u16> = events
            .iter()
            .filter_map(|e| match e {
                FrameEvent::Response { status, .. } => Some(*status),
                _ => None,
            })
            .collect();
        assert_eq!(statuses, vec![304, 200]);
    }

    #[test]
    fn malformed_input_yields_unparsed_and_stops() {
        let mut f = HttpFramer::new(Direction::Request);
        let events = f.push(b"\x16\x03\x01 not http at all \x00\x00\r\n\r\n");
        assert_eq!(events, vec![FrameEvent::Unparsed { reason: "request head" }]);
        // Once stopped, valid traffic afterward is ignored — no garbage, no recovery.
        assert!(f.push(b"GET /x HTTP/1.1\r\n\r\n").is_empty());
    }

    #[test]
    fn http2_preface_yields_unparsed() {
        let mut f = HttpFramer::new(Direction::Request);
        let events = f.push(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], FrameEvent::Unparsed { .. }));
    }

    #[test]
    fn connect_tunnel_yields_unparsed() {
        let mut f = HttpFramer::new(Direction::Request);
        let events = f.push(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert_eq!(
            events,
            vec![FrameEvent::Unparsed { reason: "connect tunnel" }]
        );
    }

    #[test]
    fn conflicting_length_headers_yield_unparsed() {
        let mut f = HttpFramer::new(Direction::Request);
        let events = f.push(
            b"POST /x HTTP/1.1\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n",
        );
        assert_eq!(
            events,
            vec![FrameEvent::Unparsed { reason: "conflicting length" }]
        );
    }

    #[test]
    fn bad_chunk_size_yields_unparsed() {
        let mut f = HttpFramer::new(Direction::Request);
        let events = f.push(
            b"POST /c HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nXYZ\r\njunk\r\n",
        );
        // The request head frames fine; the garbage chunk size trips the body machine.
        assert_eq!(req_lines(&events), vec![("POST".into(), "/c".into(), None)]);
        assert!(events
            .iter()
            .any(|e| matches!(e, FrameEvent::Unparsed { reason: "chunked framing" })));
    }

    /// The headers of the first request event, in order.
    fn req_headers(events: &[FrameEvent]) -> Vec<(String, String)> {
        events
            .iter()
            .find_map(|e| match e {
                FrameEvent::Request { headers, .. } => Some(headers.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn request_headers_are_captured_and_redacted() {
        let mut names = std::collections::HashMap::new();
        names.insert("dome_tok_xyz".to_string(), "ECHO_TOKEN".to_string());
        let mut f = HttpFramer::with_names(Direction::Request, std::sync::Arc::new(names));
        let events = f.push(
            b"GET /get HTTP/1.1\r\nHost: api.example.com\r\nAuthorization: Bearer dome_tok_xyz\r\n\r\n",
        );
        assert_eq!(
            req_headers(&events),
            vec![
                ("Host".into(), "api.example.com".into()),
                ("Authorization".into(), "Bearer <secret:ECHO_TOKEN>".into()),
            ]
        );
    }

    #[test]
    fn unknown_sensitive_header_is_length_redacted_in_event() {
        let mut f = HttpFramer::new(Direction::Request);
        let events = f.push(b"GET / HTTP/1.1\r\nCookie: session=opaque99\r\n\r\n");
        assert_eq!(
            req_headers(&events),
            vec![("Cookie".into(), "<redacted len=16>".into())]
        );
    }

    #[test]
    fn into_audit_carries_headers() {
        let ev = FrameEvent::Request {
            method: "GET".into(),
            path: "/a".into(),
            http_minor: 1,
            head_bytes: 20,
            body_bytes: Some(0),
            headers: vec![("Host".into(), "x".into())],
        }
        .into_audit(1, Direction::Request, 9);
        assert_eq!(
            ev,
            AuditEvent::HttpRequest {
                conn_id: 1,
                method: "GET".into(),
                path: "/a".into(),
                http_minor: 1,
                head_bytes: 20,
                body_bytes: Some(0),
                headers: vec![("Host".into(), "x".into())],
                ts_ms: 9,
            }
        );
    }

    #[test]
    fn into_audit_stamps_identity_and_direction() {
        let req = FrameEvent::Request {
            method: "GET".into(),
            path: "/a".into(),
            http_minor: 1,
            head_bytes: 20,
            body_bytes: Some(0),
            headers: vec![],
        }
        .into_audit(7, Direction::Request, 123);
        assert_eq!(
            req,
            AuditEvent::HttpRequest {
                conn_id: 7,
                method: "GET".into(),
                path: "/a".into(),
                http_minor: 1,
                head_bytes: 20,
                body_bytes: Some(0),
                headers: vec![],
                ts_ms: 123,
            }
        );

        let note = FrameEvent::Unparsed { reason: "response head" }
            .into_audit(7, Direction::Response, 456);
        assert_eq!(
            note,
            AuditEvent::Unparsed {
                conn_id: 7,
                direction: "response",
                reason: "response head",
                ts_ms: 456,
            }
        );
    }
}
