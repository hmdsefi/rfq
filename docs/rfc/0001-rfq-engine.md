# RFC 0001 — RFQ Matching & Settlement Engine

- **Status:** Accepted and implemented; Gate A spec review applied 2026-10-14 ([decisions](../reviews/spec-review.md#decisions))
- **Date:** 2026-10-13
- **Stack:** Rust, Aeron + Aeron Archive (via `rusteron`), Docker Compose

## 1. Problem

Build the core of a **permissionless RFQ venue for binary-outcome contracts** (prediction markets). A requester asks for a price, market makers (MMs) answer with firm quotes, the best quote is presented, and on acceptance both sides' funds lock in escrow until the contract resolves.

Unlike an exchange RFQ, the trade doesn't end the story: it *creates* a fully-collateralised position whose money sits in escrow until an oracle decides the outcome. Real money sits in every intermediate state, and every participant is assumed adversarial.

## 2. Goals

1. **Money-state correctness** — funds can never be lost, duplicated, or stuck without an exit.
2. **Adversarial robustness** — no participant can extract value or grief others cheaply.
3. **Deterministic, replayable core** — every state is derivable from an ordered command log.
4. **Thin interfaces** where the product is undecided (quote lifetime, reservation policy, selection policy).

## 3. Non-goals

UI, auth/KYC, real chain or money movement, pricing/risk logic, fees, Aeron Cluster replication (designed, not built).

## 4. Product decisions

| # | Topic | Decision | Why |
|---|---|---|---|
| P1 | Payout | Each share pays **$1** to the winner. Buyer locks `p`, counterparty locks `1−p`. Fully collateralised. | No leverage ⇒ no liquidations; escrow always exactly covers payout. |
| P2 | Side | Requester **declares the side** they buy (`BuyYes` / `BuyNo`). | Simple; "sell YES" ≡ "buy NO" in a fully-backed binary. Info-leak trade-off noted. |
| P3 | Fills | **One quote fills a whole leg.** No splitting. | One counterparty per leg keeps escrow simple. |
| P4 | Requester funds | **Reserved at submission** at `qty × limit_price`. | Requests without funds can't tie up MM capital. |
| P5 | MM cancel | MMs may cancel/replace **until selection**; a selected quote is **firm** through `accept_by`. Ordering decides races. | Standard firm-quote RFQ: the MM knows `ACCEPT_WINDOW` and prices it in. A cancel inside the window would let one MM void every RFQ for free. |
| P6 | Multi-leg | **Built (decision of 2026-10-16; first design only).** A request has 1 to `MAX_LEGS` (8) legs, filled all-or-nothing in one `Accept`. Zero or more legs is `InvalidInput`. Until then the engine rejected `legs.len() != 1`. | The data model was already `Vec<Leg>` and the atomicity design held (specs/engine.md §8), so building it cost one removed guard, a leg cap and an `Accept` re-check. The cap bounds the work of the one command that escrows every leg. |
| P7 | Visibility | **Sealed** until the response deadline. | Prevents last-second undercutting and quote leakage. |
| P8 | Expiry in window | A selected quote can't be cancelled and is eligible only if firm through `accept_by`, so the presented price stays fillable for the whole window. Defensively, if it were ever gone the **request fails**, everything released. No silent fallback. | Requester only ever accepts a price they saw. |
| P9 | Resolution | Outcomes `Yes`/`No`/`Invalid`. **Hard deadline ⇒ final:** `Invalid` (refund) if the oracle never proposed; an unruled dispute keeps the proposal. A timeout never turns a proposal into `Invalid`. | Money can never be stuck forever, and running out the clock never pays the losing side. |
| P10 | Contracts | Listed by an **operator**; trading is permissionless. | The resolver must know the exact question; stops requesters crafting ambiguous wording. |
| P11 | Best quote | Lowest price for the requester; tie → earliest sequence number. | Deterministic and fair. |
| P12 | Disputes | Anyone may dispute once per contract by posting a **bond**; a mocked **arbiter** rules. | Cheap honest path, costly spam. |

## 5. Technical decisions

| # | Decision | Alternatives considered |
|---|---|---|
| T1 | **Single-writer deterministic engine:** `apply(seq, time, cmd) -> events`. All commands are totally ordered by a sequencer. | Lock-per-account concurrency (hard to prove leak-free); actor-per-request (cross-request ledger needs coordination). |
| T2 | **Validate-then-apply** in every handler: all checks run before the first mutation; mutations are infallible. | Undo logs / compensations (more code paths that can leak). |
| T3 | **Aeron** for ingress/log/egress streams; **Aeron Archive** records the *sequenced command log*. State = replay(log). | Redpanda (ms latency, simpler ops — fine for RFQ, but Aeron matches the sequencer/replay model and production Aeron Cluster path); custom journal file (fallback if the spike fails). |
| T4 | **Docker Compose**: Java `ArchivingMediaDriver` container + Rust containers sharing IPC (`/dev/shm`). | Native install (needs Java/cmake locally). |
| T5 | Time is **data**: sequencer stamps time; the engine never reads a clock. Due timers fire before each command. | Wall-clock reads in the engine (non-deterministic replay). |
| T6 | Integer money: micro-USD (`u64`), prices in basis points (1..=9999). `qty × bps × 100` is exact — **no rounding anywhere**. | Decimals/floats. |
| T7 | `BTreeMap` everywhere state is iterated; no randomness. | `HashMap` (randomised iteration breaks replay). |

## 6. Production path (designed, not built)

- **Aeron Cluster** (Raft) replaces the single sequencer for HA: the engine becomes the clustered service unchanged.
- **Snapshots** in Archive bound replay time.
- **Sharding** by contract group if one engine ever saturates (see architecture §8).
- **On-chain escrow contract** (non-custodial settlement), plus **effect workers** (e.g. Temporal) for chain transactions and oracle polling. Both attach at the existing `Deposit`/`Withdrawn` seam (see architecture §7). Temporal is deliberately kept out of the core.

## 7. Risks

| Risk | Mitigation |
|---|---|
| `rusteron` + Java Archive interop on Apple Silicon/Docker | Task 0 spike. Fallback: file journal behind the same `Journal` trait. **Outcome:** the spike passed ([transport-aeron.md](../specs/transport-aeron.md#verified-api-task-0)); `FileJournal` is kept as the fallback behind the same trait. |
| Aeron version mismatch (C client vs Java jar) | Pin jar version to the Aeron version bundled by `rusteron`. **Outcome:** `rusteron-archive =0.2.11` bundles Aeron C 1.53.3, and `docker/aeron/Dockerfile` pins `aeron-all` 1.53.3. |
| Time budget (~6 h) | Pure engine + tests first; Aeron adapter second; docs last. |
