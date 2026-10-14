//! The gateway: the engine's client edge (docs/specs/gateway.md).
//!
//! Clients speak JSON over a WebSocket ([`protocol`]). The gateway puts their commands on ingress,
//! with the next nonce of each actor ([`nonce_book`]), and sends every egress batch to each
//! connection filtered for its view ([`view`]), so a participant sees only what engine.md §7 lets
//! it see. [`hub`] is the state machine that ties these together.
//!
//! Everything in this library is pure: no Aeron, no sockets, no threads, no clock. The binary
//! (feature `aeron`) wraps the hub in I/O.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod hub;
pub mod nonce_book;
pub mod protocol;
pub mod view;
