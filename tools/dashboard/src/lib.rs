//! Demo aid for the live session: the real `rfq-domain` engine, compiled to WebAssembly and
//! driven from one HTML page. Not part of the engine: nothing under `crates/` depends on it.
#![deny(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod session;
