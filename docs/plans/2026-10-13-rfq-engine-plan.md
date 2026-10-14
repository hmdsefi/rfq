# RFQ Matching & Settlement Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a deterministic, replayable RFQ engine for binary-outcome contracts in Rust. Commands are sequenced and recorded through Aeron + Aeron Archive in Docker, with a demo that runs one happy path, five failure paths, and a crash-recovery replay.

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
- Default config: `accept_window = 30_000` ms, `min_response = 1_000`, `max_response = 604_800_000`, `max_qty = 1_000_000_000`, `dispute_bond = 100 × ONE_DOLLAR`.
- Aeron streams are all `aeron:ipc`: ingress 1001, log 1002, egress 1003, replay 1004. Never use stream 10, which is Archive's local control.
- Codec: a `u8` version prefix (`1`) followed by bincode (fixint, little-endian, 64 KiB limit).
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
3. **A client trying to inject `Tick`/`Genesis` bytes:** they must never reach the log. The test is in Task 9 (`client_cannot_inject_system_commands`).
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
  src/model.rs                     Contract/Request/Leg/Quote/Position + states, TimerKey
  src/engine/mod.rs                Engine struct, apply, timers, invariants, state_hash
  src/engine/account.rs            Deposit/Withdraw
  src/engine/rfq.rs                requests, quotes, selection, accept/reject
  src/engine/contract.rs           list, close, propose, dispute, rule, finalize
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
| 4 → 5 → 6 → 7 | previous | All edit `engine/mod.rs` (dispatch + timer match) and `rfq.rs`/`contract.rs`; Task 6 tests need positions from Task 5; Task 7 exercises everything |
| 8 Codec | 3 | Needs only `Command`, `Action`, `Emitted`, `testkit::accounts` |
| 9 Node core | 4, 8 | Uses the codec; `ticks_only_when_a_timer_is_due` lists a contract (Task 4) |
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

### Setup: Git repository (before any task)

**Files:**
- Create: `.gitignore`

- [ ] **Step 1: Initialise the repo and commit the design docs**

```bash
cd ~/projects/rfq
git init -b main
printf 'target/\n.DS_Store\n.helper/\n' > .gitignore   # .helper/ holds local Claude skills; never pushed
git add .gitignore docs
git commit -m "docs: RFC, architecture, specs and implementation plan"
```
Expected: `git log --oneline` shows one commit, and `git status` doesn't list `.helper/`.

- [ ] **Step 2: Create the private GitHub repo and push**

**Confirm with the user before running this step**, because it publishes to GitHub. The repo is **private**; it's shared with the reviewers only in Task 14.
```bash
gh repo create hmdsefi/rfq --private --source . --remote origin --push \
  --description "RFQ matching & settlement engine for binary-outcome contracts (take-home)"
gh repo view hmdsefi/rfq --json visibility --jq .visibility
```
Expected: `PRIVATE`.

---

### Gate A: Adversarial spec review (before Task 1)

**Goal:** find money-state bugs while they're still a one-line spec fix. A fresh reviewer attacks the design and doesn't edit any code.

**Files:**
- Create: `docs/reviews/spec-review.md`
- Modify: whichever of `docs/specs/*.md` and this plan the accepted findings touch

- [ ] **Step 1: Dispatch a fresh reviewer agent with this prompt**

```text
You are an adversarial reviewer for an RFQ settlement engine where real money sits in every
intermediate state and every participant is hostile. Read docs/RFQ_Technical_Exercise.pdf,
docs/rfc/0001-rfq-engine.md, docs/specs/engine.md, docs/specs/failure-modes.md and
docs/specs/resolution.md. Do not read or write code.

Attack the design as each of: a malicious requester, a malicious market maker, a colluding
requester+MM pair, a malicious oracle, a griefing third party, and a crashing node. For every
state and transition in engine.md §6, ask:
  1. Can funds be lost, duplicated, or stuck with no exit (check invariants I1–I5)?
  2. Can an actor extract value or impose cost on others more cheaply than it costs them?
  3. Is any transition reachable by the wrong actor, or reachable twice?
  4. Does any timer ordering (same timestamp, idle engine, replay) change an outcome?
  5. Is any requirement in the PDF unaddressed?

Write findings to docs/reviews/spec-review.md as a table:
| # | Severity (critical/high/medium/low) | Attacker | Scenario (concrete steps) | Violated property | Proposed spec fix |
Only include findings with a concrete step-by-step scenario. End with "No findings" sections
for anything you checked and found sound, so coverage is visible.
```

- [ ] **Step 2: Triage with the user**

For each finding, decide with the user: **fix** (edit the spec, add the test to the owning task in this plan, and add the defence to `failure-modes.md`), **accept** (record it as a residual risk in `failure-modes.md` → Adversarial table), or **reject** (record the reason in the review file).

- [ ] **Step 3: Commit and push**

```bash
git add docs
git commit -m "docs: adversarial spec review and resulting spec fixes"
git push
```

---

### Task 0: Aeron spike (throwaway, gate)

**Goal:** Prove these four things before committing to Aeron: `rusteron` builds in a Linux container on Apple Silicon; a Rust client and the Java `ArchivingMediaDriver` share `/dev/shm` across containers; Archive records an IPC stream; and the Rust archive client can replay it. **Output:** the verified API calls and pinned versions, written into `docs/specs/transport-aeron.md` → "Verified API". If any of the four fails after 60 minutes, stop and switch `ArchiveJournal` to `FileJournal` (Task 9) as the durable journal. Record that decision in the RFC risks table.

**Files:**
- Create: `spike/Cargo.toml`, `spike/src/main.rs`, `spike/docker-compose.yml`, `spike/Dockerfile`, `docker/aeron/Dockerfile`
- Modify: `docs/specs/transport-aeron.md` (append "Verified API")

- [ ] **Step 1: Start Docker Desktop and create the lane-B worktree**

```bash
open -a Docker
cd ~/projects/rfq && git worktree add ../rfq-spike -b task-0-spike main && cd ../rfq-spike
docker info --format '{{.OSType}}/{{.Architecture}}'
```
Run every remaining Task 0 step from `../rfq-spike`.
Expected: `linux/aarch64`

- [ ] **Step 2: Find the Aeron version bundled by rusteron**

```bash
cargo search rusteron-archive --limit 1
cargo info rusteron-archive
```
Note the crate version (e.g. `0.2.11`). In its repository, the `aeron` git submodule tag gives the Aeron C version (e.g. `1.48.x`). The Java jar must use the same `1.x` minor version.

- [ ] **Step 3: Write the Java archive image**

`docker/aeron/Dockerfile`:
```dockerfile
FROM eclipse-temurin:21-jre
ARG AERON_VERSION=1.48.5
ADD https://repo1.maven.org/maven2/io/aeron/aeron-all/${AERON_VERSION}/aeron-all-${AERON_VERSION}.jar /opt/aeron/aeron-all.jar
ENTRYPOINT ["java", "--add-opens", "java.base/jdk.internal.misc=ALL-UNNAMED", "--add-opens", "java.base/sun.nio.ch=ALL-UNNAMED", \
  "-Daeron.dir=/dev/shm/aeron", "-Daeron.dir.delete.on.start=true", \
  "-Daeron.archive.dir=/data/archive", \
  "-Daeron.archive.control.channel=aeron:udp?endpoint=localhost:8010", \
  "-Daeron.archive.replication.channel=aeron:udp?endpoint=localhost:0", \
  "-Daeron.archive.recording.events.enabled=false", \
  "-cp", "/opt/aeron/aeron-all.jar", "io.aeron.archive.ArchivingMediaDriver"]
```
Set `AERON_VERSION` to the version found in Step 2.

- [ ] **Step 4: Write the spike client**

`spike/Cargo.toml`:
```toml
[package]
name = "spike"
version = "0.0.0"
edition = "2021"

[dependencies]
rusteron-archive = { version = "=0.2.11", features = ["static", "precompile"] }
```

`spike/src/main.rs`:
```rust
use rusteron_archive::*;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ctx = AeronContext::new()?;
    ctx.set_dir(&"/dev/shm/aeron".into_c_string())?;
    let aeron = Aeron::new(&ctx)?;
    aeron.start()?;

    let channel = "aeron:ipc".into_c_string();
    let archive_ctx = AeronArchiveContext::new_with_no_credentials_supplier(
        &aeron,
        "aeron:ipc".into_c_string().as_c_str(),            // control request (local)
        "aeron:ipc".into_c_string().as_c_str(),            // control response
        "aeron:udp?endpoint=localhost:8011".into_c_string().as_c_str(), // recording events (unused)
    )?;
    let archive = AeronArchiveAsyncConnect::new_with_aeron(&archive_ctx, &aeron)?
        .poll_blocking(Duration::from_secs(10))?;

    archive.start_recording(&channel, 1002, SOURCE_LOCATION_LOCAL, true)?;
    let publication = aeron
        .async_add_publication(&channel, 1002)?
        .poll_blocking(Duration::from_secs(5))?;
    let mut last = 0;
    for i in 0..3u8 {
        loop {
            let r = publication.offer(&[i; 16], Handlers::no_reserved_value_supplier_handler());
            if r > 0 { last = r; break; }
        }
    }
    println!("published up to position {last}");

    let mut found = Vec::new();
    archive.list_recordings_for_uri(0, i32::MAX, &channel, 1002, |d: AeronArchiveRecordingDescriptor| {
        found.push(d.recording_id());
    })?;
    let recording_id = *found.last().expect("a recording exists");
    while archive.get_recording_position(recording_id)? < last {
        std::thread::sleep(Duration::from_millis(1));
    }
    println!("recording {recording_id} durable up to {last}");

    let replay_sub = aeron
        .async_add_subscription(&channel, 1004, Handlers::no_available_image_handler(), Handlers::no_unavailable_image_handler())?
        .poll_blocking(Duration::from_secs(5))?;
    let params = AeronArchiveReplayParams::new(0, i32::MAX, 0, last, 0, 0)?;
    archive.start_replay(recording_id, &channel, 1004, &params)?;
    let mut got = 0;
    while got < 3 {
        got += replay_sub.poll_once(|msg: &[u8], _h: AeronHeader| println!("replayed {:?}", &msg[..2]), 10)?;
    }
    println!("SPIKE OK");
    Ok(())
}
```
These calls are a best guess at the `rusteron` API. Fix any compile errors against `docs.rs/rusteron-archive` and its `examples/record_and_replay.rs`. **Whatever compiles and runs here is the API that Task 10 uses.**

- [ ] **Step 5: Compose and run**

`spike/Dockerfile`:
```dockerfile
FROM rust:1-bookworm
WORKDIR /spike
COPY . .
RUN cargo build --release
CMD ["/spike/target/release/spike"]
```
`spike/docker-compose.yml`:
```yaml
services:
  aeron:
    build: ../docker/aeron
    ipc: shareable
    shm_size: 512m
  spike:
    build: .
    ipc: "service:aeron"
    depends_on: [aeron]
```
```bash
cd spike && docker compose up --build --abort-on-container-exit spike
```
Expected: the output ends with `SPIKE OK`. If `precompile` fails to download prebuilt libraries for aarch64 Linux, add `RUN apt-get update && apt-get install -y cmake clang default-jdk-headless` to `spike/Dockerfile`, drop the `precompile` feature, and retry.

- [ ] **Step 6: Record the verified API and versions**

Append a section to `docs/specs/transport-aeron.md`:
```markdown
## Verified API (Task 0)
- rusteron-archive = <version>, Aeron C <version>; aeron-all jar <version>
- Context/connect: <exact calls that worked>
- Publish: <call + return type>; Subscribe/poll: <call + closure signature>
- Recording: start_recording(...), list_recordings_for_uri(...), get_recording_position(...)
- Replay: <params constructor + start_replay call>
- Build: features <...>; extra apt packages: <none | list>
```

- [ ] **Step 7: Delete the spike, commit the findings, merge lane B**

```bash
rm -rf spike
git add docs/specs/transport-aeron.md docker/aeron/Dockerfile
git commit -m "spike: pin Aeron versions and record verified rusteron API"
cd ~/projects/rfq && git merge --no-ff task-0-spike && git worktree remove ../rfq-spike
```

---

### Task 1: Workspace and core types

**Files:**
- Create: `Cargo.toml`, `crates/rfq-core/Cargo.toml`, `crates/rfq-core/src/lib.rs`, `crates/rfq-core/src/types.rs`

**Interfaces:**
- Produces: `Amount`, `Qty`, `Timestamp`, `Duration` (all `u64`), `ONE_DOLLAR`, ids `AccountId/ContractId/RequestId/QuoteId/PositionId/HoldId(pub u64)`, `Price::new(u16) -> Option<Price>`, `Price::bps()`, `Price::complement()`, `Side::{BuyYes, BuyNo}`, `Outcome::{Yes, No, Invalid}`, `buyer_lock(Qty, Price) -> Option<Amount>`, `seller_lock(Qty, Price) -> Option<Amount>`.

- [ ] **Step 1: Create the workspace**

`Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["crates/rfq-core"]

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
bincode = "1.3"
sha2 = "0.10"
proptest = "1"
```
`crates/rfq-core/Cargo.toml`:
```toml
[package]
name = "rfq-core"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { workspace = true }
bincode = { workspace = true }
sha2 = { workspace = true }

[dev-dependencies]
proptest = { workspace = true }
```
Append to the root `Cargo.toml`:
```toml
# Unchecked overflow must never silently wrap money, even in optimised builds.
[profile.release]
overflow-checks = true

[profile.bench]
overflow-checks = true
```
`clippy.toml` (workspace root):
```toml
allow-unwrap-in-tests = true
```
`crates/rfq-core/src/lib.rs`:
```rust
//! Deterministic RFQ engine: no I/O, no clock, no randomness.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod types;

pub use types::*;
```
Later tasks rewrite `lib.rs` with more modules. **Keep these attributes at the top every time.**

- [ ] **Step 2: Write the failing tests**

`crates/rfq-core/src/types.rs`: start with only the test module.
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_bounds() {
        assert!(Price::new(0).is_none());
        assert!(Price::new(10_000).is_none());
        assert_eq!(Price::new(1).unwrap().bps(), 1);
        assert_eq!(Price::new(9_999).unwrap().bps(), 9_999);
    }

    #[test]
    fn locks_sum_to_one_dollar_per_share() {
        for bps in [1u16, 37, 5_000, 6_000, 9_999] {
            let p = Price::new(bps).unwrap();
            assert_eq!(buyer_lock(1_000, p).unwrap() + seller_lock(1_000, p).unwrap(), 1_000 * ONE_DOLLAR);
        }
    }

    #[test]
    fn spec_example_60_cents() {
        let p = Price::new(6_000).unwrap();
        assert_eq!(buyer_lock(1_000, p), Some(600 * ONE_DOLLAR));
        assert_eq!(seller_lock(1_000, p), Some(400 * ONE_DOLLAR));
    }

    #[test]
    fn overflow_is_none() {
        assert_eq!(buyer_lock(u64::MAX, Price::new(2).unwrap()), None);
    }

    #[test]
    fn deserializing_out_of_range_price_fails() {
        let bytes = bincode::serialize(&0u16).unwrap();
        assert!(bincode::deserialize::<Price>(&bytes).is_err());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p rfq-core`
Expected: FAIL to compile (`Price`, `buyer_lock` not found).

- [ ] **Step 4: Implement the types**

Prepend to `crates/rfq-core/src/types.rs`:
```rust
use serde::{Deserialize, Serialize};

/// Money in micro-USD. `$1 = 1_000_000`.
pub type Amount = u64;
/// Number of shares; each share pays $1 to the winning side.
pub type Qty = u64;
/// Milliseconds since the Unix epoch. Only the sequencer stamps time.
pub type Timestamp = u64;
/// Milliseconds.
pub type Duration = u64;

pub const ONE_DOLLAR: Amount = 1_000_000;
const MICROS_PER_BP: Amount = 100;

macro_rules! id_type {
    ($($name:ident),*) => {$(
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub u64);
    )*};
}
id_type!(AccountId, ContractId, RequestId, QuoteId, PositionId, HoldId);

/// Price of the side the requester buys, in basis points of $1, strictly inside (0, 10_000).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct Price(u16);

impl Price {
    pub const fn new(bps: u16) -> Option<Price> {
        if bps >= 1 && bps <= 9_999 { Some(Price(bps)) } else { None }
    }
    pub fn bps(self) -> u16 { self.0 }
    /// Price of the opposite side: `10_000 - bps`.
    pub fn complement(self) -> Price { Price(10_000 - self.0) }
}

impl TryFrom<u16> for Price {
    type Error = &'static str;
    fn try_from(bps: u16) -> Result<Self, Self::Error> {
        Price::new(bps).ok_or("price must be within 1..=9999 bps")
    }
}

impl From<Price> for u16 {
    fn from(p: Price) -> u16 { p.0 }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Side { BuyYes, BuyNo }

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Outcome { Yes, No, Invalid }

/// Collateral the buyer of `qty` shares at `price` locks: `qty × bps × 100` micro-USD.
pub fn buyer_lock(qty: Qty, price: Price) -> Option<Amount> {
    qty.checked_mul(u64::from(price.0))?.checked_mul(MICROS_PER_BP)
}

/// Collateral the counterparty locks: the buyer lock at the complement price.
pub fn seller_lock(qty: Qty, price: Price) -> Option<Amount> {
    buyer_lock(qty, price.complement())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rfq-core && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`
Expected: 5 passed; clippy and fmt are clean.

- [ ] **Step 6: Commit and push**

```bash
git add Cargo.toml clippy.toml crates/rfq-core
git commit -m "feat(core): money, price and id types with exact collateral math; strict lints"
git push
```

---

### Task 2: Ledger

**Files:**
- Create: `crates/rfq-core/src/ledger.rs`
- Modify: `crates/rfq-core/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 types.
- Produces: `Balance { free, reserved, escrowed }`, `Ledger` with `balance(AccountId) -> Balance`, `accounts() -> impl Iterator<Item=(AccountId, Balance)>`, `hold_amount(HoldId) -> Option<Amount>`, `hold_ids() -> impl Iterator<Item=HoldId>`, `deposit`, `withdraw`, `hold -> Result<HoldId>`, `release -> Result<Amount>`, `escrow_from_hold(HoldId, Amount)`, `pay_from_escrow(from, Amount, to)`, `forfeit(HoldId, to) -> Result<Amount>`, `check() -> Result<(), String>`; `LedgerError::{InsufficientFunds, UnknownHold, Overflow}`.

- [ ] **Step 1: Write the failing tests**

`crates/rfq-core/src/ledger.rs`: test module only.
```rust
#[cfg(test)]
mod tests {
    use super::*;
    const A: AccountId = AccountId(1);
    const B: AccountId = AccountId(2);

    fn funded(x: Amount) -> Ledger {
        let mut l = Ledger::default();
        l.deposit(A, x).unwrap();
        l
    }

    #[test]
    fn hold_moves_free_to_reserved() {
        let mut l = funded(100);
        let h = l.hold(A, 60).unwrap();
        assert_eq!(l.balance(A), Balance { free: 40, reserved: 60, escrowed: 0 });
        assert_eq!(l.hold_amount(h), Some(60));
        l.check().unwrap();
    }

    #[test]
    fn hold_more_than_free_fails_without_mutation() {
        let mut l = funded(100);
        assert_eq!(l.hold(A, 101), Err(LedgerError::InsufficientFunds));
        assert_eq!(l.hold(B, 0).map(|_| ()), Err(LedgerError::InsufficientFunds));
        assert_eq!(l.balance(A).free, 100);
        assert_eq!(l.accounts().count(), 1, "failed ops must not create accounts");
        l.check().unwrap();
    }

    #[test]
    fn release_returns_whole_hold_to_free() {
        let mut l = funded(100);
        let h = l.hold(A, 60).unwrap();
        assert_eq!(l.release(h), Ok(60));
        assert_eq!(l.balance(A), Balance { free: 100, reserved: 0, escrowed: 0 });
        assert_eq!(l.release(h), Err(LedgerError::UnknownHold));
        l.check().unwrap();
    }

    #[test]
    fn escrow_from_hold_splits_and_consumes_hold() {
        let mut l = funded(100);
        let h = l.hold(A, 70).unwrap();
        l.escrow_from_hold(h, 60).unwrap();
        assert_eq!(l.balance(A), Balance { free: 40, reserved: 0, escrowed: 60 });
        assert_eq!(l.hold_amount(h), None);
        l.check().unwrap();
    }

    #[test]
    fn escrow_more_than_hold_fails() {
        let mut l = funded(100);
        let h = l.hold(A, 50).unwrap();
        assert_eq!(l.escrow_from_hold(h, 51), Err(LedgerError::InsufficientFunds));
        assert_eq!(l.hold_amount(h), Some(50));
    }

    #[test]
    fn pay_from_escrow_moves_to_winner() {
        let mut l = funded(100);
        let h = l.hold(A, 100).unwrap();
        l.escrow_from_hold(h, 100).unwrap();
        l.pay_from_escrow(A, 100, B).unwrap();
        assert_eq!(l.balance(A), Balance::default());
        assert_eq!(l.balance(B).free, 100);
        l.check().unwrap();
    }

    #[test]
    fn forfeit_moves_hold_to_other_account() {
        let mut l = funded(100);
        let h = l.hold(A, 30).unwrap();
        assert_eq!(l.forfeit(h, B), Ok(30));
        assert_eq!(l.balance(A).free, 70);
        assert_eq!(l.balance(B).free, 30);
        l.check().unwrap();
    }

    #[test]
    fn withdraw_only_from_free() {
        let mut l = funded(100);
        l.hold(A, 80).unwrap();
        assert_eq!(l.withdraw(A, 21), Err(LedgerError::InsufficientFunds));
        l.withdraw(A, 20).unwrap();
        assert_eq!(l.balance(A).free, 0);
        l.check().unwrap();
    }

    #[test]
    fn deposit_overflow_fails() {
        let mut l = funded(u64::MAX);
        assert_eq!(l.deposit(B, 1), Err(LedgerError::Overflow));
        assert_eq!(l.balance(B), Balance::default());
        l.check().unwrap();
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Add `pub mod ledger;` and `pub use ledger::*;` to `lib.rs`.
Run: `cargo test -p rfq-core ledger`
Expected: FAIL to compile (`Ledger` not found).

- [ ] **Step 3: Implement the ledger**

Prepend to `crates/rfq-core/src/ledger.rs`:
```rust
//! The only code that moves money. Every account's funds are split into
//! free / reserved (backing a live request, quote or bond) / escrowed (backing a position).
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{AccountId, Amount, HoldId};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    pub free: Amount,
    pub reserved: Amount,
    pub escrowed: Amount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub account: AccountId,
    pub amount: Amount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerError {
    InsufficientFunds,
    UnknownHold,
    Overflow,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    balances: BTreeMap<AccountId, Balance>,
    holds: BTreeMap<HoldId, Hold>,
    next_hold: u64,
    deposited: Amount,
    withdrawn: Amount,
}

impl Ledger {
    pub fn balance(&self, a: AccountId) -> Balance {
        self.balances.get(&a).copied().unwrap_or_default()
    }

    pub fn accounts(&self) -> impl Iterator<Item = (AccountId, Balance)> + '_ {
        self.balances.iter().map(|(a, b)| (*a, *b))
    }

    pub fn hold_amount(&self, h: HoldId) -> Option<Amount> {
        self.holds.get(&h).map(|h| h.amount)
    }

    pub fn hold_ids(&self) -> impl Iterator<Item = HoldId> + '_ {
        self.holds.keys().copied()
    }

    pub fn deposit(&mut self, a: AccountId, x: Amount) -> Result<(), LedgerError> {
        // free ≤ deposited, so if the total doesn't overflow neither can the account.
        let deposited = self.deposited.checked_add(x).ok_or(LedgerError::Overflow)?;
        self.deposited = deposited;
        self.balances.entry(a).or_default().free += x;
        Ok(())
    }

    pub fn withdraw(&mut self, a: AccountId, x: Amount) -> Result<(), LedgerError> {
        if self.balance(a).free < x {
            return Err(LedgerError::InsufficientFunds);
        }
        self.balances.get_mut(&a).expect("checked above").free -= x;
        self.withdrawn += x;
        Ok(())
    }

    /// free → reserved. Returns the hold that now owns the funds.
    pub fn hold(&mut self, a: AccountId, x: Amount) -> Result<HoldId, LedgerError> {
        if !self.balances.contains_key(&a) || self.balance(a).free < x {
            return Err(LedgerError::InsufficientFunds);
        }
        let b = self.balances.get_mut(&a).expect("checked above");
        b.free -= x;
        b.reserved += x;
        self.next_hold += 1;
        let id = HoldId(self.next_hold);
        self.holds.insert(id, Hold { account: a, amount: x });
        Ok(id)
    }

    /// reserved → free for the whole hold.
    pub fn release(&mut self, h: HoldId) -> Result<Amount, LedgerError> {
        let hold = self.holds.remove(&h).ok_or(LedgerError::UnknownHold)?;
        let b = self.balances.get_mut(&hold.account).expect("hold owner has a balance");
        b.reserved -= hold.amount;
        b.free += hold.amount;
        Ok(hold.amount)
    }

    /// Moves `x` of the hold into escrow and releases the remainder. Consumes the hold.
    pub fn escrow_from_hold(&mut self, h: HoldId, x: Amount) -> Result<(), LedgerError> {
        let hold = *self.holds.get(&h).ok_or(LedgerError::UnknownHold)?;
        if x > hold.amount {
            return Err(LedgerError::InsufficientFunds);
        }
        self.holds.remove(&h);
        let b = self.balances.get_mut(&hold.account).expect("hold owner has a balance");
        b.reserved -= hold.amount;
        b.escrowed += x;
        b.free += hold.amount - x;
        Ok(())
    }

    /// escrowed(from) → free(to). Used at settlement.
    pub fn pay_from_escrow(&mut self, from: AccountId, x: Amount, to: AccountId) -> Result<(), LedgerError> {
        if self.balance(from).escrowed < x {
            return Err(LedgerError::InsufficientFunds);
        }
        self.balances.get_mut(&from).expect("checked above").escrowed -= x;
        self.balances.entry(to).or_default().free += x;
        Ok(())
    }

    /// reserved(owner) → free(to) for the whole hold. Used to forfeit a dispute bond.
    pub fn forfeit(&mut self, h: HoldId, to: AccountId) -> Result<Amount, LedgerError> {
        let hold = self.holds.remove(&h).ok_or(LedgerError::UnknownHold)?;
        self.balances.get_mut(&hold.account).expect("hold owner has a balance").reserved -= hold.amount;
        self.balances.entry(to).or_default().free += hold.amount;
        Ok(hold.amount)
    }

    /// I1 conservation and I2 hold accounting.
    pub fn check(&self) -> Result<(), String> {
        let total: u128 = self
            .balances
            .values()
            .map(|b| b.free as u128 + b.reserved as u128 + b.escrowed as u128)
            .sum();
        let expected = self.deposited as u128 - self.withdrawn as u128;
        if total != expected {
            return Err(format!("I1 conservation: total {total} != deposited - withdrawn {expected}"));
        }
        let mut held: BTreeMap<AccountId, Amount> = BTreeMap::new();
        for h in self.holds.values() {
            *held.entry(h.account).or_default() += h.amount;
        }
        for (a, b) in &self.balances {
            let h = held.remove(a).unwrap_or(0);
            if b.reserved != h {
                return Err(format!("I2 holds: {a:?} reserved {} != live holds {h}", b.reserved));
            }
        }
        if let Some(a) = held.keys().next() {
            return Err(format!("I2 holds: hold for unknown account {a:?}"));
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p rfq-core ledger`
Expected: 9 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/rfq-core
git commit -m "feat(core): ledger with free/reserved/escrowed buckets and conservation check"
```

---

### Task 3: Commands, events, model and engine skeleton

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

---

### Task 4: Contracts listing, requests and quotes

**Files:**
- Create: `crates/rfq-core/src/engine/contract.rs`, `crates/rfq-core/src/engine/rfq.rs`, `crates/rfq-core/tests/requests_quotes.rs`
- Modify: `crates/rfq-core/src/engine/mod.rs` (modules, dispatch, timer match)

**Interfaces:**
- Consumes: Task 3 types and `Engine` internals (`arm`, `config`, `genesis`, `VALIDATED`, `Out`, `Handled`).
- Produces (crate-internal, used by Tasks 5–6): `Engine::close_request(RequestId, RequestState, &mut Out)` and `Engine::end_quote(QuoteId, QuoteState, &mut Out)`; handlers `list_contract`, `on_contract_close`, `submit_request`, `cancel_request`, `submit_quote`, `cancel_quote` (Active path), `on_quote_expiry` (Active path).

- [ ] **Step 1: Write the failing tests**

`crates/rfq-core/tests/requests_quotes.rs`:
```rust
use rfq_core::testkit::{accounts::*, *};
use rfq_core::*;

pub const CLOSE: Timestamp = 1_000_000;
pub const HARD: Timestamp = 2_000_000;
pub const DEADLINE: Timestamp = 10_000;

/// Funded accounts and one listed contract.
fn setup() -> (Harness, ContractId) {
    let mut h = Harness::new(Config::default());
    for a in [ALICE, MM1, MM2, CAROL] {
        h.fund(a, 1_000);
    }
    let c = ContractId(1);
    let out = h.user(OPERATOR, Action::ListContract {
        id: c,
        description: "Will it rain in Paris on 2026-11-01?".into(),
        oracle: ORACLE,
        trading_close: CLOSE,
        resolution_deadline: HARD,
        dispute_window: 10_000,
    });
    assert_eq!(rejection(&out), None);
    (h, c)
}

fn leg(c: ContractId, qty: Qty, limit: u16) -> LegSpec {
    LegSpec { contract: c, side: Side::BuyYes, qty, limit_price: price(limit) }
}

fn request(h: &mut Harness, c: ContractId) -> RequestId {
    let id = RequestId(h.next_seq());
    let out = h.user(ALICE, Action::SubmitRequest { legs: vec![leg(c, 1_000, 7_000)], response_deadline: DEADLINE });
    assert_eq!(rejection(&out), None);
    id
}

fn quote(h: &mut Harness, mm: AccountId, r: RequestId, bps: u16) -> QuoteId {
    let id = QuoteId(h.next_seq());
    let out = h.user(mm, Action::SubmitQuote { request: r, leg: 0, price: price(bps), size: 1_000, expires_at: 100_000 });
    assert_eq!(rejection(&out), None);
    id
}

#[test]
fn list_contract_requires_operator_and_valid_times() {
    let (mut h, _) = setup();
    let list = |h: &mut Harness, actor, id, close, hard| {
        h.user(actor, Action::ListContract { id: ContractId(id), description: "x".into(), oracle: ORACLE, trading_close: close, resolution_deadline: hard, dispute_window: 1 })
    };
    assert_eq!(rejection(&list(&mut h, ALICE, 2, CLOSE, HARD)), Some(RejectReason::Unauthorized));
    assert_eq!(rejection(&list(&mut h, OPERATOR, 1, CLOSE, HARD)), Some(RejectReason::InvalidInput));
    assert_eq!(rejection(&list(&mut h, OPERATOR, 3, HARD, CLOSE)), Some(RejectReason::InvalidInput));
}

#[test]
fn submit_request_holds_qty_times_limit() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    assert_eq!(h.engine.balance(ALICE), Balance { free: dollars(300), reserved: dollars(700), escrowed: 0 });
    assert_eq!(h.engine.request(r).unwrap().state, RequestState::Open);
}

#[test]
fn submit_request_validation() {
    let (mut h, c) = setup();
    let mut submit = |legs: Vec<LegSpec>, deadline| rejection(&h.user(ALICE, Action::SubmitRequest { legs, response_deadline: deadline }));
    assert_eq!(submit(vec![leg(c, 1, 7_000), leg(c, 1, 7_000)], DEADLINE), Some(RejectReason::Unsupported));
    assert_eq!(submit(vec![leg(c, 1, 7_000)], 500), Some(RejectReason::InvalidInput), "deadline sooner than min_response");
    assert_eq!(submit(vec![leg(c, 1, 7_000)], CLOSE - 1), Some(RejectReason::InvalidInput), "deadline + window after close");
    assert_eq!(submit(vec![leg(ContractId(9), 1, 7_000)], DEADLINE), Some(RejectReason::UnknownObject));
    assert_eq!(submit(vec![leg(c, 0, 7_000)], DEADLINE), Some(RejectReason::InvalidInput));
    assert_eq!(submit(vec![leg(c, 1_000_000_001, 7_000)], DEADLINE), Some(RejectReason::InvalidInput));
    assert_eq!(submit(vec![leg(c, 2_000, 7_000)], DEADLINE), Some(RejectReason::InsufficientFunds));
}

#[test]
fn quote_holds_complement_and_stays_private() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let out = h.user(MM1, Action::SubmitQuote { request: r, leg: 0, price: price(6_000), size: 1_000, expires_at: 100_000 });
    assert_eq!(h.engine.balance(MM1).reserved, dollars(400));
    assert!(out.iter().all(|e| e.audience == Audience::Account(MM1)), "quotes are sealed");
}

#[test]
fn quote_validation() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let mut q = |actor, bps, size, exp| rejection(&h.user(actor, Action::SubmitQuote { request: r, leg: 0, price: price(bps), size, expires_at: exp }));
    assert_eq!(q(ALICE, 6_000, 1_000, 100_000), Some(RejectReason::SelfTrade));
    assert_eq!(q(MM1, 7_001, 1_000, 100_000), Some(RejectReason::InvalidInput), "above requester limit");
    assert_eq!(q(MM1, 6_000, 999, 100_000), Some(RejectReason::InvalidInput), "size below leg qty");
    assert_eq!(q(MM1, 6_000, 1_000, 0), Some(RejectReason::InvalidInput), "already expired");
    let out = h.user(MM1, Action::SubmitQuote { request: r, leg: 1, price: price(6_000), size: 1_000, expires_at: 100_000 });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidInput), "unknown leg");
}

#[test]
fn quote_after_deadline_rejected() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    h.set_time(DEADLINE);
    let out = h.user(MM1, Action::SubmitQuote { request: r, leg: 0, price: price(6_000), size: 1_000, expires_at: 100_000 });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(h.engine.balance(MM1).reserved, 0);
}

#[test]
fn replace_quote_atomic() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let q1 = quote(&mut h, MM1, r, 6_000); // holds 400
    // MM1 now has 600 free; a 3000-bps quote needs 700 — only possible net of the old hold.
    let q2 = quote(&mut h, MM1, r, 3_000);
    assert_eq!(h.engine.quote(q1).unwrap().state, QuoteState::Replaced);
    assert_eq!(h.engine.quote(q2).unwrap().state, QuoteState::Active);
    assert_eq!(h.engine.balance(MM1).reserved, dollars(700));
    assert_eq!(h.engine.request(r).unwrap().legs[0].quotes.len(), 1);
}

#[test]
fn cancel_quote_releases() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let q = quote(&mut h, MM1, r, 6_000);
    assert_eq!(rejection(&h.user(MM2, Action::CancelQuote { quote: q })), Some(RejectReason::Unauthorized));
    assert_eq!(rejection(&h.user(MM1, Action::CancelQuote { quote: q })), None);
    assert_eq!(h.engine.balance(MM1).free, dollars(1_000));
    assert_eq!(rejection(&h.user(MM1, Action::CancelQuote { quote: q })), Some(RejectReason::InvalidState));
}

#[test]
fn quote_expiry_releases_hold() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let id = QuoteId(h.next_seq());
    h.user(MM1, Action::SubmitQuote { request: r, leg: 0, price: price(6_000), size: 1_000, expires_at: 5_000 });
    let out = h.tick(5_000);
    assert_eq!(names(&out), vec!["QuoteExpired"]);
    assert_eq!(h.engine.quote(id).unwrap().state, QuoteState::Expired);
    assert_eq!(h.engine.balance(MM1).free, dollars(1_000));
}

#[test]
fn cancel_request_releases_everything() {
    let (mut h, c) = setup();
    let r = request(&mut h, c);
    let q = quote(&mut h, MM1, r, 6_000);
    assert_eq!(rejection(&h.user(MM1, Action::CancelRequest { request: r })), Some(RejectReason::Unauthorized));
    let out = h.user(ALICE, Action::CancelRequest { request: r });
    assert_eq!(names(&out), vec!["QuoteLost", "RequestCancelled"]);
    assert_eq!(h.engine.balance(ALICE).free, dollars(1_000));
    assert_eq!(h.engine.balance(MM1).free, dollars(1_000));
    assert_eq!(h.engine.quote(q).unwrap().state, QuoteState::Lost);
}

#[test]
fn withdraw_cannot_touch_reserved() {
    let (mut h, c) = setup();
    request(&mut h, c);
    let out = h.user(ALICE, Action::Withdraw { amount: dollars(301) });
    assert_eq!(rejection(&out), Some(RejectReason::InsufficientFunds));
}

#[test]
fn contract_closes_at_trading_close() {
    let (mut h, c) = setup();
    let out = h.tick(CLOSE);
    assert_eq!(names(&out), vec!["ContractClosed"]);
    assert_eq!(h.engine.contract(c).unwrap().state, ContractState::Closed);
    let out = h.user(ALICE, Action::SubmitRequest { legs: vec![leg(c, 1, 7_000)], response_deadline: CLOSE + 10_000 });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rfq-core --test requests_quotes`
Expected: FAIL. Most tests get `Unsupported` rejections because the handlers don't exist yet.

- [ ] **Step 3: Implement contract listing and close**

`crates/rfq-core/src/engine/contract.rs`:
```rust
use super::{Engine, Handled, Out};
use crate::event::{Emitted, Event, RejectReason};
use crate::model::*;
use crate::types::*;

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn list_contract(
        &mut self,
        actor: AccountId,
        id: ContractId,
        description: &str,
        oracle: AccountId,
        trading_close: Timestamp,
        resolution_deadline: Timestamp,
        dispute_window: Duration,
        out: &mut Out,
    ) -> Handled {
        if actor != self.genesis().operator {
            return Err(RejectReason::Unauthorized);
        }
        if self.contracts.contains_key(&id) {
            return Err(RejectReason::InvalidInput);
        }
        if !(self.now < trading_close && trading_close < resolution_deadline) || dispute_window == 0 {
            return Err(RejectReason::InvalidInput);
        }
        self.contracts.insert(id, Contract {
            id,
            description: description.to_owned(),
            oracle,
            trading_close,
            resolution_deadline,
            dispute_window,
            state: ContractState::Trading,
            dispute: None,
            positions: Vec::new(),
        });
        self.arm(trading_close, TimerKey::ContractClose(id));
        self.arm(resolution_deadline, TimerKey::HardDeadline(id));
        out.push(Emitted::public(Event::ContractListed { contract: id }));
        Ok(())
    }

    pub(super) fn on_contract_close(&mut self, id: ContractId, out: &mut Out) {
        let Some(c) = self.contracts.get_mut(&id) else { return };
        if c.state != ContractState::Trading {
            return;
        }
        c.state = ContractState::Closed;
        out.push(Emitted::public(Event::ContractClosed { contract: id }));
    }
}
```

- [ ] **Step 4: Implement requests and quotes**

`crates/rfq-core/src/engine/rfq.rs`:
```rust
use std::collections::BTreeSet;

use super::{Engine, Handled, Out, VALIDATED};
use crate::command::LegSpec;
use crate::event::{Emitted, Event, RejectReason};
use crate::model::*;
use crate::types::*;

impl Engine {
    pub(super) fn submit_request(&mut self, actor: AccountId, legs: &[LegSpec], response_deadline: Timestamp, out: &mut Out) -> Handled {
        let cfg = self.config();
        if legs.len() != 1 {
            return Err(RejectReason::Unsupported); // multi-leg is designed, not built (RFC P6)
        }
        let earliest = self.now.checked_add(cfg.min_response).ok_or(RejectReason::Overflow)?;
        let latest = self.now.saturating_add(cfg.max_response);
        if response_deadline < earliest || response_deadline > latest {
            return Err(RejectReason::InvalidInput);
        }
        let last_accept = response_deadline.checked_add(cfg.accept_window).ok_or(RejectReason::Overflow)?;
        let mut total: Amount = 0;
        for l in legs {
            let c = self.contracts.get(&l.contract).ok_or(RejectReason::UnknownObject)?;
            if c.state != ContractState::Trading {
                return Err(RejectReason::InvalidState);
            }
            if last_accept >= c.trading_close {
                return Err(RejectReason::InvalidInput);
            }
            if l.qty == 0 || l.qty > cfg.max_qty {
                return Err(RejectReason::InvalidInput);
            }
            let lock = buyer_lock(l.qty, l.limit_price).ok_or(RejectReason::Overflow)?;
            total = total.checked_add(lock).ok_or(RejectReason::Overflow)?;
        }
        if self.ledger.balance(actor).free < total {
            return Err(RejectReason::InsufficientFunds);
        }

        let id = RequestId(self.last_seq);
        let hold = self.ledger.hold(actor, total).expect(VALIDATED);
        self.requests.insert(id, Request {
            id,
            requester: actor,
            legs: legs
                .iter()
                .map(|l| Leg { contract: l.contract, side: l.side, qty: l.qty, limit_price: l.limit_price, quotes: BTreeSet::new(), selected: None })
                .collect(),
            response_deadline,
            hold: Some(hold),
            state: RequestState::Open,
        });
        self.arm(response_deadline, TimerKey::RequestDeadline(id));
        for (i, l) in legs.iter().enumerate() {
            out.push(Emitted::public(Event::RequestOpened {
                request: id,
                leg: u32::try_from(i).expect("leg count is bounded"),
                contract: l.contract,
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
        let r = self.requests.get(&request).ok_or(RejectReason::UnknownObject)?;
        if r.state != RequestState::Open || self.now >= r.response_deadline {
            return Err(RejectReason::InvalidState);
        }
        if r.requester == actor {
            return Err(RejectReason::SelfTrade);
        }
        let l = r.legs.get(leg as usize).ok_or(RejectReason::InvalidInput)?;
        if price > l.limit_price || size < l.qty || expires_at <= self.now {
            return Err(RejectReason::InvalidInput);
        }
        let need = seller_lock(l.qty, price).ok_or(RejectReason::Overflow)?;
        let previous = l.quotes.iter().copied().find(|q| self.quotes[q].mm == actor);
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
        let hold = self.ledger.hold(actor, need).expect(VALIDATED);
        self.quotes.insert(id, Quote { id, mm: actor, request, leg, price, size, expires_at, hold: Some(hold), state: QuoteState::Active });
        self.requests.get_mut(&request).expect(VALIDATED).legs[leg as usize].quotes.insert(id);
        self.arm(expires_at, TimerKey::QuoteExpiry(id));
        out.push(Emitted::to(actor, Event::QuoteSubmitted { quote: id, request, price, expires_at }));
        Ok(())
    }

    pub(super) fn cancel_quote(&mut self, actor: AccountId, id: QuoteId, out: &mut Out) -> Handled {
        let q = self.quotes.get(&id).ok_or(RejectReason::UnknownObject)?;
        if q.mm != actor {
            return Err(RejectReason::Unauthorized);
        }
        match q.state {
            QuoteState::Active => {
                self.end_quote(id, QuoteState::Cancelled, out);
                Ok(())
            }
            _ => Err(RejectReason::InvalidState),
        }
    }

    pub(super) fn on_quote_expiry(&mut self, id: QuoteId, out: &mut Out) {
        let Some(q) = self.quotes.get(&id) else { return };
        if q.state == QuoteState::Active {
            self.end_quote(id, QuoteState::Expired, out);
        }
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
            QuoteState::Active | QuoteState::Selected | QuoteState::Filled => unreachable!("not a release state"),
        };
        out.push(Emitted::to(mm, event));
    }

    /// Moves a live request to a terminal non-filled state, releasing the requester's hold and
    /// every live quote on every leg in this one command (multi-leg atomicity, engine.md §8).
    pub(super) fn close_request(&mut self, id: RequestId, state: RequestState, out: &mut Out) {
        let r = self.requests.get_mut(&id).expect("request exists");
        debug_assert!(r.state.is_live());
        if let Some(h) = r.hold.take() {
            self.ledger.release(h).expect("live request hold exists");
        }
        r.state = state;
        let live: Vec<QuoteId> = r.legs.iter().flat_map(|l| l.quotes.iter().copied()).collect();
        for q in live {
            let terminal = if self.quotes[&q].state == QuoteState::Selected { QuoteState::Released } else { QuoteState::Lost };
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
```

- [ ] **Step 5: Wire up dispatch and timers**

In `crates/rfq-core/src/engine/mod.rs`, add `mod contract; mod rfq;` next to `mod account;`. Then replace the `let result = match action { ... }` block in `apply_user` with:
```rust
        let result = match action {
            Action::Deposit { to, amount } => self.deposit(actor, *to, *amount, out),
            Action::Withdraw { amount } => self.withdraw(actor, *amount, out),
            Action::ListContract { id, description, oracle, trading_close, resolution_deadline, dispute_window } => {
                self.list_contract(actor, *id, description, *oracle, *trading_close, *resolution_deadline, *dispute_window, out)
            }
            Action::SubmitRequest { legs, response_deadline } => self.submit_request(actor, legs, *response_deadline, out),
            Action::CancelRequest { request } => self.cancel_request(actor, *request, out),
            Action::SubmitQuote { request, leg, price, size, expires_at } => {
                self.submit_quote(actor, *request, *leg, *price, *size, *expires_at, out)
            }
            Action::CancelQuote { quote } => self.cancel_quote(actor, *quote, out),
            _ => Err(RejectReason::Unsupported),
        };
```
Then replace the timer `match key { _ => ... }` in `fire_due_timers` with:
```rust
            match key {
                TimerKey::QuoteExpiry(q) => self.on_quote_expiry(q, out),
                TimerKey::ContractClose(c) => self.on_contract_close(c, out),
                _ => {}
            }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p rfq-core`
Expected: all pass, including 13 in `requests_quotes`. Note that `request(...)` creates its request at `DEADLINE = 10_000`, which satisfies `min_response` because the harness time is 0.

- [ ] **Step 7: Commit**

```bash
git add crates/rfq-core
git commit -m "feat(core): contract listing, requests with worst-case holds, sealed quotes with atomic replace"
```

---

### Task 5: Selection, acceptance, rejection and races

**Files:**
- Modify: `crates/rfq-core/src/engine/rfq.rs`, `crates/rfq-core/src/engine/mod.rs`
- Create: `crates/rfq-core/tests/selection_accept.rs`

**Interfaces:**
- Consumes: `close_request`, `end_quote` (Task 4).
- Produces: handlers `on_request_deadline(RequestId, Timestamp, &mut Out)`, `on_accept_by`, `accept`, `reject`; `cancel_quote` and `on_quote_expiry` gain the `Selected` path. Positions are created with ids `PositionId(next_position)`.

- [ ] **Step 1: Write the failing tests**

`crates/rfq-core/tests/selection_accept.rs`:
```rust
use rfq_core::testkit::{accounts::*, *};
use rfq_core::*;

const DEADLINE: Timestamp = 10_000;
const ACCEPT_BY: Timestamp = DEADLINE + 30_000;

struct World { h: Harness, c: ContractId, r: RequestId }

fn world(side: Side) -> World {
    let mut h = Harness::new(Config::default());
    for a in [ALICE, MM1, MM2] {
        h.fund(a, 1_000);
    }
    let c = ContractId(1);
    h.user(OPERATOR, Action::ListContract { id: c, description: "q".into(), oracle: ORACLE, trading_close: 1_000_000, resolution_deadline: 2_000_000, dispute_window: 10_000 });
    let r = RequestId(h.next_seq());
    let out = h.user(ALICE, Action::SubmitRequest { legs: vec![LegSpec { contract: c, side, qty: 1_000, limit_price: price(7_000) }], response_deadline: DEADLINE });
    assert_eq!(rejection(&out), None);
    World { h, c, r }
}

fn quote(w: &mut World, mm: AccountId, bps: u16, expires_at: Timestamp) -> QuoteId {
    let id = QuoteId(w.h.next_seq());
    let out = w.h.user(mm, Action::SubmitQuote { request: w.r, leg: 0, price: price(bps), size: 1_000, expires_at });
    assert_eq!(rejection(&out), None);
    id
}

fn picks(q: QuoteId, bps: u16) -> Vec<Pick> { vec![Pick { leg: 0, quote: q, price: price(bps) }] }

#[test]
fn best_quote_is_presented_and_losers_released() {
    let mut w = world(Side::BuyYes);
    let q1 = quote(&mut w, MM1, 6_200, 100_000);
    let q2 = quote(&mut w, MM2, 6_000, 100_000);
    let out = w.h.tick(DEADLINE);
    assert_eq!(names(&out), vec!["QuoteLost", "QuoteSelected", "RequestPresented"]);
    assert_eq!(w.h.engine.request(w.r).unwrap().state, RequestState::Presented { accept_by: ACCEPT_BY });
    assert_eq!(w.h.engine.quote(q1).unwrap().state, QuoteState::Lost);
    assert_eq!(w.h.engine.quote(q2).unwrap().state, QuoteState::Selected);
    assert_eq!(w.h.engine.balance(MM1).free, dollars(1_000));
    let presented = out.iter().find(|e| e.event.name() == "RequestPresented").unwrap();
    assert_eq!(presented.audience, Audience::Account(ALICE));
    assert_eq!(presented.event, Event::RequestPresented { request: w.r, picks: picks(q2, 6_000), accept_by: ACCEPT_BY });
}

#[test]
fn tie_goes_to_earliest_quote() {
    let mut w = world(Side::BuyYes);
    let q1 = quote(&mut w, MM1, 6_000, 100_000);
    quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    assert_eq!(w.h.engine.quote(q1).unwrap().state, QuoteState::Selected);
}

#[test]
fn short_expiry_not_selected() {
    let mut w = world(Side::BuyYes);
    quote(&mut w, MM1, 5_000, ACCEPT_BY - 1); // cheaper, but not firm through the window
    let q2 = quote(&mut w, MM2, 6_000, ACCEPT_BY);
    w.h.tick(DEADLINE);
    assert_eq!(w.h.engine.quote(q2).unwrap().state, QuoteState::Selected);
}

#[test]
fn no_quotes_expires_request() {
    let mut w = world(Side::BuyYes);
    let out = w.h.tick(DEADLINE);
    assert_eq!(names(&out), vec!["RequestExpired"]);
    assert_eq!(w.h.engine.balance(ALICE).free, dollars(1_000));
}

#[test]
fn accept_escrows_exact_amounts_buy_yes() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    let out = w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) });
    assert_eq!(names(&out), vec!["QuoteFilled", "RequestFilled"]);
    assert_eq!(w.h.engine.balance(ALICE), Balance { free: dollars(400), reserved: 0, escrowed: dollars(600) });
    assert_eq!(w.h.engine.balance(MM2), Balance { free: dollars(600), reserved: 0, escrowed: dollars(400) });
    let p = w.h.engine.position(PositionId(1)).unwrap();
    assert_eq!((p.yes_holder, p.yes_lock, p.no_holder, p.no_lock), (ALICE, dollars(600), MM2, dollars(400)));
    assert_eq!(w.h.engine.contract(w.c).unwrap().positions, vec![PositionId(1)]);
}

#[test]
fn accept_buy_no_maps_sides() {
    let mut w = world(Side::BuyNo);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) });
    let p = w.h.engine.position(PositionId(1)).unwrap();
    assert_eq!((p.yes_holder, p.yes_lock, p.no_holder, p.no_lock), (MM2, dollars(400), ALICE, dollars(600)));
}

#[test]
fn accept_must_echo_exact_selection() {
    let mut w = world(Side::BuyYes);
    let q1 = quote(&mut w, MM1, 6_200, 100_000);
    let q2 = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    let mut accept = |p| rejection(&w.h.user(ALICE, Action::Accept { request: w.r, picks: p }));
    assert_eq!(accept(picks(q2, 5_999)), Some(RejectReason::Mismatch));
    assert_eq!(accept(picks(q1, 6_200)), Some(RejectReason::Mismatch));
    assert_eq!(accept(vec![]), Some(RejectReason::Mismatch));
    let out = w.h.user(MM1, Action::Accept { request: w.r, picks: picks(q2, 6_000) });
    assert_eq!(rejection(&out), Some(RejectReason::Unauthorized));
}

#[test]
fn double_accept_rejected() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    assert_eq!(rejection(&w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) })), None);
    let out = w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(rejection(&w.h.user(ALICE, Action::Reject { request: w.r })), Some(RejectReason::InvalidState));
}

#[test]
fn reject_releases_everything() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    let out = w.h.user(ALICE, Action::Reject { request: w.r });
    assert_eq!(names(&out), vec!["QuoteReleased", "RequestRejected"]);
    assert_eq!(w.h.engine.request(w.r).unwrap().state, RequestState::Rejected(RejectCause::Declined));
    assert_eq!(w.h.engine.quote(q).unwrap().state, QuoteState::Released);
    assert_eq!(w.h.engine.balance(ALICE).free, dollars(1_000));
    assert_eq!(w.h.engine.balance(MM2).free, dollars(1_000));
}

#[test]
fn accept_after_window_rejected_without_tick() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    w.h.set_time(ACCEPT_BY);
    let out = w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) });
    assert_eq!(names(&out), vec!["QuoteReleased", "RequestRejected", "CommandRejected"]);
    assert_eq!(w.h.engine.request(w.r).unwrap().state, RequestState::Rejected(RejectCause::Lapsed));
}

#[test]
fn idle_engine_fires_deadline_then_lapse_in_one_command() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.set_time(ACCEPT_BY + 5_000); // no tick since the quote
    let out = w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) });
    assert_eq!(
        names(&out),
        vec!["QuoteSelected", "RequestPresented", "QuoteReleased", "RequestRejected", "CommandRejected"]
    );
    assert_eq!(w.h.engine.balance(ALICE).free, dollars(1_000));
}

#[test]
fn race_cancel_before_accept() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    let out = w.h.user(MM2, Action::CancelQuote { quote: q });
    assert_eq!(names(&out), vec!["QuoteCancelled", "RequestRejected"]);
    let out = w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(w.h.engine.request(w.r).unwrap().state, RequestState::Rejected(RejectCause::QuoteWithdrawn));
    for a in [ALICE, MM2] {
        assert_eq!(w.h.engine.balance(a).free, dollars(1_000));
    }
}

#[test]
fn race_accept_before_cancel() {
    let mut w = world(Side::BuyYes);
    let q = quote(&mut w, MM2, 6_000, 100_000);
    w.h.tick(DEADLINE);
    assert_eq!(rejection(&w.h.user(ALICE, Action::Accept { request: w.r, picks: picks(q, 6_000) })), None);
    let out = w.h.user(MM2, Action::CancelQuote { quote: q });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(w.h.engine.balance(MM2).escrowed, dollars(400));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rfq-core --test selection_accept`
Expected: FAIL (no selection happens; Accept/Reject give `Unsupported`).

- [ ] **Step 3: Implement selection, accept, reject and lapse**

Append to the `impl Engine` block in `crates/rfq-core/src/engine/rfq.rs`:
```rust
    /// Fired at `response_deadline`. Picks the best eligible quote per leg; all-or-nothing.
    pub(super) fn on_request_deadline(&mut self, id: RequestId, deadline: Timestamp, out: &mut Out) {
        let Some(r) = self.requests.get(&id) else { return };
        if r.state != RequestState::Open {
            return;
        }
        let accept_by = deadline.saturating_add(self.config().accept_window);
        // Eligible = active, within limit, and firm through the whole acceptance window.
        let picks: Option<Vec<Pick>> = r
            .legs
            .iter()
            .enumerate()
            .map(|(i, l)| {
                l.quotes
                    .iter()
                    .map(|q| &self.quotes[q])
                    .filter(|q| q.state == QuoteState::Active && q.price <= l.limit_price && q.expires_at >= accept_by)
                    .min_by_key(|q| (q.price, q.id))
                    .map(|q| Pick { leg: u32::try_from(i).expect("leg count is bounded"), quote: q.id, price: q.price })
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
        out.push(Emitted::to(requester, Event::RequestPresented { request: id, picks, accept_by }));
    }

    pub(super) fn on_accept_by(&mut self, id: RequestId, out: &mut Out) {
        let Some(r) = self.requests.get(&id) else { return };
        if matches!(r.state, RequestState::Presented { .. }) {
            self.close_request(id, RequestState::Rejected(RejectCause::Lapsed), out);
        }
    }

    pub(super) fn reject(&mut self, actor: AccountId, id: RequestId, out: &mut Out) -> Handled {
        let r = self.requests.get(&id).ok_or(RejectReason::UnknownObject)?;
        if r.requester != actor {
            return Err(RejectReason::Unauthorized);
        }
        if !matches!(r.state, RequestState::Presented { .. }) {
            return Err(RejectReason::InvalidState);
        }
        self.close_request(id, RequestState::Rejected(RejectCause::Declined), out);
        Ok(())
    }

    /// Escrows every leg in one command. All checks run before the first mutation.
    pub(super) fn accept(&mut self, actor: AccountId, id: RequestId, picks: &[Pick], out: &mut Out) -> Handled {
        let r = self.requests.get(&id).ok_or(RejectReason::UnknownObject)?;
        if r.requester != actor {
            return Err(RejectReason::Unauthorized);
        }
        if !matches!(r.state, RequestState::Presented { .. }) {
            return Err(RejectReason::InvalidState);
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
        self.ledger.escrow_from_hold(hold, requester_total).expect(VALIDATED);
        let mut positions = Vec::with_capacity(legs.len());
        for l in &legs {
            let qid = l.selected.expect(VALIDATED);
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
            self.positions.insert(pid, Position {
                id: pid,
                contract: l.contract,
                request: id,
                qty: l.qty,
                yes_holder,
                yes_lock,
                no_holder,
                no_lock,
                settled: false,
            });
            self.contracts.get_mut(&l.contract).expect(VALIDATED).positions.push(pid);
            out.push(Emitted::to(q.mm, Event::QuoteFilled { quote: qid, position: pid }));
            positions.push(pid);
        }
        let r = self.requests.get_mut(&id).expect(VALIDATED);
        r.state = RequestState::Filled;
        r.hold = None;
        for l in &mut r.legs {
            l.quotes.clear();
        }
        out.push(Emitted::to(requester, Event::RequestFilled { request: id, positions }));
        Ok(())
    }
```
Add `use crate::command::Pick;` to the imports at the top of `rfq.rs`.

Replace the existing `cancel_quote` match and `on_quote_expiry` body with versions that handle `Selected`:
```rust
        match q.state {
            QuoteState::Active => {
                self.end_quote(id, QuoteState::Cancelled, out);
                Ok(())
            }
            QuoteState::Selected => {
                let request = q.request;
                self.end_quote(id, QuoteState::Cancelled, out);
                self.close_request(request, RequestState::Rejected(RejectCause::QuoteWithdrawn), out);
                Ok(())
            }
            _ => Err(RejectReason::InvalidState),
        }
```
```rust
    pub(super) fn on_quote_expiry(&mut self, id: QuoteId, out: &mut Out) {
        let Some(q) = self.quotes.get(&id) else { return };
        match q.state {
            QuoteState::Active => self.end_quote(id, QuoteState::Expired, out),
            QuoteState::Selected => {
                let request = q.request;
                self.end_quote(id, QuoteState::Expired, out);
                self.close_request(request, RequestState::Rejected(RejectCause::QuoteExpired), out);
            }
            _ => {}
        }
    }
```

- [ ] **Step 4: Wire up dispatch and timers**

In `apply_user`, add these arms before `_ =>`:
```rust
            Action::Accept { request, picks } => self.accept(actor, *request, picks, out),
            Action::Reject { request } => self.reject(actor, *request, out),
```
In `fire_due_timers`, add these arms before `_ => {}`:
```rust
                TimerKey::RequestDeadline(r) => self.on_request_deadline(r, at, out),
                TimerKey::AcceptBy(r) => self.on_accept_by(r, out),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rfq-core`
Expected: all pass, including 13 in `selection_accept`.

- [ ] **Step 6: Commit**

```bash
git add crates/rfq-core
git commit -m "feat(core): sealed selection at deadline, atomic accept into escrow, lapse and withdrawal races"
```

---

### Task 6: Resolution and settlement

**Files:**
- Modify: `crates/rfq-core/src/engine/contract.rs`, `crates/rfq-core/src/engine/mod.rs`
- Create: `crates/rfq-core/tests/resolution.rs`

**Interfaces:**
- Consumes: positions created by `accept` (Task 5).
- Produces: handlers `propose_outcome`, `dispute`, `rule`, `on_dispute_window`, `on_hard_deadline`, `finalize`.

- [ ] **Step 1: Write the failing tests**

`crates/rfq-core/tests/resolution.rs`:
```rust
use rfq_core::testkit::{accounts::*, *};
use rfq_core::*;

const CLOSE: Timestamp = 100_000;
const HARD: Timestamp = 500_000;
const WINDOW: Duration = 10_000;

/// ALICE holds 1000 YES bought at 0.60 from MM2; contract closed.
fn filled() -> (Harness, ContractId) {
    let mut h = Harness::new(Config::default());
    for a in [ALICE, MM2, CAROL] {
        h.fund(a, 1_000);
    }
    let c = ContractId(1);
    h.user(OPERATOR, Action::ListContract { id: c, description: "q".into(), oracle: ORACLE, trading_close: CLOSE, resolution_deadline: HARD, dispute_window: WINDOW });
    let r = RequestId(h.next_seq());
    h.user(ALICE, Action::SubmitRequest { legs: vec![LegSpec { contract: c, side: Side::BuyYes, qty: 1_000, limit_price: price(7_000) }], response_deadline: 10_000 });
    let q = QuoteId(h.next_seq());
    h.user(MM2, Action::SubmitQuote { request: r, leg: 0, price: price(6_000), size: 1_000, expires_at: 90_000 });
    h.tick(10_000);
    let out = h.user(ALICE, Action::Accept { request: r, picks: vec![Pick { leg: 0, quote: q, price: price(6_000) }] });
    assert_eq!(rejection(&out), None);
    h.tick(CLOSE);
    (h, c)
}

fn free(h: &Harness, a: AccountId) -> Amount { h.engine.balance(a).free }

#[test]
fn yes_pays_yes_holder_after_window() {
    let (mut h, c) = filled();
    assert_eq!(rejection(&h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes })), None);
    let out = h.tick(CLOSE + WINDOW);
    assert_eq!(names(&out), vec!["ContractFinalized", "PositionSettled", "PositionSettled"]);
    assert_eq!(free(&h, ALICE), dollars(1_400));
    assert_eq!(free(&h, MM2), dollars(600));
    assert_eq!(h.engine.balance(ALICE).escrowed + h.engine.balance(MM2).escrowed, 0);
}

#[test]
fn no_pays_no_holder() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::No });
    h.tick(CLOSE + WINDOW);
    assert_eq!(free(&h, ALICE), dollars(400));
    assert_eq!(free(&h, MM2), dollars(1_600));
}

#[test]
fn propose_rules() {
    let mut h = Harness::new(Config::default());
    let c = ContractId(1);
    h.user(OPERATOR, Action::ListContract { id: c, description: "q".into(), oracle: ORACLE, trading_close: CLOSE, resolution_deadline: HARD, dispute_window: WINDOW });
    let out = h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState), "trading still open");
    h.tick(CLOSE);
    let out = h.user(ALICE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    assert_eq!(rejection(&out), Some(RejectReason::Unauthorized));
}

#[test]
fn dispute_upheld_forfeits_bond_to_treasury() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    assert_eq!(rejection(&h.user(CAROL, Action::Dispute { contract: c })), None);
    assert_eq!(h.engine.balance(CAROL).reserved, dollars(100));
    let out = h.user(ARBITER, Action::Rule { contract: c, outcome: Outcome::Yes });
    assert_eq!(names(&out)[0], "Ruled");
    assert_eq!(free(&h, CAROL), dollars(900));
    assert_eq!(free(&h, TREASURY), dollars(100));
    assert_eq!(free(&h, ALICE), dollars(1_400));
}

#[test]
fn dispute_overturned_returns_bond_and_invalid_refunds() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    h.user(CAROL, Action::Dispute { contract: c });
    h.user(ARBITER, Action::Rule { contract: c, outcome: Outcome::Invalid });
    assert_eq!(free(&h, CAROL), dollars(1_000));
    assert_eq!(free(&h, ALICE), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(1_000));
}

#[test]
fn dispute_rules() {
    let (mut h, c) = filled();
    assert_eq!(rejection(&h.user(CAROL, Action::Dispute { contract: c })), Some(RejectReason::InvalidState), "nothing proposed");
    h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    h.user(CAROL, Action::Dispute { contract: c });
    assert_eq!(rejection(&h.user(ALICE, Action::Dispute { contract: c })), Some(RejectReason::InvalidState), "one dispute per contract");
    assert_eq!(rejection(&h.user(CAROL, Action::Rule { contract: c, outcome: Outcome::No })), Some(RejectReason::Unauthorized));
}

#[test]
fn dispute_after_window_rejected() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    h.set_time(CLOSE + WINDOW);
    let out = h.user(CAROL, Action::Dispute { contract: c });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(free(&h, CAROL), dollars(1_000));
}

#[test]
fn oracle_silent_hard_deadline_refunds() {
    let (mut h, c) = filled();
    let out = h.tick(HARD);
    assert_eq!(names(&out)[0], "ContractFinalized");
    assert_eq!(h.engine.contract(c).unwrap().state, ContractState::Final(Outcome::Invalid));
    assert_eq!(free(&h, ALICE), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(1_000));
}

#[test]
fn arbiter_silent_hard_deadline_returns_bond() {
    let (mut h, c) = filled();
    h.user(ORACLE, Action::ProposeOutcome { contract: c, outcome: Outcome::Yes });
    h.user(CAROL, Action::Dispute { contract: c });
    h.tick(HARD);
    assert_eq!(free(&h, CAROL), dollars(1_000));
    assert_eq!(free(&h, ALICE), dollars(1_000));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rfq-core --test resolution`
Expected: FAIL (`Unsupported` rejections; no finalisation).

- [ ] **Step 3: Implement resolution**

Append to the `impl Engine` block in `crates/rfq-core/src/engine/contract.rs`, and add `use super::VALIDATED;` to its imports:
```rust
    pub(super) fn propose_outcome(&mut self, actor: AccountId, id: ContractId, outcome: Outcome, out: &mut Out) -> Handled {
        let c = self.contracts.get(&id).ok_or(RejectReason::UnknownObject)?;
        if actor != c.oracle {
            return Err(RejectReason::Unauthorized);
        }
        if c.state != ContractState::Closed {
            return Err(RejectReason::InvalidState);
        }
        let dispute_until = self.now.checked_add(c.dispute_window).ok_or(RejectReason::Overflow)?;
        self.contracts.get_mut(&id).expect(VALIDATED).state = ContractState::Proposed { outcome, dispute_until };
        self.arm(dispute_until, TimerKey::DisputeWindow(id));
        out.push(Emitted::public(Event::OutcomeProposed { contract: id, outcome, dispute_until }));
        Ok(())
    }

    pub(super) fn dispute(&mut self, actor: AccountId, id: ContractId, out: &mut Out) -> Handled {
        let bond = self.config().dispute_bond;
        let c = self.contracts.get(&id).ok_or(RejectReason::UnknownObject)?;
        let ContractState::Proposed { outcome, dispute_until } = c.state else {
            return Err(RejectReason::InvalidState);
        };
        if self.now >= dispute_until {
            return Err(RejectReason::InvalidState);
        }
        if self.ledger.balance(actor).free < bond {
            return Err(RejectReason::InsufficientFunds);
        }
        let hold = self.ledger.hold(actor, bond).expect(VALIDATED);
        let c = self.contracts.get_mut(&id).expect(VALIDATED);
        c.state = ContractState::Disputed { proposed: outcome };
        c.dispute = Some(Dispute { disputer: actor, hold: Some(hold) });
        out.push(Emitted::public(Event::Disputed { contract: id, disputer: actor }));
        Ok(())
    }

    pub(super) fn rule(&mut self, actor: AccountId, id: ContractId, outcome: Outcome, out: &mut Out) -> Handled {
        if actor != self.genesis().arbiter {
            return Err(RejectReason::Unauthorized);
        }
        let treasury = self.genesis().treasury;
        let c = self.contracts.get_mut(&id).ok_or(RejectReason::UnknownObject)?;
        let ContractState::Disputed { proposed } = c.state else {
            return Err(RejectReason::InvalidState);
        };
        let hold = c.dispute.as_mut().and_then(|d| d.hold.take()).expect("disputed contract holds a bond");
        let bond_returned = outcome != proposed;
        if bond_returned {
            self.ledger.release(hold).expect(VALIDATED);
        } else {
            self.ledger.forfeit(hold, treasury).expect(VALIDATED);
        }
        out.push(Emitted::public(Event::Ruled { contract: id, outcome, bond_returned }));
        self.finalize(id, outcome, out);
        Ok(())
    }

    pub(super) fn on_dispute_window(&mut self, id: ContractId, out: &mut Out) {
        let Some(c) = self.contracts.get(&id) else { return };
        if let ContractState::Proposed { outcome, .. } = c.state {
            self.finalize(id, outcome, out);
        }
    }

    /// Money can never be stuck: any contract not final by its hard deadline resolves Invalid.
    pub(super) fn on_hard_deadline(&mut self, id: ContractId, out: &mut Out) {
        let Some(c) = self.contracts.get_mut(&id) else { return };
        if matches!(c.state, ContractState::Final(_)) {
            return;
        }
        if let Some(h) = c.dispute.as_mut().and_then(|d| d.hold.take()) {
            self.ledger.release(h).expect("dispute bond hold exists");
        }
        self.finalize(id, Outcome::Invalid, out);
    }

    /// Sets the final outcome and settles every position of the contract in this command.
    fn finalize(&mut self, id: ContractId, outcome: Outcome, out: &mut Out) {
        let c = self.contracts.get_mut(&id).expect("contract exists");
        c.state = ContractState::Final(outcome);
        let positions = c.positions.clone();
        out.push(Emitted::public(Event::ContractFinalized { contract: id, outcome }));
        for pid in positions {
            let p = self.positions.get_mut(&pid).expect("contract position exists");
            if p.settled {
                continue;
            }
            let (yes_payout, no_payout) = match outcome {
                Outcome::Yes => (p.yes_lock + p.no_lock, 0),
                Outcome::No => (0, p.yes_lock + p.no_lock),
                Outcome::Invalid => (p.yes_lock, p.no_lock),
            };
            let (yes_to, no_to) = match outcome {
                Outcome::Yes => (p.yes_holder, p.yes_holder),
                Outcome::No => (p.no_holder, p.no_holder),
                Outcome::Invalid => (p.yes_holder, p.no_holder),
            };
            self.ledger.pay_from_escrow(p.yes_holder, p.yes_lock, yes_to).expect("position escrow exists");
            self.ledger.pay_from_escrow(p.no_holder, p.no_lock, no_to).expect("position escrow exists");
            p.settled = true;
            let event = Event::PositionSettled { position: pid, yes_holder: p.yes_holder, yes_payout, no_holder: p.no_holder, no_payout };
            out.push(Emitted::to(p.yes_holder, event.clone()));
            out.push(Emitted::to(p.no_holder, event));
        }
    }
```

- [ ] **Step 4: Wire up dispatch and timers**

In `apply_user`, add these arms and **delete the `_ => Err(RejectReason::Unsupported)` arm**, so the match is exhaustive:
```rust
            Action::ProposeOutcome { contract, outcome } => self.propose_outcome(actor, *contract, *outcome, out),
            Action::Dispute { contract } => self.dispute(actor, *contract, out),
            Action::Rule { contract, outcome } => self.rule(actor, *contract, *outcome, out),
```
In `fire_due_timers`, add these arms and **delete `_ => {}`**:
```rust
                TimerKey::DisputeWindow(c) => self.on_dispute_window(c, out),
                TimerKey::HardDeadline(c) => self.on_hard_deadline(c, out),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rfq-core`
Expected: all pass, including 9 in `resolution`, and there are no exhaustiveness warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/rfq-core
git commit -m "feat(core): oracle proposals, bonded disputes, arbiter rulings, hard-deadline invalidation and settlement"
```

---

### Task 7: Scenarios, property test and replay determinism

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

---

### Task 8: Codec

**Files:**
- Create: `crates/rfq-core/src/codec.rs`
- Modify: `crates/rfq-core/src/lib.rs` (add `pub mod codec;`)

**Interfaces:**
- Produces: `codec::{VERSION, MAX_MESSAGE, encode<T: Serialize>(&T) -> Vec<u8>, decode<T: DeserializeOwned>(&[u8]) -> Result<T, CodecError>, CodecError::{Empty, UnsupportedVersion(u8), Malformed(String)}, ClientCommand { actor, nonce, action }, ClientCommand::into_command(self) -> Command, EventBatch { seq, origin: Option<(AccountId, u64)>, events: Vec<Emitted> }}`.

- [ ] **Step 1: Write the failing tests**

At the bottom of `crates/rfq-core/src/codec.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::accounts::ALICE;

    #[test]
    fn roundtrip_client_command() {
        let c = ClientCommand { actor: ALICE, nonce: 3, action: Action::Withdraw { amount: 5 } };
        assert_eq!(decode::<ClientCommand>(&encode(&c)).unwrap(), c);
    }

    #[test]
    fn rejects_empty_and_wrong_version() {
        assert_eq!(decode::<ClientCommand>(&[]), Err(CodecError::Empty));
        let mut bytes = encode(&ClientCommand { actor: ALICE, nonce: 1, action: Action::Withdraw { amount: 1 } });
        bytes[0] = 9;
        assert_eq!(decode::<ClientCommand>(&bytes), Err(CodecError::UnsupportedVersion(9)));
    }

    #[test]
    fn decode_rejects_oversized() {
        // A Vec<LegSpec> length prefix claiming 2^40 elements must not allocate.
        let mut bytes = vec![VERSION];
        bytes.extend(10u64.to_le_bytes()); // actor
        bytes.extend(1u64.to_le_bytes()); // nonce
        bytes.extend(3u32.to_le_bytes()); // Action::SubmitRequest variant
        bytes.extend((1u64 << 40).to_le_bytes()); // legs length
        assert!(matches!(decode::<ClientCommand>(&bytes), Err(CodecError::Malformed(_))));
    }

    #[test]
    fn system_command_bytes_do_not_decode_as_client_command() {
        assert!(decode::<ClientCommand>(&encode(&Command::Tick)).is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Add `pub mod codec;` to `lib.rs`.
Run: `cargo test -p rfq-core codec`
Expected: FAIL to compile.

- [ ] **Step 3: Implement the codec**

Prepend to `crates/rfq-core/src/codec.rs`:
```rust
//! Wire format: one version byte, then bincode (fixint, little-endian, size-limited).
use bincode::Options;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::command::{Action, Command};
use crate::event::Emitted;
use crate::types::AccountId;

pub const VERSION: u8 = 1;
pub const MAX_MESSAGE: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    Empty,
    UnsupportedVersion(u8),
    Malformed(String),
}

fn opts() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .with_limit(MAX_MESSAGE)
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = vec![VERSION];
    bytes.extend(opts().serialize(value).expect("message fits MAX_MESSAGE"));
    bytes
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    match bytes.split_first() {
        None => Err(CodecError::Empty),
        Some((&VERSION, rest)) => opts().deserialize(rest).map_err(|e| CodecError::Malformed(e.to_string())),
        Some((v, _)) => Err(CodecError::UnsupportedVersion(*v)),
    }
}

/// What a client may put on ingress. It cannot express `Genesis` or `Tick`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCommand {
    pub actor: AccountId,
    pub nonce: u64,
    pub action: Action,
}

impl ClientCommand {
    pub fn into_command(self) -> Command {
        Command::User { actor: self.actor, nonce: self.nonce, action: self.action }
    }
}

/// One egress message per applied command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventBatch {
    pub seq: u64,
    /// `(actor, nonce)` of the user command that produced these events; `None` for ticks.
    pub origin: Option<(AccountId, u64)>,
    pub events: Vec<Emitted>,
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p rfq-core codec`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/rfq-core
git commit -m "feat(core): versioned size-limited codec, ClientCommand and EventBatch"
```

---

### Task 9: Node core (sequencer, journals, transport, write-ahead loop)

**Files:**
- Create: `crates/rfq-node/Cargo.toml`, `src/lib.rs`, `src/sequencer.rs`, `src/journal.rs`, `src/transport.rs`, `src/node.rs`, `tests/node.rs`
- Modify: `Cargo.toml` (add the member and workspace deps)

**Interfaces:**
- Consumes: `rfq_core::{Engine, Command, SequencedCommand, Genesis, codec::*}`.
- Produces: `Sequencer::{resume(last_seq, last_time), stamp(wall_ms, Command) -> SequencedCommand}`; `trait Journal { append(&mut self, &SequencedCommand) -> anyhow::Result<u64>; wait_durable(&mut self, u64) -> anyhow::Result<()>; replay(&mut self, &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<()> }`; `MemJournal`, `FileJournal::open(path)`; `trait Transport { poll_ingress(&mut self, max: usize, sink: &mut dyn FnMut(&[u8])); publish_egress(&mut self, &[u8]) -> anyhow::Result<()> }`; `MemTransport { ingress: VecDeque<Vec<u8>>, egress: Vec<Vec<u8>> }`; `Node::{start(J, T, Genesis) -> anyhow::Result<Node<J,T>>, poll(wall_ms) -> anyhow::Result<usize>, engine(), state_hash(), into_parts() -> (J, T)}`.

- [ ] **Step 1: Create the crate**

Add `"crates/rfq-node"` to the workspace `members`, and add `anyhow = "1"`, `tracing = "0.1"`, `tracing-subscriber = "0.3"`, `crc32fast = "1"`, `tempfile = "3"` to `[workspace.dependencies]`.

`crates/rfq-node/Cargo.toml`:
```toml
[package]
name = "rfq-node"
version = "0.1.0"
edition = "2021"

[dependencies]
rfq-core = { path = "../rfq-core" }
anyhow = { workspace = true }
tracing = { workspace = true }
crc32fast = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```
`crates/rfq-node/src/lib.rs`:
```rust
pub mod journal;
pub mod node;
pub mod sequencer;
pub mod transport;
```

- [ ] **Step 2: Write the failing tests**

`crates/rfq-node/tests/node.rs`:
```rust
use rfq_core::codec::{decode, encode, ClientCommand, EventBatch};
use rfq_core::testkit::{accounts::*, dollars, genesis};
use rfq_core::*;
use rfq_node::journal::{FileJournal, Journal, MemJournal};
use rfq_node::node::Node;
use rfq_node::sequencer::Sequencer;
use rfq_node::transport::MemTransport;

fn deposit(nonce: u64) -> Vec<u8> {
    encode(&ClientCommand { actor: OPERATOR, nonce, action: Action::Deposit { to: ALICE, amount: dollars(5) } })
}

fn batches(t: &MemTransport) -> Vec<EventBatch> {
    t.egress.iter().map(|b| decode(b).unwrap()).collect()
}

#[test]
fn sequencer_time_is_monotonic() {
    let mut s = Sequencer::resume(10, 1_000);
    assert_eq!(s.stamp(900, Command::Tick).time, 1_000);
    let sc = s.stamp(2_000, Command::Tick);
    assert_eq!((sc.seq, sc.time), (12, 2_000));
}

#[test]
fn fresh_node_writes_genesis_then_sequences_client_commands() {
    let mut t = MemTransport::default();
    t.ingress.push_back(deposit(1));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    assert_eq!(node.poll(5_000).unwrap(), 1);
    assert_eq!(node.engine().balance(ALICE).free, dollars(5));
    let (journal, transport) = node.into_parts();
    assert_eq!(journal.entries().len(), 2);
    assert!(matches!(journal.entries()[0].cmd, Command::Genesis(_)));
    let b = batches(&transport);
    assert_eq!(b[0].seq, 2);
    assert_eq!(b[0].origin, Some((OPERATOR, 1)));
}

#[test]
fn restart_replays_to_same_state_without_republishing() {
    let mut t = MemTransport::default();
    t.ingress.push_back(deposit(1));
    t.ingress.push_back(deposit(2));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    node.poll(5_000).unwrap();
    let hash = node.state_hash();
    let (journal, _) = node.into_parts();
    let restarted = Node::start(journal, MemTransport::default(), genesis(Config::default())).unwrap();
    assert_eq!(restarted.state_hash(), hash);
    assert_eq!(restarted.engine().last_seq(), 3);
    let (journal, transport) = restarted.into_parts();
    assert_eq!(journal.entries().len(), 3, "genesis not rewritten");
    assert!(transport.egress.is_empty(), "replay publishes nothing");
}

#[test]
fn ticks_only_when_a_timer_is_due() {
    let mut node = Node::start(MemJournal::default(), MemTransport::default(), genesis(Config::default())).unwrap();
    assert_eq!(node.poll(1_000).unwrap(), 0, "no timers armed: nothing logged");
    // A contract arms close/hard-deadline timers.
    let list = ClientCommand { actor: OPERATOR, nonce: 1, action: Action::ListContract { id: ContractId(1), description: "x".into(), oracle: ORACLE, trading_close: 10_000, resolution_deadline: 20_000, dispute_window: 1 } };
    let (j, mut t) = node.into_parts();
    t.ingress.push_back(encode(&list));
    let mut node = Node::start(j, t, genesis(Config::default())).unwrap();
    node.poll(1_000).unwrap();
    assert_eq!(node.poll(9_999).unwrap(), 0);
    assert_eq!(node.poll(10_000).unwrap(), 1, "tick at trading_close");
}

#[test]
fn malformed_ingress_is_dropped() {
    let mut t = MemTransport::default();
    t.ingress.push_back(vec![1, 2, 3]);
    t.ingress.push_back(vec![]);
    t.ingress.push_back(deposit(1));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    assert_eq!(node.poll(5_000).unwrap(), 1);
    let (journal, _) = node.into_parts();
    assert_eq!(journal.entries().len(), 2);
}

#[test]
fn client_cannot_inject_system_commands() {
    let mut t = MemTransport::default();
    t.ingress.push_back(encode(&Command::Tick));
    t.ingress.push_back(encode(&Command::Genesis(genesis(Config::default()))));
    let mut node = Node::start(MemJournal::default(), t, genesis(Config::default())).unwrap();
    node.poll(5_000).unwrap();
    let (journal, _) = node.into_parts();
    let system = journal.entries().iter().filter(|sc| !matches!(sc.cmd, Command::User { .. })).count();
    assert_eq!(system, 1, "only the node's own genesis");
}

#[test]
fn file_journal_truncates_torn_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    let mut j = FileJournal::open(&path).unwrap();
    let mut s = Sequencer::resume(0, 0);
    for _ in 0..3 {
        let pos = j.append(&s.stamp(1, Command::Tick)).unwrap();
        j.wait_durable(pos).unwrap();
    }
    drop(j);
    // Simulate a crash mid-write: append half a header.
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(&[7, 0]).unwrap();
    let mut j = FileJournal::open(&path).unwrap();
    let mut seen = Vec::new();
    j.replay(&mut |sc| seen.push(sc.seq)).unwrap();
    assert_eq!(seen, vec![1, 2, 3]);
    let pos = j.append(&s.stamp(2, Command::Tick)).unwrap();
    j.wait_durable(pos).unwrap();
    let mut seen = Vec::new();
    FileJournal::open(&path).unwrap().replay(&mut |sc| seen.push(sc.seq)).unwrap();
    assert_eq!(seen, vec![1, 2, 3, 4], "torn tail truncated, appends continue");
}

use std::io::Write;
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p rfq-node`
Expected: FAIL to compile (modules empty).

- [ ] **Step 4: Implement the sequencer and transport**

`crates/rfq-node/src/sequencer.rs`:
```rust
use rfq_core::{Command, SequencedCommand, Timestamp};

/// Assigns the total order. Time is the wall clock, but never moves backwards.
pub struct Sequencer {
    last_seq: u64,
    last_time: Timestamp,
}

impl Sequencer {
    pub fn resume(last_seq: u64, last_time: Timestamp) -> Self { Sequencer { last_seq, last_time } }

    pub fn stamp(&mut self, wall_ms: Timestamp, cmd: Command) -> SequencedCommand {
        self.last_seq += 1;
        self.last_time = self.last_time.max(wall_ms);
        SequencedCommand { seq: self.last_seq, time: self.last_time, cmd }
    }
}
```
`crates/rfq-node/src/transport.rs`:
```rust
use std::collections::VecDeque;

pub trait Transport {
    /// Hands up to `max` raw ingress messages to `sink`.
    fn poll_ingress(&mut self, max: usize, sink: &mut dyn FnMut(&[u8]));
    fn publish_egress(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
}

#[derive(Default)]
pub struct MemTransport {
    pub ingress: VecDeque<Vec<u8>>,
    pub egress: Vec<Vec<u8>>,
}

impl Transport for MemTransport {
    fn poll_ingress(&mut self, max: usize, sink: &mut dyn FnMut(&[u8])) {
        for _ in 0..max {
            match self.ingress.pop_front() {
                Some(m) => sink(&m),
                None => break,
            }
        }
    }

    fn publish_egress(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.egress.push(bytes.to_vec());
        Ok(())
    }
}
```

- [ ] **Step 5: Implement the journals**

`crates/rfq-node/src/journal.rs`:
```rust
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::Context;
use rfq_core::codec::{decode, encode};
use rfq_core::SequencedCommand;

/// The durable, ordered command log. State = replay(log).
pub trait Journal {
    /// Appends and returns a position to pass to `wait_durable`.
    fn append(&mut self, sc: &SequencedCommand) -> anyhow::Result<u64>;
    /// Blocks until everything up to `pos` survives a crash (write-ahead gate).
    fn wait_durable(&mut self, pos: u64) -> anyhow::Result<()>;
    /// Feeds every recorded command, in order, to `f`.
    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<()>;
}

#[derive(Default)]
pub struct MemJournal {
    entries: Vec<SequencedCommand>,
}

impl MemJournal {
    pub fn entries(&self) -> &[SequencedCommand] { &self.entries }
}

impl Journal for MemJournal {
    fn append(&mut self, sc: &SequencedCommand) -> anyhow::Result<u64> {
        self.entries.push(sc.clone());
        Ok(self.entries.len() as u64)
    }
    fn wait_durable(&mut self, _pos: u64) -> anyhow::Result<()> { Ok(()) }
    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<()> {
        self.entries.iter().cloned().for_each(f);
        Ok(())
    }
}

/// Append-only file: `[len: u32 LE][crc32: u32 LE][body: len bytes]` per record.
pub struct FileJournal {
    file: File,
    end: u64,
}

impl FileJournal {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).open(path).with_context(|| format!("open {path:?}"))?;
        let mut j = FileJournal { file, end: 0 };
        j.scan(&mut |_| {})?; // finds the last good record and truncates any torn tail
        Ok(j)
    }

    fn scan(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        let mut at = 0usize;
        while bytes.len() - at >= 8 {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let crc = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap());
            let Some(body) = bytes.get(at + 8..at + 8 + len) else { break };
            if crc32fast::hash(body) != crc {
                break;
            }
            let sc: SequencedCommand = decode(body).map_err(|e| anyhow::anyhow!("journal record at {at}: {e:?}"))?;
            f(sc);
            at += 8 + len;
        }
        if at as u64 != bytes.len() as u64 {
            tracing::warn!(good = at, total = bytes.len(), "truncating torn journal tail");
            self.file.set_len(at as u64)?;
        }
        self.end = at as u64;
        self.file.seek(SeekFrom::Start(self.end))?;
        Ok(())
    }
}

impl Journal for FileJournal {
    fn append(&mut self, sc: &SequencedCommand) -> anyhow::Result<u64> {
        let body = encode(sc);
        let mut rec = Vec::with_capacity(8 + body.len());
        rec.extend((body.len() as u32).to_le_bytes());
        rec.extend(crc32fast::hash(&body).to_le_bytes());
        rec.extend(&body);
        self.file.write_all(&rec)?;
        self.end += rec.len() as u64;
        Ok(self.end)
    }
    fn wait_durable(&mut self, _pos: u64) -> anyhow::Result<()> {
        self.file.sync_data()?;
        Ok(())
    }
    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<()> { self.scan(f) }
}
```

- [ ] **Step 6: Implement the node loop**

`crates/rfq-node/src/node.rs`:
```rust
use rfq_core::codec::{decode, encode, ClientCommand, EventBatch};
use rfq_core::{Command, Engine, Genesis, Timestamp};

use crate::journal::Journal;
use crate::sequencer::Sequencer;
use crate::transport::Transport;

const MAX_BATCH: usize = 256;

pub struct Node<J, T> {
    engine: Engine,
    journal: J,
    transport: T,
    sequencer: Sequencer,
}

impl<J: Journal, T: Transport> Node<J, T> {
    /// Replays the journal (egress suppressed). On an empty journal, writes `genesis` as seq 1.
    pub fn start(mut journal: J, transport: T, genesis: Genesis) -> anyhow::Result<Self> {
        let mut engine = Engine::new();
        let (mut last_seq, mut last_time) = (0, 0);
        journal.replay(&mut |sc| {
            last_seq = sc.seq;
            last_time = sc.time;
            engine.apply(&sc);
        })?;
        let mut sequencer = Sequencer::resume(last_seq, last_time);
        if last_seq == 0 {
            let sc = sequencer.stamp(0, Command::Genesis(genesis));
            let pos = journal.append(&sc)?;
            journal.wait_durable(pos)?;
            engine.apply(&sc);
        }
        tracing::info!(seq = engine.last_seq(), hash = %hex(&engine.state_hash()), "replayed");
        Ok(Node { engine, journal, transport, sequencer })
    }

    /// One loop iteration: sequence → append → wait durable → apply → publish.
    pub fn poll(&mut self, wall_ms: Timestamp) -> anyhow::Result<usize> {
        let mut incoming = Vec::new();
        self.transport.poll_ingress(MAX_BATCH, &mut |bytes| match decode::<ClientCommand>(bytes) {
            Ok(c) => incoming.push(c),
            Err(e) => tracing::warn!(?e, "dropping undecodable ingress message"),
        });
        let mut stamped: Vec<_> = incoming.into_iter().map(|c| self.sequencer.stamp(wall_ms, c.into_command())).collect();
        if stamped.is_empty() && self.engine.next_timer().is_some_and(|t| t <= wall_ms) {
            stamped.push(self.sequencer.stamp(wall_ms, Command::Tick));
        }
        if stamped.is_empty() {
            return Ok(0);
        }
        let mut pos = 0;
        for sc in &stamped {
            pos = self.journal.append(sc)?;
        }
        self.journal.wait_durable(pos)?;
        for sc in &stamped {
            let events = self.engine.apply(sc);
            let origin = match &sc.cmd {
                Command::User { actor, nonce, .. } => Some((*actor, *nonce)),
                _ => None,
            };
            if origin.is_some() || !events.is_empty() {
                self.transport.publish_egress(&encode(&EventBatch { seq: sc.seq, origin, events }))?;
            }
        }
        Ok(stamped.len())
    }

    pub fn engine(&self) -> &Engine { &self.engine }
    pub fn state_hash(&self) -> [u8; 32] { self.engine.state_hash() }
    pub fn into_parts(self) -> (J, T) { (self.journal, self.transport) }
}

pub fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p rfq-node`
Expected: 7 passed.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/rfq-node
git commit -m "feat(node): sequencer, file/memory journals, write-ahead node loop with replay"
```

---

### Task 10: Aeron adapters and the `rfq-node` binary

**Files:**
- Create: `crates/rfq-node/src/aeron.rs`, `crates/rfq-node/src/main.rs`
- Modify: `crates/rfq-node/Cargo.toml`, `crates/rfq-node/src/lib.rs`

**Interfaces:**
- Consumes: the Task 0 "Verified API" section (this is the source of truth for every `rusteron` call below), plus Task 9 traits.
- Produces: `aeron::{connect(dir) -> anyhow::Result<Aeron>, AeronTransport::new(&Aeron) -> anyhow::Result<AeronTransport>, ArchiveJournal::new(&Aeron) -> anyhow::Result<ArchiveJournal>}`. Constants `INGRESS = 1001, LOG = 1002, EGRESS = 1003, REPLAY = 1004`. The binary is configured by env vars `AERON_DIR` (default `/dev/shm/aeron`), `RFQ_JOURNAL` (`archive` | `file:<path>`), and `RFQ_HASH_EVERY_BATCH` (`1` to log the state hash after each batch).

- [ ] **Step 1: Add dependencies**

In `crates/rfq-node/Cargo.toml`:
```toml
rusteron-archive = { version = "=<version from Task 0>", features = ["static", "precompile"] }
tracing-subscriber = { workspace = true }
```
Add `pub mod aeron;` to `lib.rs`.

- [ ] **Step 2: Implement the adapters**

`crates/rfq-node/src/aeron.rs` (adjust each `rusteron` call to the Verified API from Task 0; the structure and semantics must stay as written):
```rust
use std::time::Duration;

use anyhow::{anyhow, bail};
use rfq_core::codec::{decode, encode};
use rfq_core::SequencedCommand;
use rusteron_archive::*;

use crate::journal::Journal;
use crate::transport::Transport;

pub const INGRESS: i32 = 1001;
pub const LOG: i32 = 1002;
pub const EGRESS: i32 = 1003;
pub const REPLAY: i32 = 1004;
const TIMEOUT: Duration = Duration::from_secs(10);

fn ipc() -> std::ffi::CString { "aeron:ipc".into_c_string() }

pub fn connect(dir: &str) -> anyhow::Result<Aeron> {
    let ctx = AeronContext::new()?;
    ctx.set_dir(&dir.into_c_string())?;
    let aeron = Aeron::new(&ctx)?;
    aeron.start()?;
    Ok(aeron)
}

pub struct AeronTransport {
    ingress: AeronSubscription,
    egress: AeronPublication,
}

impl AeronTransport {
    pub fn new(aeron: &Aeron) -> anyhow::Result<Self> {
        let ingress = aeron
            .async_add_subscription(&ipc(), INGRESS, Handlers::no_available_image_handler(), Handlers::no_unavailable_image_handler())?
            .poll_blocking(TIMEOUT)?;
        let egress = aeron.async_add_publication(&ipc(), EGRESS)?.poll_blocking(TIMEOUT)?;
        Ok(AeronTransport { ingress, egress })
    }
}

impl Transport for AeronTransport {
    fn poll_ingress(&mut self, max: usize, sink: &mut dyn FnMut(&[u8])) {
        let _ = self.ingress.poll_once(|msg: &[u8], _h: AeronHeader| sink(msg), max);
    }

    /// Events are derivable from the log, so with no subscriber they are dropped, not blocked on.
    fn publish_egress(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        for _ in 0..1_000 {
            let r = self.egress.offer(bytes, Handlers::no_reserved_value_supplier_handler());
            if r > 0 {
                return Ok(());
            }
            if r == AERON_PUBLICATION_NOT_CONNECTED as i64 {
                return Ok(());
            }
            if r == AERON_PUBLICATION_CLOSED as i64 {
                bail!("egress publication closed");
            }
            std::thread::yield_now(); // back pressure / admin action
        }
        bail!("egress back-pressured for too long")
    }
}

/// The log is the set of Archive recordings of stream 1002, in recording-id order.
pub struct ArchiveJournal {
    aeron: Aeron,
    archive: AeronArchive,
    publication: AeronPublication,
    recording_id: Option<i64>,
}

impl ArchiveJournal {
    pub fn new(aeron: &Aeron) -> anyhow::Result<Self> {
        let ctx = AeronArchiveContext::new_with_no_credentials_supplier(
            aeron,
            "aeron:udp?endpoint=localhost:8010".into_c_string().as_c_str(),
            "aeron:udp?endpoint=localhost:0".into_c_string().as_c_str(),
            "aeron:udp?endpoint=localhost:8030".into_c_string().as_c_str(),
        )?;
        let archive = AeronArchiveAsyncConnect::new_with_aeron(&ctx, aeron)?.poll_blocking(TIMEOUT)?;
        let publication = aeron.async_add_publication(&ipc(), LOG)?.poll_blocking(TIMEOUT)?;
        Ok(ArchiveJournal { aeron: aeron.clone(), archive, publication, recording_id: None })
    }

    fn recordings(&self) -> anyhow::Result<Vec<i64>> {
        let mut ids = Vec::new();
        self.archive.list_recordings_for_uri(0, i32::MAX, &ipc(), LOG, |d: AeronArchiveRecordingDescriptor| ids.push(d.recording_id()))?;
        ids.sort_unstable();
        Ok(ids)
    }
}

impl Journal for ArchiveJournal {
    fn append(&mut self, sc: &SequencedCommand) -> anyhow::Result<u64> {
        if self.recording_id.is_none() {
            let before = self.recordings()?;
            self.archive.start_recording(&ipc(), LOG, SOURCE_LOCATION_LOCAL, true)?;
            let id = loop {
                let now = self.recordings()?;
                if let Some(id) = now.into_iter().find(|id| !before.contains(id)) {
                    break id;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            self.recording_id = Some(id);
        }
        let bytes = encode(sc);
        loop {
            let r = self.publication.offer(&bytes, Handlers::no_reserved_value_supplier_handler());
            if r > 0 {
                return Ok(r as u64);
            }
            if r == AERON_PUBLICATION_CLOSED as i64 {
                bail!("log publication closed");
            }
            std::thread::yield_now();
        }
    }

    fn wait_durable(&mut self, pos: u64) -> anyhow::Result<()> {
        let id = self.recording_id.ok_or_else(|| anyhow!("no active recording"))?;
        while (self.archive.get_recording_position(id)? as u64) < pos {
            std::thread::yield_now();
        }
        Ok(())
    }

    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<()> {
        for id in self.recordings()? {
            let stop = self.archive.get_stop_position(id)?;
            if stop <= 0 {
                continue; // empty recording
            }
            let sub = self
                .aeron
                .async_add_subscription(&ipc(), REPLAY, Handlers::no_available_image_handler(), Handlers::no_unavailable_image_handler())?
                .poll_blocking(TIMEOUT)?;
            let params = AeronArchiveReplayParams::new(0, i32::MAX, 0, stop, 0, 0)?;
            self.archive.start_replay(id, &ipc(), REPLAY, &params)?;
            let mut consumed = 0i64;
            while consumed < stop {
                sub.poll_once(
                    |msg: &[u8], h: AeronHeader| {
                        consumed = h.position();
                        match decode::<SequencedCommand>(msg) {
                            Ok(sc) => f(sc),
                            Err(e) => tracing::error!(?e, "corrupt log record"),
                        }
                    },
                    256,
                )?;
            }
        }
        Ok(())
    }
}
```
**Note:** if `get_stop_position` returns "still active" (`-1`) for the recording of a crashed node, use `get_recording_position(id)` as the stop position instead. The spike shows which applies.

- [ ] **Step 3: Write the binary**

`crates/rfq-node/src/main.rs`:
```rust
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rfq_core::scenarios::demo_config;
use rfq_core::testkit::genesis;
use rfq_node::aeron::{connect, ArchiveJournal, AeronTransport};
use rfq_node::journal::{FileJournal, Journal};
use rfq_node::node::{hex, Node};

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }

fn run<J: Journal>(journal: J, transport: AeronTransport) -> anyhow::Result<()> {
    let hash_every_batch = std::env::var("RFQ_HASH_EVERY_BATCH").as_deref() == Ok("1");
    let mut node = Node::start(journal, transport, genesis(demo_config()))?;
    loop {
        let n = node.poll(now_ms())?;
        if n == 0 {
            std::thread::sleep(Duration::from_millis(1));
        } else if hash_every_batch {
            tracing::info!(seq = node.engine().last_seq(), hash = %hex(&node.state_hash()), "applied");
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let dir = std::env::var("AERON_DIR").unwrap_or_else(|_| "/dev/shm/aeron".into());
    let aeron = connect(&dir)?;
    let transport = AeronTransport::new(&aeron)?;
    match std::env::var("RFQ_JOURNAL").unwrap_or_else(|_| "archive".into()).as_str() {
        "archive" => run(ArchiveJournal::new(&aeron)?, transport),
        other => {
            let path = other.strip_prefix("file:").ok_or_else(|| anyhow::anyhow!("RFQ_JOURNAL must be archive or file:<path>"))?;
            run(FileJournal::open(path.as_ref())?, transport)
        }
    }
}
```
The demo genesis uses `demo_config()` so the scripted scenarios run in seconds. The default `Config` is what the tests use.

- [ ] **Step 4: Build**

Run: `cargo build -p rfq-node`
Expected: compiles. On macOS the `precompile` feature should link without cmake; if it doesn't, build inside Docker only (Task 11).

- [ ] **Step 5: Commit**

```bash
git add crates/rfq-node
git commit -m "feat(node): Aeron transport and Archive-backed journal; rfq-node binary"
```

---

### Task 11: Demo client, Docker Compose and crash-recovery script

**Files:**
- Create: `crates/rfq-demo/Cargo.toml`, `crates/rfq-demo/src/main.rs`, `docker/rust.Dockerfile`, `docker-compose.yml`, `scripts/demo.sh`
- Modify: `Cargo.toml` (members)

**Interfaces:**
- Consumes: `scenarios::*`, `codec::*`, `rfq_node::aeron::{connect, INGRESS, EGRESS}`.

- [ ] **Step 1: Write the demo client**

`crates/rfq-demo/Cargo.toml`:
```toml
[package]
name = "rfq-demo"
version = "0.1.0"
edition = "2021"

[dependencies]
rfq-core = { path = "../rfq-core" }
rfq-node = { path = "../rfq-node" }
anyhow = { workspace = true }
rusteron-archive = { version = "=<version from Task 0>", features = ["static", "precompile"] }
```
`crates/rfq-demo/src/main.rs`:
```rust
//! Runs every scenario against a live rfq-node over Aeron and checks the expected event order.
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::bail;
use rfq_core::codec::{decode, encode, ClientCommand, EventBatch};
use rfq_core::scenarios::{all, contains_in_order, Ctx, StepKind};
use rfq_core::{AccountId, Emitted};
use rfq_node::aeron::{connect, EGRESS, INGRESS};
use rusteron_archive::*;

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }

fn main() -> anyhow::Result<()> {
    let only = std::env::args().nth(1);
    let aeron = connect(&std::env::var("AERON_DIR").unwrap_or_else(|_| "/dev/shm/aeron".into()))?;
    let ipc = "aeron:ipc".into_c_string();
    let ingress = aeron.async_add_publication(&ipc, INGRESS)?.poll_blocking(Duration::from_secs(10))?;
    let egress = aeron
        .async_add_subscription(&ipc, EGRESS, Handlers::no_available_image_handler(), Handlers::no_unavailable_image_handler())?
        .poll_blocking(Duration::from_secs(10))?;
    let nonce_base = now_ms() * 1_000; // strictly above any previous run's nonces
    let mut nonces: BTreeMap<AccountId, u64> = BTreeMap::new();
    let mut failed = 0;

    for s in all().into_iter().filter(|s| only.as_deref().map_or(true, |o| o == s.name)) {
        let t0 = now_ms() + 500;
        let mut ctx = Ctx::new(t0);
        let mut events: Vec<Emitted> = Vec::new();
        let mut drain = |events: &mut Vec<Emitted>, ctx: &mut Ctx, want: Option<(AccountId, u64)>, until: Instant| -> anyhow::Result<Option<u64>> {
            let mut found = None;
            while Instant::now() < until && found.is_none() {
                egress.poll_once(|msg: &[u8], _h: AeronHeader| {
                    if let Ok(b) = decode::<EventBatch>(msg) {
                        ctx.observe(&b.events);
                        if want.is_some() && b.origin == want { found = Some(b.seq); }
                        events.extend(b.events);
                    }
                }, 64)?;
                if want.is_none() { std::thread::sleep(Duration::from_millis(1)); }
            }
            Ok(found)
        };
        println!("== {} ==", s.name);
        for step in &s.steps {
            let at = t0 + step.at;
            while now_ms() < at {
                drain(&mut events, &mut ctx, None, Instant::now() + Duration::from_millis(1))?;
            }
            if let StepKind::Send { label, actor, action } = &step.kind {
                let n = nonces.entry(*actor).or_insert(nonce_base);
                *n += 1;
                let cmd = ClientCommand { actor: *actor, nonce: *n, action: action(&ctx) };
                let bytes = encode(&cmd);
                while ingress.offer(&bytes, Handlers::no_reserved_value_supplier_handler()) <= 0 {
                    std::thread::yield_now();
                }
                match drain(&mut events, &mut ctx, Some((*actor, *n)), Instant::now() + Duration::from_secs(5))? {
                    Some(seq) => ctx.record_label(*label, seq),
                    None => bail!("{}: no response to step {label}", s.name),
                }
            }
        }
        drain(&mut events, &mut ctx, None, Instant::now() + Duration::from_millis(1_000))?;
        for e in &events {
            println!("  {:?} {:?}", e.audience, e.event);
        }
        match contains_in_order(&events, &s.expect) {
            Ok(()) => println!("PASS {}", s.name),
            Err(e) => { println!("FAIL {}: {e}", s.name); failed += 1; }
        }
    }
    if failed > 0 { bail!("{failed} scenario(s) failed") }
    Ok(())
}
```
`ctx.record_label` runs after the response arrives, so a step's own action closure must never refer to its own label. The scenario steps already follow that rule.

Add `"crates/rfq-demo"` to the workspace `members`.

- [ ] **Step 2: Docker files**

`docker/rust.Dockerfile`:
```dockerfile
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
ARG BIN
RUN cargo build --release --bin ${BIN} && cp target/release/${BIN} /app

FROM debian:bookworm-slim
COPY --from=build /app /usr/local/bin/app
ENTRYPOINT ["/usr/local/bin/app"]
```
If Task 0 needed apt packages, add them to the build stage.

`docker-compose.yml`:
```yaml
services:
  aeron:
    build: docker/aeron
    ipc: shareable
    shm_size: 512m
    volumes: [archive:/data/archive]
  engine:
    build: { context: ., dockerfile: docker/rust.Dockerfile, args: { BIN: rfq-node } }
    ipc: "service:aeron"
    environment: { RFQ_JOURNAL: archive, RFQ_HASH_EVERY_BATCH: "1" }
    depends_on: [aeron]
  demo:
    build: { context: ., dockerfile: docker/rust.Dockerfile, args: { BIN: rfq-demo } }
    ipc: "service:aeron"
    depends_on: [engine]
    profiles: [demo]
volumes:
  archive: {}
```

- [ ] **Step 3: Write the demo and crash-recovery script**

`scripts/demo.sh`:
```bash
#!/usr/bin/env bash
# Runs all scenarios over Aeron, then kills the engine and proves replay restores the same state.
set -euo pipefail
cd "$(dirname "$0")/.."

docker compose down -v --remove-orphans
docker compose up -d --build aeron engine
sleep 3

docker compose run --rm demo

before=$(docker compose logs engine | grep -E 'applied|replayed' | tail -1 | sed -E 's/.*seq=([0-9]+).*hash=([0-9a-f]+).*/\1 \2/')
echo "before crash: seq+hash = $before"

docker compose kill engine
docker compose up -d engine
sleep 3

after=$(docker compose logs engine | grep replayed | tail -1 | sed -E 's/.*seq=([0-9]+).*hash=([0-9a-f]+).*/\1 \2/')
echo "after replay: seq+hash = $after"

if [ "$before" = "$after" ]; then echo "RECOVERY OK"; else echo "RECOVERY MISMATCH"; exit 1; fi
```
```bash
chmod +x scripts/demo.sh
```

- [ ] **Step 4: Run it end to end**

Run: `./scripts/demo.sh`
Expected: six `PASS <scenario>` lines (happy_path, mm_withdraws_after_selection, no_quotes_expire, requester_lapses, disputed_ruled_invalid, oracle_silent_hard_deadline), then `RECOVERY OK`.

If the order of `PositionSettled` and other events across scenarios gets interleaved, that's expected, because `contains_in_order` tolerates interleaving. A `FAIL` means a real ordering bug: debug it with `cargo test -p rfq-core --test scenarios` first.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/rfq-demo docker docker-compose.yml scripts
git commit -m "feat(demo): scenarios over Aeron in Docker with crash-recovery replay check"
```

---

### Task 12: Docs sync and README

**Files:**
- Create: `README.md`
- Modify: `docs/specs/failure-modes.md` (test names), `docs/README.md`

- [ ] **Step 1: Check that the failure-mode notes match the code**

Run:
```bash
# The Test column (5th '|' field) of the R1–R8 race table.
grep -E '^\| R[0-9]' docs/specs/failure-modes.md | awk -F'|' '{print $5}' | grep -oE '[a-z_]+' | sort -u | while read t; do
  grep -rq "fn $t" crates || echo "missing test: $t"
done
```
Expected: no output. Rename the tests or update the doc until nothing is missing. The reviewers will compare this list against the code.

- [ ] **Step 2: Write the README**

`README.md`:
````markdown
# RFQ Matching & Settlement Engine

Deterministic RFQ engine for binary-outcome contracts. Design docs: [docs/README.md](docs/README.md).

## Run the tests (no Docker needed)
```bash
cargo test
```

## Run the end-to-end demo (Docker)
```bash
./scripts/demo.sh
```
It runs the happy path and five failure paths over Aeron, then kills the engine and checks that replaying the Aeron Archive log restores the identical state hash.

## Layout
- `crates/rfq-core`: the engine (pure; all money logic and invariants)
- `crates/rfq-node`: sequencer, journals (Aeron Archive / file / memory), Aeron transport
- `crates/rfq-demo`: scenario client over Aeron
````

- [ ] **Step 3: Final verification**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && ./scripts/demo.sh`
Expected: all tests pass, clippy is clean, six PASS lines, `RECOVERY OK`.

- [ ] **Step 4: Commit**

```bash
git add README.md docs
git commit -m "docs: README and failure-mode notes synced with tests"
git push
```

---

### Task 13: Quality gates (dependency audit, benchmark, CI)

**Files:**
- Create: `deny.toml`, `crates/rfq-core/benches/apply.rs`, `.github/workflows/ci.yml`
- Modify: `Cargo.toml` (criterion), `crates/rfq-core/Cargo.toml` (bench target), `docs/architecture/overview.md` §8 (measured numbers), `README.md` (CI badge)

**Interfaces:**
- Consumes: `Engine`, `testkit::{accounts, dollars, genesis, price}`.

- [ ] **Step 1: Dependency audit**

```bash
cargo install cargo-deny --locked
```
`deny.toml`:
```toml
[advisories]
version = 2
yanked = "deny"

[licenses]
version = 2
allow = ["MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Unicode-3.0", "Zlib"]

[bans]
multiple-versions = "warn"

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```
Run: `cargo deny check`
Expected: `advisories ok, bans ok, licenses ok, sources ok`. If a licence fails, add it only after checking that it's permissive, and note why in a comment. If an advisory fails, upgrade the crate, or add an `ignore` entry with the advisory id and the reason it doesn't apply.

- [ ] **Step 2: Write the benchmark**

Add `criterion = "0.5"` to `[workspace.dependencies]`. In `crates/rfq-core/Cargo.toml` add:
```toml
[dev-dependencies]
criterion = { workspace = true }

[[bench]]
name = "apply"
harness = false
```
`crates/rfq-core/benches/apply.rs`:
```rust
//! Engine throughput: replay a ~120k-command log of full RFQ rounds; cost of state_hash.
use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use rfq_core::testkit::{accounts::*, dollars, genesis, price};
use rfq_core::*;

const ROUNDS: u64 = 20_000; // 6 commands per round

struct Builder {
    engine: Engine,
    log: Vec<SequencedCommand>,
    seq: u64,
    nonce: u64,
}

impl Builder {
    fn push(&mut self, time: Timestamp, cmd: Command) {
        self.seq += 1;
        let sc = SequencedCommand { seq: self.seq, time, cmd };
        self.engine.apply(&sc);
        self.log.push(sc);
    }

    /// One global nonce counter is strictly increasing per actor too.
    fn user(&mut self, time: Timestamp, actor: AccountId, action: Action) {
        self.nonce += 1;
        self.push(time, Command::User { actor, nonce: self.nonce, action });
    }
}

fn build_log() -> Vec<SequencedCommand> {
    let mut b = Builder { engine: Engine::new(), log: Vec::new(), seq: 0, nonce: 0 };
    b.push(0, Command::Genesis(genesis(Config::default())));
    for a in [ALICE, MM1, MM2] {
        b.user(0, OPERATOR, Action::Deposit { to: a, amount: dollars(1_000_000_000) });
    }
    let c = ContractId(1);
    b.user(0, OPERATOR, Action::ListContract {
        id: c,
        description: "bench".into(),
        oracle: ORACLE,
        trading_close: u64::MAX / 4,
        resolution_deadline: u64::MAX / 2,
        dispute_window: 1,
    });
    let mut t = 0;
    for _ in 0..ROUNDS {
        t += 2_000;
        let r = RequestId(b.seq + 1);
        b.user(t, ALICE, Action::SubmitRequest {
            legs: vec![LegSpec { contract: c, side: Side::BuyYes, qty: 10, limit_price: price(7_000) }],
            response_deadline: t + 1_000,
        });
        b.user(t, MM1, Action::SubmitQuote { request: r, leg: 0, price: price(6_100), size: 10, expires_at: t + 100_000 });
        let q = QuoteId(b.seq + 1);
        b.user(t, MM2, Action::SubmitQuote { request: r, leg: 0, price: price(6_000), size: 10, expires_at: t + 100_000 });
        b.push(t + 1_000, Command::Tick);
        b.user(t + 1_000, ALICE, Action::Accept { request: r, picks: vec![Pick { leg: 0, quote: q, price: price(6_000) }] });
    }
    // Each round is request, quote, quote, tick, accept: the last request is seq - 4.
    // If this fails, the workload is being silently rejected and the numbers are meaningless.
    assert_eq!(b.engine.request(RequestId(b.seq - 4)).map(|r| r.state), Some(RequestState::Filled));
    b.log
}

fn engine(c: &mut Criterion) {
    let log = build_log();
    let mut g = c.benchmark_group("engine");
    g.sample_size(10);
    g.throughput(Throughput::Elements(log.len() as u64));
    g.bench_function("replay_log", |bench| {
        bench.iter_batched(
            Engine::new,
            |mut e| {
                for sc in &log {
                    e.apply(sc);
                }
                e
            },
            BatchSize::LargeInput,
        )
    });
    let mut full = Engine::new();
    for sc in &log {
        full.apply(sc);
    }
    g.throughput(Throughput::Elements(1));
    g.bench_function("state_hash", |bench| bench.iter(|| full.state_hash()));
    g.finish();
}

criterion_group!(benches, engine);
criterion_main!(benches);
```

- [ ] **Step 3: Run it and record the numbers**

Run: `cargo bench -p rfq-core --bench apply`
Expected: criterion prints `replay_log` throughput in elements/s (commands per second) and the `state_hash` time.

Replace the first scaling bullet in `docs/architecture/overview.md` §8 with the measured value, keeping the machine and conditions honest:
```markdown
- **Measured:** `Engine::apply` replays a 120k-command log of full RFQ rounds at **<N> commands/s** on <machine> (single thread, release build, `overflow-checks` on); `state_hash` over that state takes <T> ms. Run `cargo bench -p rfq-core` to reproduce.
```
If the throughput is surprisingly low (under ~100k commands/s), profile before claiming anything: `cargo install flamegraph && cargo flamegraph --bench apply -p rfq-core -- --bench`.

- [ ] **Step 4: CI workflow**

`.github/workflows/ci.yml`:
```yaml
name: ci
on:
  push:
    branches: [main]
  pull_request:

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace
      - run: cargo bench -p rfq-core --no-run
  deny:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2
```
If Task 0 needed extra apt packages to build `rusteron`, add `- run: sudo apt-get update && sudo apt-get install -y <packages>` before the clippy step.

Add the badge as the README's second line:
```markdown
[![ci](https://github.com/hmdsefi/rfq/actions/workflows/ci.yml/badge.svg)](https://github.com/hmdsefi/rfq/actions/workflows/ci.yml)
```

- [ ] **Step 5: Commit, push, and check CI**

```bash
git add Cargo.toml crates/rfq-core deny.toml .github README.md docs/architecture/overview.md
git commit -m "ci: cargo-deny, engine benchmark, GitHub Actions"
git push
gh run watch --exit-status
```
Expected: both jobs pass.

---

### Task 14: Final review and publish

**Files:**
- Create: `docs/reviews/code-review.md`
- Modify: whatever the confirmed findings touch

- [ ] **Step 1: Whole-branch code review**

Run the `code-review` skill at `high` effort against all commits since the Setup commit:
```text
/code-review high <setup-commit-sha>..HEAD
```
Get the sha from `git log --reverse --format=%h | head -1`.

- [ ] **Step 2: Security review**

Run the `security-review` skill over the same range. Ask it to focus on: money paths in `rfq-core` (ledger, accept, finalize), untrusted input on ingress (codec limits, `ClientCommand`), journal and replay integrity, and the Docker/Aeron setup (shared `/dev/shm`, exposed ports).

- [ ] **Step 3: Triage and fix**

Record every finding in `docs/reviews/code-review.md` (`# | source | severity | finding | decision | commit`). For each confirmed bug: write the failing test first, fix it, and add the defence to `docs/specs/failure-modes.md` if it's a race or partial failure. Re-run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo deny check && ./scripts/demo.sh`.

```bash
git add -A && git commit -m "fix: address final code and security review findings" && git push
gh run watch --exit-status
```

- [ ] **Step 4: Share the repo with the reviewers**

**Confirm with the user first**, and get the reviewers' GitHub usernames. Keep the repo private: it contains the exercise brief PDF, which belongs to Outcome Labs.
```bash
for u in <reviewer-1> <reviewer-2>; do
  gh api -X PUT "repos/hmdsefi/rfq/collaborators/$u" -f permission=pull
done
gh api repos/hmdsefi/rfq/invitations --jq '.[].invitee.login'
```
Expected: every reviewer is listed as invited. If they ever ask for a public repo instead, run `git rm docs/RFQ_Technical_Exercise.pdf` and push before changing visibility.
