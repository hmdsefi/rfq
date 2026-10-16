//! Multi-leg requests (docs/specs/engine.md §8): the five atomicity rules, one behaviour per test.
//! The harness checks I1–I5 after every command, so each test also proves the invariants hold.
//!
//! The setup: three markets (ids 1, 2, 3) and Alice's request for 400 YES on each at a sealed
//! limit of 0.70, so she holds 3 × 400 × 0.70 = $840. MM1 and MM2 quote the legs.
use rfq_domain::*;
use rfq_testkit::{accounts::*, *};

const CLOSE: Timestamp = 1_000_000;
const HARD: Timestamp = 2_000_000;
const DEADLINE: Timestamp = 10_000;
/// `Config::default()`'s accept window is 30 s.
const ACCEPT_BY: Timestamp = DEADLINE + 30_000;
const QTY: Qty = 400;
const LIMIT: u16 = 7_000;
const MARKETS: [MarketId; 3] = [MarketId(1), MarketId(2), MarketId(3)];

/// Funded accounts and the three listed markets, with no request yet.
fn markets() -> Harness {
    let mut h = Harness::new(Config::default());
    for a in [ALICE, MM1, MM2] {
        h.fund(a, 1_000);
    }
    for c in MARKETS {
        let out = h.user(
            OPERATOR,
            Action::ListMarket {
                id: c,
                description: format!("market {}", c.0),
                oracle: ORACLE,
                trading_close: CLOSE,
                resolution_deadline: HARD,
                dispute_window: 10_000,
            },
        );
        assert_eq!(rejection(&out), None);
    }
    h
}

fn leg(c: MarketId, side: Side, qty: Qty, limit: u16) -> LegSpec {
    LegSpec {
        market: c,
        side,
        qty,
        limit_price: price(limit),
    }
}

fn three_legs() -> Vec<LegSpec> {
    MARKETS.map(|c| leg(c, Side::BuyYes, QTY, LIMIT)).to_vec()
}

struct World {
    h: Harness,
    r: RequestId,
}

/// The markets plus Alice's three-leg request (Open until `DEADLINE`).
fn world() -> World {
    let mut h = markets();
    let r = RequestId(h.next_seq());
    let out = h.user(
        ALICE,
        Action::SubmitRequest {
            legs: three_legs(),
            response_deadline: DEADLINE,
        },
    );
    assert_eq!(rejection(&out), None);
    World { h, r }
}

/// `mm` quotes leg `leg` of the request at `bps`; the quote is firm through the accept window.
fn quote(w: &mut World, mm: AccountId, leg: u32, bps: u16) -> QuoteId {
    let id = QuoteId(w.h.next_seq());
    let out = w.h.user(
        mm,
        Action::SubmitQuote {
            request: w.r,
            leg,
            price: price(bps),
            size: QTY,
            expires_at: 100_000,
        },
    );
    assert_eq!(rejection(&out), None);
    id
}

/// MM1 quotes leg indices 0 and 2 (0.62, 0.64), MM2 index 1 (0.60): the shape of `multi_leg_happy`.
fn quote_every_leg(w: &mut World) -> [QuoteId; 3] {
    [
        quote(w, MM1, 0, 6_200),
        quote(w, MM2, 1, 6_000),
        quote(w, MM1, 2, 6_400),
    ]
}

/// Alice's picks, from the `RequestPresented` among `out`.
fn picks_in(out: &[Emitted]) -> Vec<Pick> {
    out.iter()
        .find_map(|e| match &e.event {
            Event::RequestPresented { picks, .. } => Some(picks.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("not presented: {:?}", names(out)))
}

/// The deadline fires: the request is `Presented` and these are Alice's picks.
fn present(w: &mut World) -> Vec<Pick> {
    picks_in(&w.h.tick(DEADLINE))
}

fn accept(w: &mut World, picks: Vec<Pick>) -> Vec<Emitted> {
    w.h.user(ALICE, Action::Accept { request: w.r, picks })
}

fn free(h: &Harness, a: AccountId) -> Amount {
    h.engine.balance(a).free
}

fn reserved(h: &Harness, a: AccountId) -> Amount {
    h.engine.balance(a).reserved
}

fn escrowed(h: &Harness, a: AccountId) -> Amount {
    h.engine.balance(a).escrowed
}

fn seller(bps: u16) -> Amount {
    seller_lock(QTY, price(bps)).expect("no overflow")
}

fn buyer(bps: u16) -> Amount {
    buyer_lock(QTY, price(bps)).expect("no overflow")
}

fn state(w: &World) -> RequestState {
    w.h.engine.request(w.r).expect("exists").state
}

fn quote_state(w: &World, q: QuoteId) -> QuoteState {
    w.h.engine.quote(q).expect("exists").state
}

fn count(out: &[Emitted], name: &str) -> usize {
    out.iter().filter(|e| e.event.name() == name).count()
}

fn no_positions(w: &World) -> bool {
    w.h.engine.markets().all(|c| c.positions.is_empty()) && w.h.engine.position(PositionId(1)).is_none()
}

// Rule 1: one hold covers the total, and every leg gets its own `RequestOpened` with its index.
#[test]
fn a_request_holds_the_total_once_and_announces_every_leg() {
    let mut h = markets();
    let r = RequestId(h.next_seq());
    let out = h.user(
        ALICE,
        Action::SubmitRequest {
            legs: three_legs(),
            response_deadline: DEADLINE,
        },
    );
    assert_eq!(names(&out), vec!["RequestOpened"; 3]);
    for (i, (e, c)) in out.iter().zip(MARKETS).enumerate() {
        assert_eq!(e.audience, Audience::Public);
        assert_eq!(
            e.event,
            Event::RequestOpened {
                request: r,
                leg: i as u32,
                market: c,
                side: Side::BuyYes,
                qty: QTY,
                response_deadline: DEADLINE,
            }
        );
    }
    assert_eq!(reserved(&h, ALICE), 3 * buyer(LIMIT));
    assert_eq!(reserved(&h, ALICE), dollars(840));
    assert_eq!(h.engine.request(r).expect("exists").legs.len(), 3);
}

// Rules 1-3: a quote on every leg presents three picks, one Accept fills all three, and every escrow matches its legs.
#[test]
fn three_legs_fill_in_one_accept() {
    let mut w = world();
    let q = quote_every_leg(&mut w);
    // MM1's two quotes (leg indices 0 and 2) are both live: a quote replaces only the same MM's quote on the same leg.
    assert_eq!(reserved(&w.h, MM1), seller(6_200) + seller(6_400));
    assert_eq!(reserved(&w.h, MM2), seller(6_000));

    let out = w.h.tick(DEADLINE);
    assert_eq!(
        names(&out),
        vec![
            "QuoteSelected",
            "QuoteSelected",
            "QuoteSelected",
            "RequestPresented"
        ]
    );
    let picks = picks_in(&out);
    assert_eq!(picks.len(), 3);
    for (i, (p, (qid, bps))) in picks
        .iter()
        .zip([(q[0], 6_200), (q[1], 6_000), (q[2], 6_400)])
        .enumerate()
    {
        assert_eq!(
            *p,
            Pick {
                leg: i as u32,
                quote: qid,
                price: price(bps)
            }
        );
    }
    assert_eq!(state(&w), RequestState::Presented { accept_by: ACCEPT_BY });

    let out = accept(&mut w, picks);
    assert_eq!(
        names(&out),
        vec!["QuoteFilled", "QuoteFilled", "QuoteFilled", "RequestFilled"]
    );
    let Some(Event::RequestFilled { positions, .. }) = out.last().map(|e| e.event.clone()) else {
        panic!("no RequestFilled")
    };
    assert_eq!(positions.len(), 3, "one position per leg");
    // In leg order (engine.md §7): position i is on leg i's market. The dashboard relies on this.
    for (pid, c) in positions.iter().zip(MARKETS) {
        let p = w.h.engine.position(*pid).expect("exists");
        assert_eq!((p.market, p.qty, p.yes_holder), (c, QTY, ALICE));
    }
    assert_eq!(state(&w), RequestState::Filled);

    // Each MM's escrow is its legs' seller locks; the requester's is the buyer locks at the picked prices.
    assert_eq!(escrowed(&w.h, MM1), seller(6_200) + seller(6_400));
    assert_eq!(escrowed(&w.h, MM2), seller(6_000));
    assert_eq!(escrowed(&w.h, ALICE), buyer(6_200) + buyer(6_000) + buyer(6_400));
    // The remainder of Alice's hold ($840 − what escrowed) went back to free in the same command.
    assert_eq!(reserved(&w.h, ALICE), 0);
    assert_eq!(free(&w.h, ALICE), dollars(1_000) - escrowed(&w.h, ALICE));
    for a in [MM1, MM2] {
        assert_eq!(reserved(&w.h, a), 0);
    }
}

// Rule 2: leg 2 of 3 has no quote, so the deadline expires the request and releases every hold in that command.
#[test]
fn leg_two_of_three_unfilled_expires_the_whole_request_and_releases_every_hold() {
    let mut w = world();
    let q0 = quote(&mut w, MM1, 0, 6_200);
    let q2 = quote(&mut w, MM2, 2, 6_000);
    // Before the deadline the holds are live and nothing is escrowed.
    assert_eq!(reserved(&w.h, ALICE), dollars(840));
    assert_eq!(reserved(&w.h, MM1), seller(6_200));
    assert_eq!(reserved(&w.h, MM2), seller(6_000));
    assert_eq!(state(&w), RequestState::Open);

    let out = w.h.tick(DEADLINE);

    assert_eq!(names(&out), vec!["QuoteLost", "QuoteLost", "RequestExpired"]);
    assert_eq!(state(&w), RequestState::Expired);
    assert_eq!(quote_state(&w, q0), QuoteState::Lost);
    assert_eq!(quote_state(&w, q2), QuoteState::Lost);
    for a in [ALICE, MM1, MM2] {
        assert_eq!(
            reserved(&w.h, a),
            0,
            "{a:?} still holds funds after the same command"
        );
        assert_eq!(escrowed(&w.h, a), 0);
        assert_eq!(free(&w.h, a), dollars(1_000), "{a:?} is back to its deposit");
    }
    assert!(no_positions(&w), "no leg filled");
}

// Rule 2: a quote over its leg's limit leaves that leg with no eligible quote, so the whole request expires.
#[test]
fn an_over_limit_quote_on_one_leg_expires_the_whole_request() {
    let mut w = world();
    let q = [
        quote(&mut w, MM1, 0, 6_200),
        quote(&mut w, MM2, 1, LIMIT + 100),
        quote(&mut w, MM1, 2, 6_400),
    ];

    let out = w.h.tick(DEADLINE);

    assert_eq!(
        names(&out),
        vec!["QuoteLost", "QuoteLost", "QuoteLost", "RequestExpired"]
    );
    assert_eq!(state(&w), RequestState::Expired);
    for id in q {
        assert_eq!(quote_state(&w, id), QuoteState::Lost);
    }
    for a in [ALICE, MM1, MM2] {
        assert_eq!(reserved(&w.h, a), 0);
        assert_eq!(free(&w.h, a), dollars(1_000));
    }
    assert!(no_positions(&w));
}

// Rule 4: with all three legs presented, the leg-2 MM cannot cancel (`InvalidState`) and Accept fills all three.
#[test]
fn a_selected_leg_cannot_be_withdrawn() {
    let mut w = world();
    let q = quote_every_leg(&mut w);
    let picks = present(&mut w);
    let before = (reserved(&w.h, MM2), reserved(&w.h, ALICE));

    let out = w.h.user(MM2, Action::CancelQuote { quote: q[1] });

    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(quote_state(&w, q[1]), QuoteState::Selected);
    assert_eq!(state(&w), RequestState::Presented { accept_by: ACCEPT_BY });
    assert_eq!(
        (reserved(&w.h, MM2), reserved(&w.h, ALICE)),
        before,
        "nothing was released"
    );

    let out = accept(&mut w, picks);
    assert_eq!(count(&out, "QuoteFilled"), 3);
    assert_eq!(count(&out, "RequestFilled"), 1);
    assert_eq!(state(&w), RequestState::Filled);
}

// Rule 4: Reject and the `accept_by` lapse each release every leg's quote and the requester's hold in one command.
#[test]
fn reject_and_lapse_release_every_leg_in_one_command() {
    for lapse in [false, true] {
        let mut w = world();
        let q = quote_every_leg(&mut w);
        present(&mut w);

        let out = if lapse {
            w.h.tick(ACCEPT_BY)
        } else {
            w.h.user(ALICE, Action::Reject { request: w.r })
        };

        assert_eq!(count(&out, "QuoteReleased"), 3, "lapse={lapse}");
        assert_eq!(count(&out, "RequestRejected"), 1, "lapse={lapse}");
        for id in q {
            assert_eq!(quote_state(&w, id), QuoteState::Released);
        }
        for a in [ALICE, MM1, MM2] {
            assert_eq!(reserved(&w.h, a), 0, "lapse={lapse} {a:?}");
            assert_eq!(free(&w.h, a), dollars(1_000), "lapse={lapse} {a:?}");
        }
        assert!(no_positions(&w));
    }
}

// Rule 3: Accept must echo every leg; a missing, extra, reordered or wrong pick is `Mismatch` and changes nothing.
#[test]
fn accept_must_echo_every_leg() {
    let mut w = world();
    quote_every_leg(&mut w);
    let picks = present(&mut w);

    let mut missing = picks.clone();
    missing.pop();
    let mut extra = picks.clone();
    extra.push(picks[0]);
    let mut reordered = picks.clone();
    reordered.swap(0, 1);
    let mut wrong_price = picks.clone();
    wrong_price[1].price = price(5_900);
    let mut wrong_quote = picks.clone();
    wrong_quote[1].quote = picks[0].quote;
    let mut wrong_leg = picks.clone();
    wrong_leg[2].leg = 5;

    let balances = |h: &Harness| [ALICE, MM1, MM2].map(|a| h.engine.balance(a));
    let before = balances(&w.h);
    for (what, bad) in [
        ("a missing pick", missing),
        ("an extra pick", extra),
        ("reordered picks", reordered),
        ("a wrong price", wrong_price),
        ("another leg's quote", wrong_quote),
        ("a wrong leg index", wrong_leg),
        ("no picks", Vec::new()),
    ] {
        let out = accept(&mut w, bad);
        assert_eq!(rejection(&out), Some(RejectReason::Mismatch), "{what}");
        assert_eq!(
            state(&w),
            RequestState::Presented { accept_by: ACCEPT_BY },
            "{what}"
        );
        assert_eq!(balances(&w.h), before, "{what} changed a balance");
    }

    let out = accept(&mut w, picks);
    assert_eq!(count(&out, "RequestFilled"), 1);
}

// Leg cap: no legs and `MAX_LEGS + 1` legs are `InvalidInput` and hold nothing; `MAX_LEGS` legs are accepted.
#[test]
fn leg_cap() {
    let mut h = markets();
    let legs = |n: usize| vec![leg(MARKETS[0], Side::BuyYes, 100, LIMIT); n];
    let submit = |h: &mut Harness, n: usize| {
        let r = RequestId(h.next_seq());
        let out = h.user(
            ALICE,
            Action::SubmitRequest {
                legs: legs(n),
                response_deadline: DEADLINE,
            },
        );
        (r, out)
    };
    assert_eq!(MAX_LEGS, 8);

    for n in [0, MAX_LEGS + 1] {
        let (_, out) = submit(&mut h, n);
        assert_eq!(rejection(&out), Some(RejectReason::InvalidInput), "{n} legs");
        assert_eq!(h.engine.balance(ALICE).reserved, 0, "{n} legs took a hold");
    }

    let (r, out) = submit(&mut h, MAX_LEGS);
    assert_eq!(rejection(&out), None);
    assert_eq!(names(&out), vec!["RequestOpened"; MAX_LEGS]);
    assert_eq!(h.engine.request(r).expect("exists").legs.len(), MAX_LEGS);
    assert_eq!(
        h.engine.balance(ALICE).reserved,
        MAX_LEGS as u64 * buyer_lock(100, price(LIMIT)).expect("no overflow")
    );
}

// A quote for a leg index past the request's legs is `InvalidInput` and takes no hold.
#[test]
fn a_quote_for_a_missing_leg_is_rejected() {
    let mut w = world();
    for leg in [3, 4, u32::MAX] {
        let out = w.h.user(
            MM1,
            Action::SubmitQuote {
                request: w.r,
                leg,
                price: price(6_200),
                size: QTY,
                expires_at: 100_000,
            },
        );
        assert_eq!(
            rejection(&out),
            Some(RejectReason::InvalidInput),
            "leg {leg} of 3"
        );
    }
    assert_eq!(reserved(&w.h, MM1), 0, "a rejected quote holds nothing");
    assert!(
        w.h.engine
            .request(w.r)
            .expect("exists")
            .legs
            .iter()
            .all(|l| l.quotes.is_empty())
    );

    // A one-leg request has only leg 0.
    let single = RequestId(w.h.next_seq());
    let out = w.h.user(
        ALICE,
        Action::SubmitRequest {
            legs: vec![leg(MARKETS[0], Side::BuyYes, 100, LIMIT)],
            response_deadline: DEADLINE,
        },
    );
    assert_eq!(rejection(&out), None);
    let quote_on = |h: &mut Harness, leg| {
        rejection(&h.user(
            MM1,
            Action::SubmitQuote {
                request: single,
                leg,
                price: price(6_200),
                size: 100,
                expires_at: 100_000,
            },
        ))
    };
    assert_eq!(quote_on(&mut w.h, 1), Some(RejectReason::InvalidInput));
    assert_eq!(quote_on(&mut w.h, 0), None, "leg 0 is fine");
}

// Each leg of a mixed-side request puts the requester on its own side of its own position.
#[test]
fn mixed_sides_fill_with_the_right_holders() {
    let mut h = markets();
    let r = RequestId(h.next_seq());
    let legs = vec![
        leg(MARKETS[0], Side::BuyYes, QTY, LIMIT),
        leg(MARKETS[1], Side::BuyNo, QTY, LIMIT),
    ];
    assert_eq!(
        rejection(&h.user(
            ALICE,
            Action::SubmitRequest {
                legs,
                response_deadline: DEADLINE
            }
        )),
        None
    );
    let mut w = World { h, r };
    quote(&mut w, MM1, 0, 6_200);
    quote(&mut w, MM2, 1, 6_000);
    let picks = present(&mut w);
    let out = accept(&mut w, picks);
    let Some(Event::RequestFilled { positions, .. }) = out.last().map(|e| e.event.clone()) else {
        panic!("no RequestFilled: {:?}", names(&out))
    };
    let p0 = w.h.engine.position(positions[0]).expect("exists");
    let p1 = w.h.engine.position(positions[1]).expect("exists");
    assert_eq!((p0.yes_holder, p0.no_holder), (ALICE, MM1));
    assert_eq!((p1.yes_holder, p1.no_holder), (MM2, ALICE));
    assert_eq!((p0.yes_lock, p0.no_lock), (buyer(6_200), seller(6_200)));
    assert_eq!((p1.no_lock, p1.yes_lock), (buyer(6_000), seller(6_000)));
}

// ---------------------------------------------------------------------------------------------
// Random multi-leg streams. The harness checks I1–I5 after every command; this adds the atomicity
// statement itself: a request ends with all of its legs filled or none of them, and all money is free again.
// ---------------------------------------------------------------------------------------------
mod streams {
    use std::collections::BTreeSet;

    use proptest::prelude::*;
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    use rfq_testkit::scenarios::demo_config;

    use super::*;

    const USERS: [AccountId; 4] = [ALICE, MM1, MM2, CAROL];
    const FUNDED_EACH: u64 = 10_000;
    const STREAM_CLOSE: Timestamp = 40_000;
    const STREAM_HARD: Timestamp = 90_000;

    #[derive(Debug, Clone)]
    struct Op {
        who: usize,
        kind: u8,
        a: u16,
        b: u16,
        dt: u32,
    }

    fn op() -> impl Strategy<Value = Op> {
        (0usize..4, 0u8..10, any::<u16>(), any::<u16>(), 0u32..700).prop_map(|(who, kind, a, b, dt)| Op {
            who,
            kind,
            a,
            b,
            dt,
        })
    }

    fn choose<T>(items: Vec<T>, i: u16) -> Option<T> {
        let n = items.len();
        items.into_iter().nth(usize::from(i) % n.max(1))
    }

    fn action(h: &Harness, o: &Op) -> Option<(AccountId, Action)> {
        let user = USERS[o.who];
        let in_state = |f: fn(&RequestState) -> bool| -> Vec<&Request> {
            h.engine.requests().filter(|r| f(&r.state)).collect()
        };
        match o.kind {
            0 | 1 => {
                // 1..=9 legs: nine is over the cap and must be rejected, not applied.
                let n = 1 + usize::from(o.a) % (MAX_LEGS + 1);
                let legs = (0..n)
                    .map(|i| LegSpec {
                        market: MARKETS[(usize::from(o.a >> 3) + i) % 3],
                        side: if (o.b >> i) & 1 == 0 {
                            Side::BuyYes
                        } else {
                            Side::BuyNo
                        },
                        qty: 10 + (u64::from(o.a) + 37 * i as u64) % 300,
                        limit_price: price(1_000 + ((usize::from(o.b) * 7 + 911 * i) % 8_000) as u16),
                    })
                    .collect();
                Some((
                    user,
                    Action::SubmitRequest {
                        legs,
                        response_deadline: h.now() + 500 + u64::from(o.b) % 2_500,
                    },
                ))
            }
            2..=4 => {
                let r = choose(in_state(|s| matches!(s, RequestState::Open)), o.a)?;
                // One past the last leg is a rejected quote.
                let i = usize::from(o.b) % (r.legs.len() + 1);
                let (qty, limit) = r
                    .legs
                    .get(i)
                    .map_or((10, 5_000), |l| (l.qty, l.limit_price.bps()));
                let bps = if o.b.is_multiple_of(4) {
                    (limit + 1 + o.b % 300).min(9_999)
                } else {
                    limit.saturating_sub(o.b % 900).max(1)
                };
                Some((
                    user,
                    Action::SubmitQuote {
                        request: r.id,
                        leg: u32::try_from(i).ok()?,
                        price: price(bps),
                        size: qty,
                        expires_at: r.response_deadline + demo_config().accept_window,
                    },
                ))
            }
            5 => {
                let live: Vec<&Quote> = h.engine.quotes().filter(|q| q.state.is_live()).collect();
                let q = choose(live, o.a)?;
                Some((q.mm, Action::CancelQuote { quote: q.id }))
            }
            6 => {
                let r = choose(in_state(|s| matches!(s, RequestState::Open)), o.a)?;
                Some((r.requester, Action::CancelRequest { request: r.id }))
            }
            7 => {
                let r = choose(in_state(|s| matches!(s, RequestState::Presented { .. })), o.a)?;
                let mut picks: Vec<Pick> = r
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
                if o.b.is_multiple_of(5) {
                    picks.pop(); // one in five misses a leg: `Mismatch`, nothing applied
                }
                Some((r.requester, Action::Accept { request: r.id, picks }))
            }
            8 => {
                let r = choose(in_state(|s| matches!(s, RequestState::Presented { .. })), o.a)?;
                Some((r.requester, Action::Reject { request: r.id }))
            }
            _ => None, // 9: Resume, handled by the caller
        }
    }

    fn run(ops: &[Op]) -> Harness {
        let mut h = Harness::new(demo_config());
        for a in USERS {
            h.fund(a, FUNDED_EACH);
        }
        for c in MARKETS {
            let out = h.user(
                OPERATOR,
                Action::ListMarket {
                    id: c,
                    description: "m".into(),
                    oracle: ORACLE,
                    trading_close: STREAM_CLOSE,
                    resolution_deadline: STREAM_HARD,
                    dispute_window: 5_000,
                },
            );
            assert_eq!(rejection(&out), None);
        }
        for o in ops {
            let t = h.now() + u64::from(o.dt);
            h.set_time(t);
            let out = match (o.kind, action(&h, o)) {
                (9, _) => h.submit(Command::Resume),
                (_, Some((who, a))) => h.user(who, a),
                (_, None) => h.submit(Command::Tick),
            };
            assert_sealed(&h, &out);
        }
        let out = h.tick(h.now() + 200_000); // past every hard deadline
        assert_sealed(&h, &out);
        h
    }

    fn check(h: &Harness) -> Result<(), TestCaseError> {
        // All money is free again, none created or destroyed.
        let mut total: Amount = 0;
        for a in USERS {
            let b = h.engine.balance(a);
            prop_assert_eq!((b.reserved, b.escrowed), (0, 0), "{:?}", a);
            total += b.free;
        }
        prop_assert_eq!(total, dollars(USERS.len() as u64 * FUNDED_EACH));
        prop_assert!(h.engine.requests().all(|r| !r.state.is_live()));
        prop_assert!(h.engine.quotes().all(|q| !q.state.is_live()));

        // Atomicity: every request has all of its legs' positions or none, and only a `Filled` one has any.
        for r in h.engine.requests() {
            let positions = h
                .engine
                .markets()
                .flat_map(|c| c.positions.iter())
                .filter(|p| h.engine.position(**p).is_some_and(|p| p.request == r.id))
                .count();
            if r.state == RequestState::Filled {
                prop_assert_eq!(positions, r.legs.len(), "{:?}", r.id);
            } else {
                prop_assert_eq!(positions, 0, "{:?} ended {:?}", r.id, r.state);
            }
            // Its quotes agree: one `Filled` quote per leg with the request, none without it (the
            // other quotes on a filled request lost, expired or were replaced or cancelled).
            let filled_quotes = h
                .engine
                .quotes()
                .filter(|q| q.request == r.id && q.state == QuoteState::Filled)
                .count();
            prop_assert_eq!(filled_quotes, positions, "{:?}", r.id);
        }

        let mut replayed = Engine::new();
        for sc in &h.log {
            replayed.apply(sc);
        }
        prop_assert_eq!(replayed.state_hash(), h.engine.state_hash());
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn invariants_and_atomicity_hold_for_random_multi_leg_streams(
            ops in prop::collection::vec(op(), 30..250)
        ) {
            check(&run(&ops))?;
        }
    }

    /// The property above is only worth anything if its streams fill and expire requests of several legs.
    #[test]
    fn the_streams_fill_and_expire_multi_leg_requests() {
        let strategy = prop::collection::vec(op(), 30..250);
        let mut runner = TestRunner::deterministic();
        let (mut filled, mut expired, mut lapsed, mut over_cap) = (0, 0, 0, 0);
        let mut leg_counts = BTreeSet::new();
        for _ in 0..256 {
            let ops = strategy.new_tree(&mut runner).unwrap().current();
            let h = run(&ops);
            for r in h.engine.requests().filter(|r| r.legs.len() > 1) {
                leg_counts.insert(r.legs.len());
                match r.state {
                    RequestState::Filled => filled += 1,
                    RequestState::Expired => expired += 1,
                    RequestState::Rejected(_) => lapsed += 1,
                    _ => {}
                }
            }
            over_cap += h
                .log
                .iter()
                .filter(|sc| matches!(&sc.cmd, Command::User { action: Action::SubmitRequest { legs, .. }, .. } if legs.len() > MAX_LEGS))
                .count();
        }
        assert!(filled > 0, "no multi-leg request was ever filled");
        assert!(expired > 0, "no multi-leg request ever expired");
        assert!(lapsed > 0, "no multi-leg request was ever rejected or lapsed");
        assert!(over_cap > 0, "no stream tried more than MAX_LEGS legs");
        assert!(leg_counts.len() >= 4, "too little variety: {leg_counts:?}");
    }
}
