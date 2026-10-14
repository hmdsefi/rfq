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
    // The harness sends the operator's nonce 2 here.
    let out = h.user(OPERATOR, Action::Deposit { to: MM1, amount: 1 });
    assert_eq!(rejection(&out), Some(RejectReason::Overflow));
    assert_eq!(h.engine.balance(MM1), Balance::default());
    // The rejected command still consumed nonce 2, so replaying it is a Duplicate.
    let replay = Command::User {
        actor: OPERATOR,
        nonce: 2,
        action: Action::Deposit { to: MM1, amount: 1 },
    };
    assert_eq!(rejection(&h.submit(replay)), Some(RejectReason::Duplicate));
}

#[test]
fn duplicate_nonce_ignored() {
    let mut h = Harness::new(Config::default());
    let deposit = |nonce| Command::User {
        actor: OPERATOR,
        nonce,
        action: Action::Deposit { to: ALICE, amount: 5 },
    };
    assert_eq!(rejection(&h.submit(deposit(1))), None);
    let out = h.submit(deposit(1));
    assert_eq!(rejection(&out), Some(RejectReason::Duplicate));
    assert_eq!(h.engine.balance(ALICE).free, 5);
    // The Duplicate consumed nothing: the next consecutive nonce is still 2.
    assert_eq!(rejection(&h.submit(deposit(2))), None);
    assert_eq!(h.engine.balance(ALICE).free, 10);
}

#[test]
fn nonce_gap_rejected_and_not_consumed() {
    let mut h = Harness::new(Config::default());
    let deposit = |nonce| Command::User {
        actor: OPERATOR,
        nonce,
        action: Action::Deposit { to: ALICE, amount: 5 },
    };
    let out = h.submit(deposit(3));
    assert_eq!(rejection(&out), Some(RejectReason::NonceGap));
    assert_eq!(h.engine.balance(ALICE), Balance::default());
    // Nothing was consumed: nonces 1, 2 and then 3 are all accepted in order.
    for nonce in 1..=3 {
        assert_eq!(rejection(&h.submit(deposit(nonce))), None, "nonce {nonce}");
    }
    assert_eq!(h.engine.balance(ALICE).free, 15);
    // A gap after progress is rejected too, and still consumes nothing.
    assert_eq!(rejection(&h.submit(deposit(5))), Some(RejectReason::NonceGap));
    assert_eq!(rejection(&h.submit(deposit(4))), None);
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
