# Gateway: the client edge (WebSocket)

Status: approved by the PO on 2026-10-15. This document specifies the WebSocket edge. The same process also serves an HTTP API on port 18081, specified in [read-api.md](read-api.md).

## 1. Why

The engine's inputs and outputs only exist on Aeron streams inside the venue:
- `ClientCommand` goes in on ingress 1001;
- an `EventBatch` comes out on egress 1003 for every applied command.

A client outside the venue needs one place to send commands and get its own results back, filtered so it sees only what it may see. engine.md §7 already assigns that filtering to "gateways". This component builds it.

The gateway makes no business decisions, and its write path holds no business state. It moves `ClientCommand`s in and `EventBatch`es out, and filters the output per viewer. Every number a client sees was produced by the engine.

This WebSocket only pushes: a client sees what happens while it is connected. Asking for current state (positions, balances, requests, the trace of what it missed) and sending commands without a WebSocket are the **HTTP API** on port 18081, which the same process serves ([read-api.md](read-api.md)). Its writes enter the hub like a WebSocket command, so everything below (nonces §5, engine status and `outcome unknown` §6) applies to them unchanged. Its reads come from a read-only replica of the engine that the gateway builds from the journal. The replica makes no decision and no write consults it, so the hub still holds no business state.

```
client (dashboard, scripts) ──WebSocket JSON──► rfq-gateway ──ClientCommand──► ingress 1001 ──► rfq-node
                            ◄─filtered batches── rfq-gateway ◄──EventBatch──── egress 1003 ◄──
```

## 2. Engine output changes (rfq-domain, rfq-app, rfq-node)

Today a client sees transitions (events) but not money. These additions make each command's output complete. No state, journal record or replay behaviour changes.

1. **Touched balances.**
   - `Ledger` records which accounts each mutation changed:
     - `deposit` and `withdraw`: the account;
     - `hold` and `release`: the hold's account;
     - `escrow_from_hold`: the hold's account;
     - `pay_from_escrow`: both sides;
     - `forfeit`: both sides.
   - The set is not part of the state: it is `#[serde(skip)]`, so `state_hash` is unchanged.
   - `Engine::apply` clears the set before the command. `Engine::take_touched_balances() -> Vec<(AccountId, Balance)>` returns each touched account with its balance *after* the command, in account order.
   - A rejected command touches nothing (validate-then-apply).
2. **`EventBatch` gains three fields:**
   - `time: Timestamp`: the sequencer's stamp of the command (engine time; wall-clock ms);
   - `kind: BatchKind` (`User`, `Tick`, `Resume`, `Genesis`): which command it was;
   - `balances: Vec<(AccountId, Balance)>`: the touched balances from item 1.
3. **`Resume` is always published**, even when it emits no events, so a client sees every restart and every stall.
   - `Tick` stays as today: published only when it emits events.
   - Replay at start-up still publishes nothing.
4. **Codec.** The journal record (`SequencedCommand`) is unchanged, so codec `VERSION` stays 1 and existing Archive journals still replay. Egress readers (`rfq-demo`, `rfq-gateway`) are built from the same workspace as the node; mixing versions is out of scope (single `docker compose` build).

## 3. The WebSocket protocol

Text frames, one JSON object each, tagged by `type`. Ids are plain numbers, prices are bps (1..=9999), amounts are integer micro-USD, and times are engine ms. The engine's types keep their serde shape: `Action` and `Event` are externally tagged.

### Client → gateway

| Message | Meaning |
|---|---|
| `{"type":"hello","view":"admin"}`, or `{"type":"hello","view":{"account":10}}` | Sets this connection's view (§4). It may be sent again to switch. Until the first `hello` the view is `admin` (venue operator; see §7). |
| `{"type":"command","id":"c1","actor":10,"action":{…}}` | Sends `ClientCommand { actor, nonce, action }` with the gateway's next nonce for `actor` (§5). |
| `{"type":"command","id":"c2","actor":20,"nonce":3,"action":{…}}` | Same, but with an explicit nonce (attack scripts: replay, gap). The gateway's nonce book follows the engine's answer. |
| `{"type":"ping"}` | Answered with `pong`. |

### Gateway → client

| Message | Meaning |
|---|---|
| `{"type":"welcome","view":…,"engine":"up"\|"down","now":ms}` | Reply to `hello`. `now` is the gateway's wall clock, so clients can align their schedule with engine time. |
| `{"type":"sent","id":"c1","actor":10,"nonce":7}` | Accepted by the gateway and offered to ingress; an `error` (for example back-pressure) may still follow. |
| `{"type":"batch","seq":12,"time":ms,"kind":"User","origin":{"actor":10,"nonce":7}\|null,"events":[{"audience":…,"event":…}],"balances":[{"account":10,"balance":{"free":…,"reserved":…,"escrowed":…}}]}` | One engine output, filtered for this connection's view (§4). |
| `{"type":"result","id":"c1","seq":12,"latency_us":840}` | To the sender only, right after the batch whose origin matches its command. `latency_us` runs from receiving the command to receiving its batch. |
| `{"type":"status","engine":"up"\|"down","last_seq":12}` | Sent when the engine's state changes (§6), and on `welcome`. |
| `{"type":"error","id":"c1"\|null,"error":"…"}` | A sentence. Cases: `bad request: …` (including `bad request: message too large …` for a frame over 64 KiB, and `bad request: command too large for ingress (N bytes, limit 65536)` for a command whose encoding is over the engine's ingress limit: never offered); `engine unavailable` (the command was **never offered**: the engine was down, or the command was still waiting its turn); `outcome unknown: the engine stopped answering after this command was sent; resend it with nonce N to retry safely (a repeat is rejected as Duplicate)` (the command **was offered**, see §6); `ingress back-pressured, try again`; `nonce sync failed for account N`. |

## 4. Views: what each connection receives (sealing)

| Field | `admin` | `account X` |
|---|---|---|
| events | all | audience `Public` or `Account(X)` |
| `origin` | always | only when the actor is X; otherwise `null` (engine.md §7: origin identifies the sender) |
| `balances` | all | only X's |
| batch | always | dropped when no event and no balance is left for X; `seq`, `time` and `kind` are public (DESIGN §3) |
| `result` | to the sender | to the sender |

The filter is a pure function, `filter(view, &EventBatch) -> Option<OutBatch>`, unit-tested for every event kind.

## 5. Nonces

Nonces are strict per actor (`last + 1`). The gateway keeps a nonce book with `last` per actor:
- It advances on every batch whose origin is `(actor, n)`, unless that batch's only event is `CommandRejected(Duplicate | NonceGap)`, which consumes nothing.
- An actor the gateway has never seen is unknown. Before its first command, the gateway finds `last` with probes, as `rfq-demo` does today:
  - Each probe is `Withdraw { amount: 0 }` with a guessed nonce. `Duplicate` means too low, `NonceGap` means too high, and `InvalidInput` means it was exactly `last + 1` and has been consumed.
  - The probe logic moves from `rfq-demo` into `rfq-app::client::nonce`, so both use one implementation.
- Commands for one actor are sent in order. A second command for the same actor waits until the first has its batch, so the nonces never race. (One exception: while the actor's outcome is unknown, its retry goes ahead of the commands waiting behind it, §6.)

## 6. Engine status

- **Down:** the ingress publication is not connected (no node is subscribed), or no batch has arrived within 3 s of a sent command.
- **Up:** the publication is connected again and the engine has shown it works since: a batch that arrived while connected, or a restarted node's `Resume`. A batch polled while disconnected was published before the failure and does not count.
- The status is broadcast on every change. A command that arrives while the engine is down is answered with `error: engine unavailable` and is not queued; so is every command still waiting its turn when the engine goes down.
- **A command in flight when the engine goes down may still be applied.** It was offered to ingress, and a node that comes back (a hung engine resuming, or a restarted node reading what was left on ingress) applies it. Its sender gets `outcome unknown: … resend it with nonce N …` instead of `engine unavailable`, and still gets its `result` if the batch comes later. The batch whose `origin` is `(actor, N)` is the truth: it says whether and how the command was applied. To retry safely, resend it with the same explicit nonce `N`: if the first one was applied, the repeat is rejected as `Duplicate` and applies nothing.
- **While a command's outcome is unknown, the gateway holds that participant's other commands** until the retry with the same nonce `N` (or the late original) settles it: the first batch with origin `(actor, N)`, whatever it says. Meanwhile only a command with the explicit nonce `N` is sent, ahead of the waiting ones; the others wait in order, and the gateway sends no nonce probe for that participant (a probe search could end by consuming `N`, and the retry would then be a `Duplicate` for a command that was never applied). The hold lasts at most 60 s: then it is released (logged as `nonce unresolved actor=… nonce=… released`) and the participant's commands go on as usual.
- **Liveness probe.** While the engine is down and ingress is connected (a hung engine, or a node whose `Resume` was missed), commands are refused, so nothing would ever publish. About every second the gateway offers its own probe: `Withdraw { amount: 0 }` as the operator (account 1) with nonce 0. Nonce 0 is never above an actor's last nonce, so the engine always rejects it, as `Duplicate` (as `NotInitialized` before genesis), and consumes nothing. Its batch is the gateway's own: the rejection and the `origin` are stripped, and it is no news for the nonce book. What is left, the events and balances of any timers that fell due in the same apply, is forwarded to clients like any other batch (nothing is forwarded when nothing is left). It arrives while connected, so it marks the engine up.
- After a restart, the node's first output is the `Resume` batch (§2.3). A client sees down, then `Resume`, then normal batches.

## 7. Trust boundary and security

- The demo has no authentication, as the brief allows ("Auth, accounts, KYC" are out of scope). The gateway trusts the `actor` a client names. **Production:** an authenticated session fixes the actor and the gateway never takes it from the message.
- The `admin` view is the venue's operator view. In production it is restricted to venue staff. In the demo every local client may use it.
- The gateway listens on `127.0.0.1` only, port 18080 (`ws://127.0.0.1:18080`; 8080 is often taken on a developer's machine). It accepts WebSocket upgrades from `Origin: null` (a page opened from disk), `http://localhost:*` / `http://127.0.0.1:*`, and from requests with no `Origin` at all: non-browser clients (Node scripts, `websocat`) send none, and browsers always send one, so the check still refuses every other web site in the user's browser. Browsers also send `Origin: null` from a sandboxed iframe, on any site: acceptable for this unauthenticated local demo, not for production. Other Origins get HTTP 403. Its HTTP port, 18081, is the same: loopback only, the same `Origin` rule, no authentication, and the admin trace open to every local client ([read-api.md §9](read-api.md)).
- It refuses text frames over 64 KiB, and commands whose encoding is over the engine's ingress limit (64 KiB; a 64 KiB JSON frame can encode larger), before offering them.
- It never panics on client input. Bad input gets an `error` frame and the connection stays open.

## 8. Logging

`tracing`, one line per event:
- connection opened and closed (with peer and view);
- each command sent (actor, nonce, kind of action);
- each result (seq, latency);
- each engine status change;
- each nonce sync (actor, last found, probes used);
- each unknown outcome (actor, nonce), and when that nonce is settled or released.

Together with the node's `applied seq hash` lines, `docker compose logs -f gateway engine` shows every step.

## 9. Shape (hexagonal, like the node)

- **`rfq-gateway` (library), pure:**
  - `protocol` (WebSocket messages);
  - `view` (the filter);
  - `nonce_book`;
  - `hub`: the gateway's state machine. Inputs: client messages, egress batches, ticks, engine status. Outputs: effects (offer a command, send a frame, broadcast).

  No I/O. Fully unit-tested.
- **`rfq-gateway` (binary, feature `aeron`):**
  - the Aeron adapter: the ingress publication and the egress subscription with the `Reassembler`, reusing `rfq-infra`;
  - the WebSocket server (`tungstenite`, one thread per connection);
  - the hub loop;
  - the HTTP server and the replica thread (journal follower, replica), specified in [read-api.md](read-api.md) §2 to §5: `GET`s read the replica, `POST`s enter the hub as a short-lived connection (one id counter shared with the WebSocket acceptor).

  Built in Docker like `rfq-node`.
- **docker-compose:** a `gateway` service (`ipc: service:aeron`, `127.0.0.1:${RFQ_GATEWAY_PORT:-18080}:18080` for the WebSocket and `127.0.0.1:${RFQ_HTTP_PORT:-18081}:18081` for the HTTP API, `restart: on-failure:3`). In the container the servers listen on `0.0.0.0` (`RFQ_GATEWAY_ADDR`, `RFQ_HTTP_ADDR`); the host side stays on loopback.

## 10. Acceptance

1. A client that sends fund, list, request, two quotes and accept over the WebSocket receives each command's `result` and the batches with every expected event, each touched account's balances, and engine time.
2. The views hide exactly what §4 says. This is tested for every view on every event of the 14 scenarios (the testkit's eight and the dashboard's six attacks).
3. Killing the engine broadcasts `down`. Restarting it broadcasts `up` and a `Resume` batch, and commands work again.
4. Bad JSON, unknown actions, huge frames and unknown message types each give an `error` frame. The gateway keeps running.
5. All existing tests stay green, `state_hash` is unchanged by the new output, and `scripts/demo.sh` still prints `RECOVERY OK`.
