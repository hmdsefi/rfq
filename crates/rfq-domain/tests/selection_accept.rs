use rfq_domain::*;
use rfq_testkit::{accounts::*, *};

const DEADLINE: Timestamp = 10_000;
const ACCEPT_BY: Timestamp = DEADLINE + 30_000;

struct World {
    h: Harness,
    c: MarketId,
    r: RequestId,
}

fn world(side: Side) -> World {
    let mut h = Harness::new(Config::default());
    for a in [ALICE, MM1, MM2] {
        h.fund(a, 1_000);
    }
    let c = MarketId(1);
    h.user(
        OPERATOR,
        Action::ListMarket {
            id: c,
            description: "q".into(),
            oracle: ORACLE,
            trading_close: 1_000_000,
            resolution_deadline: 2_000_000,
            dispute_window: 10_000,
        },
    );
    let r = RequestId(h.next_seq());
    let out = h.user(
        ALICE,
        Action::SubmitRequest {
            legs: vec![LegSpec {
                market: c,
                side,
                qty: 1_000,
                limit_price: price(7_000),
            }],
            response_deadline: DEADLINE,
        },
    );
    assert_eq!(rejection(&out), None);
    World { h, c, r }
}

fn quote(w: &mut World, mm: AccountId, bps: u16, expires_at: Timestamp) -> QuoteId {
    let id = QuoteId(w.h.next_seq());
    let out = w.h.user(
        mm,
        Action::SubmitQuote {
            request: w.r,
            leg: 0,
            price: price(bps),
            size: 1_000,
            expires_at,
        },
    );
    assert_eq!(rejection(&out), None);
    id
}

fn picks(q: QuoteId, bps: u16) -> Vec<Pick> {
    vec![Pick {
        leg: 0,
        quote: q,
        price: price(bps),
    }]
}

#[test]
fn best_quote_is_presented_and_losers_released() {
    let mut w = world(Side::BuyYes);
    let q1 = quote(&mut w, MM1, 6_200, 100_000);
    let q2 = quote(&mut w, MM2, 6_000, 100_000);
    let out = w.h.tick(DEADLINE);
    assert_eq!(
        names(&out),
        vec!["QuoteLost", "QuoteSelected", "RequestPresented"]
    );
    assert_sealed(&w.h, &out); // QuoteLost/QuoteSelected only to their MM, RequestPresented only to ALICE
    assert_eq!(
        w.h.engine.request(w.r).unwrap().state,
        RequestState::Presented { accept_by: ACCEPT_BY }
    );
    assert_eq!(w.h.engine.quote(q1).unwrap().state, QuoteState::Lost);
    assert_eq!(w.h.engine.quote(q2).unwrap().state, QuoteState::Selected);
    assert_eq!(w.h.engine.balance(MM1).free, dollars(1_000));
    let presented = out.iter().find(|e| e.event.name() == "RequestPresented").unwrap();
    assert_eq!(presented.audience, Audience::Account(ALICE));
    assert_eq!(
        presented.event,
        Event::RequestPresented {
            request: w.r,
            picks: picks(q2, 6_000),
            accept_by: ACCEPT_BY
        }
    );
}

#[test]
fn tie_goes_to_earliest_quote() {
    let mut w = world(Side::BuyYes);
    let q1 = quote(&mut w, MM1, 6_000, 100_000);
    quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    assert_eq!(w.h.engine.quote(q1).unwrap().state, QuoteState::Selected);
}

#[test]
fn short_expiry_not_selected() {
    let mut w = world(Side::BuyYes);
    quote(&mut w, MM1, 5_000, ACCEPT_BY - 1); // cheaper, but not firm through the window
    let q2 = quote(&mut w, MM2, 6_000, ACCEPT_BY);
    w.h.tick(DEADLINE);
    assert_eq!(w.h.engine.quote(q2).unwrap().state, QuoteState::Selected);
}

// Controller ruling R23: the limit is inclusive. Only a price strictly above it is ineligible.
#[test]
fn quote_at_exact_limit_is_selected() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM1, 7_000, 100_000); // exactly ALICE's 0.70 limit
    let out = w.h.tick(DEADLINE);
    assert_eq!(names(&out), vec!["QuoteSelected", "RequestPresented"]);
    assert_sealed(&w.h, &out);
    assert_eq!(w.h.engine.quote(q).unwrap().state, QuoteState::Selected);
    assert_eq!(
        w.h.engine.request(w.r).unwrap().state,
        RequestState::Presented { accept_by: ACCEPT_BY }
    );
}

// Controller ruling R23: pins the timer order at one timestamp. `QuoteExpiry` fires before
// `RequestDeadline` (variant order), so a quote expiring exactly at the deadline is Expired. If the
// request deadline fired first it would drop the quote as ineligible (it is not firm through the
// accept window) and the quote would end Lost instead.
#[test]
fn quote_expiring_at_deadline_is_expired_not_lost() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM1, 6_000, DEADLINE);
    let out = w.h.tick(DEADLINE);
    assert_eq!(names(&out), vec!["QuoteExpired", "RequestExpired"]);
    assert_sealed(&w.h, &out);
    assert_eq!(w.h.engine.quote(q).unwrap().state, QuoteState::Expired);
    assert_eq!(w.h.engine.request(w.r).unwrap().state, RequestState::Expired);
    for a in [ALICE, MM1] {
        assert_eq!(w.h.engine.balance(a).free, dollars(1_000));
    }
}

#[test]
fn no_quotes_expires_request() {
    let mut w = world(Side::BuyYes);
    let out = w.h.tick(DEADLINE);
    assert_eq!(names(&out), vec!["RequestExpired"]);
    assert_sealed(&w.h, &out);
    assert_eq!(w.h.engine.balance(ALICE).free, dollars(1_000));
}

#[test]
fn accept_escrows_exact_amounts_buy_yes() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    let out = w.h.user(
        ALICE,
        Action::Accept {
            request: w.r,
            picks: picks(q, 6_000),
        },
    );
    assert_eq!(names(&out), vec!["QuoteFilled", "RequestFilled"]);
    assert_sealed(&w.h, &out); // QuoteFilled only to MM2, RequestFilled only to ALICE
    assert_eq!(
        w.h.engine.balance(ALICE),
        Balance {
            free: dollars(400),
            reserved: 0,
            escrowed: dollars(600)
        }
    );
    assert_eq!(
        w.h.engine.balance(MM2),
        Balance {
            free: dollars(600),
            reserved: 0,
            escrowed: dollars(400)
        }
    );
    let p = w.h.engine.position(PositionId(1)).unwrap();
    assert_eq!(
        (p.yes_holder, p.yes_lock, p.no_holder, p.no_lock),
        (ALICE, dollars(600), MM2, dollars(400))
    );
    assert_eq!(w.h.engine.market(w.c).unwrap().positions, vec![PositionId(1)]);
}

#[test]
fn accept_buy_no_maps_sides() {
    let mut w = world(Side::BuyNo);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    w.h.user(
        ALICE,
        Action::Accept {
            request: w.r,
            picks: picks(q, 6_000),
        },
    );
    let p = w.h.engine.position(PositionId(1)).unwrap();
    assert_eq!(
        (p.yes_holder, p.yes_lock, p.no_holder, p.no_lock),
        (MM2, dollars(400), ALICE, dollars(600))
    );
}

#[test]
fn accept_must_echo_exact_selection() {
    let mut w = world(Side::BuyYes);
    let q1 = quote(&mut w, MM1, 6_200, 100_000);
    let q2 = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    let mut accept = |p| {
        rejection(&w.h.user(
            ALICE,
            Action::Accept {
                request: w.r,
                picks: p,
            },
        ))
    };
    assert_eq!(accept(picks(q2, 5_999)), Some(RejectReason::Mismatch));
    assert_eq!(accept(picks(q1, 6_200)), Some(RejectReason::Mismatch));
    assert_eq!(accept(vec![]), Some(RejectReason::Mismatch));
    let out = w.h.user(
        MM1,
        Action::Accept {
            request: w.r,
            picks: picks(q2, 6_000),
        },
    );
    assert_eq!(rejection(&out), Some(RejectReason::Unauthorized));
}

#[test]
fn double_accept_rejected() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    assert_eq!(
        rejection(&w.h.user(
            ALICE,
            Action::Accept {
                request: w.r,
                picks: picks(q, 6_000)
            }
        )),
        None
    );
    let out = w.h.user(
        ALICE,
        Action::Accept {
            request: w.r,
            picks: picks(q, 6_000),
        },
    );
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(
        rejection(&w.h.user(ALICE, Action::Reject { request: w.r })),
        Some(RejectReason::InvalidState)
    );
}

#[test]
fn reject_releases_everything() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    let out = w.h.user(ALICE, Action::Reject { request: w.r });
    assert_eq!(names(&out), vec!["QuoteReleased", "RequestRejected"]);
    assert_sealed(&w.h, &out); // QuoteReleased only to MM2
    assert_eq!(
        w.h.engine.request(w.r).unwrap().state,
        RequestState::Rejected(RejectCause::Declined)
    );
    assert_eq!(w.h.engine.quote(q).unwrap().state, QuoteState::Released);
    assert_eq!(w.h.engine.balance(ALICE).free, dollars(1_000));
    assert_eq!(w.h.engine.balance(MM2).free, dollars(1_000));
}

#[test]
fn accept_after_window_rejected_without_tick() {
    // The quote's expiry is clamped to ACCEPT_BY, so its timer fires first at ACCEPT_BY as a no-op
    // (the quote is Selected) and AcceptBy then lapses the request.
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    w.h.set_time(ACCEPT_BY);
    let out = w.h.user(
        ALICE,
        Action::Accept {
            request: w.r,
            picks: picks(q, 6_000),
        },
    );
    assert_eq!(
        names(&out),
        vec!["QuoteReleased", "RequestRejected", "CommandRejected"]
    );
    assert_sealed(&w.h, &out);
    assert_eq!(
        w.h.engine.request(w.r).unwrap().state,
        RequestState::Rejected(RejectCause::Lapsed)
    );
}

#[test]
fn idle_engine_fires_deadline_then_lapse_in_one_command() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.set_time(ACCEPT_BY + 5_000); // no tick since the quote
    let out = w.h.user(
        ALICE,
        Action::Accept {
            request: w.r,
            picks: picks(q, 6_000),
        },
    );
    assert_eq!(
        names(&out),
        vec![
            "QuoteSelected",
            "RequestPresented",
            "QuoteReleased",
            "RequestRejected",
            "CommandRejected"
        ]
    );
    assert_sealed(&w.h, &out);
    assert_eq!(w.h.engine.balance(ALICE).free, dollars(1_000));
}

#[test]
fn over_limit_quote_accepted_but_never_selected() {
    // SubmitQuote doesn't check the sealed limit (no oracle for it); selection does.
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM1, 7_001, 100_000); // above ALICE's 0.70 limit, yet accepted
    assert_eq!(w.h.engine.quote(q).unwrap().state, QuoteState::Active);
    let out = w.h.tick(DEADLINE);
    // It is the only quote: without the eligibility check it would have been selected.
    assert_eq!(names(&out), vec!["QuoteLost", "RequestExpired"]);
    assert_sealed(&w.h, &out);
    assert_eq!(w.h.engine.quote(q).unwrap().state, QuoteState::Lost);
    for a in [ALICE, MM1] {
        assert_eq!(w.h.engine.balance(a).free, dollars(1_000));
    }
}

#[test]
fn cancel_before_deadline_removes_quote() {
    let mut w = world(Side::BuyYes);
    let q1 = quote(&mut w, MM1, 6_000, 100_000);
    let q2 = quote(&mut w, MM2, 6_200, 100_000);
    w.h.set_time(DEADLINE - 1);
    let out = w.h.user(MM1, Action::CancelQuote { quote: q1 });
    assert_eq!(names(&out), vec!["QuoteCancelled"]);
    assert_sealed(&w.h, &out);
    assert_eq!(w.h.engine.balance(MM1).free, dollars(1_000));
    // A cancel stamped at the deadline is sequenced after selection: the quote is already Selected.
    w.h.set_time(DEADLINE);
    let out = w.h.user(MM2, Action::CancelQuote { quote: q2 });
    assert_eq!(
        names(&out),
        vec!["QuoteSelected", "RequestPresented", "CommandRejected"]
    );
    assert_sealed(&w.h, &out);
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(w.h.engine.quote(q2).unwrap().state, QuoteState::Selected);
}

#[test]
fn cancel_after_selection_rejected() {
    let mut w = world(Side::BuyYes);
    let q1 = quote(&mut w, MM1, 6_200, 100_000);
    let q2 = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    // Selected quotes are firm through accept_by (RFC P5); a lost quote is already released.
    assert_eq!(
        rejection(&w.h.user(MM2, Action::CancelQuote { quote: q2 })),
        Some(RejectReason::InvalidState)
    );
    assert_eq!(
        rejection(&w.h.user(MM1, Action::CancelQuote { quote: q1 })),
        Some(RejectReason::InvalidState)
    );
    assert_eq!(w.h.engine.quote(q2).unwrap().state, QuoteState::Selected);
    assert_eq!(w.h.engine.balance(MM2).reserved, dollars(400));
    let out = w.h.user(
        ALICE,
        Action::Accept {
            request: w.r,
            picks: picks(q2, 6_000),
        },
    );
    assert_eq!(names(&out), vec!["QuoteFilled", "RequestFilled"]);
    assert_sealed(&w.h, &out);
    assert_eq!(w.h.engine.balance(MM2).escrowed, dollars(400));
}

// Added beyond the brief: the lapse without a user command in the way, and the actor/state guards
// of Accept and Reject.
#[test]
fn lapse_at_accept_by_releases_everything() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    assert_eq!(w.h.tick(ACCEPT_BY - 1), vec![], "still inside the window");
    let out = w.h.tick(ACCEPT_BY);
    assert_eq!(names(&out), vec!["QuoteReleased", "RequestRejected"]);
    assert_sealed(&w.h, &out);
    assert_eq!(
        out[1],
        Emitted::public(Event::RequestRejected {
            request: w.r,
            cause: RejectCause::Lapsed
        })
    );
    assert_eq!(w.h.engine.quote(q).unwrap().state, QuoteState::Released);
    for a in [ALICE, MM2] {
        assert_eq!(w.h.engine.balance(a).free, dollars(1_000));
    }
}

#[test]
fn accept_and_reject_guard_actor_and_state() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    let accept = |w: &mut World, actor, request| {
        rejection(&w.h.user(
            actor,
            Action::Accept {
                request,
                picks: picks(q, 6_000),
            },
        ))
    };
    // Still Open: there is nothing to accept or reject yet.
    let r = w.r;
    assert_eq!(accept(&mut w, ALICE, r), Some(RejectReason::InvalidState));
    assert_eq!(
        rejection(&w.h.user(ALICE, Action::Reject { request: w.r })),
        Some(RejectReason::InvalidState)
    );
    let unknown = RequestId(999);
    assert_eq!(accept(&mut w, ALICE, unknown), Some(RejectReason::UnknownObject));
    assert_eq!(
        rejection(&w.h.user(ALICE, Action::Reject { request: unknown })),
        Some(RejectReason::UnknownObject)
    );
    w.h.tick(DEADLINE);
    assert_eq!(
        rejection(&w.h.user(MM2, Action::Reject { request: w.r })),
        Some(RejectReason::Unauthorized)
    );
    assert_eq!(
        w.h.engine.request(w.r).unwrap().state,
        RequestState::Presented { accept_by: ACCEPT_BY }
    );
}
