# Failure-Mode Notes (D3)

Each race and partial failure handled, with its test. Detail: [engine.md](engine.md), [transport-aeron.md](transport-aeron.md). Attackers: [adversarial.md](adversarial.md).

**Global mechanisms.** (G1) One sequencer orders everything: lower `seq` wins a race. (G2) Validate, then apply: all-or-nothing. (G3) Due timers fire before the command reaching them; only `Resume` goes first (F9). (G4) Tests check I1–I5 after every command. (G5) Write-ahead: fsynced (Archive sync level 2, or `FileJournal`) before applied or published.

## Races

| # | Race | Handling | Test |
|---|---|---|---|
| R1 | Cancel vs response deadline `T` | `< T` removes the quote; `≥ T` finds it `Selected` (firm): `InvalidState`. | `cancel_before_deadline_removes_quote`, `cancel_after_selection_rejected` |
| R2 | Accept vs `accept_by` | `≥ accept_by` runs after the lapse timer: rejected. | `accept_after_window_rejected_without_tick` |
| R3 | Quote vs response deadline | `≥ deadline` finds the request `Presented` or `Expired`. | `quote_after_deadline_rejected` |
| R4 | Quote expiry vs selection | Eligible quotes are firm through the accept window; expiry is clamped to it. | `short_expiry_not_selected`, `selected_quote_expiring_at_accept_by_is_left_to_the_lapse` |
| R5 | Double accept or reject | The second finds the request no longer `Presented`: `InvalidState`. | `double_accept_rejected` |
| R6 | Duplicate or skipped command | Strict nonces: `≤ last` ⇒ `Duplicate`, `> last + 1` ⇒ `NonceGap`; neither consumes anything. | `duplicate_nonce_ignored`, `nonce_gap_rejected_and_not_consumed` |
| R7 | Withdraw vs reservation | Only `free` is withdrawable; holds leave `free` atomically. | `withdraw_cannot_touch_reserved` |
| R8 | Quote replace vs selection | One command: release old, hold new. | `replace_quote_atomic` |
| R9 | Dispute vs window end | `≥ dispute_until` runs after the window timer: `InvalidState`. | `dispute_after_window_rejected`, `dispute_at_the_window_end_is_too_late` |

## Partial failures

| # | Failure | Handling | Test |
|---|---|---|---|
| F1 | Funds short mid-accept | All legs validated before any escrow; holds already cover accept. | — |
| F2 | Arithmetic overflow | `checked_*` ⇒ `CommandRejected`; balances untouched, nonce consumed. | `deposit_overflow_rejected` |
| F3 | Crash before durable | Nothing applied or published; torn `FileJournal` tail truncated; a retry applies once or is `Duplicate`. | `failed_durability_applies_and_publishes_nothing`, `file_journal_truncates_torn_tail` |
| F4 | Crash before apply | Replay applies it. | `restart_replays_to_same_state_without_republishing` |
| F5 | Egress lost after apply | Egress is best-effort: never re-published on replay, dropped with no subscriber. Exactly-once effects (withdrawals) read the journal. | `failed_publish_still_applies_the_durable_batch`, `restart_replays_to_same_state_without_republishing` |
| F6 | Replay diverges | No clock, RNG or `HashMap`; replay hashes checked in tests and the demo. | `replay_reproduces_state_hash` |
| F7 | Oracle never answers | Hard deadline ⇒ `Final(Invalid)`, full refunds. | `oracle_silent_hard_deadline_refunds` |
| F8 | Arbiter never rules | Hard deadline ⇒ `Final(proposed)`, bond returned (`DisputeBondReturned` to the disputer). | `arbiter_silent_hard_deadline_returns_bond`, `hard_deadline_announces_the_returned_bond_to_the_disputer` |
| F9 | Downtime in a dispute window | `Resume` goes first and extends open windows to `min(now + RESUME_GRACE, resolution_deadline − 1)` ([resolution.md](resolution.md)). | `resume_extends_open_dispute_window`, `resume_does_not_extend_accept_window`, `restart_appends_resume` |
| F10 | Clock behind the log | Time is `max(wall, last_time)`, resumed from the journal. | `sequencer_time_is_monotonic`, `clock_never_goes_back_across_a_restart` |
| F11 | A port fails | The node latches `Halted`; restart replays. `FileJournal` poisons itself. | `poll_after_failure_is_refused`, `write_failure_poisons_the_handle` |
| F12 | Replayed `seq` gap or duplicate | `Node::start` refuses to start. | `replay_refuses_a_duplicate_seq`, `replay_refuses_a_gap` |
| F13 | Aeron crashes | Per-session recordings; redelivered ingress is `Duplicate` if durable, else applied once ([transport-aeron.md](transport-aeron.md#implementation-notes-task-10)). | `archive_journal_replays_every_recording_across_restarts_and_crashes` (Docker, `#[ignore]`d) |
| F14 | Live stall or clock jump | A poll gap over `STALL_GAP_MS` (1 s) ⇒ `Resume` first, as F9. **Residual:** downtime past `resolution_deadline − dispute_window` locks the oracle out: `Invalid`, refunding everyone. | `a_stall_cannot_eat_a_dispute_window`, `a_stall_longer_than_the_gap_sequences_resume_before_the_batch` |
| F15 | Egress subscriber stops polling | 5 s of tethered back-pressure halts the node, and again after each restart. **Residual:** subscribers must be venue-operated. | — |

## Multi-leg: where capital could leak

Only at holds of a provisionally matched leg. Each belongs to a live request with a pending timer (I4, I5), and every non-`Filled` exit releases all legs in one command ([engine.md §8](engine.md)).
