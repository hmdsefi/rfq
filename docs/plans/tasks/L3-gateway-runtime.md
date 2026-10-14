# Task L3: Gateway runtime (Aeron adapter, WebSocket server, Docker service, smoke script)

**Spec:** [gateway.md](../../specs/gateway.md) §6–§9. **Goal:** a running `rfq-gateway` container next to `rfq-node`. A WebSocket client on `ws://127.0.0.1:8080` can drive a full trade and get every batch, result and status back.

**Files:**
- Create:
  - `crates/rfq-gateway/src/main.rs` (binary; without the `aeron` feature it prints a message and exits 2, like rfq-node);
  - `crates/rfq-gateway/src/config.rs` (env: `AERON_DIR`, `RFQ_GATEWAY_ADDR` default `127.0.0.1:8080`; in Docker `0.0.0.0:8080`, published on host `127.0.0.1` only);
  - `crates/rfq-gateway/src/aeron.rs` (feature `aeron`);
  - `crates/rfq-gateway/src/ws.rs` (WebSocket server);
  - `scripts/gateway-smoke.mjs` (Node 25: global `WebSocket`).
- Modify:
  - `crates/rfq-gateway/Cargo.toml` (feature `aeron = ["dep:rfq-infra", "rfq-infra/aeron", …]`, `tungstenite`, `anyhow`, `tracing-subscriber`);
  - `Cargo.toml` (workspace dep `tungstenite`; pick the newest release at least two weeks old, default features only as needed, no TLS);
  - `deny.toml` only if a new license needs allowing (prefer crates that don't);
  - `docker-compose.yml` (service `gateway`);
  - `README.md` (a short "Gateway" paragraph: how to start it, the port, and the smoke script).

**Interfaces.**
- Consumes from L2: `Hub::handle`, `Input`, `Effect`.
- Consumes from rfq-infra: `aeron::{connect, offer, INGRESS, EGRESS, IPC, frame_of, …}`, `fragment::Reassembler`, `codec::{encode, decode}`. Look at how `rfq-demo/src/live.rs` publishes to ingress and reads egress, and do the same.
- Produces: the running service and the protocol on `ws://127.0.0.1:8080`.

## Requirements

1. **One hub thread (the core loop)** owns the `Hub`, the ingress publication and the egress subscription. Each turn of the loop:
   1. drain the WebSocket inbox (an `mpsc::Receiver<(ConnId, InboxItem)>`);
   2. poll egress, reassemble, decode the `EventBatch`, and feed `Input::Batch`;
   3. when `publication.is_connected()` changes, feed `Input::Ingress{connected}`;
   4. every 100 ms feed `Input::Tick`;
   5. perform each `Effect`:
      - `Offer` encodes the `ClientCommand` and offers it with the existing `offer` helper (back-pressure retries with a short patience; on failure send `Error {id, "ingress back-pressured, try again"}` back through the hub, or treat it as down if unconnected);
      - `Send` pushes a text frame to that connection's outbox.

   When idle, sleep 1 ms.
2. **The WebSocket server:**
   - an acceptor thread on `RFQ_GATEWAY_ADDR`;
   - for each connection, check the upgrade's `Origin` (gateway.md §7) and refuse others with HTTP 403;
   - one thread per connection, with a read timeout of about 20 ms, alternating between reading frames (text → inbox) and writing its outbox;
   - frames over 64 KiB get an `error` frame and the frame is discarded (tungstenite's max message and frame size configs, so the connection isn't killed for one bad frame where possible; if the library must close the connection, log it and document it);
   - ping/pong and close are handled by tungstenite;
   - a connection error closes only that connection (`Input::Close`).
3. **Logging** (`tracing`, `RUST_LOG` default info), as gateway.md §8:
   - `gateway listening addr=…`;
   - `ws open peer=… conn=…`, `ws close conn=…`;
   - `command conn=… actor=… nonce=… action=SubmitQuote`;
   - `result seq=… latency_us=…`;
   - `engine status=down|up`;
   - `nonce sync actor=… last=… probes=…`.

   No payload bodies at info level.
4. **Failure behaviour:**
   - The Aeron client, the driver or the publication can fail fatally (the driver is dead). Then log it and exit with status 1; Docker restarts the gateway (`on-failure:3`).
   - A dead engine is not fatal for the gateway. It shows as `Ingress{connected:false}` and the status goes down.
5. **docker-compose `gateway` service:**

```yaml
  gateway:
    build: { context: ., dockerfile: docker/rust.Dockerfile, args: { BIN: rfq-gateway } }
    ipc: "service:aeron"
    init: true
    restart: "on-failure:3"
    environment:
      RFQ_GATEWAY_ADDR: "0.0.0.0:8080"
    ports: ["127.0.0.1:8080:8080"]
    depends_on: [aeron]
```

   (The gateway may start before the engine: the status is simply down until the engine's first batch.) Check that `docker/rust.Dockerfile` builds `-p rfq-gateway --features aeron` unchanged, through its `BIN`/`FEATURES` args, and adjust only if needed.
6. **`scripts/gateway-smoke.mjs`:** a Node 25 script using the global `WebSocket`.
   - Connect as admin and send `hello`.
   - With fresh per-run account ids (`t0 * 100 + id`, as `rfq-demo`'s Scope does), send, as their actors:
     - the operator's deposits for the scenario accounts;
     - `ListContract` (`id = t0`, close `+5 s`, hard deadline `+15 s`, window `2 s`);
     - Alice's request (1000 BuyYes, limit 7000, deadline `+1.5 s`);
     - MM1's quote at 6200 and MM2's at 6000;
     - after the deadline, Alice's `Accept` with the picks taken from `RequestPresented`.
   - Expect, in order:
     - `RequestOpened`, `QuoteSubmitted` ×2, `RequestPresented`, `QuoteFilled`, `RequestFilled`;
     - balances in each batch (Alice's escrowed equals `$600` after accept, in micro-USD);
     - a `result` for every command, with latency.
   - Open a second socket with `hello` as MM1 and check it never receives an event addressed to another account, nor Alice's or MM2's balances.
   - Print `GATEWAY SMOKE OK` and exit 0, or the first failure and exit 1.

## Steps

- [ ] **Step 1:** Add the dependencies and the feature. Write `config.rs` with a unit test (default and env addresses) and `main.rs` (no-feature path exits 2). Gates (aeron off) green.
- [ ] **Step 2:** Write `ws.rs`, with a unit test that needs no Aeron. Start the server on an ephemeral port with a fake hub loop (a channel echo). Then:
  - a client with `Origin: http://localhost:1234` connects and an evil Origin gets 403;
  - a 65 KiB frame yields an `error` frame or a documented close;
  - a text frame reaches the inbox.

  Use `tungstenite::client` in the test.
- [ ] **Step 3:** Write `aeron.rs` and the hub loop (feature `aeron`; compiled only in Docker). Mirror rfq-demo's ingress and egress code.
- [ ] **Step 4:** Add the compose service. Build it: `docker compose build gateway engine`. Expected: success.
- [ ] **Step 5:** Run it end to end. Expected: `GATEWAY SMOKE OK`, and the gateway log shows command, result and status lines.

```bash
docker compose down -v
docker compose up -d aeron engine gateway
node scripts/gateway-smoke.mjs
docker compose logs gateway | tail -30
```

- [ ] **Step 6:** Run the engine-down drill by hand while connected:
  1. `docker compose kill engine` → the smoke client (or a `websocat`-free Node one-liner) receives `status down`, and a command gets `engine unavailable`;
  2. `docker compose start engine` → a `Resume` batch, then `status up`, and a command works again.

  Record the frames in the report.
- [ ] **Step 7:** Gates.
  - Workspace (aeron off): test, clippy, fmt, deny, all green.
  - `./scripts/demo.sh`: 6 PASS and `RECOVERY OK`, unaffected.
  - `docker compose down -v` at the end.
- [ ] **Step 8:** Commit (stage by path):
  - `feat(gateway): Aeron adapter and WebSocket server`
  - `feat(docker): gateway service`
  - `test(gateway): end-to-end smoke script`
  - `docs: gateway in the README`
