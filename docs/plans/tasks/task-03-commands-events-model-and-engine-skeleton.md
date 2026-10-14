# Task 3: Commands, events, model and engine skeleton

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Files:**
- Create: `crates/rfq-core/src/command.rs`, `src/event.rs`, `src/model.rs`, `src/engine/mod.rs`, `src/engine/account.rs`, `src/testkit.rs`, `tests/engine_basics.rs`
- Modify: `crates/rfq-core/src/lib.rs`

**Interfaces:**
- Consumes: Tasks 1–2.
- Produces:
  - `Config` (fields per Global Constraints, with `Default`), `Genesis { operator, arbiter, treasury, config }`, `LegSpec { contract, side, qty, limit_price }`, `Pick { leg: u32, quote: QuoteId, price: Price }`, `Action` (12 variants below), `Command::{Genesis(Genesis), Tick, User { actor, nonce, action }}`, `SequencedCommand { seq, time, cmd }`.
  - `Audience::{Public, Account(AccountId)}`, `RejectReason` (12 variants below), `Event` (all variants below), `Event::name() -> &'static str`, `Emitted { audience, event }` with `Emitted::public(e)` and `Emitted::to(a, e)`.
  - Model types (listed in `model.rs` below) and `TimerKey`.
  - `Engine::new()`, `apply(&SequencedCommand) -> Vec<Emitted>`, `balance(AccountId) -> Balance`, `request(RequestId) -> Option<&Request>`, `quote(QuoteId) -> Option<&Quote>`, `contract(ContractId) -> Option<&Contract>`, `position(PositionId) -> Option<&Position>`, `requests()`, `quotes()`, `contracts()` iterators, `next_timer() -> Option<Timestamp>`, `last_seq()`, `state_hash() -> [u8; 32]`, `check_invariants() -> Result<(), String>`.
  - `testkit::{Harness, accounts::*, rejection, names, dollars}`.

- [ ] **Step 1: Write the command, event and model types**

`crates/rfq-core/src/command.rs`:
```rust
use serde::{Deserialize, Serialize};

use crate::types::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub accept_window: Duration,
    pub min_response: Duration,
    pub max_response: Duration,
    pub max_qty: Qty,
    pub dispute_bond: Amount,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            accept_window: 30_000,
            min_response: 1_000,
            max_response: 7 * 24 * 3_600_000,
            max_qty: 1_000_000_000,
            dispute_bond: 100 * ONE_DOLLAR,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Genesis {
    pub operator: AccountId,
    pub arbiter: AccountId,
    pub treasury: AccountId,
    pub config: Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegSpec {
    pub contract: ContractId,
    pub side: Side,
    pub qty: Qty,
    pub limit_price: Price,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pick {
    pub leg: u32,
    pub quote: QuoteId,
    pub price: Price,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Deposit { to: AccountId, amount: Amount },
    Withdraw { amount: Amount },
    ListContract {
        id: ContractId,
        description: String,
        oracle: AccountId,
        trading_close: Timestamp,
        resolution_deadline: Timestamp,
        dispute_window: Duration,
    },
    SubmitRequest { legs: Vec<LegSpec>, response_deadline: Timestamp },
    CancelRequest { request: RequestId },
    SubmitQuote { request: RequestId, leg: u32, price: Price, size: Qty, expires_at: Timestamp },
    CancelQuote { quote: QuoteId },
    Accept { request: RequestId, picks: Vec<Pick> },
    Reject { request: RequestId },
    ProposeOutcome { contract: ContractId, outcome: Outcome },
    Dispute { contract: ContractId },
    Rule { contract: ContractId, outcome: Outcome },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Genesis(Genesis),
    Tick,
    User { actor: AccountId, nonce: u64, action: Action },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequencedCommand {
    pub seq: u64,
    pub time: Timestamp,
    pub cmd: Command,
}
```

`crates/rfq-core/src/model.rs`:
```rust
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::types::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContractState {
    Trading,
    Closed,
    Proposed { outcome: Outcome, dispute_until: Timestamp },
    Disputed { proposed: Outcome },
    Final(Outcome),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispute {
    pub disputer: AccountId,
    /// `Some` while the bond is held; `None` once returned or forfeited.
    pub hold: Option<HoldId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contract {
    pub id: ContractId,
    pub description: String,
    pub oracle: AccountId,
    pub trading_close: Timestamp,
    pub resolution_deadline: Timestamp,
    pub dispute_window: Duration,
    pub state: ContractState,
    pub dispute: Option<Dispute>,
    pub positions: Vec<PositionId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectCause { Declined, Lapsed, QuoteWithdrawn, QuoteExpired }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestState {
    Open,
    Presented { accept_by: Timestamp },
    Filled,
    Cancelled,
    Expired,
    Rejected(RejectCause),
}

impl RequestState {
    pub fn is_live(&self) -> bool {
        matches!(self, RequestState::Open | RequestState::Presented { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leg {
    pub contract: ContractId,
    pub side: Side,
    pub qty: Qty,
    pub limit_price: Price,
    /// Live quotes (Active or Selected) on this leg.
    pub quotes: BTreeSet<QuoteId>,
    pub selected: Option<QuoteId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub requester: AccountId,
    pub legs: Vec<Leg>,
    pub response_deadline: Timestamp,
    /// `Some` while live; consumed on accept, released on any other exit.
    pub hold: Option<HoldId>,
    pub state: RequestState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuoteState { Active, Selected, Filled, Replaced, Cancelled, Expired, Lost, Released }

impl QuoteState {
    pub fn is_live(&self) -> bool {
        matches!(self, QuoteState::Active | QuoteState::Selected)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub id: QuoteId,
    pub mm: AccountId,
    pub request: RequestId,
    pub leg: u32,
    pub price: Price,
    pub size: Qty,
    pub expires_at: Timestamp,
    pub hold: Option<HoldId>,
    pub state: QuoteState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub id: PositionId,
    pub contract: ContractId,
    pub request: RequestId,
    pub qty: Qty,
    pub yes_holder: AccountId,
    pub yes_lock: Amount,
    pub no_holder: AccountId,
    pub no_lock: Amount,
    pub settled: bool,
}

/// Variant order is the firing order for timers that share a timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TimerKey {
    QuoteExpiry(QuoteId),
    RequestDeadline(RequestId),
    AcceptBy(RequestId),
    ContractClose(ContractId),
    DisputeWindow(ContractId),
    HardDeadline(ContractId),
}
```

`crates/rfq-core/src/event.rs`:
```rust
use serde::{Deserialize, Serialize};

use crate::command::Pick;
use crate::model::RejectCause;
use crate::types::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Audience { Public, Account(AccountId) }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    NotInitialized,
    AlreadyInitialized,
    Duplicate,
    Unauthorized,
    UnknownObject,
    InvalidState,
    InvalidInput,
    Unsupported,
    SelfTrade,
    InsufficientFunds,
    Overflow,
    Mismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Initialized,
    CommandRejected { reason: RejectReason },
    Deposited { account: AccountId, amount: Amount },
    Withdrawn { account: AccountId, amount: Amount },
    ContractListed { contract: ContractId },
    ContractClosed { contract: ContractId },
    RequestOpened { request: RequestId, leg: u32, contract: ContractId, side: Side, qty: Qty, response_deadline: Timestamp },
    RequestCancelled { request: RequestId },
    RequestExpired { request: RequestId },
    RequestPresented { request: RequestId, picks: Vec<Pick>, accept_by: Timestamp },
    RequestRejected { request: RequestId, cause: RejectCause },
    RequestFilled { request: RequestId, positions: Vec<PositionId> },
    QuoteSubmitted { quote: QuoteId, request: RequestId, price: Price, expires_at: Timestamp },
    QuoteReplaced { old: QuoteId, new: QuoteId },
    QuoteCancelled { quote: QuoteId },
    QuoteExpired { quote: QuoteId },
    QuoteLost { quote: QuoteId },
    QuoteSelected { quote: QuoteId },
    QuoteReleased { quote: QuoteId },
    QuoteFilled { quote: QuoteId, position: PositionId },
    OutcomeProposed { contract: ContractId, outcome: Outcome, dispute_until: Timestamp },
    Disputed { contract: ContractId, disputer: AccountId },
    Ruled { contract: ContractId, outcome: Outcome, bond_returned: bool },
    ContractFinalized { contract: ContractId, outcome: Outcome },
    PositionSettled { position: PositionId, yes_holder: AccountId, yes_payout: Amount, no_holder: AccountId, no_payout: Amount },
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Initialized => "Initialized",
            Event::CommandRejected { .. } => "CommandRejected",
            Event::Deposited { .. } => "Deposited",
            Event::Withdrawn { .. } => "Withdrawn",
            Event::ContractListed { .. } => "ContractListed",
            Event::ContractClosed { .. } => "ContractClosed",
            Event::RequestOpened { .. } => "RequestOpened",
            Event::RequestCancelled { .. } => "RequestCancelled",
            Event::RequestExpired { .. } => "RequestExpired",
            Event::RequestPresented { .. } => "RequestPresented",
            Event::RequestRejected { .. } => "RequestRejected",
            Event::RequestFilled { .. } => "RequestFilled",
            Event::QuoteSubmitted { .. } => "QuoteSubmitted",
            Event::QuoteReplaced { .. } => "QuoteReplaced",
            Event::QuoteCancelled { .. } => "QuoteCancelled",
            Event::QuoteExpired { .. } => "QuoteExpired",
            Event::QuoteLost { .. } => "QuoteLost",
            Event::QuoteSelected { .. } => "QuoteSelected",
            Event::QuoteReleased { .. } => "QuoteReleased",
            Event::QuoteFilled { .. } => "QuoteFilled",
            Event::OutcomeProposed { .. } => "OutcomeProposed",
            Event::Disputed { .. } => "Disputed",
            Event::Ruled { .. } => "Ruled",
            Event::ContractFinalized { .. } => "ContractFinalized",
            Event::PositionSettled { .. } => "PositionSettled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Emitted {
    pub audience: Audience,
    pub event: Event,
}

impl Emitted {
    pub fn public(event: Event) -> Self { Emitted { audience: Audience::Public, event } }
    pub fn to(account: AccountId, event: Event) -> Self { Emitted { audience: Audience::Account(account), event } }
}
```

- [ ] **Step 2: Write the test harness**

`crates/rfq-core/src/testkit.rs`:
```rust
//! Drives an Engine the way the sequencer would. Used by tests, scenarios and the demo.
use std::collections::BTreeMap;

use crate::command::*;
use crate::engine::Engine;
use crate::event::{Emitted, Event, RejectReason};
use crate::types::*;

pub mod accounts {
    use crate::types::AccountId;
    pub const OPERATOR: AccountId = AccountId(1);
    pub const ARBITER: AccountId = AccountId(2);
    pub const TREASURY: AccountId = AccountId(3);
    pub const ORACLE: AccountId = AccountId(4);
    pub const ALICE: AccountId = AccountId(10);
    pub const MM1: AccountId = AccountId(20);
    pub const MM2: AccountId = AccountId(21);
    pub const CAROL: AccountId = AccountId(30);
}
use accounts::*;

pub fn genesis(config: Config) -> Genesis {
    Genesis { operator: OPERATOR, arbiter: ARBITER, treasury: TREASURY, config }
}

pub fn dollars(x: u64) -> Amount { x * ONE_DOLLAR }

pub fn price(bps: u16) -> Price { Price::new(bps).expect("valid test price") }

pub fn rejection(events: &[Emitted]) -> Option<RejectReason> {
    events.iter().find_map(|e| match e.event {
        Event::CommandRejected { reason } => Some(reason),
        _ => None,
    })
}

pub fn names(events: &[Emitted]) -> Vec<&'static str> {
    events.iter().map(|e| e.event.name()).collect()
}

pub struct Harness {
    pub engine: Engine,
    pub log: Vec<SequencedCommand>,
    seq: u64,
    time: Timestamp,
    nonces: BTreeMap<AccountId, u64>,
}

impl Harness {
    /// Fresh engine with `Genesis` applied at seq 1, time 0.
    pub fn new(config: Config) -> Self {
        let mut h = Harness { engine: Engine::new(), log: Vec::new(), seq: 0, time: 0, nonces: BTreeMap::new() };
        let out = h.submit(Command::Genesis(genesis(config)));
        assert_eq!(names(&out), vec!["Initialized"]);
        h
    }

    pub fn now(&self) -> Timestamp { self.time }

    /// The seq (and therefore the RequestId/QuoteId) the next command will get.
    pub fn next_seq(&self) -> u64 { self.seq + 1 }

    pub fn set_time(&mut self, t: Timestamp) -> &mut Self {
        assert!(t >= self.time, "time must not go backwards");
        self.time = t;
        self
    }

    pub fn submit(&mut self, cmd: Command) -> Vec<Emitted> {
        self.seq += 1;
        let sc = SequencedCommand { seq: self.seq, time: self.time, cmd };
        let out = self.engine.apply(&sc);
        if let Err(e) = self.engine.check_invariants() {
            panic!("invariant violated after seq {}: {e}", self.seq);
        }
        self.log.push(sc);
        out
    }

    pub fn user(&mut self, actor: AccountId, action: Action) -> Vec<Emitted> {
        let n = self.nonces.entry(actor).or_insert(0);
        *n += 1;
        let nonce = *n;
        self.submit(Command::User { actor, nonce, action })
    }

    pub fn tick(&mut self, t: Timestamp) -> Vec<Emitted> {
        self.set_time(t);
        self.submit(Command::Tick)
    }

    pub fn fund(&mut self, to: AccountId, usd: u64) {
        let out = self.user(OPERATOR, Action::Deposit { to, amount: dollars(usd) });
        assert_eq!(rejection(&out), None);
    }
}
```

- [ ] **Step 3: Write the failing engine tests**

`crates/rfq-core/tests/engine_basics.rs`:
```rust
use rfq_core::testkit::{accounts::*, *};
use rfq_core::*;

#[test]
fn genesis_initializes_once() {
    let mut h = Harness::new(Config::default());
    let out = h.submit(Command::Genesis(genesis(Config::default())));
    assert_eq!(rejection(&out), Some(RejectReason::AlreadyInitialized));
}

#[test]
fn commands_before_genesis_are_rejected() {
    let mut e = Engine::new();
    let out = e.apply(&SequencedCommand {
        seq: 1,
        time: 0,
        cmd: Command::User { actor: ALICE, nonce: 1, action: Action::Withdraw { amount: 1 } },
    });
    assert_eq!(rejection(&out), Some(RejectReason::NotInitialized));
}

#[test]
fn deposit_requires_operator() {
    let mut h = Harness::new(Config::default());
    let out = h.user(ALICE, Action::Deposit { to: ALICE, amount: 1 });
    assert_eq!(rejection(&out), Some(RejectReason::Unauthorized));
    assert_eq!(h.engine.balance(ALICE), Balance::default());
}

#[test]
fn deposit_and_withdraw() {
    let mut h = Harness::new(Config::default());
    h.fund(ALICE, 100);
    assert_eq!(rejection(&h.user(ALICE, Action::Withdraw { amount: dollars(40) })), None);
    assert_eq!(h.engine.balance(ALICE).free, dollars(60));
    let out = h.user(ALICE, Action::Withdraw { amount: dollars(61) });
    assert_eq!(rejection(&out), Some(RejectReason::InsufficientFunds));
}

#[test]
fn deposit_overflow_rejected() {
    let mut h = Harness::new(Config::default());
    assert_eq!(rejection(&h.user(OPERATOR, Action::Deposit { to: ALICE, amount: u64::MAX })), None);
    let before = h.engine.state_hash();
    let out = h.user(OPERATOR, Action::Deposit { to: MM1, amount: 1 });
    assert_eq!(rejection(&out), Some(RejectReason::Overflow));
    assert_eq!(h.engine.balance(MM1), Balance::default());
    // Only the operator's nonce changed.
    assert_ne!(before, h.engine.state_hash());
}

#[test]
fn duplicate_nonce_ignored() {
    let mut h = Harness::new(Config::default());
    let deposit = Command::User { actor: OPERATOR, nonce: 7, action: Action::Deposit { to: ALICE, amount: 5 } };
    assert_eq!(rejection(&h.submit(deposit.clone())), None);
    let out = h.submit(deposit);
    assert_eq!(rejection(&out), Some(RejectReason::Duplicate));
    assert_eq!(h.engine.balance(ALICE).free, 5);
}

#[test]
fn already_applied_seq_is_ignored() {
    let mut h = Harness::new(Config::default());
    h.fund(ALICE, 1);
    let last = h.log.last().unwrap().clone();
    let before = h.engine.state_hash();
    assert!(h.engine.apply(&last).is_empty());
    assert_eq!(before, h.engine.state_hash());
}

#[test]
fn identical_logs_give_identical_hashes() {
    let mut a = Harness::new(Config::default());
    a.fund(ALICE, 10);
    let mut b = Engine::new();
    for sc in &a.log {
        b.apply(sc);
    }
    assert_eq!(a.engine.state_hash(), b.state_hash());
}

#[test]
fn time_never_goes_backwards() {
    let mut e = Engine::new();
    e.apply(&SequencedCommand { seq: 1, time: 100, cmd: Command::Genesis(genesis(Config::default())) });
    e.apply(&SequencedCommand { seq: 2, time: 50, cmd: Command::Tick });
    assert_eq!(e.now(), 100);
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Update `crates/rfq-core/src/lib.rs`:
```rust
//! Deterministic RFQ engine: no I/O, no clock, no randomness.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod command;
pub mod engine;
pub mod event;
pub mod ledger;
pub mod model;
pub mod testkit;
pub mod types;

pub use command::*;
pub use engine::Engine;
pub use event::*;
pub use ledger::*;
pub use model::*;
pub use types::*;
```
Run: `cargo test -p rfq-core --test engine_basics`
Expected: FAIL to compile (`engine` module missing).

- [ ] **Step 5: Implement the engine skeleton**

`crates/rfq-core/src/engine/mod.rs`:
```rust
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
    pub fn new() -> Self { Self::default() }

    pub fn last_seq(&self) -> u64 { self.last_seq }
    pub fn now(&self) -> Timestamp { self.now }
    pub fn balance(&self, a: AccountId) -> Balance { self.ledger.balance(a) }
    pub fn request(&self, id: RequestId) -> Option<&Request> { self.requests.get(&id) }
    pub fn quote(&self, id: QuoteId) -> Option<&Quote> { self.quotes.get(&id) }
    pub fn contract(&self, id: ContractId) -> Option<&Contract> { self.contracts.get(&id) }
    pub fn position(&self, id: PositionId) -> Option<&Position> { self.positions.get(&id) }
    pub fn requests(&self) -> impl Iterator<Item = &Request> { self.requests.values() }
    pub fn quotes(&self) -> impl Iterator<Item = &Quote> { self.quotes.values() }
    pub fn contracts(&self) -> impl Iterator<Item = &Contract> { self.contracts.values() }
    pub fn next_timer(&self) -> Option<Timestamp> { self.timers.first().map(|(t, _)| *t) }

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
                    out.push(Emitted::public(Event::CommandRejected { reason: RejectReason::AlreadyInitialized }));
                } else {
                    self.genesis = Some(g.clone());
                    out.push(Emitted::public(Event::Initialized));
                }
            }
            Command::Tick => {}
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
        if nonce <= self.nonces.get(&actor).copied().unwrap_or(0) {
            return reject(out, RejectReason::Duplicate);
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

    fn fire_due_timers(&mut self, out: &mut Out) {
        while let Some(&(at, key)) = self.timers.first() {
            if at > self.now {
                break;
            }
            self.timers.pop_first();
            // Timers are stale-safe: each handler is a no-op if its object moved on.
            match key {
                _ => { let _ = (at, out.len()); }
            }
        }
    }

    pub(crate) fn arm(&mut self, at: Timestamp, key: TimerKey) {
        self.timers.insert((at, key));
    }

    pub(crate) fn genesis(&self) -> &Genesis { self.genesis.as_ref().expect("initialized") }
    pub(crate) fn config(&self) -> Config { self.genesis().config }

    pub fn state_hash(&self) -> [u8; 32] {
        Sha256::digest(bincode::serialize(self).expect("engine state serializes")).into()
    }

    /// I1–I5 from docs/specs/engine.md §3.
    pub fn check_invariants(&self) -> Result<(), String> {
        self.ledger.check()?;

        // I3 escrow.
        let mut escrow: BTreeMap<AccountId, Amount> = BTreeMap::new();
        for p in self.positions.values().filter(|p| !p.settled) {
            if p.yes_lock as u128 + p.no_lock as u128 != p.qty as u128 * ONE_DOLLAR as u128 {
                return Err(format!("I3 position {:?}: locks don't sum to qty × $1", p.id));
            }
            *escrow.entry(p.yes_holder).or_default() += p.yes_lock;
            *escrow.entry(p.no_holder).or_default() += p.no_lock;
        }
        for (a, b) in self.ledger.accounts() {
            let e = escrow.remove(&a).unwrap_or(0);
            if b.escrowed != e {
                return Err(format!("I3 escrow: {a:?} escrowed {} != open position locks {e}", b.escrowed));
            }
        }
        if let Some(a) = escrow.keys().next() {
            return Err(format!("I3 escrow: position lock for account {a:?} without balance"));
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
            return Err(format!("I4 ownership: ledger holds {held:?} != owned holds {owned:?}"));
        }

        // I5 no dead ends: every live money-owning object has a timer that ends it.
        for r in self.requests.values() {
            let timer = match r.state {
                RequestState::Open => Some((r.response_deadline, TimerKey::RequestDeadline(r.id))),
                RequestState::Presented { accept_by } => Some((accept_by, TimerKey::AcceptBy(r.id))),
                _ => None,
            };
            if let Some(t) = timer {
                if !self.timers.contains(&t) {
                    return Err(format!("I5 request {:?} in {:?} has no exit timer", r.id, r.state));
                }
            }
        }
        for c in self.contracts.values() {
            let is_final = matches!(c.state, ContractState::Final(_));
            if !is_final && !self.timers.contains(&(c.resolution_deadline, TimerKey::HardDeadline(c.id))) {
                return Err(format!("I5 contract {:?} has no hard-deadline timer", c.id));
            }
        }
        Ok(())
    }
}
```

`crates/rfq-core/src/engine/account.rs`:
```rust
use super::{Engine, Handled, Out};
use crate::event::{Emitted, Event, RejectReason};
use crate::ledger::LedgerError;
use crate::types::*;

impl Engine {
    pub(super) fn deposit(&mut self, actor: AccountId, to: AccountId, amount: Amount, out: &mut Out) -> Handled {
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
        self.ledger.withdraw(actor, amount).map_err(|_| RejectReason::InsufficientFunds)?;
        out.push(Emitted::to(actor, Event::Withdrawn { account: actor, amount }));
        Ok(())
    }
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p rfq-core`
Expected: all pass (5 types, 9 ledger, 9 engine_basics).

- [ ] **Step 7: Commit**

```bash
git add crates/rfq-core
git commit -m "feat(core): engine skeleton with genesis, nonces, deposits, timers and invariants I1-I5"
```

