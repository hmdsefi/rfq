//! The engine's read accessors for positions and nonces. They only read: no state, rule or hash changes.
use rfq_domain::*;
use rfq_testkit::accounts::*;
use rfq_testkit::scenarios::{all, demo_config, run_in_memory};
use rfq_testkit::{Harness, dollars, rejection};

/// Sends `action` from `actor` with an explicit `nonce`, bypassing the harness's own counter.
fn send_with_nonce(h: &mut Harness, actor: AccountId, nonce: u64, action: Action) -> Vec<Emitted> {
    h.submit(Command::User { actor, nonce, action })
}

fn deposit(to: AccountId, usd: u64) -> Action {
    Action::Deposit {
        to,
        amount: dollars(usd),
    }
}

#[test]
fn last_nonce_is_zero_for_an_unknown_account_and_follows_accepted_commands() {
    assert_eq!(Engine::new().last_nonce(OPERATOR), 0);

    let mut h = Harness::new(demo_config());
    assert_eq!(h.engine.last_nonce(OPERATOR), 0, "genesis is not a user command");

    h.fund(ALICE, 10); // OPERATOR's nonce 1
    h.fund(MM1, 10); // OPERATOR's nonce 2
    assert_eq!(h.engine.last_nonce(OPERATOR), 2);
    assert_eq!(
        h.engine.last_nonce(ALICE),
        0,
        "a funded account has sent nothing itself"
    );

    // A replay of nonce 2 and a skip to nonce 4 are rejected and consume nothing.
    let out = send_with_nonce(&mut h, OPERATOR, 2, deposit(ALICE, 1));
    assert_eq!(rejection(&out), Some(RejectReason::Duplicate));
    assert_eq!(h.engine.last_nonce(OPERATOR), 2);
    let out = send_with_nonce(&mut h, OPERATOR, 4, deposit(ALICE, 1));
    assert_eq!(rejection(&out), Some(RejectReason::NonceGap));
    assert_eq!(h.engine.last_nonce(OPERATOR), 2);

    // A valid nonce is consumed even when the action itself is refused.
    let out = send_with_nonce(&mut h, ALICE, 1, Action::Withdraw { amount: dollars(11) });
    assert!(rejection(&out).is_some());
    assert_eq!(h.engine.last_nonce(ALICE), 1);
}

#[test]
fn positions_lists_every_position_in_id_order() {
    let happy = all()
        .into_iter()
        .find(|s| s.name == "happy_path")
        .expect("happy_path");
    let (h, _) = run_in_memory(&happy, 1_000);

    // Alice bought 1,000 YES from MM2 at 0.60: she locks $600, MM2 locks the other $400.
    let positions: Vec<&Position> = h.engine.positions().collect();
    assert_eq!(positions.len(), 1);
    let p = positions[0];
    assert_eq!(p.id, PositionId(1));
    assert_eq!(p.qty, 1_000);
    assert_eq!((p.yes_holder, p.yes_lock), (ALICE, dollars(600)));
    assert_eq!((p.no_holder, p.no_lock), (MM2, dollars(400)));
    assert!(p.settled, "happy_path runs to settlement");
    assert_eq!(h.engine.position(p.id), Some(p));

    // Three legs fill into three positions, listed by ascending id.
    let multi = all()
        .into_iter()
        .find(|s| s.name == "multi_leg_happy")
        .expect("multi_leg_happy");
    let (h, _) = run_in_memory(&multi, 1_000);
    let ids: Vec<PositionId> = h.engine.positions().map(|p| p.id).collect();
    assert_eq!(ids, vec![PositionId(1), PositionId(2), PositionId(3)]);
}
