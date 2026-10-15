//! `rfq-gateway`: the engine's client edge (docs/specs/gateway.md). Clients speak JSON over a
//! WebSocket; the gateway puts their commands on ingress and sends every egress batch back,
//! filtered for each connection's view.
//!
//! Three kinds of threads (gateway.md §9):
//! - the hub loop ([`hub_loop`]), which owns the [`Hub`](rfq_gateway::hub::Hub), the ingress
//!   publication and the egress subscription ([`aeron`]);
//! - the WebSocket acceptor, and one thread per connection ([`ws`]).
//!
//! It reads its configuration from the environment ([`config`]). A failure of the Aeron client,
//! the media driver or a publication is fatal: it is logged and the process exits with status 1
//! (docker-compose restarts it). A dead engine is not: the gateway reports it to its clients as
//! `status down` and keeps serving.
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

// Only the Aeron runtime uses these; they are tested either way.
#[cfg_attr(not(feature = "aeron"), allow(dead_code))]
mod config;
#[cfg_attr(not(feature = "aeron"), allow(dead_code))]
mod hub_loop;
#[cfg_attr(not(feature = "aeron"), allow(dead_code))]
mod ws;

#[cfg(feature = "aeron")]
mod aeron;

#[cfg(not(feature = "aeron"))]
fn main() -> ExitCode {
    eprintln!(
        "rfq-gateway was built without Aeron support; build it with `--features aeron` \
         (docker/rust.Dockerfile does)"
    );
    ExitCode::from(2)
}

#[cfg(feature = "aeron")]
fn main() -> ExitCode {
    use std::io::IsTerminal;

    use tracing_subscriber::filter::LevelFilter;

    // `RUST_LOG` is one level for everything (`debug`, say); anything else means `info`.
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|l| l.parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    match run() {
        Ok(never) => match never {},
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "aeron")]
fn run() -> anyhow::Result<std::convert::Infallible> {
    use std::net::TcpListener;
    use std::sync::mpsc;

    use anyhow::Context;

    let config = config::Config::from_env()?;
    tracing::info!(?config, "starting rfq-gateway");
    let client = rfq_infra::aeron::connect(&config.aeron_dir)
        .with_context(|| format!("connect to the media driver at {}", config.aeron_dir))?;
    let venue = aeron::AeronVenue::open(client)?;
    let listener = TcpListener::bind(config.addr).with_context(|| format!("listen on {}", config.addr))?;
    tracing::info!(addr = %listener.local_addr()?, "gateway listening");
    let (inbox, items) = mpsc::sync_channel(ws::INBOX);
    ws::spawn(listener, inbox).context("start the WebSocket acceptor")?;
    hub_loop::HubLoop::new(venue, items).run()
}
