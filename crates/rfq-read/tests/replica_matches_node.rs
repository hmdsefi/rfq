//! The replica equals the node (docs/specs/read-api.md §7): fed the journal of a node that ran a
//! scenario, it reaches the node's `state_hash` and regenerates every batch the node published,
//! plus the ones the node applied without publishing (`Genesis`).
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use rfq_app::{BatchKind, BoxError, ClientCommand, EventBatch, Node, Transport};
use rfq_domain::{AccountId, Command, Emitted, Event, SequencedCommand, Timestamp};
use rfq_infra::journal::MemJournal;
use rfq_infra::transport::MemTransport;
use rfq_read::{Replica, ReplicaError, TraceEntry};
use rfq_testkit::genesis;
use rfq_testkit::scenarios::{Ctx, Scenario, Step, StepKind, all, contains_in_order, demo_config};

/// The scenario's start, in engine milliseconds.
const T0: Timestamp = 1_000_000;

/// A `MemTransport` the test keeps a handle to, so it can queue commands between polls and read
/// the egress after the node is gone (as `crates/rfq-infra/tests/node.rs` does).
#[derive(Clone, Default)]
struct SharedTransport(Rc<RefCell<MemTransport>>);

impl SharedTransport {
    fn push(&self, c: ClientCommand) {
        self.0.borrow_mut().ingress.push_back(c);
    }
    fn egress_len(&self) -> usize {
        self.0.borrow().egress.len()
    }
    fn egress_from(&self, start: usize) -> Vec<EventBatch> {
        self.0.borrow().egress[start..].to_vec()
    }
}

impl Transport for SharedTransport {
    fn poll(&mut self, max: usize, sink: &mut dyn FnMut(ClientCommand)) -> Result<(), BoxError> {
        self.0.borrow_mut().poll(max, sink)
    }
    fn publish(&mut self, batch: &EventBatch) -> Result<(), BoxError> {
        self.0.borrow_mut().publish(batch)
    }
}

type TestNode = Node<MemJournal, SharedTransport>;

/// Drives a `Node` through a scenario's steps the way a live gateway would.
struct Run {
    node: Option<TestNode>,
    transport: SharedTransport,
    ctx: Ctx,
    /// Each actor's last nonce (they start at 1).
    nonces: BTreeMap<AccountId, u64>,
    /// Every command the test put on ingress, by `(actor, nonce)`.
    pushed: BTreeMap<(AccountId, u64), ClientCommand>,
}

impl Run {
    fn new() -> Self {
        let transport = SharedTransport::default();
        let node = Node::start(MemJournal::default(), transport.clone(), genesis(demo_config())).unwrap();
        Run {
            node: Some(node),
            transport,
            ctx: Ctx::new(T0),
            nonces: BTreeMap::new(),
            pushed: BTreeMap::new(),
        }
    }

    fn node(&mut self) -> &mut TestNode {
        self.node.as_mut().expect("the node is running")
    }

    /// One poll. The events of every batch it published go to the context, so an `Accept` finds its
    /// picks in the `RequestPresented` that a `Tick` poll (or a stall's `Resume`) published.
    fn poll(&mut self, wall: Timestamp) {
        let before = self.transport.egress_len();
        self.node().poll(wall).unwrap();
        for batch in self.transport.egress_from(before) {
            self.ctx.observe(&batch.events);
        }
    }

    fn step(&mut self, step: &Step) {
        let wall = T0 + step.at;
        match &step.kind {
            StepKind::Send { label, actor, action } => {
                let nonce = {
                    let n = self.nonces.entry(*actor).or_insert(0);
                    *n += 1;
                    *n
                };
                let cmd = ClientCommand {
                    actor: *actor,
                    nonce,
                    action: action(&self.ctx),
                };
                self.pushed.insert((*actor, nonce), cmd.clone());
                self.transport.push(cmd);
                self.poll(wall);
                // The poll may also have sequenced a stall's `Resume` before it, never after: the
                // command is the last one the engine applied.
                let seq = self.node().engine().last_seq();
                self.ctx.record_label(label, seq);
            }
            StepKind::Tick => self.poll(wall),
        }
    }

    /// Stops the node and starts a new one on the same journal and transport: its next poll
    /// sequences a `Resume`.
    fn restart(&mut self) {
        let (journal, transport) = self.node.take().expect("the node is running").into_parts();
        self.node = Some(Node::start(journal, transport, genesis(demo_config())).unwrap());
    }

    /// Polls once more, late enough that every timer has fired.
    fn finish(&mut self) {
        self.poll(T0 + 20_000);
    }
}

/// Everything the node left behind.
struct Outcome {
    state_hash: [u8; 32],
    journal: Vec<SequencedCommand>,
    published: Vec<EventBatch>,
    pushed: BTreeMap<(AccountId, u64), ClientCommand>,
}

fn stop(mut run: Run) -> Outcome {
    let node = run.node.take().expect("the node is running");
    let state_hash = node.state_hash();
    let (journal, _) = node.into_parts();
    Outcome {
        state_hash,
        journal: journal.entries().to_vec(),
        published: run.transport.egress_from(0),
        pushed: run.pushed,
    }
}

fn replica_of(journal: &[SequencedCommand]) -> Replica {
    let mut replica = Replica::new();
    for sc in journal {
        replica.apply(sc).unwrap();
    }
    replica
}

fn trace_of(replica: &Replica) -> BTreeMap<u64, &TraceEntry> {
    replica.trace_after(0).map(|e| (e.batch.seq, e)).collect()
}

/// The replica fed `o`'s journal equals the node that wrote it.
fn assert_replica_matches(name: &str, o: &Outcome) {
    let replica = replica_of(&o.journal);
    assert_eq!(
        replica.last_seq(),
        u64::try_from(o.journal.len()).unwrap(),
        "{name}: the replica applied the whole journal"
    );
    assert_eq!(replica.state_hash(), o.state_hash, "{name}: state hash");

    let trace = trace_of(&replica);
    assert!(!o.published.is_empty(), "{name}: the node published something");
    for b in &o.published {
        let entry = trace
            .get(&b.seq)
            .unwrap_or_else(|| panic!("{name}: the trace has no entry for published seq {}", b.seq));
        assert_eq!(&entry.batch, b, "{name}: batch at seq {}", b.seq);
    }

    // The node applied Genesis without publishing it; the replica keeps it.
    let first = replica.trace_after(0).next().expect("the trace is not empty");
    assert_eq!(first.batch.seq, 1, "{name}");
    assert_eq!(first.batch.kind, BatchKind::Genesis, "{name}");
    assert_eq!(
        first.batch.events,
        vec![Emitted::public(Event::Initialized)],
        "{name}"
    );
    assert_eq!(first.command, None, "{name}: Genesis has no user command");
    assert!(
        o.published.iter().all(|b| b.seq != 1),
        "{name}: the node did not publish Genesis, so the trace holds more than egress"
    );

    // A user batch carries the command the test pushed; a system batch carries none.
    let mut user_entries = 0;
    for e in trace.values() {
        match e.batch.origin {
            Some(key) => {
                user_entries += 1;
                assert_eq!(e.batch.kind, BatchKind::User, "{name}");
                assert_eq!(
                    e.command.as_ref(),
                    Some(&o.pushed[&key]),
                    "{name}: command behind seq {}",
                    e.batch.seq
                );
            }
            None => assert_eq!(e.command, None, "{name}: seq {} is a system batch", e.batch.seq),
        }
    }
    assert_eq!(
        user_entries,
        o.pushed.len(),
        "{name}: every pushed command has its batch"
    );
}

fn scenario(name: &str) -> Scenario {
    all()
        .into_iter()
        .find(|s| s.name == name)
        .expect("scenario exists")
}

#[test]
fn the_replica_matches_the_node_for_every_scenario() {
    let scenarios = all();
    assert!(scenarios.len() >= 8, "the multi-leg scenarios are included");
    for s in &scenarios {
        let mut run = Run::new();
        for step in &s.steps {
            run.step(step);
        }
        run.finish();
        let o = stop(run);
        // The node really played the scenario, so the comparison below is over its full story.
        let events: Vec<Emitted> = o.published.iter().flat_map(|b| b.events.clone()).collect();
        assert_eq!(contains_in_order(&events, &s.expect), Ok(()), "{}", s.name);
        assert_replica_matches(s.name, &o);
    }
}

#[test]
fn replica_matches_a_node_restarted_mid_scenario() {
    let happy = scenario("happy_path");
    let after_accept = 1 + happy
        .steps
        .iter()
        .position(|s| matches!(&s.kind, StepKind::Send { label: "accept", .. }))
        .expect("happy_path accepts");

    let mut run = Run::new();
    for step in &happy.steps[..after_accept] {
        run.step(step);
    }
    let before_restart = run.node().engine().last_seq();
    run.restart();
    for step in &happy.steps[after_accept..] {
        run.step(step);
    }
    run.finish();
    let o = stop(run);
    let events: Vec<Emitted> = o.published.iter().flat_map(|b| b.events.clone()).collect();
    assert_eq!(
        contains_in_order(&events, &happy.expect),
        Ok(()),
        "the scenario ran through"
    );

    // The restart's `Resume` is the first command after the replayed journal, and it was published.
    let resume_seq = before_restart + 1;
    let resume = &o.journal[usize::try_from(before_restart).unwrap()];
    assert_eq!((resume.seq, &resume.cmd), (resume_seq, &Command::Resume));
    assert!(
        o.published
            .iter()
            .any(|b| b.seq == resume_seq && b.kind == BatchKind::Resume),
        "the node published the Resume it sequenced after restarting"
    );

    assert_replica_matches("happy_path restarted", &o);
}

#[test]
fn apply_refuses_a_gap_and_a_duplicate_and_changes_nothing() {
    let genesis_cmd = SequencedCommand {
        seq: 1,
        time: 0,
        cmd: Command::Genesis(genesis(demo_config())),
    };
    let resume = |seq| SequencedCommand {
        seq,
        time: 1_000,
        cmd: Command::Resume,
    };

    let mut replica = Replica::new();
    assert_eq!(replica.last_seq(), 0);
    assert_eq!(
        replica.apply(&resume(2)),
        Err(ReplicaError::OutOfSequence {
            expected: 1,
            found: 2
        }),
        "a fresh replica starts at seq 1"
    );
    assert_eq!(replica.last_seq(), 0);
    assert_eq!(replica.trace_after(0).count(), 0);

    replica.apply(&genesis_cmd).unwrap();
    let hash = replica.state_hash();

    assert_eq!(
        replica.apply(&resume(3)),
        Err(ReplicaError::OutOfSequence {
            expected: 2,
            found: 3
        }),
        "a gap"
    );
    assert_eq!(
        replica.apply(&resume(1)),
        Err(ReplicaError::OutOfSequence {
            expected: 2,
            found: 1
        }),
        "a duplicate"
    );
    assert_eq!(replica.last_seq(), 1);
    assert_eq!(replica.state_hash(), hash);
    assert_eq!(
        replica.trace_after(0).count(),
        1,
        "no batch was kept for a refused command"
    );

    replica.apply(&resume(2)).unwrap();
    assert_eq!(replica.last_seq(), 2, "the next seq in order still applies");
}

#[test]
fn trace_after_yields_the_entries_past_a_seq_in_ascending_order() {
    let happy = scenario("happy_path");
    let mut run = Run::new();
    for step in &happy.steps {
        run.step(step);
    }
    run.finish();
    let o = stop(run);
    let replica = replica_of(&o.journal);

    let all_seqs: Vec<u64> = replica.trace_after(0).map(|e| e.batch.seq).collect();
    assert_eq!(all_seqs.first(), Some(&1));
    assert!(all_seqs.windows(2).all(|w| w[0] < w[1]), "ascending, no repeats");

    let cut = all_seqs[all_seqs.len() / 2];
    let past: Vec<u64> = replica.trace_after(cut).map(|e| e.batch.seq).collect();
    let expected: Vec<u64> = all_seqs.iter().copied().filter(|s| *s > cut).collect();
    assert_eq!(past, expected, "seq > after_seq, the cut itself excluded");
    assert_eq!(replica.trace_after(replica.last_seq()).count(), 0);
    assert_eq!(replica.trace_after(u64::MAX).count(), 0);
}
