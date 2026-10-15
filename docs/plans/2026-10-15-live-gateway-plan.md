# Live Gateway and Dashboard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Each task's full text is in [`tasks/`](tasks/) (files `L*-*.md`). This file is only the roadmap.

**Goal.** The dashboard runs the brief's scenarios through the real stack. The page sends each participant's commands over a WebSocket to a new product component, `rfq-gateway`, which puts them on Aeron for `rfq-node` in Docker. The engine's real output streams back live: events, D1 state transitions, balances and engine time. The page records it, steps through it, and measures it like a perf tool.

**Architecture.**
- Three small engine-output additions (touched balances, `time`, `kind`, always-published `Resume`), with no state or journal change.
- A new hexagonal crate, `rfq-gateway`: a pure hub (protocol, per-viewer filter, nonce book), plus an Aeron + WebSocket binary.
- The dashboard's data source switches from the WASM snapshot to the recorded gateway stream, driven by declarative JS scenario scripts.

**Tech stack.** Rust 1.99 workspace (edition 2024), `serde_json`, `tungstenite` (sync WebSocket), `rusteron` via `rfq-infra` (Docker-only), plain HTML/JS, and Node 25 for the scripted checks.

**Specs.** Read both before any task:
- [docs/specs/gateway.md](../specs/gateway.md) (product);
- [tools/dashboard/DESIGN.md §11](../../tools/dashboard/DESIGN.md) (demo page).

## Global Constraints

- **Engine changes are exactly gateway.md §2:**
  - the touched-balances set, which is `#[serde(skip)]` and not state;
  - `EventBatch.{time, kind, balances}`;
  - a `Resume` batch always published.
- **Engine invariants hold:**
  - `state_hash` values are unchanged and the journal record format is unchanged (codec `VERSION` stays 1);
  - every existing test stays green;
  - `scripts/demo.sh` still prints 6 PASS and `RECOVERY OK`.
- **New crate.** `crates/rfq-gateway` joins the workspace with the same crate-level lints as the others: `#![forbid(unsafe_code)]`, and `#![deny(clippy::unwrap_used, clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::cast_sign_loss)]`. The Aeron parts sit behind feature `aeron` (default off) and build only in Docker, like `rfq-node`.
- **New dependencies** go in `[workspace.dependencies]`: `serde_json = "1"`, `tungstenite = "0.24"` (or the newest release at least two weeks old). `cargo deny check` must stay clean.
- **The gateway never panics on client input.** It caps frames at 64 KiB, binds to `127.0.0.1`, allows these Origins only: `null`, `http://localhost:*`, `http://127.0.0.1:*`.
- **Money stays integer micro-USD and prices integer bps** everywhere. The page formats them and diffs them only for display.
- **Gates for every Rust task**, from the repo root, aeron off:
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --all --check`
  - `cargo deny check`

  Tasks that touch Docker also run the Docker build of `rfq-node` and `rfq-gateway`.
- **Commits:**
  - scopes `domain`, `app`, `node`, `gateway`, `demo`, `dashboard`, `docker` and `docs`;
  - stage by explicit path, never `git add -A`;
  - never commit `.claude/`, `.helper/`, `.superpowers/`, `dist/` or `target/`;
  - push after each task's review is clean.
- **Approval gate.** The PO approves both specs and this plan before L1 starts.

## Review Focus

1. **Sealing through the gateway.** For every event kind, an account view gets only `Public` and its own `Account` events, its own balances, and `origin` only for its own commands. Owner: L2, `view_hides_what_engine_md_section_7_hides` (all 12 scenarios × 5 views).
2. **The engine dies with a command in flight.** The sender gets `error: outcome unknown … resend it with nonce N …` (no hang): the command was offered, so it may still be applied. A command never offered gets `engine unavailable`. Until the retry with nonce N (or the late original) settles it, the gateway holds that participant's other commands, for at most 60 s. After the restart, commands work and the `Resume` batch arrives. Owner: L2, `in_flight_command_fails_when_the_engine_goes_down` and the `an_unresolved_nonce_*` tests; L6 drill.
3. **Nonces after `Duplicate`/`NonceGap`, and after a gateway restart against a long-lived node.** The book never advances on an unconsumed nonce, and an unknown actor is probed exactly as `rfq-demo` does. Owner: L2, `nonce_book_*` tests.
4. **Malformed, unknown or oversized frames** each give an `error` frame, and the connection survives. Owner: L2 (protocol decode tests); L3 (server test with a 65 KiB frame).
5. **Split egress batches** (`split_batch` for oversized outputs). `balances` travel once, on the last chunk, and the page merges chunks by `seq`. Owner: L1, `split_batch_keeps_balances_on_the_last_chunk`; L4 recorder test.

---

## Tasks

| # | Task | Depends on | Model | Gate |
|---|---|---|---|---|
| L1 | [Engine output: touched balances, time, kind, Resume always published](tasks/L1-engine-output.md) | approval | standard | task review |
| L2 | [Gateway core: protocol, view filter, nonce book, hub (pure, no I/O)](tasks/L2-gateway-core.md) | L1 | most capable | task review |
| L3 | [Gateway runtime: Aeron adapter, WebSocket server, Docker service, smoke script](tasks/L3-gateway-runtime.md) | L2 | most capable | task review and a Docker smoke |
| L4 | [Dashboard driver: JS scenario scripts, run scope, recorder, perf metrics](tasks/L4-dashboard-driver.md) | L3 | standard | task review and 12/12 live PASS |
| L5 | [Dashboard UI on the live stream: status, Run controls, timeline, perf, drills, view-as](tasks/L5-dashboard-ui.md) | L4 | most capable | task review and a browser check |
| L6 | [End to end: failure drills, docs, final review, push](tasks/L6-end-to-end.md) | L5 | controller | final review |
| L7 | [Optional: offline fallback (the WASM engine behind the same protocol)](tasks/L7-offline-fallback.md) | L5 | standard | task review; cut if short of time |

**Order.** Strictly in sequence: each task consumes the previous task's interfaces. L7 is optional and comes last.

**Estimate.** About 1 day of wall-clock time with reviews (L1 ~1.5 h, L2 ~2 h, L3 ~3 h, L4 ~2 h, L5 ~3 h, L6 ~1.5 h).

## Status

| # | Status |
|---|---|
| L1–L7 | approved by the PO on 2026-10-15 |
| L1–L6 | done |
| L7 | not built (optional; the page needs the running stack) |
