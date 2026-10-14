# Failure-Mode Notes (D3)

**Global mechanisms.** (G1) One sequencer totally orders every command; races become "lower `seq` wins". (G2) Validate-then-apply: no fallible step after the first mutation, so a command is all-or-nothing. (G3) Due timers fire *before* the command stamped at/after their deadline, in `TimerKey` order ([engine.md §6](engine.md)); `Resume` is the one command that acts before them (F9). (G4) Invariants I1–I5 ([engine.md §3](engine.md)) are asserted after every command in tests, including a property test over random command streams. (G5) The log is recorded before events are published (write-ahead).

## Races

| # | Race | Handling | Test |
|---|---|---|---|
| R1 | MM cancel vs response deadline | G1+G3: a cancel stamped `< T` removes the quote before selection. One stamped `≥ T` runs after selection and finds the quote `Selected` (firm, P5) or `Lost` ⇒ `InvalidState`. A selected quote can't be withdrawn, so cancel never races accept. | `cancel_before_deadline_removes_quote`, `cancel_after_selection_rejected` |
| R2 | Accept vs `accept_by` | G3: an accept stamped `≥ accept_by` is processed *after* the lapse timer ⇒ rejected even if no `Tick` arrived. | `accept_after_window_rejected_without_tick` |
| R3 | Quote submit vs response deadline | Same: quote stamped `≥ deadline` sees request already `Presented`/`Expired`. | `quote_after_deadline_rejected` |
| R4 | Quote expiry vs selection | Eligibility requires `expires_at ≥ deadline + ACCEPT_WINDOW`, and submit clamps `expires_at` to that bound, so a selected quote expires exactly at `accept_by`: its timer is a no-op and `AcceptBy` lapses the request. Stale timers are no-ops. | `short_expiry_not_selected` |
| R5 | Double accept / accept vs reject | State check `Presented`; second command gets `InvalidState`. | `double_accept_rejected` |
| R6 | Duplicate, replayed or skipped command | Strict nonces: only `last + 1` is accepted. `≤ last` ⇒ `Duplicate` (already applied); `> last + 1` ⇒ `NonceGap`. Neither consumes anything, so a lost command is never mistaken for an applied one. | `duplicate_nonce_ignored`, `nonce_gap_rejected_and_not_consumed` |
| R7 | Withdraw racing a reservation | Only `free` is withdrawable; holds are taken from `free` atomically. | `withdraw_cannot_touch_reserved` |
| R8 | Quote replace vs selection | Replace is one command (release old + hold new); selection sees exactly one active quote per MM per leg. | `replace_quote_atomic` |

## Partial failures

| # | Failure | Handling | Test |
|---|---|---|---|
| F1 | Insufficient funds mid-accept (multi-leg design) | All legs validated before any escrow (G2). Holds were sized at submit time, so accept cannot fail for funds in single-leg. | — |
| F2 | Arithmetic overflow | `checked_*` in validation; overflow ⇒ `CommandRejected`, nothing mutated. `MAX_QTY` bounds amounts. | `deposit_overflow_rejected` |
| F3 | Engine crash before log record | Command never existed; client retries same nonce. | — |
| F4 | Crash after record, before apply | Replay applies it on restart. | `restart_replays_to_same_state_without_republishing` |
| F5 | Crash after apply, before egress | State rebuilt by replay; events re-emitted from the reconnect position; consumers dedupe by `seq`. | — |
| F6 | Replay produces different state | Prevented by determinism rules (no clock, no RNG, `BTreeMap`, genesis in log); verified live vs replay and in the Docker crash demo. | `replay_reproduces_state_hash` |
| F7 | Oracle never answers | Hard `resolution_deadline` ⇒ `Final(Invalid)`, full refunds (liveness fallback; see Adversarial). | `oracle_silent_hard_deadline_refunds` |
| F8 | Arbiter never rules | Hard deadline ⇒ `Final(proposed)`, bond returned: the arbiter failed, not the disputer. A timeout never yields `Invalid` once proposed: `ProposeOutcome` needs `now + dispute_window < resolution_deadline`. | `arbiter_silent_hard_deadline_returns_bond`, `propose_too_late_rejected` |
| F9 | Node down across an open dispute window | On restart (non-empty journal) the node sequences `Resume` before any ingress or `Tick`. Before due timers fire, every window ending before `now + RESUME_GRACE` moves there (capped at `resolution_deadline − 1`) and is re-armed; the old timer is deadline-matched, so a no-op. Lapse, quote expiry, request deadline and hard deadline are refund-safe and fire normally. | `resume_extends_open_dispute_window`, `resume_does_not_extend_accept_window`, `restart_appends_resume` |
| F10 | Sequencer restarts with a clock behind the log | It resumes `last_time` from the journal and stamps `max(wall, last_time)`; the engine also takes `max(now, time)`. Time never regresses. | `sequencer_time_is_monotonic` |

## Adversarial behaviour

`#n` = finding in [spec-review.md](../reviews/spec-review.md). **Residual** = accepted risk, with its production defence.

| Attacker | Attack | Defence |
|---|---|---|
| Requester | Spam requests to tie up MM attention/capital (#12) | Must hold `qty × limit` per request (P4), and every leg is at least `MIN_REQUEST_NOTIONAL` ($1). |
| Requester | Accept a different price than shown | `Accept` must echo quote ids **and** prices; mismatch ⇒ rejected. |
| Requester | Free option: accept only if the market moved its way, else reject or lapse (#10) | **Residual**, inherent to RFQ: bounded by a short `ACCEPT_WINDOW`, and firm quotes stop the MM racing it. Grows with days-long windows. Production: reject/lapse fee to the selected MM; per-requester reject-rate limits. |
| Requester + MM | Shadow RFQ with the same public fields to read the best honest price, then reject (#11) | **Residual**, as above: sealed bids are only as strong as the cost of a shadow RFQ. |
| MM | Spam quotes | Each quote holds full collateral; one active quote per MM per leg. |
| MM | Probe the sealed limit with quote submissions (#2) | `SubmitQuote` doesn't check the limit; selection drops over-limit quotes and `QuoteLost` doesn't say why. |
| MM | Grow the timer index with far-future expiries (#14) | `expires_at` clamped to `response_deadline + ACCEPT_WINDOW`. |
| MM | Undercut by 1 bp after seeing others (#25) | Sealed bids; quote events go only to their owner (`audience`). Egress, Archive and gateways are venue-operated. Production: encrypted or commit-reveal quotes. |
| MM | Win with a quote that expires right after selection | Not eligible unless firm through the whole acceptance window. |
| MM | Win, then cancel in the window to void the RFQ (#1) | Selected quotes are firm (P5): cancelling a `Selected` quote ⇒ `InvalidState`. |
| Either | Self-trade | `mm ≠ requester` rejected. Sybil accounts can't be prevented in a permissionless venue — they only trade with themselves at their own cost. |
| Oracle | Propose a wrong outcome (#8) | Dispute window + bond + arbiter. **Residual:** a wrong proposal costs the oracle nothing. Production: oracle bond per proposal, slashed to the disputer when overturned. |
| Oracle | Stay silent so the contract resolves `Invalid` (#7) | **Residual:** `Invalid` at the hard deadline is the liveness fallback for a trusted (mocked) oracle. Production: oracle bond + auto-escalation to the arbiter at a `proposal_deadline`. |
| Oracle | Propose late so the window outlives the hard deadline (#20) | `ProposeOutcome` needs `now + dispute_window < resolution_deadline`. |
| Oracle / arbiter | Judge its own proposal; trade its own contract; years-long or zero-window listings (#22) | `ListContract` rejects an oracle that is operator, arbiter or treasury, and a zero `dispute_window`. **Residual:** insider trading, `MAX_HORIZON`, `MIN_DISPUTE_WINDOW` are production listing policy. |
| Anyone | Dispute spam (#9) | Bond; one dispute per contract; bond forfeited if the ruling upholds the proposal. **Residual:** a flat $100 bond can freeze any amount of escrow until the arbiter rules. Production: bond scaled to open interest, or an escalation ladder. |
| Anyone | Forge `actor`, or inject `Tick`/`Genesis`/`Resume` (#6) | The gateway authenticates `actor` (out of scope per the brief); `ClientCommand` can't express system commands. Strict nonces: no `u64::MAX` lockout. |
| Anyone | Flood ingress with invalid commands, each one logged (#13) | **Residual:** stopped at the gateway (per-actor rate limits, pre-validation) before sequencing; gateway out of scope. |
| Colluders | 10⁶ tiny positions so one finalisation stalls the single writer (#23) | Each position costs at least `MIN_REQUEST_NOTIONAL` of escrow. **Residual:** production settles in bounded batches or per position (`Claim`). |
| Anyone | Exploit wording found ambiguous mid-life (#24) | **Residual:** resolved `Invalid` at resolution. Production: operator `HaltContract`. |

## Where capital could leak in the multi-leg stub

Only at holds owned by a provisionally-matched leg. Each such hold belongs to a live request with a pending deadline or `accept_by` timer (I4, I5), and every non-`Filled` exit releases all legs in one command ([engine.md §8](engine.md)). Splitting legs across shards is forbidden because it would need 2PC.
