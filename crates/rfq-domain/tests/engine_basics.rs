use rfq_domain::*;
use rfq_testkit::{accounts::*, *};

#[test]
fn genesis_initializes_once() {
    let mut h = Harness::new(Config::default());
    let out = h.submit(Command::Genesis(genesis(Config::default())));
    assert_eq!(rejection(&out), Some(RejectReason::AlreadyInitialized));
}

#[test]
fn commands_before_genesis_are_rejected() {
    let mut e = Engine::new();
    let out = e.apply(&SequencedCommand {
        seq: 1,
        time: 0,
        cmd: Command::User {
            actor: ALICE,
            nonce: 1,
            action: Action::Withdraw { amount: 1 },
        },
    });
    assert_eq!(rejection(&out), Some(RejectReason::NotInitialized));
}

#[test]
fn deposit_requires_operator() {
    let mut h = Harness::new(Config::default());
    let out = h.user(ALICE, Action::Deposit { to: ALICE, amount: 1 });
    assert_eq!(rejection(&out), Some(RejectReason::Unauthorized));
    assert_eq!(h.engine.balance(ALICE), Balance::default());
}

#[test]
fn deposit_and_withdraw() {
    let mut h = Harness::new(Config::default());
    h.fund(ALICE, 100);
    assert_eq!(
        rejection(&h.user(ALICE, Action::Withdraw { amount: dollars(40) })),
        None
    );
    assert_eq!(h.engine.balance(ALICE).free, dollars(60));
    let out = h.user(ALICE, Action::Withdraw { amount: dollars(61) });
    assert_eq!(rejection(&out), Some(RejectReason::InsufficientFunds));
}

#[test]
fn deposit_overflow_rejected() {
    let mut h = Harness::new(Config::default());
    assert_eq!(
        rejection(&h.user(
            OPERATOR,
            Action::Deposit {
                to: ALICE,
                amount: u64::MAX
            }
        )),
        None
    );
    let before = h.engine.state_hash();
    let out = h.user(OPERATOR, Action::Deposit { to: MM1, amount: 1 });
    assert_eq!(rejection(&out), Some(RejectReason::Overflow));
    assert_eq!(h.engine.balance(MM1), Balance::default());
    // Only the operator's nonce changed.
    assert_ne!(before, h.engine.state_hash());
}

#[test]
fn duplicate_nonce_ignored() {
    let mut h = Harness::new(Config::default());
    let deposit = Command::User {
        actor: OPERATOR,
        nonce: 7,
        action: Action::Deposit { to: ALICE, amount: 5 },
    };
    assert_eq!(rejection(&h.submit(deposit.clone())), None);
    let out = h.submit(deposit);
    assert_eq!(rejection(&out), Some(RejectReason::Duplicate));
    assert_eq!(h.engine.balance(ALICE).free, 5);
}

#[test]
fn already_applied_seq_is_ignored() {
    let mut h = Harness::new(Config::default());
    h.fund(ALICE, 1);
    let last = h.log.last().unwrap().clone();
    let before = h.engine.state_hash();
    assert!(h.engine.apply(&last).is_empty());
    assert_eq!(before, h.engine.state_hash());
}

#[test]
fn identical_logs_give_identical_hashes() {
    let mut a = Harness::new(Config::default());
    a.fund(ALICE, 10);
    let mut b = Engine::new();
    for sc in &a.log {
        b.apply(sc);
    }
    assert_eq!(a.engine.state_hash(), b.state_hash());
}

#[test]
fn time_never_goes_backwards() {
    let mut e = Engine::new();
    e.apply(&SequencedCommand {
        seq: 1,
        time: 100,
        cmd: Command::Genesis(genesis(Config::default())),
    });
    e.apply(&SequencedCommand {
        seq: 2,
        time: 50,
        cmd: Command::Tick,
    });
    assert_eq!(e.now(), 100);
}
