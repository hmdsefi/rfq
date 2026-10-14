# Design Note: Quotes Living Seconds vs Days (D5)

## Invariant to the decision

- **State machines, ledger, invariants I1–I5.** None mentions a duration; every deadline is an absolute `Timestamp`.
- **Time as data, stale-safe timers.** A timer firing in 200 ms or in 3 days takes the same path and replays identically.
- **Sequencing, validate-then-apply, settlement.** Atomicity, races and payouts don't depend on how long a quote lived.

## Not invariant

| Concern | Seconds | Days |
|---|---|---|
| Capital cost of full holds (P4) | Negligible | Large: MMs need one limit backing many quotes |
| Stale-price risk | Small | Big: cheap cancel/replace, auto-cancel on market events |
| Selection style | Sealed auction at the deadline | Requesters may want to accept the live best quote |
| State size, timers | Few live objects | Many: pruning and snapshots matter |
| Acceptance window | Seconds | Hours: the requester's free option and shadow-RFQ discovery grow, so a reject/lapse fee to the selected MM |

## What we built so that either answer is cheap

1. **Built: durations are genesis config** (`ACCEPT_WINDOW`, `MIN/MAX_RESPONSE`, `RESUME_GRACE`; `dispute_window` per contract), so they are in the log.
2. **Built: two policy seams, `ReservationPolicy` (`FullHold`) and `SelectionPolicy` (`SealedAtDeadline`).** They are enums in the genesis config (`crates/rfq-domain/src/command.rs`). One engine function matches each exhaustively (`quote_reservation`, `on_request_deadline`), and every other site a new variant must change holds a compile-time tripwire (`let ReservationPolicy::FullHold = ..;`), so the compiler forces a decision at each listed site:
   - **Reservation (6):** `quote_reservation`; `submit_quote` funds check and hold creation; the `select_sealed` commit point; `accept`'s `mm_lock`; I4 in `check_invariants`.
   - **Selection (5):** `on_request_deadline`; `submit_quote` (a new best quote); `accept` and `reject` (the `Presented` gate); `on_accept_by` (stamped timer).
   - The lists are in those functions' doc comments and are maintained by hand: a site missing from them has no tripwire.
3. **Designed, not built, behind those seams.** `SharedLimit`: quotes draw on an MM pool, so **selection is the commit point** (hard hold at selection, next-best quote if the pool can't cover it, never a funds check at accept, or one pool could win many selections and fail all but one accept). `Continuous`: run the same selection step on each new best quote and allow `Accept` while `Open`. It re-enters `Presented`, so its timers must carry the deadline they expect; the dispute timer already does, because `Resume` can extend its window.
4. **Built: cheap replace** (one atomic command per reprice) and **a timer index** (`BTreeSet<(Timestamp, TimerKey)>`). Pruning terminal objects and snapshots are designed, not built ([architecture §8](../architecture/overview.md)).
