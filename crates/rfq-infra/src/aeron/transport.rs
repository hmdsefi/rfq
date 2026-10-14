//! [`AeronTransport`]: client commands in on stream 1001, event batches out on stream 1003.
use std::time::Duration;

use rfq_app::{BoxError, ClientCommand, EventBatch, Transport};
use rusteron_archive::{Aeron, AeronHeader, AeronPublication, AeronSubscription, Handlers};

use super::{AeronError, EGRESS, INGRESS, IPC, RESOURCE_TIMEOUT, Unconnected, frame_of, offer};
use crate::codec::{MAX_INGRESS, decode_ingress, encode};
use crate::fragment::Reassembler;
use crate::transport::split_batch;

/// How long egress may stay back-pressured by a slow subscriber before the node gives up.
const EGRESS_PATIENCE: Duration = Duration::from_secs(5);

pub struct AeronTransport {
    ingress: AeronSubscription,
    egress: AeronPublication,
    /// The egress publication's max message length; larger batches are split.
    max_message: usize,
    /// Ingress messages over [`MAX_INGRESS`] are dropped while still in fragments.
    reassembler: Reassembler,
}

impl AeronTransport {
    pub fn new(aeron: &Aeron) -> Result<Self, AeronError> {
        let ingress = aeron
            .async_add_subscription(IPC, INGRESS, Handlers::NONE, Handlers::NONE)?
            .poll_blocking(RESOURCE_TIMEOUT)?;
        let egress = aeron
            .async_add_publication(IPC, EGRESS)?
            .poll_blocking(RESOURCE_TIMEOUT)?;
        let max_message = egress.get_constants()?.max_message_length();
        tracing::info!(max_message, "aeron transport ready (ingress 1001, egress 1003)");
        Ok(AeronTransport {
            ingress,
            egress,
            max_message,
            reassembler: Reassembler::new(MAX_INGRESS),
        })
    }
}

impl Transport for AeronTransport {
    /// Polls up to `max` fragments, so at most `max` commands. Fragmented messages are joined;
    /// oversized or malformed ones are dropped by [`decode_ingress`].
    fn poll(&mut self, max: usize, sink: &mut dyn FnMut(ClientCommand)) -> Result<(), BoxError> {
        if self.ingress.is_closed() {
            return Err(AeronError::Closed("ingress subscription").into());
        }
        let reassembler = &mut self.reassembler;
        let mut bad_header = None;
        self.ingress
            .poll_fn(
                |data: &[u8], header: AeronHeader| match frame_of(&header) {
                    Ok((session, flags)) => reassembler.on_fragment(session, flags, data, &mut |msg| {
                        if let Some(cmd) = decode_ingress(msg) {
                            sink(cmd);
                        }
                    }),
                    Err(e) => bad_header = Some(e),
                },
                max,
            )
            .map_err(AeronError::from)?;
        match bad_header {
            Some(e) => Err(AeronError::from(e).into()),
            None => Ok(()),
        }
    }

    /// Publishes `batch` as one message, or as several chunks with the same `seq` when it exceeds
    /// the max message length. With no subscriber connected the batch is dropped: events are
    /// derivable from the log, and clients deduplicate by `seq`.
    fn publish(&mut self, batch: &EventBatch) -> Result<(), BoxError> {
        for chunk in split_batch(batch, self.max_message) {
            let bytes = encode(&chunk);
            let sent = offer(
                |b| self.egress.offer(b),
                &bytes,
                self.max_message,
                "egress",
                Unconnected::Drop,
                EGRESS_PATIENCE,
            )?;
            if sent.is_none() {
                tracing::debug!(seq = batch.seq, "no egress subscriber; batch dropped");
                break;
            }
        }
        Ok(())
    }
}
