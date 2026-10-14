# Task 7: Scenarios, property test and replay determinism

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Files:**
- Create: `crates/rfq-core/src/scenarios.rs`, `crates/rfq-core/tests/scenarios.rs`, `crates/rfq-core/tests/properties.rs`
- Modify: `crates/rfq-core/src/lib.rs` (add `pub mod scenarios;`)

**Interfaces:**
- Consumes: `Harness`, `accounts`, `genesis`.
- Produces (used by `rfq-demo`, Task 11): `scenarios::{demo_config() -> Config, all() -> Vec<Scenario>, Scenario { name, steps, expect }, Step { at, kind }, StepKind::{Send { label, actor, action }, Tick}, Ctx::{new(t0), record_label, observe, seq, request, quote, contract, picks, at}, run_in_memory(&Scenario, t0) -> (Harness, Vec<Emitted>), contains_in_order(&[Emitted], &[&str]) -> Result<(), String>}`.

- [ ] **Step 1: Write the scenarios module**

`crates/rfq-core/src/scenarios.rs`:
```rust
//! The happy path and the failure paths from the brief, as timed scripts.
//! The same scripts run in memory (tests) and over Aeron (rfq-demo).
use std::collections::BTreeMap;

use crate::command::*;
use crate::event::{Emitted, Event};
use crate::testkit::{accounts::*, dollars, price, Harness};
use crate::types::*;

/// Short windows so the Aeron demo runs in seconds.
pub fn demo_config() -> Config {
    Config { accept_window: 2_000, min_response: 500, max_response: 60_000, max_qty: 1_000_000, dispute_bond: dollars(100) }
}

pub struct Ctx {
    t0: Timestamp,
    labels: BTreeMap<&'static str, u64>,
    presented: BTreeMap<RequestId, Vec<Pick>>,
}

impl Ctx {
    pub fn new(t0: Timestamp) -> Self { Ctx { t0, labels: BTreeMap::new(), presented: BTreeMap::new() } }
    pub fn at(&self, offset: Duration) -> Timestamp { self.t0 + offset }
    /// One contract per scenario run, keyed by its start time so reruns never collide.
    pub fn contract(&self) -> ContractId { ContractId(self.t0) }
    pub fn record_label(&mut self, label: &'static str, seq: u64) { self.labels.insert(label, seq); }
    pub fn seq(&self, label: &str) -> u64 {
        *self.labels.get(label).unwrap_or_else(|| panic!("label {label} has no seq yet"))
    }
    pub fn request(&self, label: &str) -> RequestId { RequestId(self.seq(label)) }
    pub fn quote(&self, label: &str) -> QuoteId { QuoteId(self.seq(label)) }
    pub fn picks(&self, request: RequestId) -> Vec<Pick> { self.presented.get(&request).cloned().unwrap_or_default() }
    pub fn observe(&mut self, events: &[Emitted]) {
        for e in events {
            if let Event::RequestPresented { request, picks, .. } = &e.event {
                self.presented.insert(*request, picks.clone());
            }
        }
    }
}

pub type ActionFn = Box<dyn Fn(&Ctx) -> Action + Send + Sync>;

pub enum StepKind {
    Send { label: &'static str, actor: AccountId, action: ActionFn },
    Tick,
}

pub struct Step {
    /// Offset from the scenario start, in ms.
    pub at: Duration,
    pub kind: StepKind,
}

pub struct Scenario {
    pub name: &'static str,
    pub steps: Vec<Step>,
    /// Event names that must appear in this order (other events may be interleaved).
    pub expect: Vec<&'static str>,
}

fn send(at: Duration, label: &'static str, actor: AccountId, f: impl Fn(&Ctx) -> Action + Send + Sync + 'static) -> Step {
    Step { at, kind: StepKind::Send { label, actor, action: Box::new(f) } }
}

fn tick(at: Duration) -> Step { Step { at, kind: StepKind::Tick } }

/// Funds everyone and lists the scenario's contract (closes at +5 s, hard deadline at +15 s).
fn setup() -> Vec<Step> {
    let mut steps: Vec<Step> = [("fund_alice", ALICE), ("fund_mm1", MM1), ("fund_mm2", MM2), ("fund_carol", CAROL)]
        .into_iter()
        .map(|(label, to)| send(0, label, OPERATOR, move |_| Action::Deposit { to, amount: dollars(1_000) }))
        .collect();
    steps.push(send(0, "list", OPERATOR, |c| Action::ListContract {
        id: c.contract(),
        description: "Will it rain in Paris on 2026-11-01?".into(),
        oracle: ORACLE,
        trading_close: c.at(5_000),
        resolution_deadline: c.at(15_000),
        dispute_window: 2_000,
    }));
    steps
}

/// Request 1000 YES (limit 0.70), quotes at 0.62 and 0.60, presented at +1.5 s.
fn quoted() -> Vec<Step> {
    let mut s = setup();
    s.push(send(100, "req", ALICE, |c| Action::SubmitRequest {
        legs: vec![LegSpec { contract: c.contract(), side: Side::BuyYes, qty: 1_000, limit_price: price(7_000) }],
        response_deadline: c.at(1_500),
    }));
    s.push(send(300, "q1", MM1, |c| Action::SubmitQuote { request: c.request("req"), leg: 0, price: price(6_200), size: 1_000, expires_at: c.at(10_000) }));
    s.push(send(400, "q2", MM2, |c| Action::SubmitQuote { request: c.request("req"), leg: 0, price: price(6_000), size: 1_000, expires_at: c.at(10_000) }));
    s.push(tick(1_600));
    s
}

fn accept(at: Duration) -> Step {
    send(at, "accept", ALICE, |c| Action::Accept { request: c.request("req"), picks: c.picks(c.request("req")) })
}

fn filled() -> Vec<Step> {
    let mut s = quoted();
    s.push(accept(2_000));
    s
}

pub fn all() -> Vec<Scenario> {
    let happy = {
        let mut s = filled();
        s.push(tick(5_100));
        s.push(send(5_200, "propose", ORACLE, |c| Action::ProposeOutcome { contract: c.contract(), outcome: Outcome::Yes }));
        s.push(tick(7_300));
        Scenario {
            name: "happy_path",
            steps: s,
            expect: vec!["RequestOpened", "QuoteSubmitted", "QuoteSubmitted", "QuoteLost", "QuoteSelected", "RequestPresented", "QuoteFilled", "RequestFilled", "ContractClosed", "OutcomeProposed", "ContractFinalized", "PositionSettled"],
        }
    };
    let withdraw = {
        let mut s = quoted();
        s.push(send(1_800, "cancel", MM2, |c| Action::CancelQuote { quote: c.quote("q2") }));
        s.push(accept(1_900));
        Scenario { name: "mm_withdraws_after_selection", steps: s, expect: vec!["RequestPresented", "QuoteCancelled", "RequestRejected", "CommandRejected"] }
    };
    let no_quotes = {
        let mut s = setup();
        s.push(send(100, "req", ALICE, |c| Action::SubmitRequest {
            legs: vec![LegSpec { contract: c.contract(), side: Side::BuyYes, qty: 1_000, limit_price: price(7_000) }],
            response_deadline: c.at(1_500),
        }));
        s.push(tick(1_600));
        Scenario { name: "no_quotes_expire", steps: s, expect: vec!["RequestOpened", "RequestExpired"] }
    };
    let lapse = {
        let mut s = quoted();
        s.push(tick(3_600));
        Scenario { name: "requester_lapses", steps: s, expect: vec!["RequestPresented", "QuoteReleased", "RequestRejected"] }
    };
    let disputed = {
        let mut s = filled();
        s.push(tick(5_100));
        s.push(send(5_200, "propose", ORACLE, |c| Action::ProposeOutcome { contract: c.contract(), outcome: Outcome::Yes }));
        s.push(send(5_500, "dispute", CAROL, |c| Action::Dispute { contract: c.contract() }));
        s.push(send(6_000, "rule", ARBITER, |c| Action::Rule { contract: c.contract(), outcome: Outcome::Invalid }));
        Scenario { name: "disputed_ruled_invalid", steps: s, expect: vec!["RequestFilled", "OutcomeProposed", "Disputed", "Ruled", "ContractFinalized", "PositionSettled"] }
    };
    let silent = {
        let mut s = filled();
        s.push(tick(15_100));
        Scenario { name: "oracle_silent_hard_deadline", steps: s, expect: vec!["RequestFilled", "ContractClosed", "ContractFinalized", "PositionSettled"] }
    };
    vec![happy, withdraw, no_quotes, lapse, disputed, silent]
}

/// Runs a scenario on a fresh in-memory engine. Returns the harness and every emitted event.
pub fn run_in_memory(s: &Scenario, t0: Timestamp) -> (Harness, Vec<Emitted>) {
    let mut h = Harness::new(demo_config());
    let mut ctx = Ctx::new(t0);
    let mut events = Vec::new();
    for step in &s.steps {
        h.set_time(t0 + step.at);
        let out = match &step.kind {
            StepKind::Tick => h.submit(Command::Tick),
            StepKind::Send { label, actor, action } => {
                ctx.record_label(*label, h.next_seq());
                let a = action(&ctx);
                h.user(*actor, a)
            }
        };
        ctx.observe(&out);
        events.extend(out);
    }
    (h, events)
}

pub fn contains_in_order(events: &[Emitted], expect: &[&str]) -> Result<(), String> {
    let mut it = events.iter().map(|e| e.event.name());
    for want in expect {
        if !it.any(|got| got == *want) {
            return Err(format!("expected event {want} (in order {expect:?})"));
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Write the failing scenario tests**

`crates/rfq-core/tests/scenarios.rs`:
```rust
use rfq_core::scenarios::*;
use rfq_core::testkit::{accounts::*, dollars};
use rfq_core::*;

const T0: Timestamp = 1_000_000;

fn run(name: &str) -> (rfq_core::testkit::Harness, Vec<Emitted>) {
    let s = all().into_iter().find(|s| s.name == name).unwrap();
    let (h, events) = run_in_memory(&s, T0);
    contains_in_order(&events, &s.expect).unwrap_or_else(|e| panic!("{name}: {e}"));
    (h, events)
}

fn free(h: &rfq_core::testkit::Harness, a: AccountId) -> Amount { h.engine.balance(a).free }

#[test]
fn happy_path() {
    let (h, _) = run("happy_path");
    assert_eq!(free(&h, ALICE), dollars(1_400));
    assert_eq!(free(&h, MM1), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(600));
}

#[test]
fn mm_withdraws_after_selection() {
    let (h, _) = run("mm_withdraws_after_selection");
    for a in [ALICE, MM1, MM2] {
        assert_eq!(free(&h, a), dollars(1_000));
    }
}

#[test]
fn no_quotes_expire() {
    let (h, _) = run("no_quotes_expire");
    assert_eq!(free(&h, ALICE), dollars(1_000));
}

#[test]
fn requester_lapses() {
    let (h, _) = run("requester_lapses");
    for a in [ALICE, MM1, MM2] {
        assert_eq!(free(&h, a), dollars(1_000));
    }
}

#[test]
fn disputed_ruled_invalid() {
    let (h, _) = run("disputed_ruled_invalid");
    for a in [ALICE, MM2, CAROL] {
        assert_eq!(free(&h, a), dollars(1_000));
    }
}

#[test]
fn oracle_silent_hard_deadline() {
    let (h, _) = run("oracle_silent_hard_deadline");
    for a in [ALICE, MM2] {
        assert_eq!(free(&h, a), dollars(1_000));
    }
}

#[test]
fn replay_reproduces_state_hash() {
    for s in all() {
        let (h, _) = run_in_memory(&s, T0);
        let mut replayed = Engine::new();
        for sc in &h.log {
            replayed.apply(sc);
        }
        assert_eq!(replayed.state_hash(), h.engine.state_hash(), "{}", s.name);
        // Replaying the log a second time is a no-op (seq <= last_seq).
        for sc in &h.log {
            assert!(replayed.apply(sc).is_empty());
        }
        assert_eq!(replayed.state_hash(), h.engine.state_hash());
    }
}
```

- [ ] **Step 3: Write the property test**

`crates/rfq-core/tests/properties.rs`:
```rust
//! Random command streams must never break I1–I5 (the harness checks them after every command).
use proptest::prelude::*;
use rfq_core::testkit::{accounts::*, dollars, price, Harness};
use rfq_core::*;

const ACTORS: [AccountId; 5] = [ALICE, MM1, MM2, CAROL, ORACLE];

#[derive(Debug, Clone)]
struct Op { actor: usize, kind: u8, a: u16, b: u16, dt: u32 }

fn op() -> impl Strategy<Value = Op> {
    (0..ACTORS.len(), 0u8..10, any::<u16>(), any::<u16>(), 0u32..20_000)
        .prop_map(|(actor, kind, a, b, dt)| Op { actor, kind, a, b, dt })
}

fn pick<T: Copy>(items: &[T], i: u16) -> Option<T> {
    if items.is_empty() { None } else { Some(items[i as usize % items.len()]) }
}

fn action(h: &Harness, o: &Op, c: ContractId) -> Option<Action> {
    let now = h.now();
    let requests: Vec<RequestId> = h.engine.requests().map(|r| r.id).collect();
    let quotes: Vec<QuoteId> = h.engine.quotes().map(|q| q.id).collect();
    let p = price(1 + (o.b % 9_999));
    Some(match o.kind {
        0 => Action::SubmitRequest {
            legs: vec![LegSpec { contract: c, side: if o.a % 2 == 0 { Side::BuyYes } else { Side::BuyNo }, qty: 1 + (o.a as u64 % 500), limit_price: p }],
            response_deadline: now + 1_000 + (o.b as u64 % 20_000),
        },
        1 => Action::SubmitQuote { request: pick(&requests, o.a)?, leg: 0, price: p, size: 500, expires_at: now + 1 + o.b as u64 * 3 },
        2 => Action::CancelQuote { quote: pick(&quotes, o.a)? },
        3 => Action::CancelRequest { request: pick(&requests, o.a)? },
        4 => {
            let r = h.engine.request(pick(&requests, o.a)?)?;
            let picks = r.legs.iter().enumerate().filter_map(|(i, l)| {
                let q = h.engine.quote(l.selected?)?;
                Some(Pick { leg: i as u32, quote: q.id, price: q.price })
            }).collect();
            Action::Accept { request: r.id, picks }
        }
        5 => Action::Reject { request: pick(&requests, o.a)? },
        6 => Action::ProposeOutcome { contract: c, outcome: [Outcome::Yes, Outcome::No, Outcome::Invalid][o.a as usize % 3] },
        7 => Action::Dispute { contract: c },
        8 => Action::Rule { contract: c, outcome: [Outcome::Yes, Outcome::No, Outcome::Invalid][o.a as usize % 3] },
        _ => Action::Withdraw { amount: dollars(1 + o.a as u64 % 50) },
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn invariants_hold_for_random_streams(ops in prop::collection::vec(op(), 1..200)) {
        let mut h = Harness::new(Config::default());
        for a in ACTORS { h.fund(a, 10_000); }
        let c = ContractId(1);
        h.user(OPERATOR, Action::ListContract { id: c, description: "p".into(), oracle: ORACLE, trading_close: 2_000_000, resolution_deadline: 4_000_000, dispute_window: 5_000 });
        let total: Amount = ACTORS.iter().map(|a| { let b = h.engine.balance(*a); b.free + b.reserved + b.escrowed }).sum();
        let mut withdrawn: Amount = 0;
        for o in &ops {
            let t = h.now() + o.dt as u64;
            h.set_time(t);
            match action(&h, o, c) {
                Some(a) => {
                    let amount = if let Action::Withdraw { amount } = a { Some(amount) } else { None };
                    let out = h.user(ACTORS[o.actor], a);
                    if let (Some(x), None) = (amount, rfq_core::testkit::rejection(&out)) { withdrawn += x; }
                }
                None => { h.submit(Command::Tick); }
            }
        }
        h.tick(h.now() + 5_000_000); // past every deadline: everything must be released or settled
        let now_total: Amount = [ALICE, MM1, MM2, CAROL, ORACLE, TREASURY].iter().map(|a| h.engine.balance(*a).free).sum();
        prop_assert_eq!(now_total + withdrawn, total);
        for a in ACTORS { prop_assert_eq!(h.engine.balance(a).reserved + h.engine.balance(a).escrowed, 0); }
    }
}
```
The trading close is placed at 2,000,000 ms, which is beyond the reach of the random times (≤ 200 × 20 s = 4,000,000 ms). Requests created after the close are rejected and that's fine. The final tick past every deadline proves the no-dead-ends property: nothing stays reserved or escrowed.

- [ ] **Step 4: Run all tests**

Run: `cargo test -p rfq-core`
Expected: all pass. The property test runs 256 cases.

- [ ] **Step 5: Commit**

```bash
git add crates/rfq-core
git commit -m "test(core): scripted happy/failure scenarios, invariant property test, replay determinism"
```

