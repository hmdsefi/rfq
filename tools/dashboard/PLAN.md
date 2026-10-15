# RFQ Dashboard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking. Each task's full text is in [`tasks/`](tasks/). This file is only the roadmap.

**Goal:** A single-file, offline demo page that runs the real `rfq-domain` engine in WebAssembly. The presenter can drive market makers and requesters, watch the match happen, and trace everything as admin in the live session.

**Architecture:**
- A small Rust crate in `tools/dashboard/` with its own `[workspace]`, so the root workspace is untouched. It wraps the engine in a never-panicking `Session`, runs the testkit scenarios and the attack scripts as steppable presets, and exposes one JSON-in/JSON-out entry point through a 3-function WASM ABI.
- `build.sh` compiles the crate to `wasm32-unknown-unknown` and inlines it (base64) into one HTML page.

**Tech stack:**
- Rust 1.99 (edition 2024) with `serde_json`;
- `wasm32-unknown-unknown` with no `wasm-bindgen`;
- plain HTML, CSS and JS with no framework and no CDN at runtime;
- Node (already installed, v25) for the headless WASM smoke test and the inliner.

**Spec:** [DESIGN.md](DESIGN.md). Read it before any task. It is the authority this plan argues from.

## Global Constraints

- **Nothing outside `tools/dashboard/` changes.** `git diff --stat -- crates Cargo.toml Cargo.lock deny.toml rust-toolchain.toml` must stay empty.
- **The crate is its own workspace.** `tools/dashboard/Cargo.toml` has an empty `[workspace]` table, `edition = "2024"` and `rust-version = "1.99"`. The toolchain comes from the root `rust-toolchain.toml` (1.99.0).
- **Lints.**
  - The crate root has `#![deny(unsafe_code)]`, plus `#![deny(clippy::unwrap_used, clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::cast_sign_loss)]` (the engine's set).
  - `#[allow(unsafe_code)]` appears only on `mod ffi;`, and only for the `#[unsafe(no_mangle)]` export attributes.
  - There are no `unsafe { }` blocks anywhere.
- **Nothing panics on an input path.** `api::handle` always returns JSON. Errors are `{"error": "<message>"}`.
- **Money is integers.** Micro-USD (`Amount`) and bps (`Price`, 1..=9999) cross the boundary as integers. The page formats and diffs them for display only.
- **Time only moves forward.** The page never computes a timer; the engine fires timers.
- **No network at runtime.** No CDN, no web fonts: use the system font stack. One self-contained `dist/rfq-dashboard.html`.
- **Required copy.** The page must show "Demo aid — not part of the engine" and, on the multi-leg tab, "Built" (it read "Designed, not built (P6)" until multi-leg was built on 2026-10-16: see DESIGN.md §11).
- **Accounts** (from `rfq_testkit::accounts`):
  - OPERATOR 1, ARBITER 2, TREASURY 3, ORACLE 4;
  - ALICE 10 and CAROL 30 (requesters);
  - MM1 20 and MM2 21 (market makers).
- **Gates for every Rust task** (run in `tools/dashboard`):
  - `cargo test`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo fmt --check`
- **Commits.** Use scope `dashboard` (for example `feat(dashboard): …`), stage by explicit path (never `git add -A`), and never commit `dist/` or `target/`.
- **Approval gate.** Task D1 (visual design) needs explicit PO approval before Tasks D6 and D7 start. Tasks D2–D5 don't depend on the visual design and may run once the PO approves the plan.

## Review Focus

The five inputs most likely to bite during the live session. Each has a pinned test in the task that owns it.

1. **An action the current state doesn't allow** (Accept after the window lapsed, Cancel on a selected quote) must show as a `CommandRejected` entry in the trace, not as an error or a JS exception. Owner: D4, `accept_on_expired_request_is_a_rejection`.
2. **Bad form input** (price 0 or ≥ 10000 bps, a non-numeric qty) must return `{"error"}` naming the field, and the page shows it inline. Owner: D4, `price_out_of_range_is_an_error`; D6, form validation.
3. **A manual action in the middle of a preset** (advance the clock, then press Step) must never move time backwards. Any manual op while a preset is unfinished means take-over. Owner: D4, `manual_op_mid_preset_takes_over`.
4. **Huge numbers** (`advance` by `u64::MAX`, nonce `u64::MAX`) must return an error, not trap. Owner: D2, `advance_overflow_is_an_error`; D4, `raw_with_max_nonce_does_not_trap`.
5. **Viewing as an account with nothing** (an unknown id, or Carol before she acts) gives empty lists, not an error. Owner: D4, `view_as_unknown_account_is_empty`.

---

## Tasks

| # | Task | Depends on | Owner | Gate |
|---|---|---|---|---|
| D1 | [Visual design and mockup](tasks/D1-visual-design.md) | spec | controller + PO | **PO approves the mockup** |
| D2 | [Crate scaffold and `Session`](tasks/D2-session-core.md) | plan approval | implementer | task review |
| D3 | [Presets: brief scenarios, attacks, sandbox, runner](tasks/D3-presets.md) | D2 | implementer | task review |
| D4 | [View filter, JSON protocol, FFI](tasks/D4-protocol-and-ffi.md) | D3 | implementer | task review |
| D5 | [WASM build, Node smoke test, inliner](tasks/D5-wasm-build.md) | D4 | implementer | task review |
| D6 | [Page: top bar, lanes, contract card](tasks/D6-page-lanes.md) | D1 approved, D5 | implementer | task review and a browser check |
| D7 | [Page: admin trace, ledger, replay, D1–D5 panels](tasks/D7-page-trace-and-deliverables.md) | D6 | implementer | task review and a browser check |
| D8 | [Verify, review, publish](tasks/D8-verify-and-publish.md) | D7 | controller | final review, PO sees the link |

**Order.** D1 runs first and in parallel with plan review. D2 → D3 → D4 → D5 run in sequence: each consumes the previous task's types. D6 and D7 wait for the D1 approval.

**Execution.** Subagent-driven, as in the engine plan:
- a fresh implementer per task, then a task review (spec and quality);
- a final whole-branch review in D8;
- the ledger lives in `.superpowers/sdd/<this plan>/progress.md` (gitignored).

## Status

| # | Status |
|---|---|
| D1 | done, approved by the PO (92fc65d) |
| D2 | done, review clean (a572952) |
| D3 | done, 1 fix round (4d632fa..30adc37) |
| D4 | done, 1 fix round (f147794..9fe987c) |
| D5 | done, 1 fix round (b1e2c01..da48055) |
| D6 | done, review clean (de817d9) |
| D7 | done, 1 fix round (a3c5d32..5b404ba) |
| D8 | done: gates green, acceptance 7/7, final review fixes (7fc508c), published as a private artifact |
