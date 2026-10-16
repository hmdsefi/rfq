# Task R5: HTTP writes through the hub, smoke script

**Spec:** [read-api.md](../../specs/read-api.md) §3 (writes), §4, §10. **Goal:** `POST /v1/requests`, `/v1/quotes` and `/v1/commands` run a command exactly as the WebSocket does and answer with its batch; an end-to-end script proves a whole trade over HTTP.

**Files:**
- Create:
  - `crates/rfq-gateway/src/write_api.rs` (lib): body parsing and the answer from the frames;
  - `scripts/http-smoke.mjs`.
- Modify:
  - `crates/rfq-gateway/src/ws.rs` (connection ids from a shared `ConnIds`);
  - `crates/rfq-gateway/src/http_server.rs` or a new binary module `http_write.rs`: the short-lived hub connection;
  - `crates/rfq-gateway/src/main.rs` (share `ConnIds` and the inbox with the HTTP handler).

**Interfaces.**
- Consumes (R4): `http_server::{spawn, Request, Reply, HttpLimits}`, the inbox (`ws::Inbox`, `InboxItem::{Open, Text, Close}`), `protocol::{ClientMsg, ServerMsg, OutBatch}`.
- Produces:

```rust
// write_api.rs (pure)
/// The client message for a POST: `/v1/requests` → `SubmitRequest`, `/v1/quotes` → `SubmitQuote`,
/// `/v1/commands` → the body's `action`. `id` is the command id to use. Err = a 400 sentence.
pub fn command_for(path: &str, body: &[u8], id: &str) -> Result<(AccountId, ClientMsg), String>;

/// Folds the hub's frames for one command into its HTTP reply. Feed every frame from the
/// short-lived connection's outbox; `Some(reply)` once the answer is complete.
pub struct Answer { /* actor, id, nonce from `sent`, merged batch for origin (actor, nonce) */ }
impl Answer {
    pub fn new(actor: AccountId, id: String) -> Answer;
    pub fn on_frame(&mut self, frame: &ServerMsg) -> Option<Reply>;
}

// ws.rs
#[derive(Clone, Default)] pub struct ConnIds(Arc<AtomicU64>);
impl ConnIds { pub fn next(&self) -> ConnId; }
```

## Requirements

1. **Bodies (spec §3):** JSON objects. `actor` (number) required; `nonce` optional. `/v1/requests`: `legs` (array of `{market, side, qty, limit_price}`), `response_deadline`. `/v1/quotes`: `request, leg, price, size, expires_at`. `/v1/commands`: `action` in the WebSocket's JSON form (any client `Action`; `Genesis`, `Tick`, `Resume` cannot be expressed, as on the WebSocket). Unknown fields are refused (`deny_unknown_fields`). Errors → `400 {"error":"bad request: …"}`.
2. **The short-lived connection (spec §4):** id from `ConnIds::next()` (the WebSocket acceptor uses the same counter, so ids never collide). Send, in order: `Open { peer, outbox }` (an outbox of the WebSocket's `OUTBOX` size), `Text(hello with view {"account": actor})`, `Text(command with id "http-<conn>")`. Then read the outbox, feeding `Answer::on_frame`, until it returns a reply or 75 s pass (`504 {"error":"no answer from the gateway"}`). **Always** send `Close { why: "http request done" }` at the end, also when the client went away or the wait timed out (Review Focus 5).
3. **`Answer`:**
   - `sent` with this id → remember its `nonce`;
   - `batch` whose `origin` is `(actor, nonce)` → keep it (a second chunk with the same `seq` appends its events and replaces `balances`, like the dashboard's recorder);
   - `result` with this id → `200` or, if the kept events hold a `CommandRejected` (it is only ever addressed to the actor), `422` with `rejected` = its reason; body `{seq, nonce, latency_us, rejected, events, balances}`;
   - `error` with this id: `bad request: …` → `400`; `engine unavailable` → `503`; `ingress back-pressured, try again` → `429`; `nonce sync failed …` → `503`; `outcome unknown: …` → `504 {error, nonce}` (the nonce from `sent`); any other → `500`;
   - every other frame (welcome, status, pong, other batches, other ids) → ignored.
4. **One account, two writers (Review Focus 3):** the hub already queues per account; the answer must only ever take the batch with this command's `(actor, nonce)`.
5. **`scripts/http-smoke.mjs`** (Node 25, global `fetch` and `WebSocket`; usage and exit codes like `gateway-smoke.mjs`):
   - open a WebSocket as admin first and record every batch;
   - fresh accounts `t0 * 100 + id` and market `t0` (as the gateway smoke);
   - over HTTP only: the operator's deposits and `ListMarket` (`/v1/commands`), Alice's request (`/v1/requests`, limit 7000, 1,000 YES, deadline `+1.5 s`), MM1 at 6200 and MM2 at 6000 (`/v1/quotes`, using the request id = the request's `seq`);
   - after the deadline, `GET /v1/accounts/{alice}/requests?min_seq=…` until `selected` is set; `POST /v1/commands` `Accept` with picks from `selected`;
   - check: `GET /v1/accounts/{alice}/positions?min_seq=<accept seq>` → one position, side `Yes`, stake `600000000`; `GET /v1/accounts/{alice}` → escrowed `600000000`; MM1's quotes show `Lost`; `GET /v1/trace?view=admin&after_seq=<first deposit seq − 1>` holds `RequestOpened`, `QuoteSubmitted` ×2, `RequestPresented`, `QuoteFilled`, `RequestFilled` in order, and every batch the WebSocket recorded in that range is in the trace, equal; `GET /v1/trace?view=<mm1>` never shows MM2's quote or Alice's balances; a `POST` with a bad body → `400`; `Origin: http://evil.example` → `403`;
   - print `HTTP SMOKE OK` and exit 0, or the first failure and exit 1.

## Steps

- [ ] **Step 1: Failing `write_api` tests:** `command_for` for each route (good bodies; a missing `actor`, an unknown field, a bad `side`, a non-object body → `Err` starting `bad request:`); `Answer` folds `sent` + chunked `batch` + `result` → `200` with merged events; a `CommandRejected(InsufficientFunds)` → `422 rejected: "InsufficientFunds"`; each error sentence → its status; a `batch` with another nonce for the same actor is ignored.
- [ ] **Step 2:** Implement `write_api.rs`. Run: PASS.
- [ ] **Step 3: Failing round-trip tests** (in the binary, next to `hub_loop`'s tests: a real `HubLoop<FakeVenue>` driven by the test thread with `turn()`, its inbox a `sync_channel`, the HTTP write handler running in a client thread):
  - `post_command_returns_its_batch` (the test answers the venue's offer by pushing an egress batch with that origin);
  - `post_when_the_engine_is_down_is_503`;
  - `two_posts_for_one_account_each_get_their_own_seq` (Review Focus 3; also one `POST` plus one WebSocket-style connection for the same actor);
  - `a_client_that_goes_away_still_closes_its_hub_connection` (Review Focus 5: drop the client socket mid-wait; the hub loop's outboxes no longer hold that `ConnId` and the actor's queue is empty after the command's batch);
  - `the_wait_times_out_with_504` (with a short wait injected for the test).
- [ ] **Step 4:** Implement the short-lived connection and `ConnIds`; route `POST` in `main.rs`. Run: PASS.
- [ ] **Step 5: End to end:** `docker compose build gateway && docker compose up -d aeron engine gateway`, then `node scripts/http-smoke.mjs` → `HTTP SMOKE OK`; `node scripts/gateway-smoke.mjs` → `GATEWAY SMOKE OK`; `./scripts/demo.sh` → 8 PASS, `RECOVERY OK`.
- [ ] **Step 6: Gates** (workspace).
- [ ] **Step 7: Commit:**
  - `feat(gateway): HTTP writes through the hub`
  - `test(gateway): end-to-end HTTP smoke script`
