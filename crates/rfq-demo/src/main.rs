//! `rfq-demo`: runs the scripted scenarios (`rfq_testkit::scenarios::all()`) against a live
//! `rfq-node` over Aeron, and checks each one's expected event order on egress.
//!
//! ```text
//! rfq-demo [scenario]      # default: every scenario
//! ```
//!
//! It is a client like any other: commands go out on ingress (stream 1001), event batches come in
//! on egress (stream 1003), through the media driver in `AERON_DIR` (default `/dev/shm/aeron`).
//! It prints each scenario's own events, then `PASS <name>` or `FAIL <name>: <why>`, and finally
//! `passed <n>/<m>`. Exit status: 0 if every scenario passed, 1 otherwise (or on any error).
//!
//! Built without the `aeron` feature, it only says so and exits with status 2.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::process::ExitCode;

// The Aeron runner is the only user; the logic is tested either way. The nonce probing it uses
// lives in `rfq_app::client::nonce`, shared with the gateway.
#[cfg_attr(not(feature = "aeron"), allow(dead_code))]
mod scope;

#[cfg(feature = "aeron")]
mod live;

#[cfg(not(feature = "aeron"))]
fn main() -> ExitCode {
    eprintln!(
        "rfq-demo was built without Aeron support; build it with `--features aeron` \
         (docker/rust.Dockerfile does)"
    );
    ExitCode::from(2)
}

#[cfg(feature = "aeron")]
fn main() -> ExitCode {
    // Logs (rfq-infra's connection retries, Aeron client errors) go to stderr; results to stdout.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false)
        .init();
    match live::run(std::env::args().nth(1).as_deref()) {
        Ok(summary) if summary.all_passed() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("rfq-demo: {e:#}");
            ExitCode::FAILURE
        }
    }
}
