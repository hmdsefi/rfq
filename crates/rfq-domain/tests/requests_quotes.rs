use rfq_domain::*;
use rfq_testkit::{accounts::*, *};

pub const CLOSE: Timestamp = 1_000_000;
pub const HARD: Timestamp = 2_000_000;
pub const DEADLINE: Timestamp = 10_000;

/// Funded accounts and one listed contract.
fn setup() -> (Harness, ContractId) {
    let mut h = Harness::new(Config::default());
    for a in [ALICE, MM1, MM2, CAROL] {
        h.fund(a, 1_000);
    }
    let c = ContractId(1);
    let out = h.user(
        OPERATOR,
        Action::ListContract {
            id: c,
            description: "Will it rain in Paris on 2026-11-01?".into(),
            oracle: ORACLE,
            trading_close: CLOSE,
            resolution_deadline: HARD,
            dispute_window: 10_000,
        },
    );
    assert_eq!(rejection(&out), None);
    (h, c)
}

fn leg(c: ContractId, qty: Qty, limit: u16) -> LegSpec {
    LegSpec {
        contract: c,
        side: Side::BuyYes,
        qty,
        limit_price: price(limit),
    }
}

fn request(h: &mut Harness, c: ContractId) -> RequestId {
    let id = RequestId(h.next_seq());
    let out = h.user(
        ALICE,
        Action::SubmitRequest {
            legs: vec![leg(c, 1_000, 7_000)],
            response_deadline: DEADLINE,
        },
    );
    assert_eq!(rejection(&out), None);
    id
}

fn quote_out(h: &mut Harness, mm: AccountId, r: RequestId, bps: u16) -> (QuoteId, Vec<Emitted>) {
    let id = QuoteId(h.next_seq());
    let out = h.user(
        mm,
        Action::SubmitQuote {
            request: r,
            leg: 0,
            price: price(bps),
            size: 1_000,
            expires_at: 100_000,
        },
    );
    assert_eq!(rejection(&out), None);
    (id, out)
}

fn quote(h: &mut Harness, mm: AccountId, r: RequestId, bps: u16) -> QuoteId {
    quote_out(h, mm, r, bps).0
}

#[test]
fn list_contract_requires_operator_and_valid_times() {
    let (mut h, _) = setup();
    let list = |h: &mut Harness, actor, id, oracle, close, hard| {
        h.user(
            actor,
            Action::ListContract {
                id: ContractId(id),
                description: "x".into(),
                oracle,
                trading_close: close,
                resolution_deadline: hard,
                dispute_window: 1,
            },
        )
    };
    assert_eq!(
        rejection(&list(&mut h, ALICE, 2, ORACLE, CLOSE, HARD)),
        Some(RejectReason::Unauthorized)
    );
    assert_eq!(
        rejection(&list(&mut h, OPERATOR, 1, ORACLE, CLOSE, HARD)),
        Some(RejectReason::InvalidInput)
    );
    assert_eq!(
        rejection(&list(&mut h, OPERATOR, 3, ORACLE, HARD, CLOSE)),
        Some(RejectReason::InvalidInput)
    );
    for role in [OPERATOR, ARBITER, TREASURY] {
        assert_eq!(
            rejection(&list(&mut h, OPERATOR, 4, role, CLOSE, HARD)),
            Some(RejectReason::InvalidInput),
            "oracle {role:?} holds a venue role"
        );
    }
}

#[test]
fn submit_request_holds_qty_times_limit() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    assert_eq!(
        h.engine.balance(ALICE),
        Balance {
            free: dollars(300),
            reserved: dollars(700),
            escrowed: 0
        }
    );
    assert_eq!(h.engine.request(r).unwrap().state, RequestState::Open);
}

#[test]
fn submit_request_validation() {
    let (mut h, c) = setup();
    let mut submit = |legs: Vec<LegSpec>, deadline| {
        rejection(&h.user(
            ALICE,
            Action::SubmitRequest {
                legs,
                response_deadline: deadline,
            },
        ))
    };
    assert_eq!(
        submit(vec![leg(c, 10, 7_000), leg(c, 10, 7_000)], DEADLINE),
        Some(RejectReason::Unsupported)
    );
    assert_eq!(
        submit(vec![leg(c, 10, 7_000)], 500),
        Some(RejectReason::InvalidInput),
        "deadline sooner than min_response"
    );
    assert_eq!(
        submit(vec![leg(c, 10, 7_000)], CLOSE - 1),
        Some(RejectReason::InvalidInput),
        "deadline + window after close"
    );
    assert_eq!(
        submit(vec![leg(ContractId(9), 10, 7_000)], DEADLINE),
        Some(RejectReason::UnknownObject)
    );
    assert_eq!(
        submit(vec![leg(c, 0, 7_000)], DEADLINE),
        Some(RejectReason::InvalidInput)
    );
    assert_eq!(
        submit(vec![leg(c, 1_000_000_001, 7_000)], DEADLINE),
        Some(RejectReason::InvalidInput)
    );
    assert_eq!(
        submit(vec![leg(c, 2_000, 7_000)], DEADLINE),
        Some(RejectReason::InsufficientFunds)
    );
}

#[test]
fn dust_request_rejected() {
    let (mut h, c) = setup();
    let mut submit = |qty, limit| {
        rejection(&h.user(
            ALICE,
            Action::SubmitRequest {
                legs: vec![leg(c, qty, limit)],
                response_deadline: DEADLINE,
            },
        ))
    };
    assert_eq!(
        submit(1, 9_999),
        Some(RejectReason::InvalidInput),
        "1 × $0.9999 is below min_request_notional ($1)"
    );
    assert_eq!(submit(2, 5_000), None, "exactly $1 is enough");
    assert_eq!(h.engine.balance(ALICE).reserved, dollars(1));
}

#[test]
fn quote_holds_complement_and_stays_private() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let out = h.user(
        MM1,
        Action::SubmitQuote {
            request: r,
            leg: 0,
            price: price(6_000),
            size: 1_000,
            expires_at: 100_000,
        },
    );
    assert_eq!(h.engine.balance(MM1).reserved, dollars(400));
    assert!(
        out.iter().all(|e| e.audience == Audience::Account(MM1)),
        "quotes are sealed"
    );
}

#[test]
fn quote_validation() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let mut q = |actor, bps, size, exp| {
        rejection(&h.user(
            actor,
            Action::SubmitQuote {
                request: r,
                leg: 0,
                price: price(bps),
                size,
                expires_at: exp,
            },
        ))
    };
    assert_eq!(q(ALICE, 6_000, 1_000, 100_000), Some(RejectReason::SelfTrade));
    assert_eq!(
        q(MM1, 6_000, 999, 100_000),
        Some(RejectReason::InvalidInput),
        "size below leg qty"
    );
    assert_eq!(
        q(MM1, 6_000, 1_000, 0),
        Some(RejectReason::InvalidInput),
        "already expired"
    );
    // No limit check: a rejection would tell the MM the sealed limit. Selection drops it instead
    // (Task 5: `over_limit_quote_accepted_but_never_selected`).
    assert_eq!(
        q(MM2, 7_001, 1_000, 100_000),
        None,
        "above the requester's limit is still accepted"
    );
    let out = h.user(
        MM1,
        Action::SubmitQuote {
            request: r,
            leg: 1,
            price: price(6_000),
            size: 1_000,
            expires_at: 100_000,
        },
    );
    assert_eq!(rejection(&out), Some(RejectReason::InvalidInput), "unknown leg");
}

#[test]
fn quote_expiry_clamped_to_request_lifetime() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let last_accept = DEADLINE + Config::default().accept_window;
    let q = QuoteId(h.next_seq());
    let out = h.user(
        MM1,
        Action::SubmitQuote {
            request: r,
            leg: 0,
            price: price(6_000),
            size: 1_000,
            expires_at: u64::MAX,
        },
    );
    assert_eq!(
        out.last().unwrap().event,
        Event::QuoteSubmitted {
            quote: q,
            request: r,
            price: price(6_000),
            expires_at: last_accept
        }
    );
    assert_eq!(h.engine.quote(q).unwrap().expires_at, last_accept);
    // The timer is bounded too: by `last_accept` the quote is gone and its hold released.
    h.tick(last_accept);
    assert!(!h.engine.quote(q).unwrap().state.is_live());
    assert_eq!(h.engine.balance(MM1).reserved, 0);
}

#[test]
fn quote_after_deadline_rejected() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    h.set_time(DEADLINE);
    let out = h.user(
        MM1,
        Action::SubmitQuote {
            request: r,
            leg: 0,
            price: price(6_000),
            size: 1_000,
            expires_at: 100_000,
        },
    );
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(h.engine.balance(MM1).reserved, 0);
}

#[test]
fn replace_quote_atomic() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let (q1, out1) = quote_out(&mut h, MM1, r, 6_000); // holds 400
    assert_eq!(names(&out1), vec!["QuoteSubmitted"]);
    assert_sealed(&h, &out1);
    // MM1 now has 600 free; a 3000-bps quote needs 700 — only possible net of the old hold.
    let (q2, out2) = quote_out(&mut h, MM1, r, 3_000);
    assert_eq!(names(&out2), vec!["QuoteReplaced", "QuoteSubmitted"]);
    assert_sealed(&h, &out2);
    assert_eq!(h.engine.quote(q1).unwrap().state, QuoteState::Replaced);
    assert_eq!(h.engine.quote(q2).unwrap().state, QuoteState::Active);
    assert_eq!(h.engine.balance(MM1).reserved, dollars(700));
    assert_eq!(h.engine.request(r).unwrap().legs[0].quotes.len(), 1);
}

#[test]
fn cancel_quote_releases() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let q = quote(&mut h, MM1, r, 6_000);
    assert_eq!(
        rejection(&h.user(MM2, Action::CancelQuote { quote: q })),
        Some(RejectReason::Unauthorized)
    );
    let out = h.user(MM1, Action::CancelQuote { quote: q });
    assert_eq!(rejection(&out), None);
    assert_eq!(names(&out), vec!["QuoteCancelled"]);
    assert_sealed(&h, &out);
    assert_eq!(h.engine.balance(MM1).free, dollars(1_000));
    assert_eq!(
        rejection(&h.user(MM1, Action::CancelQuote { quote: q })),
        Some(RejectReason::InvalidState)
    );
}

#[test]
fn quote_expiry_releases_hold() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let id = QuoteId(h.next_seq());
    let submitted = h.user(
        MM1,
        Action::SubmitQuote {
            request: r,
            leg: 0,
            price: price(6_000),
            size: 1_000,
            expires_at: 5_000,
        },
    );
    assert_sealed(&h, &submitted);
    let out = h.tick(5_000);
    assert_eq!(names(&out), vec!["QuoteExpired"]);
    assert_sealed(&h, &out);
    assert_eq!(h.engine.quote(id).unwrap().state, QuoteState::Expired);
    assert_eq!(h.engine.balance(MM1).free, dollars(1_000));
}

#[test]
fn cancel_request_releases_everything() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let q = quote(&mut h, MM1, r, 6_000);
    assert_eq!(
        rejection(&h.user(MM1, Action::CancelRequest { request: r })),
        Some(RejectReason::Unauthorized)
    );
    let out = h.user(ALICE, Action::CancelRequest { request: r });
    assert_eq!(names(&out), vec!["QuoteLost", "RequestCancelled"]);
    assert_sealed(&h, &out);
    assert_eq!(out[1].audience, Audience::Public);
    assert_eq!(h.engine.balance(ALICE).free, dollars(1_000));
    assert_eq!(h.engine.balance(MM1).free, dollars(1_000));
    assert_eq!(h.engine.quote(q).unwrap().state, QuoteState::Lost);
}

#[test]
fn rejected_replace_keeps_old_quote() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let q1 = quote(&mut h, MM1, r, 6_000); // holds $400, leaves $600 free
    assert_eq!(
        rejection(&h.user(MM1, Action::Withdraw { amount: dollars(500) })),
        None
    );
    let before = h.engine.balance(MM1);
    assert_eq!(
        before,
        Balance {
            free: dollars(100),
            reserved: dollars(400),
            escrowed: 0
        }
    );
    let hold = h.engine.quote(q1).unwrap().hold;
    // A 1_000-bps quote needs $900, but the old hold plus free funds are only $500.
    let out = h.user(
        MM1,
        Action::SubmitQuote {
            request: r,
            leg: 0,
            price: price(1_000),
            size: 1_000,
            expires_at: 100_000,
        },
    );
    assert_eq!(rejection(&out), Some(RejectReason::InsufficientFunds));
    assert_eq!(
        names(&out),
        vec!["CommandRejected"],
        "no QuoteReplaced, nothing half-done"
    );
    let old = h.engine.quote(q1).unwrap();
    assert_eq!(old.state, QuoteState::Active);
    assert_eq!(old.price, price(6_000));
    assert_eq!(old.hold, hold, "same hold, untouched");
    assert_eq!(h.engine.balance(MM1), before);
    let quotes = &h.engine.request(r).unwrap().legs[0].quotes;
    assert_eq!((quotes.len(), quotes.contains(&q1)), (1, true));
}

#[test]
fn two_mms_do_not_replace_each_other() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let q1 = quote(&mut h, MM1, r, 6_000);
    let q2 = quote(&mut h, MM2, r, 6_200);
    assert_eq!(h.engine.quote(q1).unwrap().state, QuoteState::Active);
    assert_eq!(h.engine.quote(q2).unwrap().state, QuoteState::Active);
    let quotes = &h.engine.request(r).unwrap().legs[0].quotes;
    assert_eq!(quotes.len(), 2);
    assert!(quotes.contains(&q1) && quotes.contains(&q2));
    let out = h.user(ALICE, Action::CancelRequest { request: r });
    assert_eq!(
        out,
        vec![
            Emitted::to(MM1, Event::QuoteLost { quote: q1 }),
            Emitted::to(MM2, Event::QuoteLost { quote: q2 }),
            Emitted::public(Event::RequestCancelled { request: r }),
        ]
    );
}

#[test]
fn double_cancel_request_rejected() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    assert_eq!(
        rejection(&h.user(ALICE, Action::CancelRequest { request: r })),
        None
    );
    let out = h.user(ALICE, Action::CancelRequest { request: r });
    assert_eq!(names(&out), vec!["CommandRejected"]);
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(h.engine.balance(ALICE).free, dollars(1_000));
}

#[test]
fn unknown_ids_rejected() {
    let (mut h, c) = setup();
    request(&mut h, c);
    let unknown = Some(RejectReason::UnknownObject);
    assert_eq!(
        rejection(&h.user(
            ALICE,
            Action::CancelRequest {
                request: RequestId(999)
            }
        )),
        unknown
    );
    assert_eq!(
        rejection(&h.user(
            MM1,
            Action::SubmitQuote {
                request: RequestId(999),
                leg: 0,
                price: price(6_000),
                size: 1_000,
                expires_at: 100_000,
            }
        )),
        unknown
    );
    assert_eq!(
        rejection(&h.user(MM1, Action::CancelQuote { quote: QuoteId(999) })),
        unknown
    );
}

#[test]
fn withdraw_cannot_touch_reserved() {
    let (mut h, c) = setup();
    request(&mut h, c);
    let out = h.user(ALICE, Action::Withdraw { amount: dollars(301) });
    assert_eq!(rejection(&out), Some(RejectReason::InsufficientFunds));
}

#[test]
fn contract_closes_at_trading_close() {
    let (mut h, c) = setup();
    let out = h.tick(CLOSE);
    assert_eq!(names(&out), vec!["ContractClosed"]);
    assert_eq!(h.engine.contract(c).unwrap().state, ContractState::Closed);
    let out = h.user(
        ALICE,
        Action::SubmitRequest {
            legs: vec![leg(c, 10, 7_000)],
            response_deadline: CLOSE + 10_000,
        },
    );
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
}

// Added beyond the brief: a request nobody quotes expires at its deadline (selection finds no
// eligible quote) and its hold is released.
#[test]
fn request_without_quotes_expires_at_deadline() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let out = h.tick(DEADLINE);
    assert_eq!(names(&out), vec!["RequestExpired"]);
    assert_eq!(h.engine.request(r).unwrap().state, RequestState::Expired);
    assert_eq!(h.engine.balance(ALICE).free, dollars(1_000));
    assert_eq!(h.engine.balance(ALICE).reserved, 0);
}
