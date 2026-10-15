//! The Aeron adapter (feature `aeron`): the gateway's [`Venue`]. Commands go out on the ingress
//! publication (stream 1001), event batches come in on the egress subscription (stream 1003),
//! both `aeron:ipc` through the media driver. It is a client like `rfq-demo`, and opens and
//! reads its streams the same way (crates/rfq-demo/src/live.rs).
use std::time::Duration;

use anyhow::{Context, ensure};
use rfq_app::{ClientCommand, EventBatch};
use rfq_infra::aeron::rusteron_archive::{AeronHeader, AeronPublication, AeronSubscription, Handlers};
use rfq_infra::aeron::{
    Aeron, AeronError, EGRESS, INGRESS, IPC, RESOURCE_TIMEOUT, Unconnected, frame_of, offer,
};
use rfq_infra::codec::{decode, encode};
use rfq_infra::fragment::Reassembler;

use crate::hub_loop::{Offered, Venue};

/// Most fragments per egress poll.
const FRAGMENTS: usize = 256;

pub struct AeronVenue {
    ingress: AeronPublication,
    max_message: usize,
    egress: AeronSubscription,
    /// Egress messages over the IPC MTU arrive in fragments. Egress is trusted: no size limit.
    reassembler: Reassembler,
    /// Kept to see a dead driver: the client closes itself when the driver times out.
    client: Aeron,
}

impl AeronVenue {
    pub fn open(client: Aeron) -> anyhow::Result<Self> {
        let ingress = client
            .async_add_publication(IPC, INGRESS)
            .and_then(|p| p.poll_blocking(RESOURCE_TIMEOUT))
            .context("add the ingress publication")?;
        let egress = client
            .async_add_subscription(IPC, EGRESS, Handlers::NONE, Handlers::NONE)
            .and_then(|s| s.poll_blocking(RESOURCE_TIMEOUT))
            .context("add the egress subscription")?;
        let max_message = ingress.get_constants()?.max_message_length();
        tracing::info!(max_message, "aeron ready (ingress 1001, egress 1003)");
        Ok(AeronVenue {
            ingress,
            max_message,
            egress,
            reassembler: Reassembler::new(usize::MAX),
            client,
        })
    }

    fn check_open(&self) -> anyhow::Result<()> {
        ensure!(
            !self.client.is_closed(),
            "the Aeron client is closed (is the media driver gone?)"
        );
        ensure!(!self.ingress.is_closed(), "the ingress publication is closed");
        ensure!(!self.egress.is_closed(), "the egress subscription is closed");
        Ok(())
    }
}

impl Venue for AeronVenue {
    fn offer(&mut self, cmd: &ClientCommand, patience: Duration) -> anyhow::Result<Offered> {
        self.check_open()?;
        let ingress = &self.ingress;
        let sent = offer(
            |bytes| ingress.offer(bytes),
            &encode(cmd),
            self.max_message,
            "ingress",
            Unconnected::Drop,
            patience,
        );
        match sent {
            Ok(Some(_)) => Ok(Offered::Sent),
            Ok(None) => Ok(Offered::NotConnected),
            Err(AeronError::GaveUp { .. }) => Ok(Offered::BackPressured),
            // Cannot happen: the hub refuses anything over the node's 64 KiB ingress limit, far
            // below the max message. Should it, the command is refused, not the gateway.
            Err(AeronError::TooLarge { len, .. }) => Ok(Offered::TooLarge(len)),
            Err(e) => Err(anyhow::Error::new(e).context("offer to ingress")),
        }
    }

    /// Polls egress once and hands every whole message to `sink`: a batch, or one chunk of a
    /// batch the node split (chunks share the batch's `seq`, `time`, `kind` and `origin`; the
    /// `balances` travel on the last chunk).
    fn poll(&mut self, sink: &mut dyn FnMut(EventBatch)) -> anyhow::Result<usize> {
        self.check_open()?;
        let reassembler = &mut self.reassembler;
        let mut bad_header = None;
        let mut undecodable = 0usize;
        // A panic in here would abort the process (it runs inside a C callback): record errors.
        let fragments = self.egress.poll_fn(
            |data: &[u8], header: AeronHeader| {
                if bad_header.is_some() {
                    return;
                }
                match frame_of(&header) {
                    Ok((session, flags)) => {
                        reassembler.on_fragment(session, flags, data, &mut |msg| match decode::<EventBatch>(
                            msg,
                        ) {
                            Ok(batch) => sink(batch),
                            Err(_) => undecodable += 1,
                        })
                    }
                    Err(e) => bad_header = Some(e),
                }
            },
            FRAGMENTS,
        )?;
        // The node is built from the same workspace, so this means a bug, not a client's input:
        // it costs the batch, not the gateway.
        if undecodable > 0 {
            tracing::error!(
                count = undecodable,
                "egress messages that are not an EventBatch were dropped"
            );
        }
        if let Some(e) = bad_header {
            return Err(anyhow::Error::new(e).context("read an egress fragment header"));
        }
        Ok(usize::try_from(fragments).unwrap_or(0))
    }

    /// A node is there when it subscribes to ingress and its egress publication has reached this
    /// subscription: before both, a command could not be sent, or its answer would be dropped.
    fn connected(&self) -> bool {
        self.ingress.is_connected() && self.egress.is_connected()
    }
}
