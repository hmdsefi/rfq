# Task 4: Contracts listing, requests and quotes

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Gate A rulings applied here** ([decisions](../../reviews/spec-review.md#decisions)): #1 only `Active` quotes can be cancelled (a `Selected` quote is firm); #2 `SubmitQuote` has no `price ≤ limit` check (no limit oracle); #12 each leg needs `qty × limit × 100 ≥ min_request_notional`; #14 `expires_at` is clamped to `response_deadline + accept_window`; #22 `ListContract` rejects an oracle that is operator, arbiter or treasury.

**Files:**
- Create: `crates/rfq-core/src/engine/contract.rs`, `crates/rfq-core/src/engine/rfq.rs`, `crates/rfq-core/tests/requests_quotes.rs`
- Modify: `crates/rfq-core/src/engine/mod.rs` (modules, dispatch, timer match, and delete the Task 3 `#[expect]` placeholders listed in Step 5)

**Interfaces:**
- Consumes: Task 3 types and `Engine` internals (`arm`, `config`, `genesis`, `VALIDATED`, `Out`, `Handled`), including `Config::min_request_notional`.
- Produces (crate-internal, used by Tasks 5–6): `Engine::close_request(RequestId, RequestState, &mut Out)` and `Engine::end_quote(QuoteId, QuoteState, &mut Out)`; handlers `list_contract`, `on_contract_close`, `submit_request`, `cancel_request`, `submit_quote`, `cancel_quote` (final: `Active` only), `on_quote_expiry` (Active path), and a placeholder `on_request_deadline(RequestId, Timestamp, &mut Out)` that expires the request (Task 5 replaces it with selection).

- [ ] **Step 1: Write the failing tests**

`crates/rfq-core/tests/requests_quotes.rs`:
```rust
use rfq_core::testkit::{accounts::*, *};
use rfq_core::*;

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
    let out = h.user(OPERATOR, Action::ListContract {
        id: c,
        description: "Will it rain in Paris on 2026-11-01?".into(),
        oracle: ORACLE,
        trading_close: CLOSE,
        resolution_deadline: HARD,
        dispute_window: 10_000,
    });
    assert_eq!(rejection(&out), None);
    (h, c)
}

fn leg(c: ContractId, qty: Qty, limit: u16) -> LegSpec {
    LegSpec { contract: c, side: Side::BuyYes, qty, limit_price: price(limit) }
}

fn request(h: &mut Harness, c: ContractId) -> RequestId {
    let id = RequestId(h.next_seq());
    let out = h.user(ALICE, Action::SubmitRequest { legs: vec![leg(c, 1_000, 7_000)], response_deadline: DEADLINE });
    assert_eq!(rejection(&out), None);
    id
}

fn quote(h: &mut Harness, mm: AccountId, r: RequestId, bps: u16) -> QuoteId {
    let id = QuoteId(h.next_seq());
    let out = h.user(mm, Action::SubmitQuote { request: r, leg: 0, price: price(bps), size: 1_000, expires_at: 100_000 });
    assert_eq!(rejection(&out), None);
    id
}

#[test]
fn list_contract_requires_operator_and_valid_times() {
    let (mut h, _) = setup();
    let list = |h: &mut Harness, actor, id, oracle, close, hard| {
        h.user(actor, Action::ListContract { id: ContractId(id), description: "x".into(), oracle, trading_close: close, resolution_deadline: hard, dispute_window: 1 })
    };
    assert_eq!(rejection(&list(&mut h, ALICE, 2, ORACLE, CLOSE, HARD)), Some(RejectReason::Unauthorized));
    assert_eq!(rejection(&list(&mut h, OPERATOR, 1, ORACLE, CLOSE, HARD)), Some(RejectReason::InvalidInput));
    assert_eq!(rejection(&list(&mut h, OPERATOR, 3, ORACLE, HARD, CLOSE)), Some(RejectReason::InvalidInput));
    for role in [OPERATOR, ARBITER, TREASURY] {
        assert_eq!(rejection(&list(&mut h, OPERATOR, 4, role, CLOSE, HARD)), Some(RejectReason::InvalidInput), "oracle {role:?} holds a venue role");
    }
}

#[test]
fn submit_request_holds_qty_times_limit() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    assert_eq!(h.engine.balance(ALICE), Balance { free: dollars(300), reserved: dollars(700), escrowed: 0 });
    assert_eq!(h.engine.request(r).unwrap().state, RequestState::Open);
}

#[test]
fn submit_request_validation() {
    let (mut h, c) = setup();
    let mut submit = |legs: Vec<LegSpec>, deadline| rejection(&h.user(ALICE, Action::SubmitRequest { legs, response_deadline: deadline }));
    assert_eq!(submit(vec![leg(c, 10, 7_000), leg(c, 10, 7_000)], DEADLINE), Some(RejectReason::Unsupported));
    assert_eq!(submit(vec![leg(c, 10, 7_000)], 500), Some(RejectReason::InvalidInput), "deadline sooner than min_response");
    assert_eq!(submit(vec![leg(c, 10, 7_000)], CLOSE - 1), Some(RejectReason::InvalidInput), "deadline + window after close");
    assert_eq!(submit(vec![leg(ContractId(9), 10, 7_000)], DEADLINE), Some(RejectReason::UnknownObject));
    assert_eq!(submit(vec![leg(c, 0, 7_000)], DEADLINE), Some(RejectReason::InvalidInput));
    assert_eq!(submit(vec![leg(c, 1_000_000_001, 7_000)], DEADLINE), Some(RejectReason::InvalidInput));
    assert_eq!(submit(vec![leg(c, 2_000, 7_000)], DEADLINE), Some(RejectReason::InsufficientFunds));
}

#[test]
fn dust_request_rejected() {
    let (mut h, c) = setup();
    let mut submit = |qty, limit| rejection(&h.user(ALICE, Action::SubmitRequest { legs: vec![leg(c, qty, limit)], response_deadline: DEADLINE }));
    assert_eq!(submit(1, 9_999), Some(RejectReason::InvalidInput), "1 × $0.9999 is below min_request_notional ($1)");
    assert_eq!(submit(2, 5_000), None, "exactly $1 is enough");
    assert_eq!(h.engine.balance(ALICE).reserved, dollars(1));
}

#[test]
fn quote_holds_complement_and_stays_private() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let out = h.user(MM1, Action::SubmitQuote { request: r, leg: 0, price: price(6_000), size: 1_000, expires_at: 100_000 });
    assert_eq!(h.engine.balance(MM1).reserved, dollars(400));
    assert!(out.iter().all(|e| e.audience == Audience::Account(MM1)), "quotes are sealed");
}

#[test]
fn quote_validation() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let mut q = |actor, bps, size, exp| rejection(&h.user(actor, Action::SubmitQuote { request: r, leg: 0, price: price(bps), size, expires_at: exp }));
    assert_eq!(q(ALICE, 6_000, 1_000, 100_000), Some(RejectReason::SelfTrade));
    assert_eq!(q(MM1, 6_000, 999, 100_000), Some(RejectReason::InvalidInput), "size below leg qty");
    assert_eq!(q(MM1, 6_000, 1_000, 0), Some(RejectReason::InvalidInput), "already expired");
    // No limit check: a rejection would tell the MM the sealed limit. Selection drops it instead
    // (Task 5: `over_limit_quote_accepted_but_never_selected`).
    assert_eq!(q(MM2, 7_001, 1_000, 100_000), None, "above the requester's limit is still accepted");
    let out = h.user(MM1, Action::SubmitQuote { request: r, leg: 1, price: price(6_000), size: 1_000, expires_at: 100_000 });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidInput), "unknown leg");
}

#[test]
fn quote_expiry_clamped_to_request_lifetime() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let last_accept = DEADLINE + Config::default().accept_window;
    let q = QuoteId(h.next_seq());
    let out = h.user(MM1, Action::SubmitQuote { request: r, leg: 0, price: price(6_000), size: 1_000, expires_at: u64::MAX });
    assert_eq!(out.last().unwrap().event, Event::QuoteSubmitted { quote: q, request: r, price: price(6_000), expires_at: last_accept });
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
    let out = h.user(MM1, Action::SubmitQuote { request: r, leg: 0, price: price(6_000), size: 1_000, expires_at: 100_000 });
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
    assert_eq!(rejection(&h.user(MM2, Action::CancelQuote { quote: q })), Some(RejectReason::Unauthorized));
    assert_eq!(rejection(&h.user(MM1, Action::CancelQuote { quote: q })), None);
    assert_eq!(h.engine.balance(MM1).free, dollars(1_000));
    assert_eq!(rejection(&h.user(MM1, Action::CancelQuote { quote: q })), Some(RejectReason::InvalidState));
}

#[test]
fn quote_expiry_releases_hold() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let id = QuoteId(h.next_seq());
    h.user(MM1, Action::SubmitQuote { request: r, leg: 0, price: price(6_000), size: 1_000, expires_at: 5_000 });
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
    assert_eq!(rejection(&h.user(MM1, Action::CancelRequest { request: r })), Some(RejectReason::Unauthorized));
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
    let out = h.user(ALICE, Action::SubmitRequest { legs: vec![leg(c, 10, 7_000)], response_deadline: CLOSE + 10_000 });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
}
```
Every leg in a test that expects a *different* rejection uses at least `qty 10` at 0.70 (= $7), so the $1 minimum notional never masks the reason under test.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rfq-core --test requests_quotes`
Expected: FAIL. Most tests get `Unsupported` rejections because the handlers don't exist yet.

- [ ] **Step 3: Implement contract listing and close**

`crates/rfq-core/src/engine/contract.rs`:
```rust
use super::{Engine, Handled, Out};
use crate::event::{Emitted, Event, RejectReason};
use crate::model::*;
use crate::types::*;

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn list_contract(
        &mut self,
        actor: AccountId,
        id: ContractId,
        description: &str,
        oracle: AccountId,
        trading_close: Timestamp,
        resolution_deadline: Timestamp,
        dispute_window: Duration,
        out: &mut Out,
    ) -> Handled {
        let g = self.genesis();
        if actor != g.operator {
            return Err(RejectReason::Unauthorized);
        }
        // Role separation: the account that proposes outcomes must never judge its own proposal,
        // list contracts, or collect forfeited bonds.
        if [g.operator, g.arbiter, g.treasury].contains(&oracle) {
            return Err(RejectReason::InvalidInput);
        }
        if self.contracts.contains_key(&id) {
            return Err(RejectReason::InvalidInput);
        }
        if !(self.now < trading_close && trading_close < resolution_deadline) || dispute_window == 0 {
            return Err(RejectReason::InvalidInput);
        }
        self.contracts.insert(id, Contract {
            id,
            description: description.to_owned(),
            oracle,
            trading_close,
            resolution_deadline,
            dispute_window,
            state: ContractState::Trading,
            dispute: None,
            positions: Vec::new(),
        });
        self.arm(trading_close, TimerKey::ContractClose(id));
        self.arm(resolution_deadline, TimerKey::HardDeadline(id));
        out.push(Emitted::public(Event::ContractListed { contract: id }));
        Ok(())
    }

    pub(super) fn on_contract_close(&mut self, id: ContractId, out: &mut Out) {
        let Some(c) = self.contracts.get_mut(&id) else { return };
        if c.state != ContractState::Trading {
            return;
        }
        c.state = ContractState::Closed;
        out.push(Emitted::public(Event::ContractClosed { contract: id }));
    }
}
```

- [ ] **Step 4: Implement requests and quotes**

`crates/rfq-core/src/engine/rfq.rs`:
```rust
use std::collections::BTreeSet;

use super::{Engine, Handled, Out, VALIDATED};
use crate::command::LegSpec;
use crate::event::{Emitted, Event, RejectReason};
use crate::model::*;
use crate::types::*;

impl Engine {
    pub(super) fn submit_request(&mut self, actor: AccountId, legs: &[LegSpec], response_deadline: Timestamp, out: &mut Out) -> Handled {
        let cfg = self.config();
        if legs.len() != 1 {
            return Err(RejectReason::Unsupported); // multi-leg is designed, not built (RFC P6)
        }
        let earliest = self.now.checked_add(cfg.min_response).ok_or(RejectReason::Overflow)?;
        let latest = self.now.saturating_add(cfg.max_response);
        if response_deadline < earliest || response_deadline > latest {
            return Err(RejectReason::InvalidInput);
        }
        let last_accept = response_deadline.checked_add(cfg.accept_window).ok_or(RejectReason::Overflow)?;
        let mut total: Amount = 0;
        // Every leg is checked, so the trading-close guard is the min over legs (engine.md §8).
        for l in legs {
            let c = self.contracts.get(&l.contract).ok_or(RejectReason::UnknownObject)?;
            if c.state != ContractState::Trading {
                return Err(RejectReason::InvalidState);
            }
            if last_accept >= c.trading_close {
                return Err(RejectReason::InvalidInput);
            }
            if l.qty == 0 || l.qty > cfg.max_qty {
                return Err(RejectReason::InvalidInput);
            }
            let lock = buyer_lock(l.qty, l.limit_price).ok_or(RejectReason::Overflow)?;
            // A dust request costs the venue (a public event, a timer, MM attention) more than it holds.
            if lock < cfg.min_request_notional {
                return Err(RejectReason::InvalidInput);
            }
            total = total.checked_add(lock).ok_or(RejectReason::Overflow)?;
        }
        if self.ledger.balance(actor).free < total {
            return Err(RejectReason::InsufficientFunds);
        }

        let id = RequestId(self.last_seq);
        let hold = self.ledger.hold(actor, total).expect(VALIDATED);
        self.requests.insert(id, Request {
            id,
            requester: actor,
            legs: legs
                .iter()
                .map(|l| Leg { contract: l.contract, side: l.side, qty: l.qty, limit_price: l.limit_price, quotes: BTreeSet::new(), selected: None })
                .collect(),
            response_deadline,
            hold: Some(hold),
            state: RequestState::Open,
        });
        self.arm(response_deadline, TimerKey::RequestDeadline(id));
        for (i, l) in legs.iter().enumerate() {
            out.push(Emitted::public(Event::RequestOpened {
                request: id,
                leg: u32::try_from(i).expect("leg count is bounded"),
                contract: l.contract,
                side: l.side,
                qty: l.qty,
                response_deadline,
            }));
        }
        Ok(())
    }

    pub(super) fn cancel_request(&mut self, actor: AccountId, id: RequestId, out: &mut Out) -> Handled {
        let r = self.requests.get(&id).ok_or(RejectReason::UnknownObject)?;
        if r.requester != actor {
            return Err(RejectReason::Unauthorized);
        }
        if r.state != RequestState::Open {
            return Err(RejectReason::InvalidState);
        }
        self.close_request(id, RequestState::Cancelled, out);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn submit_quote(
        &mut self,
        actor: AccountId,
        request: RequestId,
        leg: u32,
        price: Price,
        size: Qty,
        expires_at: Timestamp,
        out: &mut Out,
    ) -> Handled {
        let cfg = self.config();
        let r = self.requests.get(&request).ok_or(RejectReason::UnknownObject)?;
        if r.state != RequestState::Open || self.now >= r.response_deadline {
            return Err(RejectReason::InvalidState);
        }
        if r.requester == actor {
            return Err(RejectReason::SelfTrade);
        }
        let l = r.legs.get(leg as usize).ok_or(RejectReason::InvalidInput)?;
        // No `price <= l.limit_price` check: a rejection would be an oracle for the sealed limit.
        // Selection ignores over-limit quotes, and `QuoteLost` doesn't say why.
        if size < l.qty || expires_at <= self.now {
            return Err(RejectReason::InvalidInput);
        }
        // A quote is useless once the request can no longer be accepted, so neither the quote nor
        // its timer may outlive `response_deadline + accept_window`.
        let expires_at = expires_at.min(r.response_deadline.saturating_add(cfg.accept_window));
        let need = seller_lock(l.qty, price).ok_or(RejectReason::Overflow)?;
        let previous = l.quotes.iter().copied().find(|q| self.quotes[q].mm == actor);
        let refundable = previous
            .and_then(|q| self.quotes[&q].hold)
            .and_then(|h| self.ledger.hold_amount(h))
            .unwrap_or(0);
        if self.ledger.balance(actor).free.saturating_add(refundable) < need {
            return Err(RejectReason::InsufficientFunds);
        }

        let id = QuoteId(self.last_seq);
        if let Some(old) = previous {
            self.end_quote(old, QuoteState::Replaced, out);
            out.push(Emitted::to(actor, Event::QuoteReplaced { old, new: id }));
        }
        let hold = self.ledger.hold(actor, need).expect(VALIDATED);
        self.quotes.insert(id, Quote { id, mm: actor, request, leg, price, size, expires_at, hold: Some(hold), state: QuoteState::Active });
        self.requests.get_mut(&request).expect(VALIDATED).legs[leg as usize].quotes.insert(id);
        self.arm(expires_at, TimerKey::QuoteExpiry(id));
        out.push(Emitted::to(actor, Event::QuoteSubmitted { quote: id, request, price, expires_at }));
        Ok(())
    }

    /// Only `Active` quotes can be cancelled. A `Selected` quote is firm through `accept_by` (RFC P5):
    /// the MM priced the known accept window into it, and a cancel would void the RFQ for free.
    pub(super) fn cancel_quote(&mut self, actor: AccountId, id: QuoteId, out: &mut Out) -> Handled {
        let q = self.quotes.get(&id).ok_or(RejectReason::UnknownObject)?;
        if q.mm != actor {
            return Err(RejectReason::Unauthorized);
        }
        if q.state != QuoteState::Active {
            return Err(RejectReason::InvalidState);
        }
        self.end_quote(id, QuoteState::Cancelled, out);
        Ok(())
    }

    pub(super) fn on_quote_expiry(&mut self, id: QuoteId, out: &mut Out) {
        let Some(q) = self.quotes.get(&id) else { return };
        if q.state == QuoteState::Active {
            self.end_quote(id, QuoteState::Expired, out);
        }
    }

    /// Placeholder until Task 5 adds selection: an `Open` request expires at its deadline and
    /// releases everything. Without it the deadline timer would be dropped and the still-`Open`
    /// request would have no exit timer, which the I5 check rejects.
    pub(super) fn on_request_deadline(&mut self, id: RequestId, _deadline: Timestamp, out: &mut Out) {
        let Some(r) = self.requests.get(&id) else { return };
        if r.state == RequestState::Open {
            self.close_request(id, RequestState::Expired, out);
        }
    }

    /// Releases a live quote's hold, sets its terminal state and detaches it from its leg.
    /// Emits the matching event to the MM, except for `Replaced` (the caller emits that).
    pub(super) fn end_quote(&mut self, id: QuoteId, state: QuoteState, out: &mut Out) {
        let q = self.quotes.get_mut(&id).expect("quote exists");
        debug_assert!(q.state.is_live());
        if let Some(h) = q.hold.take() {
            self.ledger.release(h).expect("live quote hold exists");
        }
        q.state = state;
        let (request, leg, mm) = (q.request, q.leg as usize, q.mm);
        if let Some(r) = self.requests.get_mut(&request) {
            r.legs[leg].quotes.remove(&id);
        }
        let event = match state {
            QuoteState::Cancelled => Event::QuoteCancelled { quote: id },
            QuoteState::Expired => Event::QuoteExpired { quote: id },
            QuoteState::Lost => Event::QuoteLost { quote: id },
            QuoteState::Released => Event::QuoteReleased { quote: id },
            QuoteState::Replaced => return,
            QuoteState::Active | QuoteState::Selected | QuoteState::Filled => unreachable!("not a release state"),
        };
        out.push(Emitted::to(mm, event));
    }

    /// Moves a live request to a terminal non-filled state, releasing the requester's hold and every
    /// live quote on every leg in this one command (multi-leg atomicity, engine.md §8). Holds are
    /// `take()`n, so each live hold is released exactly once.
    pub(super) fn close_request(&mut self, id: RequestId, state: RequestState, out: &mut Out) {
        let r = self.requests.get_mut(&id).expect("request exists");
        debug_assert!(r.state.is_live());
        if let Some(h) = r.hold.take() {
            self.ledger.release(h).expect("live request hold exists");
        }
        r.state = state;
        let live: Vec<QuoteId> = r.legs.iter().flat_map(|l| l.quotes.iter().copied()).collect();
        for q in live {
            let terminal = if self.quotes[&q].state == QuoteState::Selected { QuoteState::Released } else { QuoteState::Lost };
            self.end_quote(q, terminal, out);
        }
        let event = match state {
            RequestState::Cancelled => Event::RequestCancelled { request: id },
            RequestState::Expired => Event::RequestExpired { request: id },
            RequestState::Rejected(cause) => Event::RequestRejected { request: id, cause },
            RequestState::Open | RequestState::Presented { .. } | RequestState::Filled => {
                unreachable!("close_request needs a terminal non-filled state")
            }
        };
        out.push(Emitted::public(event));
    }
}
```

- [ ] **Step 5: Wire up dispatch and timers**

In `crates/rfq-core/src/engine/mod.rs`, add `mod contract; mod rfq;` next to `mod account;`. Then replace the `let result = match action { ... }` block in `apply_user` with:
```rust
        let result = match action {
            Action::Deposit { to, amount } => self.deposit(actor, *to, *amount, out),
            Action::Withdraw { amount } => self.withdraw(actor, *amount, out),
            Action::ListContract { id, description, oracle, trading_close, resolution_deadline, dispute_window } => {
                self.list_contract(actor, *id, description, *oracle, *trading_close, *resolution_deadline, *dispute_window, out)
            }
            Action::SubmitRequest { legs, response_deadline } => self.submit_request(actor, legs, *response_deadline, out),
            Action::CancelRequest { request } => self.cancel_request(actor, *request, out),
            Action::SubmitQuote { request, leg, price, size, expires_at } => {
                self.submit_quote(actor, *request, *leg, *price, *size, *expires_at, out)
            }
            Action::CancelQuote { quote } => self.cancel_quote(actor, *quote, out),
            _ => Err(RejectReason::Unsupported),
        };
```
Then replace the timer `match key { _ => ... }` in `fire_due_timers` with:
```rust
            match key {
                TimerKey::QuoteExpiry(q) => self.on_quote_expiry(q, out),
                TimerKey::RequestDeadline(r) => self.on_request_deadline(r, at, out),
                TimerKey::ContractClose(c) => self.on_contract_close(c, out),
                _ => {}
            }
```
Delete the Task 3 placeholders that this step fulfils (each is an unfulfilled `#[expect]` now, which fails clippy):
- `#[expect(dead_code, …)]` on `VALIDATED`;
- `#[expect(clippy::match_single_binding, …)]` and `#[expect(unused_variables, …)]` on `fire_due_timers`, and the "Placeholder dispatch" comment above them;
- `#[expect(dead_code, …)]` on `arm`;
- `#[expect(dead_code, …)]` on `config`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p rfq-core`, then `cargo clippy --workspace --all-targets -- -D warnings`
Expected: all pass, including 15 in `requests_quotes`, and clippy is clean. Note that `request(...)` creates its request at `DEADLINE = 10_000`, which satisfies `min_response` because the harness time is 0.

- [ ] **Step 7: Commit**

```bash
git add crates/rfq-core
git commit -m "feat(core): contract listing, requests with worst-case holds, sealed quotes with atomic replace"
```
