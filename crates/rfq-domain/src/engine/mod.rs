mod account;
mod contract;
mod rfq;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::command::{Action, Command, Config, Genesis, ReservationPolicy, SequencedCommand};
use crate::event::{Emitted, Event, RejectReason};
use crate::ledger::{Balance, Ledger};
use crate::model::*;
use crate::types::*;

/// Message for `.expect` on mutations whose preconditions were checked in validation.
pub(crate) const VALIDATED: &str = "precondition checked during validation";

pub(crate) type Out = Vec<Emitted>;
pub(crate) type Handled = Result<(), RejectReason>;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Engine {
    pub(crate) genesis: Option<Genesis>,
    pub(crate) last_seq: u64,
    pub(crate) now: Timestamp,
    pub(crate) nonces: BTreeMap<AccountId, u64>,
    pub(crate) ledger: Ledger,
    pub(crate) contracts: BTreeMap<ContractId, Contract>,
    pub(crate) requests: BTreeMap<RequestId, Request>,
    pub(crate) quotes: BTreeMap<QuoteId, Quote>,
    pub(crate) positions: BTreeMap<PositionId, Position>,
    pub(crate) next_position: u64,
    pub(crate) timers: BTreeSet<(Timestamp, TimerKey)>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }
    pub fn now(&self) -> Timestamp {
        self.now
    }
    pub fn balance(&self, a: AccountId) -> Balance {
        self.ledger.balance(a)
    }
    pub fn request(&self, id: RequestId) -> Option<&Request> {
        self.requests.get(&id)
    }
    pub fn quote(&self, id: QuoteId) -> Option<&Quote> {
        self.quotes.get(&id)
    }
    pub fn contract(&self, id: ContractId) -> Option<&Contract> {
        self.contracts.get(&id)
    }
    pub fn position(&self, id: PositionId) -> Option<&Position> {
        self.positions.get(&id)
    }
    pub fn requests(&self) -> impl Iterator<Item = &Request> {
        self.requests.values()
    }
    pub fn quotes(&self) -> impl Iterator<Item = &Quote> {
        self.quotes.values()
    }
    pub fn contracts(&self) -> impl Iterator<Item = &Contract> {
        self.contracts.values()
    }
    pub fn next_timer(&self) -> Option<Timestamp> {
        self.timers.first().map(|(t, _)| *t)
    }

    /// The accounts the last `apply` changed (its timers included), with each one's balance after
    /// it, in account order. Gateways send these with the command's events (docs/specs/gateway.md §2).
    pub fn take_touched_balances(&mut self) -> Vec<(AccountId, Balance)> {
        let touched = self.ledger.take_touched();
        touched.into_iter().map(|a| (a, self.ledger.balance(a))).collect()
    }

    /// Applies one sequenced command. Commands with `seq <= last_seq` are ignored (replay-safe).
    pub fn apply(&mut self, sc: &SequencedCommand) -> Vec<Emitted> {
        // A previous command's touches are never reported again.
        self.ledger.take_touched();
        let mut out = Vec::new();
        if sc.seq <= self.last_seq {
            return out;
        }
        self.last_seq = sc.seq;
        self.now = self.now.max(sc.time);
        if self.genesis.is_some() {
            // Resume first, so downtime can't eat a dispute window (engine.md §6.3).
            if matches!(sc.cmd, Command::Resume) {
                self.on_resume(&mut out);
            }
            self.fire_due_timers(&mut out);
        }
        match &sc.cmd {
            Command::Genesis(g) => {
                if self.genesis.is_some() {
                    out.push(Emitted::public(Event::CommandRejected {
                        reason: RejectReason::AlreadyInitialized,
                    }));
                } else {
                    self.genesis = Some(g.clone());
                    out.push(Emitted::public(Event::Initialized));
                }
            }
            Command::Tick | Command::Resume => {}
            Command::User { actor, nonce, action } => self.apply_user(*actor, *nonce, action, &mut out),
        }
        debug_assert_eq!(self.check_invariants(), Ok(()));
        out
    }

    fn apply_user(&mut self, actor: AccountId, nonce: u64, action: &Action, out: &mut Out) {
        let reject = |out: &mut Out, reason| out.push(Emitted::to(actor, Event::CommandRejected { reason }));
        if self.genesis.is_none() {
            return reject(out, RejectReason::NotInitialized);
        }
        // Nonces are strictly consecutive per actor, so `Duplicate` always means "already sequenced"
        // and a skipped nonce can never be mistaken for one. Neither rejection consumes anything.
        let last = self.nonces.get(&actor).copied().unwrap_or(0);
        if nonce <= last {
            return reject(out, RejectReason::Duplicate);
        }
        if last.checked_add(1) != Some(nonce) {
            return reject(out, RejectReason::NonceGap);
        }
        // A valid nonce is consumed even if the action is then rejected: retries stay idempotent.
        self.nonces.insert(actor, nonce);
        let result = match action {
            Action::Deposit { to, amount } => self.deposit(actor, *to, *amount, out),
            Action::Withdraw { amount } => self.withdraw(actor, *amount, out),
            Action::ListContract {
                id,
                description,
                oracle,
                trading_close,
                resolution_deadline,
                dispute_window,
            } => self.list_contract(
                actor,
                *id,
                description,
                *oracle,
                *trading_close,
                *resolution_deadline,
                *dispute_window,
                out,
            ),
            Action::SubmitRequest {
                legs,
                response_deadline,
            } => self.submit_request(actor, legs, *response_deadline, out),
            Action::CancelRequest { request } => self.cancel_request(actor, *request, out),
            Action::SubmitQuote {
                request,
                leg,
                price,
                size,
                expires_at,
            } => self.submit_quote(actor, *request, *leg, *price, *size, *expires_at, out),
            Action::CancelQuote { quote } => self.cancel_quote(actor, *quote, out),
            Action::Accept { request, picks } => self.accept(actor, *request, picks, out),
            Action::Reject { request } => self.reject(actor, *request, out),
            Action::ProposeOutcome { contract, outcome } => {
                self.propose_outcome(actor, *contract, *outcome, out)
            }
            Action::Dispute { contract } => self.dispute(actor, *contract, out),
            Action::Rule { contract, outcome } => self.rule(actor, *contract, *outcome, out),
        };
        if let Err(reason) = result {
            reject(out, reason);
        }
    }

    fn fire_due_timers(&mut self, out: &mut Out) {
        while let Some(&(at, key)) = self.timers.first() {
            if at > self.now {
                break;
            }
            self.timers.pop_first();
            // Timers are stale-safe: each handler is a no-op if its object moved on.
            match key {
                TimerKey::QuoteExpiry(q) => self.on_quote_expiry(q, out),
                TimerKey::RequestDeadline(r) => self.on_request_deadline(r, at, out),
                TimerKey::AcceptBy(r) => self.on_accept_by(r, out),
                TimerKey::ContractClose(c) => self.on_contract_close(c, out),
                TimerKey::DisputeWindow(c) => self.on_dispute_window(c, at, out),
                TimerKey::HardDeadline(c) => self.on_hard_deadline(c, out),
            }
        }
    }

    pub(crate) fn arm(&mut self, at: Timestamp, key: TimerKey) {
        self.timers.insert((at, key));
    }

    pub(crate) fn genesis(&self) -> &Genesis {
        self.genesis.as_ref().expect("initialized")
    }
    pub(crate) fn config(&self) -> Config {
        self.genesis().config
    }

    pub fn state_hash(&self) -> [u8; 32] {
        Sha256::digest(bincode::serialize(self).expect("engine state serializes")).into()
    }

    /// I1–I5 from docs/specs/engine.md §3.
    pub fn check_invariants(&self) -> Result<(), String> {
        self.ledger.check()?;

        // I3 escrow. Sums are accumulated in u128 so a corrupt state is reported, never an overflow panic.
        let mut escrow: BTreeMap<AccountId, u128> = BTreeMap::new();
        for p in self.positions.values().filter(|p| !p.settled) {
            if u128::from(p.yes_lock) + u128::from(p.no_lock) != u128::from(p.qty) * u128::from(ONE_DOLLAR) {
                return Err(format!("I3 position {:?}: locks don't sum to qty × $1", p.id));
            }
            *escrow.entry(p.yes_holder).or_default() += u128::from(p.yes_lock);
            *escrow.entry(p.no_holder).or_default() += u128::from(p.no_lock);
        }
        for (a, b) in self.ledger.accounts() {
            let e = escrow.remove(&a).unwrap_or(0);
            if u128::from(b.escrowed) != e {
                return Err(format!(
                    "I3 escrow: {a:?} escrowed {} != open position locks {e}",
                    b.escrowed
                ));
            }
        }
        if let Some(a) = escrow.keys().next() {
            return Err(format!(
                "I3 escrow: position lock for account {a:?} without balance"
            ));
        }

        // I4 ownership: every live hold has exactly one live owner, and terminal objects own none.
        // D5 tripwire: a new `ReservationPolicy` must relax "live without a hold" below (`SharedLimit`
        // quotes are live and hold nothing until selection commits them). This runs before genesis
        // too, so it reads the policy from `genesis` instead of `config()`, which would panic.
        match self.genesis.as_ref().map(|g| g.config.reservation) {
            None | Some(ReservationPolicy::FullHold) => {}
        }
        let mut owned: BTreeSet<HoldId> = BTreeSet::new();
        let mut own = |h: Option<HoldId>, live: bool, what: String| -> Result<(), String> {
            match (h, live) {
                (Some(h), true) if owned.insert(h) => Ok(()),
                (Some(_), true) => Err(format!("I4 {what}: hold owned twice")),
                (None, true) => Err(format!("I4 {what}: live without a hold")),
                (Some(_), false) => Err(format!("I4 {what}: terminal but still holds funds")),
                (None, false) => Ok(()),
            }
        };
        for r in self.requests.values() {
            own(r.hold, r.state.is_live(), format!("request {:?}", r.id))?;
        }
        for q in self.quotes.values() {
            own(q.hold, q.state.is_live(), format!("quote {:?}", q.id))?;
        }
        for c in self.contracts.values() {
            if let Some(d) = c.dispute {
                let live = matches!(c.state, ContractState::Disputed { .. });
                own(d.hold, live, format!("dispute on {:?}", c.id))?;
            }
        }
        let held: BTreeSet<HoldId> = self.ledger.hold_ids().collect();
        if held != owned {
            return Err(format!(
                "I4 ownership: ledger holds {held:?} != owned holds {owned:?}"
            ));
        }

        // I5 no dead ends: every live money-owning object has a timer that ends it.
        for r in self.requests.values() {
            let timer = match r.state {
                RequestState::Open => Some((r.response_deadline, TimerKey::RequestDeadline(r.id))),
                RequestState::Presented { accept_by } => Some((accept_by, TimerKey::AcceptBy(r.id))),
                _ => None,
            };
            if let Some(t) = timer
                && !self.timers.contains(&t)
            {
                return Err(format!(
                    "I5 request {:?} in {:?} has no exit timer",
                    r.id, r.state
                ));
            }
        }
        // A live quote's exit is its request's exit (or its own expiry timer), so it is only
        // reachable if the request is live in the matching state and lists the quote on its leg.
        for q in self.quotes.values().filter(|q| q.state.is_live()) {
            let request = self.requests.get(&q.request);
            let state_matches = matches!(
                (q.state, request.map(|r| r.state)),
                (QuoteState::Active, Some(RequestState::Open))
                    | (QuoteState::Selected, Some(RequestState::Presented { .. }))
            );
            let listed = request
                .zip(usize::try_from(q.leg).ok())
                .and_then(|(r, i)| r.legs.get(i))
                .is_some_and(|l| l.quotes.contains(&q.id));
            if !state_matches || !listed {
                return Err(format!(
                    "I5 quote {:?} in {:?} is not reachable from a live request (request {:?}: {:?}, listed on leg {}: {listed})",
                    q.id,
                    q.state,
                    q.request,
                    request.map(|r| r.state),
                    q.leg
                ));
            }
        }
        for c in self.contracts.values() {
            let is_final = matches!(c.state, ContractState::Final(_));
            if !is_final
                && !self
                    .timers
                    .contains(&(c.resolution_deadline, TimerKey::HardDeadline(c.id)))
            {
                return Err(format!("I5 contract {:?} has no hard-deadline timer", c.id));
            }
        }
        Ok(())
    }
}

// Added beyond the brief: nothing else exercises the I3-I5 checks until Task 4, so mutate state
// directly and assert each check fires.
#[cfg(test)]
mod tests {
    use super::*;

    const A: AccountId = AccountId(1);
    const B: AccountId = AccountId(2);

    fn engine() -> Engine {
        let mut e = Engine::new();
        let genesis = Genesis {
            operator: A,
            arbiter: AccountId(3),
            treasury: AccountId(4),
            config: Config::default(),
        };
        e.apply(&SequencedCommand {
            seq: 1,
            time: 0,
            cmd: Command::Genesis(genesis),
        });
        assert_eq!(e.check_invariants(), Ok(()));
        e
    }

    fn position(yes_lock: Amount, no_lock: Amount) -> Position {
        Position {
            id: PositionId(1),
            contract: ContractId(1),
            request: RequestId(1),
            qty: 1,
            yes_holder: A,
            yes_lock,
            no_holder: B,
            no_lock,
            settled: false,
        }
    }

    fn request(hold: Option<HoldId>) -> Request {
        Request {
            id: RequestId(7),
            requester: A,
            legs: Vec::new(),
            response_deadline: 100,
            hold,
            state: RequestState::Open,
        }
    }

    fn err(e: &Engine) -> String {
        e.check_invariants().unwrap_err()
    }

    #[test]
    fn i3_rejects_locks_that_do_not_sum_to_qty_dollars() {
        let mut e = engine();
        e.positions.insert(PositionId(1), position(1, 1));
        assert!(err(&e).starts_with("I3 position"));
    }

    #[test]
    fn i3_rejects_open_position_without_matching_escrow() {
        let mut e = engine();
        e.positions.insert(PositionId(1), position(600_000, 400_000));
        assert!(err(&e).starts_with("I3 escrow"));
    }

    #[test]
    fn i4_rejects_live_request_without_hold() {
        let mut e = engine();
        e.requests.insert(RequestId(7), request(None));
        e.timers.insert((100, TimerKey::RequestDeadline(RequestId(7))));
        assert!(err(&e).starts_with("I4 request"));
    }

    #[test]
    fn i4_rejects_ledger_hold_with_no_owner() {
        let mut e = engine();
        e.ledger.deposit(A, 10).unwrap();
        e.ledger.hold(A, 10).unwrap();
        assert!(err(&e).starts_with("I4 ownership"));
    }

    #[test]
    fn i5_rejects_live_request_without_exit_timer() {
        let mut e = engine();
        e.ledger.deposit(A, 10).unwrap();
        let hold = e.ledger.hold(A, 10).unwrap();
        e.requests.insert(RequestId(7), request(Some(hold)));
        assert!(err(&e).starts_with("I5 request"));
        e.timers.insert((100, TimerKey::RequestDeadline(RequestId(7))));
        assert_eq!(e.check_invariants(), Ok(()));
    }

    #[test]
    fn i5_rejects_open_contract_without_hard_deadline_timer() {
        let mut e = engine();
        let contract = Contract {
            id: ContractId(1),
            description: String::new(),
            oracle: A,
            trading_close: 10,
            resolution_deadline: 20,
            dispute_window: 5,
            state: ContractState::Trading,
            dispute: None,
            positions: Vec::new(),
        };
        e.contracts.insert(ContractId(1), contract);
        assert!(err(&e).starts_with("I5 contract"));
        e.timers.insert((20, TimerKey::HardDeadline(ContractId(1))));
        assert_eq!(e.check_invariants(), Ok(()));
    }

    fn leg(quotes: &[QuoteId]) -> Leg {
        Leg {
            contract: ContractId(1),
            side: Side::BuyYes,
            qty: 1,
            limit_price: Price::new(5_000).unwrap(),
            quotes: quotes.iter().copied().collect(),
            selected: None,
        }
    }

    fn quote(hold: Option<HoldId>, state: QuoteState) -> Quote {
        Quote {
            id: QuoteId(8),
            mm: B,
            request: RequestId(7),
            leg: 0,
            price: Price::new(5_000).unwrap(),
            size: 1,
            expires_at: 100,
            hold,
            state,
        }
    }

    /// A consistent state: an `Open` request with one `Active` quote, both holding funds.
    fn engine_with_live_quote() -> Engine {
        let mut e = engine();
        e.ledger.deposit(A, 10).unwrap();
        e.ledger.deposit(B, 10).unwrap();
        let request_hold = e.ledger.hold(A, 10).unwrap();
        let quote_hold = e.ledger.hold(B, 10).unwrap();
        let mut r = request(Some(request_hold));
        r.legs = vec![leg(&[QuoteId(8)])];
        e.requests.insert(RequestId(7), r);
        e.quotes
            .insert(QuoteId(8), quote(Some(quote_hold), QuoteState::Active));
        e.timers.insert((100, TimerKey::RequestDeadline(RequestId(7))));
        assert_eq!(e.check_invariants(), Ok(()));
        e
    }

    #[test]
    fn i5_accepts_active_quote_on_open_request_and_selected_on_presented() {
        let mut e = engine_with_live_quote();
        e.requests.get_mut(&RequestId(7)).unwrap().state = RequestState::Presented { accept_by: 100 };
        e.quotes.get_mut(&QuoteId(8)).unwrap().state = QuoteState::Selected;
        e.timers.insert((100, TimerKey::AcceptBy(RequestId(7))));
        assert_eq!(e.check_invariants(), Ok(()));
    }

    /// `engine_with_live_quote` moved on to selection: `Presented` until `accept_by`, quote `Selected`.
    fn engine_with_selected_quote(expires_at: Timestamp) -> Engine {
        let mut e = engine_with_live_quote();
        let r = e.requests.get_mut(&RequestId(7)).unwrap();
        r.state = RequestState::Presented { accept_by: 200 };
        r.legs[0].selected = Some(QuoteId(8));
        let q = e.quotes.get_mut(&QuoteId(8)).unwrap();
        q.state = QuoteState::Selected;
        q.expires_at = expires_at;
        e.timers.insert((200, TimerKey::AcceptBy(RequestId(7))));
        assert_eq!(e.check_invariants(), Ok(()));
        e
    }

    // Unreachable through commands (submit clamps expiry to `accept_by`), so the state is built directly.
    #[test]
    fn selected_quote_expiring_before_accept_by_fails_the_request() {
        let mut e = engine_with_selected_quote(100);
        let mut out = Vec::new();
        e.on_quote_expiry(QuoteId(8), &mut out);
        let names: Vec<_> = out.iter().map(|e| e.event.name()).collect();
        assert_eq!(names, vec!["QuoteExpired", "RequestRejected"]);
        assert_eq!(
            e.requests[&RequestId(7)].state,
            RequestState::Rejected(RejectCause::QuoteExpired)
        );
        assert_eq!(e.quotes[&QuoteId(8)].state, QuoteState::Expired);
        assert_eq!(e.check_invariants(), Ok(()));
        for a in [A, B] {
            assert_eq!(e.balance(a).free, 10, "both holds released");
        }
    }

    #[test]
    fn selected_quote_expiring_at_accept_by_is_left_to_the_lapse() {
        let mut e = engine_with_selected_quote(200);
        let before = e.state_hash();
        let mut out = Vec::new();
        e.on_quote_expiry(QuoteId(8), &mut out);
        assert_eq!(out, Vec::new());
        assert_eq!(e.state_hash(), before);
    }

    #[test]
    fn i5_rejects_live_quote_on_a_terminal_request() {
        // The request ended (no hold, no timer needed) but its quote still holds funds.
        let mut e = engine();
        e.ledger.deposit(B, 10).unwrap();
        let quote_hold = e.ledger.hold(B, 10).unwrap();
        let mut r = request(None);
        r.state = RequestState::Cancelled;
        r.legs = vec![leg(&[QuoteId(8)])];
        e.requests.insert(RequestId(7), r);
        e.quotes
            .insert(QuoteId(8), quote(Some(quote_hold), QuoteState::Active));
        assert!(err(&e).starts_with("I5 quote"), "{}", err(&e));
    }

    #[test]
    fn i5_rejects_live_quote_whose_state_does_not_match_its_request() {
        let mut active_on_presented = engine_with_live_quote();
        active_on_presented.requests.get_mut(&RequestId(7)).unwrap().state =
            RequestState::Presented { accept_by: 100 };
        active_on_presented
            .timers
            .insert((100, TimerKey::AcceptBy(RequestId(7))));
        assert!(err(&active_on_presented).starts_with("I5 quote"));

        let mut selected_on_open = engine_with_live_quote();
        selected_on_open.quotes.get_mut(&QuoteId(8)).unwrap().state = QuoteState::Selected;
        assert!(err(&selected_on_open).starts_with("I5 quote"));
    }

    #[test]
    fn i5_rejects_live_quote_missing_from_its_leg() {
        let mut e = engine_with_live_quote();
        e.requests.get_mut(&RequestId(7)).unwrap().legs[0].quotes.clear();
        assert!(err(&e).starts_with("I5 quote"));
    }

    #[test]
    fn i5_rejects_live_quote_with_unknown_request_or_leg() {
        let mut unknown_request = engine_with_live_quote();
        unknown_request.quotes.get_mut(&QuoteId(8)).unwrap().request = RequestId(99);
        assert!(err(&unknown_request).starts_with("I5 quote"));

        let mut unknown_leg = engine_with_live_quote();
        unknown_leg.quotes.get_mut(&QuoteId(8)).unwrap().leg = 5;
        assert!(err(&unknown_leg).starts_with("I5 quote"));
    }

    #[test]
    fn resume_without_open_dispute_windows_is_a_tick() {
        let mut base = engine();
        base.timers.insert((10, TimerKey::RequestDeadline(RequestId(7))));
        let mut ticked = base.clone();
        let mut resumed = base.clone();
        let at = |cmd| SequencedCommand {
            seq: 2,
            time: 10,
            cmd,
        };
        assert_eq!(ticked.apply(&at(Command::Tick)), Vec::new());
        assert_eq!(resumed.apply(&at(Command::Resume)), Vec::new());
        // Same effect: the due timer fired (and, being stale, did nothing), seq and time advanced.
        assert_eq!(resumed.next_timer(), None);
        assert_eq!(resumed.last_seq(), 2);
        assert_eq!(resumed.now(), 10);
        assert_eq!(ticked.state_hash(), resumed.state_hash());
    }

    fn closed_contract(resolution_deadline: Timestamp) -> Contract {
        Contract {
            id: ContractId(1),
            description: String::new(),
            oracle: A,
            trading_close: 10,
            resolution_deadline,
            dispute_window: 5,
            state: ContractState::Closed,
            dispute: None,
            positions: Vec::new(),
        }
    }

    // Unreachable through commands (`propose_outcome` and `on_resume` keep the window strictly before
    // the hard deadline, so the dispute timer always fires first), so the state is built directly.
    // Pins the defensive branch: even then, a timeout never turns a proposal into Invalid.
    #[test]
    fn hard_deadline_on_a_proposed_contract_keeps_the_proposal() {
        let mut e = engine();
        let mut c = closed_contract(20);
        c.state = ContractState::Proposed {
            outcome: Outcome::Yes,
            dispute_until: 20,
        };
        e.contracts.insert(ContractId(1), c);
        let mut out = Vec::new();
        e.on_hard_deadline(ContractId(1), &mut out);
        assert_eq!(
            out,
            vec![Emitted::public(Event::ContractFinalized {
                contract: ContractId(1),
                outcome: Outcome::Yes,
            })]
        );
        assert_eq!(
            e.contracts[&ContractId(1)].state,
            ContractState::Final(Outcome::Yes)
        );
    }

    #[test]
    fn stale_dispute_timer_does_not_finalize_an_extended_window() {
        let mut e = engine();
        let mut c = closed_contract(100);
        c.state = ContractState::Proposed {
            outcome: Outcome::No,
            dispute_until: 50,
        };
        e.contracts.insert(ContractId(1), c);
        let mut out = Vec::new();
        e.on_dispute_window(ContractId(1), 15, &mut out); // armed for the window that ended at 15
        assert_eq!(out, Vec::new());
        assert_eq!(
            e.contracts[&ContractId(1)].state,
            ContractState::Proposed {
                outcome: Outcome::No,
                dispute_until: 50
            }
        );
        e.on_dispute_window(ContractId(1), 50, &mut out);
        assert_eq!(names_of(&out), vec!["ContractFinalized"]);
    }

    // Through `apply` the window's own timer finalizes the contract before a dispute at its end is
    // handled, so this guard is defence in depth; call the handler directly to pin the boundary.
    #[test]
    fn dispute_at_the_window_end_is_too_late() {
        let mut e = engine();
        let mut c = closed_contract(100);
        c.state = ContractState::Proposed {
            outcome: Outcome::No,
            dispute_until: 50,
        };
        e.contracts.insert(ContractId(1), c);
        e.now = 50;
        let mut out = Vec::new();
        assert_eq!(
            e.dispute(B, ContractId(1), &mut out),
            Err(RejectReason::InvalidState)
        );
        assert_eq!(out, Vec::new());
    }

    fn names_of(out: &[Emitted]) -> Vec<&'static str> {
        out.iter().map(|e| e.event.name()).collect()
    }
}
