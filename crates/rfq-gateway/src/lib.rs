//! The gateway: the engine's client edge (docs/specs/gateway.md).
//!
//! Clients speak JSON over a WebSocket ([`protocol`]). The gateway puts their commands on ingress,
//! with the next nonce of each actor ([`nonce_book`]), and sends every egress batch to each
//! connection filtered for its view ([`view`]), so a participant sees only what engine.md §7 lets
//! it see. [`hub`] is the state machine that ties these together.
//!
//! Everything in this library is pure: no Aeron, no sockets, no threads of its own, no clock.
//! The binary (feature `aeron`) wraps the hub in I/O. The HTTP read API's routing
//! ([`read_api`]) and its shared state ([`read_api::ReadSide`], [`status`]) live here too, so
//! they are tested without a socket; the server and the replica thread are the binary's.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod hub;
pub mod nonce_book;
pub mod origin;
pub mod protocol;
pub mod read_api;
pub mod status;
pub mod view;
