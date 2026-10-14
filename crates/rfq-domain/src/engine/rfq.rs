use std::collections::BTreeSet;

use super::{Engine, Handled, Out, VALIDATED};
use crate::command::{LegSpec, Pick};
use crate::event::{Emitted, Event, RejectReason};
use crate::model::*;
use crate::types::*;

impl Engine {
    pub(super) fn submit_request(
        &mut self,
        actor: AccountId,
        legs: &[LegSpec],
        response_deadline: Timestamp,
        out: &mut Out,
    ) -> Handled {
        let cfg = self.config();
        if legs.len() != 1 {
            return Err(RejectReason::Unsupported); // multi-leg is designed, not built (RFC P6)
        }
        let earliest = self
            .now
            .checked_add(cfg.min_response)
            .ok_or(RejectReason::Overflow)?;
        let latest = self.now.saturating_add(cfg.max_response);
        if response_deadline < earliest || response_deadline > latest {
            return Err(RejectReason::InvalidInput);
        }
        let last_accept = response_deadline
            .checked_add(cfg.accept_window)
            .ok_or(RejectReason::Overflow)?;
        let mut total: Amount = 0;
        // Every leg is checked, so the trading-close guard is the min over legs (engine.md §8).
        for l in legs {
            let c = self
                .contracts
                .get(&l.contract)
                .ok_or(RejectReason::UnknownObject)?;
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
        self.requests.insert(
            id,
            Request {
                id,
                requester: actor,
                legs: legs
                    .iter()
                    .map(|l| Leg {
                        contract: l.contract,
                        side: l.side,
                        qty: l.qty,
                        limit_price: l.limit_price,
                        quotes: BTreeSet::new(),
                        selected: None,
                    })
                    .collect(),
                response_deadline,
                hold: Some(hold),
                state: RequestState::Open,
            },
        );
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
        self.quotes.insert(
            id,
            Quote {
                id,
                mm: actor,
                request,
                leg,
                price,
                size,
                expires_at,
                hold: Some(hold),
                state: QuoteState::Active,
            },
        );
        self.requests.get_mut(&request).expect(VALIDATED).legs[leg as usize]
            .quotes
            .insert(id);
        self.arm(expires_at, TimerKey::QuoteExpiry(id));
        out.push(Emitted::to(
            actor,
            Event::QuoteSubmitted {
                quote: id,
                request,
                price,
                expires_at,
            },
        ));
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
        let Some(q) = self.quotes.get(&id) else {
            return;
        };
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

    /// Fired at `response_deadline`. Picks the best eligible quote per leg; all-or-nothing.
    pub(super) fn on_request_deadline(&mut self, id: RequestId, deadline: Timestamp, out: &mut Out) {
        let Some(r) = self.requests.get(&id) else {
            return;
        };
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
                    .filter(|q| {
                        q.state == QuoteState::Active && q.price <= l.limit_price && q.expires_at >= accept_by
                    })
                    .min_by_key(|q| (q.price, q.id))
                    .map(|q| Pick {
                        leg: u32::try_from(i).expect("leg count is bounded"),
                        quote: q.id,
                        price: q.price,
                    })
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
        out.push(Emitted::to(
            requester,
            Event::RequestPresented {
                request: id,
                picks,
                accept_by,
            },
        ));
    }

    pub(super) fn on_accept_by(&mut self, id: RequestId, out: &mut Out) {
        let Some(r) = self.requests.get(&id) else {
            return;
        };
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
    pub(super) fn accept(
        &mut self,
        actor: AccountId,
        id: RequestId,
        picks: &[Pick],
        out: &mut Out,
    ) -> Handled {
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
        self.ledger
            .escrow_from_hold(hold, requester_total)
            .expect(VALIDATED);
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
            self.positions.insert(
                pid,
                Position {
                    id: pid,
                    contract: l.contract,
                    request: id,
                    qty: l.qty,
                    yes_holder,
                    yes_lock,
                    no_holder,
                    no_lock,
                    settled: false,
                },
            );
            self.contracts
                .get_mut(&l.contract)
                .expect(VALIDATED)
                .positions
                .push(pid);
            out.push(Emitted::to(
                q.mm,
                Event::QuoteFilled {
                    quote: qid,
                    position: pid,
                },
            ));
            positions.push(pid);
        }
        let r = self.requests.get_mut(&id).expect(VALIDATED);
        r.state = RequestState::Filled;
        r.hold = None;
        for l in &mut r.legs {
            l.quotes.clear();
        }
        out.push(Emitted::to(
            requester,
            Event::RequestFilled {
                request: id,
                positions,
            },
        ));
        Ok(())
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
            QuoteState::Active | QuoteState::Selected | QuoteState::Filled => {
                unreachable!("not a release state")
            }
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
            let terminal = if self.quotes[&q].state == QuoteState::Selected {
                QuoteState::Released
            } else {
                QuoteState::Lost
            };
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
