# Task 14: Final review and publish

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Files:**
- Create: `docs/reviews/code-review.md`
- Modify: whatever the confirmed findings touch

- [ ] **Step 1: Whole-branch code review**

Run the `code-review` skill at `high` effort against all commits since the Setup commit:
```text
/code-review high <setup-commit-sha>..HEAD
```
Get the sha from `git log --reverse --format=%h | head -1`.

- [ ] **Step 2: Security review**

Run the `security-review` skill over the same range. Ask it to focus on: money paths in `rfq-core` (ledger, accept, finalize), untrusted input on ingress (codec limits, `ClientCommand`), journal and replay integrity, and the Docker/Aeron setup (shared `/dev/shm`, exposed ports).

- [ ] **Step 3: Triage and fix**

Record every finding in `docs/reviews/code-review.md` (`# | source | severity | finding | decision | commit`). For each confirmed bug: write the failing test first, fix it, and add the defence to `docs/specs/failure-modes.md` if it's a race or partial failure. Re-run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo deny check && ./scripts/demo.sh`.

```bash
git add -A && git commit -m "fix: address final code and security review findings" && git push
gh run watch --exit-status
```

- [ ] **Step 4: Share the repo with the reviewers**

**Confirm with the user first**, and get the reviewers' GitHub usernames. Keep the repo private: it contains the exercise brief PDF, which belongs to Outcome Labs.
```bash
for u in <reviewer-1> <reviewer-2>; do
  gh api -X PUT "repos/hmdsefi/rfq/collaborators/$u" -f permission=pull
done
gh api repos/hmdsefi/rfq/invitations --jq '.[].invitee.login'
```
Expected: every reviewer is listed as invited. If they ever ask for a public repo instead, run `git rm docs/RFQ_Technical_Exercise.pdf` and push before changing visibility.

