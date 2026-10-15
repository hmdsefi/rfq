# Failure-Mode Notes (D3)

Every race (R) and partial failure (F) handled, with a test that shows it (`cargo test --workspace`). **Mechanisms:** one sequencer orders everything, so the lower `seq` wins a race; a command is fully validated, then applied (all or nothing); due timers fire before the command that reaches them; tests check invariants I1–I5 after every command; nothing is applied or published before the journal has fsynced it. Detail: [engine.md](engine.md), [transport-aeron.md](transport-aeron.md); attackers: [adversarial.md](adversarial.md).

| # | Race or failure: handling | Test |
|---|---|---|
| R1 | **Cancel vs deadline:** after it, the quote is firm. | `cancel_after_selection_rejected` |
| R2 | **Accept vs `accept_by`:** the lapse timer fires first. | `accept_after_window_rejected_without_tick` |
| R3 | **Quote vs deadline:** the request is no longer `Open`. | `quote_after_deadline_rejected` |
| R4 | **Expiry vs selection:** must be firm through `accept_by`. | `short_expiry_not_selected` |
| R5 | **Double accept or reject:** no longer `Presented`. | `double_accept_rejected` |
| R6 | **Duplicate or skipped command:** strict nonces, nothing consumed. | `duplicate_nonce_ignored`, `nonce_gap_rejected_and_not_consumed` |
| R7 | **Withdraw vs reservation:** only `free` can be withdrawn. | `withdraw_cannot_touch_reserved` |
| R8 | **Quote replace vs selection:** one command. | `replace_quote_atomic` |
| R9 | **Dispute vs window end:** the window timer runs first. | `dispute_at_the_window_end_is_too_late` |
| F1 | **Funds short mid-accept:** checked before any escrow. | — |
| F2 | **Overflow:** `checked_*`, rejected, balances untouched. | `deposit_overflow_rejected` |
| F3 | **Crash before durable:** not applied; a torn tail is truncated. | `failed_durability_applies_and_publishes_nothing`, `file_journal_truncates_torn_tail` |
| F4 | **Crash before apply:** replay applies it. | `restart_replays_to_same_state_without_republishing` |
| F5 | **Egress lost:** never re-sent; side effects read the journal. | `failed_publish_still_applies_the_durable_batch` |
| F6 | **Replay diverges:** no clock, RNG or `HashMap`. | `replay_reproduces_state_hash` |
| F7 | **Oracle silent:** hard deadline, `Invalid`, full refunds. | `oracle_silent_hard_deadline_refunds` |
| F8 | **Arbiter silent:** hard deadline, proposal stands, bond back. | `arbiter_silent_hard_deadline_returns_bond` |
| F9 | **Downtime in a dispute window:** `Resume` extends it. | `resume_extends_open_dispute_window` |
| F10 | **Clock behind the log:** `time = max(wall, last_time)`. | `clock_never_goes_back_across_a_restart` |
| F11 | **A port fails:** the node halts; a restart replays. | `poll_after_failure_is_refused` |
| F12 | **`seq` gap or duplicate in replay:** the node won't start. | `replay_refuses_a_gap`, `replay_refuses_a_duplicate_seq` |
| F13 | **Aeron crash (Docker test):** redelivery is `Duplicate` or applied once. | `archive_journal_replays_every_recording_across_restarts_and_crashes` |
| F14 | **Stall or clock jump:** `Resume` first, as F9. *Residual:* long downtime can lock the oracle out: `Invalid`, refunds. | `a_stall_cannot_eat_a_dispute_window` |
| F15 | **Egress reader stops:** the node halts after 5 s. *Residual:* readers must be venue-run. | — |

**Multi-leg: where capital could leak.** Only in the holds of a provisionally matched leg, each owned by a live request with a pending timer (I4, I5). Every exit but `Filled` releases every leg's holds in one command ([engine.md §8](engine.md)); tests `leg_two_of_three_unfilled_expires_the_whole_request_and_releases_every_hold`, `a_selected_leg_cannot_be_withdrawn`, scenario `multi_leg_leg2_fails`.
