//! In-memory transport for tests: a queue in, a list out.
use std::collections::VecDeque;

use rfq_app::{BoxError, ClientCommand, EventBatch, Transport};

#[derive(Clone, Debug, Default)]
pub struct MemTransport {
    /// Commands waiting to be polled, front first.
    pub ingress: VecDeque<ClientCommand>,
    /// Every batch published so far, in order.
    pub egress: Vec<EventBatch>,
}

impl Transport for MemTransport {
    fn poll(&mut self, max: usize, sink: &mut dyn FnMut(ClientCommand)) {
        let n = max.min(self.ingress.len());
        self.ingress.drain(..n).for_each(sink);
    }

    fn publish(&mut self, batch: &EventBatch) -> Result<(), BoxError> {
        self.egress.push(batch.clone());
        Ok(())
    }
}
