# Failure-Mode Notes (D3)

Every race and partial failure the code handles, with its test. Detail: [engine.md](engine.md), [transport-aeron.md](transport-aeron.md). Attackers: [adversarial.md](adversarial.md).

**Global mechanisms.** (G1) One sequencer totally orders commands, so a race is "lower `seq` wins". (G2) Validate-then-apply: every check runs before the first mutation, so a command is all-or-nothing (the nonce is consumed first, by design). (G3) Due timers fire before the command stamped at or after their deadline, in `TimerKey` order; only `Resume` acts before them (F9). (G4) Invariants I1–I5 are checked after every command in the tests, including property tests over random command streams. (G5) Write-ahead: a batch is durable before anything is applied or published.

## Races

| # | Race | Handling | Test |
|---|---|---|---|
| R1 | MM cancel vs response deadline `T` | A cancel stamped `< T` removes the quote. One stamped `≥ T` runs after selection and gets `InvalidState`: a `Selected` quote is firm (P5). | `cancel_before_deadline_removes_quote`, `cancel_after_selection_rejected` |
| R2 | Accept vs `accept_by` | An accept stamped `≥ accept_by` runs after the lapse timer, so it is rejected even if no `Tick` arrived. | `accept_after_window_rejected_without_tick` |
| R3 | Quote vs response deadline | A quote stamped `≥ deadline` finds the request already `Presented` or `Expired`. | `quote_after_deadline_rejected` |
| R4 | Quote expiry vs selection | Only quotes firm through `deadline + ACCEPT_WINDOW` are eligible and `expires_at` is clamped to that bound, so a selected quote's expiry is a no-op and `AcceptBy` lapses the request. | `short_expiry_not_selected`, `selected_quote_expiring_at_accept_by_is_left_to_the_lapse` |
| R5 | Double accept, accept vs reject | The second command finds the request no longer `Presented`: `InvalidState`. | `double_accept_rejected` |
| R6 | Duplicate or skipped command | Strict nonces: `≤ last` ⇒ `Duplicate`, `> last + 1` ⇒ `NonceGap`. Neither consumes anything. | `duplicate_nonce_ignored`, `nonce_gap_rejected_and_not_consumed` |
| R7 | Withdraw vs reservation | Only `free` can be withdrawn; holds are taken from `free` atomically. | `withdraw_cannot_touch_reserved` |
| R8 | Quote replace vs selection | Replace is one command (release old, hold new), so selection sees one active quote per MM per leg. | `replace_quote_atomic` |

## Partial failures

| # | Failure | Handling | Test |
|---|---|---|---|
| F1 | Funds short mid-accept (multi-leg) | All legs are validated before any escrow (G2). In single-leg, the hold made at submission already covers accept. | — |
| F2 | Arithmetic overflow | `checked_*` throughout ⇒ `CommandRejected`, balances untouched (the nonce is consumed). | `deposit_overflow_rejected` |
| F3 | Crash or journal failure before the record is durable | Nothing was applied or published; a torn `FileJournal` tail is truncated on open. A retry of the same nonce is applied once, or is `Duplicate` if the record survived (F4). | `failed_durability_applies_and_publishes_nothing`, `file_journal_truncates_torn_tail` |
| F4 | Crash after record, before apply | Replay applies it. | `restart_replays_to_same_state_without_republishing` |
| F5 | Crash or egress failure after apply, before publish | Replay suppresses egress, so that batch is not re-published. Its events are derivable from the journal; consumers reconcile from state or the log, matching their commands by `EventBatch.origin`. | `failed_publish_still_applies_the_durable_batch`, `restart_replays_to_same_state_without_republishing` |
| F6 | Replay yields a different state | Determinism rules (no clock, no RNG, `BTreeMap`, genesis in the log), checked live vs replay in tests and by the Docker crash demo. | `replay_reproduces_state_hash` |
| F7 | Oracle never answers | Hard `resolution_deadline` ⇒ `Final(Invalid)`, full refunds. | `oracle_silent_hard_deadline_refunds` |
| F8 | Arbiter never rules | Hard deadline ⇒ `Final(proposed)`, bond returned; a proposal's window must end before the deadline, so a timeout never yields `Invalid` once proposed. No event announces the bond (known gap, [engine.md §7](engine.md)). | `arbiter_silent_hard_deadline_returns_bond`, `propose_too_late_rejected` |
| F9 | Node down across a dispute window | On restart the node sequences `Resume` first: it extends an open window to `min(now + RESUME_GRACE, resolution_deadline − 1)`, never shortens one, and refund-only deadlines fire normally. If downtime outlasts `resolution_deadline − 1`, no dispute time is given back and the proposal finalizes as proposed. | `resume_extends_open_dispute_window`, `resume_does_not_extend_accept_window`, `restart_appends_resume` |
| F10 | Clock behind the log after a restart | The sequencer resumes `last_time` from the journal and stamps `max(wall, last_time)`; the engine also takes `max(now, time)`. | `sequencer_time_is_monotonic`, `clock_never_goes_back_across_a_restart` |
| F11 | A port fails (journal, ingress, egress) | The node latches `NodeError::Halted` and touches no port again; a restart replays the journal. `FileJournal` poisons itself on its first I/O error. | `poll_after_failure_is_refused`, `write_failure_poisons_the_handle` |
| F12 | Replay shows a duplicate or gap in `seq` | `Node::start` refuses to start (`SequenceError`): the engine would silently skip a duplicate and diverge from the live state. | `replay_refuses_a_duplicate_seq`, `replay_refuses_a_gap` |
| F13 | Aeron: a crash leaves an active recording or an unrecorded tail; repeated crashes | An exclusive per-session recording keeps a dead node's unrecorded bytes out of the log, and replay stops active recordings first. Redelivered ingress: durable ⇒ `Duplicate`, never durable ⇒ applied late, once ([transport-aeron.md](transport-aeron.md#implementation-notes-task-10)). The test is `#[ignore]`d (needs feature `aeron` and a driver). | `archive_journal_replays_every_recording_across_restarts_and_crashes` |

## Where capital could leak in the multi-leg stub

Only at holds owned by a provisionally-matched leg. Each such hold belongs to a live request with a pending deadline or `accept_by` timer (I4, I5), and every non-`Filled` exit releases all legs in one command ([engine.md §8](engine.md)). Splitting legs across shards is forbidden because it would need 2PC.
