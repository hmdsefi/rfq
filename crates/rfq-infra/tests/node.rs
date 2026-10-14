//! Node behaviour over the in-memory adapters: startup, replay, write-ahead order, ticks.
use std::cell::RefCell;
use std::rc::Rc;

use rfq_app::{BoxError, ClientCommand, EventBatch, Journal, Node, NodeError, Transport};
use rfq_domain::*;
use rfq_infra::journal::MemJournal;
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

// ---- Write-ahead rule (added beyond the brief): spies record the order of port calls. ----

type CallLog = Rc<RefCell<Vec<String>>>;

struct SpyJournal {
    inner: MemJournal,
    log: CallLog,
    fail_durable: bool,
}

impl Journal for SpyJournal {
    fn append(&mut self, sc: &SequencedCommand) -> Result<u64, BoxError> {
        self.log.borrow_mut().push(format!("append {}", sc.seq));
        self.inner.append(sc)
    }
    fn wait_durable(&mut self, pos: u64) -> Result<(), BoxError> {
        self.log.borrow_mut().push(format!("durable {pos}"));
        if self.fail_durable {
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
    fail_publish: bool,
}

impl Transport for SpyTransport {
    fn poll(&mut self, max: usize, sink: &mut dyn FnMut(ClientCommand)) {
        self.inner.poll(max, sink);
    }
    fn publish(&mut self, batch: &EventBatch) -> Result<(), BoxError> {
        self.log.borrow_mut().push(format!("publish {}", batch.seq));
        if self.fail_publish {
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

fn spies(fail_durable: bool, fail_publish: bool) -> (Node<SpyJournal, SpyTransport>, CallLog) {
    let log = CallLog::default();
    let mut t = MemTransport::default();
    t.ingress.extend([deposit(1), deposit(2)]);
    let j = SpyJournal {
        inner: journal_with_genesis(),
        log: log.clone(),
        fail_durable,
    };
    let t = SpyTransport {
        inner: t,
        log: log.clone(),
        fail_publish,
    };
    (Node::start(j, t, genesis(Config::default())).unwrap(), log)
}

#[test]
fn whole_batch_is_durable_before_anything_is_published() {
    let (mut node, log) = spies(false, false);
    assert_eq!(node.poll(5_000).unwrap(), 3, "Resume and two deposits");
    assert_eq!(
        *log.borrow(),
        [
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
    let (mut node, log) = spies(true, false);
    assert!(matches!(node.poll(5_000), Err(NodeError::Journal(_))));
    assert_eq!(node.engine().last_seq(), 1, "nothing applied");
    assert!(
        !log.borrow().iter().any(|c| c.starts_with("publish")),
        "nothing published"
    );
}

#[test]
fn failed_publish_still_applies_the_durable_batch() {
    let (mut node, _) = spies(false, true);
    assert!(matches!(node.poll(5_000), Err(NodeError::Transport(_))));
    assert_eq!(
        node.engine().last_seq(),
        4,
        "in-memory state matches the journal even though egress failed"
    );
    assert_eq!(node.engine().balance(ALICE).free, dollars(10));
}
