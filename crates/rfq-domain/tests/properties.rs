//! Random command streams must never break I1–I5 (the harness checks them after every command).
//!
//! Two properties:
//! - `invariants_hold_for_random_streams` (from the brief): uniformly random actors and objects. It
//!   mostly probes guards (wrong actor, wrong state) and the early request/quote lifecycle.
//! - `invariants_hold_for_directed_streams` (added): the same kind of stream, but owner-restricted
//!   commands usually come from the rightful account, the arbiter acts, and the contract closes inside
//!   the reachable time range, so streams reach fills, settlement, disputes, rulings and `Resume`
//!   extensions. `directed_streams_reach_every_lifecycle_path` keeps it from silently going vacuous.
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use rfq_domain::*;
use rfq_testkit::scenarios::demo_config;
use rfq_testkit::{Harness, accounts::*, assert_sealed, dollars, price, rejection};

const ACTORS: [AccountId; 5] = [ALICE, MM1, MM2, CAROL, ORACLE];

#[derive(Debug, Clone)]
struct Op {
    actor: usize,
    kind: u8,
    a: u16,
    b: u16,
    dt: u32,
}

fn op() -> impl Strategy<Value = Op> {
    (0..ACTORS.len(), 0u8..11, any::<u16>(), any::<u16>(), 0u32..20_000).prop_map(
        |(actor, kind, a, b, dt)| Op {
            actor,
            kind,
            a,
            b,
            dt,
        },
    )
}

fn pick<T: Copy>(items: &[T], i: u16) -> Option<T> {
    if items.is_empty() {
        None
    } else {
        Some(items[i as usize % items.len()])
    }
}

/// Replaying the whole log into a fresh engine must land on exactly the live engine's state.
fn replay_hash(h: &Harness) -> [u8; 32] {
    let mut replayed = Engine::new();
    for sc in &h.log {
        replayed.apply(sc);
    }
    replayed.state_hash()
}

fn action(h: &Harness, o: &Op, c: ContractId) -> Option<Action> {
    let now = h.now();
    let requests: Vec<RequestId> = h.engine.requests().map(|r| r.id).collect();
    let quotes: Vec<QuoteId> = h.engine.quotes().map(|q| q.id).collect();
    let p = price(1 + (o.b % 9_999));
    Some(match o.kind {
        0 => Action::SubmitRequest {
            legs: vec![LegSpec {
                contract: c,
                side: if o.a.is_multiple_of(2) {
                    Side::BuyYes
                } else {
                    Side::BuyNo
                },
                qty: 1 + (o.a as u64 % 500),
                limit_price: p,
            }],
            response_deadline: now + 1_000 + (o.b as u64 % 20_000),
        },
        1 => Action::SubmitQuote {
            request: pick(&requests, o.a)?,
            leg: 0,
            price: p,
            size: 500,
            expires_at: now + 1 + o.b as u64 * 3,
        },
        2 => Action::CancelQuote {
            quote: pick(&quotes, o.a)?,
        },
        3 => Action::CancelRequest {
            request: pick(&requests, o.a)?,
        },
        4 => {
            let r = h.engine.request(pick(&requests, o.a)?)?;
            let picks = r
                .legs
                .iter()
                .enumerate()
                .filter_map(|(i, l)| {
                    let q = h.engine.quote(l.selected?)?;
                    Some(Pick {
                        leg: i as u32,
                        quote: q.id,
                        price: q.price,
                    })
                })
                .collect();
            Action::Accept { request: r.id, picks }
        }
        5 => Action::Reject {
            request: pick(&requests, o.a)?,
        },
        6 => Action::ProposeOutcome {
            contract: c,
            outcome: [Outcome::Yes, Outcome::No, Outcome::Invalid][o.a as usize % 3],
        },
        7 => Action::Dispute { contract: c },
        8 => Action::Rule {
            contract: c,
            outcome: [Outcome::Yes, Outcome::No, Outcome::Invalid][o.a as usize % 3],
        },
        _ => Action::Withdraw {
            amount: dollars(1 + o.a as u64 % 50),
        },
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn invariants_hold_for_random_streams(ops in prop::collection::vec(op(), 1..200)) {
        let mut h = Harness::new(Config::default());
        for a in ACTORS { h.fund(a, 10_000); }
        let c = ContractId(1);
        h.user(OPERATOR, Action::ListContract { id: c, description: "p".into(), oracle: ORACLE, trading_close: 2_000_000, resolution_deadline: 4_000_000, dispute_window: 5_000 });
        let total: Amount = ACTORS.iter().map(|a| { let b = h.engine.balance(*a); b.free + b.reserved + b.escrowed }).sum();
        let mut withdrawn: Amount = 0;
        for o in &ops {
            let t = h.now() + o.dt as u64;
            h.set_time(t);
            if o.kind == 10 {
                let out = h.submit(Command::Resume); // a restart can happen between any two commands
                assert_sealed(&h, &out);
                continue;
            }
            match action(&h, o, c) {
                Some(a) => {
                    let amount = if let Action::Withdraw { amount } = a { Some(amount) } else { None };
                    let out = h.user(ACTORS[o.actor], a);
                    assert_sealed(&h, &out);
                    if let (Some(x), None) = (amount, rejection(&out)) { withdrawn += x; }
                }
                None => { h.submit(Command::Tick); }
            }
        }
        h.tick(h.now() + 5_000_000); // past every deadline: everything must be released or settled
        let now_total: Amount = [ALICE, MM1, MM2, CAROL, ORACLE, TREASURY].iter().map(|a| h.engine.balance(*a).free).sum();
        prop_assert_eq!(now_total + withdrawn, total);
        for a in ACTORS { prop_assert_eq!(h.engine.balance(a).reserved + h.engine.balance(a).escrowed, 0); }
        prop_assert_eq!(replay_hash(&h), h.engine.state_hash());
    }
}

// ---------------------------------------------------------------------------------------------
// Directed streams (added beyond the brief; see the module comment).
// ---------------------------------------------------------------------------------------------

/// Reachable inside a stream: ~165 ops at ~400 ms each, so most long streams cross both times.
const CLOSE: Timestamp = 40_000;
const HARD: Timestamp = 90_000;
const WINDOW: Duration = 5_000;
const CONTRACT: ContractId = ContractId(1);
const USERS: [AccountId; 4] = [ALICE, MM1, MM2, CAROL];
const EVERYONE: [AccountId; 8] = [OPERATOR, ARBITER, TREASURY, ORACLE, ALICE, MM1, MM2, CAROL];
const FUNDED_EACH: u64 = 10_000;

fn choose<T>(items: Vec<&T>, i: u16) -> Option<&T> {
    if items.is_empty() {
        None
    } else {
        Some(items[usize::from(i) % items.len()])
    }
}

fn directed_op() -> impl Strategy<Value = Op> {
    // actor 9 (1 in 10) makes an owner-restricted command come from the wrong account.
    (0usize..10, 0u8..16, any::<u16>(), any::<u16>(), 0u32..800).prop_map(|(actor, kind, a, b, dt)| Op {
        actor,
        kind,
        a,
        b,
        dt,
    })
}

/// The command for `o`, with the account that sends it. Owner-restricted commands (cancel, accept,
/// reject, propose, rule) come from the rightful account unless `o.actor == 9`.
fn directed(h: &Harness, o: &Op) -> Option<(AccountId, Action)> {
    let user = USERS[o.actor % USERS.len()];
    let wrong = o.actor == 9;
    let who = |owner: AccountId| if wrong { user } else { owner };
    let now = h.now();
    let cfg = demo_config();
    let in_state = |f: fn(&RequestState) -> bool| -> Vec<&Request> {
        h.engine.requests().filter(|r| f(&r.state)).collect()
    };
    let open = || in_state(|s| matches!(s, RequestState::Open));
    let presented = || in_state(|s| matches!(s, RequestState::Presented { .. }));
    match o.kind {
        0 | 1 => {
            let limit = 1_000 + o.b % 8_001;
            Some((
                user,
                Action::SubmitRequest {
                    legs: vec![LegSpec {
                        contract: CONTRACT,
                        side: if o.a.is_multiple_of(2) {
                            Side::BuyYes
                        } else {
                            Side::BuyNo
                        },
                        qty: 10 + u64::from(o.a) % 990,
                        limit_price: price(limit),
                    }],
                    response_deadline: now + cfg.min_response + u64::from(o.b) % 3_000,
                },
            ))
        }
        2..=4 => {
            let r = choose(open(), o.a)?;
            let leg = &r.legs[0];
            // Mostly under the limit; one in four is above it (never selected) or short-lived
            // (not firm through the accept window, so never selected either).
            let bps = match o.b % 4 {
                0 => (leg.limit_price.bps() + 1 + o.b % 500).min(9_999),
                _ => leg.limit_price.bps().saturating_sub(o.b % 2_500).max(1),
            };
            let firm = r.response_deadline + cfg.accept_window;
            let expires_at = if o.a.is_multiple_of(4) {
                firm.saturating_sub(1 + u64::from(o.b) % 4_000).max(now + 1)
            } else {
                firm
            };
            Some((
                user,
                Action::SubmitQuote {
                    request: r.id,
                    leg: 0,
                    price: price(bps),
                    size: leg.qty,
                    expires_at,
                },
            ))
        }
        5 => {
            let live: Vec<QuoteId> = h
                .engine
                .quotes()
                .filter(|q| q.state.is_live())
                .map(|q| q.id)
                .collect();
            let q = h.engine.quote(pick(&live, o.a)?)?;
            Some((who(q.mm), Action::CancelQuote { quote: q.id }))
        }
        6 => {
            let r = choose(open(), o.a)?;
            Some((who(r.requester), Action::CancelRequest { request: r.id }))
        }
        7..=9 => {
            let r = choose(presented(), o.a)?;
            let picks = r
                .legs
                .iter()
                .enumerate()
                .filter_map(|(i, l)| {
                    let q = h.engine.quote(l.selected?)?;
                    Some(Pick {
                        leg: u32::try_from(i).ok()?,
                        quote: q.id,
                        price: q.price,
                    })
                })
                .collect();
            Some((who(r.requester), Action::Accept { request: r.id, picks }))
        }
        10 => {
            let r = choose(presented(), o.a)?;
            Some((who(r.requester), Action::Reject { request: r.id }))
        }
        11 => Some((
            who(ORACLE),
            Action::ProposeOutcome {
                contract: CONTRACT,
                outcome: [Outcome::Yes, Outcome::No, Outcome::Invalid][usize::from(o.a) % 3],
            },
        )),
        12 => Some((user, Action::Dispute { contract: CONTRACT })),
        13 => Some((
            who(ARBITER),
            Action::Rule {
                contract: CONTRACT,
                outcome: [Outcome::Yes, Outcome::No, Outcome::Invalid][usize::from(o.a) % 3],
            },
        )),
        14 => Some((
            user,
            Action::Withdraw {
                amount: dollars(1 + u64::from(o.a) % 200),
            },
        )),
        // 15: Resume is handled by the caller.
        _ => None,
    }
}

struct Run {
    h: Harness,
    events: Vec<Emitted>,
}

fn run_directed(ops: &[Op]) -> Run {
    let mut h = Harness::new(demo_config());
    for a in USERS {
        h.fund(a, FUNDED_EACH);
    }
    let out = h.user(
        OPERATOR,
        Action::ListContract {
            id: CONTRACT,
            description: "p".into(),
            oracle: ORACLE,
            trading_close: CLOSE,
            resolution_deadline: HARD,
            dispute_window: WINDOW,
        },
    );
    assert_eq!(rejection(&out), None);
    let mut events = out;
    for o in ops {
        let t = h.now() + u64::from(o.dt);
        h.set_time(t);
        let out = match (o.kind, directed(&h, o)) {
            // Resume (15) can land between any two commands; the other kinds fall back to a tick when
            // there is nothing to act on (e.g. no presented request to accept yet).
            (15, _) => h.submit(Command::Resume),
            (_, Some((who, action))) => h.user(who, action),
            (_, None) => h.submit(Command::Tick),
        };
        assert_sealed(&h, &out);
        events.extend(out);
    }
    let out = h.tick(h.now() + 200_000); // past the hard deadline: every contract is final
    assert_sealed(&h, &out);
    events.extend(out);
    Run { h, events }
}

fn check_run(run: &Run) -> Result<(), TestCaseError> {
    let Run { h, events } = run;

    // Money: all of it is free again and none is created or destroyed (I1, I2 hold after every
    // command; this is the end-to-end statement).
    let mut free_total: Amount = 0;
    for a in EVERYONE {
        let b = h.engine.balance(a);
        prop_assert_eq!(b.reserved, 0, "{:?}", a);
        prop_assert_eq!(b.escrowed, 0, "{:?}", a);
        free_total += b.free;
    }
    let withdrawn: Amount = events
        .iter()
        .filter_map(|e| match e.event {
            Event::Withdrawn { amount, .. } => Some(amount),
            _ => None,
        })
        .sum();
    prop_assert_eq!(free_total + withdrawn, dollars(USERS.len() as u64 * FUNDED_EACH));

    // No dead ends: nothing live is left anywhere once past the hard deadline.
    prop_assert!(h.engine.requests().all(|r| !r.state.is_live()));
    prop_assert!(h.engine.quotes().all(|q| !q.state.is_live()));
    let contract = h.engine.contract(CONTRACT).expect("listed");
    let ContractState::Final(outcome) = contract.state else {
        return Err(TestCaseError::fail(format!(
            "contract not final: {:?}",
            contract.state
        )));
    };
    prop_assert!(
        contract
            .positions
            .iter()
            .all(|p| h.engine.position(*p).expect("exists").settled)
    );

    // The resolution state machine, restated from the events alone: a ruling wins, else the oracle's
    // proposal stands (even if the arbiter never ruled), else the contract is Invalid.
    let proposed = events.iter().find_map(|e| match e.event {
        Event::OutcomeProposed { outcome, .. } => Some(outcome),
        _ => None,
    });
    let ruled = events.iter().find_map(|e| match e.event {
        Event::Ruled {
            outcome,
            bond_returned,
            ..
        } => Some((outcome, bond_returned)),
        _ => None,
    });
    let expected = match (ruled, proposed) {
        (Some((ruling, _)), _) => ruling,
        (None, Some(proposal)) => proposal,
        (None, None) => Outcome::Invalid,
    };
    prop_assert_eq!(outcome, expected);
    // Only an upheld proposal forfeits the dispute bond.
    let forfeited = matches!(ruled, Some((_, false)));
    prop_assert_eq!(
        h.engine.balance(TREASURY).free,
        if forfeited { dollars(100) } else { 0 }
    );
    // A dispute nobody ruled on ends at the hard deadline, which returns the bond and tells only
    // the disputer.
    let disputer = events.iter().find_map(|e| match e.event {
        Event::Disputed { disputer, .. } => Some(disputer),
        _ => None,
    });
    let returned: Vec<_> = events
        .iter()
        .filter_map(|e| match e.event {
            Event::DisputeBondReturned { disputer, amount, .. } => Some((e.audience, disputer, amount)),
            _ => None,
        })
        .collect();
    match (disputer, ruled) {
        (Some(d), None) => prop_assert_eq!(
            returned,
            vec![(Audience::Account(d), d, demo_config().dispute_bond)]
        ),
        _ => prop_assert!(returned.is_empty(), "{:?}", returned),
    }

    // Each position pays exactly what the final outcome says, and is announced to both holders.
    let mut settled = 0;
    for e in events {
        if let Event::PositionSettled {
            position,
            yes_payout,
            no_payout,
            ..
        } = e.event
        {
            settled += 1;
            let p = h.engine.position(position).expect("exists");
            let pot = p.qty * ONE_DOLLAR;
            let want = match outcome {
                Outcome::Yes => (pot, 0),
                Outcome::No => (0, pot),
                Outcome::Invalid => (p.yes_lock, p.no_lock),
            };
            prop_assert_eq!((yes_payout, no_payout), want, "position {:?}", position);
        }
    }
    prop_assert_eq!(settled, 2 * contract.positions.len());

    // Who got what, from the events alone: each account ends with what it was funded, minus what it
    // withdrew and locked into positions, plus what its positions paid out, and the dispute bond moves
    // from the disputer to the treasury only when the proposal was upheld. This catches a payout that
    // reaches the wrong account, which conserves money and so slips past I1–I5.
    let mut expected: BTreeMap<AccountId, i128> = USERS
        .iter()
        .map(|a| (*a, i128::from(dollars(FUNDED_EACH))))
        .collect();
    let mut priced = BTreeSet::new();
    for e in events {
        match e.event {
            Event::Withdrawn { account, amount } => {
                *expected.entry(account).or_default() -= i128::from(amount)
            }
            Event::PositionSettled {
                position,
                yes_holder,
                yes_payout,
                no_holder,
                no_payout,
            } if priced.insert(position) => {
                let p = h.engine.position(position).expect("exists");
                *expected.entry(yes_holder).or_default() += i128::from(yes_payout) - i128::from(p.yes_lock);
                *expected.entry(no_holder).or_default() += i128::from(no_payout) - i128::from(p.no_lock);
            }
            _ => {}
        }
    }
    if forfeited {
        let disputer = events
            .iter()
            .find_map(|e| match e.event {
                Event::Disputed { disputer, .. } => Some(disputer),
                _ => None,
            })
            .expect("a ruling implies a dispute");
        let bond = i128::from(demo_config().dispute_bond);
        *expected.entry(disputer).or_default() -= bond;
        *expected.entry(TREASURY).or_default() += bond;
    }
    for a in EVERYONE {
        prop_assert_eq!(
            i128::from(h.engine.balance(a).free),
            expected.get(&a).copied().unwrap_or(0),
            "{:?}",
            a
        );
    }

    // Replay determinism, with every `Resume` in the log.
    prop_assert_eq!(replay_hash(h), h.engine.state_hash());
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn invariants_hold_for_directed_streams(ops in prop::collection::vec(directed_op(), 30..300)) {
        check_run(&run_directed(&ops))?;
    }
}

/// The directed property is only worth anything if its streams get somewhere. Over a fixed
/// deterministic sample it must produce every lifecycle path at least once.
#[test]
fn directed_streams_reach_every_lifecycle_path() {
    let strategy = prop::collection::vec(directed_op(), 30..300);
    let mut runner = TestRunner::deterministic();
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    let (mut re_extended, mut upheld, mut overturned, mut finals) = (0, 0, 0, BTreeSet::new());
    for _ in 0..256 {
        let ops = strategy.new_tree(&mut runner).unwrap().current();
        let run = run_directed(&ops);
        seen.extend(run.events.iter().map(|e| e.event.name()));
        let proposals = run
            .events
            .iter()
            .filter(|e| matches!(e.event, Event::OutcomeProposed { .. }))
            .count();
        re_extended += usize::from(proposals > 1); // a window re-announced by Resume
        for e in &run.events {
            match e.event {
                Event::Ruled {
                    bond_returned: true, ..
                } => overturned += 1,
                Event::Ruled {
                    bond_returned: false, ..
                } => upheld += 1,
                Event::ContractFinalized { outcome, .. } => {
                    finals.insert(outcome);
                }
                _ => {}
            }
        }
    }
    for must in [
        "RequestOpened",
        "RequestPresented",
        "RequestFilled",
        "RequestExpired",
        "RequestCancelled",
        "RequestRejected",
        "QuoteSubmitted",
        "QuoteSelected",
        "QuoteFilled",
        "QuoteLost",
        "QuoteReleased",
        "QuoteExpired",
        "QuoteCancelled",
        "ContractClosed",
        "OutcomeProposed",
        "Disputed",
        "Ruled",
        "DisputeBondReturned",
        "ContractFinalized",
        "PositionSettled",
    ] {
        assert!(
            seen.contains(must),
            "no directed stream produced {must}; saw {seen:?}"
        );
    }
    assert!(re_extended > 0, "no stream had a Resume extend a dispute window");
    assert!(
        upheld > 0 && overturned > 0,
        "rulings: {upheld} upheld, {overturned} overturned"
    );
    assert_eq!(
        finals.len(),
        3,
        "contracts should end Yes, No and Invalid across the sample: {finals:?}"
    );
}
