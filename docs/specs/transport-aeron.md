# Transport Spec: Aeron + Archive

## Streams (all `aeron:ipc`, shared `/dev/shm/aeron`)

| Stream | Id | Writer → Reader | Payload |
|---|---|---|---|
| Ingress | 1001 | clients → sequencer | `ClientCommand{actor, nonce, action}` |
| Log | 1002 | sequencer → engine (and recorded by Archive) | `SequencedCommand` |
| Egress | 1003 | engine → clients | `EventBatch{seq, origin, events}` |
| Replay | 1004 | Archive → engine at startup | `SequencedCommand` |

Stream ids start at 1001 because Aeron Archive's local control channel already uses `aeron:ipc` stream 10.

Encoding: `serde` + `bincode` (fixed-int, little-endian, 64 KiB size limit), with a `u8` version prefix. A message that fails to decode on ingress is dropped and counted; it never reaches the log. Ingress decodes to `ClientCommand`, which cannot express `Genesis` or `Tick`. Only the node itself creates those.

## Sequencer

- Polls ingress. For each command it sets `seq = last_seq + 1` and `time = max(last_time, wall_clock_ms)`, so time never goes backwards.
- When there's no client traffic and the engine's earliest timer (`Engine::next_timer()`) is due, it emits a `Tick`. Idle periods add nothing to the log.
- Publishes the `SequencedCommand` to stream 1002, retrying on back pressure. On `NOT_CONNECTED`/`CLOSED` it is fatal and the node exits; restarting resumes cleanly.

## Journal (the `Journal` trait)

```rust
trait Journal {
    fn append(&mut self, sc: &SequencedCommand) -> Result<LogPosition>;
    fn wait_durable(&mut self, pos: LogPosition) -> Result<()>;     // write-ahead gate
    fn replay(&mut self, from_seq: u64, f: &mut dyn FnMut(SequencedCommand)) -> Result<u64>;
}
```

- **`ArchiveJournal`:** each node start calls `start_recording(aeron:ipc, 1002, LOCAL)`, which creates a new recording. Together, all the recordings of stream 1002 in recording-id order make up the log. `wait_durable` polls `get_recording_position(current)` until it reaches the publication position returned by `offer`. `replay` lists every recording for stream 1002 and replays each one in order onto stream 1004.
- **`FileJournal` (fallback, and for local runs without Docker):** an append-only file of length-prefixed records with a CRC32 on each. `wait_durable` = `fsync`.
- **`MemJournal`:** used by tests.

## Startup

1. Connect to the media driver and the archive.
2. If no recording exists, append `Genesis` as seq 1.
3. Replay everything through `Engine::apply` with egress **suppressed**. Record `last_seq` and `last_time`.
4. Log `state_hash`, then enter the live loop at `last_seq + 1`.

## Live loop (batching)

Poll up to N ingress messages → sequence and append them all → `wait_durable(last)` → apply each one → publish one `EventBatch` per command. Batching amortises the durability wait without changing order.

## Docker Compose

```yaml
services:
  aeron:   # Java ArchivingMediaDriver, pinned aeron-all version
    build: docker/aeron
    ipc: shareable
    shm_size: 512m
    volumes: [archive:/data/archive]
  engine:
    build: { context: ., dockerfile: docker/rust.Dockerfile, args: { BIN: rfq-node } }
    ipc: "service:aeron"
    depends_on: [aeron]
  demo:
    build: { context: ., dockerfile: docker/rust.Dockerfile, args: { BIN: rfq-demo } }
    ipc: "service:aeron"
    depends_on: [engine]
volumes: { archive: {} }
```

## Open items (resolved by the Task 0 spike)

- Which Aeron version `rusteron` bundles, so the `aeron-all` jar can be pinned to match.
- Whether `rusteron-archive` exposes a recording-position counter for `wait_durable`. If it doesn't, poll `get_recording_position` over the control session.
- Whether IPC across containers through `ipc: service:` works on Docker Desktop for Apple Silicon.

## Verified API (Task 0)

Verified on Docker Desktop, `linux/aarch64` (Apple Silicon), by a spike that ran a Rust client and the Java `ArchivingMediaDriver` in two containers sharing `/dev/shm` (`ipc: shareable` + `ipc: "service:aeron"`). The spike recorded stream 1002, read the recorded position, and replayed the recording onto stream 1004. All four gate checks passed, including across repeated runs and a simulated client crash.

These calls live in `crates/rfq-infra/src/aeron/` behind the cargo feature `aeron`.

- **Versions.** `rusteron-archive = "=0.2.11"` (it pulls `rusteron-code-gen =0.2.11`). It bundles Aeron C **1.53.3** (`aeron/version.txt` inside the crate). The Java jar must be `aeron-all` **1.53.3** (`docker/aeron/Dockerfile` sets `AERON_VERSION=1.53.3`; jar sha1 `2ffcf6acba37da20d475e27f2896a1a52cf42de3`, which matches Maven Central). Java runtime image: `eclipse-temurin:21-jre`.
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
  - `archive.get_recording_position(id) -> Result<i64, AeronArchiveError>`. This is the `wait_durable` primitive: poll until it is `>=` the position returned by `offer`.
  - `archive.get_stop_position(id)` returns `-1` while the recording is active.
  - Alternative that avoids a control round trip: `RecordingPos::find_counter_id_by_recording_id(&aeron.counters_reader(), id)` followed by `counters.get_counter_value(counter_id)`. It read 192 for a 192-byte publish.
  - "Durable" means written by the Archive process to its segment files. The spike did not test an OS crash. `-Daeron.archive.file.sync.level` (not set in our Dockerfile) controls `fsync`.
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
     - On `ActiveSubscription`, call `archive.stop_recording_channel_and_stream(&ch, 1002)?` and retry. This succeeded immediately. The new `async_add_publication` shares the crashed publication's session (same session id, position continues, for example 192 to 384). The old recording ends at 192 and the new one starts at 192, so the log stays contiguous. Find the new recording as "the id that was not in the list before `start_recording`", not by session id, because both recordings share it. Recommended, because a 10 s restart stall is bad for a demo.
  4. When a publication closes cleanly, its recording gets a stop position within under a second.
- **Docker.** The compose file and Java image from the spike worked unchanged apart from `AERON_VERSION`. Java flags used: the ones in `docker/aeron/Dockerfile` (IPC dir `/dev/shm/aeron`, archive dir `/data/archive`, UDP control `localhost:8010`, recording events disabled). Docker Desktop `shm_size: 512m` was enough. `depends_on` only waits for container start, so keep the client retry loops.
- **Corrections this spike makes elsewhere.**
  - The Sequencer section says the log is published to "stream 20". It must be 1002 (stream 20 is Archive's control response stream).
  - The Task 10 plan code must change in these ways: drop `"precompile"` from the `rusteron-archive` features; use `aeron:ipc` control channels instead of the UDP endpoints; use the calls above in place of `Handlers::no_*_handler()`, `poll_once`, `offer(bytes, handler)` returning `i64`, `AeronArchiveReplayParams::new(0, ...)`, and the `list_recordings_for_uri` closure form; replay each recording from its `start_position()`; skip a recording only when `stop <= start`, and use `get_recording_position` when it is still active (`-1`).

### Resolved open items

- `rusteron` bundles Aeron C 1.53.3, so the jar is `aeron-all` 1.53.3.
- `rusteron-archive` exposes the recording position both as a control call (`get_recording_position`) and as a shared-memory counter (`RecordingPos` + `counters_reader`).
- IPC across containers through `ipc: service:` works on Docker Desktop for Apple Silicon.
