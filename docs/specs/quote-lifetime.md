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

Built means it is in the code. Designed means the code is shaped for it but the extension is not written.

1. **Built: all durations are genesis config** (`ACCEPT_WINDOW`, `MIN/MAX_RESPONSE`, `RESUME_GRACE`; `dispute_window` per contract). They are not constants in code.
2. **Built: quote collateral is decided in one place, not spread over the engine.** Today every quote takes its full collateral as a hold in `submit_quote` and `accept` escrows it (`FullHold`; there is no policy type yet). **Designed:** a `SharedLimit` would let quotes draw on an MM's pool. It would change only `submit_quote` (no hold at submission) and selection (`on_request_deadline`). **Selection is then the commit point:** the selected quote converts to a hard hold at selection, and selection skips to the next-best quote if the pool can't cover it. Funds are never checked at acceptance, or one pool could win many selections and fail all but one accept.
3. **Built: selection is one function** (`on_request_deadline`, sealed, at the response deadline). **Designed:** a `Continuous` policy would run the same selection step on each new best quote and allow `Accept` while `Open`. The state machine already has the `Presented` step to reuse.
   - **Re-entered states need stamped timers.** "Left the state it was armed for" makes a timer stale only while no state is re-entered with a new deadline. `Continuous` re-enters `Presented` (and dispute rounds would re-enter `Proposed`), so each such timer must carry the deadline or epoch it expects and fire only on a match. **Built:** the dispute timer already does, because `Resume` can extend its window.
4. **Built: cheap replace.** A quote replace is one atomic command, so frequent repricing over days costs one log entry, not a cancel and a submit.
5. **Built: a timer index.** `BTreeSet<(Timestamp, TimerKey)>` keeps insertion and firing logarithmic in the number of pending timers, however far out they are. **Designed, not built:** pruning terminal objects from memory (Archive keeps the history) and snapshots to bound replay time ([architecture §8](../architecture/overview.md)). Every request, quote and position stays in memory today.
