//! Aeron adapters (feature `aeron`): [`AeronTransport`] for client ingress and event egress, and
//! [`ArchiveJournal`], whose log is recorded by Aeron Archive and replayed from it.
//!
//! Every `rusteron` call follows the API verified by the Task 0 spike against `rusteron-archive`
//! 0.2.11 and the Java `ArchivingMediaDriver` 1.53.3 (`docs/specs/transport-aeron.md`, "Verified
//! API"). All streams are `aeron:ipc`, so the node and the driver share only `/dev/shm`.
use std::ffi::CStr;
use std::time::{Duration, Instant};

use rusteron_archive::{
    AeronArchiveError, AeronCError, AeronContext, AeronHeader, AeronOfferError, AeronPublication,
};

use crate::codec::CodecError;

mod archive;
mod transport;

pub use archive::ArchiveJournal;
pub use rusteron_archive::Aeron;
pub use transport::AeronTransport;

/// Clients → sequencer: `ClientCommand`.
pub const INGRESS: i32 = 1001;
/// Sequencer → Archive: `SequencedCommand`, the recorded log.
pub const LOG: i32 = 1002;
/// Node → clients: `EventBatch`.
pub const EGRESS: i32 = 1003;
/// Archive → node at startup: the log replayed.
pub const REPLAY: i32 = 1004;

/// The channel of every stream, and of the Archive control session. Containers share IPC, not a
/// network, so a UDP `localhost` channel would never reach the Archive.
pub(crate) const IPC: &CStr = c"aeron:ipc";

/// How long to keep retrying a connection to a driver or Archive that may still be starting.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_BACKOFF: Duration = Duration::from_millis(250);
/// How long the driver may take to create a publication or subscription.
pub(crate) const RESOURCE_TIMEOUT: Duration = Duration::from_secs(10);

/// Why an Aeron adapter call failed. The node treats every one as fatal. Each message names what
/// failed; the cause, if any, is its `source()`.
#[derive(Debug, thiserror::Error)]
pub enum AeronError {
    #[error("aeron client")]
    Client(#[from] AeronCError),
    #[error("aeron archive")]
    Archive(#[from] AeronArchiveError),
    #[error("offer to {what}")]
    Offer {
        what: &'static str,
        #[source]
        source: AeronOfferError,
    },
    #[error("{what}: no success within {after:?}")]
    GaveUp {
        what: &'static str,
        after: Duration,
        #[source]
        source: Box<AeronError>,
    },
    #[error("{what}: timed out after {after:?}")]
    Timeout { what: &'static str, after: Duration },
    #[error("{0} is closed")]
    Closed(&'static str),
    #[error("aeron directory {0:?} contains a NUL byte")]
    InvalidDir(String),
    #[error("{what}: message of {len} bytes exceeds the publication's max message length {max}")]
    TooLarge {
        what: &'static str,
        len: usize,
        max: usize,
    },
    #[error("recording {recording_id}: record ending at position {position} does not decode")]
    Corrupt {
        recording_id: i64,
        position: i64,
        #[source]
        source: CodecError,
    },
    #[error("recording {recording_id}: replay ended at position {at}, before {stop}")]
    ReplayEnded { recording_id: i64, at: i64, stop: i64 },
    #[error("wait_durable called before any append")]
    NothingAppended,
    #[error("recording {0} of the log stopped while the node was running")]
    RecordingStopped(i64),
}

/// Connects a client to the media driver whose directory is `dir` (e.g. `/dev/shm/aeron`),
/// retrying for [`CONNECT_TIMEOUT`] while the driver starts.
pub fn connect(dir: &str) -> Result<Aeron, AeronError> {
    let c_dir = std::ffi::CString::new(dir).map_err(|_| AeronError::InvalidDir(dir.to_owned()))?;
    let ctx = AeronContext::new()?;
    ctx.set_dir(&c_dir)?;
    // The C default error handler exits the process; log instead. A fatal client error closes the
    // client, so the next poll or offer fails and the node stops through its own error path.
    ctx.set_error_handler(Some(|code: i32, msg: &str| {
        tracing::error!(code, msg, "aeron client error");
    }))?;
    retry("connect to the media driver", || {
        let aeron = Aeron::new(&ctx)?;
        aeron.start()?;
        Ok(aeron)
    })
}

/// Runs `op` until it succeeds, for at most [`CONNECT_TIMEOUT`].
pub(crate) fn retry<T>(
    what: &'static str,
    mut op: impl FnMut() -> Result<T, AeronError>,
) -> Result<T, AeronError> {
    let start = Instant::now();
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if start.elapsed() < CONNECT_TIMEOUT => {
                tracing::info!(error = %chain(&e), "{what}: retrying");
                std::thread::sleep(CONNECT_BACKOFF);
            }
            Err(e) => {
                return Err(AeronError::GaveUp {
                    what,
                    after: CONNECT_TIMEOUT,
                    source: Box::new(e),
                });
            }
        }
    }
}

/// `e` and its sources, joined by `: ` (what anyhow's `{:#}` prints).
fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

/// What to do when no subscriber is connected to a publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unconnected {
    /// Keep retrying (the log: the Archive's recording subscription must be there).
    Wait,
    /// Give up on this message and return `Ok(None)` (egress: events are derivable from the log).
    Drop,
}

/// Offers `bytes`, retrying back pressure and admin actions for up to `patience`. Returns the new
/// stream position, or `None` if nobody subscribes and `unconnected` is [`Unconnected::Drop`].
pub(crate) fn offer(
    publication: &AeronPublication,
    bytes: &[u8],
    max_message: usize,
    what: &'static str,
    unconnected: Unconnected,
    patience: Duration,
) -> Result<Option<i64>, AeronError> {
    if bytes.len() > max_message {
        return Err(AeronError::TooLarge {
            what,
            len: bytes.len(),
            max: max_message,
        });
    }
    let start = Instant::now();
    loop {
        match publication.offer(bytes) {
            Ok(position) => return Ok(Some(position)),
            Err(AeronOfferError::NotConnected) if unconnected == Unconnected::Drop => return Ok(None),
            Err(e) if e.is_retryable() && start.elapsed() < patience => std::thread::yield_now(),
            Err(source) if source.is_retryable() => {
                return Err(AeronError::GaveUp {
                    what,
                    after: patience,
                    source: Box::new(AeronError::Offer { what, source }),
                });
            }
            Err(source) => return Err(AeronError::Offer { what, source }),
        }
    }
}

/// The session id and frame flags of a polled fragment: what a subscriber passes to
/// [`Reassembler::on_fragment`](crate::fragment::Reassembler::on_fragment).
pub fn frame_of(header: &AeronHeader) -> Result<(i32, u8), AeronCError> {
    let frame = header.get_values()?.frame();
    Ok((frame.session_id(), frame.flags()))
}
