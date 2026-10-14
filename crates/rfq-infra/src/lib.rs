//! Infrastructure layer of the RFQ engine: adapters that connect the application's ports to the
//! outside world. Currently the versioned wire codec; journals and transports follow.
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
