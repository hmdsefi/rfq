# Task 5: Selection, acceptance, rejection and races

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Gate A rulings applied here** ([decisions](../../reviews/spec-review.md#decisions)): #1 selected quotes are firm — `cancel_quote` stays `Active`-only (Task 4), there is no "cancel selected ⇒ `Rejected(QuoteWithdrawn)`" path, and `RejectCause::QuoteWithdrawn` is deleted; R1 is now "MM cancel vs response deadline". #2 selection is the only place the sealed limit is checked. #14 with the submit clamp, a selected quote expires exactly at `accept_by`, so its expiry timer is a no-op and `AcceptBy` lapses the request.

**Files:**
- Modify: `crates/rfq-core/src/engine/rfq.rs`, `crates/rfq-core/src/engine/mod.rs`, `crates/rfq-core/src/model.rs` (delete `RejectCause::QuoteWithdrawn`)
- Create: `crates/rfq-core/tests/selection_accept.rs`

**Interfaces:**
- Consumes: `close_request`, `end_quote`, `cancel_quote` (Task 4, unchanged here).
- Produces: handlers `on_request_deadline(RequestId, Timestamp, &mut Out)` (replacing Task 4's expire-only placeholder), `on_accept_by`, `accept`, `reject`; `on_quote_expiry` gains a defensive `Selected` path. Positions are created with ids `PositionId(next_position)`.

- [ ] **Step 1: Write the failing tests**

`crates/rfq-core/tests/selection_accept.rs`:
```rust
use rfq_core::testkit::{accounts::*, *};
use rfq_core::*;

const DEADLINE: Timestamp = 10_000;
const ACCEPT_BY: Timestamp = DEADLINE + 30_000;

struct World { h: Harness, c: MarketId, r: RequestId }

fn world(side: Side) -> World {
    let mut h = Harness::new(Config::default());
    for a in [ALICE, MM1, MM2] {
        h.fund(a, 1_000);
    }
    let c = MarketId(1);
    h.user(OPERATOR, Action::ListMarket { id: c, description: "q".into(), oracle: ORACLE, trading_close: 1_000_000, resolution_deadline: 2_000_000, dispute_window: 10_000 });
    let r = RequestId(h.next_seq());
    let out = h.user(ALICE, Action::SubmitRequest { legs: vec![LegSpec { market: c, side, qty: 1_000, limit_price: price(7_000) }], response_deadline: DEADLINE });
    assert_eq!(rejection(&out), None);
    World { h, c, r }
}

fn quote(w: &mut World, mm: AccountId, bps: u16, expires_at: Timestamp) -> QuoteId {
    let id = QuoteId(w.h.next_seq());
    let out = w.h.user(mm, Action::SubmitQuote { request: w.r, leg: 0, price: price(bps), size: 1_000, expires_at });
    assert_eq!(rejection(&out), None);
    id
}

fn picks(q: QuoteId, bps: u16) -> Vec<Pick> { vec![Pick { leg: 0, quote: q, price: price(bps) }] }

#[test]
fn best_quote_is_presented_and_losers_released() {
    let mut w = world(Side::BuyYes);
    let q1 = quote(&mut w, MM1, 6_200, 100_000);
    let q2 = quote(&mut w, MM2, 6_000, 100_000);
    let out = w.h.tick(DEADLINE);
    assert_eq!(names(&out), vec!["QuoteLost", "QuoteSelected", "RequestPresented"]);
    assert_eq!(w.h.engine.request(w.r).unwrap().state, RequestState::Presented { accept_by: ACCEPT_BY });
    assert_eq!(w.h.engine.quote(q1).unwrap().state, QuoteState::Lost);
    assert_eq!(w.h.engine.quote(q2).unwrap().state, QuoteState::Selected);
    assert_eq!(w.h.engine.balance(MM1).free, dollars(1_000));
    let presented = out.iter().find(|e| e.event.name() == "RequestPresented").unwrap();
    assert_eq!(presented.audience, Audience::Account(ALICE));
    assert_eq!(presented.event, Event::RequestPresented { request: w.r, picks: picks(q2, 6_000), accept_by: ACCEPT_BY });
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

#[test]
fn no_quotes_expires_request() {
    let mut w = world(Side::BuyYes);
    let out = w.h.tick(DEADLINE);
    assert_eq!(names(&out), vec!["RequestExpired"]);
    assert_eq!(w.h.engine.balance(ALICE).free, dollars(1_000));
}

#[test]
fn accept_escrows_exact_amounts_buy_yes() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    let out = w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) });
    assert_eq!(names(&out), vec!["QuoteFilled", "RequestFilled"]);
    assert_eq!(w.h.engine.balance(ALICE), Balance { free: dollars(400), reserved: 0, escrowed: dollars(600) });
    assert_eq!(w.h.engine.balance(MM2), Balance { free: dollars(600), reserved: 0, escrowed: dollars(400) });
    let p = w.h.engine.position(PositionId(1)).unwrap();
    assert_eq!((p.yes_holder, p.yes_lock, p.no_holder, p.no_lock), (ALICE, dollars(600), MM2, dollars(400)));
    assert_eq!(w.h.engine.market(w.c).unwrap().positions, vec![PositionId(1)]);
}

#[test]
fn accept_buy_no_maps_sides() {
    let mut w = world(Side::BuyNo);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) });
    let p = w.h.engine.position(PositionId(1)).unwrap();
    assert_eq!((p.yes_holder, p.yes_lock, p.no_holder, p.no_lock), (MM2, dollars(400), ALICE, dollars(600)));
}

#[test]
fn accept_must_echo_exact_selection() {
    let mut w = world(Side::BuyYes);
    let q1 = quote(&mut w, MM1, 6_200, 100_000);
    let q2 = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    let mut accept = |p| rejection(&w.h.user(ALICE, Action::Accept { request: w.r, picks: p }));
    assert_eq!(accept(picks(q2, 5_999)), Some(RejectReason::Mismatch));
    assert_eq!(accept(picks(q1, 6_200)), Some(RejectReason::Mismatch));
    assert_eq!(accept(vec![]), Some(RejectReason::Mismatch));
    let out = w.h.user(MM1, Action::Accept { request: w.r, picks: picks(q2, 6_000) });
    assert_eq!(rejection(&out), Some(RejectReason::Unauthorized));
}

#[test]
fn double_accept_rejected() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    assert_eq!(rejection(&w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) })), None);
    let out = w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(rejection(&w.h.user(ALICE, Action::Reject { request: w.r })), Some(RejectReason::InvalidState));
}

#[test]
fn reject_releases_everything() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    let out = w.h.user(ALICE, Action::Reject { request: w.r });
    assert_eq!(names(&out), vec!["QuoteReleased", "RequestRejected"]);
    assert_eq!(w.h.engine.request(w.r).unwrap().state, RequestState::Rejected(RejectCause::Declined));
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
    let out = w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) });
    assert_eq!(names(&out), vec!["QuoteReleased", "RequestRejected", "CommandRejected"]);
    assert_eq!(w.h.engine.request(w.r).unwrap().state, RequestState::Rejected(RejectCause::Lapsed));
}

#[test]
fn idle_engine_fires_deadline_then_lapse_in_one_command() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.set_time(ACCEPT_BY + 5_000); // no tick since the quote
    let out = w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) });
    assert_eq!(
        names(&out),
        vec!["QuoteSelected", "RequestPresented", "QuoteReleased", "RequestRejected", "CommandRejected"]
    );
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
    assert_eq!(w.h.engine.balance(MM1).free, dollars(1_000));
    // A cancel stamped at the deadline is sequenced after selection: the quote is already Selected.
    w.h.set_time(DEADLINE);
    let out = w.h.user(MM2, Action::CancelQuote { quote: q2 });
    assert_eq!(names(&out), vec!["QuoteSelected", "RequestPresented", "CommandRejected"]);
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
    assert_eq!(rejection(&w.h.user(MM2, Action::CancelQuote { quote: q2 })), Some(RejectReason::InvalidState));
    assert_eq!(rejection(&w.h.user(MM1, Action::CancelQuote { quote: q1 })), Some(RejectReason::InvalidState));
    assert_eq!(w.h.engine.quote(q2).unwrap().state, QuoteState::Selected);
    assert_eq!(w.h.engine.balance(MM2).reserved, dollars(400));
    let out = w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q2, 6_000) });
    assert_eq!(names(&out), vec!["QuoteFilled", "RequestFilled"]);
    assert_eq!(w.h.engine.balance(MM2).escrowed, dollars(400));
}
```

`over_limit_quote_accepted_but_never_selected` is the selection half of Gate A #2; Task 4's `quote_validation` covers the submit half.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rfq-core --test selection_accept`
Expected: FAIL (Task 4's placeholder expires every request at its deadline instead of selecting; Accept/Reject give `Unsupported`).

- [ ] **Step 3: Implement selection, accept, reject and lapse**

Delete Task 4's placeholder `on_request_deadline`, then append to the `impl Engine` block in `crates/rfq-core/src/engine/rfq.rs`:
```rust
    /// Fired at `response_deadline`. Picks the best eligible quote per leg; all-or-nothing.
    pub(super) fn on_request_deadline(&mut self, id: RequestId, deadline: Timestamp, out: &mut Out) {
        let Some(r) = self.requests.get(&id) else { return };
        if r.state != RequestState::Open {
            return;
        }
        let accept_by = deadline.saturating_add(self.config().accept_window);
        // Eligible = active, within limit, and firm through the whole acceptance window. This is the
        // only check of the sealed limit (SubmitQuote doesn't do it, so it can't be probed).
        let picks: Option<Vec<Pick>> = r
            .legs
            .iter()
            .enumerate()
            .map(|(i, l)| {
                l.quotes
                    .iter()
                    .map(|q| &self.quotes[q])
                    .filter(|q| q.state == QuoteState::Active && q.price <= l.limit_price && q.expires_at >= accept_by)
                    .min_by_key(|q| (q.price, q.id))
                    .map(|q| Pick { leg: u32::try_from(i).expect("leg count is bounded"), quote: q.id, price: q.price })
            })
            .collect();
        let Some(picks) = picks else {
            // Some leg has no eligible quote: the whole request expires (all-or-nothing).
            self.close_request(id, RequestState::Expired, out);
            return;
        };
        for p in &picks {
            let losers: Vec<QuoteId> = self.requests[&id].legs[p.leg as usize]
                .quotes
                .iter()
                .copied()
                .filter(|q| *q != p.quote)
                .collect();
            for q in losers {
                self.end_quote(q, QuoteState::Lost, out);
            }
            self.requests.get_mut(&id).expect("exists").legs[p.leg as usize].selected = Some(p.quote);
            let q = self.quotes.get_mut(&p.quote).expect("exists");
            q.state = QuoteState::Selected;
            out.push(Emitted::to(q.mm, Event::QuoteSelected { quote: p.quote }));
        }
        let r = self.requests.get_mut(&id).expect("exists");
        r.state = RequestState::Presented { accept_by };
        let requester = r.requester;
        self.arm(accept_by, TimerKey::AcceptBy(id));
        out.push(Emitted::to(requester, Event::RequestPresented { request: id, picks, accept_by }));
    }

    pub(super) fn on_accept_by(&mut self, id: RequestId, out: &mut Out) {
        let Some(r) = self.requests.get(&id) else { return };
        if matches!(r.state, RequestState::Presented { .. }) {
            self.close_request(id, RequestState::Rejected(RejectCause::Lapsed), out);
        }
    }

    pub(super) fn reject(&mut self, actor: AccountId, id: RequestId, out: &mut Out) -> Handled {
        let r = self.requests.get(&id).ok_or(RejectReason::UnknownObject)?;
        if r.requester != actor {
            return Err(RejectReason::Unauthorized);
        }
        if !matches!(r.state, RequestState::Presented { .. }) {
            return Err(RejectReason::InvalidState);
        }
        self.close_request(id, RequestState::Rejected(RejectCause::Declined), out);
        Ok(())
    }

    /// Escrows every leg in one command. All checks run before the first mutation.
    pub(super) fn accept(&mut self, actor: AccountId, id: RequestId, picks: &[Pick], out: &mut Out) -> Handled {
        let r = self.requests.get(&id).ok_or(RejectReason::UnknownObject)?;
        if r.requester != actor {
            return Err(RejectReason::Unauthorized);
        }
        if !matches!(r.state, RequestState::Presented { .. }) {
            return Err(RejectReason::InvalidState);
        }
        if picks.len() != r.legs.len() {
            return Err(RejectReason::Mismatch);
        }
        let mut requester_total: Amount = 0;
        for (i, (l, p)) in r.legs.iter().zip(picks).enumerate() {
            if p.leg as usize != i || l.selected != Some(p.quote) {
                return Err(RejectReason::Mismatch);
            }
            let q = &self.quotes[&p.quote];
            if q.state != QuoteState::Selected || q.price != p.price {
                return Err(RejectReason::Mismatch);
            }
            let lock = buyer_lock(l.qty, q.price).ok_or(RejectReason::Overflow)?;
            requester_total = requester_total.checked_add(lock).ok_or(RejectReason::Overflow)?;
        }
        let hold = r.hold.expect("live request has a hold");
        if self.ledger.hold_amount(hold).unwrap_or(0) < requester_total {
            return Err(RejectReason::InsufficientFunds);
        }

        // Apply: nothing below can fail.
        let requester = r.requester;
        let legs = r.legs.clone();
        self.ledger.escrow_from_hold(hold, requester_total).expect(VALIDATED);
        let mut positions = Vec::with_capacity(legs.len());
        for l in &legs {
            let qid = l.selected.expect(VALIDATED);
            let q = self.quotes.get_mut(&qid).expect(VALIDATED);
            let mm_hold = q.hold.take().expect("selected quote has a hold");
            let mm_lock = self.ledger.hold_amount(mm_hold).expect(VALIDATED);
            self.ledger.escrow_from_hold(mm_hold, mm_lock).expect(VALIDATED);
            q.state = QuoteState::Filled;
            let buyer = buyer_lock(l.qty, q.price).expect(VALIDATED);
            let (yes_holder, yes_lock, no_holder, no_lock) = match l.side {
                Side::BuyYes => (requester, buyer, q.mm, mm_lock),
                Side::BuyNo => (q.mm, mm_lock, requester, buyer),
            };
            self.next_position += 1;
            let pid = PositionId(self.next_position);
            self.positions.insert(pid, Position {
                id: pid,
                market: l.market,
                request: id,
                qty: l.qty,
                yes_holder,
                yes_lock,
                no_holder,
                no_lock,
                settled: false,
            });
            self.markets.get_mut(&l.market).expect(VALIDATED).positions.push(pid);
            out.push(Emitted::to(q.mm, Event::QuoteFilled { quote: qid, position: pid }));
            positions.push(pid);
        }
        let r = self.requests.get_mut(&id).expect(VALIDATED);
        r.state = RequestState::Filled;
        r.hold = None;
        for l in &mut r.legs {
            l.quotes.clear();
        }
        out.push(Emitted::to(requester, Event::RequestFilled { request: id, positions }));
        Ok(())
    }
```
Add `use crate::command::Pick;` to the imports at the top of `rfq.rs`.

Leave `cancel_quote` exactly as Task 4 wrote it: only `Active` quotes can be cancelled, and a `Selected` one gets `InvalidState` (Gate A #1). Do **not** add a `Selected` arm.

Replace the `on_quote_expiry` body with a version that knows about `Selected`:
```rust
    pub(super) fn on_quote_expiry(&mut self, id: QuoteId, out: &mut Out) {
        let Some(q) = self.quotes.get(&id) else { return };
        match q.state {
            QuoteState::Active => self.end_quote(id, QuoteState::Expired, out),
            // A selected quote is firm through `accept_by`: eligibility needs `expires_at >= accept_by`
            // and submit clamps it to at most that, so this timer fires together with (just before)
            // the request's `AcceptBy`, which lapses the request. Defensive only: a selected quote
            // that expired *before* `accept_by` would fail the request, never fall back (RFC P8).
            QuoteState::Selected => {
                let request = q.request;
                let early = matches!(
                    self.requests[&request].state,
                    RequestState::Presented { accept_by } if q.expires_at < accept_by
                );
                if early {
                    self.end_quote(id, QuoteState::Expired, out);
                    self.close_request(request, RequestState::Rejected(RejectCause::QuoteExpired), out);
                }
            }
            _ => {}
        }
    }
```
In `crates/rfq-core/src/model.rs`, delete the `QuoteWithdrawn` variant from `RejectCause` (it becomes `Declined, Lapsed, QuoteExpired`). Nothing produces it any more; the compiler finds any leftover use.

- [ ] **Step 4: Wire up dispatch and timers**

In `apply_user`, add these arms before `_ =>`:
```rust
            Action::Accept { request, picks } => self.accept(actor, *request, picks, out),
            Action::Reject { request } => self.reject(actor, *request, out),
```
In `fire_due_timers`, add this arm before `_ => {}` (the `RequestDeadline` arm is already there from Task 4):
```rust
                TimerKey::AcceptBy(r) => self.on_accept_by(r, out),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rfq-core`
Expected: all pass, including 14 in `selection_accept`.

- [ ] **Step 6: Commit**

```bash
git add crates/rfq-core
git commit -m "feat(core): sealed selection at deadline, firm selected quotes, atomic accept into escrow, lapse"
```

