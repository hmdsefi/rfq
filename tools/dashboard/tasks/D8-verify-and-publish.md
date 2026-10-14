# Task D8: Verify, review, publish

**Owner:** the controller. **Goal:** prove every acceptance criterion in DESIGN.md §8, get one whole-branch review, publish the page as a **private** artifact, and push.

**Files:**
- Modify: `tools/dashboard/PLAN.md` (Status table), and `tools/dashboard/DESIGN.md` only if the review requires it.

## Steps

- [ ] **Step 1: Gates.**

Run: `cd tools/dashboard && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check && ./build.sh`
Expected: all tests pass, the lints are clean, 12 smoke PASS lines, and the page is written.

Run: `git diff --stat main@{u} -- crates Cargo.toml Cargo.lock deny.toml rust-toolchain.toml`
Expected: empty. This is DESIGN.md §8, item 6.

Run, from the repo root: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: green, exactly as before the dashboard.

- [ ] **Step 2: Acceptance walkthrough** in the built-in browser on `dist/rfq-dashboard.html`. Record the result of each item:
  1. All 6 brief presets show PASS, and all 6 attack presets show PASS with their pinned rejection reasons.
  2. From a mid-preset step, take over and do quote, cancel, request, accept, reject, propose, dispute, rule, +1 s and next timer. Each is either applied or answered with a visible engine rejection.
  3. View as Alice, Carol, MM1 and MM2 each hides exactly what the audiences hide: no other party's quotes, limits or balances.
  4. The ledger total changes only on Deposit; I1–I5 show OK after every command.
  5. The replay check passes after every preset and after manual play.
  6. The `crates/` diff is empty (Step 1).
  7. The page shows "Demo aid — not part of the engine", and the multi-leg tab says "Designed, not built (P6)".
- [ ] **Step 3: Whole-branch review.** Dispatch one reviewer on the most capable model with DESIGN.md, PLAN.md and `git diff <base>..HEAD -- tools/dashboard`. The reviewer looks for:
  - correctness of the view filter (sealed data leaking to the wrong viewer is a blocker);
  - any panic path in `api::handle`;
  - the page doing money arithmetic beyond display;
  - copy that overstates what is built.

  Fix the findings in one wave, re-run Steps 1–2, and record the review in the ledger.
- [ ] **Step 4: Publish a private artifact.** Load the `artifact-design` skill and run the Artifact `quickstart` first, as the Artifact tool requires. Publish `dist/rfq-dashboard.html`. Open the published page and confirm the engine boots there (the health chip shows a seq and hash).
  - If the host's content security policy blocks WebAssembly, don't work around it. Tell the PO, and the fallback is screen-sharing the local file, which needs no host.
  - Give the PO the link. It stays private; sharing it is the PO's call.
- [ ] **Step 5: Update the status and push.** Set every row in PLAN.md's Status table to done with its commit, then:

```bash
git add tools/dashboard/PLAN.md
git commit -m "docs(dashboard): plan status"
git push origin main
```

- [ ] **Step 6: Report to the PO:** the artifact link, the local fallback command (`tools/dashboard/build.sh && open tools/dashboard/dist/rfq-dashboard.html`), and a 30-second "how to drive it in the session" list (which preset to play when they ask X).
