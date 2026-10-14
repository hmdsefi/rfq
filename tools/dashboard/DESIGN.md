# RFQ Dashboard: design spec

Status: approved by the PO on 2026-10-14 (spec, plan and the D1 mockup).

> **A demo aid for the live session. It is not part of the engine.**
> The brief puts UI out of scope. This tool sits outside the cargo workspace and changes nothing under `crates/`. It runs the real `rfq-domain` engine, compiled to WebAssembly, so the reviewers can watch and drive the same code the tests and the Aeron node run.

## 1. Purpose

The page is used in the 30-minute defense to *trace, justify, attack and extend* the engine live:

- show market makers quoting, replacing and cancelling, users requesting, accepting and rejecting, and the match happening at the deadline;
- let the presenter act as any participant and try "what if…" moves on the spot;
- give an **admin trace** of everything: every command, every event (sealed ones too), every balance and hold, every state transition;
- cover the five deliverables (D1–D5) with real engine output, or with a clearly labelled design where the brief allows design only (multi-leg).

**Success** means three things:
- each of the 6 brief scenarios and the attack presets runs in one click with the expected events;
- the presenter can take over at any step;
- every number on screen comes from the engine, never from page logic.

## 2. Non-goals

- No server, no Aeron, no Docker: the Aeron path is already shown by `scripts/demo.sh`.
- No auth, accounts or persistence: a page reload starts fresh.
- No changes to `crates/` or to the workspace manifests.
- No multi-leg *execution*: the engine rejects more than one leg (P6). The multi-leg panel is a design walkthrough and is labelled that way.
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
- contracts are public;
- other parties' commands appear only through the events the viewer is allowed to see.

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
   - **D1** the request, quote and contract state machines, with the current state of the selected object lit and the last transition labelled with who triggered it; a *Multi-leg (designed, not built, P6)* tab walking through engine.md §8's "leg 2 of 3" table;
   - **D3** failure modes R1–R9 and F1–F15 with their test names, each linked to the preset that demonstrates it where one exists;
   - **D4** resolution paths (undisputed, disputed then ruled, oracle silent, unruled dispute);
   - **D5** the two seams, read from this run's `Genesis` (`ReservationPolicy::FullHold`, `SelectionPolicy::SealedAtDeadline`), with invariant and non-invariant lists.

## 5. Presets

**Brief scenarios.** All six come straight from `rfq_testkit::scenarios::all()`, the same scripts as the tests and the Aeron demo:
- `happy_path`
- `mm_cannot_withdraw_after_selection`
- `no_quotes_expire`
- `requester_lapses`
- `disputed_ruled_invalid`
- `oracle_silent_hard_deadline`

When a preset finishes, the page runs `contains_in_order` against the scenario's `expect` list and shows PASS or FAIL.

**Attack presets.** These live in the dashboard crate and are built from the testkit's public `Step`/`Ctx` types. Each pins the engine's answer, taken from the existing tests:

| Preset | Attacker move | Engine answer |
|---|---|---|
| `attack_accept_wrong_price` | Alice accepts with a price other than the one presented | `CommandRejected(Mismatch)`; the presented quote stays firm |
| `attack_double_accept` | Alice accepts twice | the second is `CommandRejected(InvalidState)`; escrow happens once |
| `attack_replayed_nonce` | MM1 resends an already-used nonce, then skips one | `Duplicate`, then `NonceGap`; nothing consumed |
| `attack_over_limit_quote` | MM2 quotes above Alice's sealed limit | quote `Active`, never selected; the in-limit quote wins |
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

1. All 6 brief presets show PASS. All 6 attack presets show their pinned answer.
2. From any preset step, take-over allows quote, cancel, request, accept, reject, propose, dispute, rule and clock moves.
3. View-as filtering hides sealed data exactly as the event audiences say.
4. The ledger total changes only on `Deposit` and `Withdraw`, and the I1–I5 status is shown after every command.
5. The replay check matches after every preset and after manual play.
6. `git diff --stat crates/ Cargo.toml Cargo.lock` is empty.
7. The page carries the label "Demo aid — not part of the engine", and the multi-leg tab says "designed, not built (P6)".

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
