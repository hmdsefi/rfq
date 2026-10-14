use super::{Engine, Handled, Out};
use crate::event::{Emitted, Event, RejectReason};
use crate::ledger::LedgerError;
use crate::types::*;

impl Engine {
    pub(super) fn deposit(
        &mut self,
        actor: AccountId,
        to: AccountId,
        amount: Amount,
        out: &mut Out,
    ) -> Handled {
        if actor != self.genesis().operator {
            return Err(RejectReason::Unauthorized);
        }
        if amount == 0 {
            return Err(RejectReason::InvalidInput);
        }
        self.ledger.deposit(to, amount).map_err(|e| match e {
            LedgerError::Overflow => RejectReason::Overflow,
            _ => RejectReason::InvalidInput,
        })?;
        out.push(Emitted::to(to, Event::Deposited { account: to, amount }));
        Ok(())
    }

    pub(super) fn withdraw(&mut self, actor: AccountId, amount: Amount, out: &mut Out) -> Handled {
        if amount == 0 {
            return Err(RejectReason::InvalidInput);
        }
        self.ledger
            .withdraw(actor, amount)
            .map_err(|_| RejectReason::InsufficientFunds)?;
        out.push(Emitted::to(
            actor,
            Event::Withdrawn {
                account: actor,
                amount,
            },
        ));
        Ok(())
    }
}
