//! The read side of the RFQ engine (docs/specs/read-api.md): a read-only copy of the engine fed by
//! the journal, the trace it regenerates, and the views the HTTP API serves.
//!
//! [`Replica`] applies the journal's commands in order through the same code as the node
//! (`Engine::apply`, then `rfq_app::batch_of`), so at the same `seq` its `state_hash` equals the
//! node's and every batch the node published is identical in its trace. [`views`] turns an
//! engine into the JSON the API returns: an account sees only its own data, the public endpoints
//! only what public events already show, and a request's live quote set never.
//!
//! No I/O and no clock: depends on `rfq-domain` and `rfq-app` only. The journal follower, the
//! lock around the replica and the HTTP server live in `rfq-infra` and `rfq-gateway`.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod replica;
pub mod views;

pub use replica::{Replica, ReplicaError, TraceEntry};
pub use views::*;
