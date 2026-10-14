//! Ports: the two traits through which the node talks to the outside world.
//!
//! Both carry typed values, never bytes. Decoding, size limits and malformed-message handling
//! belong to the adapters in `rfq-infra`. Because [`Transport::poll`] yields [`ClientCommand`],
//! which cannot express `Genesis`, `Tick` or `Resume`, a client cannot inject a system command:
//! the type makes it impossible, so no runtime check is needed. Only the node creates those.
use rfq_domain::SequencedCommand;

use crate::error::BoxError;
use crate::protocol::{ClientCommand, EventBatch};

/// The durable, totally ordered command log. Engine state is `replay(log)`.
pub trait Journal {
    /// Appends `sc` after every command appended before it, and returns a position to pass to
    /// [`wait_durable`](Journal::wait_durable). Positions grow with each append.
    ///
    /// Returning `Ok` does **not** mean durable: the record may still be lost in a crash.
    fn append(&mut self, sc: &SequencedCommand) -> Result<u64, BoxError>;

    /// The write-ahead gate. Returns `Ok` only once every command appended up to and including
    /// `pos` survives a crash. The node applies and publishes nothing for a command before this
    /// returns. On `Err`, durability of those commands is unknown and the node must stop.
    fn wait_durable(&mut self, pos: u64) -> Result<(), BoxError>;

    /// Feeds every recorded command to `f` exactly once, in append (= `seq`) order. The node
    /// calls this once at startup, before any `append`. Records lost to a crash before
    /// `wait_durable` returned may be missing (an adapter drops a torn tail); later appends
    /// continue after the last replayed record.
    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> Result<(), BoxError>;
}

/// Client ingress and event egress.
pub trait Transport {
    /// Hands up to `max` client commands to `sink`, in arrival order, without blocking. Bytes that
    /// do not decode to a [`ClientCommand`] are dropped by the adapter and never reach `sink`; a
    /// bad message is not an error. `Err` means the transport itself failed (for example its
    /// ingress subscription closed), which the node treats as fatal.
    fn poll(&mut self, max: usize, sink: &mut dyn FnMut(ClientCommand)) -> Result<(), BoxError>;

    /// Publishes one batch. The node calls this in `seq` order, and only after the batch's command
    /// is durable in the journal (write-ahead rule).
    fn publish(&mut self, batch: &EventBatch) -> Result<(), BoxError>;
}
