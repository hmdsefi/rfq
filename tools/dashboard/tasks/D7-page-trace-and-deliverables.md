# Task D7: Page — admin trace, inspector, ledger, replay, D1–D5 panels

**Blocked until** D6 is done and D1 is approved. The visual source is `web/mockup.html`.

**Goal:** fill in the four render stubs D6 left (`renderTrace`, `renderLedger`, `renderInspector`, `renderPanels`) so an admin can trace everything. Add the deliverable panels that tie the demo to D1–D5.

**Files:**
- Modify: `tools/dashboard/web/index.html`
- Read (for panel content, transcribed into the page):
  - `docs/specs/engine.md` §6 (state machines) and §8 (multi-leg table);
  - `docs/specs/failure-modes.md` (R1–R9, F1–F15 with test names);
  - `docs/specs/resolution.md`;
  - `docs/specs/quote-lifetime.md`.

**Interfaces:**
- Consumes, from D6: `state`, `prev`, `presets`, `selected`, `send`, `select`, `fmtUsd`, `fmtPrice`, `fmtT`, `name`, `variant`.
- Produces: the finished page.

## Requirements

1. **Command log** (`renderTrace`): one row per `state.trace` entry, newest at the bottom, and the view auto-scrolls to new rows unless the presenter scrolled up.
   - **Row content:** `seq`, `fmtT(time)`, the actor name (or "system" or "hidden"), `nonce`, and `kind`.
   - **Command summary** from `command` when present, e.g. "MM2 SubmitQuote · req #6 · 0.60 × 1000 · exp t+10.000 s".
   - **Highlighting.** Rows whose `seq` is in `state.new_seqs` are highlighted (the mockup's "new" style) until the next response.
   - **Expanding a row** shows its events. Each event shows its variant name, its fields in short form, and an **audience badge**: "Public", or "→ Alice" for `{Account: 10}`.
   - **Rejections.** `CommandRejected` events show the reason in the danger style.
   - **Filter chips:** All · Rejections · Money (Deposited / Withdrawn / QuoteFilled / PositionSettled / DisputeBondReturned) · Clock (tick / resume).
2. **Object inspector** (`renderInspector`): when `selected` is set, show:
   - the object's current state from the snapshot (request, quote or market);
   - its **history**: every trace entry whose command or events reference the id. Match numeric fields named `request`, `quote`, `market`, `old`, `new` or `id` that equal the id, searching the event and command objects recursively. Ids are command seqs, so they never collide across kinds; market ids are `1000`.
   - Clicking an id inside the inspector re-selects. Show an empty-state hint when nothing is selected: "Click any request, quote or market id to trace it".
3. **Ledger** (`renderLedger`): a table of `snapshot.accounts` with name, role, free, reserved, escrowed and total.
   - Show a Δ per cell against `prev.snapshot` for the same account id, when `prev` exists and the mode is unchanged. Positive and negative deltas are styled differently.
   - Footer: **ledger total** (`snapshot.total`, admin only), with the note "changes only on Deposit/Withdraw (I1)".
   - The I1–I5 status line comes from `snapshot.invariants`.
   - **Replay check** button: sends `replay_check` and shows "Replayed N commands → hash a1b2c3d4 = live a1b2c3d4 ✓", or a red mismatch line. The result clears on the next command.
4. **D1 panel: state machines.**
   - Three diagrams (Request, Quote, Market) as inline SVG, transcribed from engine.md §6.1–6.3: every state, every edge, and on each edge the trigger and who can fire it (requester, MM, clock, oracle, anyone + bond, arbiter, system).
   - When `selected` is set, its diagram highlights the object's current state, plus the edge it took last (from the inspector history's last state-changing event).
   - A tab **"Multi-leg — Designed, not built (P6)"** shows engine.md §8's "leg 2 of 3" table as a three-step walkthrough (before the deadline → the deadline fires → the alternative path), and the five atomicity rules in one line each. The tab title includes the exact text "Designed, not built (P6)".
5. **D3 panel: failure modes.** A compact table transcribed from failure-modes.md: id (R1–R9, F1–F15), the short race or failure, how it's handled, and the test name.
   - Rows that a preset demonstrates have a **Run** button that resets to that preset. The mapping:
     - double accept → `attack_double_accept`;
     - MM withdraws after selection → `mm_cannot_withdraw_after_selection`;
     - no quotes → `no_quotes_expire`;
     - requester lapses → `requester_lapses`;
     - oracle silent → `oracle_silent_hard_deadline`;
     - dispute → `disputed_ruled_invalid`;
     - nonce replay → `attack_replayed_nonce`;
     - accept echo / price mismatch → `attack_accept_wrong_price`;
     - add any other row whose test matches a preset's behaviour.
   - Under the table: "Full notes: docs/specs/failure-modes.md · Attacker table: docs/specs/adversarial.md".
6. **D4 panel: resolution.** Four paths as a small table:
   - undisputed (proposal → window ends → Final);
   - disputed and ruled (bond; arbiter);
   - oracle silent (hard deadline → INVALID, refunds);
   - unruled dispute (hard deadline → proposed outcome, bond returned).

   Each path has a **Run** button to its preset where one exists, and one line on how funds unlock (positions settle in the same command as Final).
7. **D5 panel: quote lifetime.**
   - Read the `Genesis` entry (`state.trace[0].command.Genesis.config`) and show `reservation` and `selection` as live values. They come from the log, not hard-coded.
   - Underneath: the "invariant to seconds vs days" list and the "not invariant" list (three to five bullets each, from quote-lifetime.md), and what was built: the two enums and the files they live in.
8. **Copy** stays short and plain. Every number comes from `state`, and the panels never invent engine data.

## Steps

- [ ] **Step 1:** Implement `renderTrace` with expandable rows, audience badges and filters. Rebuild, open the page, play `happy_path`, and check the rows and audiences against `javascript_tool` → `state.trace`.
- [ ] **Step 2:** Implement `select` and `renderInspector`. Check: clicking the request id in Alice's card shows SubmitRequest, the deadline tick (RequestPresented), Accept and RequestFilled.
- [ ] **Step 3:** Implement `renderLedger` with deltas and the replay check. Check: after Accept in `happy_path`, Alice's escrowed Δ is `+$600.00` and MM2's is `+$400.00`; the replay check shows ✓.
- [ ] **Step 4:** Implement `renderPanels`: the D1 SVGs with highlighting and the multi-leg tab, the D3 table with Run buttons, D4 and D5.
- [ ] **Step 5: Browser checks** at 1440×900 and 1920×1080, both themes, with screenshots compared against the mockup:
  1. Every preset run from the picker ends with PASS.
  2. View as Alice: no `QuoteSubmitted` rows and no MM balances; the trace shows "hidden" rows only where she sees an event.
  3. In `attack_replayed_nonce`, the trace shows Duplicate and NonceGap in the danger style, and MM1's nonce column reads 1, (1), (3), 2.
  4. The multi-leg tab shows "Designed, not built (P6)".
  5. The D5 panel reads `FullHold` and `SealedAtDeadline` from the log.
  6. There are no console errors after running all twelve presets back to back.
- [ ] **Step 6: Commit.**

```bash
git add tools/dashboard/web/index.html
git commit -m "feat(dashboard): admin trace, object inspector, ledger with deltas, replay check, D1–D5 panels"
```
