//! `rfq-node`: the composition root. Reads its configuration from the environment (see
//! [`config`]), connects to the Aeron media driver, wires the Aeron transport and a journal into
//! the node, and runs the node loop until the first error, which ends the process with status 1.
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

// Only the Aeron runtime reads the configuration; the parsing is tested either way.
#[cfg_attr(not(feature = "aeron"), allow(dead_code))]
mod config;

#[cfg(not(feature = "aeron"))]
fn main() -> ExitCode {
    eprintln!(
        "rfq-node was built without Aeron support; build it with `--features aeron` \
         (docker/rust.Dockerfile does)"
    );
    ExitCode::from(2)
}

#[cfg(feature = "aeron")]
fn main() -> ExitCode {
    use std::io::IsTerminal;

    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    match node::run() {
        Ok(never) => match never {},
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "aeron")]
mod node {
    use std::convert::Infallible;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use anyhow::Context;
    use rfq_app::node::hex;
    use rfq_app::{Journal, Node};
    use rfq_infra::aeron::{self, AeronTransport, ArchiveJournal};
    use rfq_infra::journal::FileJournal;
    use rfq_testkit::genesis;
    use rfq_testkit::scenarios::demo_config;

    use crate::config::{Config, JournalKind};

    /// How long to sleep after a poll that sequenced nothing.
    const IDLE: Duration = Duration::from_millis(1);

    pub fn run() -> anyhow::Result<Infallible> {
        let config = Config::from_env()?;
        tracing::info!(?config, "starting rfq-node");
        let aeron = aeron::connect(&config.aeron_dir)
            .with_context(|| format!("connect to the media driver at {}", config.aeron_dir))?;
        let transport = AeronTransport::new(&aeron).context("open ingress and egress")?;
        match &config.journal {
            JournalKind::Archive => {
                let journal = ArchiveJournal::new(&aeron).context("connect to the archive")?;
                serve(journal, transport, config.hash_every_batch)
            }
            JournalKind::File(path) => {
                let journal = FileJournal::open(path)
                    .with_context(|| format!("open the journal {}", path.display()))?;
                serve(journal, transport, config.hash_every_batch)
            }
        }
    }

    /// Replays the journal (logging `replayed seq=.. hash=..`), then runs the node loop.
    fn serve<J: Journal>(
        journal: J,
        transport: AeronTransport,
        hash_every_batch: bool,
    ) -> anyhow::Result<Infallible> {
        let mut node = Node::start(journal, transport, genesis(demo_config())).context("start the node")?;
        loop {
            let sequenced = node.poll(now_ms()?).context("node loop")?;
            if sequenced == 0 {
                std::thread::sleep(IDLE);
            } else if hash_every_batch {
                tracing::info!(seq = node.engine().last_seq(), hash = %hex(&node.state_hash()), "applied");
            }
        }
    }

    fn now_ms() -> anyhow::Result<u64> {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before 1970")?;
        u64::try_from(since_epoch.as_millis()).context("system clock is out of range")
    }
}
