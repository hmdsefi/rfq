# RFQ Matching & Settlement Engine
[![ci](https://github.com/hmdsefi/rfq/actions/workflows/ci.yml/badge.svg)](https://github.com/hmdsefi/rfq/actions/workflows/ci.yml)

A deterministic matching and settlement engine for a permissionless RFQ venue for binary-outcome contracts, in Rust. A requester asks for a price, market makers answer with firm quotes, the best quote is presented, and on acceptance both sides' collateral locks in escrow until an oracle resolves the contract. Real money sits in every intermediate state and every participant is assumed adversarial, so the design goal is that funds can never be lost, duplicated or stuck without an exit.

Design docs: see [Documentation](#documentation) below, or the index in [docs/README.md](docs/README.md). The brief (the exercise PDF) is not in the repository: it belongs to the company that set the exercise.

The repository is private (`hmdsefi/rfq`), so you need access first. Then: `git clone https://github.com/hmdsefi/rfq.git && cd rfq`.

> **CI status, 2026-10-14.** The badge is red, and not because of the code. GitHub refuses to start any job of this private repository ("recent account payments have failed or your spending limit needs to be increased"), so no workflow step has ever run. The same checks pass locally: `cargo fmt --all --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`, `cargo test --locked --workspace` (all pass), `cargo deny check`, the Docker builds of both binaries, and `./scripts/demo.sh`. The workflow is [.github/workflows/ci.yml](.github/workflows/ci.yml).

## Contents

- [Quick start](#quick-start)
  - [Run the tests (no Docker)](#run-the-tests-no-docker)
  - [Run the end-to-end demo (Docker)](#run-the-end-to-end-demo-docker)
  - [Gateway (WebSocket)](#gateway-websocket)
  - [Live dashboard](#live-dashboard)
  - [HTTP API](#http-api)
- [Layout](#layout): the crates and their layers
- [Where each deliverable is](#where-each-deliverable-is): D1–D5
- [Documentation](#documentation): every design doc, and what it answers
- [Design at a glance](#design-at-a-glance)
- [Measured numbers](#measured-numbers)
- [Not built](#not-built)
- [License](#license)

## Quick start

### Run the tests (no Docker)

```bash
cargo test --workspace
```

The toolchain is pinned to Rust 1.99.0 in `rust-toolchain.toml`, so `rustup` installs it on first use. This never builds Aeron: the Aeron adapters sit behind the off-by-default cargo feature `aeron`. The tests are:

- unit and behaviour tests for the ledger and every command, including the race and partial-failure cases ([failure-modes.md](docs/specs/failure-modes.md) names the test for each one) and the multi-leg atomicity rules ([`multi_leg.rs`](crates/rfq-domain/tests/multi_leg.rs));
- the eight scripted scenarios of the demo (six for the brief's lifecycle and failure cases, two for its multi-leg case), run in memory, and replay tests that rebuild the engine from its log and compare `state_hash`;
- three property tests that run random command streams and check the invariants I1–I5 after every command (the third generates multi-leg requests, and also checks that a request fills all of its legs or none);
- node tests over the in-memory and file journals: startup replay, write-ahead order, restart, `Resume` after a stall, failure latch;
- read-side tests: every scenario is run through a node, and a replica fed only its journal must reproduce the node's batches and `state_hash` (also across a restart); the read views and their sealing rules, the HTTP routes and statuses, and a write's round trip through the hub.

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
2. runs the eight scenarios through the demo client (`rfq-demo`) over Aeron, printing each scenario's events and a `PASS <name>` line, then `passed 8/8`;
3. `SIGKILL`s the engine, restarts it, and checks that replaying the Archive journal rebuilds exactly the state it had: the last state hash the engine logged before the kill must equal the hash it logs after the replay. It prints `RECOVERY OK`.

The scenarios are the happy path (`happy_path`); five failure paths: `mm_cannot_withdraw_after_selection` (a selected quote is firm), `no_quotes_expire`, `requester_lapses` (no accept inside the window), `disputed_ruled_invalid`, and `oracle_silent_hard_deadline`; and two multi-leg paths on three markets: `multi_leg_happy` (a quote on every leg, and one accept fills all three) and `multi_leg_leg2_fails` (leg 2 of 3 has no quote, so the whole request expires and every hold is released in one command). The scenario scripts are in [`crates/rfq-testkit/src/scenarios.rs`](crates/rfq-testkit/src/scenarios.rs). The demo node runs with short windows (`demo_config()`: 2 s accept window) so the scenarios take under a minute.

Afterwards the stack is still running:

```bash
docker compose run --rm demo [scenario]     # run all scenarios again, or just one
docker compose --profile demo down -v       # stop everything and delete the Archive volume
```

`RFQ_DEMO_TIMEOUT` (seconds, default 120) is how long the script waits for the engine at each step. Docker Compose details are in [transport-aeron.md](docs/specs/transport-aeron.md#docker-compose). Without the `aeron` feature, `rfq-node`, `rfq-demo` and `rfq-gateway` only print that Aeron is missing and exit with status 2.

### Gateway (WebSocket)

`rfq-gateway` is the client edge ([gateway.md](docs/specs/gateway.md)): JSON over a WebSocket on `ws://127.0.0.1:18080`, published on the host's loopback only. It puts each client's commands on ingress with the actor's next nonce, and sends every egress batch back, filtered for the connection's view (`admin`, or one account), with a `result` per command and a `status` when the engine goes down or comes back. The same process serves the [HTTP API](#http-api) on port 18081. Start it next to the engine, then drive one trade through it with the smoke script (Node 22 or later):

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

Recorded runs of five scenarios (the happy path, a dispute, a silent oracle, no quotes, a lapsed request), each with the engine's events and the engine and gateway logs side by side: [docs/demo.md](docs/demo.md).

- **Scenarios.** Pick one of the 14 (eight brief scenarios, six attacks) and press **Run**, **Run ×N** or **Run all**. The page acts as every participant through the gateway and judges PASS or FAIL against the expected events. Panels: lanes, contract, ledger, trace, a timeline scrubber, the runs table with each run's latency, and **View as** (what one account receives). The failure drills below are listed at the bottom of the page. `node tools/dashboard/web/live-smoke.mjs` runs the 14 and `happy_path` ×20 from a terminal.
- **Reload.** The page keeps nothing across a reload, so on load it asks the gateway's [HTTP API](#http-api) for the newest 5,000 `seq`s of history: lanes, contracts, positions, ledger and trace come back, and so do the earlier scenario runs, in the timeline's run list, marked as history with their PASS or FAIL. The page then shows the newest run, not every batch; **All batches** shows everything. The runs table does not come back: it is the page's own record, with its timing. `?http=URL` on the page's address points it at another HTTP base. If the load fails (an older gateway, the port closed), a banner says so and the page stays live ([DESIGN §11.2](tools/dashboard/DESIGN.md)).
- **Engine crash:** `docker compose kill engine` during a **Run ×N**, then `docker compose start engine`. The status goes "engine down" (when nothing is being sent, noticing can take up to about 10 s) and the command in flight reports `outcome unknown … nonce N`. After the start: `Resume`, "engine up", and a retry with the same nonce, whose log line says what happened (`nonce N taken once (Duplicate on retry); original: applied`, or `retry applied`, for example). Until that retry settles it, the gateway holds the participant's other commands, for at most 60 s ([gateway.md §6](docs/specs/gateway.md)). Runs not yet started wait for the engine (up to 60 s) and pass; a run in flight at the kill may fail on timing, or with "engine unavailable" (commands queued behind the in-flight one on the shared operator are refused). `docker compose logs engine` shows `replayed seq=… hash=…`, the hash it had.
- **Stall:** `docker compose pause engine; sleep 2; docker compose unpause engine`. A `Resume` batch follows and open dispute windows are extended, so a run in flight inside a window may fail on timing: correct engine behaviour.
- **Driver crash:** `docker compose kill aeron`. The engine is reported down about 3 s later; about 10 s after the kill the engine and gateway exit 1 and the page shows "Reconnecting". A `kill` is a manual stop, so Docker does not restart the driver, and the engine and gateway cannot rejoin its dead IPC namespace: run `docker compose up -d aeron engine gateway`. The engine replays its journal to the same hash and the page reconnects without a reload. The self-healing variant is `docker compose exec aeron kill -TERM 1`: the driver exits by itself, Docker restarts it in about 1 s, and the engine and gateway follow. Docker restarts a container automatically at most three times over its lifetime (`restart: on-failure:3`), so this drill self-heals about three times before `docker compose up -d` is needed again ([architecture §5](docs/architecture/overview.md) says what restarts by itself).

### HTTP API

The gateway also serves plain HTTP on `http://127.0.0.1:18081`, next to the WebSocket on 18080 and, like it, published on the host's loopback only (`RFQ_HTTP_PORT` moves it). `GET`s read a **replica**: a read-only copy of the engine that the gateway keeps by following the Archive journal, so a read never touches the engine. `POST`s go through the same hub as the WebSocket's commands, with the same nonces and safety rules. Every read says which `seq` it reflects (`as_of_seq`), and `?min_seq=N` makes it wait for a write you just made. There is no authentication: whoever can reach the port can act as, or read, any account, the admin trace included ([read-api.md §9](docs/specs/read-api.md)).

With the stack up (`docker compose up -d aeron engine gateway`), paste this whole block into a shell: one trade, written and read back over HTTP. It has no comments, so it pastes into zsh as it is. `get` and `post` are `curl` with the base URL, and `post` keeps the reply's `seq` in `SEQ` (a request's or quote's id is its `seq`). Amounts are micro-USD, prices basis points, times engine milliseconds: the engine stamps the wall clock, so `T` is now (Docker's clock follows the host's). The steps:

0. `get /v1/status`: the stack is up when it says `"engine":"up"` and the replica `"following"`. The engine needs a few seconds after `docker compose up`: if it is not yet, wait and run the block again.
1. The operator (account 1) funds Alice (10) and MM2 (21) with $1,000 each and lists contract 7. Run the block again and the listing is refused (`InvalidInput`, contract 7 exists), which is harmless.
2. Alice asks to buy 1,000 YES at 0.70 or better (7000 basis points); quotes are due in 3 s.
3. MM2 quotes 0.60 for all 1,000.
4. At the deadline the engine presents the best quote to Alice, and her request shows it as `selected`. The loop waits up to 10 s for that.
5. Alice accepts what she was shown. Both stakes go to escrow. The demo node's accept window is 2 s, so this has to follow step 4 at once: that is why it is one block.
6. Read it back. `min_seq` makes a read wait until the replica has applied that `seq`.

```bash
H=http://127.0.0.1:18081
get() { curl -s "$H$1"; echo; }
post() { R=$(curl -s -X POST "$H$1" -H 'content-type: application/json' -d "$2"); printf '%s\n' "$R"; SEQ=$(printf '%s' "$R" | sed 's/^{"seq":\([0-9]*\).*/\1/'); }

get /v1/status

T=$(date +%s)000
post /v1/commands '{"actor":1,"action":{"Deposit":{"to":10,"amount":1000000000}}}'
post /v1/commands '{"actor":1,"action":{"Deposit":{"to":21,"amount":1000000000}}}'
post /v1/commands '{"actor":1,"action":{"ListContract":{"id":7,"description":"Rain in Paris tomorrow?","oracle":4,"trading_close":'$((T+3600000))',"resolution_deadline":'$((T+7200000))',"dispute_window":60000}}}'

T=$(date +%s)000
post /v1/requests '{"actor":10,"legs":[{"contract":7,"side":"BuyYes","qty":1000,"limit_price":7000}],"response_deadline":'$((T+3000))'}'
REQ=$SEQ

post /v1/quotes '{"actor":21,"request":'$REQ',"leg":0,"price":6000,"size":1000,"expires_at":'$((T+60000))'}'
QUOTE=$SEQ

for i in $(seq 20); do get /v1/accounts/10/requests | grep -q '"state":{"Presented"' && break; [ $i = 20 ] && echo "the request was not presented within 10 s"; sleep 0.5; done
get /v1/accounts/10/requests

post /v1/commands '{"actor":10,"action":{"Accept":{"request":'$REQ',"picks":[{"leg":0,"quote":'$QUOTE',"price":6000}]}}}'

get "/v1/accounts/10/positions?min_seq=$SEQ"
get "/v1/trace?view=10&limit=20"
```

Each `POST` answers `200` with its command's batch as its actor sees it (`seq`, `nonce`, `latency_us`, events, balances), or `422` with `"rejected": "<reason>"` when the engine refuses the command. Step 6's first read ends like this (the ids differ from run to run): Alice holds YES with $600 locked, and MM2 holds the other side.

```json
{"as_of_seq":11,"positions":[{"contract":7,"id":1,"payout":null,"qty":1000,"request":7,"settled":false,"side":"Yes","stake":600000000}]}
```

`view=10` is what Alice's own connection would have received: her commands and what the engine told her, nothing of MM2's quote or balance (`view=admin` is the operator's view, every command). The other endpoints (balances, quotes, open requests, contracts, status), the error statuses, the limits and the trace paging are in [read-api.md](docs/specs/read-api.md). If you moved the port, change `H`. `node scripts/http-smoke.mjs` runs a whole trade over HTTP and checks every reply and read, and that the trace equals the batches the WebSocket delivered; it prints `HTTP SMOKE OK`. Its arguments are the HTTP base and the WebSocket URL, or `RFQ_HTTP_URL` and `RFQ_GATEWAY_URL`.

## Layout

Code is split into hexagonal layers, and dependencies point inward only: crate boundaries enforce it ([architecture §0](docs/architecture/overview.md)).

| Crate | Layer | What it holds |
|---|---|---|
| [`rfq-domain`](crates/rfq-domain) | Domain, pure | Money types, `Ledger`, requests, quotes, contracts, positions, `Command` and `Event`, and the `Engine` state machine with its invariants. All money logic lives here. No I/O, no clock, no randomness. `#![forbid(unsafe_code)]`. |
| [`rfq-app`](crates/rfq-app) | Application | The ports `Journal` and `Transport`, the protocol types `ClientCommand` and `EventBatch`, `batch_of` (which command makes which batch, shared by the node and the replica), the `Sequencer`, and `Node`, the write-ahead loop: sequence, append, wait durable, apply, publish. |
| [`rfq-infra`](crates/rfq-infra) | Infrastructure | Adapters for the ports: the versioned `codec`, `FileJournal`, `MemJournal`, `MemTransport`; with feature `aeron`, `AeronTransport`, `ArchiveJournal` (Aeron Archive) and `JournalFollower` (replays the journal and keeps following it, for the read side). |
| [`rfq-node`](crates/rfq-node) | Composition root (binary) | Reads its configuration from the environment, wires an Aeron transport and a journal into `Node`, and runs the loop. |
| [`rfq-demo`](crates/rfq-demo) | Client (binary) | Runs the scripted scenarios against a live node over Aeron and checks the events it gets back. |
| [`rfq-read`](crates/rfq-read) | Read side, pure | The replica: a read-only `Engine` fed by the journal, the trace of every batch it regenerates, and the read views (positions, requests, quotes, contracts). No I/O, no clock. `#![forbid(unsafe_code)]`. |
| [`rfq-gateway`](crates/rfq-gateway) | Client edge (library and binary) | The WebSocket gateway and the HTTP API. The library is pure (protocol, per-viewer filter, nonce book, hub state machine, the HTTP read and write rules); the binary adds the Aeron adapter, the WebSocket and HTTP servers, the hub loop and the replica thread that follows the journal. |
| [`rfq-testkit`](crates/rfq-testkit) | Test support | The test `Harness`, well-known accounts, and the scripted scenarios (shared by the tests and the demo). |

## Where each deliverable is

| | Deliverable | Where |
|---|---|---|
| D1 | State machine | [specs/engine.md](docs/specs/engine.md) §6, with the multi-leg "leg 2 of 3" walk-through in §8 |
| D2 | Working implementation | `crates/`, `cargo test --workspace`, and `./scripts/demo.sh` (this README) |
| D3 | Failure-mode notes | [specs/failure-modes.md](docs/specs/failure-modes.md) (races and partial failures). It links to a companion, [specs/adversarial.md](docs/specs/adversarial.md), the adversarial analysis (attackers). |
| D4 | Resolution design | [specs/resolution.md](docs/specs/resolution.md) |
| D5 | Design note (quotes living seconds vs days) | [specs/quote-lifetime.md](docs/specs/quote-lifetime.md) |

## Documentation

All design docs live in [`docs/`](docs/README.md). Where to start:

| Doc | What it answers |
|---|---|
| [rfc/0001-rfq-engine.md](docs/rfc/0001-rfq-engine.md) | What is being built, the product decisions (P1–P12) and technical decisions (T1–T8), and why |
| [architecture/overview.md](docs/architecture/overview.md) | Layers and crates, the write path and the read side, Aeron streams, the write-ahead loop and its crash points, Docker topology, production path, measured numbers |
| [demo.md](docs/demo.md) | Recorded runs (GIFs) of five scenarios on the live stack, with the engine's events and logs |

The specs:

| Doc | What it answers |
|---|---|
| [specs/engine.md](docs/specs/engine.md) | Domain model, ledger and invariants I1–I5, commands and their validation, state machines, events, multi-leg (§8) |
| [specs/failure-modes.md](docs/specs/failure-modes.md) | Every race (R1–R9) and partial failure (F1–F16), how each is handled, and its test |
| [specs/adversarial.md](docs/specs/adversarial.md) | What a malicious requester, market maker, oracle or bystander can try, and the defence or accepted residual risk |
| [specs/resolution.md](docs/specs/resolution.md) | How funds unlock: oracle outcomes, disputes, delays, ambiguity, the hard deadline |
| [specs/quote-lifetime.md](docs/specs/quote-lifetime.md) | Quotes that live seconds vs days: what stays invariant, what changes, what is built vs designed |
| [specs/transport-aeron.md](docs/specs/transport-aeron.md) | Aeron and Archive wiring, the journal, replay and the journal follower, Docker Compose |
| [specs/gateway.md](docs/specs/gateway.md) | The WebSocket edge: protocol, what each view may see, nonces, engine status and `outcome unknown`, the trust boundary |
| [specs/read-api.md](docs/specs/read-api.md) | The read replica and the HTTP API: endpoints and statuses, the trace per view, `as_of_seq` and `min_seq`, limits and security |

Reviews and plans: the adversarial review of the spec before coding ([reviews/spec-review.md](docs/reviews/spec-review.md)), and the implementation plans with one file per task ([docs/plans/](docs/plans/), indexed in [docs/README.md](docs/README.md)). The dashboard's own design is in [tools/dashboard/DESIGN.md](tools/dashboard/DESIGN.md).

## Design at a glance

- **Exact money.** Amounts are `u64` micro-USD and prices are `u16` basis points. The buyer locks `qty × bps × 100` and the counterparty locks the complement, so the two always sum to `qty × $1` with no rounding. All arithmetic is checked; overflow rejects the command instead of wrapping.
- **Deterministic, replayable.** One sequencer totally orders every command and stamps its time. The engine never reads a clock or randomness and iterates only `BTreeMap`/`BTreeSet`. State is the replay of the log, and `state_hash` proves it.
- **Multi-leg, all or nothing.** A request has up to 8 legs. One hold covers them all, and at the deadline every leg must have an eligible quote or the whole request expires and releases every hold in that one command. One accept escrows every leg ([engine.md §8](docs/specs/engine.md)).
- **Money only moves through the ledger,** and every handler validates before it mutates. Five invariants (conservation, holds, escrow, ownership, no dead ends) are checked after every command in the tests.
- **Adversarial by default.** Sealed bids, firm quotes once selected, strict per-actor nonces, a bond for disputes, role separation for oracles, a hard deadline so no money is stuck forever. The remaining risks are listed with their production defences in [adversarial.md](docs/specs/adversarial.md).
- **Write-ahead.** Nothing is applied or published until its command is durable, meaning fsynced, in the journal: Aeron Archive with `aeron.archive.file.sync.level=2`, or a local file with `sync_data`. That costs an fsync on the write-ahead path, one wait per batch. Egress is a best-effort notification stream; anything that must happen exactly once reads the journal.
- **Reads come from the journal too.** A replica of the engine, built by replaying and following the Archive, serves the HTTP reads and the trace, so the engine never answers a read. It shows only commands that were fsynced, and its `state_hash` at a `seq` equals the node's ([read-api.md](docs/specs/read-api.md)).

## Measured numbers

`cargo bench -p rfq-domain` replays a 100,005-command log (20,000 full RFQ rounds: request, two quotes, tick, accept) through `Engine::apply` at about **3.7 million commands/s** (about 270 ns each), and `state_hash` over the resulting state takes about **11.9 ms**. This is the engine alone: Apple M4 Max, macOS 27.0.1, rustc 1.99.0, release profile with overflow checks on, one thread, 3 funded accounts and 1 contract, with no codec, no fsync and no Aeron, measured while other builds kept the machine busy. End-to-end throughput over Aeron was not measured. Details and caveats: [architecture §8](docs/architecture/overview.md).

## Not built

Designed only: snapshots and pruning, Aeron Cluster for HA, on-chain escrow and effect workers, and authentication. (The gateway and its HTTP API are built, but they trust the `actor` a client names: [gateway.md §7](docs/specs/gateway.md), [read-api.md §9](docs/specs/read-api.md). The read side is built inside the gateway: it has no snapshots, so it replays the whole journal on every start, and its trace grows with it.) The brief puts auth, real chain integration and pricing out of scope.

## License

Mozilla Public License 2.0 (MPL-2.0), see [LICENSE](LICENSE).

Copyright (c) 2026 hmdsefi (https://github.com/hmdsefi/rfq) <hdyousefi@gmail.com>. See [COPYRIGHT](COPYRIGHT).
