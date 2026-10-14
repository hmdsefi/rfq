# Gateway: the client edge (WebSocket)

Status: approved by the PO on 2026-10-15.

## 1. Why

The engine's inputs and outputs only exist on Aeron streams inside the venue:
- `ClientCommand` goes in on ingress 1001;
- an `EventBatch` comes out on egress 1003 for every applied command.

A client outside the venue needs one place to send commands and get its own results back, filtered so it sees only what it may see. engine.md §7 already assigns that filtering to "gateways", and the architecture lists the client read port as future work. This component builds it.

The gateway holds no business state and makes no business decisions. It moves `ClientCommand`s in and `EventBatch`es out, and filters the output per viewer. Every number a client sees was produced by the engine.

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
| `{"type":"sent","id":"c1","actor":10,"nonce":7}` | The command is on ingress. |
| `{"type":"batch","seq":12,"time":ms,"kind":"User","origin":{"actor":10,"nonce":7}\|null,"events":[{"audience":…,"event":…}],"balances":[{"account":10,"balance":{"free":…,"reserved":…,"escrowed":…}}]}` | One engine output, filtered for this connection's view (§4). |
| `{"type":"result","id":"c1","seq":12,"latency_us":840}` | To the sender only, right after the batch whose origin matches its command. `latency_us` runs from receiving the command to receiving its batch. |
| `{"type":"status","engine":"up"\|"down","last_seq":12}` | Sent when the engine's state changes (§6), and on `welcome`. |
| `{"type":"error","id":"c1"\|null,"error":"…"}` | A sentence. Cases: `bad request: …`, `engine unavailable`, `ingress back-pressured, try again`, `nonce sync failed for account N`, `message too large`. |

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
- Commands for one actor are sent in order. A second command for the same actor waits until the first has its batch, so the nonces never race.

## 6. Engine status

- **Down:** the ingress publication is not connected (no node is subscribed), or no batch has arrived within 3 s of a sent command.
- **Up:** the publication is connected again and a batch has arrived since.
- The status is broadcast on every change. A command that arrives while the engine is down is answered with `error: engine unavailable` and is not queued.
- After a restart, the node's first output is the `Resume` batch (§2.3). A client sees down, then `Resume`, then normal batches.

## 7. Trust boundary and security

- The demo has no authentication, as the brief allows ("Auth, accounts, KYC" are out of scope). The gateway trusts the `actor` a client names. **Production:** an authenticated session fixes the actor and the gateway never takes it from the message.
- The `admin` view is the venue's operator view. In production it is restricted to venue staff. In the demo every local client may use it.
- The gateway listens on `127.0.0.1` only. It accepts WebSocket upgrades only from `Origin: null` (a page opened from disk) and `http://localhost:*` / `http://127.0.0.1:*`, and it refuses frames over 64 KiB (the engine's ingress limit).
- It never panics on client input. Bad input gets an `error` frame and the connection stays open.

## 8. Logging

`tracing`, one line per event:
- connection opened and closed (with peer and view);
- each command sent (actor, nonce, kind of action);
- each result (seq, latency);
- each engine status change;
- each nonce sync (actor, last found, probes used).

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
  - the hub loop.

  Built in Docker like `rfq-node`.
- **docker-compose:** a `gateway` service (`ipc: service:aeron`, `127.0.0.1:8080:8080`, `restart: on-failure:3`).

## 10. Acceptance

1. A client that sends fund, list, request, two quotes and accept over the WebSocket receives each command's `result` and the batches with every expected event, each touched account's balances, and engine time.
2. The views hide exactly what §4 says. This is tested for every view on every event of the 12 scenarios.
3. Killing the engine broadcasts `down`. Restarting it broadcasts `up` and a `Resume` batch, and commands work again.
4. Bad JSON, unknown actions, huge frames and unknown message types each give an `error` frame. The gateway keeps running.
5. All existing tests stay green, `state_hash` is unchanged by the new output, and `scripts/demo.sh` still prints `RECOVERY OK`.
