//! Infrastructure layer of the RFQ engine: adapters that connect the application's ports to the
//! outside world: the versioned wire codec, the file and in-memory `Journal`s, the in-memory
//! `Transport`, and, behind the `aeron` feature, the Aeron transport and Archive journal. The
//! pieces of the Aeron path that need no Aeron (fragment reassembly, egress batch splitting) are
//! always built, so they are tested without it.
//!
//! Depends on `rfq-app` and `rfq-domain`; never on a binary.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

#[cfg(feature = "aeron")]
pub mod aeron;
pub mod codec;
pub mod fragment;
pub mod journal;
pub mod transport;
