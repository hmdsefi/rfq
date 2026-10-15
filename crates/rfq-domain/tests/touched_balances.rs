//! The accounts a command changes, with their balances after it (docs/specs/gateway.md §2.1).
//! The set is output only: it never reaches the state hash, so replay and snapshots are unchanged.
use rfq_domain::*;
use rfq_testkit::Harness;
use rfq_testkit::accounts::*;
use rfq_testkit::dollars;
use rfq_testkit::scenarios::{Ctx, StepKind, all, demo_config};

fn touched(h: &mut Harness) -> Vec<AccountId> {
    h.engine
        .take_touched_balances()
        .into_iter()
        .map(|(a, _)| a)
        .collect()
}

#[test]
fn a_deposit_touches_only_its_account_and_reports_the_balance_after_it() {
    let mut h = Harness::new(demo_config());
    h.fund(ALICE, 10);
    let out = h.engine.take_touched_balances();
    assert_eq!(
        out,
        vec![(
            ALICE,
            Balance {
                free: dollars(10),
                reserved: 0,
                escrowed: 0
            }
        )]
    );
}

#[test]
fn a_rejected_command_touches_nothing() {
    let mut h = Harness::new(demo_config());
    h.fund(ALICE, 10);
    h.engine.take_touched_balances();
    let out = h.user(ALICE, Action::Withdraw { amount: dollars(11) });
    assert!(rfq_testkit::rejection(&out).is_some());
    assert_eq!(touched(&mut h), Vec::<AccountId>::new());
}

#[test]
fn each_apply_starts_a_fresh_set() {
    let mut h = Harness::new(demo_config());
    h.fund(ALICE, 10);
    h.fund(MM1, 10); // ALICE's touch from the previous command is not reported again
    assert_eq!(touched(&mut h), vec![MM1]);
}

#[test]
fn accept_touches_the_requester_and_the_selected_mm_only() {
    // Run happy_path up to the accept and check the accept's touched set is {ALICE, MM2}
    // (MM1's hold was released earlier, at the deadline tick).
    let s = all()
        .into_iter()
        .find(|s| s.name == "happy_path")
        .expect("happy_path");
    let mut h = Harness::new(demo_config());
    let mut ctx = Ctx::new(1_000);
    for step in s.steps {
        h.set_time(1_000 + step.at);
        let label = match &step.kind {
            StepKind::Send { label, .. } => Some(*label),
            StepKind::Tick => None,
        };
        let out = match step.kind {
            StepKind::Tick => h.submit(Command::Tick),
            StepKind::Send { label, actor, action } => {
                ctx.record_label(label, h.next_seq());
                let a = action(&ctx);
                h.user(actor, a)
            }
        };
        ctx.observe(&out);
        let t = touched(&mut h);
        if label == Some("accept") {
            assert_eq!(t, vec![ALICE, MM2]);
            return;
        }
    }
    panic!("happy_path has no accept step");
}

#[test]
fn draining_the_set_never_changes_the_state_hash() {
    let mut h = Harness::new(demo_config());
    h.fund(ALICE, 10);
    let before = h.engine.state_hash();
    h.engine.take_touched_balances();
    assert_eq!(h.engine.state_hash(), before);
}
