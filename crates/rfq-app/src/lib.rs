//! Application layer of the RFQ engine: protocol DTOs, the `Journal` and `Transport` ports, the
//! sequencer, the `Node` service that runs the write-ahead loop, and the logic its clients share
//! (`client`).
//!
//! Depends only on `rfq-domain`. Knows nothing of wire formats, files or Aeron: adapters live in
//! `rfq-infra` and translate bytes to and from the typed values defined here.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod client;
pub mod error;
pub mod node;
pub mod output;
pub mod ports;
pub mod protocol;
pub mod sequencer;

pub use error::{BoxError, NodeError, SequenceError};
pub use node::Node;
pub use output::batch_of;
pub use ports::{Journal, Transport};
pub use protocol::*;
pub use sequencer::Sequencer;
