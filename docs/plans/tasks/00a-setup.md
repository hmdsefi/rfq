# Setup: Git repository (before any task)

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Files:**
- Create: `.gitignore`

- [ ] **Step 1: Initialise the repo and commit the design docs**

```bash
cd ~/projects/rfq
git init -b main
printf 'target/\n.DS_Store\n.helper/\n' > .gitignore   # .helper/ holds local Claude skills; never pushed
git add .gitignore docs
git commit -m "docs: RFC, architecture, specs and implementation plan"
```
Expected: `git log --oneline` shows one commit, and `git status` doesn't list `.helper/`.

- [ ] **Step 2: Create the private GitHub repo and push**

**Confirm with the user before running this step**, because it publishes to GitHub. The repo is **private**; it's shared with the reviewers only in Task 14.
```bash
gh repo create hmdsefi/rfq --private --source . --remote origin --push \
  --description "RFQ matching & settlement engine for binary-outcome markets (take-home)"
gh repo view hmdsefi/rfq --json visibility --jq .visibility
```
Expected: `PRIVATE`.

