# Task 2: Ledger

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Files:**
- Create: `crates/rfq-core/src/ledger.rs`
- Modify: `crates/rfq-core/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 types.
- Produces: `Balance { free, reserved, escrowed }`, `Ledger` with `balance(AccountId) -> Balance`, `accounts() -> impl Iterator<Item=(AccountId, Balance)>`, `hold_amount(HoldId) -> Option<Amount>`, `hold_ids() -> impl Iterator<Item=HoldId>`, `deposit`, `withdraw`, `hold -> Result<HoldId>`, `release -> Result<Amount>`, `escrow_from_hold(HoldId, Amount)`, `pay_from_escrow(from, Amount, to)`, `forfeit(HoldId, to) -> Result<Amount>`, `check() -> Result<(), String>`; `LedgerError::{InsufficientFunds, UnknownHold, Overflow}`.

- [ ] **Step 1: Write the failing tests**

`crates/rfq-core/src/ledger.rs`: test module only.
```rust
#[cfg(test)]
mod tests {
    use super::*;
    const A: AccountId = AccountId(1);
    const B: AccountId = AccountId(2);

    fn funded(x: Amount) -> Ledger {
        let mut l = Ledger::default();
        l.deposit(A, x).unwrap();
        l
    }

    #[test]
    fn hold_moves_free_to_reserved() {
        let mut l = funded(100);
        let h = l.hold(A, 60).unwrap();
        assert_eq!(l.balance(A), Balance { free: 40, reserved: 60, escrowed: 0 });
        assert_eq!(l.hold_amount(h), Some(60));
        l.check().unwrap();
    }

    #[test]
    fn hold_more_than_free_fails_without_mutation() {
        let mut l = funded(100);
        assert_eq!(l.hold(A, 101), Err(LedgerError::InsufficientFunds));
        assert_eq!(l.hold(B, 0).map(|_| ()), Err(LedgerError::InsufficientFunds));
        assert_eq!(l.balance(A).free, 100);
        assert_eq!(l.accounts().count(), 1, "failed ops must not create accounts");
        l.check().unwrap();
    }

    #[test]
    fn release_returns_whole_hold_to_free() {
        let mut l = funded(100);
        let h = l.hold(A, 60).unwrap();
        assert_eq!(l.release(h), Ok(60));
        assert_eq!(l.balance(A), Balance { free: 100, reserved: 0, escrowed: 0 });
        assert_eq!(l.release(h), Err(LedgerError::UnknownHold));
        l.check().unwrap();
    }

    #[test]
    fn escrow_from_hold_splits_and_consumes_hold() {
        let mut l = funded(100);
        let h = l.hold(A, 70).unwrap();
        l.escrow_from_hold(h, 60).unwrap();
        assert_eq!(l.balance(A), Balance { free: 40, reserved: 0, escrowed: 60 });
        assert_eq!(l.hold_amount(h), None);
        l.check().unwrap();
    }

    #[test]
    fn escrow_more_than_hold_fails() {
        let mut l = funded(100);
        let h = l.hold(A, 50).unwrap();
        assert_eq!(l.escrow_from_hold(h, 51), Err(LedgerError::InsufficientFunds));
        assert_eq!(l.hold_amount(h), Some(50));
    }

    #[test]
    fn pay_from_escrow_moves_to_winner() {
        let mut l = funded(100);
        let h = l.hold(A, 100).unwrap();
        l.escrow_from_hold(h, 100).unwrap();
        l.pay_from_escrow(A, 100, B).unwrap();
        assert_eq!(l.balance(A), Balance::default());
        assert_eq!(l.balance(B).free, 100);
        l.check().unwrap();
    }

    #[test]
    fn forfeit_moves_hold_to_other_account() {
        let mut l = funded(100);
        let h = l.hold(A, 30).unwrap();
        assert_eq!(l.forfeit(h, B), Ok(30));
        assert_eq!(l.balance(A).free, 70);
        assert_eq!(l.balance(B).free, 30);
        l.check().unwrap();
    }

    #[test]
    fn withdraw_only_from_free() {
        let mut l = funded(100);
        l.hold(A, 80).unwrap();
        assert_eq!(l.withdraw(A, 21), Err(LedgerError::InsufficientFunds));
        l.withdraw(A, 20).unwrap();
        assert_eq!(l.balance(A).free, 0);
        l.check().unwrap();
    }

    #[test]
    fn deposit_overflow_fails() {
        let mut l = funded(u64::MAX);
        assert_eq!(l.deposit(B, 1), Err(LedgerError::Overflow));
        assert_eq!(l.balance(B), Balance::default());
        l.check().unwrap();
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Add `pub mod ledger;` and `pub use ledger::*;` to `lib.rs`.
Run: `cargo test -p rfq-core ledger`
Expected: FAIL to compile (`Ledger` not found).

- [ ] **Step 3: Implement the ledger**

Prepend to `crates/rfq-core/src/ledger.rs`:
```rust
//! The only code that moves money. Every account's funds are split into
//! free / reserved (backing a live request, quote or bond) / escrowed (backing a position).
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{AccountId, Amount, HoldId};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    pub free: Amount,
    pub reserved: Amount,
    pub escrowed: Amount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub account: AccountId,
    pub amount: Amount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerError {
    InsufficientFunds,
    UnknownHold,
    Overflow,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    balances: BTreeMap<AccountId, Balance>,
    holds: BTreeMap<HoldId, Hold>,
    next_hold: u64,
    deposited: Amount,
    withdrawn: Amount,
}

impl Ledger {
    pub fn balance(&self, a: AccountId) -> Balance {
        self.balances.get(&a).copied().unwrap_or_default()
    }

    pub fn accounts(&self) -> impl Iterator<Item = (AccountId, Balance)> + '_ {
        self.balances.iter().map(|(a, b)| (*a, *b))
    }

    pub fn hold_amount(&self, h: HoldId) -> Option<Amount> {
        self.holds.get(&h).map(|h| h.amount)
    }

    pub fn hold_ids(&self) -> impl Iterator<Item = HoldId> + '_ {
        self.holds.keys().copied()
    }

    pub fn deposit(&mut self, a: AccountId, x: Amount) -> Result<(), LedgerError> {
        // free ≤ deposited, so if the total doesn't overflow neither can the account.
        let deposited = self.deposited.checked_add(x).ok_or(LedgerError::Overflow)?;
        self.deposited = deposited;
        self.balances.entry(a).or_default().free += x;
        Ok(())
    }

    pub fn withdraw(&mut self, a: AccountId, x: Amount) -> Result<(), LedgerError> {
        if self.balance(a).free < x {
            return Err(LedgerError::InsufficientFunds);
        }
        self.balances.get_mut(&a).expect("checked above").free -= x;
        self.withdrawn += x;
        Ok(())
    }

    /// free → reserved. Returns the hold that now owns the funds.
    pub fn hold(&mut self, a: AccountId, x: Amount) -> Result<HoldId, LedgerError> {
        if !self.balances.contains_key(&a) || self.balance(a).free < x {
            return Err(LedgerError::InsufficientFunds);
        }
        let b = self.balances.get_mut(&a).expect("checked above");
        b.free -= x;
        b.reserved += x;
        self.next_hold += 1;
        let id = HoldId(self.next_hold);
        self.holds.insert(id, Hold { account: a, amount: x });
        Ok(id)
    }

    /// reserved → free for the whole hold.
    pub fn release(&mut self, h: HoldId) -> Result<Amount, LedgerError> {
        let hold = self.holds.remove(&h).ok_or(LedgerError::UnknownHold)?;
        let b = self.balances.get_mut(&hold.account).expect("hold owner has a balance");
        b.reserved -= hold.amount;
        b.free += hold.amount;
        Ok(hold.amount)
    }

    /// Moves `x` of the hold into escrow and releases the remainder. Consumes the hold.
    pub fn escrow_from_hold(&mut self, h: HoldId, x: Amount) -> Result<(), LedgerError> {
        let hold = *self.holds.get(&h).ok_or(LedgerError::UnknownHold)?;
        if x > hold.amount {
            return Err(LedgerError::InsufficientFunds);
        }
        self.holds.remove(&h);
        let b = self.balances.get_mut(&hold.account).expect("hold owner has a balance");
        b.reserved -= hold.amount;
        b.escrowed += x;
        b.free += hold.amount - x;
        Ok(())
    }

    /// escrowed(from) → free(to). Used at settlement.
    pub fn pay_from_escrow(&mut self, from: AccountId, x: Amount, to: AccountId) -> Result<(), LedgerError> {
        if self.balance(from).escrowed < x {
            return Err(LedgerError::InsufficientFunds);
        }
        self.balances.get_mut(&from).expect("checked above").escrowed -= x;
        self.balances.entry(to).or_default().free += x;
        Ok(())
    }

    /// reserved(owner) → free(to) for the whole hold. Used to forfeit a dispute bond.
    pub fn forfeit(&mut self, h: HoldId, to: AccountId) -> Result<Amount, LedgerError> {
        let hold = self.holds.remove(&h).ok_or(LedgerError::UnknownHold)?;
        self.balances.get_mut(&hold.account).expect("hold owner has a balance").reserved -= hold.amount;
        self.balances.entry(to).or_default().free += hold.amount;
        Ok(hold.amount)
    }

    /// I1 conservation and I2 hold accounting.
    pub fn check(&self) -> Result<(), String> {
        let total: u128 = self
            .balances
            .values()
            .map(|b| b.free as u128 + b.reserved as u128 + b.escrowed as u128)
            .sum();
        let expected = self.deposited as u128 - self.withdrawn as u128;
        if total != expected {
            return Err(format!("I1 conservation: total {total} != deposited - withdrawn {expected}"));
        }
        let mut held: BTreeMap<AccountId, Amount> = BTreeMap::new();
        for h in self.holds.values() {
            *held.entry(h.account).or_default() += h.amount;
        }
        for (a, b) in &self.balances {
            let h = held.remove(a).unwrap_or(0);
            if b.reserved != h {
                return Err(format!("I2 holds: {a:?} reserved {} != live holds {h}", b.reserved));
            }
        }
        if let Some(a) = held.keys().next() {
            return Err(format!("I2 holds: hold for unknown account {a:?}"));
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p rfq-core ledger`
Expected: 9 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/rfq-core
git commit -m "feat(core): ledger with free/reserved/escrowed buckets and conservation check"
```

