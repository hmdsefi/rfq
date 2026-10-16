# Task R2: `rfq-read`: replica, queries, trace

**Spec:** [read-api.md](../../specs/read-api.md) §2, §3 (reads), §6, §7, §9 (sealing). **Goal:** a pure crate that turns journal commands into the engine's state, the trace and the API's read views, provably equal to the node.

**Files:**
- Create:
  - `crates/rfq-read/Cargo.toml` (`publish = false`, `license = "MPL-2.0"`, the workspace's lints and `[dependencies]`: `rfq-domain`, `rfq-app`, `serde`, `thiserror`; `[dev-dependencies]`: `rfq-testkit`, `rfq-infra` (no features: the in-memory adapters), `serde_json`);
  - `crates/rfq-read/src/lib.rs` (crate doc, `#![forbid(unsafe_code)]` and the same `deny` list as the other crates; re-exports);
  - `crates/rfq-read/src/replica.rs`;
  - `crates/rfq-read/src/views.rs`;
  - `crates/rfq-read/tests/replica_matches_node.rs`;
  - `crates/rfq-read/tests/views.rs`.
- Modify: `Cargo.toml` (workspace members: add `crates/rfq-read`).

**Interfaces.**
- Consumes (R1): `rfq_app::batch_of`, `Engine::positions`, `Engine::last_nonce`.
- Produces (used by R4 and R5):

```rust
// replica.rs
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TraceEntry {
    pub batch: EventBatch,
    /// The user command behind the batch (`None` for Genesis, Tick, Resume).
    pub command: Option<ClientCommand>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReplicaError {
    #[error("journal out of sequence: expected seq {expected}, got {found}")]
    OutOfSequence { expected: u64, found: u64 },
}

#[derive(Clone, Debug, Default)]
pub struct Replica { /* engine: Engine, trace: BTreeMap<u64, TraceEntry> */ }

impl Replica {
    pub fn new() -> Self;
    /// Applies the next journal command; `sc.seq` must be `last_seq() + 1`. Builds the batch with
    /// `batch_of` (after `take_touched_balances`) and keeps it with its command.
    pub fn apply(&mut self, sc: &SequencedCommand) -> Result<(), ReplicaError>;
    pub fn last_seq(&self) -> u64;
    pub fn state_hash(&self) -> [u8; 32];
    pub fn engine(&self) -> &Engine;
    /// Trace entries with `seq > after_seq`, ascending.
    pub fn trace_after(&self, after_seq: u64) -> impl Iterator<Item = &TraceEntry>;
}

// views.rs: serialized as the spec's JSON (field names exactly as below)
#[derive(Serialize)] pub struct AccountView { pub account: AccountId, pub balance: Balance, pub last_nonce: u64 }
#[derive(Serialize)] pub enum Holding { Yes, No }
#[derive(Serialize)] pub struct PositionView { pub id: PositionId, pub market: MarketId, pub request: RequestId, pub qty: Qty, pub side: Holding, pub stake: Amount, pub settled: bool, pub payout: Option<Amount> }
#[derive(Serialize)] pub struct Selected { pub quote: QuoteId, pub price: Price }
#[derive(Serialize)] pub struct OwnLegView { pub market: MarketId, pub side: Side, pub qty: Qty, pub limit_price: Price, pub selected: Option<Selected> }
#[derive(Serialize)] pub struct OwnRequestView { pub id: RequestId, pub state: RequestState, pub response_deadline: Timestamp, pub legs: Vec<OwnLegView> }
#[derive(Serialize)] pub struct PublicLegView { pub market: MarketId, pub side: Side, pub qty: Qty }
#[derive(Serialize)] pub struct PublicRequestView { pub id: RequestId, pub response_deadline: Timestamp, pub legs: Vec<PublicLegView> }
#[derive(Serialize)] pub struct OwnQuoteView { pub id: QuoteId, pub request: RequestId, pub leg: u32, pub price: Price, pub size: Qty, pub expires_at: Timestamp, pub state: QuoteState }
#[derive(Serialize)] pub struct MarketView { pub id: MarketId, pub description: String, pub oracle: AccountId, pub trading_close: Timestamp, pub resolution_deadline: Timestamp, pub dispute_window: Duration, pub state: MarketState }

pub fn account(e: &Engine, a: AccountId) -> AccountView;
pub fn positions_of(e: &Engine, a: AccountId) -> Vec<PositionView>;        // id order
pub fn requests_of(e: &Engine, a: AccountId) -> Vec<OwnRequestView>;       // id order
pub fn quotes_of(e: &Engine, a: AccountId) -> Vec<OwnQuoteView>;           // id order
pub fn open_requests(e: &Engine, market: Option<MarketId>) -> Vec<PublicRequestView>; // state Open only; a request matches `market` if any leg is on it
pub fn markets(e: &Engine) -> Vec<MarketView>;
pub fn market(e: &Engine, id: MarketId) -> Option<MarketView>;
```

## Requirements

1. **`apply`:** refuse `seq != last_seq + 1` with `OutOfSequence` and change nothing. Otherwise `engine.apply(sc)`, then `take_touched_balances()`, then `batch_of(...)`; keep the entry when `Some`, with `command` = the `ClientCommand` of a `Command::User` (else `None`).
2. **`positions_of`:** positions where the account is `yes_holder` (side `Yes`, stake `yes_lock`) or `no_holder` (side `No`, stake `no_lock`). `payout` is `None` until settled; once settled it is what `finalize` paid this account: from the market's `Final(outcome)`, `Yes` → the YES holder gets `yes_lock + no_lock`, `No` → the NO holder gets it, `Invalid` → each gets its own lock; the other side gets `0`. No counterparty field.
3. **`requests_of`:** never expose a leg's live quote set. `selected` is `Some({quote, price})` only when the leg has a selected quote (the request was presented): its price is that quote's price.
4. **`open_requests`:** state `Open` only; no requester, no limit, no quotes.
5. **`account`:** `balance` from the ledger (zero for an unknown account), `last_nonce` from R1.
6. No `HashMap`; iteration in id order everywhere.

## Steps

- [ ] **Step 1: The equivalence test, failing.** In `tests/replica_matches_node.rs`, for **every** scenario in `rfq_testkit::scenarios::all()` (the multi-leg ones included):
  1. Drive a `Node<MemJournal, SharedTransport>` (define `SharedTransport` in the test: `Rc<RefCell<MemTransport>>` implementing `rfq_app::Transport`, as `crates/rfq-infra/tests/node.rs` does) with `rfq_testkit::scenarios::demo_config()`:
     - for each step at `t0 + at`: a `Send` pushes `ClientCommand { actor, nonce: next for that actor (start at 1), action: action(&ctx) }` and calls `node.poll(t)`; record the label's seq as `node.engine().last_seq()` (`ctx.record_label`) and `ctx.observe` the events of the batches published by that poll (so `Accept` picks resolve);
     - a `Tick` calls `node.poll(t)` (the node ticks when a timer is due);
     - after the last step, poll once more at `t0 + 20_000` so every timer fires.
  2. Feed `journal.entries()` (from `node.into_parts()`) to a fresh `Replica`.
  3. Assert:
     - `replica.state_hash() == node.state_hash()` (take the hash before `into_parts`);
     - every batch in `transport.egress` equals `replica` trace entry `batch` with the same `seq`;
     - the trace's first entry is `seq` 1, `kind: Genesis`, events `[Initialized]` (the node applied it without publishing);
     - each user batch's `command` equals the `ClientCommand` the test pushed.
- [ ] **Step 2: Restart equivalence (Review Focus 1).** `replica_matches_a_node_restarted_mid_scenario`: run `happy_path` up to after the accept, `into_parts`, `Node::start` again on the same journal (a `Resume` is sequenced on the next poll), finish the scenario. The replica fed the whole journal still matches every published batch, including the `Resume`, and the final hash.
- [ ] **Step 3: Sequence tests:** `apply_refuses_a_gap_and_a_duplicate_and_changes_nothing` (`OutOfSequence { expected: 2, found: 3 }`, then `{ expected: 2, found: 1 }`; `last_seq` and hash unchanged).
- [ ] **Step 4:** Implement `replica.rs`. Run steps 1–3: PASS.
- [ ] **Step 5: View tests, failing** (`tests/views.rs`; build engines with `rfq_testkit::scenarios::run_in_memory`, or a `Harness` stopped part-way):
  - `positions_of_after_happy_path`: Alice `[{side: Yes, stake: 600_000_000, settled: true, payout: Some(1_000_000_000)}]`; MM2 `[{side: No, stake: 400_000_000, settled: true, payout: Some(0)}]`; MM1 `[]`;
  - `positions_of_after_an_invalid_resolution_refunds_each_stake` (`disputed_ruled_invalid`: Alice payout 600, MM2 400);
  - `positions_are_unsettled_with_no_payout_before_resolution` (stop after the accept);
  - `requests_of_hides_the_quote_set_and_shows_the_selection_once_presented`: before the deadline the serialized JSON of Alice's request has `"selected": null` on every leg and no `quotes` key anywhere; after the deadline `selected == {quote, price}` of MM2's quote;
  - `open_requests_shows_only_public_fields` (serialized keys exactly `id`, `response_deadline`, `legs[].market|side|qty`; no `requester`, no `limit_price`), lists only `Open` requests, and filters by market;
  - `quotes_of_lists_the_mm_own_quotes_with_their_states` (MM1's quote is `Lost` after the deadline);
  - `account_is_zero_for_an_unknown_account`;
  - `markets_and_market_show_terms_and_state`.
- [ ] **Step 6:** Implement `views.rs`. Run: PASS.
- [ ] **Step 7: Gates** (workspace test, clippy, fmt, deny).
- [ ] **Step 8: Commit:** `feat(read): rfq-read, a replica fed by the journal`.
