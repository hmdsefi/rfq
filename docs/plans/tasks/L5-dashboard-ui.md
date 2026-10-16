# Task L5: Dashboard UI on the live stream

**Spec:** [DESIGN.md §11.1–§11.3](../../../tools/dashboard/DESIGN.md). The approved visual design (§10 and `web/mockup.html`) still applies. **Goal:** the page runs scenarios through the real stack and shows the real output: status, Run controls, timeline scrubber, perf panel, failure drills, view-as. All panels render from the recorded stream.

**Files:**
- Modify: `tools/dashboard/web/index.html`.
- Create: `tools/dashboard/web/model.js`: `modelAt(recording, position)`, which builds the shape the existing renderers read:
  - accounts with balances: the latest `balances` up to `position`;
  - requests, quotes, markets and positions, from events plus the page's own sent commands (a requester's limit, a quote's size);
  - trace entries, one per batch.

  Its unit test is `web/model.test.mjs`.

## Requirements

1. **Connection.**
   - Default `ws://127.0.0.1:18080`; override it with `?gateway=ws://…`.
   - A status bar shows:
     - Connected, Reconnecting or Disconnected;
     - engine up or down (from `status`);
     - last seq;
     - the engine clock (from the last batch time, ticking locally between batches for display only).
   - Live mode has no +100 ms, +1 s or Next timer buttons.
2. **Run controls.**
   - The combo box lists the 12 scenarios.
   - Buttons: **Run**, **Run ×N** (N field), **Run all**, and a concurrency field.
   - While running: a progress line ("happy_path: step 7 of 13") and a Stop button. Stop abandons the driver, while the engine finishes whatever is already sent.
   - Results table: scenario, PASS/FAIL (with detail), commands, p50, p95, max, wall time.
3. **Timeline.** A scrubber (range input plus ←/→ buttons and keyboard arrows) over the recorded batches of the selected run, or "all".
   - Every panel renders `modelAt(recording, position)`: lanes, market card, trace, ledger, D1 state machines with the selected object lit.
   - "Live" mode follows the newest batch; moving the scrubber pins it.
4. **Lanes and market.**
   - Same look as v1, but read-only in live mode: scenarios drive the commands. Manual commands are out of scope for v2.
   - Show the run-scoped account ids with their display names: "Alice (run …1234)".
5. **Trace.**
   - One row per batch: seq, engine time, kind, actor and nonce from `origin`, events with audience badges, balance deltas.
   - System batches are labelled (Tick, **Resume**).
   - A Resume row gets a distinct style and the note "engine restarted or stalled".
6. **Ledger.** Balances from `balances` only. Deltas are the display diff between the selected and the previous position. Money is never computed from prices.
7. **View as.** Re-sends `hello` with the view, clears nothing, and records what arrives from then on. Show a note: "From now on you see what the gateway sends <name>." Earlier frames stay as they were recorded.
8. **Failure drills panel.** DESIGN §11.1 item 7's three drills, with copy buttons (`navigator.clipboard` with a fallback of selecting the text) and "what to watch".
9. **Perf panel.**
   - Latency histogram (inline SVG; bars from the recorded `latency_us`);
   - p50, p95, max;
   - commands per second;
   - runs passed of total.
10. **D1–D5 panels** stay, and D1 lights objects from the model.
11. **The v1 WASM code path is removed from the default page.** L7 may bring it back as a labelled offline mode.

## Steps

- [ ] **Step 1:** `model.js` and `model.test.mjs`. Feed recorded frames (save some from L4's live run as a fixture JSON under `web/fixtures/`) and assert:
  - object states at given positions (for example, at the `RequestPresented` batch request #N is `Presented` and quote #M is `Selected`);
  - balances equal the last `balances` seen.

  Run with node. Expected: all pass.
- [ ] **Step 2:** Rework `index.html` (requirements 1–11), reusing v1's renderers on the model. Rebuild with `tools/dashboard/build.sh`.
- [ ] **Step 3:** Browser check against the live stack. Start the stack:

```bash
docker compose down -v && docker compose up -d aeron engine gateway
```

  Open `tools/dashboard/dist/rfq-dashboard.html`. Serve it on localhost if the browser pane can't open `file://`; the gateway's Origin allowlist accepts localhost. Then check:
  1. **Run** each scenario: 12 PASS.
  2. **Run all**: 12 PASS.
  3. **Run ×20** at concurrency 4: 20 PASS, with a histogram.
  4. Scrub `happy_path` back to the `RequestPresented` batch: the request lane shows Presented and the D1 diagram lights Presented.
  5. View as MM1, then run `happy_path`: no Alice data and no MM2 balances in the DOM.
  6. `docker compose kill engine` during a Run ×20: the status shows down; the in-flight command shows "outcome unknown", then its safe retry with the same nonce ("applied once") once the engine is back; runs not yet started are refused with "engine unavailable"; nothing hangs. `docker compose start engine`: a Resume row appears, status up, and the next Run passes.
  7. No console errors.

  Take screenshots at 1440×900, both themes.
- [ ] **Step 4:** Commit: `feat(dashboard): live mode — runs, timeline, perf, drills and view-as over the gateway`, staging `tools/dashboard/web/*` (never `dist/`). Run `docker compose down -v` afterwards.
