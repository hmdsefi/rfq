# Multi-leg Execution Plan

> **For agentic workers:** use superpowers:subagent-driven-development. The task text is in [`tasks/M1-multi-leg-engine.md`](tasks/M1-multi-leg-engine.md) and [`tasks/M2-multi-leg-dashboard.md`](tasks/M2-multi-leg-dashboard.md).

**Decision (PO, 2026-10-16).** Build multi-leg requests (RFC P6 was "design only") and show both multi-leg stories on the live dashboard:
- a 3-leg request where every leg fills in one accept;
- "leg 2 of 3 fails": legs 1 and 3 are quoted, leg 2 is not, and at the deadline the whole request expires with every hold released in one command.

**Spec.** [docs/specs/engine.md §8](../specs/engine.md). Its five atomicity rules are the requirements. Rules 1–4 already hold in the code. This plan:
- removes the single-leg guard;
- adds a leg cap;
- adds rule 5's accept re-check;
- tests rules 1–5;
- adds the two scenarios end to end.

## Global Constraints

- **Engine behaviour for one-leg requests is unchanged.** Every existing test stays green, and `state_hash` values for the existing scenarios are byte-identical.
- **Multi-leg rules (engine.md §8):**
  - one hold per request covering every leg;
  - selection is all-or-nothing, so a missing eligible quote on any leg expires the whole request and releases every hold in that command;
  - accept is one command that validates every leg first;
  - a presented request stays whole;
  - every leg's contract must outlive the request.
- **Leg cap:** `MAX_LEGS = 8`. More legs is `InvalidInput`; zero legs is `InvalidInput`.
- **Scenario contract ids:** the run's contract stays `ContractId(t0)`. A multi-leg scenario lists extra markets as `ContractId(t0 * 10 + k)`, k = 1..=9 (`Ctx::contract_n(k)`). These stay below 2^53 for the JS page, and never collide across runs, because distinct `t0` give distinct `t0 * 10 + k`. Run scope (rfq-demo `scope.rs`, the page's `Scope`) owns those ids too.
- **Gates:** the same as the live-gateway plan (workspace test, clippy, fmt, deny; Docker builds; `scripts/demo.sh`; gateway smoke; dashboard `build.sh`; live smoke).
- **Commits:**
  - scopes `domain`, `testkit`, `demo`, `dashboard` and `docs`;
  - stage by explicit path;
  - never commit `.claude/`, `.helper/`, `.superpowers/`, `dist/` or `target/`;
  - push after each clean review.

## Tasks

| # | Task | Depends on | Model | Gate |
|---|---|---|---|---|
| M1 | [Engine, testkit scenarios, rfq-demo scope, docs](tasks/M1-multi-leg-engine.md) | PO decision | standard | review (most capable), then demo.sh 8 PASS |
| M2 | [Dashboard: the two scripts, scope, multi-leg rendering, D1 tab](tasks/M2-multi-leg-dashboard.md) | M1 | standard | review, then live 14/14 |

## Status

| # | Status |
|---|---|
| M1–M2 | approved by the PO on 2026-10-16 ("build it, add both scenarios to the dashboard") |
