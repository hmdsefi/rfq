use std::collections::BTreeSet;

use super::{Engine, Handled, Out, VALIDATED};
use crate::command::{LegSpec, MAX_LEGS, Pick, ReservationPolicy, SelectionPolicy};
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
        // The cap also bounds every `leg` index below, so the `u32` conversions can't fail.
        if legs.is_empty() || legs.len() > MAX_LEGS {
            return Err(RejectReason::InvalidInput);
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
            let c = self.markets.get(&l.market).ok_or(RejectReason::UnknownObject)?;
            if c.state != MarketState::Trading {
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
                        market: l.market,
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
                market: l.market,
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
        let need = self
            .quote_reservation(l.qty, price)
            .ok_or(RejectReason::Overflow)?;
        let previous = l.quotes.iter().copied().find(|q| self.quotes[q].mm == actor);
        // D5 tripwire: a new `ReservationPolicy` must rework the funds check and `refundable` below
        // (`SharedLimit` checks pool headroom, not the MM's free balance, and refunds nothing).
        let ReservationPolicy::FullHold = cfg.reservation;
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
        // D5 tripwire: a new `ReservationPolicy` must decide whether the quote takes a hold here
        // (`SharedLimit` takes none at submission: `hold: None` below, and `end_quote` already copes).
        let ReservationPolicy::FullHold = cfg.reservation;
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
        // D5 tripwire: a new `SelectionPolicy` must decide what a new quote does to the request here
        // (`Continuous` re-selects when this quote is the new best, emitting `QuoteSelected` and
        // `RequestPresented` with a deadline-stamped timer).
        let SelectionPolicy::SealedAtDeadline = cfg.selection;
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

    /// Seam 1 of 2 for D5 (quote lifetime): how much collateral a quote reserves in `submit_quote`.
    /// `None` means the amount overflows. `Config::reservation` is genesis data, so the choice is in the
    /// log and replay is deterministic.
    ///
    /// Contract for a future `SharedLimit` (docs/specs/quote-lifetime.md): quotes draw on an MM pool
    /// instead of holding their own collateral, and **selection is the commit point**. The selected
    /// quote converts to a hard hold at selection (`select_sealed`), skipping to the next-best quote if
    /// the pool cannot cover it. Funds are never checked at `accept`, or one pool could win many
    /// selections and fail all but one accept.
    ///
    /// A new `ReservationPolicy` variant must change every site below. Each one matches the policy
    /// exhaustively (this fn) or destructures it with an irrefutable `let` (a tripwire), so the
    /// compiler forces a decision at each listed site:
    /// - `engine/rfq.rs::quote_reservation` (this match): the amount a quote needs
    /// - `engine/rfq.rs::submit_quote`: the funds check and `refundable`
    /// - `engine/rfq.rs::submit_quote`: hold creation (`hold: Some(..)`)
    /// - `engine/rfq.rs::select_sealed`: the commit point (convert to a hard hold, skip to next-best)
    /// - `engine/rfq.rs::accept`: `mm_lock = hold_amount(..)` (no funds check at accept)
    /// - `engine/mod.rs::check_invariants`: I4 "live without a hold"
    fn quote_reservation(&self, qty: Qty, price: Price) -> Option<Amount> {
        match self.config().reservation {
            ReservationPolicy::FullHold => seller_lock(qty, price),
        }
    }

    /// Fired at `response_deadline`. Seam 2 of 2 for D5: `Config::selection` picks how quotes are
    /// compared.
    ///
    /// Contract for a future `Continuous` (docs/specs/engine.md §6, quote-lifetime.md): run the same
    /// selection step on each new best quote and allow `Accept` while the request is `Open`. It
    /// re-enters `Presented`, so every timer armed for a re-entered state must carry the deadline it
    /// expects and fire only on a match. `on_accept_by` checks only the state today, so it would need
    /// that stamp (the dispute timer already has it: `on_dispute_window`).
    ///
    /// A new `SelectionPolicy` variant must change every site below. Each one matches the policy
    /// exhaustively (this fn) or destructures it with an irrefutable `let` (a tripwire), so the
    /// compiler forces a decision at each listed site:
    /// - `engine/rfq.rs::on_request_deadline` (this match): what happens at the response deadline
    /// - `engine/rfq.rs::submit_quote`: a new best quote may trigger selection
    /// - `engine/rfq.rs::accept`: the `Presented` gate (`Continuous` also allows `Accept` while `Open`)
    /// - `engine/rfq.rs::reject`: the `Presented` gate
    /// - `engine/rfq.rs::on_accept_by`: the deadline-stamped timer requirement
    pub(super) fn on_request_deadline(&mut self, id: RequestId, deadline: Timestamp, out: &mut Out) {
        match self.config().selection {
            SelectionPolicy::SealedAtDeadline => self.select_sealed(id, deadline, out),
        }
    }

    /// `SealedAtDeadline`: quotes stay sealed until `response_deadline`, then the best eligible quote
    /// per leg is presented once; all-or-nothing.
    fn select_sealed(&mut self, id: RequestId, deadline: Timestamp, out: &mut Out) {
        let Some(r) = self.requests.get(&id) else {
            return;
        };
        if r.state != RequestState::Open {
            return;
        }
        let accept_by = deadline.saturating_add(self.config().accept_window);
        // Eligible = active, within limit, and firm through the whole acceptance window. This is the
        // only check of the sealed limit (SubmitQuote doesn't do it, so it can't be probed).
        // D5 tripwire: a new `ReservationPolicy` must commit here. `SharedLimit` converts the chosen
        // quote to a hard hold at selection and skips to the next-best quote if the pool can't cover it.
        let ReservationPolicy::FullHold = self.config().reservation;
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
        // D5 tripwire: a new `SelectionPolicy` must stamp this timer. `Continuous` re-enters `Presented`
        // with a new `accept_by`, so a stale timer must fire only if its deadline matches the state's.
        let SelectionPolicy::SealedAtDeadline = self.config().selection;
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
        // D5 tripwire: a new `SelectionPolicy` must decide when `Reject` is allowed (`Continuous`
        // presents a best quote while the request is still `Open`).
        let SelectionPolicy::SealedAtDeadline = self.config().selection;
        if !matches!(r.state, RequestState::Presented { .. }) {
            return Err(RejectReason::InvalidState);
        }
        self.close_request(id, RequestState::Rejected(RejectCause::Declined), out);
        Ok(())
    }

    /// Escrows every leg in one command. All checks (the request, every leg's market, every pick,
    /// the funds) run before the first mutation.
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
        // D5 tripwire: a new `SelectionPolicy` must decide when `Accept` is allowed (`Continuous`
        // allows it while the request is still `Open`).
        let SelectionPolicy::SealedAtDeadline = self.config().selection;
        if !matches!(r.state, RequestState::Presented { .. }) {
            return Err(RejectReason::InvalidState);
        }
        // Defence in depth (engine.md §8 rule 5): `submit_request` makes `accept_by < trading_close`
        // for every leg, so no market can leave `Trading` while the request is `Presented`. If one
        // ever did, the whole request fails here, before the first mutation.
        for l in &r.legs {
            let c = self.markets.get(&l.market).ok_or(RejectReason::UnknownObject)?;
            if c.state != MarketState::Trading {
                return Err(RejectReason::InvalidState);
            }
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
            // D5 tripwire: a new `ReservationPolicy` must decide where the MM's lock comes from.
            // `SharedLimit` committed a hard hold at selection, so accept only escrows it and never
            // checks funds (a pool that won many selections would otherwise fail all but one accept).
            let ReservationPolicy::FullHold = self.config().reservation;
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
                    market: l.market,
                    request: id,
                    qty: l.qty,
                    yes_holder,
                    yes_lock,
                    no_holder,
                    no_lock,
                    settled: false,
                },
            );
            self.markets
                .get_mut(&l.market)
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

// `accept`'s per-leg `Trading` check (engine.md §8 rule 5) is defence in depth: `submit_request` keeps
// `accept_by` below every leg's `trading_close`, so no scripted flow reaches it. This forces a
// market out of `Trading` directly, which only a unit test inside the crate can do.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Action, Command, Config, Genesis, SequencedCommand};

    const OPERATOR: AccountId = AccountId(1);
    const ALICE: AccountId = AccountId(10);
    const MM: AccountId = AccountId(20);
    const DEADLINE: Timestamp = 10_000;

    fn price(bps: u16) -> Price {
        Price::new(bps).unwrap()
    }

    /// Applies `cmd` at `time` as the next seq. A user command takes the actor's next nonce.
    fn apply(
        e: &mut Engine,
        time: Timestamp,
        actor: Option<AccountId>,
        action: Option<Action>,
    ) -> Vec<Emitted> {
        let cmd = match (actor, action) {
            (Some(actor), Some(action)) => Command::User {
                actor,
                nonce: e.nonces.get(&actor).copied().unwrap_or(0) + 1,
                action,
            },
            _ => Command::Tick,
        };
        e.apply(&SequencedCommand {
            seq: e.last_seq() + 1,
            time,
            cmd,
        })
    }

    fn reason(out: &[Emitted]) -> Option<RejectReason> {
        out.iter().find_map(|e| match e.event {
            Event::CommandRejected { reason } => Some(reason),
            _ => None,
        })
    }

    /// Two markets, a two-leg request quoted on both legs and presented; returns Alice's picks.
    fn presented() -> (Engine, Vec<Pick>) {
        let mut e = Engine::new();
        e.apply(&SequencedCommand {
            seq: 1,
            time: 0,
            cmd: Command::Genesis(Genesis {
                operator: OPERATOR,
                arbiter: AccountId(2),
                treasury: AccountId(3),
                config: Config::default(),
            }),
        });
        let user = |e: &mut Engine, actor, action| {
            let out = apply(e, 0, Some(actor), Some(action));
            assert_eq!(reason(&out), None);
        };
        for to in [ALICE, MM] {
            user(
                &mut e,
                OPERATOR,
                Action::Deposit {
                    to,
                    amount: 1_000 * ONE_DOLLAR,
                },
            );
        }
        for id in [1, 2] {
            user(
                &mut e,
                OPERATOR,
                Action::ListMarket {
                    id: MarketId(id),
                    description: "m".into(),
                    oracle: AccountId(4),
                    trading_close: 1_000_000,
                    resolution_deadline: 2_000_000,
                    dispute_window: 10_000,
                },
            );
        }
        let leg = |id| LegSpec {
            market: MarketId(id),
            side: Side::BuyYes,
            qty: 100,
            limit_price: price(7_000),
        };
        let request = RequestId(e.last_seq() + 1);
        user(
            &mut e,
            ALICE,
            Action::SubmitRequest {
                legs: vec![leg(1), leg(2)],
                response_deadline: DEADLINE,
            },
        );
        for (leg, bps) in [(0, 6_200), (1, 6_000)] {
            user(
                &mut e,
                MM,
                Action::SubmitQuote {
                    request,
                    leg,
                    price: price(bps),
                    size: 100,
                    expires_at: 100_000,
                },
            );
        }
        let out = apply(&mut e, DEADLINE, None, None);
        let picks = out
            .iter()
            .find_map(|x| match &x.event {
                Event::RequestPresented { picks, .. } => Some(picks.clone()),
                _ => None,
            })
            .expect("presented");
        assert_eq!(e.check_invariants(), Ok(()));
        (e, picks)
    }

    #[test]
    fn accept_rechecks_every_leg_market_is_trading() {
        for forced in [1, 2] {
            let (mut e, picks) = presented();
            let request = *e.requests.keys().next().expect("one request");
            let balances = [ALICE, MM].map(|a| e.balance(a));
            e.markets.get_mut(&MarketId(forced)).unwrap().state = MarketState::Closed;

            let out = apply(
                &mut e,
                DEADLINE + 1,
                Some(ALICE),
                Some(Action::Accept {
                    request,
                    picks: picks.clone(),
                }),
            );

            assert_eq!(reason(&out), Some(RejectReason::InvalidState), "market {forced}");
            assert!(matches!(
                e.requests[&request].state,
                RequestState::Presented { .. }
            ));
            assert!(e.quotes.values().all(|q| q.state == QuoteState::Selected));
            assert!(e.positions.is_empty(), "nothing was escrowed");
            assert_eq!([ALICE, MM].map(|a| e.balance(a)), balances, "no balance moved");

            // Control: with the market `Trading` again the same picks fill, so the re-check was
            // the only thing that stopped the accept above.
            e.markets.get_mut(&MarketId(forced)).unwrap().state = MarketState::Trading;
            let out = apply(
                &mut e,
                DEADLINE + 2,
                Some(ALICE),
                Some(Action::Accept { request, picks }),
            );
            assert_eq!(reason(&out), None, "market {forced}");
            assert_eq!(e.requests[&request].state, RequestState::Filled);
            assert_eq!(e.positions.len(), 2);
            assert_eq!(e.check_invariants(), Ok(()));
        }
    }
}
