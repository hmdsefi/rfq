//! The only code that moves money. Every account's funds are split into
//! free / reserved (backing a live request, quote or bond) / escrowed (backing a position).
//!
//! Every mutation computes all of its arithmetic before the first write, so a rejected call leaves
//! the ledger untouched. Overflow and underflow are returned as errors, never wrapped or panicked on.
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

/// Money addition. Overflow is an error, never a wrap.
fn add(x: Amount, y: Amount) -> Result<Amount, LedgerError> {
    x.checked_add(y).ok_or(LedgerError::Overflow)
}

/// Money subtraction. A bucket that cannot cover `y` reports `InsufficientFunds`.
fn sub(x: Amount, y: Amount) -> Result<Amount, LedgerError> {
    x.checked_sub(y).ok_or(LedgerError::InsufficientFunds)
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
        // free ≤ deposited, so the account total cannot overflow once the deposited total fits.
        // Both are checked anyway, so no path relies on that argument.
        let deposited = add(self.deposited, x)?;
        let free = add(self.balance(a).free, x)?;
        self.deposited = deposited;
        self.balances.entry(a).or_default().free = free;
        Ok(())
    }

    pub fn withdraw(&mut self, a: AccountId, x: Amount) -> Result<(), LedgerError> {
        let b = self.balances.get_mut(&a).ok_or(LedgerError::InsufficientFunds)?;
        let free = sub(b.free, x)?;
        let withdrawn = add(self.withdrawn, x)?;
        b.free = free;
        self.withdrawn = withdrawn;
        Ok(())
    }

    /// free → reserved. Returns the hold that now owns the funds.
    pub fn hold(&mut self, a: AccountId, x: Amount) -> Result<HoldId, LedgerError> {
        let b = self.balances.get_mut(&a).ok_or(LedgerError::InsufficientFunds)?;
        let free = sub(b.free, x)?;
        let reserved = add(b.reserved, x)?;
        let id = HoldId(add(self.next_hold, 1)?);
        b.free = free;
        b.reserved = reserved;
        self.next_hold = id.0;
        self.holds.insert(
            id,
            Hold {
                account: a,
                amount: x,
            },
        );
        Ok(id)
    }

    /// reserved → free for the whole hold.
    pub fn release(&mut self, h: HoldId) -> Result<Amount, LedgerError> {
        let hold = *self.holds.get(&h).ok_or(LedgerError::UnknownHold)?;
        let b = self
            .balances
            .get_mut(&hold.account)
            .expect("hold owner has a balance");
        let reserved = sub(b.reserved, hold.amount)?;
        let free = add(b.free, hold.amount)?;
        b.reserved = reserved;
        b.free = free;
        self.holds.remove(&h);
        Ok(hold.amount)
    }

    /// Moves `x` of the hold into escrow and releases the remainder. Consumes the hold.
    pub fn escrow_from_hold(&mut self, h: HoldId, x: Amount) -> Result<(), LedgerError> {
        let hold = *self.holds.get(&h).ok_or(LedgerError::UnknownHold)?;
        let remainder = sub(hold.amount, x)?;
        let b = self
            .balances
            .get_mut(&hold.account)
            .expect("hold owner has a balance");
        let reserved = sub(b.reserved, hold.amount)?;
        let escrowed = add(b.escrowed, x)?;
        let free = add(b.free, remainder)?;
        b.reserved = reserved;
        b.escrowed = escrowed;
        b.free = free;
        self.holds.remove(&h);
        Ok(())
    }

    /// escrowed(from) → free(to). Used at settlement.
    pub fn pay_from_escrow(&mut self, from: AccountId, x: Amount, to: AccountId) -> Result<(), LedgerError> {
        let escrowed = sub(self.balance(from).escrowed, x)?;
        let to_free = add(self.balance(to).free, x)?;
        // `from` must exist even when x == 0. Paying out of an unknown account is never valid.
        let src = self
            .balances
            .get_mut(&from)
            .ok_or(LedgerError::InsufficientFunds)?;
        src.escrowed = escrowed;
        self.balances.entry(to).or_default().free = to_free;
        Ok(())
    }

    /// reserved(owner) → free(to) for the whole hold. Used to forfeit a dispute bond.
    pub fn forfeit(&mut self, h: HoldId, to: AccountId) -> Result<Amount, LedgerError> {
        let hold = *self.holds.get(&h).ok_or(LedgerError::UnknownHold)?;
        let reserved = sub(self.balance(hold.account).reserved, hold.amount)?;
        let to_free = add(self.balance(to).free, hold.amount)?;
        let owner = self
            .balances
            .get_mut(&hold.account)
            .expect("hold owner has a balance");
        owner.reserved = reserved;
        self.balances.entry(to).or_default().free = to_free;
        self.holds.remove(&h);
        Ok(hold.amount)
    }

    /// I1 conservation and I2 hold accounting.
    pub fn check(&self) -> Result<(), String> {
        let total: u128 = self
            .balances
            .values()
            .map(|b| b.free as u128 + b.reserved as u128 + b.escrowed as u128)
            .sum();
        let expected = (self.deposited as u128)
            .checked_sub(self.withdrawn as u128)
            .ok_or_else(|| {
                format!(
                    "I1 conservation: withdrawn {} exceeds deposited {}",
                    self.withdrawn, self.deposited
                )
            })?;
        if total != expected {
            return Err(format!(
                "I1 conservation: total {total} != deposited - withdrawn {expected}"
            ));
        }
        let mut held: BTreeMap<AccountId, Amount> = BTreeMap::new();
        for h in self.holds.values() {
            *held.entry(h.account).or_default() += h.amount;
        }
        for (a, b) in &self.balances {
            let h = held.remove(a).unwrap_or(0);
            if b.reserved != h {
                return Err(format!(
                    "I2 holds: {a:?} reserved {} != live holds {h}",
                    b.reserved
                ));
            }
        }
        if let Some(a) = held.keys().next() {
            return Err(format!("I2 holds: hold for unknown account {a:?}"));
        }
        Ok(())
    }
}

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
        assert_eq!(
            l.balance(A),
            Balance {
                free: 40,
                reserved: 60,
                escrowed: 0
            }
        );
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
        assert_eq!(
            l.balance(A),
            Balance {
                free: 100,
                reserved: 0,
                escrowed: 0
            }
        );
        assert_eq!(l.release(h), Err(LedgerError::UnknownHold));
        l.check().unwrap();
    }

    #[test]
    fn escrow_from_hold_splits_and_consumes_hold() {
        let mut l = funded(100);
        let h = l.hold(A, 70).unwrap();
        l.escrow_from_hold(h, 60).unwrap();
        assert_eq!(
            l.balance(A),
            Balance {
                free: 40,
                reserved: 0,
                escrowed: 60
            }
        );
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

    // Added beyond the brief: regression and coverage for paths the brief leaves untested.

    #[test]
    fn zero_amount_ops_on_unknown_account_fail_without_panicking() {
        let mut l = Ledger::default();
        assert_eq!(l.withdraw(B, 0), Err(LedgerError::InsufficientFunds));
        assert_eq!(l.pay_from_escrow(B, 0, A), Err(LedgerError::InsufficientFunds));
        assert_eq!(l.accounts().count(), 0, "failed ops must not create accounts");
        l.check().unwrap();
    }

    #[test]
    fn pay_more_than_escrowed_fails_without_mutation() {
        let mut l = funded(100);
        let h = l.hold(A, 50).unwrap();
        l.escrow_from_hold(h, 50).unwrap();
        assert_eq!(l.pay_from_escrow(A, 51, B), Err(LedgerError::InsufficientFunds));
        assert_eq!(
            l.balance(A),
            Balance {
                free: 50,
                reserved: 0,
                escrowed: 50
            }
        );
        assert_eq!(l.accounts().count(), 1);
        l.check().unwrap();
    }

    #[test]
    fn unknown_hold_is_rejected_without_mutation() {
        let mut l = funded(100);
        assert_eq!(l.escrow_from_hold(HoldId(9), 1), Err(LedgerError::UnknownHold));
        assert_eq!(l.forfeit(HoldId(9), B), Err(LedgerError::UnknownHold));
        assert_eq!(l.balance(A).free, 100);
        assert_eq!(l.accounts().count(), 1);
        l.check().unwrap();
    }

    #[test]
    fn check_detects_broken_conservation() {
        let mut l = funded(100);
        l.balances.get_mut(&A).unwrap().free += 1;
        assert!(l.check().unwrap_err().starts_with("I1"));
    }

    #[test]
    fn check_detects_reserved_without_hold() {
        let mut l = funded(100);
        l.hold(A, 60).unwrap();
        let b = l.balances.get_mut(&A).unwrap();
        b.free -= 1;
        b.reserved += 1;
        assert!(l.check().unwrap_err().starts_with("I2"));
    }

    #[test]
    fn check_reports_withdrawn_above_deposited_without_panicking() {
        let mut l = funded(100);
        l.withdrawn = 101;
        assert!(l.check().unwrap_err().starts_with("I1"));
    }
}
