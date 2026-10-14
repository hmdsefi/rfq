# Design Note: Quotes Living Seconds vs Days (D5)

**Invariant to the decision.** The state machines, ledger and invariants I1–I5 never mention a duration. Every deadline is an absolute `Timestamp`, so a timer firing in 200 ms or in 3 days takes the same path and replays identically.

**Not invariant.**

| Concern | Seconds | Days |
|---|---|---|
| Collateral | Full holds are cheap | MMs need one limit behind many quotes |
| Stale prices | Small risk | Cheap cancel/replace |
| Selection | Sealed at the deadline | Accept the live best quote |
| State size | Few objects | Pruning, snapshots |
| Acceptance window | Seconds | Hours: the free option grows, so a lapse fee |

**What we built so that either answer is cheap.**

1. **Durations are genesis config** (`ACCEPT_WINDOW`, `MIN/MAX_RESPONSE`, `RESUME_GRACE`; `dispute_window` per contract), so they are in the log.
2. **Two policy seams:** `ReservationPolicy` (`FullHold`) and `SelectionPolicy` (`SealedAtDeadline`), enums in the genesis config (`crates/rfq-domain/src/command.rs`). One engine function matches each exhaustively, and each site listed here holds a compile-time tripwire (`let ReservationPolicy::FullHold = ..;`), so the compiler forces a decision there:
   - Reservation: `quote_reservation`; `submit_quote` (funds check, hold creation); `select_sealed`; `accept`; I4 in `check_invariants`.
   - Selection: `on_request_deadline`; `submit_quote`; `accept`; `reject`; `on_accept_by`.

   The lists are in those functions' doc comments and are maintained by hand: a site missing from them has no tripwire.
3. **Designed, not built:** `SharedLimit` (quotes draw on an MM pool, so selection becomes the commit point, with no funds check at accept), `Continuous` selection (accept while `Open`; re-entered states need deadline-stamped timers, as the dispute timer already is), pruning and snapshots. Built: atomic quote replace and a `BTreeSet` timer index.
