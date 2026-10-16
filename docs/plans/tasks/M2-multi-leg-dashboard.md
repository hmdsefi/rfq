# Task M2: Multi-leg on the dashboard

**Spec:** [engine.md §8](../../specs/engine.md), [tools/dashboard/DESIGN.md §11](../../../tools/dashboard/DESIGN.md). **Depends on:**
- M1's two scenarios in `rfq_testkit::scenarios::all()`: `multi_leg_happy` and `multi_leg_leg2_fails`;
- `Ctx::market_n(k) = t0 * 10 + k`;
- rfq-demo scope owning those ids.

**Goal:** both multi-leg stories run on the live dashboard through the real stack, and the page renders requests with several legs, several picks and several positions.

## Requirements

1. **Catalogue and counts.** Through `tools/dashboard/src/presets.rs` the Rust catalogue now has 8 brief scenarios and 6 attacks, 14 in total. Update every count and name list:
   - the presets tests;
   - the api tests (the catalogue is the sandbox plus 14);
   - `web/smoke.mjs`;
   - `web/live-smoke.mjs`;
   - the scenario parity test;
   - the page tests;
   - the README and DESIGN texts that say 12.
2. **JS scripts.** Add `multi_leg_happy` and `multi_leg_leg2_fails` to `web/scenarios.js`, step for step identical to the Rust ones: actors, offsets, actions, legs, `expect`. Add `ctx.marketN(k)`. `scenarios.test.mjs` parity must cover them. Change something on purpose to check the test fails, then restore it.
3. **Scope** (`web/recorder.js` `Scope`): own `t0 * 10 + k` markets (k = 1..=9), with a test.
4. **Rendering:**
   - A request card lists every leg: market name, side, qty, and the limit for its own requester only.
   - A `Presented` request lists every pick, per leg.
   - A `Filled` request lists every position.
   - A quote shows its leg ("leg 2 of 3").
   - The market area shows every market the run listed.
   - An `Expired` multi-leg request makes clear in its card and trace that no leg was filled and every hold was released. This comes from the events and balances alone; the page computes nothing.
5. **The D1 multi-leg tab:**
   - Replace "Designed, not built (P6)" with "Built", with **Run** buttons for both scenarios, gated on the catalogue as today.
   - Keep the three-step walkthrough. Map step 1–2 to `multi_leg_leg2_fails` and note that step 3, the alternative path, is pinned by the engine test `a_selected_leg_cannot_be_withdrawn`.
   - Update DESIGN.md's required-copy line and §4/§5/§11 text to match.
6. **Sealing:** nothing new reaches a non-admin view. Extend `page.test.mjs` with a multi-leg run viewed as MM2: no MM1 legs' prices, no Alice limits.

## Steps

- [ ] Write the failing tests first: parity of the two new scripts, scope ownership, model states for a 3-leg request (`Presented` with 3 picks, `Filled` with 3 positions, `Expired` with every balance back), and page sealing.
- [ ] Implement until they pass. Run `tools/dashboard/build.sh`: all suites green.
- [ ] Run `cargo test` and clippy in `tools/dashboard`: green.
- [ ] **Live:** run the commands below. Expected: 14 PASS, then 20/20.

```bash
docker compose down -v
docker compose up -d --build aeron engine gateway
node tools/dashboard/web/live-smoke.mjs
```

- [ ] **Browser check** (serve `dist/` on localhost, in your own tab):
  - run `multi_leg_leg2_fails`: Expired, both quotes Lost, every hold back, no positions;
  - run `multi_leg_happy`: 3 picks, then 3 positions;
  - view as MM2 during a multi-leg run: sealed;
  - console clean;
  - take one screenshot of each scenario at 1440×900.

  Afterwards stop the server, close your tab, reset the viewport and leave the stack down.
- [ ] **Commit:** `feat(dashboard): multi-leg scenarios and rendering`, staging explicit paths (`tools/dashboard/**` except `dist/` and `target/`). Do not push.
