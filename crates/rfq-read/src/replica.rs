//! The replica: an [`Engine`] fed the journal's commands, and the trace of batches it regenerates.
use std::collections::BTreeMap;
use std::ops::Bound;

use rfq_app::{ClientCommand, EventBatch, batch_of};
use rfq_domain::{Command, Engine, SequencedCommand};
use serde::Serialize;

/// One command's batch, as the node would have published it, with the command behind it.
///
/// The batch is unfiltered (what the admin view sees); a gateway filters it per viewer.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TraceEntry {
    pub batch: EventBatch,
    /// The user command behind the batch (`None` for Genesis, Tick, Resume).
    pub command: Option<ClientCommand>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReplicaError {
    #[error("journal out of sequence: expected seq {expected}, got {found}")]
    OutOfSequence { expected: u64, found: u64 },
}

/// A read-only copy of the engine, advanced only by [`apply`](Replica::apply).
///
/// It runs the journal's commands through the same steps as the node's loop (apply, take the
/// touched balances, build the batch with `batch_of`), so at the same `seq` its state hash equals
/// the node's and every batch the node published is identical in the trace.
///
/// The trace keeps every batch, in memory, so it grows with the journal (read-api.md §6).
#[derive(Clone, Debug, Default)]
pub struct Replica {
    engine: Engine,
    /// Indexed by `seq`: a batch exists only for the commands `batch_of` publishes (a user command,
    /// a `Resume`, any command that emitted events), so the keys have gaps.
    trace: BTreeMap<u64, TraceEntry>,
}

impl Replica {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies the next journal command; `sc.seq` must be `last_seq() + 1`. Builds the batch with
    /// `batch_of` (after `take_touched_balances`) and keeps it with its command.
    ///
    /// A command out of sequence is refused and nothing changes: the caller decides whether that
    /// means a gap to rebuild from or a new journal.
    pub fn apply(&mut self, sc: &SequencedCommand) -> Result<(), ReplicaError> {
        let expected = self.last_seq() + 1;
        if sc.seq != expected {
            return Err(ReplicaError::OutOfSequence {
                expected,
                found: sc.seq,
            });
        }
        let events = self.engine.apply(sc);
        let balances = self.engine.take_touched_balances();
        if let Some(batch) = batch_of(sc, events, balances) {
            let command = match &sc.cmd {
                Command::User { actor, nonce, action } => Some(ClientCommand {
                    actor: *actor,
                    nonce: *nonce,
                    action: action.clone(),
                }),
                Command::Genesis(_) | Command::Tick | Command::Resume => None,
            };
            self.trace.insert(sc.seq, TraceEntry { batch, command });
        }
        Ok(())
    }

    /// The `seq` of the last command applied; 0 for a replica that has applied none.
    pub fn last_seq(&self) -> u64 {
        self.engine.last_seq()
    }

    pub fn state_hash(&self) -> [u8; 32] {
        self.engine.state_hash()
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Trace entries with `seq > after_seq`, ascending.
    pub fn trace_after(&self, after_seq: u64) -> impl Iterator<Item = &TraceEntry> {
        self.trace
            .range((Bound::Excluded(after_seq), Bound::Unbounded))
            .map(|(_, entry)| entry)
    }
}
