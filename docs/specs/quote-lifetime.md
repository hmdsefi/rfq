# Design Note: Quotes Living Seconds vs Days (D5)

## Invariant to the decision

- **The state machines and ledger.** States, transitions, holds, escrow, and invariants I1–I5 never mention a duration.
- **Absolute timestamps everywhere.** Every deadline is a `Timestamp`; nothing assumes "short".
- **Time as data + stale-safe timers.** A timer firing in 200 ms or in 3 days goes through the same code path, and replay reproduces it exactly. Quote timers are bounded by the request: `expires_at` is clamped to `response_deadline + ACCEPT_WINDOW`.
- **Sequencing and validate-then-apply.** Atomicity and race handling don't depend on how long anything lives.
- **Settlement and resolution.** Positions exist only after acceptance and aren't affected by quote lifetime.

## Not invariant

| Concern | Seconds | Days |
|---|---|---|
| **Capital cost of full reservation (P4/MM holds)** | Negligible | Large: MMs lock collateral per quote for days. They would need **shared quoting limits** (one pool backing many quotes, committed at selection) |
| **Stale-price risk** | Small | Big: MMs need cheap cancel/replace, and maybe auto-cancel on market events |
| **Selection style** | Sealed auction at the deadline works | Requesters may want to accept the live best quote at any time (streaming) |
| **State size / timers** | Few live objects | Many live objects. The timer index and pruning matter, and snapshots bound replay time |
| **Acceptance window** | Seconds | Could be hours. Selected quotes are firm (P5), so the requester's free option (accept only if the market moved its way, else reject or lapse) and shadow-RFQ price discovery grow with the window: they'd need a reject/lapse fee paid to the selected MM |

## What we built so that either answer is cheap

1. **All durations are genesis config** (`ACCEPT_WINDOW`, `MIN/MAX_RESPONSE`, `RESUME_GRACE`; `dispute_window` per contract). They are not constants in code.
2. **`ReservationPolicy` seam.** Today it is `FullHold` (each quote holds its full collateral). A future `SharedLimit` would let quotes draw on an MM's pool. **Selection is then the commit point:** the selected quote converts to a hard hold at selection, and selection skips to the next-best quote if the pool can't cover it. Funds are never checked at acceptance, or one pool could win many selections and fail all but one accept. Only the quote-submit and selection handlers call it.
3. **`SelectionPolicy` seam.** Today it is `SealedAtDeadline`. A future `Continuous` would fire selection on each new best quote and allow `Accept` while `Open`. The state machine already has the `Presented` step to reuse.
   - **Re-entered states need stamped timers.** "Left the state it was armed for" makes a timer stale only while no state is re-entered with a new deadline. `Continuous` re-enters `Presented` (and dispute rounds would re-enter `Proposed`), so each such timer must carry the deadline or epoch it expects and fire only on a match. The dispute timer already does, because `Resume` can extend its window.
4. **Cheap replace.** A quote replace is one atomic command, so frequent repricing over days costs one log entry, not a cancel and a submit.
5. **Timer index and pruning.** `BTreeMap<(Timestamp, TimerKey)>` makes long-lived objects O(log n). Terminal objects are pruned from memory, since Archive keeps the history.
