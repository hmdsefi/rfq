# Task R7: Docs and architecture

**Spec:** [read-api.md](../../specs/read-api.md). **Goal:** every doc says what the system now does: a read side, an HTTP API with working curl examples, a trace retrievable per view.

**Files (modify):**
- `README.md`: an "HTTP API" section after "Live dashboard": the two ports; the curl walk-through below; a link to the spec; `http-smoke.mjs` in the list of checks; the dashboard's history on load.
- `docs/README.md`: rows for `specs/read-api.md` and the plan.
- `docs/architecture/overview.md`: the layers and crates (§0: `rfq-read`), the shape and components (§1: the read side, the follower, the HTTP server), the Docker topology (§5: the gateway's second port), the production path (§6: read replicas are now built, inside the gateway), the measured numbers if any are taken. Update its Mermaid diagrams and check each renders.
- `docs/specs/transport-aeron.md`: stream 1005 in the streams table; the follower (how it follows, recording switch, control-session polling) under the journal section.
- `docs/specs/gateway.md`: §1 and §9 point to read-api.md for the HTTP port.
- `docs/specs/failure-modes.md`: one row for the read side (the replica behind or lost: `as_of_seq`, `min_seq`, rebuild, never shows a non-durable command), keeping the file to **one printed page** (check: render to PDF at 11 pt A4 as was done for D3; see that commit's message).
- `docs/specs/adversarial.md`: the HTTP API has no authentication (anyone local can act as or read any account, including the admin trace): an accepted residual like the WebSocket's, with the production fix.
- `docs/rfc/0001-rfq-engine.md`: where it lists the read side or read replicas as future work, say what is built.
- `tools/dashboard/DESIGN.md` §11: history on load (if R6 did not already).

**The README curl walk-through** (every command run against the Docker stack before committing; paste real ids from that run into the text only as examples):

```bash
# fund Alice (10) and MM2 (21), list a contract, as the operator (1).
# Each POST answers with its batch; its "time" is the engine's clock (ms), the base for deadlines.
curl -s -X POST http://127.0.0.1:18081/v1/commands -H 'content-type: application/json' \
  -d '{"actor":1,"action":{"Deposit":{"to":10,"amount":1000000000}}}'
# ... ListContract, then:
curl -s -X POST http://127.0.0.1:18081/v1/requests -H 'content-type: application/json' \
  -d '{"actor":10,"legs":[{"contract":7,"side":"BuyYes","qty":1000,"limit_price":7000}],"response_deadline":…}'
curl -s -X POST http://127.0.0.1:18081/v1/quotes -H 'content-type: application/json' \
  -d '{"actor":21,"request":118,"leg":0,"price":6000,"size":1000,"expires_at":…}'
curl -s http://127.0.0.1:18081/v1/accounts/10/requests          # selected: {quote, price}
curl -s -X POST http://127.0.0.1:18081/v1/commands -H 'content-type: application/json' \
  -d '{"actor":10,"action":{"Accept":{"request":118,"picks":[{"leg":0,"quote":122,"price":6000}]}}}'
curl -s http://127.0.0.1:18081/v1/accounts/10/positions
curl -s "http://127.0.0.1:18081/v1/trace?view=10&limit=20"
```

Fill in the elided parts (the `ListContract` body, deadlines from engine time: the response to any `POST` carries the batch `time`, which is engine ms) so the sequence works when pasted. Keep the README section short: the walk-through, one sentence per step, a link to the spec for the full table.

## Steps

- [ ] **Step 1:** Write the README section; run every curl in order against `docker compose up -d aeron engine gateway`; fix until it works when pasted. Record the session in the report.
- [ ] **Step 2:** The other docs above. Render every changed Mermaid diagram (headless Chrome with mermaid 11, as earlier diagrams were checked) and the one-page check for D3.
- [ ] **Step 3: Final gates:** the full end-to-end set from the plan's Global Constraints.
- [ ] **Step 4: Commit:** `docs: the read side and HTTP API in the README, architecture and specs`.
