# RFQ Dashboard: design spec

Status: approved by the PO on 2026-10-14 (spec, plan and the D1 mockup).

> **A demo aid for the live session. It is not part of the engine.**
> The brief puts UI out of scope. This tool sits outside the cargo workspace and changes nothing under `crates/`. It runs the real `rfq-domain` engine, compiled to WebAssembly, so the reviewers can watch and drive the same code the tests and the Aeron node run.

## 1. Purpose

The page is used in the 30-minute defense to *trace, justify, attack and extend* the engine live:

- show market makers quoting, replacing and cancelling, users requesting, accepting and rejecting, and the match happening at the deadline;
- let the presenter act as any participant and try "what if…" moves on the spot;
- give an **admin trace** of everything: every command, every event (sealed ones too), every balance and hold, every state transition;
- cover the five deliverables (D1–D5) with real engine output. Multi-leg was a labelled design (P6) until the PO decided to build it on 2026-10-16; its two stories now run live (§11).

**Success** means three things:
- each of the 8 brief scenarios (six single-leg, two multi-leg) and the attack presets runs in one click with the expected events;
- the presenter can take over at any step;
- every number on screen comes from the engine, never from page logic.

## 2. Non-goals

- No server, no Aeron, no Docker: the Aeron path is already shown by `scripts/demo.sh`.
- No auth, accounts or persistence: a page reload starts fresh.
- No changes to `crates/` or to the workspace manifests.
- No form for composing a multi-leg request by hand. Multi-leg is built (engine.md §8) and shown by two scripted scenarios, `multi_leg_happy` and `multi_leg_leg2_fails`; the Multi-leg tab runs them.
- No mobile layout beyond "doesn't break": it is presented on a laptop or projector.

## 3. Users and views

There is one presenter. A **"view as"** switch changes who the page shows the world to:

| View | Sees |
|---|---|
| **Admin** (default) | Everything: all commands, all events whatever their audience, all balances, holds and states |
| **Alice / Carol** (requesters) | Public events plus `Account(self)` events. Their own requests. Never another party's quote prices |
| **MM1 / MM2** (market makers) | Public events plus their own. Open requests show contract, side, qty and deadline; never the limit or the requester |

The filter applies the engine's own `audience` field, which is how sealing works (engine.md §7). "View as Alice" is the live proof that quotes stay sealed.

For a viewer who isn't the owner:
- a request shows only its public fields (contract, side, qty, deadline) and a coarse state, `open` or `closed`;
- quotes, balances and positions show only the viewer's own;
- a position's counterparty stays hidden until the position settles, because the engine names both holders only in `PositionSettled`;
- contracts are public, except their list of positions, which shows only the viewer's own (fills are announced privately);
- the next pending timer is shown to admin only, because it can be a sealed quote's expiry;
- other parties' commands appear only through the events the viewer is allowed to see.

The chosen view stays in place across a reset.

## 4. Layout (the Task D1 design refines it)

1. **Top bar.** Preset picker, the step controls (step, play/pause, reset, "take over"), the simulated clock (`+100 ms`, `+1 s`, `next timer →`), the view-as switch, and the engine health readout: seq, `now`, short state hash, I1–I5 status.
2. **Market makers lane.** One card per MM.
   - Open requests they can quote on.
   - Their quotes with state badges (Active, Selected, Lost, Cancelled, Expired, Released, Filled, Replaced) and the collateral each one holds.
   - Actions: **Quote** (price, size, expiry), **Replace** (submit again on the same leg), **Cancel**.
3. **Users lane.** One card per requester.
   - **New request** (contract, side, qty, limit, response deadline).
   - Their requests with state badges.
   - **Accept / Reject** on a presented request (accept echoes the presented picks; the attack presets show what happens otherwise).
   - **Dispute** on a proposed outcome.
4. **Contract card.** Contract state, the resolution timeline (trading close, proposal, dispute window, hard deadline, a "now" cursor), the oracle's **Propose** and the arbiter's **Rule**, and positions with payouts after settlement.
5. **Admin trace.**
   - (a) The command log: seq, time, actor, nonce and action, each expandable to its events with audience badges.
   - (b) An object inspector: click any request, quote or contract id anywhere to see its full history.
   - (c) The ledger table: free, reserved and escrowed per account, with deltas against the previous command, and the ledger total, which only `Deposit` and `Withdraw` change (I1).
   - (d) **Replay check**: rebuild from the command log in a fresh engine and compare state hashes.
6. **Deliverable panels** (secondary):
   - **D1** the request, quote and contract state machines, with the current state of the selected object lit and the last transition labelled with who triggered it; a *Multi-leg (built)* tab with a Run button for `multi_leg_leg2_fails` and one for `multi_leg_happy`, and the three-step walkthrough of engine.md §8's "leg 2 of 3" table: steps 1 and 2 are played by `multi_leg_leg2_fails`, and step 3 (the alternative path) is pinned by the engine test `a_selected_leg_cannot_be_withdrawn`;
   - **D3** failure modes R1–R9 and F1–F15 with their test names, each linked to the preset that demonstrates it where one exists;
   - **D4** resolution paths (undisputed, disputed then ruled, oracle silent, unruled dispute);
   - **D5** the two seams, read from this run's `Genesis` (`ReservationPolicy::FullHold`, `SelectionPolicy::SealedAtDeadline`), with invariant and non-invariant lists.

## 5. Presets

**Brief scenarios.** All eight come straight from `rfq_testkit::scenarios::all()`, the same scripts as the tests and the Aeron demo:
- `happy_path`
- `mm_cannot_withdraw_after_selection`
- `no_quotes_expire`
- `requester_lapses`
- `disputed_ruled_invalid`
- `oracle_silent_hard_deadline`
- `multi_leg_happy`: one request for three markets, a quote on every leg, and one accept fills all three legs
- `multi_leg_leg2_fails`: legs 1 and 3 are quoted, leg 2 is not, so at the deadline the whole request expires and every hold is released in that one command

The two multi-leg scenarios list three markets (Paris, London, Rome) as `ContractId(t0 * 10 + k)` for k = 1..=3 (`Ctx::contract_n`), and a run's scope owns those ids too.

When a preset finishes, the page runs `contains_in_order` against the scenario's `expect` list and shows PASS or FAIL.

**Attack presets.** These live in the dashboard crate and are built from the testkit's public `Step`/`Ctx` types. Each pins the engine's answer, taken from the existing tests:

| Preset | Attacker move | Engine answer |
|---|---|---|
| `attack_accept_wrong_price` | Alice accepts with a price other than the one presented | `CommandRejected(Mismatch)`; the presented quote stays firm |
| `attack_double_accept` | Alice accepts twice | the second is `CommandRejected(InvalidState)`; escrow happens once |
| `attack_replayed_nonce` | MM1 resends an already-used nonce, then skips one | `Duplicate`, then `NonceGap`; nothing consumed |
| `attack_over_limit_quote` | MM2 quotes above Alice's sealed limit, and it's the only quote | quote accepted as `Active` but never eligible: at the deadline it's `Lost` and the request `Expired`; every hold released, Alice never pays above her limit |
| `attack_self_trade` | Alice quotes her own request | `CommandRejected(SelfTrade)` |
| `attack_cancel_others_quote` | Carol cancels MM1's quote | `CommandRejected(Unauthorized)` |

**Sandbox.** The accounts are funded ($1,000 each) and one contract is listed with long windows (close at +60 s, hard deadline at +120 s, dispute window 5 s), so manual play has room. Otherwise it uses `demo_config()`.

**Take over.** At any point in a preset, the presenter can stop the script and continue by hand from the current state.

## 6. Architecture

```
tools/dashboard/
  DESIGN.md          this spec
  PLAN.md            roadmap and task index
  tasks/             one file per task
  Cargo.toml         own [workspace], so it is not a member of the root workspace
  src/lib.rs         the WASM shim: JSON request in, JSON response out
  src/session.rs     Session: engine, log, clock, per-actor nonces (like the testkit Harness, but it never panics)
  src/presets.rs     testkit scenarios and attack presets as steppable scripts
  src/view.rs        snapshot and trace DTOs (serde_json); audience filter
  web/index.html     page template (HTML, CSS, JS; no framework, no CDN at runtime)
  build.sh           cargo build --target wasm32-unknown-unknown, then base64-inline the .wasm into dist/rfq-dashboard.html
  dist/              built single file (gitignored)
```

**Dependencies.** Path dependencies on `rfq-domain` and `rfq-testkit`, plus `serde`/`serde_json`. No `wasm-bindgen`.

**ABI.** Three exports:
- `in_buf(len) -> ptr`;
- `handle() -> out_len`;
- `out_ptr() -> ptr`.

The buffers are `thread_local! RefCell<Vec<u8>>`. The crate has **no `unsafe` blocks**. The only items the `unsafe_code` lint flags are the `#[unsafe(no_mangle)]` export attributes, allowed in one `ffi` module. The engine crates keep `#![forbid(unsafe_code)]`.

**Requests** (`{"op": …}`):

| Op | Effect |
|---|---|
| `reset {mode}` | fresh `Session` in `sandbox` or a named preset |
| `presets` | names, descriptions, step counts, expectations |
| `step` | run the next scripted step of the loaded preset |
| `act {actor, action}` | a user command with the next nonce |
| `raw {actor, nonce, action}` | a user command with an explicit nonce (attacks) |
| `advance {ms}` | `Tick` at `now + ms` |
| `next_timer` | `Tick` at `engine.next_timer()` |
| `replay_check` | fresh engine over the log; returns both hashes |

**Responses.** Every mutating op returns:
- the appended trace entries (`seq`, `time`, command, events with audience);
- a full `snapshot`: accounts, requests, quotes, contracts, positions, `now`, `seq`, `state_hash` (hex), the `check_invariants()` result, `next_timer`;
- for presets, the step index and PASS or FAIL once finished.

Errors come back as `{"error": "..."}`, never as a trap.

**Money.** Amounts stay integer micro-USD and prices integer bps in JSON. The page only formats and diffs them for display. Every balance, hold, total and payout comes from the engine.

**Clock.** Time is data. The page owns a simulated clock and only moves it forward. Every command is stamped with it. Timers fire because the engine fires them before each command, so the page has no timer logic.

**Delivery.** One self-contained HTML file with the WASM inlined. It opens from disk, and is published as a **private** artifact for screen sharing.

## 7. Testing

- **Native unit tests** in the dashboard crate (`cargo test`, host target):
  - every brief preset run through `Session` passes its `expect` list;
  - every attack preset gets its pinned rejection reason;
  - `replay_check` hashes match after every preset;
  - `act` assigns nonces 1, 2, 3, … per actor;
  - `raw` with a used nonce gives `Duplicate`;
  - malformed JSON returns `{"error"}`;
  - `advance` never goes backwards.
- **Audience filter test:** in `happy_path`, the view as MM1 contains none of MM2's quote events, and the view as Alice contains no quote prices before `RequestPresented`.
- **Browser smoke test** (built-in browser on `dist/rfq-dashboard.html`): every preset runs to PASS, there are no console errors, and take-over plus one manual quote works.
- **Gates:** `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` in `tools/dashboard`. The root workspace gates (`cargo test --workspace`, clippy, deny) stay unchanged and green.

## 8. Acceptance criteria

1. All 8 brief presets show PASS. All 6 attack presets show their pinned answer.
2. From any preset step, take-over allows quote, cancel, request, accept, reject, propose, dispute, rule and clock moves.
3. View-as filtering hides sealed data exactly as the event audiences say.
4. The ledger total changes only on `Deposit` and `Withdraw`, and the I1–I5 status is shown after every command.
5. The replay check matches after every preset and after manual play.
6. `git diff --stat crates/ Cargo.toml Cargo.lock` is empty.
7. The page carries the label "Demo aid — not part of the engine", and the multi-leg tab says "Built" (it said "designed, not built (P6)" until multi-leg was built, 2026-10-16).

## 9. Risks

| Risk | Mitigation |
|---|---|
| The `wasm32-unknown-unknown` target is missing for toolchain 1.99.0 | `rustup target add wasm32-unknown-unknown --toolchain 1.99.0` (local, documented in `build.sh`) |
| A dependency doesn't build for wasm32 | The engine depends only on `serde`, `bincode` and `sha2`, all of which build for wasm32. Task D2 builds them first |
| The page is mistaken for scope creep | Kept outside the workspace and labelled. It adds zero lines to the engine |
| A live demo breaks mid-session | Deterministic engine, no network, a single file. Reset always works |
| The artifact host's content security policy blocks WebAssembly | Task D8 checks this. The fallback is opening `dist/rfq-dashboard.html` from disk, which needs no host |

## 10. Visual design (Task D1)

**Source of truth:** [`web/mockup.html`](web/mockup.html). D6 and D7 copy its `<style>` block verbatim and render the same markup from engine state.

**Direction.** A calm, light, ledger-like console that reads well on a projector, with a dark theme for screens (the "Theme" toggle). Minimal Swiss structure: flat surfaces, 1 px lines, 6 px radius, no shadows. These are the choices behind it:

- **Light by default.** The design-system search proposed a near-black background with an acid-green accent. That's the generic trading-terminal look, and dark themes wash out on projectors.
- **Money is the protagonist.** Every amount uses the same three bucket colours: **free** in slate, **reserved** in hatched amber (held, and can be released), **escrowed** in solid indigo (locked to the contract). The hatching keeps the buckets distinguishable without colour. The ledger's split bars animate their widths when money moves (320 ms, and disabled under `prefers-reduced-motion`). That is the page's one deliberate motion.
- **Two sides of the trade around the contract.**
  - The contract card holds the market question, its state, a resolution timeline with a "now" cursor, and the oracle and arbiter actions. It sits between **requesters** (left, green rule: they ask) and **market makers** (right, indigo rule: they answer with sealed quotes).
  - Below 1720 px the contract spans the full width above the two sides.
  - The admin **trace** is a sticky right sidebar (440 px) holding the command log and the object inspector. It stacks under the main column below 1100 px.
- **The clock is data.** Engine time `t+…` shows only in the top bar and the timeline, and it moves only when a command is stamped.
- **Sealed by default.** What a viewer may not see is replaced by an explicit lock and "sealed" marker, never left blank. Commands from other actors appear as "Public" events.

**Tokens** (on `:root`, with a `[data-theme="dark"]` override): `--page`, `--surface`, `--inset`, `--ink`, `--ink-2`, `--ink-3`, `--line`, `--line-2`, `--accent`, `--free`, `--reserved`, `--escrow`, `--yes`, `--no`, `--ok`, `--bad`, `--warn`, `--gone`, each with a `-tint`/`-ink` variant where needed, plus `--new` (row highlight) and `--focus`. The light ink colours on white are ≥ 4.5:1.

**Type.** The system UI sans everywhere, with tabular figures. Monospace is used only for hashes and test names. Sizes: 13 px minimum, 14 px body, 15 px party and section names, 21 px for the market question.

**Status badges** encode meaning, never sequence, and always carry their text:

| Class | Meaning | States |
|---|---|---|
| `b-live` | in progress | Open, Active, Presented, Trading, Proposed |
| `b-warn` | held firm or contested | Selected, Disputed, Taken over |
| `b-good` | done well | Filled, Final, invariants hold, PASS |
| `b-gone` | ended without a fill | Lost, Released, Expired, Cancelled, Replaced |
| `b-bad` | rejected or broken | Rejected, FAIL, I1–I5 broken |

**Components** (class names, for D6 and D7):
- `.topbar`, `.scenario` (with `.chip.seen` expectation chips), `.arena` (`.requesters`, `.contract-col`, `.makers`);
- `.party`, `.item`, `.bar` (`.seg.free/.reserved/.escrowed`), `.form-row`, `.field`, `.field-error`, `.toast` (auto-dismisses after 4 s), `.sealed`;
- `.contract` (`.contract-main`, `.contract-side`, `.timeline`, `.roles`, `.positions`), `.ledger` (with `tr.changed` and `.delta.up/.down`);
- `.trace` (`.log`, `.row` with `.is-new/.is-open/.is-rejected`, `.row-events`, `.aud`/`.aud-acct`, `.inspector`), `.tabs`/`.tabpanel`;
- `.machine` (SVG `.sm-node.is-current/.is-terminal`, `.sm-edge.is-last`, `.sm-label`).

**Interaction rules:**
- All controls are real buttons and inputs with visible labels and focus rings.
- Form errors appear inline under the field (`role="alert"`) and block sending.
- Engine rejections show as a toast in the acting card and as a red row in the trace; they're never an error banner.
- Ids are underlined buttons that select the object for the inspector and the D1 diagram.

## 11. Live mode: the dashboard drives the real stack (v2, approved by the PO, 2026-10-15)

**Decision (PO, 2026-10-15).** The dashboard is only the window; the product is what it shows. In live mode every command goes through the real stack and every number comes back from it:

```
dashboard → WebSocket → rfq-gateway → Aeron → rfq-node (Docker) → Aeron → rfq-gateway → WebSocket → dashboard
```

Nothing is replayed or recomputed in the page. The gateway is a product component, specified in [docs/specs/gateway.md](../../docs/specs/gateway.md).

The in-browser (WASM) engine was to stay only as an **offline fallback**, labelled "Offline: engine in the browser" (Task L7, optional). **Not built** (Task L7 was not done): the page needs the running stack.

### 11.1 What the presenter does

1. Start the stack (`docker compose up -d aeron engine gateway`) and open `dist/rfq-dashboard.html`. The status bar shows "Connected to the gateway, engine up".
2. Pick a scenario in the combo box (the 8 brief scenarios and 6 attacks) and press **Run**. The page then acts as every participant:
   - the operator funds the accounts and lists the contract;
   - Alice submits the request;
   - MM1 and MM2 quote;
   - Alice accepts (or doesn't);
   - the oracle proposes, Carol disputes, the arbiter rules, as the script says.

   Each step is a real command sent through the gateway. The page moves to the next step when the previous command's `result` arrives. A scripted wait (a deadline, an accept window) waits on the engine's clock, and the engine fires its own timers.
3. Batches stream in. The page shows each request, quote and contract moving through its D1 states (Open → Presented → Filled, Active → Selected → Filled, Trading → … → Final), the events with their audiences, and money moving between free, reserved and escrowed. Money comes from the engine's `balances` in each batch.
4. At the end: PASS or FAIL against the scenario's expected events (the same lists as the tests).
5. **Timeline.** The whole run is recorded. A scrubber steps through it batch by batch, even if it took milliseconds. Every panel shows the state at the scrubbed position.
6. **Runs.** **Run ×N**, **Run all**, and a concurrency setting for parallel runs. The Runs table shows each run's per-command latency (sent → result) as p50 / p95 / max. Runs are scoped like `rfq-demo` (per-run account ids = `t0 × 100 + id`, contract `t0`, or for a multi-leg run its markets `t0 × 10 + k`, k = 1..=9), so parallel and repeated runs never see each other's objects.
7. **Failures**, from a terminal: a "Failure drills" panel, at the bottom of the page under the D1–D5 tabs, lists the commands and what to watch for.

   | Drill | Command | What to watch |
   |---|---|---|
   | Engine crash mid-run | `docker compose kill engine`, then `docker compose start engine` | status "engine down" (when nothing is being sent, noticing can take up to about 10 s) → the command in flight gets `outcome unknown … nonce N` → a `Resume` batch → "engine up" → the safe retry with nonce N, logged as `nonce N taken once (Duplicate on retry); original: …` or `retry applied` / `retry rejected: …` (the gateway holds that participant's other commands until it settles, at most 60 s, gateway.md §6) → runs not yet started, which waited for the engine (up to 60 s), start and pass; a run in flight at the kill may fail on timing, or with "engine unavailable" (commands queued behind the in-flight one on the shared operator are refused). `docker compose logs engine` shows `replayed seq=… hash=…` with the hash it had |
   | Stall | `docker compose pause engine; sleep 2; docker compose unpause engine` | a `Resume` batch right after unpause; open dispute windows extended, so a run in flight may FAIL on timing (correct engine behaviour) |
   | Driver crash | `docker compose kill aeron`, then `docker compose up -d aeron engine gateway` | the engine is reported down about 3 s later; about 10 s after the kill the engine and gateway exit 1 and the page shows "Reconnecting". Docker does not restart a container you killed, and the engine and gateway cannot rejoin its dead IPC namespace, so nothing comes back until the `up -d`. Then the engine replays its journal (same hash) and the page recovers without a reload |
   | Driver crash, self-healing | `docker compose exec aeron kill -TERM 1` | the driver exits by itself and Docker restarts it in about 1 s; the engine and gateway follow (same hash); the page reconnects without a reload, and runs in flight lose their connection. Docker restarts a container automatically at most three times over its lifetime (`restart: on-failure:3`), so this drill self-heals about three times before `docker compose up -d` is needed again |

8. **View as** (Admin, Alice, Carol, MM1, MM2) re-sends `hello` with that view. From then on the page receives exactly what the gateway would send that participant. The sealing proof is now done by the product, not by the page.

### 11.2 What changes in the page

- **Data source.** The recorded stream of gateway frames replaces the WASM `snapshot`. Object states come from the events (each event is a transition). Balances come from `balances`. A requester's own limit comes from the command the page itself sent as that requester.
- **The clock** is the engine's. The +100 ms / +1 s / Next timer buttons are gone in live mode.
- **Kept:** lanes, contract card, trace, ledger, D1–D5 panels, view-as.
- **Multi-leg requests** (engine.md §8) are drawn from the events and balances alone:
  - A request card lists every leg: its market (the contract's question once the page knows the listing, else its id), side, qty, and the limit for its own requester only. The market makers' open-request cards list every leg and mark each limit sealed.
  - A `Presented` request lists a pick per leg and a `Filled` one a position per leg. A quote shows its leg ("leg 2 of 3"): the leg of the page's own `SubmitQuote`, else the leg the presented picks name it with, else none, because `QuoteSubmitted` carries no leg and no leg is guessed.
  - The contract area shows every market the run listed, side by side, each as a compact card that names its leg and holds its own position. Markets are ordered newest run first, then by contract id; a run's extra markets are `t0 * 10 + k`, so for both scenarios that is the order the run listed them. The compact card has no timeline: the contract's deadlines are in the inspector, one click away.
  - An `Expired` multi-leg request says in its card and in the trace row that no leg was filled and every hold was released, and the balances under that row show the release. The page adds nothing up.
  - Sealing is unchanged: an account view renders only its own connection's frames, so MM2's view of a multi-leg run has the public legs, its own quote and balance, and none of MM1's prices, Alice's limits or the markets' terms (`page.test.mjs`).
- **New:** a connection and engine status bar, Run / Run ×N / Run all / concurrency, the timeline scrubber, the runs table, the failure drills panel.
- **History on load** ([read-api.md](../../docs/specs/read-api.md) §8). A reload keeps no frames, so the page asks the gateway's HTTP read API for what it missed, next to its live WebSocket, which it never waits for: `GET /v1/status`, then the admin trace (`GET /v1/trace?view=admin`, pages of 1,000) from `last_seq − 5,000`, the newest 5,000 seqs at most. The base URL is `http://127.0.0.1:18081`, or `?http=` on the page URL (a bad one is ignored with a banner, as `?gateway=` is). The recorder seeds those batches as history (`seedHistory`): only seqs below the epoch's first live batch, so a batch that arrived during the fetch stays. A live frame that is a history batch again, the same seq, time and kind (the replica regenerates exactly what the node published), is ignored, so nothing is doubled. Before the epoch's first live batch, any other frame at or below the history's top seq is a new journal's (`docker compose down -v` after the page loaded) and starts a new epoch, and a history whose fetch saw the epoch change is not seeded at all, so the old history never hides a reset. History never counts as the newest batch, so chunk merging and the rest of the epoch detection of live frames (a seq that goes back) are unchanged. Each history batch carries the `command` that made it, which the model uses where the page has none of its own (same actor and nonce as the batch's origin): that restores the limits and quote sizes no event carries. Lanes, contracts, positions, ledger and trace come back, the accounts of those runs named by the scenarios' id scheme (`t0 × 100 + id`); the runs table does not (it is the page's own record of the runs it drove), and neither do the account views, whose second connection starts when the view is chosen. If the history cannot load (an older gateway, the port closed, a bad response), a dismissible banner says so and the page works as before, live only.
- **Scenario scripts** are declarative JS, one per preset, with the same steps, timings and expect lists as `rfq_testkit::scenarios` and the D3 attack presets.

### 11.3 Acceptance (live mode)

1. With the stack up, all 14 scenarios PASS from the combo box, one by one and with **Run all**.
2. **Run ×20** at concurrency 4 passes every run and shows latency percentiles.
3. Each failure drill shows what the table says, and the page recovers without a reload.
4. View as Alice / MM1 shows only what gateway.md §4 allows. Spot-checked in the DOM, as in D8.
5. No page computation of money or timers: every amount on screen is a `balances` value or an event field.
