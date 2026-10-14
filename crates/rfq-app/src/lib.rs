//! Application layer of the RFQ engine: protocol DTOs now; ports, sequencer and node service later.
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

pub mod protocol;

pub use protocol::*;
