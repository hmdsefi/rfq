# Read Side and HTTP API Execution Plan

> **For agentic workers:** use superpowers:subagent-driven-development. Each task's text is in its file under [`tasks/`](tasks/) (R1–R7).

**Goal.** A read-only replica of the engine, fed by the journal, serving an HTTP API (reads and writes) on the gateway, a trace retrievable per view, and a dashboard that loads its history on page load.

**Spec.** [docs/specs/read-api.md](../specs/read-api.md), approved by the PO on 2026-10-16. The spec is the authority; this plan argues from it.

**Architecture.** A journal follower (`rfq-infra`, Aeron) replays and follows the log from the Archive into a replica (`rfq-read`, no I/O) that regenerates every `EventBatch` with the node's own batch builder. The gateway runs the replica in its own thread and adds its own small HTTP server on port 18081: `GET` reads the replica, `POST` enters the hub as a short-lived connection, so writes keep every WebSocket safety rule.

**Tech stack.** Rust (std threads, `httparse`, `serde_json`, `rusteron-archive` for the follower), Node 25 for the smoke script, the dashboard's plain JS.

## Global Constraints

- **The live engine is untouched in behaviour.** `rfq-node`, the sequencer, the journal, the codec (`VERSION` 1) and streams 1001–1004 behave exactly as today. Every existing test stays green, and `state_hash` values are byte-identical. The only node change is calling the shared batch builder (R1).
- **The replica never shows a command that was not durable:** the follower replays only what the Archive recorded (spec §5).
- **Determinism:** `BTreeMap`/`BTreeSet`, never `HashMap`, in `rfq-read` and in any ordering the API exposes.
- **`rfq-read` has no I/O and no Aeron dependency:** it depends on `rfq-domain`, `rfq-app`, `serde` only (plus dev-dependencies).
- **Stream ids:** the follower replays onto stream **1005** (`FOLLOW`). No other stream id changes.
- **Ports and env:** HTTP `RFQ_HTTP_ADDR`, default `127.0.0.1:18081`; in Docker `0.0.0.0:18081`, published as `127.0.0.1:${RFQ_HTTP_PORT:-18081}:18081`. The WebSocket stays on 18080.
- **Limits (spec §3, §9):** request body 64 KiB (`MAX_TEXT`), `Content-Length` required on `POST` (no chunked), one request per connection (`Connection: close`), 64 HTTP connections at once (more get `503`), request head within 10 s, write wait 75 s, `min_seq` wait 2 s, trace `limit` default 500 and maximum 1,000, dashboard history = the newest 5,000 `seq`s.
- **Origin rule:** exactly the WebSocket's allowlist (`null`, `http://localhost[:port]`, `http://127.0.0.1[:port]`); no `Origin` header passes.
- **No new HTTP framework:** the server is `std::net` + `httparse` (already in the lock through `tungstenite`). Any new dependency must pass `cargo deny`.
- **Aeron code builds only in Docker** (`--features aeron` needs cmake, clang and a JDK): the follower and the gateway's replica wiring are checked by `docker compose build` and the `#[ignore]`d live tests (instructions at the top of `crates/rfq-infra/tests/aeron_live.rs`).
- **Docker images are shared with the main checkout** (the compose project is named `rfq`): building from this worktree replaces the images the PO's stack runs, until `main` is rebuilt.
- **Gates (every task that touches Rust):** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`, `cargo deny check`. Tasks touching Docker or Aeron add `docker compose build` and the live checks they name. The end-to-end set: `scripts/demo.sh` (8 PASS, `RECOVERY OK`), `node scripts/gateway-smoke.mjs` (`GATEWAY SMOKE OK`), `node scripts/http-smoke.mjs` (`HTTP SMOKE OK`), `tools/dashboard/build.sh`, `node tools/dashboard/web/live-smoke.mjs` (14/14).
- **Commits:** scopes `app`, `domain`, `read`, `infra`, `gateway`, `docker`, `dashboard`, `docs`; stage by explicit path; never commit `.claude/`, `.helper/`, `.superpowers/`, `dist/` or `target/`. Work stays on branch `worktree-read-api`; it is merged into `main` and pushed only after the final review.

## Review Focus

The inputs the spec implies but no obvious test exercises, most likely to bite first. Each has its test in the owning task.

1. **The engine restarts while the follower follows.** The old recording stops and a new one starts with `Resume`: the follower moves on and the replica stays contiguous. (R3 live test; R2 restart-equivalence test.)
2. **The journal is reset** (`docker compose down -v` of aeron and engine while the gateway lives): `seq` 1 arrives after a higher `seq`, and the replica starts empty instead of failing or mixing journals. (R4 read-side test.)
3. **Concurrent writes for one account** (two `POST`s, or a `POST` and a WebSocket command): each gets its own `seq`, never the other's batch. (R5 test.)
4. **An account's trace over many batches it cannot see:** paging advances and ends; no endless or empty full pages. (R4 test.)
5. **An HTTP client that disconnects mid-wait:** the hub gets `Close` for the short-lived connection and its queue is not leaked. (R5 test.)

## Tasks

| # | Task | Depends on | Model | Gate |
|---|---|---|---|---|
| R1 | [Shared batch builder, engine read accessors](tasks/R1-batch-builder-and-accessors.md) | spec | cheap | review |
| R2 | [`rfq-read`: replica, queries, trace](tasks/R2-rfq-read-replica.md) | R1 | standard | review |
| R3 | [Journal follower (Aeron)](tasks/R3-journal-follower.md) | spec | standard | review, live test in Docker |
| R4 | [Gateway HTTP server, reads, replica thread, Docker port](tasks/R4-http-reads.md) | R2, R3 | standard | review, `docker compose build` |
| R5 | [HTTP writes through the hub, smoke script](tasks/R5-http-writes.md) | R4 | standard | review, end-to-end set |
| R6 | [Dashboard loads history on page load](tasks/R6-dashboard-history.md) | R5 | standard | review, `build.sh`, live smoke |
| R7 | [Docs and architecture](tasks/R7-docs.md) | R6 | standard | review, every README curl run |

Then a final whole-branch review (most capable model), one fix round if needed, merge to `main`, push.

## Status

| # | Status |
|---|---|
| R1–R7 | approved by the PO on 2026-10-16 ("write the spec then build it, update the docs and architecture at the end") |
