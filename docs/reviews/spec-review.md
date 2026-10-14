# Adversarial Spec Review

- **Date:** 2026-10-14
- **Reviewed:** `docs/RFQ_Technical_Exercise.pdf`, `docs/rfc/0001-rfq-engine.md`, `docs/specs/engine.md`, `docs/specs/failure-modes.md`, `docs/specs/resolution.md`, `docs/specs/quote-lifetime.md`
- **Method:** I walked every state and transition in engine.md §6 as six attackers: a malicious requester, a malicious MM, a colluding requester+MM pair, a malicious oracle, a griefing third party, and a crashing node. For each one I asked five questions:
  1. Can funds be lost, duplicated or stuck (I1–I5)?
  2. Can someone extract value or impose cost more cheaply than it costs them?
  3. Can the wrong actor reach a transition, or reach it twice?
  4. Does timer ordering change an outcome (same timestamp, idle engine, replay)?
  5. Is any PDF requirement unaddressed?
- **Counts:** 1 critical · 5 high · 13 medium · 6 low

## Findings

| # | Severity | Attacker | Scenario (concrete steps) | Violated property | Proposed spec fix |
|---|---|---|---|---|---|
| 1 | critical | Malicious MM (or a competitor funding one) | (1) On every `RequestOpened`, the MM quotes a price sure to be best (e.g. 1 bp), with `expires_at ≥ T + ACCEPT_WINDOW`. It quotes immediately, so it also wins the earliest-`QuoteId` tie-break. (2) At deadline T its quote becomes `Selected` and every honest quote becomes `Lost`, holds released (§6.1). (3) The MM sends `CancelQuote` timed to land just after T, before the requester can see `RequestPresented` and send `Accept`. If it lands before T it only cancels an `Active` quote, so it costs nothing either way. (4) The request goes to `Rejected(QuoteWithdrawn)` and the collateral comes back. (5) Repeat on every request: no trade on the venue ever completes, at zero cost (capital is recycled and there is no penalty). Multi-leg makes it worse: one MM on one leg kills all N legs (§8 rule 4). failure-modes.md calls this "griefing" of a single requester, but it is a venue-wide DoS. | Goal 2 (no cheap griefing). PDF step 3 (the requester gets to accept the best quote). It also defeats the §6.1 eligibility rule: quotes must stay firm through `accept_by` against *passive* expiry, yet an *active* cancel in the same window is allowed, so the rule protects nothing. | Make `Selected` firm: `CancelQuote` is valid only in `Active`, so P5 becomes "cancel/replace until selection". If the product keeps cancel-in-window, it must (a) forfeit a penalty from the quote hold to the requester that is at least the requester's loss, and (b) keep the runner-up quote held (not `Lost`) until accept or lapse, so the request can be re-presented with a fresh window. Update P5, P8, R1 and the "accepted residual risk" row. |
| 2 | high | Malicious MM | (1) Request X is open with a hidden `limit_price` L (§7 says it is never published). (2) The MM sends `SubmitQuote` at p = 5000 bp. A `CommandRejected` means p > L; acceptance means p ≤ L. Once it has a live quote it probes with *replace*: a rejected replace changes nothing (validate-then-apply), so the live quote is untouched. (3) A binary search over 1..9999 takes about 14 commands. It needs no capital, because rejected commands hold nothing. (4) The MM quotes exactly L and captures the requester's whole surplus. | Sealed limit (§7, P7). Goal 2: value extraction at zero cost. | Remove `price ≤ leg.limit_price` from `SubmitQuote` validation. Selection eligibility already enforces it, and `QuoteLost` doesn't say why a quote lost. So that a hidden low limit can't bait MM capital cheaply, hold the requester at `qty × 9999 × 100` (the maximum possible escrow) instead of `qty × limit`, and release the remainder at accept. The alternative is to publish the limit and drop the privacy claim. |
| 3 | high | Losing-side position holder (or any third party) | (1) Contract C has $1M escrowed. The oracle proposes the correct `Yes` at t. (2) A `No` holder sends `Dispute` as late in the window as possible, posting the $100 bond. This leaves the arbiter as little time as possible before `resolution_deadline`. (3) If the arbiter hasn't ruled by `resolution_deadline`, C becomes `Final(Invalid)` (§6.3). The `No` holder gets its lock back instead of losing it, and its bond is returned (resolution.md step 6). (4) If the arbiter does rule, the attempt costs $100. Every loser therefore holds a free option, worth taking whenever P(arbiter late) × loss > $100. | Goal 2. Resolution correctness: `Invalid` is not a neutral refund. It moves the winner's expected payout to the loser. | Once an outcome has been proposed, a timeout must never yield `Invalid`. Add `arbiter_deadline = dispute_time + ARBITER_SLA`, validated to fit before `resolution_deadline` (or extending it). If the arbiter misses it, finalize the *proposed* outcome. Return the bond only when the ruling overturns the proposal. Scale the bond to open interest (see #9). |
| 4 | high | Crashing node (optionally with a malicious oracle) | (1) The oracle proposes a wrong outcome at t, with `dispute_window` = 1 h. (2) The sequencer or engine is down from t+5 min to t+2 h (crash, failover, or a slow replay, since snapshots aren't built). (3) On restart, the first command or `Tick` fires the dispute timer before anything else (G3). The contract becomes `Final(wrong)` and positions settle to the wrong side, irreversibly. Honest holders never had a sequenced chance to dispute. The same mechanism also: (a) consumes the accept window, so the request goes `Open`→`Presented`→`Rejected` in one batch; (b) lets `resolution_deadline` pass while a correct `ProposeOutcome` was waiting to be sequenced, giving `Invalid`. A zero-cost ingress flood (#13) can delay a `Dispute` the same way without any crash. | Money-state correctness (irreversible misallocation). G3 treats downtime as if the window to act had been available. failure-modes.md has no row for it. | Separate *deadline* timers (trading close, hard deadline) from *window* timers that give someone time to act (dispute window, accept window). On restart the sequencer logs `Resumed{last_logged_time}`. Any window timer due in `[last_logged_time, now + GRACE)` is re-armed to `now + GRACE`. Specify the `Tick` cadence and per-actor ingress fairness. Add a row to failure-modes.md. |
| 5 | high | Spec defect, triggered by any `Accept` | The spec contradicts itself. §3 defines `escrow(h, x, pos)` as "reserved → escrowed, **remainder released**". §5 `Accept` says "per leg: escrow requester `q×p×100` from request hold … release requester remainder". §8 rule 1 mandates **one** requester hold for all legs. (1) Take a 3-leg request with a single hold H = Σ legs. (2) On `Accept`, `escrow(H, x₁)` escrows leg 1 and releases the rest of H, including legs 2–3's share. (3) `escrow(H, x₂)` then runs on a consumed hold. Either it fails after the first mutation (a partial fill, which breaks T2 and the §8 atomicity claim), or it reads the stale amount and escrows funds that are already `free` (duplication, and `reserved` underflows). Single-leg also breaks: the explicit "release requester remainder" step after `escrow` has already released it is a double release. | I1, I2, T2. PDF item 6 (atomic multi-leg). The §8 / failure-modes claim "where capital could leak … it can't" is false at the ledger level. | Define `escrow_all(h, [(xᵢ, posᵢ)])`. It validates Σxᵢ ≤ hold up front, escrows every leg, then releases the remainder once. Make `HoldId` linear: every ledger op consumes it, and an op on a dead hold is an invariant violation, never a lookup of a stale amount. Delete the separate "release requester remainder" step from §5. |
| 6 | high | Griefing third party | §2 says every command "carries `actor`" but never says what authenticates it. The PDF puts auth out of scope, so an implementation may trust the field as sent. (1) The attacker sends `Deposit{to: self, amount: 10^15}` with `actor = operator` and mints money. (2) It can equally send `Rule` as the arbiter, `Accept`/`Reject` as a requester, or `CancelQuote` as an MM. (3) Or it sends `Withdraw{amount: 1}` as the victim with `nonce = u64::MAX`. The nonce is valid and consumed, so every later command from the victim is `Duplicate` and the victim's free balance can never be withdrawn. Even with authentication, a buggy client can self-brick the same way. | I1 (minting). "No funds stuck with no exit". The role table in §2. | State the trust boundary in §2. Ingress verifies a signature over `(actor, nonce, command)`. `Tick` and privileged commands are accepted only from the sequencer or operator channel. The engine trusts `actor` only on that path. Bound nonce growth with `nonce ≤ last_nonce + MAX_NONCE_GAP`, or require exactly `last + 1` (see #16). |
| 7 | medium | Malicious oracle (bribed by the losing side) | (1) Contract C closes and the true outcome is clearly `Yes`. (2) The oracle stays silent, or proposes at `resolution_deadline − dispute_window + 1 ms` so the dispute timer lands after the hard deadline. (3) At `resolution_deadline`, C becomes `Final(Invalid)` and every `No` holder recovers its lock. resolution.md says silence "only delays settlement", but it actually moves each winner's expected profit to the loser, and the oracle bears no cost for it. | Goal 2. D4 ("delayed" resolution). | Escalate silence instead of defaulting to `Invalid`. Add a `proposal_deadline ≤ resolution_deadline − dispute_window − ARBITER_SLA`. A contract still `Closed` at that point moves to `Disputed`, and the arbiter rules with no bond posted. Reject `ProposeOutcome` after `proposal_deadline`. Reserve `Invalid`-by-timeout for when the arbiter also fails. Validate these windows at `ListContract`. |
| 8 | medium | Malicious oracle | (1) The oracle proposes a wrong outcome on every contract it resolves. (2) Each time, a harmed holder must notice, lock $100 and wait for the arbiter. The ruling returns the bond but pays no reward (§5 `Rule`). (3) The oracle loses nothing and repeats. On any contract where the harmed side is offline during the window, the wrong outcome settles. | Goal 2: the oracle imposes cost on others for free. | Bond the oracle per proposal (or per contract at listing). Slash it to the disputer when the ruling overturns the proposal, so successful disputers are paid. |
| 9 | medium | Griefing third party | (1) Contract C has $10M escrowed and the oracle proposes the correct outcome. (2) An attacker with no position disputes, posting the flat $100 bond (§9). (3) All $10M stays frozen until the arbiter rules, which can be as late as `resolution_deadline`. The attacker forfeits $100. Its cost is about 1:10⁵ of the holders' time-value loss, and it can do this on every contract. | Goal 2. | Set the bond to `max(DISPUTE_BOND, k × contract open interest)`, or use an escalation ladder. Bound arbiter latency with `ARBITER_SLA` (#3). |
| 10 | medium | Malicious requester | (1) Submit a request with limit 9999 so every quote is eligible. The hold is recycled afterwards. (2) At T, receive the best firm price p with a 30 s window. (3) Watch an external venue. `Accept` only if the market has moved toward the requester; otherwise `Reject` or let the window lapse, at zero cost. (4) Repeat on every RFQ. Each MM is short a free option of `ACCEPT_WINDOW` length, which becomes hours if quotes live for days (quote-lifetime.md). Today P5 turns the window into a latency race with the MM's cancel. Once #1 is fixed, the requester's option becomes pure. | Goal 2: the requester extracts option value for free. | Charge a reject/lapse fee from the request hold to the selected MM (e.g. `REJECT_FEE_BPS × notional`), symmetric with any MM cancel penalty. Keep `ACCEPT_WINDOW` as short as the product allows. |
| 11 | medium | Colluding requester + MM | (1) An honest requester opens X (contract C, BuyYes, qty 1000, deadline T). (2) The MM's sybil opens a shadow request Y with the same public fields (§7 `RequestOpened`: contract, side, qty, deadline), deadline T′ < T, limit 9999. (3) Honest MMs can't tell Y from X and quote the same prices. (4) At T′ the sybil receives `RequestPresented` showing the best honest price p*, then `Reject`s for free. (5) The MM quotes p* − 1 bp on X and wins. This defeats sealed bidding (P7) for the cost of two commands. | P7 (sealed bids prevent undercutting). Goal 2. | The reject/lapse fee from #10 gives every probe a price. Optionally add per-requester reject-rate limits. Document that sealed bids are only as strong as the cost of a shadow RFQ. |
| 12 | medium | Griefing third party | (1) Fund one account with $100. (2) Send 10⁶ `SubmitRequest`s with qty 1, limit 1 bp and deadline `now + 7 d` (`MAX_RESPONSE`). Each hold is `1 × 1 × 100` µUSD = $0.0001. (3) The single-writer engine now carries 10⁶ live requests and timers for a week, and every MM must process 10⁶ public `RequestOpened` events. The failure-modes defence ("must hold qty × limit") costs this attacker nothing. | Goal 2. Engine liveness. | Set a minimum hold per request (`hold = max(qty × limit × 100, MIN_REQUEST_HOLD)`) or a minimum notional. Cap open requests per account. Optionally charge a non-refundable submission fee. |
| 13 | medium | Griefing third party (no capital needed) | (1) Create an account holding $0. (2) Stream `Withdraw{1}`, or any invalid command, at line rate with increasing nonces. (3) Each one is sequenced, logged, consumes a nonce and emits `CommandRejected` (§2: "a valid nonce is consumed even if the command is then rejected"). The log, Archive and replay time grow without bound, honest commands queue behind the flood, and the flood can delay a `Dispute` past its window (#4). | Goal 2. Engine liveness. Bounded replay. | Apply a per-actor ingress rate limit at the gateway, *before* sequencing. Require a minimum deposit for an actor to send commands. Pre-validate shape and ranges at the gateway, so malformed commands are never logged. |
| 14 | medium | Malicious MM | (1) Hold one active quote on a 7-day request. (2) Replace it 10⁶ times, each with `expires_at = u64::MAX − i`; §5 sets no upper bound. (3) Each replace arms a new timer. The old ones are "never cancelled, only ignored" (§6.1), so they stay in the `BTreeMap` until their far-future deadlines, even after the quote objects are pruned (quote-lifetime.md item 5). It costs one quote's hold. The timer index and snapshot size grow without bound. | Engine liveness. The D5 claim that the timer index and pruning bound state. | Key timers by `(object, purpose)` so that re-arming overwrites, and drop them when the object becomes terminal. Clamp `expires_at` to `min(expires_at, deadline + ACCEPT_WINDOW)`, since a quote is useless after that. Rate-limit replaces per actor. |
| 15 | medium | Spec defect (MM cancel, or any lapse) | §6.1 says "every terminal state other than Filled releases **all** holds of the request and its quotes". Two paths release a hold twice. (a) `CancelQuote` on a `Selected` quote: §5 already releases that quote's hold, then the request's move to `Rejected` releases "all holds of its quotes", including that one again. (b) At selection, losing quotes become `Lost` and are released. When `accept_by` lapses, the request releases "all holds of its quotes" again. `release(h)` releases the "whole hold", and the spec doesn't say whether a released `HoldId` still resolves. An implementation that looks up the amount double-credits `free` and underflows `reserved`. | I1, I2. | Apply the linear-`HoldId` rule from #5. Reword §6.1 to "releases every *live* hold owned by the request and its quotes". Add both paths as named property-test cases. |
| 16 | medium | Crashing node (gateway failover) | (1) An MM sends `CancelQuote` with nonce 41 via gateway G1, and G1 crashes before forwarding it. (2) The MM's next command (nonce 42, a `SubmitQuote` elsewhere) goes via G2 and is sequenced. (3) The client retries nonce 41 and gets `Duplicate` ("nothing consumed"). (4) Because §2 says "retries are idempotent", the client treats `Duplicate` as "already applied". The quote stays `Active` and is filled at a stale price. A `nonce > last` rule cannot tell "applied" apart from "skipped". | §2 idempotency claim. R6. | Require `nonce == last_nonce + 1`. A gap gets `NonceGap{expected}` and nothing is consumed. `Duplicate` then unambiguously means "applied", and the reply should carry the original `seq` and result. |
| 17 | medium | Malicious requester (multi-leg design) — locks MM capital permanently at equal cost to itself | With N > 1 enabled per §8: (1) Leg 1 is on contract A (closes in 30 d). Leg 2 is on contract B (closes in 2 h, 10-minute dispute window). (2) A deadline of +6 h passes `deadline + ACCEPT_WINDOW < trading_close`, because §5 checks "contract" in the singular. (3) B closes, its oracle proposes, and B becomes `Final` at +2 h 10 m. (4) At +6 h the request is selected and `Accept` creates a position on the already-`Final` contract B. No timer will ever settle that position, so both sides' escrow is stuck forever. Neither `SubmitQuote`, selection nor `Accept` checks contract state. | I5. "No stuck funds". | Apply the trading-close check to every leg (min over legs). As defence in depth, `Accept` and selection re-check that every leg's contract is `Trading`, and `SubmitQuote` does too. |
| 18 | medium | Any participant (in the designed extensions) | A timer is a no-op if its object "has left the state it was armed for" (§6.1). That only works while no state is ever re-entered, and both promised extensions re-enter one. (a) `SelectionPolicy::Continuous` (quote-lifetime.md): quote A is presented at t₁ with `accept_by₁ = t₁ + W`. A better quote B re-presents at t₁ + 20 s. At t₁ + W the first timer finds the request `Presented` and lapses it 20 s early. (b) Dispute escalation rounds (resolution.md, "Disputed"): the round-1 dispute timer fires during round 2's `Proposed` and finalizes it before anyone can dispute. | Timer semantics behind I5. The D5 claim that either quote lifetime is "cheap later". | Stamp each timer with the object's `epoch`, bumped on every state entry (or with the exact deadline it expects). The timer fires only if it matches. |
| 19 | medium | Malicious MM (designed `SharedLimit`) | quote-lifetime.md lets `SharedLimit` "run the funds check at selection or acceptance". Suppose the check runs at acceptance. (1) An MM with a $1k pool backs $1k quotes on 100 requests at the best price. (2) It wins all 100 selections, and every competing quote is `Lost` and released. (3) The first `Accept` drains the pool, and the other 99 fail for lack of funds. That is #1's outcome with no cancel on the log to attribute. It also breaks F1 ("accept cannot fail for funds"). | Goal 2. F1. T2 for multi-leg. | Under `SharedLimit`, selection is the commit point: convert the quote to a hard hold at selection, and skip to the next-best quote if the pool can't cover it. Never check funds at acceptance. |
| 20 | low | Malicious oracle | (1) A contract has `resolution_deadline` R and `dispute_window` w. (2) The oracle proposes at exactly R − w. (3) The dispute timer and the hard-deadline timer both fire at R. Whether the result is `Final(proposed)` or `Final(Invalid)` depends on the `TimerKey` order across timer kinds, which no spec defines (R4 says only "ordered by key"). Two builds could disagree, and replay after an upgrade could diverge. | Determinism (F6). A well-defined outcome. | Specify the total order of `TimerKey` across kinds in §6. Reject proposals where `now + w ≥ R` (subsumed by #7's `proposal_deadline`). |
| 21 | low | Crashing node (sequencer failover) | (1) The last logged command is stamped t. (2) The sequencer restarts on a host whose clock is 5 s behind. (3) The next commands are stamped < t. An `Accept` that arrives after `accept_by` in real time is stamped before it and fills, holding the MM beyond the window it priced. `ProposeOutcome` arms `now + w` from the regressed `now`. | T5 (time is data) assumes stamps never go backwards. R2. | The sequencer stamps `max(wall_clock, last_logged_time)` and reads `last_logged_time` back from the log on start. The engine treats any time regression as fatal. |
| 22 | low | Careless or colluding operator or oracle | `ListContract` validates only `trading_close < resolution_deadline`. (1) A listing with `oracle = arbiter` lets one account propose and then judge the dispute against itself. (2) With `dispute_window = 0`, the proposal finalizes before any `Dispute` can be sequenced. (3) A `resolution_deadline` years away leaves escrow effectively stuck. (4) The oracle account can trade its own contract (trading is permissionless) and then choose its proposal timing (#7). Genesis also allows `ACCEPT_WINDOW = 0`, an empty window. | Role separation. I5 in practice. Resolution integrity. | `ListContract` checks: oracle ∉ {arbiter, operator, treasury}; `dispute_window ≥ MIN_DISPUTE_WINDOW`; `resolution_deadline − trading_close ≥ ORACLE_SLA + dispute_window + ARBITER_SLA`; `resolution_deadline ≤ now + MAX_HORIZON`. Genesis checks `ACCEPT_WINDOW > 0`. Reject `SubmitRequest` and `SubmitQuote` from `contract.oracle` and from the arbiter. |
| 23 | low | Colluding requester + MM | (1) Two sybils create 10⁶ positions of qty 1 on contract C. Each takes 3 free commands and $1 of escrow, all of which returns to them. (2) At `Final`, §6.3 settles every position "in the same command". (3) That one command performs 10⁶ ledger moves, so the single writer stalls for every user, and again on every replay. | Engine liveness. | `Final` records only the outcome. Positions settle in bounded batches (≤ K per command) on later Ticks, or through a per-position `Claim`. I5 is kept by a pending settlement-queue timer. |
| 24 | low | Any trader (PDF D4: "ambiguous wording") | (1) Ten days into a 30-day contract, its wording turns out to be ambiguous. (2) Nothing can stop trading: requests keep filling, with each side pricing its own reading of the question. (3) The arbiter's eventual reading moves value between positions opened after the ambiguity was already known. The design handles ambiguity only at resolution time. | D4 asks what the system does, mechanically, about ambiguous wording. | Add an operator `HaltContract{id}`. It moves the contract from `Trading` to `Closed` immediately and, in the same command, moves open requests to `Rejected` with their holds released. Optionally pre-commit `Invalid` for positions opened after the halt notice. |
| 25 | low | Malicious MM with stream access | (1) Quotes are sealed only because gateways filter egress by `audience` (§7). The Aeron egress stream and the Archive carry every `QuoteSubmitted` in plaintext. (2) An MM that runs or compromises a gateway, or that can read the egress stream or Archive, sees competitors' prices before T and undercuts them by 1 bp. | P7. | State the trust boundary: egress, Archive and gateways are operated by the venue, and participants reach only filtered sessions. For production, use commit-reveal or encrypted quotes. |

## Checked and sound

### State and transition sweep (engine.md §6)

**Request**

- **`[*] → Open`.** Only `SubmitRequest` gets here.
  - The hold covers the worst-case escrow, since p ≤ limit.
  - The funds check, the deadline bounds, and the single-leg trading-close guard all run before any mutation (T2).
  - Sound, apart from #2 and #12.
- **`Open → Cancelled`.** Only the requester, and only from `Open`.
  - A `CancelRequest` stamped exactly T is processed after selection (G3), so it is rejected and the requester must `Reject` instead.
  - Requester cancels are uninformed, because quotes are sealed and the requester sees nothing before T.
- **`Open → Expired` / `Open → Presented`.** A single timer drives both and fires once.
  - Selection is all-or-nothing, and losing quotes are released in the same command.
  - A quote with `expires_at = T` can't be selected whatever the timer order, because eligibility needs `≥ T + W`.
  - Selection is deterministic: lowest price, then lowest `QuoteId`. A replace gets a new id and so loses time priority, which is fair.
- **`Presented → Filled`.**
  - Only the requester can accept.
  - `Accept` must echo the quote ids and prices, so the requester can't accept a price it wasn't shown.
  - The MM can't change the price after selection, because `SubmitQuote` requires `Open`.
  - Single-leg `Accept` can't fail for funds.
  - An `Accept` stamped `≥ accept_by` loses to the lapse timer (R2). A double accept, or accept versus reject, gets `InvalidState` (R5).
  - Sound, apart from the ledger wording in #5.
- **`Presented → Rejected` (by `Reject` or lapse).** Triggered by the requester or the clock, and releases everything. Sound, apart from the wording in #15.
- **`Presented → Rejected` (MM cancel).** See #1.

**Quote**

- **`Active → Replaced`.** Atomic, with the funds check net of the old hold (R8). Sound, apart from #14.
- **`Active → Cancelled / Expired / Lost`.** Each releases the hold.
- **`Selected → Expired`.** Unreachable: eligibility guarantees `expires_at ≥ accept_by`, and the expiry timer of a `Selected` quote is a stale no-op.
- **`Selected → Filled`.** The MM hold equals the MM escrow exactly.
- **`Selected → Released`.** Releases the hold. Sound.

**Contract**

- **`Trading → Closed`.** The strict `<` in the submit guard means no single-leg request can still be `Open` or `Presented` at close.
- **`Closed → Proposed`.** Only the contract's oracle can propose, only from `Closed`, and only once.
- **`Proposed → Disputed`.** One dispute per contract. A `Dispute` stamped at the window end consistently loses to the timer.
- **`Disputed → Final` (by `Rule`).** Only the arbiter can rule, and only from `Disputed`. Forfeiting the bond to the treasury preserves I1. `Rule` and `ProposeOutcome` stamped at `resolution_deadline` are rejected consistently.
- **Hard deadline.** It reaches `Final` from every non-final state, so liveness holds. The economic problems with it are #3 and #7.
- **`Final`.** Terminal, so positions settle once, and payouts go only to the two holders of each position.

### Invariants

- **I1 conservation.**
  - Every ledger op moves funds between accounts or between an account and the outside.
  - Collateral has no rounding: `q×p×100 + q×(10000−p)×100 = q×10⁶`.
  - The largest per-leg amount is `10⁹ × 10⁶ = 10¹⁵`, far below `u64::MAX`, and all arithmetic is checked.
  - Sound once #5 and #15 are fixed.
- **I2 and I4.**
  - Every hold is created by exactly one command and owned by exactly one object.
  - Every non-`Filled` terminal state releases its holds, and `Filled` consumes them through escrow.
  - Sound once `HoldId` is made linear (#5).
- **I3.** The two escrow locks always sum to `qty × 10⁶`.
- **I5.**
  - Every state that owns money has a timer leading toward a terminal state.
  - Wording nit: `Open`'s timer leads to `Presented`, which is not terminal. But the chain `Open → Presented → terminal` is finite, so the invariant holds in substance; it should be reworded.
  - It holds provided timers actually fire (#4), and except for #17.

### Timer ordering

- **Same-timestamp pairs that are sound:**
  - quote expiry vs. request deadline;
  - lapse vs. `Accept`;
  - window end vs. `Dispute`;
  - hard deadline vs. `Rule` or `ProposeOutcome`;
  - `trading_close` vs. `accept_by` (kept apart by a strict inequality).
- **The one unsound pair:** dispute-window end vs. hard deadline (#20).
- **Idle engine.** Due timers fire before the next command or `Tick`. Nothing is stuck, only delayed, except for windows that give someone time to act (#4).
- **Replay.**
  - Time is data, state uses `BTreeMap`, genesis is in the log, the log is written ahead of events, and a `state_hash` test compares runs.
  - Under today's state machines no state is re-entered, so staleness by state alone is sound. #18 covers the extensions.

### Personas with no further finding

- **Requester.** Can't move reserved funds (R7). Can't accept an altered price. Sees nothing before T.
- **MM.**
  - Passive expiry inside the window is blocked by the eligibility rule.
  - Quote spam needs a full collateral hold per quote, at most one live quote per MM per leg, and recycled capital.
  - Can't reprice after selection.
- **Colluding pair.**
  - Sybil self-trades only move their own money; there are no fees or rebates to farm.
  - There is no ledger path into an honest MM's hold: holds are only released to their owner or forfeited to the treasury.
- **Oracle.** Can't move funds except between the two holders of a position. Can't propose before close or propose twice.
- **Third party.** Can't trigger requester or MM transitions, because of the actor checks (given #6).
- **Crashing node.** F3, F4 and F5 are sound: write-ahead logging, replay, and consumers deduplicating by `seq`. F7 and F8 are sound for liveness.

### PDF coverage

- **Items 1–6:**
  - Item 1 (request) is in §5. It deviates from the PDF: a request references an operator-listed contract instead of carrying a free-form description (P10). This is documented and defensible, but expect it to be probed.
  - Items 2–3 (quotes, selection) are in §5 and §6.1.
  - Items 4–5 (escrow, payout) are in §3 and resolution.md.
  - Item 6 (multi-leg) is in §8. Competing quotes are released at *selection*, earlier than the PDF's "acceptance". That is stronger than required, but it removes the fallback that #1 exploits.
- **Deliverables D1–D5:**
  - D1 is engine.md §6, including §8's "leg 2 of 3" table.
  - D3 is failure-modes.md. It is missing the crash and ordering cases #4, #16 and #21.
  - D4 is resolution.md, covering disputed, delayed and ambiguous. See #3, #7 and #24.
  - D5 is quote-lifetime.md. See #18 and #19.
- **Out of scope:** auth is excluded by the PDF, but it still needs the one-line trust assumption from #6.

## Decisions

Ruled 2026-10-14 (Gate A). FIX = spec and task code/tests changed; DOC = spec wording only; ACCEPT = residual risk recorded in [failure-modes.md](../specs/failure-modes.md#adversarial-behaviour) with its production defence.

- #1 — FIX — Selected quotes are firm: `CancelQuote` only while `Active` (else `InvalidState`); P5 is "cancel/replace until selection"; `Rejected(QuoteWithdrawn)` path and variant removed; R1 is now cancel vs response deadline.
- #2 — FIX — `SubmitQuote` no longer checks `price ≤ limit` (no limit oracle); selection filters over-limit quotes; requester hold stays `qty × limit`.
- #3 — FIX — `ProposeOutcome` requires `now + dispute_window < resolution_deadline`; hard deadline while `Disputed` ⇒ `Final(proposed)` with the bond returned; only `Closed` ⇒ `Invalid`.
- #4 — FIX — The node sequences `Command::Resume` first after restarting on a non-empty journal; before timers fire it extends open dispute windows to `now + resume_grace` (capped at `resolution_deadline − 1`); the dispute timer is deadline-matched.
- #5 — DOC — The requester hold is escrowed once per request by `escrow_from_hold(h, Σ legs)`; holds are linear (`UnknownHold` on a dead hold); the separate "release remainder" step is gone.
- #6 — FIX — Strict nonces (see #16) end the `u64::MAX` lockout; trust boundary documented (gateway authenticates `actor`; clients can't express `Genesis`/`Tick`/`Resume`).
- #7 — ACCEPT — Oracle silence ⇒ `Invalid` is the liveness fallback for a trusted oracle; production: oracle bond + escalation to the arbiter at a `proposal_deadline`.
- #8 — ACCEPT — Production: oracle bond per proposal, slashed to the disputer when overturned.
- #9 — ACCEPT — Flat bond is a known weakness; production: bond scaled to open interest or an escalation ladder.
- #10 — ACCEPT — Requester free option is inherent to RFQ, bounded by a short `ACCEPT_WINDOW` and firm quotes; production: reject/lapse fee to the selected MM, reject-rate limits.
- #11 — ACCEPT — Shadow-RFQ price discovery: same mitigation and production defence as #10.
- #12 — FIX — Each leg needs `qty × limit × 100 ≥ min_request_notional` (default $1), else `InvalidInput`.
- #13 — ACCEPT — Ingress flooding is stopped at the gateway (per-actor rate limits, pre-validation) before sequencing; gateway out of scope.
- #14 — FIX — `SubmitQuote` clamps `expires_at` to `response_deadline + accept_window` for the quote, its event and its timer.
- #15 — DOC — §6.1 says "releases every *live* hold"; linear holds (#5) make a double release impossible.
- #16 — FIX — Only `nonce == last + 1` is accepted; `≤ last` ⇒ `Duplicate` (= already applied), a gap ⇒ `NonceGap`; neither consumes anything.
- #17 — DOC — The trading-close guard is per leg (min over legs); `Accept` re-checking contract state is a multi-leg-enablement item.
- #18 — DOC — Extensions that re-enter a state must stamp timers with the expected deadline or epoch (the dispute timer already does).
- #19 — DOC — Under `SharedLimit`, selection is the commit point (hard hold at selection, never a funds check at acceptance).
- #20 — FIX — Subsumed by #3 (the dispute timer always fires before the hard deadline); the same-timestamp timer order (`TimerKey` variant order) is stated in engine.md §6.
- #21 — DOC — The sequencer stamps `max(wall, last_logged_time)` and resumes `last_time` from the journal; the engine takes `max` too (failure-modes F10).
- #22 — FIX — `ListContract` rejects an oracle that is operator, arbiter or treasury; insider trading, `MAX_HORIZON` and `MIN_DISPUTE_WINDOW` are ACCEPTED as production listing policy.
- #23 — ACCEPT — Production: settlement in bounded batches or per-position `Claim`.
- #24 — ACCEPT — Production: operator `HaltContract`; today mid-life ambiguity is resolved `Invalid` at resolution.
- #25 — DOC — Egress, Archive and gateways are venue-operated and participants see only audience-filtered sessions; production: encrypted or commit-reveal quotes.
