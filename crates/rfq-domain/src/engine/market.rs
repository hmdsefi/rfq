use super::{Engine, Handled, Out, VALIDATED};
use crate::event::{Emitted, Event, RejectReason};
use crate::ledger::LedgerError;
use crate::model::*;
use crate::types::*;

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn list_market(
        &mut self,
        actor: AccountId,
        id: MarketId,
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
        // list markets, or collect forfeited bonds.
        if [g.operator, g.arbiter, g.treasury].contains(&oracle) {
            return Err(RejectReason::InvalidInput);
        }
        if self.markets.contains_key(&id) {
            return Err(RejectReason::InvalidInput);
        }
        if !(self.now < trading_close && trading_close < resolution_deadline) || dispute_window == 0 {
            return Err(RejectReason::InvalidInput);
        }
        // Room to resolve (R20): a full dispute window must fit between close and the hard deadline,
        // exactly as `propose_outcome` requires. Otherwise no proposal could ever be accepted and the
        // market could only finalize Invalid.
        if trading_close
            .checked_add(dispute_window)
            .is_none_or(|window_end| window_end >= resolution_deadline)
        {
            return Err(RejectReason::InvalidInput);
        }
        self.markets.insert(
            id,
            Market {
                id,
                description: description.to_owned(),
                oracle,
                trading_close,
                resolution_deadline,
                dispute_window,
                state: MarketState::Trading,
                dispute: None,
                positions: Vec::new(),
            },
        );
        self.arm(trading_close, TimerKey::MarketClose(id));
        self.arm(resolution_deadline, TimerKey::HardDeadline(id));
        out.push(Emitted::public(Event::MarketListed { market: id }));
        Ok(())
    }

    pub(super) fn on_market_close(&mut self, id: MarketId, out: &mut Out) {
        let Some(c) = self.markets.get_mut(&id) else {
            return;
        };
        if c.state != MarketState::Trading {
            return;
        }
        c.state = MarketState::Closed;
        out.push(Emitted::public(Event::MarketClosed { market: id }));
    }

    pub(super) fn propose_outcome(
        &mut self,
        actor: AccountId,
        id: MarketId,
        outcome: Outcome,
        out: &mut Out,
    ) -> Handled {
        let c = self.markets.get(&id).ok_or(RejectReason::UnknownObject)?;
        if actor != c.oracle {
            return Err(RejectReason::Unauthorized);
        }
        if c.state != MarketState::Closed {
            return Err(RejectReason::InvalidState);
        }
        let dispute_until = self
            .now
            .checked_add(c.dispute_window)
            .ok_or(RejectReason::Overflow)?;
        // The window must end strictly before the hard deadline, so a timeout can never turn this
        // proposal into Invalid (resolution.md).
        if dispute_until >= c.resolution_deadline {
            return Err(RejectReason::InvalidState);
        }
        self.markets.get_mut(&id).expect(VALIDATED).state = MarketState::Proposed {
            outcome,
            dispute_until,
        };
        self.arm(dispute_until, TimerKey::DisputeWindow(id));
        out.push(Emitted::public(Event::OutcomeProposed {
            market: id,
            outcome,
            dispute_until,
        }));
        Ok(())
    }

    pub(super) fn dispute(&mut self, actor: AccountId, id: MarketId, out: &mut Out) -> Handled {
        let bond = self.config().dispute_bond;
        let c = self.markets.get(&id).ok_or(RejectReason::UnknownObject)?;
        let MarketState::Proposed {
            outcome,
            dispute_until,
        } = c.state
        else {
            return Err(RejectReason::InvalidState);
        };
        if self.now >= dispute_until {
            return Err(RejectReason::InvalidState);
        }
        // The ledger checks everything before it writes, so a failing `hold` leaves state untouched
        // and doubles as the funds check. (A zero bond from an account the ledger has never seen
        // fails here too, instead of panicking.)
        let hold = self.ledger.hold(actor, bond).map_err(|e| match e {
            LedgerError::Overflow => RejectReason::Overflow,
            _ => RejectReason::InsufficientFunds,
        })?;
        let c = self.markets.get_mut(&id).expect(VALIDATED);
        c.state = MarketState::Disputed { proposed: outcome };
        c.dispute = Some(Dispute {
            disputer: actor,
            hold: Some(hold),
        });
        out.push(Emitted::public(Event::Disputed {
            market: id,
            disputer: actor,
        }));
        Ok(())
    }

    pub(super) fn rule(
        &mut self,
        actor: AccountId,
        id: MarketId,
        outcome: Outcome,
        out: &mut Out,
    ) -> Handled {
        if actor != self.genesis().arbiter {
            return Err(RejectReason::Unauthorized);
        }
        let treasury = self.genesis().treasury;
        let c = self.markets.get_mut(&id).ok_or(RejectReason::UnknownObject)?;
        let MarketState::Disputed { proposed } = c.state else {
            return Err(RejectReason::InvalidState);
        };
        let hold = c
            .dispute
            .as_mut()
            .and_then(|d| d.hold.take())
            .expect("disputed market holds a bond");
        let bond_returned = outcome != proposed;
        if bond_returned {
            self.ledger.release(hold).expect(VALIDATED);
        } else {
            self.ledger.forfeit(hold, treasury).expect(VALIDATED);
        }
        out.push(Emitted::public(Event::Ruled {
            market: id,
            outcome,
            bond_returned,
        }));
        self.finalize(id, outcome, out);
        Ok(())
    }

    /// Deadline-matched: finalizes only if the window this timer was armed for is the current one.
    /// A window extended by `Resume` leaves its old timer behind as a no-op.
    pub(super) fn on_dispute_window(&mut self, id: MarketId, at: Timestamp, out: &mut Out) {
        let Some(c) = self.markets.get(&id) else {
            return;
        };
        if let MarketState::Proposed {
            outcome,
            dispute_until,
        } = c.state
            && at >= dispute_until
        {
            self.finalize(id, outcome, out);
        }
    }

    /// Money can never be stuck: every market is final by its hard deadline. A timeout never turns
    /// a proposal into Invalid: an unruled dispute keeps the proposal and returns the bond (the
    /// arbiter failed, not the disputer). Only a market the oracle never resolved becomes Invalid.
    pub(super) fn on_hard_deadline(&mut self, id: MarketId, out: &mut Out) {
        let Some(c) = self.markets.get_mut(&id) else {
            return;
        };
        let outcome = match c.state {
            MarketState::Final(_) => return,
            MarketState::Disputed { proposed } => {
                let dispute = c.dispute.as_mut().expect("disputed market has a dispute");
                let disputer = dispute.disputer;
                let hold = dispute.hold.take().expect("disputed market holds a bond");
                let amount = self.ledger.release(hold).expect("dispute bond hold exists");
                out.push(Emitted::to(
                    disputer,
                    Event::DisputeBondReturned {
                        market: id,
                        disputer,
                        amount,
                    },
                ));
                proposed
            }
            // Unreachable: `propose_outcome` and `on_resume` keep `dispute_until < resolution_deadline`,
            // so the dispute timer always fires first. Kept consistent with the rule above.
            MarketState::Proposed { outcome, .. } => outcome,
            MarketState::Trading | MarketState::Closed => Outcome::Invalid,
        };
        self.finalize(id, outcome, out);
    }

    /// `Command::Resume`: the node's first command after a restart on a non-empty journal, and also
    /// the first command of a poll that follows a gap longer than the node's `STALL_GAP_MS`. Runs
    /// *before* due timers fire (see `apply`), so downtime or a stall can't eat a dispute window:
    /// every open window ending before `now + resume_grace` moves there, capped at
    /// `resolution_deadline - 1` so it still ends before the hard deadline. Nothing else is
    /// extended: the other deadlines are refund-safe.
    pub(super) fn on_resume(&mut self, out: &mut Out) {
        let until = self.now.saturating_add(self.config().resume_grace);
        let mut extended = Vec::new();
        for c in self.markets.values_mut() {
            if let MarketState::Proposed {
                outcome,
                dispute_until,
            } = c.state
            {
                let new_until = until.min(c.resolution_deadline.saturating_sub(1));
                if dispute_until < new_until {
                    c.state = MarketState::Proposed {
                        outcome,
                        dispute_until: new_until,
                    };
                    extended.push((c.id, outcome, new_until));
                }
            }
        }
        for (id, outcome, dispute_until) in extended {
            self.arm(dispute_until, TimerKey::DisputeWindow(id));
            out.push(Emitted::public(Event::OutcomeProposed {
                market: id,
                outcome,
                dispute_until,
            }));
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
            let pot = p
                .yes_lock
                .checked_add(p.no_lock)
                .expect("locks sum to qty × $1 (I3)");
            let (yes_payout, no_payout) = match outcome {
                Outcome::Yes => (pot, 0),
                Outcome::No => (0, pot),
                Outcome::Invalid => (p.yes_lock, p.no_lock),
            };
            let (yes_to, no_to) = match outcome {
                Outcome::Yes => (p.yes_holder, p.yes_holder),
                Outcome::No => (p.no_holder, p.no_holder),
                Outcome::Invalid => (p.yes_holder, p.no_holder),
            };
            self.ledger
                .pay_from_escrow(p.yes_holder, p.yes_lock, yes_to)
                .expect("position escrow exists");
            self.ledger
                .pay_from_escrow(p.no_holder, p.no_lock, no_to)
                .expect("position escrow exists");
            p.settled = true;
            let event = Event::PositionSettled {
                position: pid,
                yes_holder: p.yes_holder,
                yes_payout,
                no_holder: p.no_holder,
                no_payout,
            };
            out.push(Emitted::to(p.yes_holder, event.clone()));
            out.push(Emitted::to(p.no_holder, event));
        }
    }
}
