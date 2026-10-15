# RFQ Matching & Settlement Engine
[![ci](https://github.com/hmdsefi/rfq/actions/workflows/ci.yml/badge.svg)](https://github.com/hmdsefi/rfq/actions/workflows/ci.yml)

A deterministic matching and settlement engine for a permissionless RFQ venue for binary-outcome contracts, in Rust. A requester asks for a price, market makers answer with firm quotes, the best quote is presented, and on acceptance both sides' collateral locks in escrow until an oracle resolves the contract. Real money sits in every intermediate state and every participant is assumed adversarial, so the design goal is that funds can never be lost, duplicated or stuck without an exit.

Design docs: [docs/README.md](docs/README.md). The brief (the exercise PDF) is not in the repository: it belongs to the company that set the exercise.

The repository is private (`hmdsefi/rfq`), so you need access first. Then: `git clone https://github.com/hmdsefi/rfq.git && cd rfq`.

> **CI status, 2026-10-14.** The badge is red, and not because of the code. GitHub refuses to start any job of this private repository ("recent account payments have failed or your spending limit needs to be increased"), so no workflow step has ever run. The same checks pass locally: `cargo fmt --all --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`, `cargo test --locked --workspace` (all pass), `cargo deny check`, the Docker builds of both binaries, and `./scripts/demo.sh`. The workflow is [.github/workflows/ci.yml](.github/workflows/ci.yml).

## Quick start

### Run the tests (no Docker)

```bash
cargo test --workspace
```

The toolchain is pinned to Rust 1.99.0 in `rust-toolchain.toml`, so `rustup` installs it on first use. This never builds Aeron: the Aeron adapters sit behind the off-by-default cargo feature `aeron`. The tests are:

- unit and behaviour tests for the ledger and every command, including the race and partial-failure cases ([failure-modes.md](docs/specs/failure-modes.md) names the test for each one);
- the six scripted scenarios of the demo, run in memory, and replay tests that rebuild the engine from its log and compare `state_hash`;
- two property tests that run random command streams and check the invariants I1–I5 after every command;
- node tests over the in-memory and file journals: startup replay, write-ahead order, restart, `Resume` after a stall, failure latch.

Engine benchmark:

```bash
cargo bench -p rfq-domain    # engine throughput and state_hash cost
```

The `#[ignore]`d tests that drive the Aeron adapters against a real media driver need the Aeron build tools and a driver, so they run in Docker. The recipe is in the header of [`crates/rfq-infra/tests/aeron_live.rs`](crates/rfq-infra/tests/aeron_live.rs).

### Run the end-to-end demo (Docker)

You need Docker Desktop (Compose v2). This was verified on Docker Desktop for Apple Silicon (linux/aarch64). It has not been run on amd64 or on a Linux host.

**Warning:** after building the images, the script runs `docker compose --profile demo down -v --remove-orphans`, which deletes the containers and the Archive volume of its compose project (`rfq`). To keep another stack, run it under its own project name: `COMPOSE_PROJECT_NAME=rfq-demo ./scripts/demo.sh`.

**Upgrading from an older checkout:** run `docker compose down -v` once first. An `archive` volume that an older, root-run stack created is root-owned, so the non-root Archive cannot open it (`Permission denied`: Docker retries the container 3 times, then leaves it down, and `docker compose logs aeron` shows why). `./scripts/demo.sh` already runs `down -v`, so it is unaffected.

```bash
./scripts/demo.sh
```

The first run compiles Aeron C from source inside the build image (it needs CMake 3.30, clang and a JDK), which takes several minutes. Later runs reuse Docker's layer cache. The script:

1. builds the images and starts a fresh stack: the Java Aeron media driver and Archive, then the engine (`rfq-node`);
2. runs the six scenarios through the demo client (`rfq-demo`) over Aeron, printing each scenario's events and a `PASS <name>` line, then `passed 6/6`;
3. `SIGKILL`s the engine, restarts it, and checks that replaying the Archive journal rebuilds exactly the state it had: the last state hash the engine logged before the kill must equal the hash it logs after the replay. It prints `RECOVERY OK`.

The scenarios are the happy path (`happy_path`) and five failure paths: `mm_cannot_withdraw_after_selection` (a selected quote is firm), `no_quotes_expire`, `requester_lapses` (no accept inside the window), `disputed_ruled_invalid`, and `oracle_silent_hard_deadline`. The scenario scripts are in [`crates/rfq-testkit/src/scenarios.rs`](crates/rfq-testkit/src/scenarios.rs). The demo node runs with short windows (`demo_config()`: 2 s accept window) so the scenarios take under a minute.

Afterwards the stack is still running:

```bash
docker compose run --rm demo [scenario]     # run all scenarios again, or just one
docker compose --profile demo down -v       # stop everything and delete the Archive volume
```

`RFQ_DEMO_TIMEOUT` (seconds, default 120) is how long the script waits for the engine at each step. Docker Compose details are in [transport-aeron.md](docs/specs/transport-aeron.md#docker-compose). Without the `aeron` feature, `rfq-node`, `rfq-demo` and `rfq-gateway` only print that Aeron is missing and exit with status 2.

### Gateway (WebSocket)

`rfq-gateway` is the client edge ([gateway.md](docs/specs/gateway.md)): JSON over a WebSocket on `ws://127.0.0.1:18080`, published on the host's loopback only. It puts each client's commands on ingress with the actor's next nonce, and sends every egress batch back, filtered for the connection's view (`admin`, or one account), with a `result` per command and a `status` when the engine goes down or comes back. Start it next to the engine, then drive one trade through it with the smoke script (Node 22 or later):

```bash
docker compose up -d aeron engine gateway
node scripts/gateway-smoke.mjs                # prints GATEWAY SMOKE OK
docker compose logs -f gateway engine         # every command, result and status change
```

To publish it on another host port, set `RFQ_GATEWAY_PORT` (for example `RFQ_GATEWAY_PORT=19000 docker compose up -d gateway`) and pass the URL: `node scripts/gateway-smoke.mjs ws://127.0.0.1:19000` (or set `RFQ_GATEWAY_URL`).

### Live dashboard

A single-page demo that drives the real stack through the gateway: every command goes through Aeron to the engine, and every number on screen comes back from it ([DESIGN §11](tools/dashboard/DESIGN.md), [gateway.md](docs/specs/gateway.md)).

```bash
docker compose up -d aeron engine gateway     # the gateway listens on ws://127.0.0.1:18080
tools/dashboard/build.sh                      # builds the page and runs its test suites
open tools/dashboard/dist/rfq-dashboard.html  # the status bar says "Connected", "engine up"
```

- **Scenarios.** Pick one of the 12 (six brief scenarios, six attacks) and press **Run**, **Run ×N** or **Run all**. The page acts as every participant through the gateway and judges PASS or FAIL against the expected events. Panels: lanes, contract, ledger, trace, a timeline scrubber, latency, and **View as** (what one account receives). `node tools/dashboard/web/live-smoke.mjs` runs the 12 and `happy_path` ×20 from a terminal.
- **Engine crash:** `docker compose kill engine` during a run, then `docker compose start engine`. The status goes "engine down", the command in flight reports `outcome unknown … nonce N`, then `Resume`, "engine up" and a retry with the same nonce; `docker compose logs engine` shows `replayed seq=… hash=…`. Runs that start while it is down fail at once (`engine unavailable`).
- **Stall:** `docker compose pause engine; sleep 2; docker compose unpause engine`. A `Resume` batch follows and open dispute windows are extended, so a run inside a window can fail on timing.
- **Driver crash:** `docker compose kill aeron`. The engine and gateway exit 1 and the page shows "Reconnecting". A `kill` is a manual stop, so Docker does not restart anything: run `docker compose up -d aeron engine gateway`; the page reconnects without a reload and the engine replays to the hash it had ([architecture §5](docs/architecture/overview.md) says what restarts by itself).

## Layout

Code is split into hexagonal layers, and dependencies point inward only: crate boundaries enforce it ([architecture §0](docs/architecture/overview.md)).

| Crate | Layer | What it holds |
|---|---|---|
| [`rfq-domain`](crates/rfq-domain) | Domain, pure | Money types, `Ledger`, requests, quotes, contracts, positions, `Command` and `Event`, and the `Engine` state machine with its invariants. All money logic lives here. No I/O, no clock, no randomness. `#![forbid(unsafe_code)]`. |
| [`rfq-app`](crates/rfq-app) | Application | The ports `Journal` and `Transport`, the protocol types `ClientCommand` and `EventBatch`, the `Sequencer`, and `Node`, the write-ahead loop: sequence, append, wait durable, apply, publish. |
| [`rfq-infra`](crates/rfq-infra) | Infrastructure | Adapters for the ports: the versioned `codec`, `FileJournal`, `MemJournal`, `MemTransport`; with feature `aeron`, `AeronTransport` and `ArchiveJournal` (Aeron Archive). |
| [`rfq-node`](crates/rfq-node) | Composition root (binary) | Reads its configuration from the environment, wires an Aeron transport and a journal into `Node`, and runs the loop. |
| [`rfq-demo`](crates/rfq-demo) | Client (binary) | Runs the scripted scenarios against a live node over Aeron and checks the events it gets back. |
| [`rfq-gateway`](crates/rfq-gateway) | Client edge (library and binary) | The WebSocket gateway. The library is pure (protocol, per-viewer filter, nonce book, hub state machine); the binary adds the Aeron adapter, the WebSocket server and the hub loop. |
| [`rfq-testkit`](crates/rfq-testkit) | Test support | The test `Harness`, well-known accounts, and the scripted scenarios (shared by the tests and the demo). |

## Where each deliverable is

| | Deliverable | Where |
|---|---|---|
| D1 | State machine | [specs/engine.md](docs/specs/engine.md) §6, with the multi-leg "leg 2 of 3" walk-through in §8 |
| D2 | Working implementation | `crates/`, `cargo test --workspace`, and `./scripts/demo.sh` (this README) |
| D3 | Failure-mode notes | [specs/failure-modes.md](docs/specs/failure-modes.md) (races and partial failures). It links to a companion, [specs/adversarial.md](docs/specs/adversarial.md), the adversarial analysis (attackers). |
| D4 | Resolution design | [specs/resolution.md](docs/specs/resolution.md) |
| D5 | Design note (quotes living seconds vs days) | [specs/quote-lifetime.md](docs/specs/quote-lifetime.md) |

Also: the product and technical decisions in [rfc/0001-rfq-engine.md](docs/rfc/0001-rfq-engine.md), the adversarial review of the spec in [reviews/spec-review.md](docs/reviews/spec-review.md), and the implementation plan in [docs/plans/](docs/plans/2026-10-13-rfq-engine-plan.md).

## Design at a glance

- **Exact money.** Amounts are `u64` micro-USD and prices are `u16` basis points. The buyer locks `qty × bps × 100` and the counterparty locks the complement, so the two always sum to `qty × $1` with no rounding. All arithmetic is checked; overflow rejects the command instead of wrapping.
- **Deterministic, replayable.** One sequencer totally orders every command and stamps its time. The engine never reads a clock or randomness and iterates only `BTreeMap`/`BTreeSet`. State is the replay of the log, and `state_hash` proves it.
- **Money only moves through the ledger,** and every handler validates before it mutates. Five invariants (conservation, holds, escrow, ownership, no dead ends) are checked after every command in the tests.
- **Adversarial by default.** Sealed bids, firm quotes once selected, strict per-actor nonces, a bond for disputes, role separation for oracles, a hard deadline so no money is stuck forever. The remaining risks are listed with their production defences in [adversarial.md](docs/specs/adversarial.md).
- **Write-ahead.** Nothing is applied or published until its command is durable, meaning fsynced, in the journal: Aeron Archive with `aeron.archive.file.sync.level=2`, or a local file with `sync_data`. That costs an fsync on the write-ahead path, one wait per batch. Egress is a best-effort notification stream; anything that must happen exactly once reads the journal.

## Measured numbers

`cargo bench -p rfq-domain` replays a 100,005-command log (20,000 full RFQ rounds: request, two quotes, tick, accept) through `Engine::apply` at about **3.7 million commands/s** (about 270 ns each), and `state_hash` over the resulting state takes about **11.9 ms**. This is the engine alone: Apple M4 Max, macOS 27.0.1, rustc 1.99.0, release profile with overflow checks on, one thread, 3 funded accounts and 1 contract, with no codec, no fsync and no Aeron, measured while other builds kept the machine busy. End-to-end throughput over Aeron was not measured. Details and caveats: [architecture §8](docs/architecture/overview.md).

## Not built

Multi-leg execution is designed but not built (the data model is `Vec<Leg>`, and the engine rejects any request with `legs.len() != 1`, see [engine.md §8](docs/specs/engine.md)). Also designed only: snapshots and pruning, Aeron Cluster for HA, on-chain escrow and effect workers, and authentication. (The gateway is built, but it trusts the `actor` a client names: [gateway.md §7](docs/specs/gateway.md).) The brief puts auth, real chain integration and pricing out of scope.

## License

Mozilla Public License 2.0 (MPL-2.0), see [LICENSE](LICENSE).

Copyright (c) 2026 hmdsefi (https://github.com/hmdsefi/rfq) <hdyousefi@gmail.com>. See [COPYRIGHT](COPYRIGHT).
