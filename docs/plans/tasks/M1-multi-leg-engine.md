# Task M1: Multi-leg in the engine, scenarios, demo scope, docs

**Spec:** [engine.md §8](../../specs/engine.md). Plan constraints: [the plan](../2026-10-16-multi-leg-plan.md).

**Files:**
- Modify:
  - `crates/rfq-domain/src/engine/rfq.rs` (remove the guard at ~:18; leg cap; accept re-check);
  - `crates/rfq-domain/src/command.rs` (or wherever constants live: `pub const MAX_LEGS: usize = 8;`);
  - `crates/rfq-testkit/src/scenarios.rs` (`Ctx::market_n`, the two scenarios in `all()`);
  - `crates/rfq-demo/src/scope.rs` (own `t0 * 10 + k` markets);
  - every count of scenarios (the brief's list of 6 becomes 8) in tests, `scripts/demo.sh` comments, the README and docs.
- Docs:
  - `docs/specs/engine.md` §8: built; rules restated; leg cap; the "leg 2 of 3" table now names the scenario and its test;
  - `docs/rfc/0001-rfq-engine.md` P6: note the 2026-10-16 decision;
  - `README.md` (the "designed but not built" sentence);
  - `docs/specs/failure-modes.md`, the multi-leg paragraph. It must stay at most about 530 words excluding table syntax: replace "(designed, not built)" with the tests and cut an equal number of words.
- Tests:
  - `crates/rfq-domain/tests/multi_leg.rs` (new);
  - the scenario tests in `crates/rfq-domain/tests/scenarios.rs`.

## Requirements

1. **`submit_request`:**
   - Remove `if legs.len() != 1 { Unsupported }`.
   - Reject `legs.is_empty() || legs.len() > MAX_LEGS` with `InvalidInput`.
   - Everything else already loops over all legs (trading-close guard, qty, dust floor, one hold for the total).
   - Check that `RequestOpened` is emitted once per leg, with its `leg` index. Fix it if it isn't.
2. **`accept`:** before any mutation, for every leg, the leg's market must still be `Trading`, else `InvalidState` (engine.md §8 rule 5, defence in depth). Everything else stays: picks validated for every leg first, then all escrows.
3. **Quotes** already carry `leg`. A quote for a leg index ≥ `legs.len()` must be `InvalidInput`; check it and add it if missing.
4. **Tests** in `multi_leg.rs`, using `Harness` (describe each in one comment line):
   - `three_legs_fill_in_one_accept`: three markets, a quote on every leg, the deadline gives `Presented` with 3 picks, Accept gives `RequestFilled` with 3 positions; each MM's escrow equals its legs' `seller_lock`, and the requester's equals the sum of `buyer_lock` at the picked prices.
   - `leg_two_of_three_unfilled_expires_the_whole_request_and_releases_every_hold`: legs 1 and 3 quoted, leg 2 not. At the deadline: `RequestExpired`, both quotes `Lost`, and in **that same command** the requester's and both MMs' reserved go to 0 and free goes back to the deposit. No positions exist.
   - `an_over_limit_quote_on_one_leg_expires_the_whole_request`: all legs quoted, one above its limit.
   - `a_selected_leg_cannot_be_withdrawn`: all three presented, then the leg-2 MM's `CancelQuote` is `InvalidState`, and Accept fills all three.
   - `accept_must_echo_every_leg`: missing, extra, reordered or wrong-price picks each give `Mismatch`.
   - `leg_cap`: 0 legs and `MAX_LEGS + 1` legs give `InvalidInput`; `MAX_LEGS` legs is accepted.
   - `a_quote_for_a_missing_leg_is_rejected`.
   - `accept_rechecks_every_leg_market_is_trading`. If the engine already makes this unreachable (`accept_by < trading_close` for every leg), write the test against a market forced out of `Trading` in a unit test inside the crate, or document why the re-check is defence in depth only.

   Also check I1–I5 after every command; `Harness` already does.
5. **Scenarios** (`rfq-testkit::scenarios::all()`; the two new ones come after the existing six):
   - **`Ctx::market_n(k: u64) -> MarketId`** is `MarketId(t0 * 10 + k)`. Document why: distinct runs never collide, and the value stays JS-safe.
   - **Shared setup.** Use the existing deposits ($1,000 each for Alice, MM1, MM2, Carol) plus three `ListMarket`s by OPERATOR:
     - `market_n(1)` "Will it rain in Paris on 2026-11-01?";
     - `market_n(2)` "…London…";
     - `market_n(3)` "…Rome…";
     - each closes at +5 s, hard deadline +15 s, dispute window 2 s.
   - **The request** (Alice at +100): three legs `BuyYes` **400** shares, limit 7000 bps each, deadline +1.5 s. That's 3 × 400 × 0.70 = $840 held, within her $1,000.
   - **`multi_leg_happy`:**
     - MM1 quotes leg 0 at 6200 (+300);
     - MM2 quotes leg 1 at 6000 (+400);
     - MM1 quotes leg 2 at 6400 (+500);
     - expiries +10 s;
     - tick +1600;
     - Alice accepts the presented picks (+2000).
     - `expect`: `RequestOpened` ×3, `QuoteSubmitted` ×3, `QuoteSelected` ×3, `RequestPresented`, `QuoteFilled` ×3, `RequestFilled`.
   - **`multi_leg_leg2_fails`:**
     - MM1 quotes leg 0 at 6200 (+300);
     - MM2 quotes leg 2 at 6000 (+400);
     - nobody quotes leg 1;
     - tick +1600.
     - `expect`: `RequestOpened` ×3, `QuoteSubmitted` ×2, `QuoteLost` ×2, `RequestExpired`.
   - Write a one-line summary for each, as the existing scenarios have.
   - Fix any `expect` order to what the engine emits, with a note. Never weaken an expectation, and verify every `expect` against the engine before writing it down.
6. **rfq-demo scope:** `Scope::owns` also owns events naming `MarketId(key * 10 + k)` for k in 1..=9. Unit test it.
7. **Counts.** Find every place that hard-codes 6 brief scenarios (`rfq-domain/tests/scenarios.rs`, rfq-demo, `scripts/demo.sh` text, the README, docs) and update it to 8. Do **not** touch `tools/dashboard`; that's M2's.
8. **Docs** as listed under Files. engine.md §8 says built, lists the leg cap, and links the two scenarios and the `multi_leg.rs` tests. D3 stays within its word budget.

## Gates

- `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check && cargo deny check`.
- **State-hash identity.** For the six original scenarios, the final `state_hash` is byte-identical to before the change. Capture it before your first edit with a small throwaway test or binary, and compare at the end.
- `docker compose build engine demo`, then `./scripts/demo.sh` gives **8 PASS** and `RECOVERY OK`, then `docker compose --profile demo down -v`.
- **Commit:**
  - `feat(domain): multi-leg requests (engine.md §8)`
  - `feat(testkit): multi-leg scenarios`
  - `feat(demo): run scope owns a scenario's extra markets`
  - `docs: multi-leg is built`

  Do not push.
