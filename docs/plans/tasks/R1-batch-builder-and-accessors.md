# Task R1: Shared batch builder, engine read accessors

**Spec:** [read-api.md](../../specs/read-api.md) §2 (rows "Batch builder", "Read accessors"). **Goal:** the replica (R2) can build exactly the batches the node builds, and read positions and nonces, with no change to engine or node behaviour.

**Files:**
- Create: `crates/rfq-app/src/output.rs`.
- Modify:
  - `crates/rfq-app/src/lib.rs` (`pub mod output; pub use output::batch_of;`);
  - `crates/rfq-app/src/node.rs` (`Node::step` builds batches with `batch_of`);
  - `crates/rfq-domain/src/engine/mod.rs` (two accessors).

**Interfaces.**
- Produces (used by R2):

```rust
// rfq-app/src/output.rs
/// The batch the node publishes for an applied command, or `None` when it publishes nothing:
/// a batch for every user command (it has an origin), for every `Resume`, and for any other
/// command that emitted events (gateway.md §2.3).
pub fn batch_of(sc: &SequencedCommand, events: Vec<Emitted>, balances: Vec<(AccountId, Balance)>) -> Option<EventBatch>;

// rfq-domain Engine
pub fn positions(&self) -> impl Iterator<Item = &Position>;   // id order
pub fn last_nonce(&self, a: AccountId) -> u64;                // 0 for an account never seen
```

## Requirements

1. `batch_of` is a move of the code inside `Node::step`'s `filter_map` (`kind` and `origin` from the command; publish when `origin.is_some() || !events.is_empty() || kind == BatchKind::Resume`). The node calls it; its behaviour is identical.
2. The accessors only read: `positions()` iterates `self.positions.values()`; `last_nonce(a)` is `self.nonces.get(&a).copied().unwrap_or(0)`. No new state, nothing serialized changes, `state_hash` unchanged.

## Steps

- [ ] **Step 1: Failing tests for `batch_of`** (unit tests in `output.rs`):
  - `a_user_command_always_has_a_batch_with_its_origin` (a user command with no events still gives `Some`, `kind: User`, `origin: Some((actor, nonce))`);
  - `a_tick_with_no_events_has_no_batch` / `a_tick_with_events_has_one` (`kind: Tick`, `origin: None`);
  - `a_resume_always_has_a_batch` (no events, still `Some`, `kind: Resume`);
  - `genesis_with_its_event_has_a_batch` (`kind: Genesis`);
  - `the_batch_carries_seq_time_and_balances_unchanged`.

  Run `cargo test -p rfq-app output`: FAIL (no `batch_of`).
- [ ] **Step 2:** Implement `batch_of`, export it, and replace the inline code in `Node::step` with it. Run `cargo test -p rfq-app` and `cargo test -p rfq-infra --test node`: PASS, unchanged.
- [ ] **Step 3: Failing tests for the accessors** (in `crates/rfq-domain/src/engine/mod.rs` tests or a new `crates/rfq-domain/tests/read_accessors.rs`):
  - `last_nonce_is_zero_for_an_unknown_account_and_follows_accepted_commands` (after nonces 1 and 2, `last_nonce == 2`; a `Duplicate` or `NonceGap` leaves it unchanged);
  - `positions_lists_every_position_in_id_order` (run the testkit `happy_path` in memory with `rfq_testkit::scenarios::run_in_memory`; `positions()` yields one position with the expected holders and locks).

  Run: FAIL. Implement. Run: PASS.
- [ ] **Step 4: Gates:** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`, `cargo deny check`. All green, the same test count plus the new ones.
- [ ] **Step 5: Commit** (stage by path):
  - `refactor(app): one batch builder for the node and its readers`
  - `feat(domain): read accessors for positions and nonces`
