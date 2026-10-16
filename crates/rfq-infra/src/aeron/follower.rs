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
use rusteron_archive::{Aeron, AeronArchive};

use super::archive::{
    ARCHIVE_TIMEOUT, Decoder, NULL_POSITION, Recording, Replay, connect_archive, list_recordings,
    start_replay,
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
    recording: Recording,
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
            archive: connect_archive(aeron)?,
            aeron: aeron.clone(),
        })
    }

    /// Delivers the next recorded commands in journal order, at most one poll's worth of
    /// fragments, and returns how many it delivered (`0` when caught up or waiting for a
    /// recording). Also polls the Archive control session. Never blocks for long: the one wait is
    /// for a replay to start, which takes milliseconds from a healthy Archive and fails with a
    /// timeout after 10 s from a broken one.
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
