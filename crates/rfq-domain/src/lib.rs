//! Pure domain layer of the deterministic RFQ engine.
//!
//! No I/O, no clock, no randomness: every function here is a pure transformation of its inputs, so
//! the same command log always replays to the same state.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod command;
pub mod engine;
pub mod event;
pub mod ledger;
pub mod model;
pub mod types;

pub use command::*;
pub use engine::Engine;
pub use event::*;
pub use ledger::*;
pub use model::*;
pub use types::*;
