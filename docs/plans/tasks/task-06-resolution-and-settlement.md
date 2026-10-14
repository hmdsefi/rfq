# Task 6: Resolution and settlement

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Files:**
- Modify: `crates/rfq-core/src/engine/contract.rs`, `crates/rfq-core/src/engine/mod.rs`
- Create: `crates/rfq-core/tests/resolution.rs`

**Interfaces:**
- Consumes: positions created by `accept` (Task 5).
- Produces: handlers `propose_outcome`, `dispute`, `rule`, `on_dispute_window`, `on_hard_deadline`, `finalize`.

- [ ] **Step 1: Write the failing tests**

`crates/rfq-core/tests/resolution.rs`:
```rust
use rfq_core::testkit::{accounts::*, *};
use rfq_core::*;

const CLOSE: Timestamp = 100_000;
const HARD: Timestamp = 500_000;
const WINDOW: Duration = 10_000;

/// ALICE holds 1000 YES bought at 0.60 from MM2; contract closed.
fn filled() -> (Harness, ContractId) {
    let mut h = Harness::new(Config::default());
    for a in [ALICE, MM2, CAROL] {
        h.fund(a, 1_000);
    }
    let c = ContractId(1);
    h.user(OPERATOR, Action::ListContract { id: c, description: "q".into(), oracle: ORACLE, trading_close: CLOSE, resolution_deadline: HARD, dispute_window: WINDOW });
    let r = RequestId(h.next_seq());
    h.user(ALICE, Action::SubmitRequest { legs: vec![LegSpec { contract: c, side: Side::BuyYes, qty: 1_000, limit_price: price(7_000) }], response_deadline: 10_000 });
    let q = QuoteId(h.next_seq());
    h.user(MM2, Action::SubmitQuote { request: r, leg: 0, price: price(6_000), size: 1_000, expires_at: 90_000 });
    h.tick(10_000);
    let out = h.user(ALICE, Action::Accept { request: r, picks: vec![Pick { leg: 0, quote: q, price: price(6_000) }] });
    assert_eq!(rejection(&out), None);
    h.tick(CLOSE);
    (h, c)
}

fn free(h: &Harness, a: AccountId) -> Amount { h.engine.balance(a).free }

#[test]
fn yes_pays_yes_holder_after_window() {
    let (mut h, c) = filled();
    assert_eq!(rejection(&h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes })), None);
    let out = h.tick(CLOSE + WINDOW);
    assert_eq!(names(&out), vec!["ContractFinalized", "PositionSettled", "PositionSettled"]);
    assert_eq!(free(&h, ALICE), dollars(1_400));
    assert_eq!(free(&h, MM2), dollars(600));
    assert_eq!(h.engine.balance(ALICE).escrowed + h.engine.balance(MM2).escrowed, 0);
}

#[test]
fn no_pays_no_holder() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::No });
    h.tick(CLOSE + WINDOW);
    assert_eq!(free(&h, ALICE), dollars(400));
    assert_eq!(free(&h, MM2), dollars(1_600));
}

#[test]
fn propose_rules() {
    let mut h = Harness::new(Config::default());
    let c = ContractId(1);
    h.user(OPERATOR, Action::ListContract { id: c, description: "q".into(), oracle: ORACLE, trading_close: CLOSE, resolution_deadline: HARD, dispute_window: WINDOW });
    let out = h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState), "trading still open");
    h.tick(CLOSE);
    let out = h.user(ALICE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    assert_eq!(rejection(&out), Some(RejectReason::Unauthorized));
}

#[test]
fn dispute_upheld_forfeits_bond_to_treasury() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    assert_eq!(rejection(&h.user(CAROL, Action::Dispute { contract: c })), None);
    assert_eq!(h.engine.balance(CAROL).reserved, dollars(100));
    let out = h.user(ARBITER, Action::Rule { contract: c, outcome: Outcome::Yes });
    assert_eq!(names(&out)[0], "Ruled");
    assert_eq!(free(&h, CAROL), dollars(900));
    assert_eq!(free(&h, TREASURY), dollars(100));
    assert_eq!(free(&h, ALICE), dollars(1_400));
}

#[test]
fn dispute_overturned_returns_bond_and_invalid_refunds() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    h.user(CAROL, Action::Dispute { contract: c });
    h.user(ARBITER, Action::Rule { contract: c, outcome: Outcome::Invalid });
    assert_eq!(free(&h, CAROL), dollars(1_000));
    assert_eq!(free(&h, ALICE), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(1_000));
}

#[test]
fn dispute_rules() {
    let (mut h, c) = filled();
    assert_eq!(rejection(&h.user(CAROL, Action::Dispute { contract: c })), Some(RejectReason::InvalidState), "nothing proposed");
    h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    h.user(CAROL, Action::Dispute { contract: c });
    assert_eq!(rejection(&h.user(ALICE, Action::Dispute { contract: c })), Some(RejectReason::InvalidState), "one dispute per contract");
    assert_eq!(rejection(&h.user(CAROL, Action::Rule { contract: c, outcome: Outcome::No })), Some(RejectReason::Unauthorized));
}

#[test]
fn dispute_after_window_rejected() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    h.set_time(CLOSE + WINDOW);
    let out = h.user(CAROL, Action::Dispute { contract: c });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(free(&h, CAROL), dollars(1_000));
}

#[test]
fn oracle_silent_hard_deadline_refunds() {
    let (mut h, c) = filled();
    let out = h.tick(HARD);
    assert_eq!(names(&out)[0], "ContractFinalized");
    assert_eq!(h.engine.contract(c).unwrap().state, ContractState::Final(Outcome::Invalid));
    assert_eq!(free(&h, ALICE), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(1_000));
}

#[test]
fn arbiter_silent_hard_deadline_returns_bond() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    h.user(CAROL, Action::Dispute { contract: c });
    h.tick(HARD);
    assert_eq!(free(&h, CAROL), dollars(1_000));
    assert_eq!(free(&h, ALICE), dollars(1_000));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rfq-core --test resolution`
Expected: FAIL (`Unsupported` rejections; no finalisation).

- [ ] **Step 3: Implement resolution**

Append to the `impl Engine` block in `crates/rfq-core/src/engine/contract.rs`, and add `use super::VALIDATED;` to its imports:
```rust
    pub(super) fn propose_outcome(&mut self, actor: AccountId, id: ContractId, outcome: Outcome, out: &mut Out) -> Handled {
        let c = self.contracts.get(&id).ok_or(RejectReason::UnknownObject)?;
        if actor != c.oracle {
            return Err(RejectReason::Unauthorized);
        }
        if c.state != ContractState::Closed {
            return Err(RejectReason::InvalidState);
        }
        let dispute_until = self.now.checked_add(c.dispute_window).ok_or(RejectReason::Overflow)?;
        self.contracts.get_mut(&id).expect(VALIDATED).state = ContractState::Proposed { outcome, dispute_until };
        self.arm(dispute_until, TimerKey::DisputeWindow(id));
        out.push(Emitted::public(Event::OutcomeProposed { contract: id, outcome, dispute_until }));
        Ok(())
    }

    pub(super) fn dispute(&mut self, actor: AccountId, id: ContractId, out: &mut Out) -> Handled {
        let bond = self.config().dispute_bond;
        let c = self.contracts.get(&id).ok_or(RejectReason::UnknownObject)?;
        let ContractState::Proposed { outcome, dispute_until } = c.state else {
            return Err(RejectReason::InvalidState);
        };
        if self.now >= dispute_until {
            return Err(RejectReason::InvalidState);
        }
        if self.ledger.balance(actor).free < bond {
            return Err(RejectReason::InsufficientFunds);
        }
        let hold = self.ledger.hold(actor, bond).expect(VALIDATED);
        let c = self.contracts.get_mut(&id).expect(VALIDATED);
        c.state = ContractState::Disputed { proposed: outcome };
        c.dispute = Some(Dispute { disputer: actor, hold: Some(hold) });
        out.push(Emitted::public(Event::Disputed { contract: id, disputer: actor }));
        Ok(())
    }

    pub(super) fn rule(&mut self, actor: AccountId, id: ContractId, outcome: Outcome, out: &mut Out) -> Handled {
        if actor != self.genesis().arbiter {
            return Err(RejectReason::Unauthorized);
        }
        let treasury = self.genesis().treasury;
        let c = self.contracts.get_mut(&id).ok_or(RejectReason::UnknownObject)?;
        let ContractState::Disputed { proposed } = c.state else {
            return Err(RejectReason::InvalidState);
        };
        let hold = c.dispute.as_mut().and_then(|d| d.hold.take()).expect("disputed contract holds a bond");
        let bond_returned = outcome != proposed;
        if bond_returned {
            self.ledger.release(hold).expect(VALIDATED);
        } else {
            self.ledger.forfeit(hold, treasury).expect(VALIDATED);
        }
        out.push(Emitted::public(Event::Ruled { contract: id, outcome, bond_returned }));
        self.finalize(id, outcome, out);
        Ok(())
    }

    pub(super) fn on_dispute_window(&mut self, id: ContractId, out: &mut Out) {
        let Some(c) = self.contracts.get(&id) else { return };
        if let ContractState::Proposed { outcome, .. } = c.state {
            self.finalize(id, outcome, out);
        }
    }

    /// Money can never be stuck: any contract not final by its hard deadline resolves Invalid.
    pub(super) fn on_hard_deadline(&mut self, id: ContractId, out: &mut Out) {
        let Some(c) = self.contracts.get_mut(&id) else { return };
        if matches!(c.state, ContractState::Final(_)) {
            return;
        }
        if let Some(h) = c.dispute.as_mut().and_then(|d| d.hold.take()) {
            self.ledger.release(h).expect("dispute bond hold exists");
        }
        self.finalize(id, Outcome::Invalid, out);
    }

    /// Sets the final outcome and settles every position of the contract in this command.
    fn finalize(&mut self, id: ContractId, outcome: Outcome, out: &mut Out) {
        let c = self.contracts.get_mut(&id).expect("contract exists");
        c.state = ContractState::Final(outcome);
        let positions = c.positions.clone();
        out.push(Emitted::public(Event::ContractFinalized { contract: id, outcome }));
        for pid in positions {
            let p = self.positions.get_mut(&pid).expect("contract position exists");
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

- [ ] **Step 4: Wire up dispatch and timers**

In `apply_user`, add these arms and **delete the `_ => Err(RejectReason::Unsupported)` arm**, so the match is exhaustive:
```rust
            Action::ProposeOutcome { contract, outcome } => self.propose_outcome(actor, *contract, *outcome, out),
            Action::Dispute { contract } => self.dispute(actor, *contract, out),
            Action::Rule { contract, outcome } => self.rule(actor, *contract, *outcome, out),
```
In `fire_due_timers`, add these arms and **delete `_ => {}`**:
```rust
                TimerKey::DisputeWindow(c) => self.on_dispute_window(c, out),
                TimerKey::HardDeadline(c) => self.on_hard_deadline(c, out),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rfq-core`
Expected: all pass, including 9 in `resolution`, and there are no exhaustiveness warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/rfq-core
git commit -m "feat(core): oracle proposals, bonded disputes, arbiter rulings, hard-deadline invalidation and settlement"
```

