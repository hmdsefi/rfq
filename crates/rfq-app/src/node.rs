//! The node service: startup replay, then the write-ahead loop
//! sequence → append → wait durable → apply → publish (architecture §4, transport spec "Live loop").
use rfq_domain::{Command, Engine, Genesis, SequencedCommand, Timestamp};

use crate::error::{NodeError, SequenceError};
use crate::ports::{Journal, Transport};
use crate::protocol::{BatchKind, ClientCommand, EventBatch};
use crate::sequencer::Sequencer;

/// Most ingress commands taken per `poll`. Bounds the batch that shares one durability wait.
pub const MAX_BATCH: usize = 256;

/// A wall-clock gap between two polls longer than this is treated like a restart: the next poll
/// sequences `Resume` first, so the stall cannot eat a dispute window.
///
/// The live loop polls about every millisecond, so a gap this long means the process was suspended
/// (`SIGSTOP`, `docker pause`, host sleep, VM migration), a step was blocked (for example on a
/// back-pressured egress), or the clock jumped forward. Ingress was not read during the gap, so
/// a holder's dispute may be waiting behind timers that are already due. A backward clock step is
/// not a gap: the sequencer clamps time so it never goes back.
pub const STALL_GAP_MS: Timestamp = 1_000;

/// One engine behind a journal and a transport. Sees typed values only, never bytes.
pub struct Node<J, T> {
    engine: Engine,
    journal: J,
    transport: T,
    sequencer: Sequencer,
    /// Set when `start` replayed a non-empty journal; the first poll takes it and sequences
    /// `Resume` first. A stall never sets it: `step` derives a local `stalled` flag from
    /// `last_poll_wall` and ORs it in, so a stall can add a `Resume` without touching this field.
    resume_pending: bool,
    /// The wall clock of the previous poll (`None` before the first). Not `engine.now()`: idle
    /// polls log nothing, so the engine's clock legitimately falls behind the wall clock.
    last_poll_wall: Option<Timestamp>,
    /// Latched by the first failed `poll`. A failure can leave the sequencer ahead of the journal
    /// or the journal ahead of the engine, so continuing could diverge from replay.
    halted: bool,
}

impl<J: Journal, T: Transport> Node<J, T> {
    /// Replays the journal into a new engine with egress suppressed (nothing is published).
    ///
    /// The replayed `seq`s must be exactly `1, 2, 3, ...`; a duplicate or a gap fails with
    /// [`NodeError::Journal`] holding a [`SequenceError`], whatever the journal adapter.
    ///
    /// On an empty journal, durably writes `genesis` as seq 1. On a non-empty journal `genesis` is
    /// ignored (the journal's own `Genesis` wins, so replay always uses the recorded config) and
    /// nothing is appended here: the first [`poll`](Node::poll) sequences `Resume`.
    pub fn start(mut journal: J, transport: T, genesis: Genesis) -> Result<Self, NodeError> {
        let mut engine = Engine::new();
        let mut last_seq = 0;
        let mut out_of_sequence = None;
        journal
            .replay(&mut |sc| {
                if out_of_sequence.is_some() {
                    return;
                }
                if sc.seq != last_seq + 1 {
                    out_of_sequence = Some(SequenceError {
                        expected: last_seq + 1,
                        found: sc.seq,
                    });
                    return;
                }
                last_seq = sc.seq;
                engine.apply(&sc);
            })
            .map_err(NodeError::Journal)?;
        if let Some(e) = out_of_sequence {
            return Err(NodeError::Journal(Box::new(e)));
        }
        // The engine's clock is the largest stamped time, i.e. the last one (stamps are monotonic).
        let mut sequencer = Sequencer::resume(engine.last_seq(), engine.now());
        let resume_pending = engine.last_seq() > 0;
        if !resume_pending {
            let sc = sequencer.stamp(0, Command::Genesis(genesis));
            let pos = journal.append(&sc).map_err(NodeError::Journal)?;
            journal.wait_durable(pos).map_err(NodeError::Journal)?;
            engine.apply(&sc);
        }
        tracing::info!(seq = engine.last_seq(), hash = %hex(&engine.state_hash()), "replayed");
        Ok(Node {
            engine,
            journal,
            transport,
            sequencer,
            resume_pending,
            last_poll_wall: None,
            halted: false,
        })
    }

    /// One loop iteration. Returns how many commands it sequenced, counting `Resume` and `Tick`;
    /// `0` means nothing was logged.
    ///
    /// 1. After a restart, or when `wall_ms` is more than [`STALL_GAP_MS`] past the previous
    ///    call's, sequences `Resume` before anything else, stamped with `wall_ms`.
    /// 2. Takes up to [`MAX_BATCH`] client commands from the transport and stamps them.
    /// 3. If there was nothing to sequence and the engine's earliest timer is due, sequences one
    ///    `Tick` (any command fires due timers, so idle periods add nothing to the log).
    /// 4. Appends the batch, waits once for it to be durable, applies it, then publishes one
    ///    `EventBatch` per command that has an origin or events, and one for every `Resume`.
    ///
    /// Any `Err` is fatal for this node: every later call returns [`NodeError::Halted`] without
    /// touching a port. Drop the node and [`start`](Node::start) again from the journal.
    pub fn poll(&mut self, wall_ms: Timestamp) -> Result<usize, NodeError> {
        if self.halted {
            return Err(NodeError::Halted);
        }
        let result = self.step(wall_ms);
        self.halted = result.is_err();
        result
    }

    fn step(&mut self, wall_ms: Timestamp) -> Result<usize, NodeError> {
        let stalled = self
            .last_poll_wall
            .replace(wall_ms)
            .is_some_and(|last| wall_ms.saturating_sub(last) > STALL_GAP_MS);
        let mut batch: Vec<SequencedCommand> = Vec::new();
        if std::mem::take(&mut self.resume_pending) || stalled {
            // Stamped with the live clock, so the engine's `resume_grace` counts from the restart
            // or the end of the stall.
            batch.push(self.sequencer.stamp(wall_ms, Command::Resume));
        }
        let sequencer = &mut self.sequencer;
        self.transport
            .poll(MAX_BATCH, &mut |c: ClientCommand| {
                batch.push(sequencer.stamp(wall_ms, c.into_command()));
            })
            .map_err(NodeError::Transport)?;
        if batch.is_empty() && self.engine.next_timer().is_some_and(|t| t <= wall_ms) {
            batch.push(self.sequencer.stamp(wall_ms, Command::Tick));
        }
        if batch.is_empty() {
            return Ok(0);
        }

        let mut pos = 0;
        for sc in &batch {
            pos = self.journal.append(sc).map_err(NodeError::Journal)?;
        }
        // Write-ahead gate: nothing below runs until the whole batch is durable.
        self.journal.wait_durable(pos).map_err(NodeError::Journal)?;

        // Apply the whole batch before publishing any of it, so a transport failure can never
        // leave the in-memory state behind the journal.
        let out: Vec<EventBatch> = batch
            .iter()
            .filter_map(|sc| {
                let events = self.engine.apply(sc);
                let balances = self.engine.take_touched_balances();
                let (kind, origin) = match &sc.cmd {
                    Command::User { actor, nonce, .. } => (BatchKind::User, Some((*actor, *nonce))),
                    Command::Tick => (BatchKind::Tick, None),
                    Command::Resume => (BatchKind::Resume, None),
                    Command::Genesis(_) => (BatchKind::Genesis, None),
                };
                // Every Resume is published, so clients see each restart and stall (gateway.md §2.3).
                (origin.is_some() || !events.is_empty() || kind == BatchKind::Resume).then_some(EventBatch {
                    seq: sc.seq,
                    time: sc.time,
                    kind,
                    origin,
                    events,
                    balances,
                })
            })
            .collect();
        for b in &out {
            self.transport.publish(b).map_err(NodeError::Transport)?;
        }
        Ok(batch.len())
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn state_hash(&self) -> [u8; 32] {
        self.engine.state_hash()
    }

    /// Stops the node and hands back its adapters (tests restart a node on the same journal).
    pub fn into_parts(self) -> (J, T) {
        (self.journal, self.transport)
    }
}

/// Lower-case hex, for logging state hashes.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
