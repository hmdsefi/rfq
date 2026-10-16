//! Node behaviour over the in-memory adapters: startup, replay, write-ahead order, ticks.
use std::cell::RefCell;
use std::rc::Rc;

use rfq_app::node::STALL_GAP_MS;
use rfq_app::{
    BatchKind, BoxError, ClientCommand, EventBatch, Journal, Node, NodeError, SequenceError, Transport,
};
use rfq_domain::*;
use rfq_infra::journal::{FileJournal, MemJournal};
use rfq_infra::transport::MemTransport;
use rfq_testkit::scenarios::demo_config;
use rfq_testkit::{accounts::*, dollars, genesis, names, price};

fn deposit(nonce: u64) -> ClientCommand {
    ClientCommand {
        actor: OPERATOR,
        nonce,
        action: Action::Deposit {
            to: ALICE,
            amount: dollars(5),
        },
    }
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
    let b = &transport.egress;
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
    assert!(
        !journal.entries().iter().any(|sc| sc.cmd == Command::Resume),
        "a fresh node does not resume"
    );
    t.ingress.push_back(deposit(2));
    let mut node = Node::start(journal, t, genesis(Config::default())).unwrap();
    assert_eq!(node.poll(9_000).unwrap(), 2, "Resume, then the deposit");
    let (journal, _) = node.into_parts();
    let tail: Vec<_> = journal.entries()[2..]
        .iter()
        .map(|sc| (sc.seq, sc.time, sc.cmd == Command::Resume))
        .collect();
    assert_eq!(
        tail,
        vec![(3, 9_000, true), (4, 9_000, false)],
        "Resume first, stamped with the live clock"
    );
}

#[test]
fn ticks_only_when_a_timer_is_due() {
    let mut node = Node::start(
        MemJournal::default(),
        MemTransport::default(),
        genesis(Config::default()),
    )
    .unwrap();
    assert_eq!(node.poll(1_000).unwrap(), 0, "no timers armed: nothing logged");
    // A market arms close/hard-deadline timers.
    let list = ClientCommand {
        actor: OPERATOR,
        nonce: 1,
        action: Action::ListMarket {
            id: MarketId(1),
            description: "x".into(),
            oracle: ORACLE,
            trading_close: 2_000,
            resolution_deadline: 20_000,
            dispute_window: 1,
        },
    };
    let (j, mut t) = node.into_parts();
    t.ingress.push_back(list);
    let mut node = Node::start(j, t, genesis(Config::default())).unwrap();
    node.poll(1_000).unwrap();
    // Polls stay within `STALL_GAP_MS` of each other, so no stall `Resume` is sequenced.
    assert_eq!(node.poll(1_999).unwrap(), 0);
    assert_eq!(node.poll(2_000).unwrap(), 1, "tick at trading_close");
}

#[test]
fn clock_never_goes_back_across_a_restart() {
    let mut t = MemTransport::default();
    t.ingress.push_back(deposit(1));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    node.poll(5_000).unwrap();
    let (journal, mut t) = node.into_parts();
    t.ingress.push_back(deposit(2));
    let mut node = Node::start(journal, t, genesis(Config::default())).unwrap();
    assert_eq!(node.poll(1_000).unwrap(), 2, "Resume, then the deposit");
    let (journal, _) = node.into_parts();
    let times: Vec<_> = journal.entries()[2..].iter().map(|sc| sc.time).collect();
    assert_eq!(
        times,
        vec![5_000, 5_000],
        "wall clock behind the journal: time is clamped"
    );
}

#[test]
fn restart_from_a_file_journal_reaches_the_same_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    let mut t = MemTransport::default();
    t.ingress.extend([deposit(1), deposit(2)]);
    let mut node = Node::start(FileJournal::open(&path).unwrap(), t, genesis(Config::default())).unwrap();
    node.poll(5_000).unwrap();
    let hash = node.state_hash();
    drop(node);

    let restarted = Node::start(
        FileJournal::open(&path).unwrap(),
        MemTransport::default(),
        genesis(Config::default()),
    )
    .unwrap();
    assert_eq!(restarted.state_hash(), hash);
    assert_eq!(restarted.engine().last_seq(), 3);
    drop(restarted);

    let mut recorded = Vec::new();
    FileJournal::open(&path)
        .unwrap()
        .replay(&mut |sc| recorded.push((sc.seq, matches!(sc.cmd, Command::Genesis(_)))))
        .unwrap();
    assert_eq!(
        recorded,
        vec![(1, true), (2, false), (3, false)],
        "genesis not rewritten"
    );
}

// ---- Stalls while running: a long gap between polls is treated like a restart. ----

/// A `MemTransport` the test keeps a handle to, so it can queue commands between polls.
#[derive(Clone, Default)]
struct SharedTransport(Rc<RefCell<MemTransport>>);

impl SharedTransport {
    fn push(&self, c: ClientCommand) {
        self.0.borrow_mut().ingress.push_back(c);
    }
    fn batches(&self) -> Vec<EventBatch> {
        self.0.borrow().egress.clone()
    }
    fn egress_names(&self) -> Vec<&'static str> {
        let t = self.0.borrow();
        t.egress
            .iter()
            .flat_map(|b| b.events.iter().map(|e| e.event.name()))
            .collect()
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

/// When the live node last polled in the stall tests.
const LAST_POLL: Timestamp = 5_000;

/// A fresh node (so no restart `Resume` is pending) that sequenced deposit 1 at [`LAST_POLL`].
fn live_node() -> (Node<MemJournal, SharedTransport>, SharedTransport) {
    let t = SharedTransport::default();
    t.push(deposit(1));
    let mut node = Node::start(MemJournal::default(), t.clone(), genesis(Config::default())).unwrap();
    assert_eq!(node.poll(LAST_POLL).unwrap(), 1);
    (node, t)
}

#[test]
fn a_stall_longer_than_the_gap_sequences_resume_before_the_batch() {
    let (mut node, t) = live_node();
    t.push(deposit(2));
    let wall = LAST_POLL + STALL_GAP_MS + 1;
    assert_eq!(node.poll(wall).unwrap(), 2, "Resume, then the deposit");
    let (journal, _) = node.into_parts();
    let tail: Vec<_> = journal.entries()[2..]
        .iter()
        .map(|sc| (sc.seq, sc.time, sc.cmd == Command::Resume))
        .collect();
    assert_eq!(
        tail,
        vec![(3, wall, true), (4, wall, false)],
        "Resume first, stamped with the live clock, exactly as after a restart"
    );
}

#[test]
fn a_stall_with_nothing_queued_still_sequences_resume() {
    let (mut node, _) = live_node();
    assert_eq!(
        node.poll(LAST_POLL + STALL_GAP_MS + 1).unwrap(),
        1,
        "Resume alone"
    );
    assert_eq!(
        node.poll(LAST_POLL + STALL_GAP_MS + 2).unwrap(),
        0,
        "the next normal poll logs nothing"
    );
}

#[test]
fn a_normal_poll_gap_does_not_resume() {
    let (mut node, t) = live_node();
    t.push(deposit(2));
    assert_eq!(
        node.poll(LAST_POLL + STALL_GAP_MS).unwrap(),
        1,
        "the deposit only"
    );
    assert_eq!(
        node.poll(LAST_POLL - 4_000).unwrap(),
        0,
        "a backward clock step is not a stall"
    );
    let (journal, _) = node.into_parts();
    assert!(!journal.entries().iter().any(|sc| sc.cmd == Command::Resume));
}

/// The attack in the final security review (M3): a live node that stalls past a dispute window
/// must not let the window's timer fire ahead of a dispute that was waiting on ingress.
#[test]
fn a_stall_cannot_eat_a_dispute_window() {
    let market = MarketId(1);
    let t = SharedTransport::default();
    t.push(ClientCommand {
        actor: OPERATOR,
        nonce: 1,
        action: Action::Deposit {
            to: CAROL,
            amount: dollars(100),
        },
    });
    t.push(ClientCommand {
        actor: OPERATOR,
        nonce: 2,
        action: Action::ListMarket {
            id: market,
            description: "x".into(),
            oracle: ORACLE,
            trading_close: 2_000,
            resolution_deadline: 10_000_000,
            dispute_window: 60_000,
        },
    });
    let mut node = Node::start(MemJournal::default(), t.clone(), genesis(Config::default())).unwrap();
    node.poll(1_000).unwrap();
    node.poll(2_000).unwrap(); // trading closes
    t.push(ClientCommand {
        actor: ORACLE,
        nonce: 1,
        action: Action::ProposeOutcome {
            market,
            outcome: Outcome::Yes,
        },
    });
    node.poll(2_500).unwrap(); // the window ends at 62_500
    // The process freezes for 65 s; Carol's dispute waits on ingress.
    t.push(ClientCommand {
        actor: CAROL,
        nonce: 1,
        action: Action::Dispute { market },
    });
    node.poll(2_500 + 65_000).unwrap();
    let names = t.egress_names();
    assert!(!names.contains(&"MarketFinalized"), "{names:?}");
    assert_eq!(
        names[names.len() - 2..],
        ["OutcomeProposed", "Disputed"],
        "Resume re-opened the window, then the dispute landed"
    );
}

// ---- What a batch carries: engine time, kind and the balances it touched (gateway.md §2). ----

#[test]
fn a_batch_carries_time_kind_and_touched_balances() {
    let mut t = MemTransport::default();
    t.ingress.push_back(deposit(1));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    node.poll(5_000).unwrap();
    let (_, transport) = node.into_parts();
    assert_eq!(
        transport.egress,
        vec![EventBatch {
            seq: 2,
            time: 5_000,
            kind: BatchKind::User,
            origin: Some((OPERATOR, 1)),
            events: transport.egress[0].events.clone(),
            balances: vec![(
                ALICE,
                Balance {
                    free: dollars(5),
                    reserved: 0,
                    escrowed: 0
                }
            )],
        }]
    );
    assert_eq!(transport.egress[0].events.len(), 1, "the Deposited event");
}

#[test]
fn a_rejected_command_is_published_with_no_balances() {
    let mut t = MemTransport::default();
    t.ingress.push_back(ClientCommand {
        actor: ALICE,
        nonce: 1,
        action: Action::Withdraw { amount: dollars(1) },
    });
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    node.poll(5_000).unwrap();
    let (_, transport) = node.into_parts();
    let b = &transport.egress[0];
    assert_eq!((b.kind, b.origin), (BatchKind::User, Some((ALICE, 1))));
    assert_eq!(b.events.len(), 1, "the rejection");
    assert_eq!(b.balances, vec![], "a rejected command touches nothing");
}

#[test]
fn time_is_the_sequencers_clamped_stamp() {
    let mut t = MemTransport::default();
    t.ingress.push_back(deposit(1));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    node.poll(5_000).unwrap();
    let (journal, mut t) = node.into_parts();
    t.egress.clear();
    t.ingress.push_back(deposit(2));
    let mut node = Node::start(journal, t, genesis(Config::default())).unwrap();
    node.poll(1_000).unwrap(); // the wall clock is behind the journal: Resume and the deposit are clamped
    let (_, transport) = node.into_parts();
    let seen: Vec<_> = transport.egress.iter().map(|b| (b.seq, b.kind, b.time)).collect();
    assert_eq!(
        seen,
        vec![(3, BatchKind::Resume, 5_000), (4, BatchKind::User, 5_000)]
    );
}

#[test]
fn resume_is_published_even_without_events() {
    let mut t = MemTransport::default();
    t.ingress.push_back(deposit(1));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    node.poll(5_000).unwrap();
    let (journal, _) = node.into_parts();
    // Restart over a journal with history, nothing queued and no dispute window open.
    let mut node = Node::start(journal, MemTransport::default(), genesis(Config::default())).unwrap();
    assert_eq!(node.poll(9_000).unwrap(), 1, "Resume alone");
    let (_, transport) = node.into_parts();
    assert_eq!(
        transport.egress,
        vec![EventBatch {
            seq: 3,
            time: 9_000,
            kind: BatchKind::Resume,
            origin: None,
            events: vec![],
            balances: vec![],
        }]
    );
}

#[test]
fn a_stall_resume_is_published_too() {
    let (mut node, t) = live_node();
    assert_eq!(t.batches().len(), 1, "the deposit");
    let wall = LAST_POLL + STALL_GAP_MS + 1;
    assert_eq!(node.poll(wall).unwrap(), 1, "Resume alone");
    let last = t.batches().pop().expect("a batch");
    assert_eq!((last.seq, last.time, last.kind), (3, wall, BatchKind::Resume));
    assert_eq!((last.origin, last.events.is_empty()), (None, true));
}

/// A listed market and a funded requester's open request (id 4, deadline 2 000), all at wall time
/// 1 000, then optionally cancelled. The cancel leaves the request's deadline timer stale.
fn node_with_request(cancel: bool) -> Node<MemJournal, MemTransport> {
    let op = |nonce, action| ClientCommand {
        actor: OPERATOR,
        nonce,
        action,
    };
    let alice = |nonce, action| ClientCommand {
        actor: ALICE,
        nonce,
        action,
    };
    let mut t = MemTransport::default();
    t.ingress.extend([
        op(
            1,
            Action::Deposit {
                to: ALICE,
                amount: dollars(1_000),
            },
        ),
        op(
            2,
            Action::ListMarket {
                id: MarketId(1),
                description: "x".into(),
                oracle: ORACLE,
                trading_close: 20_000,
                resolution_deadline: 40_000,
                dispute_window: 1_000,
            },
        ),
        alice(
            1,
            Action::SubmitRequest {
                legs: vec![LegSpec {
                    market: MarketId(1),
                    side: Side::BuyYes,
                    qty: 1_000,
                    limit_price: price(7_000),
                }],
                response_deadline: 2_000,
            },
        ),
    ]);
    if cancel {
        t.ingress.push_back(alice(
            2,
            Action::CancelRequest {
                request: RequestId(4),
            },
        ));
    }
    let mut node = Node::start(MemJournal::default(), t, genesis(demo_config())).unwrap();
    node.poll(1_000).unwrap();
    node
}

#[test]
fn a_tick_that_emits_is_published_with_the_balances_its_timers_changed() {
    let mut node = node_with_request(false);
    assert_eq!(node.poll(2_000).unwrap(), 1, "the request's deadline Tick");
    let (_, transport) = node.into_parts();
    let tick = transport.egress.last().expect("a batch");
    assert_eq!(
        (tick.seq, tick.time, tick.kind, tick.origin),
        (5, 2_000, BatchKind::Tick, None),
        "after genesis, deposit, listing and request"
    );
    assert_eq!(names(&tick.events), vec!["RequestExpired"]);
    assert_eq!(
        tick.balances,
        vec![(
            ALICE,
            Balance {
                free: dollars(1_000),
                reserved: 0,
                escrowed: 0
            }
        )],
        "the expired request's hold went back to free"
    );
}

#[test]
fn a_tick_with_no_events_is_not_published() {
    let mut node = node_with_request(true);
    assert_eq!(
        node.engine().last_seq(),
        5,
        "genesis, deposit, listing, request, cancel"
    );
    assert_eq!(
        node.poll(2_000).unwrap(),
        1,
        "the stale deadline timer is still ticked"
    );
    assert_eq!(node.engine().last_seq(), 6, "the Tick was sequenced");
    let (journal, transport) = node.into_parts();
    assert_eq!(
        journal.entries().last().map(|sc| sc.cmd.clone()),
        Some(Command::Tick)
    );
    assert_eq!(
        transport.egress.iter().map(|b| b.seq).collect::<Vec<_>>(),
        vec![2, 3, 4, 5],
        "the four user commands, and nothing for the silent Tick"
    );
}

// ---- Write-ahead rule (added beyond the brief): spies record the order of port calls. ----

type CallLog = Rc<RefCell<Vec<String>>>;

/// Which port call fails.
#[derive(Clone, Copy, Default)]
struct Fail {
    durable: bool,
    poll: bool,
    publish: bool,
}

struct SpyJournal {
    inner: MemJournal,
    log: CallLog,
    fail: Fail,
}

impl Journal for SpyJournal {
    fn append(&mut self, sc: &SequencedCommand) -> Result<u64, BoxError> {
        self.log.borrow_mut().push(format!("append {}", sc.seq));
        self.inner.append(sc)
    }
    fn wait_durable(&mut self, pos: u64) -> Result<(), BoxError> {
        self.log.borrow_mut().push(format!("durable {pos}"));
        if self.fail.durable {
            return Err("fsync failed".into());
        }
        self.inner.wait_durable(pos)
    }
    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> Result<(), BoxError> {
        self.inner.replay(f)
    }
}

struct SpyTransport {
    inner: MemTransport,
    log: CallLog,
    fail: Fail,
}

impl Transport for SpyTransport {
    fn poll(&mut self, max: usize, sink: &mut dyn FnMut(ClientCommand)) -> Result<(), BoxError> {
        self.log.borrow_mut().push("poll".into());
        if self.fail.poll {
            return Err("ingress closed".into());
        }
        self.inner.poll(max, sink)
    }
    fn publish(&mut self, batch: &EventBatch) -> Result<(), BoxError> {
        self.log.borrow_mut().push(format!("publish {}", batch.seq));
        if self.fail.publish {
            return Err("egress closed".into());
        }
        self.inner.publish(batch)
    }
}

/// A journal that already holds `Genesis`, so `start` appends nothing.
fn journal_with_genesis() -> MemJournal {
    let mut j = MemJournal::default();
    let g = SequencedCommand {
        seq: 1,
        time: 0,
        cmd: Command::Genesis(genesis(Config::default())),
    };
    j.append(&g).unwrap();
    j
}

/// A restarted node (so its first poll sequences `Resume`) with two deposits waiting on ingress.
fn spies(fail: Fail) -> (Node<SpyJournal, SpyTransport>, CallLog) {
    let log = CallLog::default();
    let mut t = MemTransport::default();
    t.ingress.extend([deposit(1), deposit(2)]);
    let j = SpyJournal {
        inner: journal_with_genesis(),
        log: log.clone(),
        fail,
    };
    let t = SpyTransport {
        inner: t,
        log: log.clone(),
        fail,
    };
    (Node::start(j, t, genesis(Config::default())).unwrap(), log)
}

#[test]
fn whole_batch_is_durable_before_anything_is_published() {
    let (mut node, log) = spies(Fail::default());
    assert_eq!(node.poll(5_000).unwrap(), 3, "Resume and two deposits");
    assert_eq!(
        *log.borrow(),
        [
            "poll",
            "append 2",
            "append 3",
            "append 4",
            "durable 4",
            "publish 2",
            "publish 3",
            "publish 4"
        ],
        "one durability wait per batch; Resume emits no events but is always published"
    );
}

#[test]
fn failed_durability_applies_and_publishes_nothing() {
    let (mut node, log) = spies(Fail {
        durable: true,
        ..Fail::default()
    });
    assert!(matches!(node.poll(5_000), Err(NodeError::Journal(_))));
    assert_eq!(node.engine().last_seq(), 1, "nothing applied");
    assert!(
        !log.borrow().iter().any(|c| c.starts_with("publish")),
        "nothing published"
    );
}

#[test]
fn failed_publish_still_applies_the_durable_batch() {
    let (mut node, _) = spies(Fail {
        publish: true,
        ..Fail::default()
    });
    assert!(matches!(node.poll(5_000), Err(NodeError::Transport(_))));
    assert_eq!(
        node.engine().last_seq(),
        4,
        "in-memory state matches the journal even though egress failed"
    );
    assert_eq!(node.engine().balance(ALICE).free, dollars(10));
}

#[test]
fn failed_ingress_poll_is_a_transport_error_and_logs_nothing() {
    let (mut node, log) = spies(Fail {
        poll: true,
        ..Fail::default()
    });
    assert!(matches!(node.poll(5_000), Err(NodeError::Transport(_))));
    assert_eq!(*log.borrow(), ["poll"], "nothing appended, not even Resume");
    assert!(matches!(node.poll(6_000), Err(NodeError::Halted)));
    assert_eq!(*log.borrow(), ["poll"], "a halted node touches no port");
}

#[test]
fn poll_after_failure_is_refused() {
    let (mut node, log) = spies(Fail {
        durable: true,
        ..Fail::default()
    });
    assert!(matches!(node.poll(5_000), Err(NodeError::Journal(_))));
    let calls = log.borrow().len();
    assert!(matches!(node.poll(6_000), Err(NodeError::Halted)));
    assert!(
        matches!(node.poll(7_000), Err(NodeError::Halted)),
        "the latch holds"
    );
    assert_eq!(log.borrow().len(), calls, "a halted node touches no port");
    let (journal, transport) = node.into_parts();
    assert_eq!(
        journal.inner.entries().len(),
        4,
        "journal unchanged since the failure"
    );
    assert!(transport.inner.egress.is_empty(), "nothing published");
}

/// A journal holding Genesis as seq 1, then ticks with the given seqs.
fn journal_with_seqs(seqs: &[u64]) -> MemJournal {
    let mut j = journal_with_genesis();
    for &seq in seqs {
        j.append(&SequencedCommand {
            seq,
            time: 1_000,
            cmd: Command::Tick,
        })
        .unwrap();
    }
    j
}

fn start_error(journal: MemJournal) -> SequenceError {
    match Node::start(journal, MemTransport::default(), genesis(Config::default())) {
        Err(NodeError::Journal(e)) => *e.downcast::<SequenceError>().expect("a SequenceError"),
        Err(other) => panic!("unexpected error: {other:?}"),
        Ok(_) => panic!("start accepted an out-of-sequence journal"),
    }
}

#[test]
fn replay_refuses_a_duplicate_seq() {
    let e = start_error(journal_with_seqs(&[2, 3, 3, 4]));
    assert_eq!(
        e,
        SequenceError {
            expected: 4,
            found: 3
        }
    );
    assert!(e.to_string().contains("duplicate"), "{e}");
}

#[test]
fn replay_refuses_a_gap() {
    let e = start_error(journal_with_seqs(&[2, 4]));
    assert_eq!(
        e,
        SequenceError {
            expected: 3,
            found: 4
        }
    );
    assert!(e.to_string().contains("gap"), "{e}");
}

#[test]
fn replay_refuses_a_log_that_does_not_start_at_seq_1() {
    let mut j = MemJournal::default();
    j.append(&SequencedCommand {
        seq: 2,
        time: 0,
        cmd: Command::Genesis(genesis(Config::default())),
    })
    .unwrap();
    assert_eq!(
        start_error(j),
        SequenceError {
            expected: 1,
            found: 2
        }
    );
}

#[test]
fn contiguous_replay_is_accepted() {
    let node = Node::start(
        journal_with_seqs(&[2, 3]),
        MemTransport::default(),
        genesis(Config::default()),
    )
    .unwrap();
    assert_eq!(node.engine().last_seq(), 3);
}
