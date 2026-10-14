# RFQ Matching & Settlement Engine — Docs

The brief (the exercise PDF) is not in the repository. How to build, test and run: the [root README](../README.md).

## The five deliverables

| | Brief deliverable | Where |
|---|---|---|
| **D1** | State machine | [specs/engine.md](specs/engine.md) §6 (request, quote and contract state machines, who triggers each transition) and §8 (multi-leg: "leg 2 of 3 fails") |
| **D2** | Working implementation | The code in [`crates/`](../crates), run by `cargo test --workspace` (in-memory scenarios, property tests, node restart tests) and by [`scripts/demo.sh`](../scripts/demo.sh) (the happy path and five failure paths over Aeron, then a crash and replay check). See the [root README](../README.md). |
| **D3** | Failure-mode notes | [specs/failure-modes.md](specs/failure-modes.md): every race (R1–R9) and partial failure (F1–F15), each with its test where one exists. It links to a companion, the adversarial analysis (attackers and what stops them): [specs/adversarial.md](specs/adversarial.md) |
| **D4** | Resolution design | [specs/resolution.md](specs/resolution.md) |
| **D5** | Design note | [specs/quote-lifetime.md](specs/quote-lifetime.md) |

## All docs

| Doc | What it answers |
|---|---|
| [rfc/0001-rfq-engine.md](rfc/0001-rfq-engine.md) | What we're building, the product decisions (P1–P12), the technical decisions (T1–T7), and why |
| [architecture/overview.md](architecture/overview.md) | Layers and crates, components, Aeron streams, write-ahead loop and crash points, Docker topology, production path, measured numbers |
| [specs/engine.md](specs/engine.md) | Domain model, ledger and invariants I1–I5, commands and validation, state machines, events |
| [specs/failure-modes.md](specs/failure-modes.md) | Every race and partial failure, and how it's handled |
| [specs/adversarial.md](specs/adversarial.md) | Companion to D3: what a malicious requester, MM, oracle or bystander can try, and the defence or accepted residual risk |
| [specs/resolution.md](specs/resolution.md) | How funds unlock; disputes, delays, ambiguity |
| [specs/quote-lifetime.md](specs/quote-lifetime.md) | Seconds vs days: what's invariant, what isn't, what is built vs designed |
| [specs/transport-aeron.md](specs/transport-aeron.md) | Aeron + Archive wiring, journal, replay, Docker Compose, the verified `rusteron` API |
| [reviews/spec-review.md](reviews/spec-review.md) | Adversarial review of the spec before coding: 25 findings and how each was ruled |
| [plans/2026-10-13-rfq-engine-plan.md](plans/2026-10-13-rfq-engine-plan.md) | Implementation roadmap: constraints, layers, execution order, task index |
| [plans/tasks/](plans/tasks/) | One file per task (Setup, Gate A, Tasks 0–14) |
