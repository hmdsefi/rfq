# Task L1: Engine output (touched balances, time, kind, Resume always published)

**Spec:** [gateway.md §2](../../specs/gateway.md). **Goal:** each `EventBatch` carries the engine time, which command produced it, and every touched account's balance after it. Every `Resume` is published. Nothing about the engine's state, hash, journal or replay changes.

**Files:**
- Modify:
  - `crates/rfq-domain/src/ledger.rs` (the touched set);
  - `crates/rfq-domain/src/engine/mod.rs` (clear on apply, `take_touched_balances`);
  - `crates/rfq-app/src/protocol.rs` (`BatchKind`, new `EventBatch` fields);
  - `crates/rfq-app/src/node.rs` (fill them, always publish `Resume`);
  - `crates/rfq-infra/src/transport/split.rs` (balances on the last chunk);
  - every `EventBatch { … }` literal in the workspace (grep for it), including `crates/rfq-demo` and the tests.
- Docs:
  - `docs/specs/engine.md` §7 (one paragraph);
  - `docs/specs/transport-aeron.md`, egress section (one paragraph).
- Tests:
  - `crates/rfq-domain/tests/engine_basics.rs` (or a new `crates/rfq-domain/tests/touched_balances.rs`);
  - node tests in `crates/rfq-app/src/node.rs` (they use `MemJournal`/`MemTransport`);
  - `crates/rfq-infra/src/transport/split.rs` tests;
  - the codec round-trip test.

**Interfaces produced (L2–L4 rely on these):**

```rust
// rfq-domain
impl Ledger { pub fn take_touched(&mut self) -> BTreeSet<AccountId> }
impl Engine { pub fn take_touched_balances(&mut self) -> Vec<(AccountId, Balance)> } // account order

// rfq-app::protocol
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BatchKind { Genesis, Tick, Resume, User }

pub struct EventBatch {
    pub seq: u64,
    pub time: Timestamp,                      // the sequencer's stamp (SequencedCommand::time)
    pub kind: BatchKind,
    pub origin: Option<(AccountId, u64)>,
    pub events: Vec<Emitted>,
    pub balances: Vec<(AccountId, Balance)>,  // touched by this command, after it
}
```

## Steps

- [ ] **Step 1: Write the failing domain tests** (new file `crates/rfq-domain/tests/touched_balances.rs`, using `rfq_testkit::Harness`):

```rust
use rfq_domain::*;
use rfq_testkit::accounts::*;
use rfq_testkit::{Harness, dollars, price};
use rfq_testkit::scenarios::demo_config;

fn touched(h: &mut Harness) -> Vec<AccountId> {
    h.engine.take_touched_balances().into_iter().map(|(a, _)| a).collect()
}

#[test]
fn a_deposit_touches_only_its_account_and_reports_the_balance_after_it() {
    let mut h = Harness::new(demo_config());
    h.fund(ALICE, 10);
    let out = h.engine.take_touched_balances();
    assert_eq!(out, vec![(ALICE, Balance { free: dollars(10), reserved: 0, escrowed: 0 })]);
}

#[test]
fn a_rejected_command_touches_nothing() {
    let mut h = Harness::new(demo_config());
    h.fund(ALICE, 10);
    h.engine.take_touched_balances();
    let out = h.user(ALICE, Action::Withdraw { amount: dollars(11) });
    assert!(rfq_testkit::rejection(&out).is_some());
    assert_eq!(touched(&mut h), Vec::<AccountId>::new());
}

#[test]
fn each_apply_starts_a_fresh_set() {
    let mut h = Harness::new(demo_config());
    h.fund(ALICE, 10);
    h.fund(MM1, 10); // ALICE's touch from the previous command is not reported again
    assert_eq!(touched(&mut h), vec![MM1]);
}

#[test]
fn accept_touches_the_requester_and_the_selected_mm_and_releases_the_loser() {
    // Run happy_path up to the accept and check the accept's touched set is {ALICE, MM2}
    // (MM1's hold was released earlier, at the deadline tick).
    let s = rfq_testkit::scenarios::all().into_iter().find(|s| s.name == "happy_path").expect("happy_path");
    let mut h = Harness::new(demo_config());
    let mut ctx = rfq_testkit::scenarios::Ctx::new(1_000);
    for step in s.steps {
        h.set_time(1_000 + step.at);
        let label = match &step.kind { rfq_testkit::scenarios::StepKind::Send { label, .. } => Some(*label), _ => None };
        let out = match step.kind {
            rfq_testkit::scenarios::StepKind::Tick => h.submit(Command::Tick),
            rfq_testkit::scenarios::StepKind::Send { label, actor, action } => {
                ctx.record_label(label, h.next_seq());
                let a = action(&ctx);
                h.user(actor, a)
            }
        };
        ctx.observe(&out);
        let t = touched(&mut h);
        if label == Some("accept") {
            assert_eq!(t, vec![ALICE, MM2]);
            return;
        }
    }
    panic!("happy_path has no accept step");
}

#[test]
fn draining_the_set_never_changes_the_state_hash() {
    let mut h = Harness::new(demo_config());
    h.fund(ALICE, 10);
    let before = h.engine.state_hash();
    h.engine.take_touched_balances();
    assert_eq!(h.engine.state_hash(), before);
}
```

Run: `cargo test -p rfq-domain --test touched_balances`. Expected: compile errors, because `take_touched_balances` doesn't exist yet. Note: `Harness::user` and `Harness::submit` call `engine.apply`, then the test drains. If `Harness` needs a public `engine` field, it already has one (`pub engine: Engine`).

- [ ] **Step 2: Implement the touched set in `ledger.rs`.** Add to `Ledger`:

```rust
    /// Accounts changed since the last `take_touched`. Output only, not state: skipped by serde,
    /// so it never reaches `state_hash`, snapshots or the journal.
    #[serde(skip)]
    touched: BTreeSet<AccountId>,
```

```rust
    /// The accounts changed since the last call, emptying the set.
    pub fn take_touched(&mut self) -> BTreeSet<AccountId> {
        std::mem::take(&mut self.touched)
    }
```

In each mutator, after its last successful write and immediately before `Ok(…)`, insert the accounts it changed:
- `deposit(a, …)` and `withdraw(a, …)`: `a`;
- `hold(a, …)`: `a`;
- `release(h)` and `escrow_from_hold(h, …)`: the hold's account;
- `pay_from_escrow(from, …, to)`: `from` and `to`;
- `forfeit(h, to)`: the hold's account and `to`.

The error paths write nothing, so they insert nothing. Add `use std::collections::BTreeSet;`.

- [ ] **Step 3: Implement it in `engine/mod.rs`.** At the start of `Engine::apply`, before the seq check, add `self.ledger.take_touched();`, so a previous command's touches are never reported again. Then:

```rust
    /// The accounts the last `apply` changed (its timers included), with each one's balance after
    /// it, in account order. Gateways send these with the command's events (docs/specs/gateway.md §2).
    pub fn take_touched_balances(&mut self) -> Vec<(AccountId, Balance)> {
        let touched = self.ledger.take_touched();
        touched.into_iter().map(|a| (a, self.ledger.balance(a))).collect()
    }
```

Run Step 1's tests. Expected: PASS. Run `cargo test -p rfq-domain`. Expected: everything passes, including the property and replay-hash tests, which prove the hash didn't change.

- [ ] **Step 4: Write the failing node tests** in `crates/rfq-app/src/node.rs` tests (follow the existing ones that use `MemJournal`/`MemTransport` and read the published batches):
  - `a_batch_carries_time_kind_and_touched_balances`: a deposit by OPERATOR to ALICE at wall time `T` publishes a batch with `time == T` (or the sequencer's clamped stamp), `kind == BatchKind::User`, and `balances == [(ALICE, Balance{free: amount, ..})]`.
  - `resume_is_published_even_without_events`: start a node over a journal with history; the first poll publishes a batch with `kind == BatchKind::Resume` and `origin == None`, even when no dispute window is open (no events).
  - `a_tick_with_no_events_is_not_published`: unchanged behaviour, now pinned.
  - `replay_publishes_nothing`: already exists; keep it green.

- [ ] **Step 5: Implement it in `protocol.rs` and `node.rs`.** Add `BatchKind` and the three fields, and update the doc comment of `EventBatch`. In `Node::step`, where batches are built:

```rust
                let events = self.engine.apply(sc);
                let balances = self.engine.take_touched_balances();
                let (kind, origin) = match &sc.cmd {
                    Command::User { actor, nonce, .. } => (BatchKind::User, Some((*actor, *nonce))),
                    Command::Tick => (BatchKind::Tick, None),
                    Command::Resume => (BatchKind::Resume, None),
                    Command::Genesis(_) => (BatchKind::Genesis, None),
                };
                // Every Resume is published, so clients see each restart and stall (gateway.md §2.3).
                (origin.is_some() || !events.is_empty() || kind == BatchKind::Resume).then_some(EventBatch {
                    seq: sc.seq,
                    time: sc.time,
                    kind,
                    origin,
                    events,
                    balances,
                })
```

`Node::start`'s replay applies with egress suppressed. It also calls `apply`, and the next `apply` clears the set, so nothing leaks.

- [ ] **Step 6: Make `split_batch` put balances on the last chunk.** Test first in `split.rs`, as `split_batch_keeps_balances_on_the_last_chunk`. Build a batch with many events and two balances, and split it small. Then:
  - every chunk has the same `seq`, `time`, `kind` and `origin`;
  - only the last chunk has the balances;
  - the concatenated events equal the original.

Implement it:
- `empty` gets `balances: Vec::new()` (and copies `time` and `kind`);
- after the loop, `current.balances = batch.balances.clone()` before pushing the last chunk;
- the early return keeps the whole batch.

Update the size bookkeeping (`debug_assert_eq!` and `encoded_len`) so it holds with the balances on the last chunk. Account for their encoded size when deciding where to cut. If the balances alone exceed `max_bytes`, put them on their own final chunk and note this in the doc comment.

- [ ] **Step 7: Fix every other `EventBatch` literal.** Grep the workspace: rfq-demo `scope.rs` tests, the transport tests, the codec tests. Add `time`, `kind` and `balances`. Extend the codec round-trip test with a batch that has balances.

- [ ] **Step 8: Docs.** Add one paragraph each:
  - engine.md §7: "Each `EventBatch` also carries the command's engine `time`, its `kind`, and the balances of every account the command changed (after it). `Resume` is always published."
  - transport-aeron.md, egress section: the same, plus "balances travel on the last chunk of a split batch".

  Keep the D3 file (`failure-modes.md`) untouched.

- [ ] **Step 9: Gates.**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check && cargo deny check`
Expected: all green.

Run: `docker build -f docker/rust.Dockerfile --build-arg BIN=rfq-node -t rfq-node . && docker build -f docker/rust.Dockerfile --build-arg BIN=rfq-demo -t rfq-demo .`
Expected: both build. If Docker is not running, start Docker Desktop first.

Run: `./scripts/demo.sh`
Expected: 6 PASS and `RECOVERY OK`.

- [ ] **Step 10: Commit** (stage by path), one commit per scope:
  - `feat(domain): ledger reports the accounts each command touched`
  - `feat(app): egress batches carry engine time, kind and touched balances; Resume always published`
  - `docs: egress batch fields`
