mod account;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::command::{Action, Command, Config, Genesis, SequencedCommand};
use crate::event::{Emitted, Event, RejectReason};
use crate::ledger::{Balance, Ledger};
use crate::model::*;
use crate::types::*;

/// Message for `.expect` on mutations whose preconditions were checked in validation.
#[expect(dead_code, reason = "first used by the request and quote handlers in Task 4")]
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

    /// Applies one sequenced command. Commands with `seq <= last_seq` are ignored (replay-safe).
    pub fn apply(&mut self, sc: &SequencedCommand) -> Vec<Emitted> {
        let mut out = Vec::new();
        if sc.seq <= self.last_seq {
            return out;
        }
        self.last_seq = sc.seq;
        self.now = self.now.max(sc.time);
        if self.genesis.is_some() {
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
            _ => Err(RejectReason::Unsupported),
        };
        if let Err(reason) = result {
            reject(out, reason);
        }
    }

    // Placeholder dispatch: Tasks 4-6 add the real arms and make this match exhaustive. Each task
    // that touches it must drop these `expect`s; an unfulfilled expectation fails clippy.
    #[expect(clippy::match_single_binding, reason = "timer arms are added in Tasks 4-6")]
    #[expect(
        unused_variables,
        reason = "`out` is first written by the timer handlers in Task 4"
    )]
    fn fire_due_timers(&mut self, out: &mut Out) {
        while let Some(&(at, key)) = self.timers.first() {
            if at > self.now {
                break;
            }
            self.timers.pop_first();
            // Timers are stale-safe: each handler is a no-op if its object moved on.
            match key {
                _ => {}
            }
        }
    }

    #[expect(
        dead_code,
        reason = "first used when contracts and requests arm timers in Task 4"
    )]
    pub(crate) fn arm(&mut self, at: Timestamp, key: TimerKey) {
        self.timers.insert((at, key));
    }

    pub(crate) fn genesis(&self) -> &Genesis {
        self.genesis.as_ref().expect("initialized")
    }
    #[expect(dead_code, reason = "first used by request validation in Task 4")]
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
    fn resume_behaves_like_tick_for_now() {
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
}
