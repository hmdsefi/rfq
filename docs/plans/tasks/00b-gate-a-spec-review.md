# Gate A: Adversarial spec review (before Task 1)

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Goal:** find money-state bugs while they're still a one-line spec fix. A fresh reviewer attacks the design and doesn't edit any code.

**Files:**
- Create: `docs/reviews/spec-review.md`
- Modify: whichever of `docs/specs/*.md` and this plan the accepted findings touch

- [ ] **Step 1: Dispatch a fresh reviewer agent with this prompt**

```text
You are an adversarial reviewer for an RFQ settlement engine where real money sits in every
intermediate state and every participant is hostile. Read docs/RFQ_Technical_Exercise.pdf,
docs/rfc/0001-rfq-engine.md, docs/specs/engine.md, docs/specs/failure-modes.md and
docs/specs/resolution.md. Do not read or write code.

Attack the design as each of: a malicious requester, a malicious market maker, a colluding
requester+MM pair, a malicious oracle, a griefing third party, and a crashing node. For every
state and transition in engine.md §6, ask:
  1. Can funds be lost, duplicated, or stuck with no exit (check invariants I1–I5)?
  2. Can an actor extract value or impose cost on others more cheaply than it costs them?
  3. Is any transition reachable by the wrong actor, or reachable twice?
  4. Does any timer ordering (same timestamp, idle engine, replay) change an outcome?
  5. Is any requirement in the PDF unaddressed?

Write findings to docs/reviews/spec-review.md as a table:
| # | Severity (critical/high/medium/low) | Attacker | Scenario (concrete steps) | Violated property | Proposed spec fix |
Only include findings with a concrete step-by-step scenario. End with "No findings" sections
for anything you checked and found sound, so coverage is visible.
```

- [ ] **Step 2: Triage with the user**

For each finding, decide with the user: **fix** (edit the spec, add the test to the owning task in this plan, and add the defence to `failure-modes.md`), **accept** (record it as a residual risk in `failure-modes.md` → Adversarial table), or **reject** (record the reason in the review file).

- [ ] **Step 3: Commit and push**

```bash
git add docs
git commit -m "docs: adversarial spec review and resulting spec fixes"
git push
```

