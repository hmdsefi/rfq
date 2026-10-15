use rfq_domain::*;
use rfq_testkit::scenarios::*;
use rfq_testkit::{Harness, accounts::*, assert_sealed, dollars, names, rejection};

const T0: Timestamp = 1_000_000;

fn run(name: &str) -> (Harness, Vec<Emitted>) {
    let s = all().into_iter().find(|s| s.name == name).unwrap();
    let (h, events) = run_in_memory(&s, T0);
    contains_in_order(&events, &s.expect).unwrap_or_else(|e| panic!("{name}: {e}"));
    (h, events)
}

fn free(h: &Harness, a: AccountId) -> Amount {
    h.engine.balance(a).free
}

/// Replays the harness's whole log into a fresh engine and checks the final state is identical.
fn assert_replays_identically(h: &Harness, what: &str) {
    let mut replayed = Engine::new();
    for sc in &h.log {
        replayed.apply(sc);
    }
    assert_eq!(replayed.state_hash(), h.engine.state_hash(), "{what}");
    // Replaying the log a second time is a no-op (seq <= last_seq).
    for sc in &h.log {
        assert!(replayed.apply(sc).is_empty(), "{what}");
    }
    assert_eq!(replayed.state_hash(), h.engine.state_hash(), "{what}");
}

#[test]
fn happy_path() {
    let (h, _) = run("happy_path");
    assert_eq!(free(&h, ALICE), dollars(1_400));
    assert_eq!(free(&h, MM1), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(600));
}

#[test]
fn mm_cannot_withdraw_after_selection() {
    let (h, events) = run("mm_cannot_withdraw_after_selection");
    assert_eq!(
        rejection(&events),
        Some(RejectReason::InvalidState),
        "MM2's cancel of its selected (firm) quote"
    );
    assert_eq!(free(&h, ALICE), dollars(400));
    assert_eq!(free(&h, MM1), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(600));
    assert_eq!(
        h.engine.balance(MM2).escrowed,
        dollars(400),
        "the selected quote stayed firm and filled"
    );
}

#[test]
fn no_quotes_expire() {
    let (h, _) = run("no_quotes_expire");
    assert_eq!(free(&h, ALICE), dollars(1_000));
}

#[test]
fn requester_lapses() {
    let (h, _) = run("requester_lapses");
    for a in [ALICE, MM1, MM2] {
        assert_eq!(free(&h, a), dollars(1_000));
    }
}

#[test]
fn disputed_ruled_invalid() {
    let (h, _) = run("disputed_ruled_invalid");
    for a in [ALICE, MM2, CAROL] {
        assert_eq!(free(&h, a), dollars(1_000));
    }
}

#[test]
fn oracle_silent_hard_deadline() {
    let (h, _) = run("oracle_silent_hard_deadline");
    for a in [ALICE, MM2] {
        assert_eq!(free(&h, a), dollars(1_000));
    }
}

// The six brief scenarios keep their names and order; the two multi-leg ones come after them.
#[test]
fn the_scenarios_are_the_six_brief_ones_then_the_two_multi_leg_ones() {
    let names: Vec<_> = all().iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        [
            "happy_path",
            "mm_cannot_withdraw_after_selection",
            "no_quotes_expire",
            "requester_lapses",
            "disputed_ruled_invalid",
            "oracle_silent_hard_deadline",
            "multi_leg_happy",
            "multi_leg_leg2_fails",
        ]
    );
}

// Three markets and one request for 400 YES on each: Alice's $840 hold pays $248 + $240 + $256
// for the three picks (0.62, 0.60, 0.64) and the rest goes back; MM1 backs leg indices 0 and 2, MM2 index 1.
#[test]
fn multi_leg_happy() {
    let (h, events) = run("multi_leg_happy");
    assert_eq!(free(&h, ALICE), dollars(1_000 - 744));
    assert_eq!(h.engine.balance(ALICE).escrowed, dollars(744));
    assert_eq!(free(&h, MM1), dollars(1_000 - 152 - 144));
    assert_eq!(h.engine.balance(MM1).escrowed, dollars(152 + 144));
    assert_eq!(free(&h, MM2), dollars(1_000 - 160));
    assert_eq!(h.engine.balance(MM2).escrowed, dollars(160));
    // One position per leg, each on its own market.
    let ctx = Ctx::new(T0);
    for k in 1..=3 {
        let c = h.engine.contract(ctx.contract_n(k)).expect("listed");
        assert_eq!(c.positions.len(), 1, "market {k}");
    }
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::RequestOpened { .. }))
            .count(),
        3,
        "one RequestOpened per leg"
    );
}

// Legs 1 and 3 are quoted, leg 2 is not: at the deadline the whole request expires and every hold
// (Alice's and both MMs') is released in that one command. Nothing was escrowed, so nothing fills.
#[test]
fn multi_leg_leg2_fails() {
    let (h, events) = run("multi_leg_leg2_fails");
    for a in [ALICE, MM1, MM2] {
        assert_eq!(free(&h, a), dollars(1_000));
        assert_eq!(h.engine.balance(a).escrowed, 0);
    }
    let ctx = Ctx::new(T0);
    for k in 1..=3 {
        let c = h.engine.contract(ctx.contract_n(k)).expect("listed");
        assert!(c.positions.is_empty(), "market {k}");
    }
    // The releases happen in the command that fires the deadline: the tick whose events end the request.
    let expired = events
        .iter()
        .position(|e| matches!(e.event, Event::RequestExpired { .. }))
        .expect("expired");
    let lost: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.event, Event::QuoteLost { .. }))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        lost,
        [expired - 2, expired - 1],
        "both quotes lost right before the expiry"
    );
}

#[test]
fn replay_reproduces_state_hash() {
    for s in all() {
        let (h, _) = run_in_memory(&s, T0);
        assert_replays_identically(&h, s.name);
    }
}

// Added beyond the brief (controller ruling R22): a log that contains `Resume` replays to the same
// state. The happy path is cut right after the oracle's proposal; the node then "restarts" inside
// and after the dispute window, a dispute lands in the extended window, and the arbiter upholds it.
#[test]
fn replay_with_resume_in_the_log_reproduces_state_hash() {
    let mut s = all().into_iter().find(|s| s.name == "happy_path").unwrap();
    s.steps.retain(|step| step.at <= 5_200); // through ProposeOutcome; the window ends at +7_200
    let (mut h, _) = run_in_memory(&s, T0);
    let c = ContractId(T0);
    assert!(matches!(
        h.engine.contract(c).unwrap().state,
        ContractState::Proposed { .. }
    ));

    // Down across the window's end (+7_200): Resume re-opens it for resume_grace (2 s), below +15_000.
    h.set_time(T0 + 9_000);
    let out = h.submit(Command::Resume);
    assert_eq!(
        out,
        vec![Emitted::public(Event::OutcomeProposed {
            contract: c,
            outcome: Outcome::Yes,
            dispute_until: T0 + 11_000
        })]
    );
    assert_eq!(rejection(&h.user(CAROL, Action::Dispute { contract: c })), None);

    // A second restart while disputed changes nothing, then the arbiter upholds the proposal.
    h.set_time(T0 + 10_000);
    assert_eq!(h.submit(Command::Resume), vec![]);
    h.set_time(T0 + 10_500);
    let out = h.user(
        ARBITER,
        Action::Rule {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    assert_eq!(names(&out)[0], "Ruled");
    h.tick(T0 + 16_000);

    assert_eq!(
        h.log.iter().filter(|sc| sc.cmd == Command::Resume).count(),
        2,
        "the log really contains the restarts"
    );
    assert_eq!(free(&h, ALICE), dollars(1_400));
    assert_eq!(free(&h, MM2), dollars(600));
    assert_eq!(free(&h, CAROL), dollars(900), "bond forfeited");
    assert_eq!(free(&h, TREASURY), dollars(100));
    assert_replays_identically(&h, "log with Resume");
}

// Added beyond the brief: the scripts are pure, so a rerun is bit-identical, and a different start
// time shifts only the timestamps (same events, same money).
#[test]
fn scenarios_are_deterministic() {
    for s in all() {
        let (h1, e1) = run_in_memory(&s, T0);
        let (h2, e2) = run_in_memory(&s, T0);
        assert_eq!(e1, e2, "{}", s.name);
        assert_eq!(h1.engine.state_hash(), h2.engine.state_hash(), "{}", s.name);
        let (h3, e3) = run_in_memory(&s, T0 + 12_345);
        assert_eq!(names(&e1), names(&e3), "{}", s.name);
        for a in [ALICE, MM1, MM2, CAROL, TREASURY] {
            assert_eq!(h1.engine.balance(a), h3.engine.balance(a), "{}", s.name);
        }
    }
}

// Added beyond the brief: sealed-bid audiences hold across every scripted run, end to end.
#[test]
fn every_scenario_keeps_quote_events_sealed() {
    for s in all() {
        let (h, events) = run_in_memory(&s, T0);
        assert_sealed(&h, &events);
    }
}

// Added beyond the brief: money is conserved in every scripted run, and no scenario ends with a hold
// left open (reserved). Escrow may remain only where the script stops with a filled, unsettled position.
#[test]
fn every_scenario_conserves_money_and_ends_with_no_open_holds() {
    for s in all() {
        let (h, _) = run_in_memory(&s, T0);
        let mut total = 0;
        for a in [ALICE, MM1, MM2, CAROL, TREASURY] {
            let b = h.engine.balance(a);
            assert_eq!(b.reserved, 0, "{} {a:?}", s.name);
            total += b.free + b.escrowed;
        }
        assert_eq!(total, dollars(4_000), "{}", s.name);
    }
}

#[test]
fn contains_in_order_reports_the_first_missing_event() {
    let (_, events) = run("no_quotes_expire");
    assert_eq!(
        contains_in_order(&events, &["RequestOpened", "RequestExpired"]),
        Ok(())
    );
    assert!(contains_in_order(&events, &["RequestExpired", "RequestOpened"]).is_err());
    let err = contains_in_order(&events, &["RequestOpened", "QuoteFilled"]).unwrap_err();
    assert!(err.contains("QuoteFilled"), "{err}");
}
