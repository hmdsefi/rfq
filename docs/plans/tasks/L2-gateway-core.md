# Task L2: Gateway core (protocol, view filter, nonce book, hub). Pure, no I/O.

**Spec:** [gateway.md](../../specs/gateway.md) §3–§6 and §9. **Goal:** the whole gateway logic as a functional core, unit-tested without Aeron or sockets. L3 wraps it in I/O.

**Files:**
- Create:
  - `crates/rfq-gateway/Cargo.toml`
  - `crates/rfq-gateway/src/lib.rs`
  - `crates/rfq-gateway/src/protocol.rs`
  - `crates/rfq-gateway/src/view.rs`
  - `crates/rfq-gateway/src/nonce_book.rs`
  - `crates/rfq-gateway/src/hub.rs`
- Move: the pure probe logic from `crates/rfq-demo/src/nonce.rs` (`Probe`, `probe_action`, and the search over guesses) into `crates/rfq-app/src/client/nonce.rs` (`pub mod client;` in rfq-app). `rfq-demo` keeps only its Aeron I/O and calls into `rfq_app::client::nonce`. Behaviour is unchanged, and rfq-demo's nonce tests move with the logic.
- Modify: `Cargo.toml` (workspace: add `serde_json = "1"` to `[workspace.dependencies]`; `crates/*` already picks up the new member).
- Tests: unit tests in each module, plus `crates/rfq-gateway/tests/sealing.rs`.

**Interfaces.**

Consumes from L1:
- `rfq_app::protocol::{ClientCommand, EventBatch, BatchKind}`;
- `rfq_domain::{AccountId, Action, Audience, Balance, Emitted, Event, RejectReason}`.

Produces, for L3:

```rust
// protocol.rs — JSON shapes exactly as gateway.md §3 (serde tag = "type", snake_case)
pub enum ClientMsg { Hello { view: View }, Command { id: String, actor: AccountId, nonce: Option<u64>, action: Action }, Ping }
pub enum ServerMsg {
    Welcome { view: View, engine: EngineState, now: u64 },
    Sent { id: String, actor: AccountId, nonce: u64 },
    Batch(OutBatch),
    Result { id: String, seq: u64, latency_us: u64 },
    Status { engine: EngineState, last_seq: Option<u64> },
    Error { id: Option<String>, error: String },
    Pong,
}
pub enum EngineState { Up, Down }                 // "up" / "down"
pub struct OutBatch { seq, time, kind, origin: Option<Origin>, events: Vec<Emitted>, balances: Vec<AccountBalance> }
pub struct Origin { actor: AccountId, nonce: u64 }
pub struct AccountBalance { account: AccountId, balance: Balance }
pub fn decode_client(text: &str) -> Result<ClientMsg, String>   // "bad request: …", never panics
pub fn encode_server(msg: &ServerMsg) -> String

// view.rs
pub enum View { Admin, Account(AccountId) }        // JSON "admin" / {"account": 10}
pub fn filter(view: View, batch: &EventBatch) -> Option<OutBatch>

// hub.rs
pub struct ConnId(pub u64);
pub enum Input { Open(ConnId), Close(ConnId), Text(ConnId, String), Batch(EventBatch), Ingress { connected: bool }, Tick }
pub enum Effect { Offer(ClientCommand), Send(ConnId, String) }
pub struct Hub { … }
impl Hub {
    pub fn new() -> Hub;
    /// One input in, the effects to perform, in order. `now_us` is the gateway's monotonic clock
    /// (latency and timeouts); `wall_ms` is its wall clock (`welcome.now`).
    pub fn handle(&mut self, input: Input, now_us: u64, wall_ms: u64) -> Vec<Effect>;
}
```

## Requirements

1. **`Cargo.toml` for rfq-gateway:** `edition`, `rust-version` and `license` from the workspace; `publish = false`. Dependencies: `rfq-domain`, `rfq-app`, `serde`, `serde_json`, `tracing`. Dev-dependencies: `rfq-testkit`. Feature `aeron = []` (L3 fills it). `lib.rs` carries the same crate attributes as the other crates (`#![forbid(unsafe_code)]` and the `#![deny(…)]` set).
2. **`view::filter`** implements gateway.md §4 exactly:

```rust
pub fn filter(view: View, b: &EventBatch) -> Option<OutBatch> {
    let sees = |a: &Audience| match (view, a) {
        (View::Admin, _) | (_, Audience::Public) => true,
        (View::Account(me), Audience::Account(to)) => me == *to,
    };
    let events: Vec<Emitted> = b.events.iter().filter(|e| sees(&e.audience)).cloned().collect();
    let balances: Vec<AccountBalance> = b.balances.iter()
        .filter(|(a, _)| matches!(view, View::Admin) || view == View::Account(*a))
        .map(|(a, bal)| AccountBalance { account: *a, balance: *bal })
        .collect();
    let origin = b.origin
        .filter(|(actor, _)| matches!(view, View::Admin) || view == View::Account(*actor))
        .map(|(actor, nonce)| Origin { actor, nonce });
    let keep = matches!(view, View::Admin) || !events.is_empty() || !balances.is_empty() || origin.is_some();
    keep.then(|| OutBatch { seq: b.seq, time: b.time, kind: b.kind, origin, events, balances })
}
```

   An account view always keeps its own command's batch, even with no events, because the origin is its own.
3. **`nonce_book`:** per actor, `Known { last }`, `Syncing { probe state }` or `Unknown`.
   - `observe(origin, events)` advances `last` to `n` unless the batch's events are exactly one `CommandRejected(Duplicate | NonceGap)`.
   - `next(actor)` gives `last + 1` for a known actor.
   - An unknown actor starts a probe search, using `rfq_app::client::nonce`.
   - `forget(actor)` sets the actor back to `Unknown`. The hub calls it when the engine goes down with a command in flight, because that nonce may or may not have been consumed.
4. **`hub`:**
   - **Connections.** `Open` registers the connection with view Admin. `Close` drops it, along with its queued commands. A pending command's `result` is then dropped.
   - **`Text`** is decoded with `decode_client`. Errors go back as `Error { id: None, error }` to that connection, and the connection stays open.
   - **`Hello`** sets the view and replies with `Welcome` (`engine`, `now: wall_ms`), then `Status`.
   - **`Command`:**
     - If the engine is `Down`: `Error { id, "engine unavailable" }`.
     - If the actor has a command in flight or is syncing: queue it, FIFO per actor.
     - If the nonce is known or explicit: `Effect::Offer(ClientCommand{actor, nonce, action})` and `Sent{…}`, and record the pending entry `(actor, nonce) → (conn, id, sent_at = now_us)`.
     - If the actor is unknown: start probing. Probes are `Offer(Withdraw{amount: 0})` effects with guessed nonces, never reported to clients. Send the queued command once `last` is found.
     - An explicit nonce is sent as given. The book follows the engine's answer through `observe`.
   - **`Batch`:**
     - Feed the nonce book (probes included).
     - For each connection, send `ServerMsg::Batch(filter(view, &b))` when `Some`. Never send the batch of a probe: a batch whose origin is a probe stays internal.
     - Then, if the origin matches a pending entry, send `Result { id, seq, latency_us: now_us − sent_at }` to its connection and start that actor's next queued command.
     - Track `last_seq`.
     - A batch arriving while `Down` means the engine is back: set `Up` and broadcast `Status`.
   - **`Ingress { connected }`:** `false` → `Down`. Then:
     - fail every pending and queued command with `Error { id, "engine unavailable" }`;
     - `forget` their actors;
     - broadcast `Status { Down }`.

     `true` does not mark `Up` by itself. `Up` comes with the next batch, so a `Resume` is the first thing clients see after a restart.
   - **`Tick`:** a pending command older than 3 s (3_000_000 µs) → `Down`, the same as above.
5. **Never panic** on any input. No `unwrap` outside tests.

## Steps

- [ ] **Step 1: Move the probe logic** from rfq-demo to `rfq_app::client::nonce`. Run `cargo test -p rfq-app -p rfq-demo`. Expected: green, with the moved tests passing in rfq-app.
- [ ] **Step 2: Write the failing protocol tests:**
  - every `ClientMsg` shape decodes from the JSON in gateway.md §3;
  - an unknown `type`, missing fields, an invalid price (`0`), bad JSON, and a 70 KiB string each give `Err("bad request: …")`, not a panic;
  - every `ServerMsg` encodes to the §3 JSON (compare `serde_json::Value`).
- [ ] **Step 3: Implement `protocol.rs`** until they pass.
- [ ] **Step 4: Write the failing `tests/sealing.rs`.** `view_hides_what_engine_md_section_7_hides`:
  - Build the real `EventBatch`es of every `rfq_testkit::scenarios::all()` scenario. Run each through an `rfq_app::Node` over `MemJournal` + `MemTransport` (look at how rfq-app's tests read published batches), or through a Harness with `take_touched_balances`.
  - For each batch, for views Admin, ALICE, CAROL, MM1 and MM2, assert:
    - an account view never gets an event with another account's audience;
    - an account view never gets another account's balance;
    - an account view gets `origin` only for its own commands;
    - Admin gets everything unchanged;
    - a batch an account can't see at all is `None`.
- [ ] **Step 5: Implement `view.rs`** (the code above) until they pass.
- [ ] **Step 6: Write the failing nonce book and hub tests.** Name them exactly:
  - `nonce_book_advances_on_consumed_nonces_only` (`Duplicate`/`NonceGap` don't advance it; `InvalidState` does);
  - `nonce_book_probes_an_unknown_actor_like_rfq_demo` (simulate the engine's answers to the probes; the search ends at `Taken` with the right `last`);
  - `hub_sends_a_command_with_the_next_nonce_and_reports_its_result_with_latency`;
  - `hub_queues_a_second_command_for_the_same_actor_until_the_first_has_its_batch`;
  - `hub_filters_batches_per_connection_view`;
  - `hub_never_forwards_a_probe_batch`;
  - `in_flight_command_fails_when_the_engine_goes_down` (`Ingress{false}` → `Error` to the sender, `Status Down` to all, and the actor forgotten);
  - `a_command_while_down_is_refused_not_queued`;
  - `first_batch_after_down_marks_up_and_broadcasts_status`;
  - `a_pending_command_times_out_after_3s` (`Tick`);
  - `bad_text_gets_an_error_and_the_connection_stays_open`;
  - `close_drops_queued_commands_and_results`.
- [ ] **Step 7: Implement `nonce_book.rs` and `hub.rs`** until all pass.
- [ ] **Step 8: Gates** (workspace, aeron off): `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check && cargo deny check`. Expected: green.
- [ ] **Step 9: Commit:**
  - `refactor(app): nonce probing lives in rfq-app::client, shared by clients`
  - `feat(gateway): protocol, per-viewer filter, nonce book and hub (pure)`
