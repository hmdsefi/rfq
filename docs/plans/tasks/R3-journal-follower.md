# Task R3: Journal follower (Aeron)

**Spec:** [read-api.md](../../specs/read-api.md) §5. **Goal:** deliver every recorded journal command, in order, for as long as the gateway runs: replay the log from the Archive and keep following it, across engine restarts.

**Files:**
- Create: `crates/rfq-infra/src/aeron/follower.rs`.
- Modify:
  - `crates/rfq-infra/src/aeron/mod.rs` (`pub const FOLLOW: i32 = 1005;` with a doc line "Archive → readers: the log replayed and followed (read side)"; `pub use follower::JournalFollower;`);
  - `crates/rfq-infra/src/aeron/archive.rs`: move into shared `pub(crate)` helpers (so the journal and the follower use the same code, unchanged in behaviour): connecting to the Archive with retries, listing the log's recordings (`Recording { id, start, stop, session }`, `NULL_POSITION`), the replay-image wait, and the fragment reassembly + decode loop body;
  - `crates/rfq-infra/tests/aeron_live.rs` (new `#[ignore]`d tests).

**Interfaces.**
- Consumes: the existing `aeron::{connect, IPC, LOG, RESOURCE_TIMEOUT, frame_of, AeronError}`, `codec::decode`, `fragment::Reassembler`.
- Produces (used by R4):

```rust
pub struct JournalFollower { /* aeron, archive control session, current recording + its replay image, reassembler */ }

impl JournalFollower {
    /// Connects to the Archive (retrying while it starts, as `ArchiveJournal::connect` does) and
    /// prepares to follow the log from its first recording. Nothing is replayed yet.
    pub fn open(aeron: &Aeron) -> Result<Self, AeronError>;
    /// Delivers the next recorded commands in journal order, at most one poll's worth of
    /// fragments, and returns how many it delivered (`0` when caught up or waiting for a
    /// recording). Also polls the Archive control session. Never blocks for long.
    pub fn poll(&mut self, sink: &mut dyn FnMut(SequencedCommand)) -> Result<usize, AeronError>;
}
```

## Requirements

1. **Start:** list the recordings of stream `LOG` (1002) in id order. With none yet, `poll` returns `Ok(0)` and lists again on a later call (at most every 100 ms).
2. **Follow one recording:** `start_replay(id, IPC, FOLLOW, params)` with `position = start` and `length = i64::MAX` (follows an active recording; a stopped one ends at its stop position). Subscribe to that replay's session only (`aeron:ipc?session-id=…`, as `replay_recording` does). Deliver each fully reassembled message decoded as `SequencedCommand`; a decode failure is `AeronError::Corrupt` (as the journal's replay).
3. **Next recording:** when the replay image reaches end of stream or closes, re-list; if the current recording has a stop position and the image reached it, close the subscription and move to the next recording id (if none yet, return `Ok(0)` and look again later). An image that ends before the recording's stop is an error.
4. **Keep the control session alive:** call `poll_for_recording_signals()` on every `poll` (the Archive closes an unpolled control session after its timeout; see the module doc of `archive.rs`).
5. **No sequencing logic here:** the follower delivers what is recorded; the replica (R2) checks `seq`. Errors are returned, never retried internally: the caller (R4) reconnects and rebuilds.
6. The journal's own behaviour (`ArchiveJournal`) is unchanged by the refactor: its existing live tests still pass.

## Steps

- [ ] **Step 1: The live test first (spike; Review Focus 1).** In `aeron_live.rs`, `#[ignore]`d, `--test-threads=1` as the file's header says:
  - `follower_delivers_every_recorded_command_in_order`: an `ArchiveJournal` (as the node uses it: `connect`, `replay` of nothing, then `append` + `wait_durable`) writes commands seq 1..=50; a `JournalFollower` opened before or after the first appends delivers exactly seq 1..=50 in order (poll until 50 or a 10 s timeout).
  - `follower_follows_appends_made_while_it_follows`: open the follower, poll to caught-up, append 20 more, they arrive.
  - `follower_moves_to_the_next_recording_after_a_restart`: write 1..=10, drop the journal (as a node stopping), open a new `ArchiveJournal` (it stops the old recording and starts a new one), write 11..=20; the follower delivers 1..=20 in order.
  - `follower_starts_with_no_recording_and_picks_up_the_first`: on an Archive with no log recording yet, `poll` returns `Ok(0)`; after a journal appends, it delivers.

  These tests need a fresh Archive each run (the file's header has the `docker run` recipe).
- [ ] **Step 2:** Refactor `archive.rs` into the shared helpers; run the existing live tests in Docker: PASS, unchanged.
- [ ] **Step 3:** Implement `follower.rs`. Build in Docker (`docker compose build engine gateway` must still build) and run the live tests from step 1: PASS. Record the run in the report.
- [ ] **Step 4: Gates:** the workspace gates (aeron off: the follower is not compiled there, so this checks nothing else broke) and the Docker build.
- [ ] **Step 5: Commit:** `feat(infra): journal follower: replay and follow the log`.
