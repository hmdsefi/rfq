//! [`ArchiveJournal`]: the log is stream 1002, recorded by Aeron Archive.
//!
//! Each node start records the stream anew, so the log is the sequence of all recordings of
//! stream 1002 in recording-id order. Startup replays each over `[start, stop)` onto stream 1004.
//!
//! **One writer per driver.** The journal assumes it is the only node writing stream 1002 on this
//! media driver and Archive: at startup it stops every recording of the stream that is still
//! active, which would cut off a second live node.
//!
//! **Only our own bytes are recorded.** Each node writes the log through an *exclusive*
//! publication, which has a session of its own, and records exactly that session
//! (`aeron:ipc?session-id=<ours>`). A crashed node's publication lives on in the driver until its
//! client times out (about 10 s), possibly holding commands it offered but the Archive never
//! recorded. Those were never durable, so never applied or published. With a shared publication
//! the next node would inherit that session, and its recording could capture those bytes as if
//! they were its own (duplicate `seq`s). A session-scoped recording of an exclusive publication
//! cannot, and before the first append the new recording must start exactly at our publication's
//! position.
//!
//! **Crash handling.** A dead node's recording stays active until the driver times it out. Replay
//! first stops every active recording (by recording id), so each has a final stop position and
//! the log cannot grow while it is read.
//!
//! **Durability** is read from the recording's position counter in the driver's shared memory
//! (the value `get_recording_position` reports), not asked of the Archive. The Archive pings each
//! control session every second and closes it once a ping has not fitted in the response stream
//! for its connect timeout; an unpolled session is gone after about nine idle minutes. So the
//! control session is used only at startup (replay, starting the recording) and closed right
//! after: closing it does not stop the recording, and an unpolled tethered response subscription
//! could otherwise hold back the Archive's other clients.
use std::collections::BTreeSet;
use std::ffi::CString;
use std::time::{Duration, Instant};

use rfq_app::{BoxError, Journal};
use rfq_domain::SequencedCommand;
use rusteron_archive::{
    Aeron, AeronArchive, AeronArchiveAsyncConnect, AeronArchiveContext, AeronArchiveErrorCode,
    AeronArchiveRecordingDescriptor, AeronArchiveReplayParams, AeronCountersReader,
    AeronExclusivePublication, AeronHeader, ChannelUri, Handlers, RecordingPos, SOURCE_LOCATION_LOCAL,
};

use super::{AeronError, IPC, LOG, REPLAY, RESOURCE_TIMEOUT, Unconnected, frame_of, offer, retry};
use crate::codec::{decode, encode};
use crate::fragment::Reassembler;

/// Aeron's "no position": the stop position of a recording that is still active.
const NULL_POSITION: i64 = -1;
/// Aeron's "no counter".
const NULL_COUNTER_ID: i32 = -1;
/// How long an offer to the log may stay back-pressured or unconnected.
const LOG_PATIENCE: Duration = Duration::from_secs(10);
/// How long the Archive may take to record an appended command.
const DURABLE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a replay may make no progress, and a stopped recording may take to get its stop
/// position.
const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// Fragments per replay poll.
const REPLAY_FRAGMENTS: usize = 256;
/// `start_recording` attempts that may hit `ActiveSubscription`.
const START_ATTEMPTS: usize = 3;

/// A recording of the log stream, from the Archive's catalog.
#[derive(Clone, Copy, Debug)]
struct Recording {
    id: i64,
    start: i64,
    /// [`NULL_POSITION`] while active.
    stop: i64,
    session: i32,
}

/// This node's own recording and the exclusive publication it records.
struct Log {
    publication: AeronExclusivePublication,
    max_message: usize,
    recording_id: i64,
    /// The recording's position counter: how far the Archive has written the log.
    counters: AeronCountersReader,
    counter_id: i32,
}

pub struct ArchiveJournal {
    aeron: Aeron,
    /// The Archive control session: used by `replay` and to start the recording, then closed.
    archive: Option<AeronArchive>,
    /// Started by the first `append`, after `replay` has read the earlier recordings.
    log: Option<Log>,
}

impl ArchiveJournal {
    /// Connects to the Archive over its local IPC control channel, retrying while it starts.
    pub fn new(aeron: &Aeron) -> Result<Self, AeronError> {
        let ctx = AeronArchiveContext::new()?;
        ctx.set_aeron(aeron)?;
        ctx.set_control_request_channel(IPC)?;
        ctx.set_control_response_channel(IPC)?;
        ctx.set_error_handler(Some(|code: i32, msg: &str| {
            tracing::error!(code, msg, "aeron archive error");
        }))?;
        let archive = retry("connect to the archive", || {
            Ok(AeronArchiveAsyncConnect::new_with_aeron(&ctx, aeron)?
                .poll_blocking(Duration::from_secs(5))?)
        })?;
        tracing::info!(archive_id = archive.get_archive_id(), "connected to the archive");
        Ok(ArchiveJournal {
            aeron: aeron.clone(),
            archive: Some(archive),
            log: None,
        })
    }

    /// The control session, which is closed once this node's recording has started.
    fn archive(&self) -> Result<&AeronArchive, AeronError> {
        self.archive.as_ref().ok_or(AeronError::ControlSessionClosed)
    }

    /// Every recording of the log stream, in recording-id order.
    fn recordings(&self) -> Result<Vec<Recording>, AeronError> {
        let mut out = Vec::new();
        let mut count = 0;
        // `aeron:ipc` matches as a fragment, so session-scoped recordings are listed too.
        self.archive()?.list_recordings_for_uri_fn(
            &mut count,
            0,
            i32::MAX,
            IPC,
            LOG,
            |d: AeronArchiveRecordingDescriptor| {
                out.push(Recording {
                    id: d.recording_id(),
                    start: d.start_position(),
                    stop: d.stop_position(),
                    session: d.session_id(),
                });
            },
        )?;
        out.sort_by_key(|r| r.id);
        Ok(out)
    }

    /// Stops every recording left active by a node that crashed, then waits until each has its
    /// final stop position.
    fn sealed_recordings(&self) -> Result<Vec<Recording>, AeronError> {
        let start = Instant::now();
        let mut stop_requested = false;
        loop {
            let all = self.recordings()?;
            let active: Vec<i64> = all
                .iter()
                .filter(|r| r.stop == NULL_POSITION)
                .map(|r| r.id)
                .collect();
            if active.is_empty() {
                return Ok(all);
            }
            if !stop_requested {
                tracing::warn!(
                    "a log recording is still active (the last node did not stop cleanly); stopping it"
                );
                for &id in &active {
                    self.archive()?.try_stop_recording_by_identity(id)?;
                }
                stop_requested = true;
            }
            if start.elapsed() > ARCHIVE_TIMEOUT {
                return Err(AeronError::Timeout {
                    what: "stop the active log recording",
                    after: ARCHIVE_TIMEOUT,
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Replays `[r.start, r.stop)` of one recording onto the replay stream, feeding each command
    /// to `f`. A message torn at `r.stop` by a crash never completes, so it is never fed.
    fn replay_recording(&self, r: &Recording, f: &mut dyn FnMut(SequencedCommand)) -> Result<(), AeronError> {
        let params = AeronArchiveReplayParams::builder()
            .position(r.start)
            .length(r.stop - r.start)
            .build()?;
        let replay_session = self.archive()?.start_replay(r.id, IPC, REPLAY, &params)?;
        // Subscribe to this replay's session only, so no other replay on the stream can mix in.
        let session = image_session(replay_session);
        let channel = session_channel(session);
        let subscription = self
            .aeron
            .async_add_subscription(&channel, REPLAY, Handlers::NONE, Handlers::NONE)?
            .poll_blocking(RESOURCE_TIMEOUT)?;
        let waiting = Instant::now();
        let image = loop {
            if let Some(image) = subscription.image_by_session_id(session) {
                break image;
            }
            if waiting.elapsed() > ARCHIVE_TIMEOUT {
                return Err(AeronError::Timeout {
                    what: "replay image to appear",
                    after: ARCHIVE_TIMEOUT,
                });
            }
            std::thread::sleep(Duration::from_millis(1));
        };

        let mut reassembler = Reassembler::new(usize::MAX);
        let mut failure: Option<AeronError> = None;
        let mut progress = Instant::now();
        // The image position also counts padding frames, which reach no handler.
        while image.position() < r.stop {
            let fragments = image.poll_fn(
                |data: &[u8], header: AeronHeader| {
                    if failure.is_some() {
                        return;
                    }
                    let position = header.position();
                    match frame_of(&header) {
                        Ok((session, flags)) => {
                            reassembler.on_fragment(session, flags, data, &mut |msg| match decode(msg) {
                                Ok(sc) => f(sc),
                                Err(source) => {
                                    failure = Some(AeronError::Corrupt {
                                        recording_id: r.id,
                                        position,
                                        source,
                                    });
                                }
                            });
                        }
                        Err(e) => failure = Some(e.into()),
                    }
                },
                REPLAY_FRAGMENTS,
            )?;
            if let Some(e) = failure {
                return Err(e);
            }
            if fragments > 0 {
                progress = Instant::now();
            } else if image.is_end_of_stream() || image.is_closed() {
                return Err(AeronError::ReplayEnded {
                    recording_id: r.id,
                    at: image.position(),
                    stop: r.stop,
                });
            } else if progress.elapsed() > ARCHIVE_TIMEOUT {
                return Err(AeronError::Timeout {
                    what: "replay to make progress",
                    after: ARCHIVE_TIMEOUT,
                });
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    /// This node's log, started by the first call: an exclusive publication on the log stream
    /// and a new recording of exactly its session. Closes the control session afterwards.
    fn log(&mut self) -> Result<&Log, AeronError> {
        if self.log.is_none() {
            self.log = Some(self.start_log()?);
            // Dropping the last handle closes the control session (the recording continues).
            self.archive = None;
            tracing::info!("closed the archive control session");
        }
        Ok(self.log.as_ref().expect("set above"))
    }

    fn start_log(&self) -> Result<Log, AeronError> {
        let archive = self.archive()?;
        let before: BTreeSet<i64> = self.recordings()?.iter().map(|r| r.id).collect();
        // Exclusive: a session of our own, never shared with a crashed node's publication.
        let publication = self
            .aeron
            .async_add_exclusive_publication(IPC, LOG)?
            .poll_blocking(RESOURCE_TIMEOUT)?;
        let constants = publication.get_constants()?;
        let (session, max_message) = (constants.session_id(), constants.max_message_length());
        let channel = session_channel(session);
        let mut attempt = 1;
        loop {
            match archive.start_recording(&channel, LOG, SOURCE_LOCATION_LOCAL, true) {
                Ok(_) => break,
                Err(e) if e.code == AeronArchiveErrorCode::ActiveSubscription && attempt < START_ATTEMPTS => {
                    tracing::warn!(
                        session,
                        "this log session is already being recorded; stopping that recording"
                    );
                    archive.stop_recording_channel_and_stream(&channel, LOG)?;
                    attempt += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }
        // The new recording is the one not listed before, of our session.
        let waiting = Instant::now();
        let recording = loop {
            let new = self
                .recordings()?
                .into_iter()
                .filter(|r| !before.contains(&r.id) && r.session == session)
                .max_by_key(|r| r.id);
            if let Some(r) = new {
                break r;
            }
            if waiting.elapsed() > ARCHIVE_TIMEOUT {
                return Err(AeronError::Timeout {
                    what: "the archive to create this node's log recording",
                    after: ARCHIVE_TIMEOUT,
                });
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        // Nothing has been offered yet, so a recording that starts anywhere but at our position
        // would hold bytes that are not ours.
        let position = publication.position();
        if recording.start != position {
            return Err(AeronError::RecordingMisaligned {
                recording_id: recording.id,
                start: recording.start,
                position,
            });
        }
        let recording_id = recording.id;
        let counters = self.aeron.counters_reader();
        let waiting = Instant::now();
        let counter_id = loop {
            let id = RecordingPos::find_counter_id_by_recording_id(&counters, recording_id);
            if id != NULL_COUNTER_ID {
                break id;
            }
            if waiting.elapsed() > ARCHIVE_TIMEOUT {
                return Err(AeronError::Timeout {
                    what: "the position counter of this node's log recording",
                    after: ARCHIVE_TIMEOUT,
                });
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        tracing::info!(
            recording_id,
            session,
            max_message,
            "recording the log (stream 1002)"
        );
        Ok(Log {
            publication,
            max_message,
            recording_id,
            counters,
            counter_id,
        })
    }
}

impl Journal for ArchiveJournal {
    /// The position is the log publication's stream position after the record.
    fn append(&mut self, sc: &SequencedCommand) -> Result<u64, BoxError> {
        let log = self.log()?;
        let bytes = encode(sc);
        let position = offer(
            |b| log.publication.offer(b),
            &bytes,
            log.max_message,
            "the log",
            Unconnected::Wait,
            LOG_PATIENCE,
        )?
        .expect("Unconnected::Wait never drops");
        Ok(u64::try_from(position).expect("offer returns a non-negative position"))
    }

    /// Waits until the Archive has recorded this node's recording up to `pos`, reading the
    /// recording's position counter (the value `get_recording_position` reports).
    fn wait_durable(&mut self, pos: u64) -> Result<(), BoxError> {
        let log = self.log.as_ref().ok_or(AeronError::NothingAppended)?;
        let target = i64::try_from(pos).expect("append returned it from an i64");
        let start = Instant::now();
        loop {
            let recorded = log.counters.get_counter_value(log.counter_id);
            // Trust the value only while the counter still belongs to our recording: a stopped
            // recording frees its counter, which may then be reused.
            if !RecordingPos::is_active(&log.counters, log.counter_id, log.recording_id)
                .map_err(AeronError::from)?
            {
                return Err(AeronError::RecordingStopped(log.recording_id).into());
            }
            if recorded >= target {
                return Ok(());
            }
            if start.elapsed() > DURABLE_TIMEOUT {
                return Err(AeronError::Timeout {
                    what: "the archive to record the log",
                    after: DURABLE_TIMEOUT,
                }
                .into());
            }
            std::thread::yield_now();
        }
    }

    /// Replays every recording of the log stream in recording-id order. Only before the first
    /// `append`: afterwards it would seal this node's own live recording.
    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> Result<(), BoxError> {
        if self.log.is_some() {
            return Err(AeronError::ReplayAfterAppend.into());
        }
        for r in self.sealed_recordings()? {
            if r.stop > r.start {
                self.replay_recording(&r, f)?;
            }
        }
        Ok(())
    }
}

/// `aeron:ipc?session-id=<session>`: the IPC channel restricted to one publication session.
fn session_channel(session: i32) -> CString {
    CString::new(ChannelUri::add_session_id("aeron:ipc", session))
        .expect("a channel URI built from an IPC channel and a number has no NUL byte")
}

/// The session id of a replay's image: by Aeron's definition, the low 32 bits of the replay
/// session id that `start_replay` returns.
#[allow(clippy::cast_possible_truncation)]
fn image_session(replay_session_id: i64) -> i32 {
    replay_session_id as i32
}
