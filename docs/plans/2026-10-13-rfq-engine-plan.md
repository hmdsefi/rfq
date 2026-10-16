# RFQ Matching & Settlement Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a deterministic, replayable RFQ engine for binary-outcome markets in Rust. Commands are sequenced and recorded through Aeron + Aeron Archive in Docker, with a demo that runs one happy path, five failure paths, and a crash-recovery replay.

**Architecture:** A pure `rfq-core` crate holds the engine, `apply(seq, time, cmd) -> events`. It has no I/O and no clock, and every money move goes through one ledger guarded by invariants I1–I5. `rfq-node` wraps the engine. It runs a sequencer, a `Journal` (Aeron Archive, file, or memory) and a `Transport` (Aeron or memory), and enforces write-ahead: no event is published before its command is durable. `rfq-demo` drives scripted scenarios over Aeron.

**Tech Stack:** Rust (edition 2021, toolchain 1.99), `serde` 1, `bincode` 1.3, `sha2` 0.10, `crc32fast` 1, `anyhow` 1, `tracing` 0.1, `proptest` 1, `rusteron-client` / `rusteron-archive` (version pinned by Task 0), Docker Compose, Java `aeron-all` jar (version pinned by Task 0), `criterion` 0.5 (benchmarks), `cargo-deny` (dependency audit), GitHub (private repo `hmdsefi/rfq`) + GitHub Actions (CI).

**Spec:**
- [docs/rfc/0001-rfq-engine.md](../rfc/0001-rfq-engine.md)
- [docs/architecture/overview.md](../architecture/overview.md)
- [docs/specs/engine.md](../specs/engine.md)
- [docs/specs/failure-modes.md](../specs/failure-modes.md)
- [docs/specs/resolution.md](../specs/resolution.md)
- [docs/specs/quote-lifetime.md](../specs/quote-lifetime.md)
- [docs/specs/transport-aeron.md](../specs/transport-aeron.md)

## Global Constraints

- Money is `u64` micro-USD (`$1 = 1_000_000`). Prices are `u16` bps in `1..=9999`. Buyer lock = `qty × bps × 100`; seller lock = `qty × (10000 − bps) × 100`. No floats and no rounding anywhere.
- All money arithmetic uses `checked_*`. Overflow rejects the command; it never panics.
- The engine never reads a clock, never uses randomness, and never iterates a `HashMap`. Use `BTreeMap`/`BTreeSet` only.
- Validate-then-apply: every handler finishes all checks before its first mutation. After that point, ledger calls use `.expect(VALIDATED)`.
- IDs come from the sequence: `RequestId(seq)` and `QuoteId(seq)`. `PositionId` and `HoldId` come from engine counters.
- `legs.len() != 1` is rejected with `Unsupported` (multi-leg is design only, RFC P6). The data model stays `Vec<Leg>`.
- Default config: `accept_window = 30_000` ms, `min_response = 1_000`, `max_response = 604_800_000`, `max_qty = 1_000_000_000`, `dispute_bond = 100 × ONE_DOLLAR`, `resume_grace = 600_000`, `min_request_notional = ONE_DOLLAR` (the last two from Gate A).
- Nonces are strict per actor: only `last + 1` is accepted (`Duplicate` / `NonceGap` otherwise). Every client, test builder and demo keeps a per-actor counter.
- Aeron streams are all `aeron:ipc`: ingress 1001, log 1002, egress 1003, replay 1004. Never use stream 10, which is Archive's local control.
- Codec: a `u8` version prefix (`1`) followed by bincode (fixint, little-endian, trailing bytes rejected). Size limit only at untrusted ingress: `decode_ingress` drops messages longer than `MAX_INGRESS` = 64 KiB before decoding. Trusted encode/decode (journal, egress) is unlimited, so `encode` never panics (ruling R21).
- Event names returned by `Event::name()` are exactly the variant names.
- `rfq-core` has `#![forbid(unsafe_code)]` and denies `clippy::unwrap_used`, `cast_possible_truncation`, `cast_possible_wrap` and `cast_sign_loss` (set up in Task 1). Narrowing conversions use `try_from(...).expect("<why it fits>")`, e.g. leg indices: `u32::try_from(i).expect("leg count is bounded")`.
- Release and bench builds keep `overflow-checks = true`. An unchecked overflow that slips past review panics instead of silently wrapping money.
- Every task's last step before commit runs `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check`.
- After the Setup step creates the GitHub repo, every commit is followed by `git push`. Once CI exists (Task 13), it must be green before the next task starts.

## Layered structure (supersedes crate names and paths in task code)

Ruling made at execution start, at the PO's request ("best structure, interfaces, application layers, best project template"). The authority is [architecture §0](../architecture/overview.md). Task code below was written against a single `rfq-core` crate. **Apply this mapping when implementing**; behaviour, tests and values stay exactly as written.

| Plan text says | Implement as |
|---|---|
| `crates/rfq-core/src/{types,ledger,command,event,model}.rs`, `src/engine/*` | `crates/rfq-domain/src/...` (same module names) |
| `crates/rfq-core/src/testkit.rs`, `src/scenarios.rs` | `crates/rfq-testkit/src/{lib.rs (Harness, accounts, genesis, helpers), scenarios.rs}` |
| `rfq_core::testkit::...` / `rfq_core::scenarios::...` | `rfq_testkit::...` / `rfq_testkit::scenarios::...` |
| `rfq_core::{Engine, Command, ...}` | `rfq_domain::{...}` |
| `crates/rfq-core/tests/*.rs`, `benches/apply.rs` | `crates/rfq-domain/tests/*.rs`, `crates/rfq-domain/benches/apply.rs` (with `rfq-testkit` as a dev-dependency; a dev-dependency cycle like tokio ↔ tokio-test is fine) |
| Task 8 `codec.rs` | `crates/rfq-infra/src/codec.rs`. `ClientCommand` and `EventBatch` live in `crates/rfq-app/src/protocol.rs` (rfq-infra depends on rfq-app) |
| Task 9 `rfq-node/src/{sequencer,node}.rs` | `crates/rfq-app/src/{sequencer,node}.rs`, plus `ports.rs` with the `Journal` and `Transport` traits and `error.rs` (`NodeError`, `BoxError`) |
| Task 9 `journal.rs` (`MemJournal`, `FileJournal`), `transport.rs` (`MemTransport`) | `crates/rfq-infra/src/{journal/mem.rs, journal/file.rs, transport/mem.rs}` |
| Task 9 `Transport` over bytes | `Transport` over typed values: `poll(&mut self, max, sink: &mut dyn FnMut(ClientCommand))`, `publish(&mut self, &EventBatch) -> Result<(), BoxError>`. Malformed bytes are dropped by the byte-level adapter via `rfq_infra::codec::decode_ingress(&[u8]) -> Option<ClientCommand>`. Tests `malformed_ingress_is_dropped` and `client_cannot_inject_system_commands` move to `rfq-infra` and assert on `decode_ingress` |
| Ports returning `anyhow::Result` | `Result<_, BoxError>`; `Node` returns `Result<_, NodeError>` (`thiserror`). `anyhow` only in `rfq-node`/`rfq-demo` binaries |
| Task 10 `rfq-node/src/aeron.rs` | `crates/rfq-infra/src/aeron/{mod,transport,archive}.rs` behind feature `aeron` |
| Task 10 `rfq-node/src/main.rs`, Task 11 `rfq-demo` | `crates/rfq-node/src/main.rs` (composition root), `crates/rfq-demo/src/main.rs` (both enable `rfq-infra/aeron`) |
| `edition = "2021"` | Workspace inheritance: `[workspace.package] edition = "2024"`, `rust-version = "1.99"`, `license = "MIT"`; members use `edition.workspace = true` etc. Add `rust-toolchain.toml` (`channel = "1.99.0"`, components `clippy`, `rustfmt`) and `rustfmt.toml` (`max_width = 110`) in Task 1 |
| Task 1 crate lints | The `#![forbid(unsafe_code)]` + `#![deny(...)]` header goes on `rfq-domain` **and** `rfq-app` (`rfq-infra` needs no unsafe either: forbid there too). |

## Review Focus

1. **Engine idle across several deadlines:** a command arrives long after `response_deadline + accept_window` with no `Tick` in between. Expect selection, then lapse, then the command, all in one `apply`, in deadline order. The test lives in Task 5 (`idle_engine_fires_deadline_then_lapse_in_one_command`).
2. **Malformed or oversized ingress bytes:** expect the message to be dropped while the node keeps running and the journal stays unchanged. Tests are in Task 8 (`decode_rejects_oversized`) and Task 9 (`malformed_ingress_is_dropped`).
3. **A client trying to inject `Tick`/`Resume`/`Genesis` bytes:** they must never reach the log. The test is in Task 9 (`client_cannot_inject_system_commands`).
4. **A torn last record in `FileJournal` after a crash:** expect replay to stop at the last good record, truncate the tail, and allow appends to continue. The test is in Task 9 (`file_journal_truncates_torn_tail`).
5. **Overflowing deposit amounts:** expect `CommandRejected(Overflow)` and no state change. The test is in Task 3 (`deposit_overflow_rejected`).

---

## File map

```
Cargo.toml                         workspace (+ release/bench overflow-checks)
clippy.toml                        allow-unwrap-in-tests
deny.toml                          cargo-deny: licences, advisories, sources
.github/workflows/ci.yml           fmt, clippy, test, bench build, cargo-deny
docs/reviews/spec-review.md        Gate A findings and decisions
docs/reviews/code-review.md        Task 14 findings and decisions
crates/rfq-core/benches/apply.rs   engine throughput + state_hash cost
crates/rfq-core/
  Cargo.toml
  src/lib.rs                       module wiring + re-exports
  src/types.rs                     Amount, Price, ids, Side, Outcome, buyer_lock/seller_lock
  src/ledger.rs                    Balance, Hold, Ledger (only code that moves money)
  src/command.rs                   Config, Genesis, LegSpec, Pick, Action, Command, SequencedCommand
  src/event.rs                     Audience, RejectReason, Event, Emitted
  src/model.rs                     Market/Request/Leg/Quote/Position + states, TimerKey
  src/engine/mod.rs                Engine struct, apply, timers, invariants, state_hash
  src/engine/account.rs            Deposit/Withdraw
  src/engine/rfq.rs                requests, quotes, selection, accept/reject
  src/engine/market.rs           list, close, propose, dispute, rule, finalize
  src/testkit.rs                   Harness + well-known accounts (used by tests and demo)
  src/scenarios.rs                 scripted scenarios (in-memory and over Aeron)
  src/codec.rs                     encode/decode, ClientCommand, EventBatch
  tests/engine_basics.rs
  tests/requests_quotes.rs
  tests/selection_accept.rs
  tests/resolution.rs
  tests/scenarios.rs
  tests/properties.rs
crates/rfq-node/
  Cargo.toml
  src/lib.rs
  src/sequencer.rs                 Sequencer (seq + monotonic time)
  src/journal.rs                   Journal trait, MemJournal, FileJournal
  src/transport.rs                 Transport trait, MemTransport
  src/node.rs                      Node: replay → live loop with write-ahead
  src/aeron.rs                     AeronTransport, ArchiveJournal (rusteron)
  src/main.rs                      rfq-node binary
  tests/node.rs
crates/rfq-demo/
  Cargo.toml
  src/main.rs                      runs scenarios over Aeron
docker/aeron/Dockerfile            Java ArchivingMediaDriver
docker/rust.Dockerfile             builds a Rust bin
docker-compose.yml
scripts/demo.sh                    scenarios + crash-recovery check
spike/                             Task 0 throwaway (deleted at end of Task 0)
```

---

## Execution order and parallelism

### Dependencies

| Task | Needs | Why |
|---|---|---|
| Setup | none | Creates the git repo and the private GitHub repo that every lane branches from |
| Gate A Adversarial spec review | Setup | Attacks the specs before any code exists; spec fixes land before Task 1 |
| 0 Aeron spike | Setup | Self-contained; only Tasks 10–11 use its output (pinned versions + Verified API) |
| 1 → 2 → 3 | Gate A, then previous | Types → ledger → engine skeleton |
| 4 → 5 → 6 → 7 | previous | All edit `engine/mod.rs` (dispatch + timer match) and `rfq.rs`/`market.rs`; Task 6 tests need positions from Task 5; Task 7 exercises everything |
| 8 Codec | 3 | Needs only `Command`, `Action`, `Emitted`, `testkit::accounts` |
| 9 Node core | 4, 8 | Uses the codec; `ticks_only_when_a_timer_is_due` lists a market (Task 4) |
| 10 Aeron adapters | 0, 7, 9 | Verified API, `scenarios::demo_config()`, node loop |
| 11 Demo + Docker | 10 | |
| 12 Docs sync | all | |
| 13 Quality gates (deny, bench, CI) | 7, 9 (12 for doc numbers) | Bench needs the engine; CI needs every crate to build |
| 14 Final review and publish | 13 | Reviews the whole branch, then shares the repo |

### Lanes

```
Setup → Gate A ─┬─ Lane A (engine, critical path): 1 → 2 → 3 → 4 → 5 → 6 → 7 ─┐
Setup ──────────┼─ Lane B: 0 (needs Docker Desktop running) ──────────────────┤
                └─ Lane C:                     (after 3) 8 → (after 4) 9 ──────┴→ 10 → 11 → 12 → 13 → 14
```
Lane B depends only on Setup, so it can run while Gate A is in progress.

- **Lane B starts immediately**, in parallel with Task 1. It's fully isolated and it's the biggest unknown. **Recommended.**
- **Lane C is optional.** Start Task 8 once Task 3 is merged, and Task 9 once Tasks 4 and 8 are merged. It saves less time and adds merges.
- Tasks 10 → 14 are sequential and start only when every lane is merged.
- **Never run Tasks 4, 5 and 6 concurrently.** They all edit the same `match` blocks in `engine/mod.rs`.

### Worktrees and merging

Each parallel lane runs in its own git worktree, branching from `main` *after* its prerequisites are merged:
```bash
git worktree add ../rfq-spike -b task-0-spike main        # lane B
git worktree add ../rfq-codec -b task-8-codec main        # lane C, after Task 3 is on main
```
Merge each lane into `main` when its task is done (`git merge --no-ff task-0-spike`), then remove the worktree (`git worktree remove ../rfq-spike`).

Expected conflicts are trivial: the `members` list in the root `Cargo.toml`, and `pub mod` lines in `crates/rfq-core/src/lib.rs`. Resolve by keeping both sides. Run `cargo test` on `main` after every merge.

---

## Tasks

Each task lives in its own file under [tasks/](tasks/). Run them in the order given by *Execution order and parallelism* above.

| # | Task | File |
|---|---|---|
| Setup | Git repository (before any task) | [tasks/00a-setup.md](tasks/00a-setup.md) |
| Gate A | Adversarial spec review (before Task 1) | [tasks/00b-gate-a-spec-review.md](tasks/00b-gate-a-spec-review.md) |
| 0 | Aeron spike (throwaway, gate) | [tasks/task-00-aeron-spike-throwaway-gate.md](tasks/task-00-aeron-spike-throwaway-gate.md) |
| 1 | Workspace and core types | [tasks/task-01-workspace-and-core-types.md](tasks/task-01-workspace-and-core-types.md) |
| 2 | Ledger | [tasks/task-02-ledger.md](tasks/task-02-ledger.md) |
| 3 | Commands, events, model and engine skeleton | [tasks/task-03-commands-events-model-and-engine-skeleton.md](tasks/task-03-commands-events-model-and-engine-skeleton.md) |
| 4 | Markets listing, requests and quotes | [tasks/task-04-markets-listing-requests-and-quotes.md](tasks/task-04-markets-listing-requests-and-quotes.md) |
| 5 | Selection, acceptance, rejection and races | [tasks/task-05-selection-acceptance-rejection-and-races.md](tasks/task-05-selection-acceptance-rejection-and-races.md) |
| 6 | Resolution and settlement | [tasks/task-06-resolution-and-settlement.md](tasks/task-06-resolution-and-settlement.md) |
| 7 | Scenarios, property test and replay determinism | [tasks/task-07-scenarios-property-test-and-replay-determinism.md](tasks/task-07-scenarios-property-test-and-replay-determinism.md) |
| 8 | Codec | [tasks/task-08-codec.md](tasks/task-08-codec.md) |
| 9 | Node core (sequencer, journals, transport, write-ahead loop) | [tasks/task-09-node-core-sequencer-journals-transport-write-ahead-loop.md](tasks/task-09-node-core-sequencer-journals-transport-write-ahead-loop.md) |
| 10 | Aeron adapters and the `rfq-node` binary | [tasks/task-10-aeron-adapters-and-the-rfq-node-binary.md](tasks/task-10-aeron-adapters-and-the-rfq-node-binary.md) |
| 11 | Demo client, Docker Compose and crash-recovery script | [tasks/task-11-demo-client-docker-compose-and-crash-recovery-script.md](tasks/task-11-demo-client-docker-compose-and-crash-recovery-script.md) |
| 12 | Docs sync and README | [tasks/task-12-docs-sync-and-readme.md](tasks/task-12-docs-sync-and-readme.md) |
| 13 | Quality gates (dependency audit, benchmark, CI) | [tasks/task-13-quality-gates-dependency-audit-benchmark-ci.md](tasks/task-13-quality-gates-dependency-audit-benchmark-ci.md) |
| 14 | Final review and publish | [tasks/task-14-final-review-and-publish.md](tasks/task-14-final-review-and-publish.md) |
