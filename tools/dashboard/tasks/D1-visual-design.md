# Task D1: Visual design and mockup (PO approval gate)

**Owner:** the controller, working with the PO. **Gate:** the PO explicitly approves the mockup. D6 and D7 build from it and may not start before that approval.

**Files:**
- Create: `tools/dashboard/web/mockup.html`, a static, self-contained mockup of every panel and state, filled with realistic sample data that looks like `happy_path` partway through. No engine and no network.
- Modify: `tools/dashboard/DESIGN.md`. Append **§10 Visual design**: style, color tokens, type scale, spacing, component inventory, states, and the layout at 1280, 1440 and 1920 px.

**Interfaces:**
- Consumes: DESIGN.md §3–§5 (views, layout, presets).
- Produces, for D6 and D7:
  - the class names and markup structure of every component;
  - the CSS custom properties (tokens) on `:root`, including dark mode;
  - the icon set, inlined as SVG symbols.

  D6 and D7 copy the mockup's `<style>` block verbatim into `web/index.html` and render the same markup from engine state.

## Steps

- [ ] **Step 1: Load the design skills.** Invoke `ui-ux-pro-max` (design-system generator: product type "fintech admin dashboard / trading ops console", plus the UX guideline search for data-dense dashboards, tables, state badges and accessibility). Also invoke `frontend-design` for the aesthetic direction. Record the chosen style, palette, type pairing and the main UX rules in DESIGN.md §10.
- [ ] **Step 2: Fix the design constraints.**
  - Projector-readable: body ≥ 14 px, data ≥ 13 px, contrast ≥ 4.5:1 (WCAG AA) in both themes.
  - System font stack plus a monospace face for ids, amounts and hashes; no web fonts.
  - Tabular numbers for money.
  - State badges colour-coded by meaning (live, terminal-good, terminal-released, rejected), never by sequence.
  - Light and dark themes through `prefers-color-scheme` plus a manual toggle.
  - Laid out for 1280–1920 px wide. At 375 px it only has to not overflow horizontally.
- [ ] **Step 3: Build `web/mockup.html`.** Show all of these:
  - the top bar: preset picker, step, play, reset, take-over, the clock buttons, view-as, and the engine health chip (seq, t+, hash, I1–I5);
  - the market makers lane: two MM cards, an open request with the quote form, quotes in Active, Selected and Lost;
  - the users lane: Alice with a presented request and Accept/Reject, Carol idle, and the new-request form;
  - the contract card: the resolution timeline with the now cursor, oracle and arbiter actions, and positions;
  - the admin trace: the command log with one row expanded to its events and audience badges, the object inspector, the ledger table with deltas and total, and the replay check result;
  - the deliverable panels: D1 state machines (selected object lit) with the multi-leg tab labelled "Designed, not built (P6)", D3, D4 and D5;
  - the label "Demo aid — not part of the engine";
  - the "view as MM1" variant (a second mockup section or a toggle) showing what is redacted.
- [ ] **Step 4: Check it in the built-in browser** at 1440×900 and 1920×1080, in both themes. Take screenshots and fix overlap, overflow and contrast issues.
- [ ] **Step 5: Present it to the PO.** Publish a private artifact preview, or show it in the browser pane, with a 5-line summary of the design decisions. **Stop and wait for approval.** Apply requested changes and repeat Steps 4–5.
- [ ] **Step 6: On approval, commit.**

```bash
git add tools/dashboard/web/mockup.html tools/dashboard/DESIGN.md
git commit -m "docs(dashboard): visual design and mockup, approved by PO"
```
