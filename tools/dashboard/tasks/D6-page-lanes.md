# Task D6: Page — top bar, lanes, contract card

**Blocked until** the PO approves D1. The approved `web/mockup.html` is the binding visual source: the same tokens, classes, markup structure and copy.

**Goal:** replace the temporary `web/index.html` with the real page. The top bar drives presets, the clock and the view. The market-maker and user lanes and the contract card render from engine state, and every button sends a real engine command. The trace and deliverable panels come in D7; this task leaves their containers empty.

**Files:**
- Modify: `tools/dashboard/web/index.html` (full rewrite, starting from the mockup).
- Test: browser checks in Step 7. All engine logic is already tested natively (D2–D4) and headless (D5).

**Interfaces:**
- Consumes:
  - `createEngine(bytes).call(request)` from `web/runtime.js`, inlined at `/*__RUNTIME_JS__*/`;
  - the JSON protocol in D4;
  - the D1 mockup.
- Produces, used by D7:
  - globals `state` (the latest response), `prev` (the one before), `presets` (the catalogue) and `selected` (`{kind, id}` or null);
  - `send(request) -> response | null`;
  - `render()`, which calls every `render*` function, including D7's `renderTrace`, `renderLedger`, `renderInspector` and `renderPanels` (stubbed as no-ops here);
  - the formatters `fmtUsd`, `fmtPrice`, `fmtT`, `name(id)`;
  - `select(kind, id)`.

## Requirements

1. **Structure.**
   - The mockup's `<style>` block is copied verbatim.
   - The mockup's markup becomes static containers plus template functions that build the same markup from `state`.
   - The page keeps the two build markers: `<script>/*__RUNTIME_JS__*/</script>` and `const WASM_BASE64 = "__WASM_BASE64__";`.
2. **Core JS.** Use this exactly; it's the only code that touches the engine.

```js
const WASM_BASE64 = "__WASM_BASE64__";
const T0 = 1000; // presets::T0; times are shown as t+ from here
const ACCOUNTS = { OPERATOR: 1, ARBITER: 2, TREASURY: 3, ORACLE: 4, ALICE: 10, MM1: 20, MM2: 21, CAROL: 30 };
let engine, state = null, prev = null, presets = [], selected = null, playing = null;

function send(request) {
  const out = engine.call(request);
  if (out.error) { showError(out.error); return null; }
  if (out.snapshot) { prev = state; state = out; }
  render();
  return out;
}

const fmtUsd = (micro) => {
  const v = Math.abs(micro), sign = micro < 0 ? "−" : "";
  const dollars = Math.floor(v / 1e6), cents = Math.floor((v % 1e6) / 1e4);
  return `${sign}$${dollars.toLocaleString("en-US")}.${String(cents).padStart(2, "0")}${v % 1e4 ? "+" : ""}`;
};
const fmtPrice = (bps) => (bps / 10000).toFixed(4).replace(/0{1,2}$/, ""); // 6000 → "0.60"
const fmtT = (ms) => `t+${((ms - T0) / 1000).toFixed(3)} s`;
const name = (id) => state?.snapshot.accounts.find((a) => a.id === id)?.name
  ?? Object.entries(ACCOUNTS).find(([, v]) => v === id)?.[0] ?? `#${id}`;
const variant = (v) => (typeof v === "string" ? v : Object.keys(v)[0]); // serde enum → its variant name

(async () => {
  const bytes = Uint8Array.from(atob(WASM_BASE64), (c) => c.charCodeAt(0));
  engine = await createEngine(bytes);
  presets = engine.call({ op: "presets" }).presets;
  send({ op: "reset", mode: "sandbox" });
})().catch((e) => showError(`The engine failed to start: ${e}`));
```

3. **Top bar.**
   - **Preset picker.** List the sandbox, the brief presets and the attack presets, grouped. Changing it sends `reset`, stops play, and clears `selected`. Show the preset's `summary`, its `expect` chips, and `step/steps` with `next`.
   - **Step and play.**
     - **Step** sends `step`.
     - **Play** sends `step` every 700 ms until the preset is `finished` or `taken_over`, or the presenter presses **Pause**.
     - **Run to end** sends `run_all`.
     - **Reset** re-sends `reset` with the current mode.
     - **Take over** sends `take_over`.
   - **Verdict.** When `preset.finished`, show PASS or FAIL from `preset.verdict`, with `detail` on FAIL.
   - **Clock.** **+100 ms** and **+1 s** send `advance`; **Next timer** sends `next_timer` and is disabled with a tooltip when `snapshot.next_timer` is null. Show `fmtT(snapshot.now)`.
   - **View as.** Admin, Alice, Carol, MM1, MM2. Each sends `view`.
   - **Health chip.** `seq`, the first 8 hex digits of `state_hash`, and I1–I5 (OK, or BROKEN with `invariants.error` in a tooltip).
   - **Label.** Show "Demo aid — not part of the engine" (exact text).
4. **Market makers lane.** One card per MM (MM1, MM2). In a non-admin view, show only the viewer's own card, if they're an MM.
   - **Free, reserved and escrowed balances** from `snapshot.accounts`.
   - **Open requests.** List requests with `public_state == "open"`: contract, side, qty, time left until `response_deadline`. **Never show a limit** in an MM card, even as admin, so the card shows what an MM sees.
   - **Quote form** per open request: price in cents (0.01–99.99, converted to bps with `Math.round(cents * 100)`), size (default = the request qty), expiry in seconds from now (default 10). It sends `act` with `{SubmitQuote:{request, leg:0, price, size, expires_at: now + secs*1000}}`. Quoting again on the same request is the engine's Replace; label the button "Replace" when the MM already has a live quote on that request.
   - **The MM's quotes** from `snapshot.quotes` with a state badge, price, size and the hold (`size × (1 − price)`, shown as a label computed from the engine's numbers for display only). **Cancel** sends `{CancelQuote:{quote}}`. Show it on every live quote: the engine answers for a Selected one, which is part of the demo.
5. **Users lane.** One card per requester (Alice, Carol). In a non-admin view, show only the viewer's own card, if they're a requester.
   - **Balances.**
   - **New request form:** side (BuyYes/BuyNo), qty, limit in cents, deadline in seconds from now (default 1.5). It sends `act` with `{SubmitRequest:{legs:[{contract, side, qty, limit_price}], response_deadline}}`, using the first contract in `snapshot.contracts`.
   - **Their requests** with a state badge from `variant(state)`; for `Rejected` also show the cause.
     - **Open:** Cancel.
     - **Presented:** the picks (quote id, price) and the time left until `accept_by`, with **Accept**, which sends `{Accept:{request, picks}}` with `picks` exactly as presented, and **Reject**.
   - **Dispute** button next to every contract in `Proposed`. Any requester may dispute; the engine takes the bond.
6. **Contract card** (always visible; contracts are public):
   - id, description, a state badge from `variant(state)`;
   - the resolution timeline as in the mockup: trading close, proposal and `dispute_until`, resolution deadline, a "now" cursor at `snapshot.now`;
   - **Oracle actions:** Propose YES, NO or INVALID (`ProposeOutcome`, actor ORACLE);
   - **Arbiter actions:** Rule YES, NO or INVALID (`Rule`, actor ARBITER);
   - positions from `snapshot.positions`: holders, qty, locks, settled.
7. **Errors and feedback.**
   - `showError(msg)` shows a dismissible inline banner with the engine's sentence. It never uses `alert()`.
   - Form validation is inline next to the field, it blocks sending, and it clears on edit: "Enter a price between 0.01 and 99.99", "Enter a whole number of shares".
   - An engine `CommandRejected` is not an error. It is shown in the trace (D7) and as a short toast in the acting card, e.g. "Rejected: InvalidState".
8. **Selection.** Clicking any id (request, quote, contract) calls `select(kind, id)`, which sets `selected` and re-renders. D7's inspector and state-machine panel read it.
9. **Accessibility.** Every control is a real `<button>`, `<select>` or `<input>` with a label. The focus ring is visible. Colour is never the only signal: badges carry text.

## Steps

- [ ] **Step 1:** Copy `web/mockup.html` to `web/index.html`. Keep `<style>`. Replace the sample-data markup with empty containers (`#topbar`, `#mm-lane`, `#user-lane`, `#contract`, `#trace`, `#ledger`, `#inspector`, `#panels`), and add the two build markers and the core JS above.
- [ ] **Step 2:** Implement `renderTopBar`, `renderMarketMakers`, `renderUsers`, `renderContract` and `showError`, plus the event handlers. Use event delegation on the lane containers (`data-action`, `data-id`). Re-render by replacing each container's `innerHTML`, and re-bind nothing.
- [ ] **Step 3:** Stub `renderTrace`, `renderLedger`, `renderInspector` and `renderPanels` as `function renderX() {}`, and call all eight from `render()`.
- [ ] **Step 4:** Run `tools/dashboard/build.sh`. Expected: the smoke checks pass and `dist/rfq-dashboard.html` is written.
- [ ] **Step 5:** Open `dist/rfq-dashboard.html` in the built-in browser at 1440×900. The console has no errors.
- [ ] **Step 6:** Play `happy_path` with Play. Expected: the quotes appear in the MM cards, Alice's card shows Presented with Accept/Reject, then Filled, and the contract card ends Final with settled positions. The verdict is PASS.
- [ ] **Step 7: Browser checks** (use `read_page`, `find` and `javascript_tool` to read state, and screenshots to compare with the mockup):
  1. In sandbox, Alice requests 1000 BuyYes at limit 70¢ with a 1.5 s deadline. MM1 quotes 62¢ and MM2 quotes 60¢. **Next timer** presents MM2's 0.60 to Alice. **Accept** gives Filled.
  2. Reset `mm_cannot_withdraw_after_selection`, step to just after `RequestPresented`, take over, press Cancel on MM2's Selected quote. Expected: toast "Rejected: InvalidState" and the quote stays Selected.
  3. A price of `0` in the quote form shows the inline error and sends nothing (check with `javascript_tool` that `state.snapshot.seq` didn't change).
  4. View as MM1 shows only MM1's card in the MM lane, no Alice card, and no limit anywhere.
  5. Compare screenshots with the mockup at 1440×900 in both themes and fix any drift.
- [ ] **Step 8: Commit.**

```bash
git add tools/dashboard/web/index.html
git commit -m "feat(dashboard): page with presets, clock, view-as, market-maker and requester lanes, contract card"
```
