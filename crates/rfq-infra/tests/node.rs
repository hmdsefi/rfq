//! Node behaviour over the in-memory adapters: startup, replay, write-ahead order, ticks.
use std::cell::RefCell;
use std::rc::Rc;

use rfq_app::{BoxError, ClientCommand, EventBatch, Journal, Node, NodeError, SequenceError, Transport};
use rfq_domain::*;
use rfq_infra::journal::{FileJournal, MemJournal};
use rfq_infra::transport::MemTransport;
use rfq_testkit::{accounts::*, dollars, genesis};

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
    // A contract arms close/hard-deadline timers.
    let list = ClientCommand {
        actor: OPERATOR,
        nonce: 1,
        action: Action::ListContract {
            id: ContractId(1),
            description: "x".into(),
            oracle: ORACLE,
            trading_close: 10_000,
            resolution_deadline: 20_000,
            dispute_window: 1,
        },
    };
    let (j, mut t) = node.into_parts();
    t.ingress.push_back(list);
    let mut node = Node::start(j, t, genesis(Config::default())).unwrap();
    node.poll(1_000).unwrap();
    assert_eq!(node.poll(9_999).unwrap(), 0);
    assert_eq!(node.poll(10_000).unwrap(), 1, "tick at trading_close");
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
            "publish 3",
            "publish 4"
        ],
        "one durability wait per batch; Resume emits no events so publishes nothing"
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
