# Task 6: Resolution and settlement

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Gate A rulings applied here** ([decisions](../../reviews/spec-review.md#decisions)):
- **#3 + #20:** a timeout never turns a proposed outcome into `Invalid`. `ProposeOutcome` also requires `now + dispute_window < resolution_deadline` (else `InvalidState`), so the dispute timer always fires strictly before the hard deadline. At the hard deadline, `Disputed` ⇒ `Final(proposed)` with the bond **returned** (the arbiter failed, not the disputer); `Closed` (never proposed) ⇒ `Final(Invalid)`.
- **#4:** downtime can't eat a dispute window. `Command::Resume` (the node sends it first after a restart; Task 3 made it a no-op) is handled **before** due timers fire: every `Proposed { dispute_until }` with `dispute_until < now + resume_grace` gets `dispute_until = min(now + resume_grace, resolution_deadline − 1)`, a re-armed timer and a re-emitted `OutcomeProposed`. `on_dispute_window(c, at)` finalizes only if the market is `Proposed` and `at >= dispute_until`, so the old timer is a stale no-op. Accept-window lapse, quote expiry, request deadline and hard deadline are refund-safe and fire normally.

**Files:**
- Modify: `crates/rfq-core/src/engine/market.rs`, `crates/rfq-core/src/engine/mod.rs`
- Create: `crates/rfq-core/tests/resolution.rs`

**Interfaces:**
- Consumes: positions created by `accept` (Task 5); `Command::Resume` and `Config::resume_grace` (Task 3).
- Produces: handlers `propose_outcome`, `dispute`, `rule`, `on_dispute_window(MarketId, Timestamp, &mut Out)`, `on_hard_deadline`, `on_resume`, `finalize`.

- [ ] **Step 1: Write the failing tests**

`crates/rfq-core/tests/resolution.rs`:
```rust
use rfq_core::testkit::{accounts::*, *};
use rfq_core::*;

const CLOSE: Timestamp = 100_000;
const HARD: Timestamp = 500_000;
const WINDOW: Duration = 10_000;

/// MM2's quote at 0.60 for ALICE's 1000 YES (limit 0.70) is presented; accept_by = 40_000.
fn presented(cfg: Config) -> (Harness, MarketId, RequestId, QuoteId) {
    let mut h = Harness::new(cfg);
    for a in [ALICE, MM2, CAROL] {
        h.fund(a, 1_000);
    }
    let c = MarketId(1);
    h.user(OPERATOR, Action::ListMarket { id: c, description: "q".into(), oracle: ORACLE, trading_close: CLOSE, resolution_deadline: HARD, dispute_window: WINDOW });
    let r = RequestId(h.next_seq());
    h.user(ALICE, Action::SubmitRequest { legs: vec![LegSpec { market: c, side: Side::BuyYes, qty: 1_000, limit_price: price(7_000) }], response_deadline: 10_000 });
    let q = QuoteId(h.next_seq());
    h.user(MM2, Action::SubmitQuote { request: r, leg: 0, price: price(6_000), size: 1_000, expires_at: 90_000 });
    h.tick(10_000);
    (h, c, r, q)
}

/// ALICE holds 1000 YES bought at 0.60 from MM2; market closed.
fn filled_with(cfg: Config) -> (Harness, MarketId) {
    let (mut h, c, r, q) = presented(cfg);
    let out = h.user(ALICE, Action::Accept { request: r, picks: vec![Pick { leg: 0, quote: q, price: price(6_000) }] });
    assert_eq!(rejection(&out), None);
    h.tick(CLOSE);
    (h, c)
}

fn filled() -> (Harness, MarketId) { filled_with(Config::default()) }

fn free(h: &Harness, a: AccountId) -> Amount { h.engine.balance(a).free }

/// Short grace so the uncapped extension is visible below HARD.
fn short_grace() -> Config { Config { resume_grace: 20_000, ..Config::default() } }

#[test]
fn yes_pays_yes_holder_after_window() {
    let (mut h, c) = filled();
    assert_eq!(rejection(&h.user(ORACLE, Action::ProposeOutcome { market: c, outcome: Outcome::Yes })), None);
    let out = h.tick(CLOSE + WINDOW);
    assert_eq!(names(&out), vec!["MarketFinalized", "PositionSettled", "PositionSettled"]);
    assert_eq!(free(&h, ALICE), dollars(1_400));
    assert_eq!(free(&h, MM2), dollars(600));
    assert_eq!(h.engine.balance(ALICE).escrowed + h.engine.balance(MM2).escrowed, 0);
}

#[test]
fn no_pays_no_holder() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { market: c, outcome: Outcome::No });
    h.tick(CLOSE + WINDOW);
    assert_eq!(free(&h, ALICE), dollars(400));
    assert_eq!(free(&h, MM2), dollars(1_600));
}

#[test]
fn propose_rules() {
    let mut h = Harness::new(Config::default());
    let c = MarketId(1);
    h.user(OPERATOR, Action::ListMarket { id: c, description: "q".into(), oracle: ORACLE, trading_close: CLOSE, resolution_deadline: HARD, dispute_window: WINDOW });
    let out = h.user(ORACLE, Action::ProposeOutcome { market: c, outcome: Outcome::Yes });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState), "trading still open");
    h.tick(CLOSE);
    let out = h.user(ALICE, Action::ProposeOutcome { market: c, outcome: Outcome::Yes });
    assert_eq!(rejection(&out), Some(RejectReason::Unauthorized));
}

#[test]
fn propose_too_late_rejected() {
    // The dispute window must end strictly before the hard deadline.
    let (mut h, c) = filled();
    h.set_time(HARD - WINDOW);
    let out = h.user(ORACLE, Action::ProposeOutcome { market: c, outcome: Outcome::Yes });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(h.engine.market(c).unwrap().state, MarketState::Closed);

    let (mut h, c) = filled();
    h.set_time(HARD - WINDOW - 1);
    assert_eq!(rejection(&h.user(ORACLE, Action::ProposeOutcome { market: c, outcome: Outcome::Yes })), None, "last moment that fits");
    let out = h.tick(HARD - 1);
    assert_eq!(names(&out)[0], "MarketFinalized");
    assert_eq!(h.engine.market(c).unwrap().state, MarketState::Final(Outcome::Yes));
}

#[test]
fn dispute_upheld_forfeits_bond_to_treasury() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { market: c, outcome: Outcome::Yes });
    assert_eq!(rejection(&h.user(CAROL, Action::Dispute { market: c })), None);
    assert_eq!(h.engine.balance(CAROL).reserved, dollars(100));
    let out = h.user(ARBITER, Action::Rule { market: c, outcome: Outcome::Yes });
    assert_eq!(names(&out)[0], "Ruled");
    assert_eq!(free(&h, CAROL), dollars(900));
    assert_eq!(free(&h, TREASURY), dollars(100));
    assert_eq!(free(&h, ALICE), dollars(1_400));
}

#[test]
fn dispute_overturned_returns_bond_and_invalid_refunds() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { market: c, outcome: Outcome::Yes });
    h.user(CAROL, Action::Dispute { market: c });
    h.user(ARBITER, Action::Rule { market: c, outcome: Outcome::Invalid });
    assert_eq!(free(&h, CAROL), dollars(1_000));
    assert_eq!(free(&h, ALICE), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(1_000));
}

#[test]
fn dispute_rules() {
    let (mut h, c) = filled();
    assert_eq!(rejection(&h.user(CAROL, Action::Dispute { market: c })), Some(RejectReason::InvalidState), "nothing proposed");
    h.user(ORACLE, Action::ProposeOutcome { market: c, outcome: Outcome::Yes });
    h.user(CAROL, Action::Dispute { market: c });
    assert_eq!(rejection(&h.user(ALICE, Action::Dispute { market: c })), Some(RejectReason::InvalidState), "one dispute per market");
    assert_eq!(rejection(&h.user(CAROL, Action::Rule { market: c, outcome: Outcome::No })), Some(RejectReason::Unauthorized));
}

#[test]
fn dispute_after_window_rejected() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { market: c, outcome: Outcome::Yes });
    h.set_time(CLOSE + WINDOW);
    let out = h.user(CAROL, Action::Dispute { market: c });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(free(&h, CAROL), dollars(1_000));
}

#[test]
fn oracle_silent_hard_deadline_refunds() {
    let (mut h, c) = filled();
    let out = h.tick(HARD);
    assert_eq!(names(&out)[0], "MarketFinalized");
    assert_eq!(h.engine.market(c).unwrap().state, MarketState::Final(Outcome::Invalid));
    assert_eq!(free(&h, ALICE), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(1_000));
}

#[test]
fn arbiter_silent_hard_deadline_returns_bond() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { market: c, outcome: Outcome::Yes });
    h.user(CAROL, Action::Dispute { market: c });
    let out = h.tick(HARD);
    assert_eq!(names(&out)[0], "MarketFinalized");
    // A timeout never turns a proposal into Invalid: the proposal stands, the bond comes back.
    assert_eq!(h.engine.market(c).unwrap().state, MarketState::Final(Outcome::Yes));
    assert_eq!(free(&h, ALICE), dollars(1_400));
    assert_eq!(free(&h, MM2), dollars(600));
    assert_eq!(free(&h, CAROL), dollars(1_000));
}

#[test]
fn resume_extends_open_dispute_window() {
    // A wrong proposal at CLOSE; the node is down from before the window ended until CLOSE + 50_000.
    let (mut h, c) = filled_with(short_grace());
    h.user(ORACLE, Action::ProposeOutcome { market: c, outcome: Outcome::No });
    h.set_time(CLOSE + 50_000);
    let out = h.submit(Command::Resume);
    let until = CLOSE + 50_000 + 20_000;
    assert_eq!(
        out,
        vec![Emitted::public(Event::OutcomeProposed { market: c, outcome: Outcome::No, dispute_until: until })],
        "re-announced; the old timer (CLOSE + WINDOW) fired as a stale no-op",
    );
    assert_eq!(h.engine.market(c).unwrap().state, MarketState::Proposed { outcome: Outcome::No, dispute_until: until });
    assert_eq!(rejection(&h.user(ALICE, Action::Dispute { market: c })), None, "holders still get a sequenced chance");

    // Near the hard deadline the extension is capped at HARD - 1, so the window still ends first.
    let (mut h, c) = filled_with(short_grace());
    h.user(ORACLE, Action::ProposeOutcome { market: c, outcome: Outcome::Yes });
    h.set_time(HARD - 5_000);
    let out = h.submit(Command::Resume);
    assert_eq!(out, vec![Emitted::public(Event::OutcomeProposed { market: c, outcome: Outcome::Yes, dispute_until: HARD - 1 })]);
    let out = h.tick(HARD - 1);
    assert_eq!(names(&out)[0], "MarketFinalized");
    assert_eq!(free(&h, ALICE), dollars(1_400), "the proposal stands; the hard deadline never sees it");
}

#[test]
fn resume_does_not_extend_accept_window() {
    // A lapse only refunds, so after downtime it fires normally, before anything else.
    let (mut h, _, r, q) = presented(short_grace());
    h.set_time(45_000); // down across accept_by = 40_000
    let out = h.submit(Command::Resume);
    assert_eq!(names(&out), vec!["QuoteReleased", "RequestRejected"]);
    assert_eq!(h.engine.request(r).unwrap().state, RequestState::Rejected(RejectCause::Lapsed));
    assert_eq!(h.engine.quote(q).unwrap().state, QuoteState::Released);
    assert_eq!(free(&h, ALICE), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(1_000));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rfq-core --test resolution`
Expected: FAIL (`Unsupported` rejections; no finalisation; `Resume` extends nothing).

- [ ] **Step 3: Implement resolution**

Append to the `impl Engine` block in `crates/rfq-core/src/engine/market.rs`, and add `use super::VALIDATED;` to its imports:
```rust
    pub(super) fn propose_outcome(&mut self, actor: AccountId, id: MarketId, outcome: Outcome, out: &mut Out) -> Handled {
        let c = self.markets.get(&id).ok_or(RejectReason::UnknownObject)?;
        if actor != c.oracle {
            return Err(RejectReason::Unauthorized);
        }
        if c.state != MarketState::Closed {
            return Err(RejectReason::InvalidState);
        }
        let dispute_until = self.now.checked_add(c.dispute_window).ok_or(RejectReason::Overflow)?;
        // The window must end strictly before the hard deadline, so a timeout can never turn this
        // proposal into Invalid (resolution.md).
        if dispute_until >= c.resolution_deadline {
            return Err(RejectReason::InvalidState);
        }
        self.markets.get_mut(&id).expect(VALIDATED).state = MarketState::Proposed { outcome, dispute_until };
        self.arm(dispute_until, TimerKey::DisputeWindow(id));
        out.push(Emitted::public(Event::OutcomeProposed { market: id, outcome, dispute_until }));
        Ok(())
    }

    pub(super) fn dispute(&mut self, actor: AccountId, id: MarketId, out: &mut Out) -> Handled {
        let bond = self.config().dispute_bond;
        let c = self.markets.get(&id).ok_or(RejectReason::UnknownObject)?;
        let MarketState::Proposed { outcome, dispute_until } = c.state else {
            return Err(RejectReason::InvalidState);
        };
        if self.now >= dispute_until {
            return Err(RejectReason::InvalidState);
        }
        if self.ledger.balance(actor).free < bond {
            return Err(RejectReason::InsufficientFunds);
        }
        let hold = self.ledger.hold(actor, bond).expect(VALIDATED);
        let c = self.markets.get_mut(&id).expect(VALIDATED);
        c.state = MarketState::Disputed { proposed: outcome };
        c.dispute = Some(Dispute { disputer: actor, hold: Some(hold) });
        out.push(Emitted::public(Event::Disputed { market: id, disputer: actor }));
        Ok(())
    }

    pub(super) fn rule(&mut self, actor: AccountId, id: MarketId, outcome: Outcome, out: &mut Out) -> Handled {
        if actor != self.genesis().arbiter {
            return Err(RejectReason::Unauthorized);
        }
        let treasury = self.genesis().treasury;
        let c = self.markets.get_mut(&id).ok_or(RejectReason::UnknownObject)?;
        let MarketState::Disputed { proposed } = c.state else {
            return Err(RejectReason::InvalidState);
        };
        let hold = c.dispute.as_mut().and_then(|d| d.hold.take()).expect("disputed market holds a bond");
        let bond_returned = outcome != proposed;
        if bond_returned {
            self.ledger.release(hold).expect(VALIDATED);
        } else {
            self.ledger.forfeit(hold, treasury).expect(VALIDATED);
        }
        out.push(Emitted::public(Event::Ruled { market: id, outcome, bond_returned }));
        self.finalize(id, outcome, out);
        Ok(())
    }

    /// Deadline-matched: finalizes only if the window this timer was armed for is the current one.
    /// A window extended by `Resume` leaves its old timer behind as a no-op.
    pub(super) fn on_dispute_window(&mut self, id: MarketId, at: Timestamp, out: &mut Out) {
        let Some(c) = self.markets.get(&id) else { return };
        if let MarketState::Proposed { outcome, dispute_until } = c.state
            && at >= dispute_until
        {
            self.finalize(id, outcome, out);
        }
    }

    /// Money can never be stuck: every market is final by its hard deadline. A timeout never turns
    /// a proposal into Invalid: an unruled dispute keeps the proposal and returns the bond (the
    /// arbiter failed, not the disputer). Only a market the oracle never resolved becomes Invalid.
    pub(super) fn on_hard_deadline(&mut self, id: MarketId, out: &mut Out) {
        let Some(c) = self.markets.get_mut(&id) else { return };
        let state = c.state;
        let outcome = match state {
            MarketState::Final(_) => return,
            MarketState::Disputed { proposed } => {
                let hold = c.dispute.as_mut().and_then(|d| d.hold.take()).expect("disputed market holds a bond");
                self.ledger.release(hold).expect("dispute bond hold exists");
                proposed
            }
            // Unreachable: `propose_outcome` and `on_resume` keep `dispute_until < resolution_deadline`,
            // so the dispute timer always fires first. Kept consistent with the rule above.
            MarketState::Proposed { outcome, .. } => outcome,
            MarketState::Trading | MarketState::Closed => Outcome::Invalid,
        };
        self.finalize(id, outcome, out);
    }

    /// `Command::Resume`, the node's first command after a restart. Runs *before* due timers fire
    /// (see `apply`), so downtime can't eat a dispute window: every open window ending before
    /// `now + resume_grace` moves there, capped at `resolution_deadline - 1` so it still ends before
    /// the hard deadline. Nothing else is extended: the other deadlines are refund-safe.
    pub(super) fn on_resume(&mut self, out: &mut Out) {
        let until = self.now.saturating_add(self.config().resume_grace);
        let mut extended = Vec::new();
        for c in self.markets.values_mut() {
            if let MarketState::Proposed { outcome, dispute_until } = c.state {
                let new_until = until.min(c.resolution_deadline.saturating_sub(1));
                if dispute_until < new_until {
                    c.state = MarketState::Proposed { outcome, dispute_until: new_until };
                    extended.push((c.id, outcome, new_until));
                }
            }
        }
        for (id, outcome, dispute_until) in extended {
            self.arm(dispute_until, TimerKey::DisputeWindow(id));
            out.push(Emitted::public(Event::OutcomeProposed { market: id, outcome, dispute_until }));
        }
    }

    /// Sets the final outcome and settles every position of the market in this command.
    fn finalize(&mut self, id: MarketId, outcome: Outcome, out: &mut Out) {
        let c = self.markets.get_mut(&id).expect("market exists");
        c.state = MarketState::Final(outcome);
        let positions = c.positions.clone();
        out.push(Emitted::public(Event::MarketFinalized { market: id, outcome }));
        for pid in positions {
            let p = self.positions.get_mut(&pid).expect("market position exists");
            if p.settled {
                continue;
            }
            let (yes_payout, no_payout) = match outcome {
                Outcome::Yes => (p.yes_lock + p.no_lock, 0),
                Outcome::No => (0, p.yes_lock + p.no_lock),
                Outcome::Invalid => (p.yes_lock, p.no_lock),
            };
            let (yes_to, no_to) = match outcome {
                Outcome::Yes => (p.yes_holder, p.yes_holder),
                Outcome::No => (p.no_holder, p.no_holder),
                Outcome::Invalid => (p.yes_holder, p.no_holder),
            };
            self.ledger.pay_from_escrow(p.yes_holder, p.yes_lock, yes_to).expect("position escrow exists");
            self.ledger.pay_from_escrow(p.no_holder, p.no_lock, no_to).expect("position escrow exists");
            p.settled = true;
            let event = Event::PositionSettled { position: pid, yes_holder: p.yes_holder, yes_payout, no_holder: p.no_holder, no_payout };
            out.push(Emitted::to(p.yes_holder, event.clone()));
            out.push(Emitted::to(p.no_holder, event));
        }
    }
```

- [ ] **Step 4: Wire up dispatch, timers and `Resume`**

In `apply_user`, add these arms and **delete the `_ => Err(RejectReason::Unsupported)` arm**, so the match is exhaustive:
```rust
            Action::ProposeOutcome { market, outcome } => self.propose_outcome(actor, *market, *outcome, out),
            Action::Dispute { market } => self.dispute(actor, *market, out),
            Action::Rule { market, outcome } => self.rule(actor, *market, *outcome, out),
```
In `fire_due_timers`, add these arms and **delete `_ => {}`**:
```rust
                TimerKey::DisputeWindow(c) => self.on_dispute_window(c, at, out),
                TimerKey::HardDeadline(c) => self.on_hard_deadline(c, out),
```
In `Engine::apply`, `Resume` must act **before** the due timers fire. Replace
```rust
        if self.genesis.is_some() {
            self.fire_due_timers(&mut out);
        }
```
with
```rust
        if self.genesis.is_some() {
            // Resume first, so downtime can't eat a dispute window (engine.md §6.3).
            if matches!(sc.cmd, Command::Resume) {
                self.on_resume(&mut out);
            }
            self.fire_due_timers(&mut out);
        }
```
The `Command::Tick | Command::Resume => {}` arm of the `match &sc.cmd` stays as Task 3 wrote it: `Resume`'s work is done above. Task 3's unit test `resume_behaves_like_tick_for_now` still passes (it has no `Proposed` market); rename it `resume_without_open_dispute_windows_is_a_tick` so its name stays true.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rfq-core`, then `cargo clippy --workspace --all-targets -- -D warnings`
Expected: all pass, including 12 in `resolution`; no exhaustiveness warnings; clippy is clean.

- [ ] **Step 6: Commit**

```bash
git add crates/rfq-core
git commit -m "feat(core): oracle proposals, bonded disputes, arbiter rulings, hard-deadline finality, resume grace"
```
