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
        self.contracts.insert(
            id,
            Contract {
                id,
                description: description.to_owned(),
                oracle,
                trading_close,
                resolution_deadline,
                dispute_window,
                state: ContractState::Trading,
                dispute: None,
                positions: Vec::new(),
            },
        );
        self.arm(trading_close, TimerKey::ContractClose(id));
        self.arm(resolution_deadline, TimerKey::HardDeadline(id));
        out.push(Emitted::public(Event::ContractListed { contract: id }));
        Ok(())
    }

    pub(super) fn on_contract_close(&mut self, id: ContractId, out: &mut Out) {
        let Some(c) = self.contracts.get_mut(&id) else {
            return;
        };
        if c.state != ContractState::Trading {
            return;
        }
        c.state = ContractState::Closed;
        out.push(Emitted::public(Event::ContractClosed { contract: id }));
    }
}
