# Design Note: Quotes Living Seconds vs Days (D5)

## Invariant to the decision

- **The state machines and ledger.** States, transitions, holds, escrow, and invariants I1–I5 never mention a duration.
- **Absolute timestamps everywhere.** Every deadline is a `Timestamp`; nothing assumes "short".
- **Time as data + stale-safe timers.** A timer firing in 200 ms or in 3 days goes through the same code path, and replay reproduces it exactly.
- **Sequencing and validate-then-apply.** Atomicity and race handling don't depend on how long anything lives.
- **Settlement and resolution.** Positions exist only after acceptance and aren't affected by quote lifetime.

## Not invariant

| Concern | Seconds | Days |
|---|---|---|
| **Capital cost of full reservation (P4/MM holds)** | Negligible | Large: MMs lock collateral per quote for days. They would need **shared quoting limits** (one pool backing many quotes, checked at accept) |
| **Stale-price risk** | Small | Big: MMs need cheap cancel/replace, and maybe auto-cancel on market events |
| **Selection style** | Sealed auction at the deadline works | Requesters may want to accept the live best quote at any time (streaming) |
| **State size / timers** | Few live objects | Many live objects. The timer index and pruning matter, and snapshots bound replay time |
| **Acceptance window** | Seconds | Could be hours, which makes a cancel-in-window griefing penalty more important |

## What we built so that either answer is cheap

1. **All durations are genesis config** (`ACCEPT_WINDOW`, `MIN/MAX_RESPONSE`, `dispute_window`). They are not constants in code.
2. **`ReservationPolicy` seam.** Today it is `FullHold` (each quote holds its full collateral). A future `SharedLimit` would let quotes draw on an MM's pool and run the funds check at selection or acceptance instead. Only the quote-submit and accept handlers call it.
3. **`SelectionPolicy` seam.** Today it is `SealedAtDeadline`. A future `Continuous` would fire selection on each new best quote and allow `Accept` while `Open`. The state machine already has the `Presented` step to reuse.
4. **Cheap replace.** A quote replace is one atomic command, so frequent repricing over days costs one log entry, not a cancel and a submit.
5. **Timer index and pruning.** `BTreeMap<(Timestamp, TimerKey)>` makes long-lived objects O(log n). Terminal objects are pruned from memory, since Archive keeps the history.
