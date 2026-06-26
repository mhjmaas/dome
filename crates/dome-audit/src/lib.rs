//! Egress audit log for dome sandboxes.
//!
//! This crate is the single correctness seam of the audit subsystem (PRD #101): it owns
//! the [`AuditEvent`] model, the JSONL writer task, identity stamping, and durability.
//! `dome-proxy` depends on it only for the event type and emits events through a channel;
//! `dome-cli` constructs the writer bound to a `{sandbox, session}` and threads its sender
//! into the proxy.
//!
//! Capture is strictly observe-and-emit, off the network hot path, and fail-open: the proxy
//! `try_send`s into a bounded channel and never blocks egress on the audit subsystem.

mod event;
mod writer;

pub use event::{AuditEvent, ConnKind};
pub use writer::{mint_session, AuditHandle, AuditWriter, WriterConfig};
