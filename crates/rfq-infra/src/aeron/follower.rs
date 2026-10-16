//! [`JournalFollower`]: the log, replayed from the Archive and followed as it grows (the read
//! side's input, `docs/specs/read-api.md` §5).
//!
//! The log is the sequence of all recordings of stream 1002 in recording-id order (see
//! [`ArchiveJournal`](super::ArchiveJournal)). The follower replays one recording from its start
//! onto stream 1005 with length `i64::MAX`: the Archive replays what is recorded and, while the
//! recording is active, keeps replaying as it grows. When the node stops, the recording gets its
//! stop position, the replay ends there, and the follower moves on to the next recording, which
//! the restarted node creates (every start records anew).
//!
//! **Only durable commands.** A replay delivers what the Archive has recorded, so a command the
//! node has offered but the Archive has not yet written never reaches the follower, and a message
//! torn at a crash's stop position never completes.
//!
//! **No sequencing, no retries.** The follower delivers what is recorded, in order; checking `seq`
//! is the caller's job. Any error is returned as it is, and the caller rebuilds from scratch.
//!
//! **The control session** must be polled, or the Archive closes it after its timeout (see the
//! module doc of `archive.rs`): every [`poll`](JournalFollower::poll) does.
use std::time::{Duration, Instant};

use rfq_domain::SequencedCommand;
use rusteron_archive::{Aeron, AeronArchive, AeronCountersReader, RecordingPos};

use super::archive::{
    ARCHIVE_TIMEOUT, Decoder, NULL_COUNTER_ID, NULL_POSITION, Recording, Replay, connect_archive,
    list_recordings, start_replay,
};
use super::{AeronError, FOLLOW};

/// How often to look in the Archive's catalog: for a recording that does not exist yet, and for
/// the stop position of a recording whose replay has ended.
const LIST_INTERVAL: Duration = Duration::from_millis(100);

/// Replays the journal's recordings in order and follows the one still being written.
///
/// Like every `rusteron` handle (and so like [`Aeron`] itself) it is not `Send`: open it, with a
/// client connected, on the thread that polls it.
pub struct JournalFollower {
    // Declared in the order to drop: the replay's subscription goes before the control session and
    // the client they belong to.
    state: State,
    decoder: Decoder,
    /// The driver's counters: where the recording's position is read, as `ArchiveJournal` does.
    counters: AeronCountersReader,
    archive: AeronArchive,
    aeron: Aeron,
}

enum State {
    /// Following no recording: the next one has not been created yet (or this is the start).
    Idle {
        /// The last recording delivered in full; `None` before the first.
        done: Option<i64>,
        /// When the catalog was last listed.
        listed: Option<Instant>,
    },
    Following(Following),
}

/// One recording being replayed.
struct Following {
    /// As listed when its replay started: `stop` is [`NULL_POSITION`] if it was active then.
    recording: Recording,
    /// Whether that listing showed a recording after this one.
    later: bool,
    replay: Replay,
    /// Set once the replay has ended while the catalog still showed the recording active.
    ended: Option<Ended>,
}

struct Ended {
    /// When the replay was first seen to end.
    since: Instant,
    /// When the catalog was last listed for it.
    listed: Instant,
}

impl JournalFollower {
    /// Connects to the Archive (retrying while it starts, as [`ArchiveJournal::new`] does) and
    /// prepares to follow the log from its first recording. Nothing is replayed yet.
    ///
    /// [`ArchiveJournal::new`]: super::ArchiveJournal::new
    pub fn open(aeron: &Aeron) -> Result<Self, AeronError> {
        Ok(JournalFollower {
            state: State::Idle {
                done: None,
                listed: None,
            },
            decoder: Decoder::new(),
            counters: aeron.counters_reader(),
            archive: connect_archive(aeron)?,
            aeron: aeron.clone(),
        })
    }

    /// Whether the follower has delivered everything the Archive has recorded: it follows the
    /// log's last recording, still active, and its replay has reached the recorded position (read
    /// from the recording's position counter, as `ArchiveJournal::wait_durable` does).
    ///
    /// `false` while the follower is idle (no recording yet, or waiting for the next one), while
    /// it follows a recording that has stopped, while the last listing shows a later recording,
    /// and while the replay is behind the recorded position. Ask between polls; a poll that
    /// returned `Ok(0)` says nothing about it. A command the Archive has recorded only in part
    /// (a fragmented one) is not yet a recorded command.
    pub fn caught_up(&self) -> bool {
        let State::Following(following) = &self.state else {
            return false;
        };
        let image = &following.replay.image;
        if following.recording.stop != NULL_POSITION
            || following.later
            || image.is_end_of_stream()
            || image.is_closed()
        {
            return false;
        }
        // The replay's position first: the recorded position read after it can only be larger, so
        // `recorded <= consumed` holds only if the replay had caught up when it was read.
        let consumed = image.position();
        let id = following.recording.id;
        let counter_id = RecordingPos::find_counter_id_by_recording_id(&self.counters, id);
        if counter_id == NULL_COUNTER_ID {
            return false; // no active recording has this id: it has stopped
        }
        let recorded = self.counters.get_counter_value(counter_id);
        // A stopped recording frees its counter, which may be reused: trust the value only while
        // the counter is still this recording's.
        RecordingPos::is_active(&self.counters, counter_id, id).unwrap_or(false) && recorded <= consumed
    }

    /// Delivers the next recorded commands in journal order, at most one poll's worth of
    /// fragments, and returns how many it delivered. Also polls the Archive control session.
    /// Never blocks for long: the one wait is for a replay to start, which takes milliseconds
    /// from a healthy Archive and fails with a timeout after 10 s from a broken one.
    ///
    /// `Ok(0)` means only that this call delivered nothing: the follower may be caught up, but it
    /// may also be waiting for a recording, have just started the next recording's replay, or
    /// have stopped in the middle of a fragmented command. [`caught_up`](Self::caught_up) says
    /// whether it has delivered everything recorded.
    ///
    /// Any error ends the follower: the caller opens a new one.
    pub fn poll(&mut self, sink: &mut dyn FnMut(SequencedCommand)) -> Result<usize, AeronError> {
        // The Archive closes a control session that nobody polls.
        self.archive.poll_for_recording_signals()?;
        if matches!(self.state, State::Idle { .. }) {
            self.look_for_next()?;
        }
        let State::Following(following) = &mut self.state else {
            return Ok(0);
        };
        let mut delivered = 0;
        let fragments = self
            .decoder
            .poll(&following.replay.image, following.recording.id, &mut |sc| {
                delivered += 1;
                sink(sc);
            })?;
        if fragments > 0 {
            return Ok(delivered);
        }
        // Nothing to read: caught up with an active recording, or the replay is over.
        let image = &following.replay.image;
        if image.is_end_of_stream() || image.is_closed() {
            self.replay_ended()?;
        }
        Ok(0)
    }

    /// While idle: lists the catalog, at most every [`LIST_INTERVAL`], and starts the next
    /// recording if it exists.
    fn look_for_next(&mut self) -> Result<(), AeronError> {
        let State::Idle { done, listed } = &self.state else {
            return Ok(());
        };
        if listed.is_some_and(|at| at.elapsed() < LIST_INTERVAL) {
            return Ok(());
        }
        let done = *done;
        let recordings = list_recordings(&self.archive)?;
        self.follow_next(done, &recordings)
    }

    /// The replay of the current recording has ended. If the recording is stopped and the replay
    /// reached its stop position, moves to the next recording; if it stopped short, that is an
    /// error. A recording the catalog still shows active is given [`ARCHIVE_TIMEOUT`] to get its
    /// stop position.
    fn replay_ended(&mut self) -> Result<(), AeronError> {
        let State::Following(following) = &mut self.state else {
            return Ok(());
        };
        let since = match &following.ended {
            // The catalog was consulted a moment ago and showed the recording active.
            Some(ended) if ended.listed.elapsed() < LIST_INTERVAL => return Ok(()),
            Some(ended) => ended.since,
            None => Instant::now(),
        };
        following.ended = Some(Ended {
            since,
            listed: Instant::now(),
        });
        let id = following.recording.id;
        let at = following.replay.image.position();
        let recordings = list_recordings(&self.archive)?;
        match recordings.iter().find(|r| r.id == id) {
            None => Err(AeronError::RecordingGone(id)),
            Some(r) if r.stop == NULL_POSITION => {
                if since.elapsed() > ARCHIVE_TIMEOUT {
                    Err(AeronError::ReplayCutShort { recording_id: id, at })
                } else {
                    Ok(())
                }
            }
            // The image position also counts padding frames, as the stop position does.
            Some(r) if at >= r.stop => {
                tracing::info!(
                    recording_id = id,
                    stop = r.stop,
                    "followed the recording to its end"
                );
                self.follow_next(Some(id), &recordings)
            }
            Some(r) => Err(AeronError::ReplayEnded {
                recording_id: id,
                at,
                stop: r.stop,
            }),
        }
    }

    /// Starts following the first of `recordings` after `done` (every recording when `None`),
    /// skipping those stopped before they recorded anything; with none, goes idle and waits for
    /// the node to create one.
    fn follow_next(&mut self, done: Option<i64>, recordings: &[Recording]) -> Result<(), AeronError> {
        let next = recordings.iter().find(|r| {
            let after_done = done.is_none_or(|d| r.id > d);
            // The Archive refuses a replay of a stopped recording that holds nothing (invalid
            // position); an active one that holds nothing yet it accepts.
            let stopped_empty = r.stop != NULL_POSITION && r.stop <= r.start;
            after_done && !stopped_empty
        });
        match next {
            Some(r) => {
                tracing::info!(
                    recording_id = r.id,
                    start = r.start,
                    stop = r.stop,
                    "following the log: replaying a recording"
                );
                let replay = start_replay(&self.aeron, &self.archive, r, i64::MAX, FOLLOW)?;
                // A message torn at the end of the previous recording never completes.
                self.decoder = Decoder::new();
                self.state = State::Following(Following {
                    recording: *r,
                    later: recordings.iter().any(|later| later.id > r.id),
                    replay,
                    ended: None,
                });
            }
            None => {
                self.state = State::Idle {
                    done,
                    listed: Some(Instant::now()),
                };
            }
        }
        Ok(())
    }
}
