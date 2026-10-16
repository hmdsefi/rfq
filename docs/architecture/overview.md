# Architecture Overview

## 0. Project structure and layers

The project uses a flat `crates/` workspace with a virtual root manifest, as described in [matklad, "Large Rust Workspaces"](https://matklad.github.io/2021/08/22/large-rust-workspaces.html). Code is split into hexagonal (ports-and-adapters) layers, and **dependencies point inward only**. Crate boundaries enforce that, not convention.

```mermaid
flowchart LR
  GATEWAY[rfq-gateway<br/>lib: protocol, view filter, nonce book, hub, HTTP read and write rules<br/>bin: Aeron, WebSocket, HTTP, replica thread] --> INFRA
  GATEWAY --> READ
  NODE[rfq-node<br/>bin: composition root] --> INFRA
  DEMO[rfq-demo<br/>bin: scenario client] --> INFRA
  READ[rfq-read<br/>pure: Replica, trace, read views] --> APP
  INFRA[rfq-infra<br/>adapters: codec, file/mem journal,<br/>Aeron transport + Archive journal + journal follower] --> APP
  APP[rfq-app<br/>use case: sequence → persist → apply → publish<br/>ports: Journal, Transport<br/>protocol: ClientCommand, EventBatch] --> DOMAIN
  DOMAIN[rfq-domain<br/>pure: types, ledger, model,<br/>commands, events, Engine state machine]
  TK[rfq-testkit<br/>Harness, accounts, scenarios] --> DOMAIN
```

| Layer | Crate | Contains | Must not depend on |
|---|---|---|---|
| Domain | `rfq-domain` | Value objects (`Amount`, `Price`, ids), `Ledger`, entities (`Request`, `Quote`, `Contract`, `Position`), `Command`/`Event`, `Engine` (the deterministic state machine and its invariants) | Any I/O, async, clock, transport or serialization *format*. `serde` derives only; `bincode` is used solely for `state_hash` |
| Application | `rfq-app` | **Ports** (`Journal`, `Transport` traits), **protocol DTOs** (`ClientCommand`, `EventBatch`), `batch_of` (the rule for which command makes which batch, shared by the node and the replica), `Sequencer`, `Node` service (write-ahead loop), `NodeError` (`thiserror`) | Concrete adapters, bincode, Aeron |
| Read side | `rfq-read` | `Replica`: a read-only `Engine` plus the **trace** (every `EventBatch` it regenerates with `batch_of`, indexed by `seq`, with the command behind it), and the read **views** (positions, requests, quotes, contracts). It is fed `SequencedCommand`s and does no I/O | Any I/O, Aeron, a clock, `rfq-infra` (tests excepted) |
| Infrastructure | `rfq-infra` | **Adapters**: `codec` (versioned bincode), `FileJournal`, `MemJournal`, `MemTransport`, and (feature `aeron`) `AeronTransport`, `ArchiveJournal` and `JournalFollower` (the read side's input). Also the Aeron-path pieces that need no Aeron and so are always built and tested: fragment `Reassembler`, egress `split_batch` | Binaries |
| Interface / composition | `rfq-node`, `rfq-demo`, `rfq-gateway` | `main`: config from env, wire adapters, run. `anyhow` lives only here. `rfq-gateway` also has a pure library (protocol, per-viewer filter, nonce book, hub, the HTTP read and write rules) | — |
| Test support | `rfq-testkit` | `Harness`, well-known accounts, scripted scenarios and `demo_config()`. A dev-dependency of the domain and infra tests; `rfq-node` and `rfq-demo` depend on it normally, for the demo genesis and the scenarios | — |

Rules:
- **Ports carry typed values, not bytes.** `Transport` yields `ClientCommand` and accepts `EventBatch`; decoding and malformed-message handling belong to the adapter. Because the application can't even express a client-sent `Tick` or `Genesis`, injecting one is impossible by construction.
- **Errors:** library crates use `thiserror` enums. Ports return `Result<_, BoxError>`, where `BoxError = Box<dyn Error + Send + Sync>`, so the application isn't generic over adapter error types. Binaries use `anyhow`.
- **The risky dependency is isolated:** `rusteron` is behind `rfq-infra`'s `aeron` feature, so `cargo test` for domain/app/infra never builds Aeron.
- **Workspace hygiene:** `[workspace.package]` (edition 2024, version, `rust-version`, license), `[workspace.dependencies]`, `rust-toolchain.toml`, `rustfmt.toml`, `clippy.toml`, `deny.toml`, `.github/workflows/ci.yml`, `LICENSE` (MPL-2.0). Every crate is `publish = false`.

## 1. Shape

One **deterministic engine** consumes a **totally-ordered command log** and produces **events**. Aeron moves the bytes; Aeron Archive records the log; replaying the log rebuilds state exactly. A second, read-only copy of the engine replays the same log to serve reads (the **read side**, below).

```mermaid
flowchart TB
  subgraph CL[Clients]
    R[Requesters]
    M[Market makers]
    O[Oracle / arbiter]
    OP[Operator / mock chain]
  end
  subgraph DRV["Aeron media driver + Archive (Java container, shared /dev/shm)"]
    ING[["ingress 1001<br/>ClientCommand"]]
    EGR[["egress 1003<br/>EventBatch"]]
    LOG[["log 1002<br/>exclusive per-node session"]]
    ARC[("Archive<br/>fsync level 2")]
    RPL[["replay 1004"]]
  end
  subgraph NODE["rfq-node process (composition root)"]
    subgraph INFRA["rfq-infra · adapters"]
      AT["AeronTransport<br/>Reassembler · decode_ingress (≤ 64 KiB) · split_batch"]
      AJ["ArchiveJournal<br/>append · wait_durable (RecordingPos) · replay"]
    end
    subgraph APP["rfq-app · ports + use case"]
      N["Node<br/>poll → stamp → append → wait_durable → apply → publish<br/>Resume on restart or stall · Tick when a timer is due · Halted latch"]
      S["Sequencer<br/>seq = last + 1 · time = max(last, wall)"]
    end
    subgraph DOM["rfq-domain · pure"]
      E["Engine.apply(seq, time, cmd)<br/>Ledger · state machines · timers · invariants I1–I5"]
    end
  end
  subgraph GWP["rfq-gateway process (client edge)"]
    GW["Hub (pure): filter per view · nonce book · engine status<br/>WebSocket server :18080 · Aeron adapter<br/>HTTP server :18081 · read-only Replica (rfq-read)<br/>fed by a JournalFollower"]
  end
  R & M & O & OP <-->|"WebSocket JSON,<br/>or HTTP JSON"| GW
  GW -->|"ClientCommand (publish)"| ING
  EGR -->|"EventBatch (best-effort)"| GW
  ING --> AT
  AT -->|"Transport::poll → ClientCommand"| N
  N --- S
  N -->|"Journal::append / wait_durable"| AJ
  AJ -->|offer| LOG
  LOG -->|recorded| ARC
  ARC -. "recording position ≥ offer position" .-> AJ
  N -->|"apply (whole batch)"| E
  N -->|"Transport::publish(EventBatch)"| AT
  AT --> EGR
  ARC -. "startup: every recording, in id order" .-> RPL
  RPL -.-> AJ
  ARC -. "gateway's follower: replay on 1005,<br/>each recording from its start, kept open" .-> GW
```

Clients reach the venue through the gateway. Its write path holds no business state: it sends each command with the actor's next nonce and filters every egress batch for the connection's view ([gateway.md](../specs/gateway.md)). `rfq-demo` is a venue-internal client and talks to Aeron directly.

**The read side** ([read-api.md](../specs/read-api.md)) answers what the push-only egress cannot: what are my positions, what happened at `seq` N, what did I miss while disconnected. The engine never serves a read. Instead the gateway process runs a second copy of it:
- `JournalFollower` (in `rfq-infra`) replays the log from the Archive onto stream 1005 with length `INT64_MAX`, so the replay keeps following the active recording as it grows; after an engine restart it moves on to the next recording ([transport-aeron.md](../specs/transport-aeron.md#journal-the-journal-trait)).
- `Replica` (in `rfq-read`) applies each command with `Engine::apply` and `rfq_app::batch_of`, the node's own code, so at the same `seq` its `state_hash` equals the node's and every batch the node published is identical in its trace. The trace also holds the batches the node applied but never published: `Genesis`, and what a restarted node replayed.
- The HTTP server (port 18081, `std::net` and `httparse`) answers `GET`s from the replica under a short read lock; the follower thread takes the write lock per command. A `POST` opens a short-lived connection on the hub, as a WebSocket client would, so an HTTP command gets the same nonce book, back-pressure and `outcome unknown` handling and the hub has no HTTP code.

```mermaid
sequenceDiagram
  participant C as Client (HTTP)
  participant G as Gateway (hub, HTTP server)
  participant N as rfq-node
  participant A as Archive (log 1002)
  participant F as Follower and replica
  C->>G: POST /v1/commands
  G->>N: ClientCommand on ingress 1001
  N->>A: append, then wait until it is fsynced
  Note over N: apply, publish
  N-->>G: EventBatch on egress 1003
  G-->>C: 200 {seq: n, events, balances}
  A-->>F: the same command, replayed on 1005
  Note over F: apply, regenerate the batch, add it to the trace
  C->>G: GET /v1/accounts/10/positions?min_seq=n
  G->>F: wait up to 2 s until as_of_seq ≥ n
  F-->>G: rows as of seq ≥ n
  G-->>C: 200 {as_of_seq, positions}
```

The replica lags the node by the time the Archive takes to record and replay a command, usually milliseconds, so `min_seq` is how a client reads its own write. It sees a command only once the Archive has recorded it (fsync level 2), so it **never shows a command that was not durable**. On a follower or Archive error it is marked `unavailable` and keeps serving what it has while a fresh replica rebuilds on the side and takes over once it has caught up ([failure-modes.md](../specs/failure-modes.md) F16).

Ports (`Journal`, `Transport`) are typed traits in `rfq-app`; `rfq-infra` implements them (Aeron behind the `aeron` feature, plus `FileJournal`, `MemJournal`, `MemTransport` for tests and the fallback). The engine never sees bytes, Aeron or time other than what the `SequencedCommand` carries.

| Component | Where | Responsibility | Depends on |
|---|---|---|---|
| **Engine** | `rfq-domain::Engine` | Pure state machine: ledger, requests, quotes, contracts, timers. No I/O, no clock. | nothing |
| **Sequencer** | `rfq-app::Sequencer` | Stamps each command with a consecutive `seq` and a monotonic `time` (wall clock, clamped, resumed from the journal). Pure, no I/O. | engine types |
| **Node runtime** | `rfq-app::Node` | Startup replay, then the live loop: poll → sequence → append → wait durable → apply → publish. Sequences `Resume` after a restart or a poll gap over `STALL_GAP_MS` (1 s), and `Tick` when idle with a due timer. Latches `NodeError::Halted` on any port failure. | the ports |
| **Ports** | `rfq-app::{Journal, Transport}` | Traits over the durable log and over client ingress / event egress, carrying typed values. | engine types |
| **Codec** | `rfq-infra::codec` | Versioned binary encoding of commands, events and journal records (`serde` + `bincode`). Size limit only at untrusted ingress. | engine and protocol types |
| **Journal adapters** | `rfq-infra::journal`, `rfq-infra::aeron` | `ArchiveJournal` = Aeron Archive recordings of stream 1002; `FileJournal`; `MemJournal` | Aeron Archive client (`aeron` feature) |
| **Transport adapters** | `rfq-infra::transport`, `rfq-infra::aeron` | `AeronTransport` = ingress 1001, egress 1003; `MemTransport` | Aeron client (`aeron` feature) |
| **Composition root** | `rfq-node` | Reads `AERON_DIR`, `RFQ_JOURNAL`, `RFQ_HASH_EVERY_BATCH`; wires an Aeron transport and a journal into `Node`; runs the loop. | all of the above |
| **Journal follower** | `rfq-infra::aeron::follower` | Replays the log (stream 1002) from the Archive onto stream 1005 and keeps following it: recording after recording, polling the Archive control session, with `caught_up()` for when it has delivered everything recorded. Only reads the Archive. | Aeron Archive client (`aeron` feature) |
| **Replica** | `rfq-read::Replica` | A read-only `Engine`, the trace of every regenerated `EventBatch` by `seq`, and the read views. Pure, no I/O. | engine, `batch_of` |
| **Gateway** | `rfq-gateway` | The client edge: JSON over WebSocket in, `ClientCommand` onto ingress; every `EventBatch` back, filtered per view, with a `result` per command and an engine `status`. No business state on the write path. | codec, Aeron client (binary) |
| **HTTP server and replica thread** | `rfq-gateway` (binary) | `GET` from the replica, `POST` through the hub (limits, statuses and `Origin` rule in [read-api.md](../specs/read-api.md)). The replica thread owns its own Aeron client and the follower, and rebuilds the replica after a fault. | `rfq-read`, follower, `httparse` |
| **Demo client** | `rfq-demo` | Runs the scripted scenarios over Aeron as an ordinary client and checks the events it gets back. | codec, Aeron client, testkit |

## 2. The engine boundary (the one interface that matters)

```rust
pub struct SequencedCommand { pub seq: u64, pub time: Timestamp, pub cmd: Command }

pub enum Command { Genesis(Genesis), Tick, Resume, User { actor: AccountId, nonce: u64, action: Action } }

impl Engine {
    pub fn new() -> Self;                                         // empty; first command must be Genesis
    pub fn apply(&mut self, sc: &SequencedCommand) -> Vec<Emitted>; // never panics on bad input
    pub fn next_timer(&self) -> Option<Timestamp>;                 // sequencer ticks only when due
    pub fn state_hash(&self) -> [u8; 32];                          // replay verification
    pub fn check_invariants(&self) -> Result<(), String>;          // tests + debug builds
}
```

Everything outside this line (Aeron, Docker, Archive, Cluster later) is plumbing. Tests drive `apply` directly.

## 3. Determinism rules

1. Engine never reads a clock; `time` comes from the `SequencedCommand`.
2. Before applying any command, the engine fires all timers with `deadline <= time` (in `(deadline, key)` order). The one exception is `Resume`, which acts first so that downtime can't eat a dispute window ([engine.md §6.3](../specs/engine.md)).
3. No randomness. IDs are derived from `seq` (`RequestId(seq)`, `QuoteId(seq)`).
4. `BTreeMap`/`BTreeSet` for all iterated state.
5. Genesis config (roles, limits) is the first log entry, so replay uses identical config.

## 4. Live loop and durability

One iteration of `Node::poll` (a batch of up to `MAX_BATCH` = 256 client commands):

```mermaid
sequenceDiagram
  participant C as Clients
  participant T as AeronTransport (1001 in / 1003 out)
  participant N as Node (rfq-app)
  participant J as ArchiveJournal (log 1002)
  participant A as Archive
  participant E as Engine (rfq-domain)
  C->>T: ClientCommand bytes (possibly fragmented)
  N->>T: poll(256)
  T-->>N: ClientCommands (reassembled, malformed or > 64 KiB dropped)
  Note over N: prepend Resume after a restart or a poll gap > 1 s,<br/>add Tick only if idle and a timer is due
  N->>N: Sequencer stamps seq n..n+k, time = max(last, wall)
  N->>J: append(SequencedCommand) for each
  J->>A: exclusive-publication offer → recorded
  N->>J: wait_durable(last position)
  A-->>J: RecordingPos ≥ position (segment fsynced)
  J-->>N: durable
  N->>E: apply(seq, time, cmd) for the whole batch
  E-->>N: Vec<Emitted> per command
  N->>T: publish(EventBatch{seq, time, kind, origin, events, balances}) per command
  T-->>C: chunks via split_batch (dropped if no subscriber)
  Note over N,J: any port Err ⇒ NodeError, Halted latch, process exits 1 ⇒ Compose restart ⇒ replay
```

Startup and recovery (`Node::start`):

```mermaid
sequenceDiagram
  participant N as Node::start
  participant J as ArchiveJournal
  participant A as Archive
  participant E as Engine
  J->>A: stop any recording of 1002 still active (a crashed node's)
  J->>A: list recordings of 1002, replay each [start, stop) on 1004 in id order
  A-->>J: SequencedCommands (fragments reassembled)
  J-->>N: replay callback
  N->>N: require seq == last + 1 (else SequenceError: refuse to start)
  N->>E: apply, with egress suppressed
  Note over N: empty journal ⇒ append + fsync Genesis as seq 1
  N->>J: first append opens an exclusive publication,<br/>records only our session-id, checks the start position
  Note over N: first poll sequences Resume (extends open dispute windows)
```

**Write-ahead rule:** nothing is applied or published before its command is durably recorded. Durable means fsynced: the Archive runs with `aeron.archive.file.sync.level=2` (and the same for its catalog), so the recording position `wait_durable` reads advances only after the segment file is forced, and `FileJournal` calls `sync_data`. That puts an fsync on the write-ahead path; a batch (up to `MAX_BATCH` client commands) shares one durability wait, and the whole batch is applied before any of it is published. If the engine crashes:

| Crash point | Effect | Recovery |
|---|---|---|
| Before log publish | Command never happened. | Client retries with same nonce. |
| After record, before apply | Command is in the log. | Replay applies it. |
| After apply, before egress | Memory is lost; the journal is not. | Replay re-derives the state with egress **suppressed**, so that batch is never published: egress is a best-effort notification stream. The command and its effects are in the journal, and anything that must happen exactly once reads them there (§7). A client whose batch was lost can read it back from the trace (`GET /v1/trace`, [read-api.md §6](../specs/read-api.md)): the replica regenerates the batch of every journaled command, whether or not the node managed to publish it. |

## 5. Deployment (Docker Compose)

```mermaid
flowchart LR
  subgraph PROJ["docker compose project · every container runs as uid 10001"]
    subgraph SHM["shared IPC namespace /dev/shm/aeron (ipc: shareable / service:aeron)"]
      AE["aeron<br/>eclipse-temurin 21 JRE + aeron-all 1.53.3 (sha256-checked)<br/>ArchivingMediaDriver, fsync on · restart on failure (capped)"]
      EN["engine<br/>rfq-node --features aeron · init: true<br/>restart on failure (capped)"]
      GWS["gateway<br/>rfq-gateway --features aeron · init: true<br/>WebSocket :18080 · HTTP :18081 · replica thread<br/>restart on failure (capped)"]
      DE["demo (profile: demo)<br/>rfq-demo: 8 scenarios"]
    end
    VOL[("named volume: archive<br/>/data/archive")]
  end
  AE --- VOL
  EN <-->|"aeron:ipc 1001–1004"| AE
  GWS <-->|"aeron:ipc 1001 / 1003,<br/>1005 (follow the log)"| AE
  CLI["clients on the host<br/>dashboard page, curl, scripts"] <-->|"WebSocket 127.0.0.1:18080<br/>HTTP 127.0.0.1:18081<br/>(loopback only)"| GWS
  DE <-->|"aeron:ipc 1001 / 1003"| AE
  SCRIPT["scripts/demo.sh<br/>build → down -v → up → demo → SIGKILL engine → start → compare seq + hash"] -.-> PROJ
```

- `aeron`: `eclipse-temurin:21-jre` and the `aeron-all` 1.53.3 jar, checked against its sha256; runs the media driver and the Archive with fsync on (`file.sync.level=2`); `ipc: shareable`, `shm_size: 512m`.
- `engine`, `gateway`, `demo`: Rust images from `docker/rust.Dockerfile` (`--features aeron`, `--locked`); `ipc: "service:aeron"` so all share `/dev/shm/aeron`; `init: true`. Every container runs as the same non-root uid 10001, because the driver's files in `/dev/shm/aeron` are `0644` and owned by it. All channels are `aeron:ipc`. The `demo` service sits in compose profile `demo` and runs on demand (`docker compose run --rm demo [scenario]`).
- `gateway`: the client edge, WebSocket and HTTP API. It listens on `0.0.0.0:18080` (WebSocket) and `0.0.0.0:18081` (HTTP, `RFQ_HTTP_ADDR`) inside the container and is published on the host as `127.0.0.1:${RFQ_GATEWAY_PORT:-18080}` and `127.0.0.1:${RFQ_HTTP_PORT:-18081}`, loopback only ([gateway.md §7](../specs/gateway.md), [read-api.md §9](../specs/read-api.md)). It may start before the engine: its clients see `status: down` until the engine is there, and if the engine's last recording has already stopped (it has been down for about 10 s, the driver's client timeout), `GET /v1/status` shows the replica `catching_up` until the engine writes, because the follower counts as caught up only while it follows a recording that is still being written. A dead engine is not fatal for it; a dead media driver, or a dead Aeron client in the replica thread, is (it exits 1). A follower error is not: the replica goes `unavailable`, keeps serving what it has, and is rebuilt ([failure-modes.md](../specs/failure-modes.md) F16).
- `aeron`, `engine` and `gateway` have `restart: "on-failure:3"`: a halted node exits with status 1 and comes back to replay ([transport-aeron.md](../specs/transport-aeron.md#docker-compose)). Retries are capped at 3: after that the container stays down, and `docker compose logs engine` shows why. A manual stop, such as the `SIGKILL` in `scripts/demo.sh` or `docker compose kill`, is not restarted.
- **A dead driver heals only when it exits cleanly.** A `SIGTERM` to its JVM (exit 143) is restarted, and the engine and gateway follow within a second and replay to the same hash. A hard JVM crash (`kill -SEGV 1` in the container) is not: the three restarts come within a second and each is refused with `active mark file detected` (the Archive's mark file stays active for about 10 s), and the engine and gateway, which exit 1 after the driver's 10 s keep-alive timeout, cannot rejoin the dead driver's IPC namespace. Recover with `docker compose up -d aeron engine gateway`; the engine replays its journal.
- Archive data is on the named volume `archive`, so it survives the engine being killed and restarted. `./scripts/demo.sh` does exactly that: it runs the eight scenarios, `SIGKILL`s the engine, restarts it, and checks that the replayed state hash equals the last hash the engine logged before the kill (`RECOVERY OK`).

## 6. Production path (not built, except the read replicas)

```mermaid
flowchart LR
  GW[Gateways<br/>auth, validation, rate limit, audience filter] --> LDR[Cluster leader]
  LDR -- Raft log --> F1[Follower]
  LDR -- Raft log --> F2[Follower]
  LDR & F1 & F2 --- E3[same RFQ engine]
  LDR --> ARC[(Archive + snapshots)]
  LDR --> EG[Egress → gateways → Kafka/Redpanda for downstream]
  ARC -- replay and follow --> RR[Read replicas<br/>reads, trace: built, inside the gateway]
  GW -- reads --> RR
```

Swap `Sequencer + ArchiveJournal` for Aeron Cluster's ingress/log; the engine becomes the `ClusteredService` unchanged. Cluster timers replace `Tick`.

**Read replicas are built** (§1): the replica and the follower run in the gateway process and replay the Archive. That is also the shape in production, where the log is the cluster's. Not built: running them as a service of their own, starting a replica from a snapshot instead of the whole log (a fresh replica replays everything, so its start-up and its trace grow with the journal, as the node's do, [transport-aeron.md](../specs/transport-aeron.md#journal-growth-and-retention-designed-not-built)), and an authenticated session that fixes the account a read or write may use (the HTTP API trusts the `actor` it is given, [read-api.md §9](../specs/read-api.md)).

## 7. Chain and external effects (production, not built)

The brief says to mock the chain, so in this build the ledger's `escrowed` bucket is the escrow, and `Deposit` (operator) and `Withdrawn` (event) are the only places money enters or leaves. Those two points are the seam where real systems attach. **The engine decides and records; workers at the edges carry out side effects.**

```mermaid
flowchart LR
  CH[(Chain / escrow contract)] -- deposit events --> CA[Chain adapter]
  CA -- "Deposit{to, amount}<br/>nonce = deposit id" --> ING[[Ingress]]
  ING --> ENG[RFQ engine]
  ENG -- "every command, journaled before apply" --> JRN[(Journal = outbox<br/>Archive recordings)]
  JRN -- "replay from own checkpoint:<br/>Withdrawn / PositionSettled" --> WF[Effect workers<br/>e.g. Temporal]
  ENG -. "best-effort notifications" .-> EGR[[Egress]]
  WF -- "send tx, retry, confirm" --> CH
  WF -- "WithdrawalFailed → re-credit" --> ING
  OR[Oracle sources] --> WF
  WF -- ProposeOutcome --> ING
```

### Escrow contract

| | Engine as escrow (this build, custodial) | On-chain escrow contract (non-custodial) |
|---|---|---|
| Who holds funds | The venue's ledger | A smart contract |
| What prevents theft | Invariants I1–I5, audit log, replay | Contract code: users sign orders (EIP-712); the contract checks both signatures and locks both sides in one transaction |
| Engine's role | Match and settle | Match only, then submit the signed match for on-chain settlement |

This is the hybrid model Polymarket uses: matching off-chain, settlement on-chain against conditional-token collateral. Moving to it doesn't change the engine. Escrow amounts and payouts are already exact integers (`qty × p` / `qty × (1−p)`), so the contract can verify them directly.

**Rules for the chain adapter:**
- **Exactly-once deposits:** nonces are strict (`nonce == last + 1`), so the adapter submits finalized deposits in chain order with consecutive operator nonces and durably records `deposit id → nonce` before sending. After a crash it re-sends from its record: `Duplicate` means already applied, and `NonceGap` means an earlier deposit is missing, so it can never double-credit or skip one.
- **Wait for finality:** only deposits past the chain's finality depth are sent, so a chain reorg can't take back money the engine has already credited.
- **Withdrawals go through an outbox:** a `Withdraw` debits the ledger and emits `Withdrawn` in the same command. The journal is the outbox, not egress: egress is best-effort, and a batch lost to a crash is never re-published (D3 F5), while the journal holds every applied command. A worker replays the Archive from its own durable checkpoint through a read-only engine (the replica and follower of §1 are that engine and that replay, built for reads; the follower starts at the first recording, so a worker would add a start position of its own), picks up each `Withdrawn`, and sends the transaction idempotently, keyed by `seq`. If the transaction ultimately fails, the worker submits a `WithdrawalFailed` command that re-credits the account, so the funds are never lost between ledger and chain. That command would be a new one, added only when the chain is real.

### Temporal (or any durable workflow engine)

**Not for the core.** Temporal's durable state, timers and replay are what the sequenced log, timers-as-data and Archive replay already provide. Its natural model is one workflow per request, but the ledger is shared across requests (one MM's capital backs quotes on many). Moving money between workflows would need distributed transactions, which is exactly where capital leaks. Every step would also become a round-trip to the Temporal server.

**Good at the edges**, for side effects that call outside systems and need retries:
- withdrawal and settlement transactions (send, retry, confirm, reconcile)
- oracle polling, then submitting `ProposeOutcome`
- notifications to requesters and MMs

Each worker reads the journal (Archive replay from its own checkpoint) for anything that must happen exactly once, uses egress only for best-effort notifications, and writes commands back to ingress. It's just another client of the engine, so the engine's determinism and replay guarantees are untouched.

## 8. Scaling

- **Measured (engine only):** `cargo bench -p rfq-domain` replays a 100,005-command log through `Engine::apply` at **~3.7 million commands/s**, about 270 ns per command. Machine: Apple M4 Max, macOS 27.0.1, rustc 1.99.0, bench profile (release with `overflow-checks` on), one thread. `state_hash` over the final state (20,000 requests, 40,000 quotes) takes **~11.9 ms**: it hashes the whole state, so it is for snapshots and replay checks, not for every command.
- **What that number is not.** The workload is 20,000 sequential full RFQ rounds (request, two quotes, tick, accept; all filled) on 3 funded accounts and 1 contract, one request in flight at a time. It is the engine alone: no codec, no fsync or Archive write, no Aeron. Both runs were taken while other builds kept the machine at load average 8 to 12, so read the figure as a rough order of magnitude (the two runs agreed within 1%). End-to-end throughput and latency over Aeron and the Archive were not measured; the durability wait, not the engine, is where that limit is expected to be.
- **Scaling claim.** Engine state is in `BTreeMap`/`BTreeSet`s (accounts, requests, quotes, positions, and the timer set), so a lookup or insert touches O(log n) entries. The benchmark does not test how that holds for much larger state than the one log above.
- Built in now: a timer index (`BTreeSet<(Timestamp, TimerKey)>`), ticks only when a timer is due, and events emitted once (gateways fan them out).
- Designed but not built: pruning terminal objects from memory, and snapshots.
- **Read side: not measured.** Per command the replica does the node's `Engine::apply` plus a `batch_of` and a trace insert, but no benchmark exists for its replay rate, for the lag between the node and the replica, or for HTTP latency. For scale only: a write's reply carries `latency_us` (command received to its batch received, through Aeron and the fsync), and the 6 writes of the README's HTTP walk-through, one at a time on Docker Desktop (Apple M4 Max, busy with other work), took 2.2 to 4.5 ms in one run. Memory: the replica holds the engine and every batch ever applied, so it grows with the journal; snapshots and retention would bound it.
- Beyond one engine: **shard by contract group**. Each shard holds its own sub-balance per account; cross-shard transfers are two-step with an `in_transit` bucket. Multi-leg requests must stay within one shard — cross-shard atomicity would need 2PC, which is exactly where capital leaks.
