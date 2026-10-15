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
  1. `docker compose kill engine` mid-run, then `docker compose start engine`: down (when nothing is being sent, it can take up to about 10 s to be detected) → the in-flight command gets `outcome unknown … nonce N`, and the gateway holds that participant's other commands until N settles (at most 60 s, gateway.md §6) → Resume → up → the driver resends with nonce N and gets `Duplicate` (logged `nonce N taken once (Duplicate on retry); original: applied`, `original: rejected: <Reason>` or `original not heard — whether it was applied is unknown`) or its own answer (`retry applied` or `retry rejected: <Reason>`) → the next runs of the set, which waited for the engine (up to 60 s), pass; a run that was in flight may still fail on timing. `docker compose logs engine` shows `replayed seq=… hash=…`.
  2. `docker compose pause engine; sleep 2; docker compose unpause engine`: a Resume batch; open dispute windows are extended, so runs in flight may FAIL on timing (correct engine behaviour); later runs continue.
  3. `docker compose kill aeron`: Docker does not restart a container that was killed by hand, and the engine and gateway exit 1 and cannot rejoin its dead IPC namespace. `docker compose up -d aeron engine gateway` brings all three back: the engine replays its journal (same hash), the page reconnects without a reload, and runs pass afterwards. The self-healing variant, `docker compose exec aeron kill -TERM 1`, makes the driver exit by itself: Docker restarts it in about 1 s, and the engine and gateway follow.
- [ ] **Step 3: Docs.**
  - README "Live dashboard" section: start the stack, open the page, run scenarios, the failure drills, what each panel shows. Five to ten lines, linking DESIGN §11 and gateway.md.
  - docs/README.md index: add gateway.md.
  - architecture/overview.md: add the gateway to the runtime diagram (one node, two arrows) and §5 deployment (service `gateway`, port `127.0.0.1:18080`). Keep D3 and D5 within their word budgets (don't touch them unless needed).
- [ ] **Step 4: Final review.** One whole-branch review on the most capable model, over this plan's range, pointed at the ledger's deferred minors. Fix them in one wave and run one scoped re-review.
- [ ] **Step 5: Push.** Update this plan's Status table and give the PO a run sheet:
  - which scenario to run for which question;
  - which drill to run;
  - the exact commands, in order.
