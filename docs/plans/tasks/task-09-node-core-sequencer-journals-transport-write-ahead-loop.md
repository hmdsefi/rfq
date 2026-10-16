# Task 9: Node core (sequencer, journals, transport, write-ahead loop)

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Gate A ruling applied here** ([decisions](../../reviews/spec-review.md#decisions)): #4 a node that starts on a non-empty journal sequences `Command::Resume` as the first command of its first `poll`, stamped with that poll's wall clock, before any ingress command or `Tick`. Stamping it in `poll` (not in `start`) gives it the live time, so the engine's `resume_grace` counts from the restart; `start` itself still appends nothing on a restart. A fresh node (empty journal) never sends `Resume`.

**Files:**
- Create: `crates/rfq-node/Cargo.toml`, `src/lib.rs`, `src/sequencer.rs`, `src/journal.rs`, `src/transport.rs`, `src/node.rs`, `tests/node.rs`
- Modify: `Cargo.toml` (add the member and workspace deps)

**Interfaces:**
- Consumes: `rfq_core::{Engine, Command, SequencedCommand, Genesis, codec::*}`.
- Produces: `Sequencer::{resume(last_seq, last_time), stamp(wall_ms, Command) -> SequencedCommand}`; `trait Journal { append(&mut self, &SequencedCommand) -> anyhow::Result<u64>; wait_durable(&mut self, u64) -> anyhow::Result<()>; replay(&mut self, &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<()> }`; `MemJournal`, `FileJournal::open(path)`; `trait Transport { poll_ingress(&mut self, max: usize, sink: &mut dyn FnMut(&[u8])); publish_egress(&mut self, &[u8]) -> anyhow::Result<()> }`; `MemTransport { ingress: VecDeque<Vec<u8>>, egress: Vec<Vec<u8>> }`; `Node::{start(J, T, Genesis) -> anyhow::Result<Node<J,T>>, poll(wall_ms) -> anyhow::Result<usize>, engine(), state_hash(), into_parts() -> (J, T)}`. `poll` counts the `Resume` it sequences after a restart.

- [ ] **Step 1: Create the crate**

Add `"crates/rfq-node"` to the workspace `members`, and add `anyhow = "1"`, `tracing = "0.1"`, `tracing-subscriber = "0.3"`, `crc32fast = "1"`, `tempfile = "3"` to `[workspace.dependencies]`.

`crates/rfq-node/Cargo.toml`:
```toml
[package]
name = "rfq-node"
version = "0.1.0"
edition = "2021"

[dependencies]
rfq-core = { path = "../rfq-core" }
anyhow = { workspace = true }
tracing = { workspace = true }
crc32fast = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```
`crates/rfq-node/src/lib.rs`:
```rust
pub mod journal;
pub mod node;
pub mod sequencer;
pub mod transport;
```

- [ ] **Step 2: Write the failing tests**

`crates/rfq-node/tests/node.rs`:
```rust
use rfq_core::codec::{decode, encode, ClientCommand, EventBatch};
use rfq_core::testkit::{accounts::*, dollars, genesis};
use rfq_core::*;
use rfq_node::journal::{FileJournal, Journal, MemJournal};
use rfq_node::node::Node;
use rfq_node::sequencer::Sequencer;
use rfq_node::transport::MemTransport;

fn deposit(nonce: u64) -> Vec<u8> {
    encode(&ClientCommand { actor: OPERATOR, nonce, action: Action::Deposit { to: ALICE, amount: dollars(5) } })
}

fn batches(t: &MemTransport) -> Vec<EventBatch> {
    t.egress.iter().map(|b| decode(b).unwrap()).collect()
}

#[test]
fn sequencer_time_is_monotonic() {
    let mut s = Sequencer::resume(10, 1_000);
    assert_eq!(s.stamp(900, Command::Tick).time, 1_000);
    let sc = s.stamp(2_000, Command::Tick);
    assert_eq!((sc.seq, sc.time), (12, 2_000));
}

#[test]
fn fresh_node_writes_genesis_then_sequences_client_commands() {
    let mut t = MemTransport::default();
    t.ingress.push_back(deposit(1));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    assert_eq!(node.poll(5_000).unwrap(), 1);
    assert_eq!(node.engine().balance(ALICE).free, dollars(5));
    let (journal, transport) = node.into_parts();
    assert_eq!(journal.entries().len(), 2);
    assert!(matches!(journal.entries()[0].cmd, Command::Genesis(_)));
    let b = batches(&transport);
    assert_eq!(b[0].seq, 2);
    assert_eq!(b[0].origin, Some((OPERATOR, 1)));
}

#[test]
fn restart_replays_to_same_state_without_republishing() {
    let mut t = MemTransport::default();
    t.ingress.push_back(deposit(1));
    t.ingress.push_back(deposit(2));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    node.poll(5_000).unwrap();
    let hash = node.state_hash();
    let (journal, _) = node.into_parts();
    let restarted = Node::start(journal, MemTransport::default(), genesis(Config::default())).unwrap();
    assert_eq!(restarted.state_hash(), hash);
    assert_eq!(restarted.engine().last_seq(), 3);
    let (journal, transport) = restarted.into_parts();
    assert_eq!(journal.entries().len(), 3, "genesis not rewritten");
    assert!(transport.egress.is_empty(), "replay publishes nothing");
}

#[test]
fn restart_appends_resume() {
    let mut t = MemTransport::default();
    t.ingress.push_back(deposit(1));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    node.poll(5_000).unwrap();
    let (journal, mut t) = node.into_parts();
    assert!(!journal.entries().iter().any(|sc| sc.cmd == Command::Resume), "a fresh node does not resume");
    t.ingress.push_back(deposit(2));
    let mut node = Node::start(journal, t, genesis(Config::default())).unwrap();
    assert_eq!(node.poll(9_000).unwrap(), 2, "Resume, then the deposit");
    let (journal, _) = node.into_parts();
    let tail: Vec<_> = journal.entries()[2..].iter().map(|sc| (sc.seq, sc.time, sc.cmd == Command::Resume)).collect();
    assert_eq!(tail, vec![(3, 9_000, true), (4, 9_000, false)], "Resume first, stamped with the live clock");
}

#[test]
fn ticks_only_when_a_timer_is_due() {
    let mut node = Node::start(MemJournal::default(), MemTransport::default(), genesis(Config::default())).unwrap();
    assert_eq!(node.poll(1_000).unwrap(), 0, "no timers armed: nothing logged");
    // A market arms close/hard-deadline timers.
    let list = ClientCommand { actor: OPERATOR, nonce: 1, action: Action::ListMarket { id: MarketId(1), description: "x".into(), oracle: ORACLE, trading_close: 10_000, resolution_deadline: 20_000, dispute_window: 1 } };
    let (j, mut t) = node.into_parts();
    t.ingress.push_back(encode(&list));
    let mut node = Node::start(j, t, genesis(Config::default())).unwrap();
    node.poll(1_000).unwrap();
    assert_eq!(node.poll(9_999).unwrap(), 0);
    assert_eq!(node.poll(10_000).unwrap(), 1, "tick at trading_close");
}

#[test]
fn malformed_ingress_is_dropped() {
    let mut t = MemTransport::default();
    t.ingress.push_back(vec![1, 2, 3]);
    t.ingress.push_back(vec![]);
    t.ingress.push_back(deposit(1));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    assert_eq!(node.poll(5_000).unwrap(), 1);
    let (journal, _) = node.into_parts();
    assert_eq!(journal.entries().len(), 2);
}

#[test]
fn client_cannot_inject_system_commands() {
    let mut t = MemTransport::default();
    t.ingress.push_back(encode(&Command::Tick));
    t.ingress.push_back(encode(&Command::Resume));
    t.ingress.push_back(encode(&Command::Genesis(genesis(Config::default()))));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    node.poll(5_000).unwrap();
    let (journal, _) = node.into_parts();
    let system = journal.entries().iter().filter(|sc| !matches!(sc.cmd, Command::User { .. })).count();
    assert_eq!(system, 1, "only the node's own genesis");
}

#[test]
fn file_journal_truncates_torn_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    let mut j = FileJournal::open(&path).unwrap();
    let mut s = Sequencer::resume(0, 0);
    for _ in 0..3 {
        let pos = j.append(&s.stamp(1, Command::Tick)).unwrap();
        j.wait_durable(pos).unwrap();
    }
    drop(j);
    // Simulate a crash mid-write: append half a header.
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(&[7, 0]).unwrap();
    let mut j = FileJournal::open(&path).unwrap();
    let mut seen = Vec::new();
    j.replay(&mut |sc| seen.push(sc.seq)).unwrap();
    assert_eq!(seen, vec![1, 2, 3]);
    let pos = j.append(&s.stamp(2, Command::Tick)).unwrap();
    j.wait_durable(pos).unwrap();
    let mut seen = Vec::new();
    FileJournal::open(&path).unwrap().replay(&mut |sc| seen.push(sc.seq)).unwrap();
    assert_eq!(seen, vec![1, 2, 3, 4], "torn tail truncated, appends continue");
}

use std::io::Write;
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p rfq-node`
Expected: FAIL to compile (modules empty).

- [ ] **Step 4: Implement the sequencer and transport**

`crates/rfq-node/src/sequencer.rs`:
```rust
use rfq_core::{Command, SequencedCommand, Timestamp};

/// Assigns the total order. Time is the wall clock, but never moves backwards.
pub struct Sequencer {
    last_seq: u64,
    last_time: Timestamp,
}

impl Sequencer {
    pub fn resume(last_seq: u64, last_time: Timestamp) -> Self { Sequencer { last_seq, last_time } }

    pub fn stamp(&mut self, wall_ms: Timestamp, cmd: Command) -> SequencedCommand {
        self.last_seq += 1;
        self.last_time = self.last_time.max(wall_ms);
        SequencedCommand { seq: self.last_seq, time: self.last_time, cmd }
    }
}
```
`crates/rfq-node/src/transport.rs`:
```rust
use std::collections::VecDeque;

pub trait Transport {
    /// Hands up to `max` raw ingress messages to `sink`.
    fn poll_ingress(&mut self, max: usize, sink: &mut dyn FnMut(&[u8]));
    fn publish_egress(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
}

#[derive(Default)]
pub struct MemTransport {
    pub ingress: VecDeque<Vec<u8>>,
    pub egress: Vec<Vec<u8>>,
}

impl Transport for MemTransport {
    fn poll_ingress(&mut self, max: usize, sink: &mut dyn FnMut(&[u8])) {
        for _ in 0..max {
            match self.ingress.pop_front() {
                Some(m) => sink(&m),
                None => break,
            }
        }
    }

    fn publish_egress(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.egress.push(bytes.to_vec());
        Ok(())
    }
}
```

- [ ] **Step 5: Implement the journals**

`crates/rfq-node/src/journal.rs`:
```rust
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::Context;
use rfq_core::codec::{decode, encode};
use rfq_core::SequencedCommand;

/// The durable, ordered command log. State = replay(log).
pub trait Journal {
    /// Appends and returns a position to pass to `wait_durable`.
    fn append(&mut self, sc: &SequencedCommand) -> anyhow::Result<u64>;
    /// Blocks until everything up to `pos` survives a crash (write-ahead gate).
    fn wait_durable(&mut self, pos: u64) -> anyhow::Result<()>;
    /// Feeds every recorded command, in order, to `f`.
    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<()>;
}

#[derive(Default)]
pub struct MemJournal {
    entries: Vec<SequencedCommand>,
}

impl MemJournal {
    pub fn entries(&self) -> &[SequencedCommand] { &self.entries }
}

impl Journal for MemJournal {
    fn append(&mut self, sc: &SequencedCommand) -> anyhow::Result<u64> {
        self.entries.push(sc.clone());
        Ok(self.entries.len() as u64)
    }
    fn wait_durable(&mut self, _pos: u64) -> anyhow::Result<()> { Ok(()) }
    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<()> {
        self.entries.iter().cloned().for_each(f);
        Ok(())
    }
}

/// Append-only file: `[len: u32 LE][crc32: u32 LE][body: len bytes]` per record.
pub struct FileJournal {
    file: File,
    end: u64,
}

impl FileJournal {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).open(path).with_context(|| format!("open {path:?}"))?;
        let mut j = FileJournal { file, end: 0 };
        j.scan(&mut |_| {})?; // finds the last good record and truncates any torn tail
        Ok(j)
    }

    fn scan(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        let mut at = 0usize;
        while bytes.len() - at >= 8 {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let crc = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap());
            let Some(body) = bytes.get(at + 8..at + 8 + len) else { break };
            if crc32fast::hash(body) != crc {
                break;
            }
            let sc: SequencedCommand = decode(body).map_err(|e| anyhow::anyhow!("journal record at {at}: {e:?}"))?;
            f(sc);
            at += 8 + len;
        }
        if at as u64 != bytes.len() as u64 {
            tracing::warn!(good = at, total = bytes.len(), "truncating torn journal tail");
            self.file.set_len(at as u64)?;
        }
        self.end = at as u64;
        self.file.seek(SeekFrom::Start(self.end))?;
        Ok(())
    }
}

impl Journal for FileJournal {
    fn append(&mut self, sc: &SequencedCommand) -> anyhow::Result<u64> {
        let body = encode(sc);
        let mut rec = Vec::with_capacity(8 + body.len());
        rec.extend((body.len() as u32).to_le_bytes());
        rec.extend(crc32fast::hash(&body).to_le_bytes());
        rec.extend(&body);
        self.file.write_all(&rec)?;
        self.end += rec.len() as u64;
        Ok(self.end)
    }
    fn wait_durable(&mut self, _pos: u64) -> anyhow::Result<()> {
        self.file.sync_data()?;
        Ok(())
    }
    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<()> { self.scan(f) }
}
```

- [ ] **Step 6: Implement the node loop**

`crates/rfq-node/src/node.rs`:
```rust
use rfq_core::codec::{decode, encode, ClientCommand, EventBatch};
use rfq_core::{Command, Engine, Genesis, Timestamp};

use crate::journal::Journal;
use crate::sequencer::Sequencer;
use crate::transport::Transport;

const MAX_BATCH: usize = 256;

pub struct Node<J, T> {
    engine: Engine,
    journal: J,
    transport: T,
    sequencer: Sequencer,
    /// Set when `start` replayed a non-empty journal: the first `poll` sequences `Resume`.
    resume_pending: bool,
}

impl<J: Journal, T: Transport> Node<J, T> {
    /// Replays the journal (egress suppressed). On an empty journal, writes `genesis` as seq 1.
    pub fn start(mut journal: J, transport: T, genesis: Genesis) -> anyhow::Result<Self> {
        let mut engine = Engine::new();
        let (mut last_seq, mut last_time) = (0, 0);
        journal.replay(&mut |sc| {
            last_seq = sc.seq;
            last_time = sc.time;
            engine.apply(&sc);
        })?;
        let mut sequencer = Sequencer::resume(last_seq, last_time);
        let resume_pending = last_seq > 0;
        if last_seq == 0 {
            let sc = sequencer.stamp(0, Command::Genesis(genesis));
            let pos = journal.append(&sc)?;
            journal.wait_durable(pos)?;
            engine.apply(&sc);
        }
        tracing::info!(seq = engine.last_seq(), hash = %hex(&engine.state_hash()), "replayed");
        Ok(Node { engine, journal, transport, sequencer, resume_pending })
    }

    /// One loop iteration: sequence → append → wait durable → apply → publish.
    pub fn poll(&mut self, wall_ms: Timestamp) -> anyhow::Result<usize> {
        let mut incoming = Vec::new();
        self.transport.poll_ingress(MAX_BATCH, &mut |bytes| match decode::<ClientCommand>(bytes) {
            Ok(c) => incoming.push(c),
            Err(e) => tracing::warn!(?e, "dropping undecodable ingress message"),
        });
        let mut stamped = Vec::new();
        if std::mem::take(&mut self.resume_pending) {
            // First command after a restart, stamped with the live clock. The engine handles it
            // before firing due timers, so downtime can't eat a dispute window (engine.md §6.3).
            stamped.push(self.sequencer.stamp(wall_ms, Command::Resume));
        }
        stamped.extend(incoming.into_iter().map(|c| self.sequencer.stamp(wall_ms, c.into_command())));
        if stamped.is_empty() && self.engine.next_timer().is_some_and(|t| t <= wall_ms) {
            stamped.push(self.sequencer.stamp(wall_ms, Command::Tick));
        }
        if stamped.is_empty() {
            return Ok(0);
        }
        let mut pos = 0;
        for sc in &stamped {
            pos = self.journal.append(sc)?;
        }
        self.journal.wait_durable(pos)?;
        for sc in &stamped {
            let events = self.engine.apply(sc);
            let origin = match &sc.cmd {
                Command::User { actor, nonce, .. } => Some((*actor, *nonce)),
                _ => None,
            };
            if origin.is_some() || !events.is_empty() {
                self.transport.publish_egress(&encode(&EventBatch { seq: sc.seq, origin, events }))?;
            }
        }
        Ok(stamped.len())
    }

    pub fn engine(&self) -> &Engine { &self.engine }
    pub fn state_hash(&self) -> [u8; 32] { self.engine.state_hash() }
    pub fn into_parts(self) -> (J, T) { (self.journal, self.transport) }
}

pub fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p rfq-node`
Expected: 8 passed.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/rfq-node
git commit -m "feat(node): sequencer, file/memory journals, write-ahead node loop with replay"
```

