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

fn quote(h: &mut Harness, mm: AccountId, r: RequestId, bps: u16) -> QuoteId {
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
    id
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
    let q1 = quote(&mut h, MM1, r, 6_000); // holds 400
    // MM1 now has 600 free; a 3000-bps quote needs 700 — only possible net of the old hold.
    let q2 = quote(&mut h, MM1, r, 3_000);
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
    assert_eq!(rejection(&h.user(MM1, Action::CancelQuote { quote: q })), None);
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
    h.user(
        MM1,
        Action::SubmitQuote {
            request: r,
            leg: 0,
            price: price(6_000),
            size: 1_000,
            expires_at: 5_000,
        },
    );
    let out = h.tick(5_000);
    assert_eq!(names(&out), vec!["QuoteExpired"]);
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
    assert_eq!(h.engine.balance(ALICE).free, dollars(1_000));
    assert_eq!(h.engine.balance(MM1).free, dollars(1_000));
    assert_eq!(h.engine.quote(q).unwrap().state, QuoteState::Lost);
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

// Added beyond the brief: exercises the `on_request_deadline` stub. With no quotes, Task 5's
// selection also ends in `Expired`, so this stays valid when the stub is replaced.
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
