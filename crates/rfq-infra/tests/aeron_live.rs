//! The Aeron adapters against a real `ArchivingMediaDriver`. Ignored by default: they need the
//! driver's directory in `AERON_DIR` (default `/dev/shm/aeron`), and an archive no node uses,
//! because they record the log stream. The follower tests start by purging every recording of the
//! log, so each begins with no recording, and
//! `archive_journal_replays_every_recording_across_restarts_and_crashes` appends test commands to
//! the log and leaves a crashed recording in it: either wrecks the journal of whatever Archive
//! they run against, so they refuse to run unless `RFQ_LIVE_TEST_MAY_PURGE=1` says the Archive is
//! a throwaway one (never the compose stack's). In Docker, from the repository root:
//!
//! ```sh
//! docker build -t rfq-aeron docker/aeron
//! docker build -f docker/rust.Dockerfile --target toolchain -t rfq-toolchain .
//! docker run -d --name rfq-test-aeron --ipc=shareable --shm-size=512m rfq-aeron
//! docker run --rm --ipc=container:rfq-test-aeron -v "$PWD":/src -w /src \
//!   -e CARGO_TARGET_DIR=/tmp/target -e RFQ_LIVE_TEST_MAY_PURGE=1 rfq-toolchain \
//!   cargo test -p rfq-infra --features aeron --test aeron_live -- --ignored --test-threads=1
//! docker rm -f rfq-test-aeron
//! ```
#![cfg(feature = "aeron")]

use std::ffi::CString;
use std::time::{Duration, Instant};

use rfq_app::{BatchKind, ClientCommand, EventBatch, Journal, Transport};
use rfq_domain::{AccountId, Action, Balance, Command, ContractId, Emitted, Event, SequencedCommand};
use rfq_infra::aeron::{
    self, AeronTransport, ArchiveJournal, EGRESS, INGRESS, JournalFollower, LOG, frame_of,
};
use rfq_infra::codec::{MAX_INGRESS, decode, encode};
use rfq_infra::fragment::Reassembler;
use rusteron_archive::{
    Aeron, AeronArchive, AeronArchiveAsyncConnect, AeronArchiveContext, AeronArchiveRecordingDescriptor,
    AeronExclusivePublication, AeronHeader, AeronPublication, ChannelUri, Handlers, SOURCE_LOCATION_LOCAL,
};

const IPC: &std::ffi::CStr = c"aeron:ipc";
const PATIENCE: Duration = Duration::from_secs(10);

fn client() -> Aeron {
    let dir = std::env::var("AERON_DIR").unwrap_or_else(|_| "/dev/shm/aeron".into());
    aeron::connect(&dir).expect("a running media driver")
}

fn list_contract(description_len: usize) -> Action {
    Action::ListContract {
        id: ContractId(1),
        description: "x".repeat(description_len),
        oracle: AccountId(4),
        trading_close: 1,
        resolution_deadline: 2,
        dispute_window: 3,
    }
}

fn offer_until_ok(publication: &AeronPublication, bytes: &[u8]) {
    let start = Instant::now();
    while let Err(e) = publication.offer(bytes) {
        assert!(
            e.is_retryable() && start.elapsed() < PATIENCE,
            "offer failed: {e}"
        );
        std::thread::yield_now();
    }
}

#[test]
#[ignore = "needs a running ArchivingMediaDriver"]
fn transport_reassembles_ingress_and_splits_egress() {
    let aeron = client();
    let mut transport = AeronTransport::new(&aeron).unwrap();
    let ingress = aeron
        .async_add_publication(IPC, INGRESS)
        .unwrap()
        .poll_blocking(PATIENCE)
        .unwrap();
    let egress = aeron
        .async_add_subscription(IPC, EGRESS, Handlers::NONE, Handlers::NONE)
        .unwrap()
        .poll_blocking(PATIENCE)
        .unwrap();

    // Ingress: a small command, one far over the 1408-byte IPC MTU (several fragments), then an
    // oversized one and garbage, which are dropped, then another small one.
    let small = |nonce| ClientCommand {
        actor: AccountId(10),
        nonce,
        action: Action::Withdraw { amount: 5 },
    };
    let big = ClientCommand {
        actor: AccountId(1),
        nonce: 2,
        action: list_contract(20_000),
    };
    let oversized = encode(&ClientCommand {
        actor: AccountId(1),
        nonce: 3,
        action: list_contract(MAX_INGRESS + 1),
    });
    offer_until_ok(&ingress, &encode(&small(1)));
    offer_until_ok(&ingress, &encode(&big));
    offer_until_ok(&ingress, &oversized);
    offer_until_ok(&ingress, &[1, 2, 3]);
    offer_until_ok(&ingress, &encode(&small(4)));
    let mut got = Vec::new();
    let start = Instant::now();
    while got.len() < 3 && start.elapsed() < PATIENCE {
        transport.poll(256, &mut |c| got.push(c)).unwrap();
    }
    assert_eq!(got, vec![small(1), big, small(4)]);

    // Egress: wait for the transport's publication to see our subscription (until then a batch
    // is dropped by design), then publish a batch over the 8 MiB max message length.
    let start = Instant::now();
    while !egress.is_connected() {
        assert!(start.elapsed() < PATIENCE, "egress never connected");
        std::thread::sleep(Duration::from_millis(1));
    }
    let events: Vec<Emitted> = (0..300_000u64)
        .map(|i| {
            Emitted::to(
                AccountId(i),
                Event::Deposited {
                    account: AccountId(i),
                    amount: i,
                },
            )
        })
        .collect();
    let batch = EventBatch {
        seq: 7,
        time: 1_234,
        kind: BatchKind::User,
        origin: Some((AccountId(10), 1)),
        events,
        balances: vec![(
            AccountId(10),
            Balance {
                free: 5,
                reserved: 6,
                escrowed: 7,
            },
        )],
    };
    assert!(encode(&batch).len() > 8 << 20, "the batch must need splitting");
    transport.publish(&batch).unwrap();
    let mut reassembler = Reassembler::new(usize::MAX);
    let mut chunks: Vec<EventBatch> = Vec::new();
    let mut received = 0;
    let start = Instant::now();
    while received < batch.events.len() && start.elapsed() < PATIENCE {
        egress
            .poll_fn(
                |data: &[u8], header: AeronHeader| {
                    let (session, flags) = frame_of(&header).unwrap_or((0, 0));
                    reassembler.on_fragment(session, flags, data, &mut |msg| {
                        let chunk: EventBatch = decode(msg).expect("egress decodes");
                        received += chunk.events.len();
                        chunks.push(chunk);
                    });
                },
                256,
            )
            .unwrap();
    }
    assert!(chunks.len() >= 2, "{} chunks", chunks.len());
    assert!(
        chunks
            .iter()
            .all(|c| (c.seq, c.time, c.kind, c.origin) == (batch.seq, batch.time, batch.kind, batch.origin))
    );
    // The balances travel once, on the last chunk.
    let (last, rest) = chunks.split_last().expect("chunks");
    assert!(rest.iter().all(|c| c.balances.is_empty()));
    assert_eq!(last.balances, batch.balances);
    let joined: Vec<Emitted> = chunks.into_iter().flat_map(|c| c.events).collect();
    assert_eq!(joined, batch.events);
}

fn user(seq: u64, action: Action) -> SequencedCommand {
    SequencedCommand {
        seq,
        time: 1_000 + seq,
        cmd: Command::User {
            actor: AccountId(1),
            nonce: seq,
            action,
        },
    }
}

fn replay_all(journal: &mut ArchiveJournal) -> Vec<SequencedCommand> {
    let mut out = Vec::new();
    journal.replay(&mut |sc| out.push(sc)).unwrap();
    out
}

fn append_durably(journal: &mut ArchiveJournal, commands: &[SequencedCommand]) {
    let mut pos = 0;
    for sc in commands {
        pos = journal.append(sc).unwrap();
    }
    journal.wait_durable(pos).unwrap();
}

#[test]
#[ignore = "needs a running ArchivingMediaDriver with a fresh archive"]
fn archive_journal_replays_every_recording_across_restarts_and_crashes() {
    throwaway_archive_only("appends test commands to the log (stream 1002) and crash-seals a recording");
    let aeron = client();
    let mut first = ArchiveJournal::new(&aeron).unwrap();
    let mut log = replay_all(&mut first);
    let n = log.len() as u64;

    // A record far over the MTU is fragmented in the log and must replay whole.
    let batch1 = vec![
        user(n + 1, Action::Withdraw { amount: 1 }),
        user(n + 2, list_contract(50_000)),
        user(n + 3, Action::Withdraw { amount: 3 }),
    ];
    append_durably(&mut first, &batch1);
    log.extend(batch1);
    drop(first); // clean close: the recording stops on its own

    let mut second = ArchiveJournal::new(&aeron).unwrap();
    assert_eq!(replay_all(&mut second), log);
    let batch2 = vec![user(n + 4, Action::Withdraw { amount: 4 })];
    append_durably(&mut second, &batch2);
    log.extend(batch2);
    // A crash: `second` is never closed, so its publication stays open and its recording active,
    // as when a node has died and the driver has not yet timed its client out.

    let mut third = ArchiveJournal::new(&aeron).unwrap();
    assert_eq!(
        replay_all(&mut third),
        log,
        "replay seals the dead node's recording"
    );
    let batch3 = vec![user(n + 5, Action::Withdraw { amount: 5 })];
    append_durably(&mut third, &batch3);
    log.extend(batch3);
    // The dead node's publication offers once more (its unrecorded tail). Its session is no
    // longer recorded, so the offer finds no subscriber and never reaches the log. Had the two
    // nodes shared one publication session, `third`'s recording would hold this record: a
    // duplicate seq on the next replay.
    let zombie = second.append(&user(n + 5, Action::Withdraw { amount: 99 }));
    assert!(zombie.is_err(), "a dead node's write must not be recorded");
    assert!(
        third.replay(&mut |_| {}).is_err(),
        "replay after append would seal our own recording"
    );
    drop(third);
    drop(second);

    let mut fourth = ArchiveJournal::new(&aeron).unwrap();
    assert_eq!(replay_all(&mut fourth), log);
}

/// Aeron's "no position": the stop position of a recording that is still active.
const NULL_POSITION: i64 = -1;

/// A control session of its own, for what the tests do to the Archive directly.
fn archive(aeron: &Aeron) -> AeronArchive {
    let ctx = AeronArchiveContext::new().unwrap();
    ctx.set_aeron(aeron).unwrap();
    ctx.set_control_request_channel(IPC).unwrap();
    ctx.set_control_response_channel(IPC).unwrap();
    AeronArchiveAsyncConnect::new_with_aeron(&ctx, aeron)
        .unwrap()
        .poll_blocking(PATIENCE)
        .unwrap()
}

/// Refuses to go on unless `RFQ_LIVE_TEST_MAY_PURGE=1` says the Archive is a throwaway one: what
/// the test does to the log (`what`) would wreck the journal of the Archive it runs against.
fn throwaway_archive_only(what: &str) {
    assert!(
        std::env::var("RFQ_LIVE_TEST_MAY_PURGE").is_ok_and(|v| v == "1"),
        "this test {what} on the Archive it runs against, which wrecks that journal. Set \
         RFQ_LIVE_TEST_MAY_PURGE=1 only when the driver is a throwaway one that no node uses (see \
         the header of this file), never the compose stack's"
    );
}

/// Empties the log: stops any recording of stream 1002 that is still active (a journal dropped a
/// moment ago may not have stopped yet) and purges every recording. Each follower test starts
/// from "no recording yet", whatever ran before it on this Archive.
///
/// This destroys the journal of the Archive it runs against, so it runs only when
/// `RFQ_LIVE_TEST_MAY_PURGE=1` says that Archive is a throwaway one.
fn empty_log(aeron: &Aeron) {
    throwaway_archive_only("purges every recording of the log (stream 1002)");
    let archive = archive(aeron);
    let start = Instant::now();
    loop {
        let mut recordings = Vec::new();
        let mut count = 0;
        archive
            .list_recordings_for_uri_fn(
                &mut count,
                0,
                i32::MAX,
                IPC,
                LOG,
                |d: AeronArchiveRecordingDescriptor| recordings.push((d.recording_id(), d.stop_position())),
            )
            .unwrap();
        if recordings.is_empty() {
            return;
        }
        assert!(
            start.elapsed() < PATIENCE,
            "the log would not empty: {recordings:?}"
        );
        for (id, stop) in recordings {
            if stop == NULL_POSITION {
                archive.try_stop_recording_by_identity(id).unwrap();
            } else {
                archive.purge_recording(id).unwrap();
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Commands `seq` in `seqs`; every tenth is far over the MTU, so the log holds fragmented records
/// that must reach the follower whole.
fn commands(seqs: std::ops::RangeInclusive<u64>) -> Vec<SequencedCommand> {
    seqs.map(|seq| {
        let action = if seq % 10 == 0 {
            list_contract(50_000)
        } else {
            Action::Withdraw { amount: seq }
        };
        user(seq, action)
    })
    .collect()
}

/// `got` is `want`. The `seq`s are compared first, so a failure prints them and not the 50 KB
/// descriptions of the large commands.
fn assert_delivered(got: &[SequencedCommand], want: &[SequencedCommand]) {
    let seqs = |commands: &[SequencedCommand]| commands.iter().map(|sc| sc.seq).collect::<Vec<_>>();
    assert_eq!(seqs(got), seqs(want));
    assert!(got == want, "the same seqs, but not the same commands");
}

/// Polls `follower` until `want` commands have arrived or [`PATIENCE`] has passed, and returns
/// what arrived: the way the gateway's thread polls, sleeping 1 ms when there is nothing.
fn follow(follower: &mut JournalFollower, want: usize) -> Vec<SequencedCommand> {
    let mut got = Vec::new();
    let start = Instant::now();
    while got.len() < want && start.elapsed() < PATIENCE {
        if follower.poll(&mut |sc| got.push(sc)).unwrap() == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    got
}

/// Polls for `how_long` and returns what arrived: nothing, when the follower is caught up.
fn follow_for(follower: &mut JournalFollower, how_long: Duration) -> Vec<SequencedCommand> {
    let mut got = Vec::new();
    let start = Instant::now();
    while start.elapsed() < how_long {
        if follower.poll(&mut |sc| got.push(sc)).unwrap() == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    got
}

/// Polls until `follower` says it has caught up (and nothing is delivered on the way: callers
/// have received what they expect), or fails after [`PATIENCE`].
fn wait_caught_up(follower: &mut JournalFollower) {
    let start = Instant::now();
    while !follower.caught_up() {
        assert!(start.elapsed() < PATIENCE, "the follower never caught up");
        let mut extra = 0;
        if follower.poll(&mut |_| extra += 1).unwrap() == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(extra, 0, "commands arrived that the test did not expect");
    }
}

/// A node as `rfq-node` starts one: connect, replay what is there, then (the caller) append.
fn start_node(aeron: &Aeron) -> ArchiveJournal {
    let mut journal = ArchiveJournal::new(aeron).unwrap();
    journal.replay(&mut |_| {}).unwrap();
    journal
}

#[test]
#[ignore = "needs a running ArchivingMediaDriver"]
fn follower_delivers_every_recorded_command_in_order() {
    let aeron = client();
    empty_log(&aeron);
    let log = commands(1..=50);
    let mut journal = start_node(&aeron);
    // One follower opens before the first append, one after the first ten.
    let mut early = JournalFollower::open(&aeron).unwrap();
    assert!(!early.caught_up(), "it follows nothing yet");
    append_durably(&mut journal, &log[..10]);
    let mut late = JournalFollower::open(&aeron).unwrap();
    assert!(!late.caught_up(), "it follows nothing yet");
    for chunk in log[10..].chunks(10) {
        append_durably(&mut journal, chunk);
    }
    assert_delivered(&follow(&mut early, 50), &log);
    assert_delivered(&follow(&mut late, 50), &log);
    wait_caught_up(&mut early);
    wait_caught_up(&mut late);
    // Nothing more: not a repeat, not an extra.
    assert!(follow_for(&mut early, Duration::from_millis(300)).is_empty());
    assert!(follow_for(&mut late, Duration::from_millis(300)).is_empty());
}

#[test]
#[ignore = "needs a running ArchivingMediaDriver"]
fn follower_follows_appends_made_while_it_follows() {
    let aeron = client();
    empty_log(&aeron);
    let log = commands(1..=35);
    let mut journal = start_node(&aeron);
    append_durably(&mut journal, &log[..10]);
    let mut follower = JournalFollower::open(&aeron).unwrap();
    assert!(!follower.caught_up(), "nothing is replayed yet");
    assert_delivered(&follow(&mut follower, 10), &log[..10]);
    wait_caught_up(&mut follower);
    assert!(
        follow_for(&mut follower, Duration::from_millis(300)).is_empty(),
        "caught up: nothing more until the node appends"
    );
    assert!(follower.caught_up(), "and it stays caught up");

    // 20 more, offered one by one while the follower polls: it never runs ahead of the appends.
    let mut got = Vec::new();
    for (i, sc) in log[10..30].iter().enumerate() {
        journal.append(sc).unwrap();
        follower.poll(&mut |sc| got.push(sc)).unwrap();
        assert!(
            got.len() <= i + 1,
            "delivered {} after {} appends",
            got.len(),
            i + 1
        );
    }
    let start = Instant::now();
    while got.len() < 20 && start.elapsed() < PATIENCE {
        if follower.poll(&mut |sc| got.push(sc)).unwrap() == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert_delivered(&got, &log[10..30]);
    wait_caught_up(&mut follower);

    // Recorded and not yet polled: not caught up, until the follower has delivered it.
    append_durably(&mut journal, &log[30..]);
    assert!(!follower.caught_up(), "5 recorded commands are unread");
    assert_delivered(&follow(&mut follower, 5), &log[30..]);
    wait_caught_up(&mut follower);
}

#[test]
#[ignore = "needs a running ArchivingMediaDriver"]
fn follower_moves_to_the_next_recording_after_a_restart() {
    let aeron = client();
    empty_log(&aeron);
    let log = commands(1..=20);
    let mut first = start_node(&aeron);
    append_durably(&mut first, &log[..10]);
    let mut follower = JournalFollower::open(&aeron).unwrap();
    // The first recording, still active: caught up once it has been delivered.
    assert_delivered(&follow(&mut follower, 10), &log[..10]);
    wait_caught_up(&mut follower);

    drop(first); // the node stops: its recording ends
    let mut second = start_node(&aeron); // the restart records anew
    append_durably(&mut second, &log[10..]);
    // The second recording exists and is unread: not caught up.
    assert!(!follower.caught_up(), "a later recording is unread");
    assert_delivered(&follow(&mut follower, 10), &log[10..]);
    wait_caught_up(&mut follower);
    assert!(follow_for(&mut follower, Duration::from_millis(300)).is_empty());
    assert!(follower.caught_up());
}

#[test]
#[ignore = "needs a running ArchivingMediaDriver"]
fn follower_moves_on_after_a_node_crashed() {
    let aeron = client();
    empty_log(&aeron);
    let log = commands(1..=20);
    let mut first = start_node(&aeron);
    append_durably(&mut first, &log[..10]);
    let mut follower = JournalFollower::open(&aeron).unwrap();
    assert_delivered(&follow(&mut follower, 10), &log[..10]);
    wait_caught_up(&mut follower);

    // A crash: the publication stays open and the recording active, until the restarted node's
    // replay stops it.
    std::mem::forget(first);
    let mut second = start_node(&aeron);
    append_durably(&mut second, &log[10..]);
    assert!(!follower.caught_up(), "a later recording is unread");
    assert_delivered(&follow(&mut follower, 10), &log[10..]);
    wait_caught_up(&mut follower);
}

#[test]
#[ignore = "needs a running ArchivingMediaDriver"]
fn follower_starts_with_no_recording_and_picks_up_the_first() {
    let aeron = client();
    empty_log(&aeron);
    let mut follower = JournalFollower::open(&aeron).unwrap();
    // Nothing is recorded yet: every poll is Ok(0), for longer than its listing interval, and
    // there is nothing to be caught up with.
    let start = Instant::now();
    let mut delivered = 0;
    while start.elapsed() < Duration::from_millis(500) {
        assert_eq!(follower.poll(&mut |_| delivered += 1).unwrap(), 0);
        assert!(!follower.caught_up(), "it follows nothing");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(delivered, 0);

    let log = commands(1..=5);
    let mut journal = start_node(&aeron);
    append_durably(&mut journal, &log);
    assert_delivered(&follow(&mut follower, 5), &log);
    wait_caught_up(&mut follower);
}

/// What the node opens at its first append, before it offers anything: a recording of a new
/// exclusive publication's session, active and holding nothing. Returns the publication and the
/// channel being recorded.
fn start_empty_recording(aeron: &Aeron, archive: &AeronArchive) -> (AeronExclusivePublication, CString) {
    let publication = aeron
        .async_add_exclusive_publication(IPC, LOG)
        .unwrap()
        .poll_blocking(PATIENCE)
        .unwrap();
    let session = publication.get_constants().unwrap().session_id();
    let channel = CString::new(ChannelUri::add_session_id("aeron:ipc", session)).unwrap();
    archive
        .start_recording(&channel, LOG, SOURCE_LOCATION_LOCAL, true)
        .unwrap();
    // The Archive creates the recording once the publication's image reaches it: wait for it,
    // as stopping a recording that does not exist yet would leave none.
    let start = Instant::now();
    loop {
        let mut count = 0;
        let mut listed = 0;
        archive
            .list_recordings_for_uri_fn(
                &mut count,
                0,
                i32::MAX,
                IPC,
                LOG,
                |_: AeronArchiveRecordingDescriptor| {
                    listed += 1;
                },
            )
            .unwrap();
        if listed > 0 {
            return (publication, channel);
        }
        assert!(
            start.elapsed() < PATIENCE,
            "the archive never created the recording"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The window the node opens at its first append: the recording exists, active, and holds nothing
/// yet. The Archive accepts a follow replay of it, and what is offered next arrives.
#[test]
#[ignore = "needs a running ArchivingMediaDriver"]
fn follower_follows_a_recording_that_holds_nothing_yet() {
    let aeron = client();
    empty_log(&aeron);
    let archive = archive(&aeron);
    let (publication, _channel) = start_empty_recording(&aeron, &archive);

    let mut follower = JournalFollower::open(&aeron).unwrap();
    assert!(follow_for(&mut follower, Duration::from_millis(500)).is_empty());
    assert!(follower.caught_up(), "active, and nothing recorded to deliver");
    let log = commands(1..=3);
    let start = Instant::now();
    for sc in &log {
        let bytes = encode(sc);
        while let Err(e) = publication.offer(&bytes) {
            assert!(
                e.is_retryable() && start.elapsed() < PATIENCE,
                "offer failed: {e}"
            );
            std::thread::yield_now();
        }
    }
    assert_delivered(&follow(&mut follower, 3), &log);
    wait_caught_up(&mut follower);
}

/// A node that died right after it started its recording leaves one that holds nothing and has
/// stopped. The follower passes over it.
#[test]
#[ignore = "needs a running ArchivingMediaDriver"]
fn follower_skips_a_stopped_recording_that_holds_nothing() {
    let aeron = client();
    empty_log(&aeron);
    let archive = archive(&aeron);
    let (publication, channel) = start_empty_recording(&aeron, &archive);
    archive.stop_recording_channel_and_stream(&channel, LOG).unwrap();
    drop(publication);
    // Wait for the stop to reach the catalog, so the follower finds the recording stopped.
    let start = Instant::now();
    loop {
        let mut stops = Vec::new();
        let mut count = 0;
        archive
            .list_recordings_for_uri_fn(
                &mut count,
                0,
                i32::MAX,
                IPC,
                LOG,
                |d: AeronArchiveRecordingDescriptor| {
                    stops.push((d.start_position(), d.stop_position()));
                },
            )
            .unwrap();
        if let [(start_position, stop_position)] = stops[..]
            && stop_position != NULL_POSITION
        {
            assert_eq!(stop_position, start_position, "it holds nothing");
            break;
        }
        assert!(
            start.elapsed() < PATIENCE,
            "the recording never stopped: {stops:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let log = commands(1..=5);
    let mut follower = JournalFollower::open(&aeron).unwrap();
    assert!(follow_for(&mut follower, Duration::from_millis(300)).is_empty());
    assert!(
        !follower.caught_up(),
        "the only recording was passed over: nothing to follow"
    );
    let mut journal = start_node(&aeron);
    append_durably(&mut journal, &log);
    assert_delivered(&follow(&mut follower, 5), &log);
    wait_caught_up(&mut follower);
}

/// A follower that opens late, as the gateway does when it rebuilds: the log already holds
/// recordings of earlier nodes, stopped, and one still being written. It replays them in order
/// and keeps following the last.
#[test]
#[ignore = "needs a running ArchivingMediaDriver"]
fn follower_replays_stopped_recordings_then_follows_the_active_one() {
    let aeron = client();
    empty_log(&aeron);
    let log = commands(1..=35);
    for chunk in [&log[..10], &log[10..20]] {
        let mut node = start_node(&aeron);
        append_durably(&mut node, chunk);
    } // each node stops as it is dropped
    let mut node = start_node(&aeron);
    append_durably(&mut node, &log[20..30]);

    let mut follower = JournalFollower::open(&aeron).unwrap();
    assert!(!follower.caught_up(), "nothing is replayed yet");
    // The first recording has stopped and later ones exist: not caught up, though all of it has
    // been delivered.
    assert_delivered(&follow(&mut follower, 10), &log[..10]);
    assert!(!follower.caught_up(), "later recordings are unread");
    assert_delivered(&follow(&mut follower, 20), &log[10..30]);
    wait_caught_up(&mut follower);
    append_durably(&mut node, &log[30..]);
    assert!(!follower.caught_up(), "5 recorded commands are unread");
    assert_delivered(&follow(&mut follower, 5), &log[30..]);
    wait_caught_up(&mut follower);
}
