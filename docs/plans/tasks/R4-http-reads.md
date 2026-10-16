# Task R4: Gateway HTTP server, reads, replica thread, Docker port

**Spec:** [read-api.md](../../specs/read-api.md) §2, §3 (reads), §6, §7, §9. **Goal:** `curl http://127.0.0.1:18081/v1/...` reads positions, requests, quotes, balances, markets, status and the trace from a replica the gateway keeps current.

**Files:**
- Create:
  - `crates/rfq-gateway/src/origin.rs` (lib): `pub fn origin_allowed(origin: Option<&str>) -> bool`, moved from `ws.rs` (which now calls it), with its tests;
  - `crates/rfq-gateway/src/status.rs` (lib): `EngineStatus` (shared, atomics) — `up: AtomicBool`, `last_seq: AtomicU64` (0 = none); `pub fn snapshot(&self) -> (bool, Option<u64>)`;
  - `crates/rfq-gateway/src/read_api.rs` (lib): the read side's shared state and the pure `GET` router;
  - `crates/rfq-gateway/src/http_server.rs` (binary module, like `ws.rs`): the HTTP/1.1 server;
  - `crates/rfq-gateway/src/read_side.rs` (binary, feature `aeron`): the replica thread.
- Modify:
  - `crates/rfq-gateway/src/lib.rs`, `main.rs`, `config.rs` (`http_addr`, env `RFQ_HTTP_ADDR`, default `127.0.0.1:18081`, with tests like the existing ones), `hub.rs` (`pub fn engine(&self) -> EngineState`, `pub fn last_seq(&self) -> Option<u64>`), `hub_loop.rs` (after each turn, store the hub's engine state and last seq into `Arc<EngineStatus>`), `ws.rs`;
  - `crates/rfq-gateway/Cargo.toml` (`rfq-read`; `httparse` as a direct dependency at the version already in `Cargo.lock`);
  - `docker-compose.yml` (gateway: env `RFQ_HTTP_ADDR: "0.0.0.0:18081"`, port `"127.0.0.1:${RFQ_HTTP_PORT:-18081}:18081"`, comments in the file's style).

**Interfaces.**
- Consumes: `rfq_read::{Replica, ReplicaError, TraceEntry, views::*}` (R2), `JournalFollower` (R3), `view::filter` and `View` (existing), `protocol::OutBatch`.
- Produces (used by R5 and R6):

```rust
// read_api.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaState { CatchingUp, Following, Unavailable }

pub struct ReadSide { /* RwLock<(Replica, ReplicaState)> + Condvar notified on every apply */ }
impl ReadSide {
    pub fn new() -> Arc<ReadSide>;
    /// The follower thread's write path: apply one command, notify waiters.
    pub fn apply(&self, sc: &SequencedCommand) -> Result<(), ReplicaError>;
    /// `apply`, except that `seq` 1 after a higher `seq` (a new journal) first replaces the
    /// replica with an empty one. The replica thread uses this.
    pub fn apply_or_reset(&self, sc: &SequencedCommand) -> Result<(), ReplicaError>;
    pub fn set_state(&self, s: ReplicaState);
    /// Swap in a rebuilt replica (spec §5 step 7).
    pub fn replace(&self, r: Replica);
    pub fn last_seq(&self) -> u64;
    /// Waits until `last_seq() >= seq` or `timeout`; true if reached.
    pub fn wait_for(&self, seq: u64, timeout: Duration) -> bool;
}

pub struct Reply { pub status: u16, pub body: String }           // body is JSON
pub const MIN_SEQ_WAIT: Duration = Duration::from_secs(2);
pub const TRACE_DEFAULT: usize = 500;
pub const TRACE_MAX: usize = 1_000;
/// Routes one GET (path without query, raw query string). Pure apart from the `min_seq` wait.
pub fn get(read: &ReadSide, engine: &EngineStatus, path: &str, query: &str, min_seq_wait: Duration) -> Reply;

// http_server.rs (binary)
pub struct HttpLimits { pub max_connections: usize, pub head_timeout: Duration, pub max_body: usize }
pub const LIMITS: HttpLimits = HttpLimits { max_connections: 64, head_timeout: Duration::from_secs(10), max_body: MAX_TEXT };
pub struct Request { pub method: String, pub path: String, pub query: String, pub origin: Option<String>, pub body: Vec<u8> }
pub fn spawn(listener: TcpListener, limits: HttpLimits, handler: Arc<dyn Fn(&Request) -> Reply + Send + Sync>) -> io::Result<JoinHandle<()>>;
```

## Requirements

1. **Routes (GET):** exactly the spec §3 read table. Every `200` body is a JSON object with `as_of_seq` (the replica's `last_seq` at the read) plus the fields named in the spec: `/v1/accounts/{id}` → the `AccountView` fields at top level; `/positions` → `{positions}`; `/requests` → `{requests}`; `/quotes` → `{quotes}`; `/v1/requests?state=open[&market=ID]` → `{requests}`; `/v1/markets` → `{markets}`; `/v1/markets/{id}` → the `MarketView` fields (404 `{"error":"no such market"}` if missing); `/v1/status` → spec shape (`state_hash` lower-case hex, `engine` from `EngineStatus`).
2. **Errors:** a non-numeric id or bad query value → `400 {"error":…}`; unknown path → `404`; a known path with another method → `405`; `min_seq` not reached within the wait → `503 {"error":"replica behind","as_of_seq":N}`; `GET /v1/requests` without `state=open` → `400`.
3. **Trace (spec §6; Review Focus 4):** `view=admin` or `view=<account id>` (required; else `400`). Scan entries with `seq > after_seq` in order; for an account view apply `view::filter(View::Account(a), &entry.batch)` and skip `None`; `command` is shown for admin always, for an account only when `command.actor == a` (else `null`). Stop after `limit` entries; `next_after_seq` = the last returned entry's `seq` if the page is full and more entries exist after it, else `null`. An entry is `OutBatch` fields at top level plus `command`.
4. **Server:** one acceptor thread; a thread per connection, at most `max_connections` (more: write `503 {"error":"too many connections"}` and close); read the head with `httparse` within `head_timeout` (16 KiB max head); `POST` needs `Content-Length` ≤ `max_body` (`411`/`413` otherwise); exactly one request per connection, answered with `Connection: close`, `Content-Type: application/json`.
5. **Origin (spec §9):** `Origin` present and not allowed → `403 {"error":"origin not allowed"}`. Allowed → add `Access-Control-Allow-Origin: <origin>` and `Vary: Origin`. `OPTIONS` from an allowed origin → `204` with `Access-Control-Allow-Methods: GET, POST`, `Access-Control-Allow-Headers: content-type`, `Access-Control-Max-Age: 600`.
6. **Replica thread (`read_side.rs`, feature `aeron`):** `JournalFollower` and rusteron's `Aeron` are not `Send` (Rc-based handles), so the thread connects its **own** Aeron client (`aeron::connect(&config.aeron_dir)`) and opens the follower inside itself. Loop: `poll` into `ReadSide::apply_or_reset`; sleep 1 ms when it delivered nothing. State: `CatchingUp` until `follower.caught_up()` is first true, then `Following` (normal lag after that does not flip it back; only a rebuild or an error changes it). `poll`'s `Ok(0)` only means "nothing delivered by this call", never "caught up" (R3 ruling).
   - `seq` 1 after a higher `seq` → a new journal (Review Focus 2): `apply_or_reset` replaces the replica with an empty one, then applies this command.
   - Any other sequence error, or a follower error → `Unavailable`, log it, wait 1 s, open a new follower into a fresh `Replica` built on the side (the old one keeps serving), and `replace` it once its `last_seq` reaches the old one's (immediately if the journal restarted at seq 1).
   - A dead Aeron client is fatal for the gateway as today (exit 1, Docker restarts it).
7. `main.rs` starts the read side and the HTTP server next to the WebSocket server; the HTTP handler for `GET` calls `read_api::get` with `MIN_SEQ_WAIT`. (`POST` routes answer `405` in this task; R5 adds them.)
8. **Logging:** `http listening addr=…`, and per request at info `http method=… path=… status=… ms=…` (no bodies); replica state changes at info (`replica state=following last_seq=…`).

## Steps

- [ ] **Step 1: Move `origin_allowed` to `origin.rs`**; `ws.rs` uses it; its tests move with it. Run the gateway tests: PASS.
- [ ] **Step 2: Failing router tests** (`read_api.rs` tests; build a `ReadSide` by applying `run_in_memory(happy_path).0.log` — the `Harness` log — through `ReadSide::apply`):
  - each read route returns `200` with `as_of_seq` and the expected rows (Alice's position, balances, MM2's quote, markets, open requests at a mid-scenario point);
  - `trace_admin_pages_in_order_with_commands` (`limit=2` → 2 entries, `next_after_seq` = the second's seq; following pages reach the end with `null`);
  - `trace_for_an_account_skips_what_it_cannot_see_and_still_advances` (Review Focus 4: a view of MM1 over a journal where most batches are invisible to MM1; pages never empty while more visible entries exist, `next_after_seq` strictly increases, the last is `null`; no MM2 quote events, no other balances, `command` only on MM1's own batches);
  - `min_seq_waits_then_answers` (a thread applies the next command after 50 ms) and `min_seq_times_out_with_503` (with a 100 ms wait);
  - `a_new_journal_resets_the_replica` (Review Focus 2: `apply_or_reset` seq 1..=5, then a fresh journal's seq 1: `last_seq` becomes 1 and no old row is served);
  - `400`, `404`, `405` cases; `/v1/status` shape.
- [ ] **Step 3:** Implement `status.rs`, `read_api.rs`, the hub accessors and the hub loop's status updates. Run: PASS.
- [ ] **Step 4: Failing server tests** (`http_server.rs` tests, real sockets on `127.0.0.1:0` with a stub handler): one request answered with `Connection: close`; `Origin: http://evil.example` → `403`; `Origin: null` → `200` with `Access-Control-Allow-Origin: null` and `Vary: Origin`; `OPTIONS` → `204` with the allow headers; a body over the limit → `413`; `POST` without `Content-Length` → `411`; a head that never completes → closed after `head_timeout` (use 200 ms in the test's limits); the `max_connections + 1`-th connection → `503`.
- [ ] **Step 5:** Implement `http_server.rs`; wire `main.rs` and `config.rs`. Run: PASS.
- [ ] **Step 6:** Implement `read_side.rs` (feature `aeron`). Add the compose port and env. `docker compose build gateway`; `docker compose up -d aeron engine gateway`; run `node scripts/gateway-smoke.mjs` (creates a trade), then by hand:

```bash
curl -s http://127.0.0.1:18081/v1/status
curl -s "http://127.0.0.1:18081/v1/trace?view=admin&limit=3"
curl -s http://127.0.0.1:18081/v1/markets
```

  Expected: `replica.state` `following`, `replica.last_seq` equal to `engine_last_seq` (or within a few), `state_hash` equal to the engine's latest `applied seq=… hash=…` log line at that seq (`docker compose logs engine | tail`). Record the output in the report.
- [ ] **Step 7: Gates:** workspace gates, `docker compose build`, `scripts/demo.sh` (8 PASS, `RECOVERY OK`), gateway smoke.
- [ ] **Step 8: Commit:**
  - `refactor(gateway): the Origin rule in its own module`
  - `feat(gateway): HTTP read API served by the replica`
  - `feat(docker): the gateway's HTTP port`
