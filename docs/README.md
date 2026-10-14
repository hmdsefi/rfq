# RFQ Matching & Settlement Engine — Docs

Brief: [RFQ_Technical_Exercise.pdf](RFQ_Technical_Exercise.pdf)

| Doc | What it answers | Brief deliverable |
|---|---|---|
| [rfc/0001-rfq-engine.md](rfc/0001-rfq-engine.md) | What we're building, the product decisions, and why | — |
| [architecture/overview.md](architecture/overview.md) | Components, Aeron streams, Docker topology, recovery, scaling | — |
| [specs/engine.md](specs/engine.md) | Domain model, ledger, commands, events, state machines | D1 (state machine), D2 (behaviour) |
| [specs/failure-modes.md](specs/failure-modes.md) | Every race and partial failure, and how it's handled | D3 |
| [specs/resolution.md](specs/resolution.md) | How funds unlock; disputes, delays, ambiguity | D4 |
| [specs/quote-lifetime.md](specs/quote-lifetime.md) | Seconds vs days: what's invariant, what isn't | D5 |
| [specs/transport-aeron.md](specs/transport-aeron.md) | Aeron + Archive wiring, journal, replay | — |
| [plans/2026-10-13-rfq-engine-plan.md](plans/2026-10-13-rfq-engine-plan.md) | Implementation roadmap: constraints, layers, execution order, task index | D2 |
| [plans/tasks/](plans/tasks/) | One file per task (Setup, Gate A, Tasks 0–14) | D2 |
