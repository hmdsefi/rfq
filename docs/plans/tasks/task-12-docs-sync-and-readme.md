# Task 12: Docs sync and README

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Files:**
- Create: `README.md`
- Modify: `docs/specs/failure-modes.md` (test names), `docs/README.md`

- [ ] **Step 1: Check that the failure-mode notes match the code**

Run:
```bash
# The Test column (5th '|' field) of the R1–R8 race table.
grep -E '^\| R[0-9]' docs/specs/failure-modes.md | awk -F'|' '{print $5}' | grep -oE '[a-z_]+' | sort -u | while read t; do
  grep -rq "fn $t" crates || echo "missing test: $t"
done
```
Expected: no output. Rename the tests or update the doc until nothing is missing. The reviewers will compare this list against the code.

- [ ] **Step 2: Write the README**

`README.md`:
````markdown
# RFQ Matching & Settlement Engine

Deterministic RFQ engine for binary-outcome contracts. Design docs: [docs/README.md](docs/README.md).

## Run the tests (no Docker needed)
```bash
cargo test
```

## Run the end-to-end demo (Docker)
```bash
./scripts/demo.sh
```
It runs the happy path and five failure paths over Aeron, then kills the engine and checks that replaying the Aeron Archive log restores the identical state hash.

## Layout
- `crates/rfq-core`: the engine (pure; all money logic and invariants)
- `crates/rfq-node`: sequencer, journals (Aeron Archive / file / memory), Aeron transport
- `crates/rfq-demo`: scenario client over Aeron
````

- [ ] **Step 3: Final verification**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && ./scripts/demo.sh`
Expected: all tests pass, clippy is clean, six PASS lines, `RECOVERY OK`.

- [ ] **Step 4: Commit**

```bash
git add README.md docs
git commit -m "docs: README and failure-mode notes synced with tests"
git push
```

