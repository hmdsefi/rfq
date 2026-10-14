# Task L6: End to end (failure drills, docs, final review, push)

**Owner:** the controller. **Goal:** prove gateway.md §10 and DESIGN §11.3 on the real stack, document how to run it, get one whole-branch review, and push.

## Steps

- [ ] **Step 1: Gates from a clean stack.**
  - Workspace (aeron off): `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`, `cargo deny check`.
  - `docker compose build`.
  - `./scripts/demo.sh`: 6 PASS and `RECOVERY OK`.
  - `docker compose down -v && docker compose up -d aeron engine gateway`.
  - `node scripts/gateway-smoke.mjs`: `GATEWAY SMOKE OK`.
  - `node tools/dashboard/web/live-smoke.mjs`: 12 PASS, then 20/20.
- [ ] **Step 2: Failure drills.** Run each one, and record the evidence (gateway frames, page screenshot, engine logs):
  1. `docker compose kill engine` mid-run, then `docker compose start engine`: down → engine unavailable → Resume → up → the next run passes. `docker compose logs engine` shows `replayed seq=… hash=…`.
  2. `docker compose pause engine; sleep 2; docker compose unpause engine`: a Resume batch, and runs continue.
  3. `docker compose kill aeron`: the engine and gateway exit 1 and are restarted by Docker (cap 3); the page reconnects; runs pass afterwards.
- [ ] **Step 3: Docs.**
  - README "Live dashboard" section: start the stack, open the page, run scenarios, the failure drills, what each panel shows. Five to ten lines, linking DESIGN §11 and gateway.md.
  - docs/README.md index: add gateway.md.
  - architecture/overview.md: add the gateway to the runtime diagram (one node, two arrows) and §5 deployment (service `gateway`, port `127.0.0.1:8080`). Keep D3 and D5 within their word budgets (don't touch them unless needed).
- [ ] **Step 4: Final review.** One whole-branch review on the most capable model, over this plan's range, pointed at the ledger's deferred minors. Fix them in one wave and run one scoped re-review.
- [ ] **Step 5: Push.** Update this plan's Status table and give the PO a run sheet:
  - which scenario to run for which question;
  - which drill to run;
  - the exact commands, in order.
