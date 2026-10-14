# Failure-Mode Notes (D3)

Every race and partial failure handled, with its test where one exists. Detail: [engine.md](engine.md), [transport-aeron.md](transport-aeron.md). Attackers: [adversarial.md](adversarial.md).

**Global mechanisms.** (G1) One sequencer orders every command: a race is "lower `seq` wins". (G2) Validate, then apply: commands are all-or-nothing. (G3) Due timers fire before the command that reaches their deadline; only `Resume` goes first (F9). (G4) Invariants I1–I5 are checked after every command in tests. (G5) Write-ahead: durable before applied or published.

## Races

| # | Race | Handling | Test |
|---|---|---|---|
| R1 | Cancel vs response deadline `T` | A cancel stamped `< T` removes the quote; stamped `≥ T` it finds a firm `Selected` quote: `InvalidState`. | `cancel_before_deadline_removes_quote`, `cancel_after_selection_rejected` |
| R2 | Accept vs `accept_by` | An accept stamped `≥ accept_by` runs after the lapse timer: rejected. | `accept_after_window_rejected_without_tick` |
| R3 | Quote vs response deadline | A quote stamped `≥ deadline` finds the request `Presented` or `Expired`. | `quote_after_deadline_rejected` |
| R4 | Quote expiry vs selection | Eligible quotes are firm through the accept window, and expiry is clamped to it. | `short_expiry_not_selected`, `selected_quote_expiring_at_accept_by_is_left_to_the_lapse` |
| R5 | Double accept or reject | The second command finds the request no longer `Presented`: `InvalidState`. | `double_accept_rejected` |
| R6 | Duplicate or skipped command | Strict nonces: `≤ last` ⇒ `Duplicate`, `> last + 1` ⇒ `NonceGap`; neither consumes anything. | `duplicate_nonce_ignored`, `nonce_gap_rejected_and_not_consumed` |
| R7 | Withdraw vs reservation | Only `free` is withdrawable; holds come out of `free` atomically. | `withdraw_cannot_touch_reserved` |
| R8 | Quote replace vs selection | One command (release old, hold new): one active quote per MM and leg. | `replace_quote_atomic` |

## Partial failures

| # | Failure | Handling | Test |
|---|---|---|---|
| F1 | Funds short mid-accept | All legs are validated before any escrow; single-leg holds already cover accept. | — |
| F2 | Arithmetic overflow | `checked_*` ⇒ `CommandRejected`; balances untouched, nonce consumed. | `deposit_overflow_rejected` |
| F3 | Crash before durable | Nothing applied or published; torn `FileJournal` tail truncated; a retry is applied once or `Duplicate`. | `failed_durability_applies_and_publishes_nothing`, `file_journal_truncates_torn_tail` |
| F4 | Crash before apply | Replay applies it. | `restart_replays_to_same_state_without_republishing` |
| F5 | Crash or egress failure after apply | Replay suppresses egress, so that batch (or one dropped with no subscriber) is not re-published; consumers reconcile via `EventBatch.origin` or state. | `failed_publish_still_applies_the_durable_batch`, `restart_replays_to_same_state_without_republishing` |
| F6 | Replay diverges | No clock, no RNG, `BTreeMap`; replay hash checked in tests and the Docker crash demo. | `replay_reproduces_state_hash` |
| F7 | Oracle never answers | Hard `resolution_deadline` ⇒ `Final(Invalid)`, full refunds. | `oracle_silent_hard_deadline_refunds` |
| F8 | Arbiter never rules | Hard deadline ⇒ `Final(proposed)`, bond returned (no event says so, [engine.md §7](engine.md)); the window must end first. | `arbiter_silent_hard_deadline_returns_bond`, `propose_too_late_rejected` |
| F9 | Downtime in a dispute window | `Resume` is sequenced first and extends open windows to `min(now + RESUME_GRACE, resolution_deadline − 1)` ([resolution.md](resolution.md)). | `resume_extends_open_dispute_window`, `resume_does_not_extend_accept_window`, `restart_appends_resume` |
| F10 | Clock behind the log | The sequencer stamps `max(wall, last_time)`, resumed from the journal. | `sequencer_time_is_monotonic`, `clock_never_goes_back_across_a_restart` |
| F11 | A port fails | The node latches `NodeError::Halted`; restart replays. `FileJournal` poisons itself on I/O error. | `poll_after_failure_is_refused`, `write_failure_poisons_the_handle` |
| F12 | Replayed `seq` gap or duplicate | `Node::start` refuses to start (`SequenceError`); replay would otherwise diverge. | `replay_refuses_a_duplicate_seq`, `replay_refuses_a_gap` |
| F13 | Aeron crashes | Exclusive per-session recording; replay stops active recordings; redelivered ingress is `Duplicate` if durable, else applied late, once ([transport-aeron.md](transport-aeron.md#implementation-notes-task-10)). | `archive_journal_replays_every_recording_across_restarts_and_crashes` |

## Where capital could leak in the multi-leg stub

Only at holds of a provisionally matched leg. Each belongs to a live request with a pending timer (I4, I5), and every non-`Filled` exit releases all legs in one command ([engine.md §8](engine.md)).
