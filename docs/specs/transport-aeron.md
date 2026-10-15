# Transport Spec: Aeron + Archive

## Streams (all `aeron:ipc`, shared `/dev/shm/aeron`)

| Stream | Id | Writer → Reader | Payload |
|---|---|---|---|
| Ingress | 1001 | clients → node | `ClientCommand{actor, nonce, action}` |
| Log | 1002 | node → Archive (recorded, never read live) | `SequencedCommand` |
| Egress | 1003 | node → clients | `EventBatch{seq, time, kind, origin, events, balances}` |
| Replay | 1004 | Archive → node at startup | `SequencedCommand` |

Stream ids start at 1001 because Aeron Archive's local control channel already uses `aeron:ipc` stream 10.

The engine does not subscribe to the log stream. It runs in the same process as the sequencer and applies a command as soon as the Archive has recorded it (`wait_durable`); the only way back out of stream 1002 is replay on 1004.

Encoding: `serde` + `bincode` (fixed-int, little-endian, trailing bytes rejected), with a `u8` version prefix. Size policy: ingress messages longer than `MAX_INGRESS` (64 KiB) are dropped before decoding; journal records and egress batches are produced by the node and are not size-limited by the codec (the Aeron adapter splits an egress batch that exceeds the transport's max message size into several `EventBatch` chunks with the same `seq`). A message that fails to decode on ingress is dropped and logged; it never reaches the log. Ingress decodes to `ClientCommand`, which cannot express `Genesis`, `Tick` or `Resume`. Only the node itself creates those.

## Sequencer

`rfq_app::Sequencer` is only the stamp: `seq = last_seq + 1` and `time = max(last_time, wall_clock_ms)`, so time never goes backwards. `Node::poll` drives it, and `ArchiveJournal::append` does the publishing.

- Each loop iteration takes up to `MAX_BATCH` (256) ingress commands and stamps each one.
- When there's no client traffic and the engine's earliest timer (`Engine::next_timer()`) is due, it stamps a `Tick`. Idle periods add nothing to the log.
- It publishes the `SequencedCommand` to stream 1002 through an exclusive publication. Back pressure, admin actions and `NOT_CONNECTED` (the Archive's recording subscription not attached yet) are retried for up to 10 s; after that, and on `CLOSED`, `MAX_POSITION_EXCEEDED` or any other offer error, it is fatal and the node exits. Restarting resumes cleanly.

## Journal (the `Journal` trait)

```rust
// crates/rfq-app/src/ports.rs — typed ports; adapters live in rfq-infra
pub trait Journal {
    fn append(&mut self, sc: &SequencedCommand) -> Result<u64, BoxError>;   // position; NOT yet durable
    fn wait_durable(&mut self, pos: u64) -> Result<(), BoxError>;          // write-ahead gate
    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> Result<(), BoxError>;
}
pub trait Transport {
    fn poll(&mut self, max: usize, sink: &mut dyn FnMut(ClientCommand)) -> Result<(), BoxError>;
    fn publish(&mut self, batch: &EventBatch) -> Result<(), BoxError>;
}
// Any Err is fatal: the Node latches it (NodeError::Halted) and FileJournal poisons itself.
```

- **`ArchiveJournal`:** each node start writes the log through a new *exclusive* publication on `aeron:ipc` stream 1002 and calls `start_recording(aeron:ipc?session-id=<that publication's session>, 1002, LOCAL)`, which creates a new recording of exactly that session. Together, all the recordings of stream 1002 in recording-id order make up the log. `wait_durable` reads the recording's position counter in shared memory until it reaches the publication position returned by `offer`. `replay` (allowed only before the first `append`) stops any recording a crashed node left active, then replays every recording for stream 1002 in order onto stream 1004. Details and reasons: [Implementation notes](#implementation-notes-task-10).
- **`FileJournal` (fallback; `RFQ_JOURNAL=file:<path>`):** an append-only file of length-prefixed records with a CRC32 on each. `wait_durable` = `File::sync_data`. On open it truncates an incomplete or checksum-failing tail: that tail was never durable, so it was never applied (a bad checksum mid-file also truncates everything after it, even records that were durable and published: a residual risk of this fallback journal, [adversarial.md](adversarial.md)). A checksum-valid record that does not decode is `Corrupt` and is never truncated. The first I/O error poisons the handle. The node still needs a media driver for the Aeron transport.
- **`MemJournal`:** used by tests.

## Startup

1. Connect to the media driver and the archive.
2. If the journal is empty, durably append `Genesis` as seq 1 (the `rfq-node` binary builds it from `demo_config()`, [engine.md §9](engine.md)). A non-empty journal keeps its own recorded `Genesis`.
3. Replay everything through `Engine::apply` with egress **suppressed**. The replayed `seq`s must be exactly `1, 2, 3, …`: `Node::start` refuses a duplicate or a gap with `NodeError::Journal(SequenceError)`, whatever the journal (the engine would silently skip a duplicate `seq`, and the state would diverge from the one built live). Record `last_seq` and `last_time`; the sequencer resumes from them, so time never goes backwards across a restart.
4. Log `state_hash`, then enter the live loop at `last_seq + 1`.
5. If the journal was non-empty (a restart, not a fresh start), the first live-loop iteration sequences `Resume`, stamped with the wall clock, before any ingress command or `Tick`. The engine handles it before firing due timers, so downtime can't eat an open dispute window ([engine.md §6.3](engine.md)). A live iteration that starts more than `STALL_GAP_MS` (1 s) after the previous one does the same: the loop polls about every millisecond, so such a gap means the process was paused, a step blocked (for example on egress back pressure), or the clock jumped forward.

## Live loop (batching)

Poll up to `MAX_BATCH` ingress messages → sequence and append them all → `wait_durable(last)` → apply the whole batch → publish one `EventBatch` per command that has an `origin` or events (a `Tick` that emits nothing publishes nothing; a `Resume` is always published). Batching amortises the durability wait without changing order, and applying everything before publishing anything means a failed publish can never leave memory behind the journal.

## Docker Compose

`docker-compose.yml` (project name `rfq`), with its comments trimmed:

```yaml
name: rfq
services:
  aeron:                        # Java ArchivingMediaDriver (aeron-all 1.53.3): media driver + Archive
    build: docker/aeron
    ipc: shareable
    shm_size: 512m
    volumes: [archive:/data/archive]
    restart: "on-failure:3"
  engine:                       # rfq-node: sequencer + engine, journaling to the Archive
    build: { context: ., dockerfile: docker/rust.Dockerfile, args: { BIN: rfq-node } }
    ipc: "service:aeron"
    init: true                  # an init as PID 1, so SIGTERM stops the node at once
    restart: "on-failure:3"     # a halted node exits with status 1; it replays, at most 3 retries
    environment:
      RFQ_JOURNAL: archive
      RFQ_HASH_EVERY_BATCH: "1" # logs `applied seq=<n> hash=<hex>` after every batch
    depends_on: [aeron]
  demo:                         # rfq-demo: on demand, `docker compose run --rm demo [scenario]`
    build: { context: ., dockerfile: docker/rust.Dockerfile, args: { BIN: rfq-demo } }
    ipc: "service:aeron"
    init: true
    depends_on: [engine]
    profiles: [demo]            # not started by `docker compose up`
volumes:
  archive: {}
```

- All three containers share the driver's `/dev/shm/aeron` through `ipc: "service:aeron"`. Every stream is `aeron:ipc`, so there is no network between them.
- No container runs as root. The driver creates its files in `/dev/shm/aeron` as `0644` owned by its own uid, and a client maps them read-write, so the driver and both Rust images all run as uid 10001. `/data/archive` in the Aeron image is owned by that uid, so a new `archive` volume starts owned by it too.
- `docker/rust.Dockerfile` builds one workspace binary (`BIN`) with `--features aeron` and `--locked` on `rust:1.99.0-bookworm`, and runs it on `debian:bookworm-slim`.
- `RFQ_HASH_EVERY_BATCH` (demo only: it hashes the whole state after every batch) is what lets `scripts/demo.sh` prove recovery: it compares the last `applied seq=<n> hash=<hex>` logged before `SIGKILL` with the `replayed seq=<n> hash=<hex>` the restarted engine logs after replaying the Archive. They must be equal (`RECOVERY OK`).
- The Archive data lives on the `archive` volume, so it survives the engine being killed and restarted. `docker compose --profile demo down -v` deletes it.
- `aeron` and `engine` have `restart: "on-failure:3"`. A halted node exits with status 1 (any port failure) and restarts into a replay. A driver that exits non-zero restarts too; the engine halts on the dead driver and replays once it is back. Checked once, by sending `SIGTERM` to the driver's JVM (exit 143): both containers restarted, and the engine replayed to the same `seq` and state hash it had logged before. A hard JVM crash (`kill -SEGV 1` in the driver container) does not recover by itself: see [architecture §5](../architecture/overview.md). Retries are capped at 3: a journal that fails to replay fails every start, so after the third retry the container stays down, and `docker compose logs engine` (or `aeron`) shows why. Checked with an engine that fails at startup (`RFQ_JOURNAL=bogus`, not a corrupt journal): `RestartCount` stopped at 3 and the container stayed `exited`, where the uncapped policy was still restarting after 30 s. The count belongs to the container, and a healthy run does not reset it (a probe container that ran 11 s between failures still stopped after 3), so three unrelated crashes over weeks use it up too. A manual stop (`docker compose stop` or `kill`) is not restarted, so `scripts/demo.sh`'s `SIGKILL` and explicit `start` are unaffected; the script checks that the engine's `RestartCount` is 0 before the `SIGKILL`.

## Implementation notes (Task 10)

How `crates/rfq-infra/src/aeron/` departs from, or adds to, the spike's findings below. Each point was verified in Docker (`crates/rfq-infra/tests/aeron_live.rs`, and end to end by the `SIGKILL` and replay check in `scripts/demo.sh`).

- **One writer per driver.** The journal assumes it is the only node writing stream 1002 on its media driver and Archive. At startup it stops every recording of the stream that is still active, which would cut off a second live node.
- **Exclusive, session-scoped log recording.** A crashed node's publication stays in the driver until its client times out (about 10 s). It may hold commands it offered but the Archive never recorded: never durable, so never applied or published. With the spike's shared (non-exclusive) publication the next node inherits that session, and its new recording joins at the slowest subscriber's position, so it can capture those bytes as its own. On a later restart they replay as duplicate `seq`s. So the log is written through `async_add_exclusive_publication` (a session of its own) and recorded with `aeron:ipc?session-id=<ours>`; a dead node's later writes find no subscriber and are never recorded. The new recording is identified as the id that was not listed before *and* has our session id. Before the first offer, its start position must equal the publication's position (otherwise `RecordingMisaligned`, fatal).
- **Sealing stale recordings before replay.** If any recording has `stop_position == -1` (left by a crashed node), replay stops it with `try_stop_recording_by_identity` and waits for its final stop position. Then it replays each recording over `[start, stop)`. Replay never reads an active recording's moving position. The `ActiveSubscription` → `stop_recording_channel_and_stream` → retry path on `start_recording` remains as a fallback.
- **`wait_durable` reads the recording position counter** (`RecordingPos::find_counter_id_by_recording_id` + `counters_reader().get_counter_value`), the same value `get_recording_position` reports, guarded by `RecordingPos::is_active` so a freed or reused counter is never trusted. Why not the control call: the Archive pings every control session once a second and closes it when a ping has not fitted in the response stream for `connectTimeoutMs`. The node reads that stream only during a request, so after about nine idle minutes the session was gone and the next `get_recording_position` failed (measured). The control session is therefore used only at startup (replay, starting the recording) and closed right after. Closing it does not stop the recording, and leaving it open would also hold an unpolled, tethered response subscription that can back-pressure the Archive's other clients.
- **Fragment reassembly.** The IPC MTU is 1408 bytes, so any message over about 1.3 KB arrives in several fragments (`BEGIN` flag `0x80`, `END` `0x40`). Ingress, replay and egress consumers join them per session with `rfq_infra::fragment::Reassembler` (fed by `rfq_infra::aeron::frame_of(&header)`). Ingress caps a message at `MAX_INGRESS` while it is still in fragments. The replay loop polls the replay image until `image.position() >= stop`, which also counts padding frames.
- **Egress.** A best-effort notification stream. A batch over the publication's max message length (8 MiB with the default IPC term) is split into chunks with the same `seq` (`transport::split_batch`). With no subscriber connected a batch is dropped, and replay never re-publishes: anything that must happen exactly once reads the journal, not egress ([architecture §7](../architecture/overview.md)). Back pressure for more than 5 s is fatal, so a tethered subscriber that stops polling halts the node, and again after each restart: egress subscribers must be venue-operated.
- **Egress batch fields.** Each `EventBatch` also carries the command's engine `time` (the sequencer's stamp), its `kind` (`Genesis`, `Tick`, `Resume` or `User`), and the `balances` of every account the command changed, after it ([engine.md §7](engine.md)). `Resume` is always published, even when it emits no events, so a subscriber sees every restart and stall; replay at startup still publishes nothing. Balances travel on the last chunk of a split batch, once, so a consumer that joins chunks by `seq` gets them with the final one. When they do not fit beside the last events they travel in a final chunk of their own, with no events. None of this touches the journal: `SequencedCommand` and the codec `VERSION` are unchanged, so existing Archive journals still replay.
- **Ingress after quick repeated crashes.** A new node's ingress subscription joins at the slowest live subscriber's position. That subscriber may be a dead node that the driver has not yet timed out, so commands an intermediate node already polled can be delivered again. What happens to a redelivered command depends on whether it became durable before that node died. A durable command is in the log and its nonce is consumed, so the copy is rejected as `Duplicate` (deterministic, and in the log like any other command). A command that was polled but never made durable is not in the log and its nonce is still free, so the copy is applied, late but exactly once. A client that reuses a nonce (for example to retry after silence) must therefore not assume its last send is the one that took effect: it reconciles through `EventBatch.origin`, which carries the `(actor, nonce)` of every user-command batch (`None` for `Tick` and `Resume`). If it arrives, the first batch for an origin is that nonce's outcome, and a later `Duplicate` batch for the same origin only means it was already sequenced. Egress is best-effort: if the first batch never arrives, read the journal ([architecture §4](../architecture/overview.md)).

## Verified API (Task 0)

Verified on Docker Desktop, `linux/aarch64` (Apple Silicon), by a spike that ran a Rust client and the Java `ArchivingMediaDriver` in two containers sharing `/dev/shm` (`ipc: shareable` + `ipc: "service:aeron"`). The spike recorded stream 1002, read the recorded position, and replayed the recording onto stream 1004. All four gate checks passed, including across repeated runs and a simulated client crash.

These calls live in `crates/rfq-infra/src/aeron/` behind the cargo feature `aeron`.

- **Versions.** `rusteron-archive = "=0.2.11"` (it pulls `rusteron-code-gen =0.2.11`). It bundles Aeron C **1.53.3** (`aeron/version.txt` inside the crate). The Java jar must be `aeron-all` **1.53.3** (`docker/aeron/Dockerfile` sets `AERON_VERSION=1.53.3`; jar sha1 `2ffcf6acba37da20d475e27f2896a1a52cf42de3`, which matches Maven Central; the Dockerfile checks its sha256 `4c6c8a5014a2b67424051170392a005daa9a998ac963302d2819d0f3db749ef2` with `ADD --checksum`). Java runtime image: `eclipse-temurin:21-jre`.
- **Cargo.**
  ```toml
  # crates/rfq-infra/Cargo.toml
  [features]
  aeron = ["dep:rusteron-archive"]
  [dependencies]
  rusteron-archive = { version = "=0.2.11", features = ["static"], optional = true }
  ```
  Default features stay on (`build-from-source`). **Do not use `precompile`:** the `v0.2.11` GitHub release has no prebuilt artifacts, so the download 404s.
- **Build environment (Debian bookworm).** apt: `clang libclang-dev default-jdk-headless uuid-dev libbsd-dev pkg-config`. `cmake` must be at least 3.30 (Aeron 1.53 requires it, and bookworm ships 3.25), so install it from `bookworm-backports` (3.31.6). Run `rustup component add rustfmt` (the build script formats generated bindings and the `rust:1-bookworm` toolchain has no rustfmt). The build script runs Gradle (`./gradlew`) to produce Aeron jars, so the build needs network access to `services.gradle.org` and Maven Central. The `cargo build --release` layer for `rusteron-archive` took about 70 s. Runtime image needs `libbsd0` and `libmd0` (the binary links them dynamically). Cache the heavy dependency layer by building a dummy `main.rs` first.
- **Context and connect.**
  ```rust
  let ctx = AeronContext::new()?;
  ctx.set_dir(&"/dev/shm/aeron".into_c_string())?;
  ctx.set_error_handler(Some(|code: i32, msg: &str| tracing::error!(code, msg)))?; // the C default handler exits the process
  let aeron = Aeron::new(&ctx)?;
  aeron.start()?;
  ```
  Wrap this in a retry loop (250 ms backoff, 30 s cap) so it tolerates a driver that is still starting.
- **Archive client (local IPC control).** `AeronArchiveContext::new_with_no_credentials_supplier` does not exist in 0.2.11.
  ```rust
  let actx = AeronArchiveContext::new()?;
  actx.set_aeron(&aeron)?;
  actx.set_control_request_channel(&"aeron:ipc".into_c_string())?;   // local control, stream 10 (default)
  actx.set_control_response_channel(&"aeron:ipc".into_c_string())?;  // stream 20 (default)
  let archive = AeronArchiveAsyncConnect::new_with_aeron(&actx, &aeron)?.poll_blocking(Duration::from_secs(5))?; // -> AeronArchive
  ```
  Do not use `aeron:udp?endpoint=localhost:8010` from the engine container. Containers share IPC but not the network, so `localhost` UDP never reaches the archive. The Java side still listens on UDP 8010, which is harmless.
- **Publish.** `aeron.async_add_publication(&ch, 1002)?.poll_blocking(d)?` returns `AeronPublication`. `publication.offer(&[u8]) -> Result<i64, AeronOfferError>`, where `Ok` is the new stream position (192 after three 16-byte messages: 64 bytes each with framing). `e.is_retryable()` is true for `NotConnected`, `BackPressured` and `AdminAction`. `Closed`, `MaxPositionExceeded` and `Error(_)` are fatal. `publication.session_id() -> i32`. There are no `AERON_PUBLICATION_*` constants or reserved-value supplier arguments.
- **Subscribe and poll.** `aeron.async_add_subscription(&ch, stream, Handlers::NONE, Handlers::NONE)?.poll_blocking(d)?` returns `AeronSubscription`. `sub.poll_fn(|msg: &[u8], h: AeronHeader| { ... }, fragment_limit: usize) -> Result<i32, AeronCError>` returns the fragment count. `h.position() -> i64` is the stream position after the fragment. A panic inside the closure aborts the process, so return early instead.
- **Recording.**
  - `archive.start_recording(&ch, 1002, SOURCE_LOCATION_LOCAL, true) -> Result<i64, AeronArchiveError>` (the subscription id). It fails with code `AeronArchiveErrorCode::ActiveSubscription` if the channel and stream are already being recorded.
  - `archive.list_recordings_for_uri_fn(&mut count, from_id, i32::MAX, &ch, 1002, |d: AeronArchiveRecordingDescriptor| ...)` calls the closure for each recording; use `d.recording_id()`, `d.start_position()`, `d.stop_position()` (`-1` while active) and `d.session_id()`. The non-`_fn` variant takes `Option<&Handler<_>>`, so prefer `_fn`.
  - `archive.get_recording_position(id) -> Result<i64, AeronArchiveError>`. This is the `wait_durable` primitive: poll until it is `>=` the position returned by `offer`. (Task 10 uses the counter below instead: an idle control session is dropped by the Archive. See [Implementation notes](#implementation-notes-task-10).)
  - `archive.get_stop_position(id)` returns `-1` while the recording is active.
  - Alternative that avoids a control round trip: `RecordingPos::find_counter_id_by_recording_id(&aeron.counters_reader(), id)` followed by `counters.get_counter_value(counter_id)`. It read 192 for a 192-byte publish.
  - "Durable" means what the recorded position promises. With the default `aeron.archive.file.sync.level=0` that is the page cache: it survives a process crash, not an OS crash or power loss. `docker/aeron/Dockerfile` therefore sets `-Daeron.archive.file.sync.level=2` and `-Daeron.archive.catalog.file.sync.level=2`: `RecordingWriter.onBlock` forces the segment file (data and metadata) before `RecordingSession` advances the position counter, so `wait_durable` returns only for fsynced bytes (Aeron 1.53.3 sources). The cost is an fsync per recorded block on the write-ahead path, amortised over a batch of up to `MAX_BATCH` commands.
- **Replay.**
  ```rust
  let params = AeronArchiveReplayParams::builder().position(start).length(end - start).build()?;
  let replay_session = archive.start_replay(rec_id, &ch, 1004, &params)?;           // -> i64
  let rch = ChannelUri::add_session_id("aeron:ipc", replay_session as i32).into_c_string();
  let sub = aeron.async_add_subscription(&rch, 1004, Handlers::NONE, Handlers::NONE)?.poll_blocking(d)?;
  // poll_fn until h.position() >= end
  ```
  Replayed fragments carry the original stream positions (the last header position equalled the published position), so `consumed = h.position()` is a valid completion test. Subscribe with the session-id channel so earlier replays can't mix in. Drop the subscription after each recording.
- **Behaviour the journal must handle.**
  1. Each node start creates a new recording. Replaying every recording of stream 1002 in id order, each over `[start_position, stop_position)`, returned all messages in order (10 recordings, 30 messages in the longest run).
  2. A recording's start position is not always 0 (see crash restart below). Always use the descriptor's `start_position()`; never hard-code `0`.
  3. After a crash, the old publication's recording stays active (`stop_position == -1`) until the driver times the dead client out, about 10 s. An immediate restart fails with `ActiveSubscription` ("recording exists for streamId=1002 channel=aeron:ipc"). Two verified ways out:
     - Retry `start_recording` every 500 ms. It succeeded after 10.5 s, on a new session starting at position 0.
     - On `ActiveSubscription`, call `archive.stop_recording_channel_and_stream(&ch, 1002)?` and retry. This succeeded immediately. The new `async_add_publication` shares the crashed publication's session (same session id, position continues, for example 192 to 384). The old recording ends at 192 and the new one starts at 192, so the log stays contiguous. Find the new recording as "the id that was not in the list before `start_recording`", not by session id, because both recordings share it. Recommended, because a 10 s restart stall is bad for a demo. (Task 10: that shared session is what lets a new recording capture a dead node's unrecorded tail, so the journal uses an exclusive publication and a session-scoped recording instead. See [Implementation notes](#implementation-notes-task-10).)
  4. When a publication closes cleanly, its recording gets a stop position within under a second.
- **Docker.** The compose file and Java image from the spike worked unchanged apart from `AERON_VERSION`. Java flags used: the ones in `docker/aeron/Dockerfile` (IPC dir `/dev/shm/aeron`, archive dir `/data/archive`, UDP control `localhost:8010`, recording events disabled, and the two Archive sync levels, both 2, that make "durable" mean fsynced). Docker Desktop `shm_size: 512m` was enough. `depends_on` only waits for container start, so keep the client retry loops.
- **Corrections this spike made elsewhere (applied).** The log stream is 1002 (stream 20 is the Archive's control response stream). Task 10 follows the calls above, not the plan's first guesses: no `precompile` feature, `aeron:ipc` control channels, the `_fn` closure variants, replay of each recording from its own `start_position()`. Replay later changed again: it stops active recordings first instead of reading an active one (see [Implementation notes](#implementation-notes-task-10)).

### Resolved open items

- `rusteron` bundles Aeron C 1.53.3, so the jar is `aeron-all` 1.53.3.
- `rusteron-archive` exposes the recording position both as a control call (`get_recording_position`) and as a shared-memory counter (`RecordingPos` + `counters_reader`).
- IPC across containers through `ipc: service:` works on Docker Desktop for Apple Silicon.
