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
- Publishes the `SequencedCommand` to stream 20, retrying on back pressure. On `NOT_CONNECTED`/`CLOSED` it is fatal and the node exits; restarting resumes cleanly.

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
