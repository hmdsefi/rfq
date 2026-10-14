//! Infrastructure layer of the RFQ engine: adapters that connect the application's ports to the
//! outside world. The versioned wire codec, the file and in-memory `Journal`s, and the in-memory
//! `Transport`; the Aeron transport and Archive journal follow behind the `aeron` feature.
//!
//! Depends on `rfq-app` and `rfq-domain`; never on a binary.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod codec;
pub mod journal;
pub mod transport;
