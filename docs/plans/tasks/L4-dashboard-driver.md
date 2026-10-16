# Task L4: Dashboard driver (JS scenario scripts, run scope, recorder, perf metrics)

**Spec:** [tools/dashboard/DESIGN.md §11](../../../tools/dashboard/DESIGN.md) and [gateway.md §3](../../specs/gateway.md). **Goal:** headless JS that runs each scenario as its participants through the gateway, judges it PASS or FAIL, records every frame, and measures latency. L5 puts a UI on it; this task proves it from Node against the live stack.

**Files:**
- Create:
  - `tools/dashboard/web/gateway.js`: the WebSocket client. `connect(url) → { send(msg), on(type, fn), hello(view), command({actor, action, nonce?}) → Promise<{sent, result, batch}>, close() }`. It reconnects automatically with backoff, and on reconnect re-sends `hello` with the last view.
  - `tools/dashboard/web/scenarios.js`: declarative scripts plus the driver.
  - `tools/dashboard/web/recorder.js`: frames → ordered batches merged by `seq` (split chunks); `results`, `statuses`, perf aggregates.
  - `tools/dashboard/web/live-smoke.mjs`: Node 25 runner against the live gateway.
- Modify: `tools/dashboard/web/inline.mjs` and `build.sh`. Inline any number of script files: replace the fixed marker pair with a list of markers `/*__<NAME>_JS__*/`, keeping the one-occurrence and leftover-marker guards.

## Requirements

1. **Scripts.** Write the 12 presets as data, identical in steps, offsets, actors, actions and `expect` to:
   - `rfq_testkit::scenarios::all()` (`crates/rfq-testkit/src/scenarios.rs`);
   - the 6 attack presets (`tools/dashboard/src/presets.rs` and DESIGN §5).

   A step is `{ at, label, actor, action: (ctx) => Action, nonce?: "reuse" | "skip" }` or `{ at, wait: true }` (a former `tick`). `ctx` mirrors the testkit's `Ctx`: `market()`, `at(ms)`, `request(label)`, `quote(label)`, `picks(requestId)`, and `account(id)` (run scope). Write a comment above each script naming its Rust source.
2. **Run scope** (as `rfq-demo`'s `scope.rs`):
   - `t0` = the engine's now (from `welcome.now` plus the measured offset) + 500 ms;
   - accounts: `t0 * 100 + id`, except OPERATOR 1, ARBITER 2 and TREASURY 3;
   - market id: `t0`.

   A run owns:
   - the batches whose origin is one of its commands (record their seqs);
   - events naming its market, its accounts, or a request or quote id equal to one of its seqs.

   Concurrent runs use distinct `t0` (offset by 1 ms each).
3. **Driver:**
   - For each step, wait until engine time `t0 + at` (browser clock plus offset), then send the command through `gateway.command()` and await its `result`.
     - A command step's `label` records the result's `seq`.
     - Its own batch is passed to `ctx.observe` (for `RequestPresented` picks).
   - A `wait` step sleeps until `t0 + at` plus 300 ms of grace. The engine fires its own timers. Check `rfq-demo/src/live.rs` for how it waits, and mirror it.
   - The verdict: `contains_in_order` over the run's own events, with the same rule as the testkit (`CommandRejected(<Reason>)` may pin a reason).
   - `nonce: "reuse"` sends `nonce = last nonce sent for that actor`; `"skip"` sends `last + 2`.
   - A refused command (`error` frame) fails the run with that sentence. It must not hang. One exception is `outcome unknown … resend it with nonce N` (gateway.md §3/§6): the engine stopped answering after the command was sent, so it may or may not have been applied. Then:
     1. wait for `status up`;
     2. resend the same action with the explicit nonce N;
     3. a normal `result` means it was not applied before and now is; `CommandRejected(Duplicate)` means it was applied before the outage.

     Either way the command happened exactly once. Record it in the run log as "outcome recovered: applied once (Duplicate on retry)" or "…: applied on retry". The run then continues. Its expect list may still fail if the original batch was lost; report that honestly, never paper over it.
4. **Recorder.** Every frame gets the browser's receive time. Split batches merge by `seq`: events concatenate in arrival order and balances come from the last chunk. Perf per run: each command's `latency_us` (from `result`) and client round-trip, then p50, p95 and max, commands per second, and wall time.
5. **Run ×N with concurrency k.** `runMany(names, n, k)` returns every run's verdict and the aggregate perf. No run sees another's events (scope).
6. **`live-smoke.mjs`:**
   - Connect to `ws://127.0.0.1:18080`.
   - Run all 12 scenarios once, sequentially. Print `PASS <name>` / `FAIL <name>: <detail>` with p50/p95.
   - Run `happy_path` ×20 at concurrency 4. Print the aggregate.
   - Exit 0 only if everything passed.

## Steps

- [ ] **Step 1:** Generalize the inliner (`inline.mjs`/`build.sh`: a list of markers). Keep the guards, keep `smoke.mjs` working, and run `tools/dashboard/build.sh`. Expected: still 12 PASS (the WASM smoke) and the page written.
- [ ] **Step 2:** Write `gateway.js` and `recorder.js`, with a Node unit test file `web/recorder.test.mjs` (no network). It covers:
  - merging split chunks;
  - balances only from the last chunk;
  - percentile maths on known samples;
  - the scope `owns` rule on hand-written batches.

  Run `node web/recorder.test.mjs`. Expected: all pass.
- [ ] **Step 3:** Write `scenarios.js` (12 scripts plus the driver).
- [ ] **Step 4:** Run against the live stack. Expected: 12 PASS, then 20/20 PASS at concurrency 4 with latency percentiles printed, exit 0.

```bash
docker compose down -v && docker compose up -d aeron engine gateway
node tools/dashboard/web/live-smoke.mjs
```

  If a scenario fails only live, diagnose it with the gateway and engine logs. Clock offset and grace are the usual suspects. Never weaken an `expect` list to pass.
- [ ] **Step 5:** Commit: `feat(dashboard): live scenario driver, recorder and perf metrics over the gateway`, staging the files under `tools/dashboard/web` only (never `dist/`). Run `docker compose down -v` afterwards.
