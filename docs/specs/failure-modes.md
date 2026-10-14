# Failure-Mode Notes (D3)

**Global mechanisms.** (G1) One sequencer totally orders every command; races become "lower `seq` wins". (G2) Validate-then-apply: no fallible step after the first mutation, so a command is all-or-nothing. (G3) Due timers fire *before* the command stamped at/after their deadline. (G4) Invariants I1–I5 ([engine.md §3](engine.md)) are asserted after every command in tests, including a property test over random command streams. (G5) The log is recorded before events are published (write-ahead).

## Races

| # | Race | Handling | Test |
|---|---|---|---|
| R1 | MM cancel vs requester accept | G1: whichever is sequenced first wins. Cancel first ⇒ request `Rejected(QuoteWithdrawn)`, accept gets `InvalidState`. Accept first ⇒ cancel gets `InvalidState`. | `race_cancel_before_accept`, `race_accept_before_cancel` |
| R2 | Accept vs `accept_by` | G3: an accept stamped `≥ accept_by` is processed *after* the lapse timer ⇒ rejected even if no `Tick` arrived. | `accept_after_window_rejected_without_tick` |
| R3 | Quote submit vs response deadline | Same: quote stamped `≥ deadline` sees request already `Presented`/`Expired`. | `quote_after_deadline_rejected` |
| R4 | Quote expiry vs selection | Eligibility requires `expires_at ≥ deadline + ACCEPT_WINDOW`; equal-time timers ordered by key; stale timers are no-ops. | `short_expiry_not_selected` |
| R5 | Double accept / accept vs reject | State check `Presented`; second command gets `InvalidState`. | `double_accept_rejected` |
| R6 | Duplicate / replayed command | Per-actor nonce must increase; duplicate ⇒ `Duplicate`, no effect. | `duplicate_nonce_ignored` |
| R7 | Withdraw racing a reservation | Only `free` is withdrawable; holds are taken from `free` atomically. | `withdraw_cannot_touch_reserved` |
| R8 | Quote replace vs selection | Replace is one command (release old + hold new); selection sees exactly one active quote per MM per leg. | `replace_quote_atomic` |

## Partial failures

| # | Failure | Handling |
|---|---|---|
| F1 | Insufficient funds mid-accept (multi-leg design) | All legs validated before any escrow (G2). Holds were sized at submit time, so accept cannot fail for funds in single-leg. |
| F2 | Arithmetic overflow | `checked_*` in validation; overflow ⇒ `CommandRejected`, nothing mutated. `MAX_QTY` bounds amounts. |
| F3 | Engine crash before log record | Command never existed; client retries same nonce. |
| F4 | Crash after record, before apply | Replay applies it on restart. |
| F5 | Crash after apply, before egress | State rebuilt by replay; events re-emitted from the reconnect position; consumers dedupe by `seq`. |
| F6 | Replay produces different state | Prevented by determinism rules (no clock, no RNG, `BTreeMap`, genesis in log); verified by `state_hash` equality test (live vs replay) and in the Docker crash demo. |
| F7 | Oracle never answers | Hard `resolution_deadline` ⇒ `Final(Invalid)`, full refunds. |
| F8 | Arbiter never rules | Same hard deadline; disputer's bond returned. |

## Adversarial behaviour

| Attacker | Attack | Defence |
|---|---|---|
| Requester | Spam requests to tie up MM attention/capital | Must hold `qty × limit` per request (P4). |
| Requester | Accept a different price than shown | `Accept` must echo quote ids **and** prices; mismatch ⇒ rejected. |
| MM | Spam quotes | Each quote holds full collateral; one active quote per MM per leg. |
| MM | Undercut by 1 bp after seeing others | Sealed bids; quote events go only to their owner (`audience`). |
| MM | Win with a quote that expires right after selection | Not eligible unless firm through the whole acceptance window. |
| MM | Win, then cancel in the window (griefing) | **Accepted residual risk:** request fails cleanly, all funds released, cancellation is attributable on the log. Mitigation (designed): cancel-after-selection penalty from the quote hold to the requester. |
| Either | Self-trade | `mm ≠ requester` rejected. Sybil accounts can't be prevented in a permissionless venue — they only trade with themselves at their own cost. |
| Oracle | Propose a wrong outcome | Dispute window + bond + arbiter. |
| Anyone | Dispute spam | Bond; one dispute per contract; bond forfeited if the ruling upholds the proposal. |

## Where capital could leak in the multi-leg stub

Only at holds owned by a provisionally-matched leg. Each such hold belongs to a live request with a pending deadline or `accept_by` timer (I4, I5), and every non-`Filled` exit releases all legs in one command ([engine.md §8](engine.md)). Splitting legs across shards is forbidden because it would need 2PC.
