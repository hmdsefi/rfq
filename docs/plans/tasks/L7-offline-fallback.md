# Task L7 (optional): Offline fallback, the in-browser engine behind the same protocol

**Cut this task if time is short.** The live stack is the demo, and this is the "Docker broke on stage" fallback.

**Goal:** the same page, with no gateway reachable, can switch to "Offline: engine in the browser" and run the same scenarios through a local adapter that speaks the gateway's protocol. It must be clearly labelled as not the real stack.

## Requirements

1. **Rust side.** `tools/dashboard` adds an op that runs one command and returns gateway-shaped batches:
   - each batch carries `seq`, `time`, `kind`, `origin`, `events`, and `balances` from `Engine::take_touched_balances`;
   - the filter for the current view uses `rfq_gateway::view::filter`, a path dependency on the pure `rfq-gateway` lib with feature `aeron` off.

   The Session's clock follows the browser's `Date.now()`: the adapter sends a `tick` op every 50 ms, so timers fire in real time, like the node.
2. **JS side.** `web/local-gateway.js` implements the same interface as `gateway.js` (`hello`, `command`, frame events). The page picks it when the user chooses Offline, or when the gateway is unreachable at load and the user confirms.
3. **Label.** The page shows "Offline: engine in the browser. Not the real stack" for as long as it is in this mode.
4. **Test.** `live-smoke.mjs --offline` runs the 12 scenarios through the local adapter in Node: 12 PASS.

## Steps

- [ ] **Step 1:** Rust op plus tests in `tools/dashboard`. The batches have the same fields as the node's: compare one scenario's batches with what the gateway smoke printed live.
- [ ] **Step 2:** `local-gateway.js`, the page switch and the label.
- [ ] **Step 3:** `live-smoke.mjs --offline`: 12 PASS. A browser check of Offline mode.
- [ ] **Step 4:** Commit: `feat(dashboard): offline fallback behind the gateway protocol`.
