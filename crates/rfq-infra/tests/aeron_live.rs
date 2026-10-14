//! The Aeron adapters against a real `ArchivingMediaDriver`. Ignored by default: they need the
//! driver's directory in `AERON_DIR` (default `/dev/shm/aeron`), and an archive no node uses,
//! because they record the log stream. In Docker, from the repository root:
//!
//! ```sh
//! docker build -t rfq-aeron docker/aeron
//! docker build -f docker/rust.Dockerfile --target toolchain -t rfq-toolchain .
//! docker run -d --name rfq-test-aeron --ipc=shareable --shm-size=512m rfq-aeron
//! docker run --rm --ipc=container:rfq-test-aeron -v "$PWD":/src -w /src \
//!   -e CARGO_TARGET_DIR=/tmp/target rfq-toolchain \
//!   cargo test -p rfq-infra --features aeron --test aeron_live -- --ignored --test-threads=1
//! docker rm -f rfq-test-aeron
//! ```
#![cfg(feature = "aeron")]

use std::time::{Duration, Instant};

use rfq_app::{ClientCommand, EventBatch, Journal, Transport};
use rfq_domain::{AccountId, Action, Command, ContractId, Emitted, Event, SequencedCommand};
use rfq_infra::aeron::{self, AeronTransport, ArchiveJournal, EGRESS, INGRESS, frame_of};
use rfq_infra::codec::{MAX_INGRESS, decode, encode};
use rfq_infra::fragment::Reassembler;
use rusteron_archive::{Aeron, AeronHeader, AeronPublication, Handlers};

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
        origin: Some((AccountId(10), 1)),
        events,
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
    assert!(chunks.iter().all(|c| c.seq == 7 && c.origin == batch.origin));
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
