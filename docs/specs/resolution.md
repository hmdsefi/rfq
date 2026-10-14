# Resolution Design (D4)

## How funds unlock

A `Filled` request creates one **position** per leg: `yes_holder`, `no_holder`, `qty`, and two locks that sum to `qty × $1`. Those funds sit in `escrowed` and can leave only through **contract finalisation**, which settles every position of the contract in the same command:

| Final outcome | Payout |
|---|---|
| `Yes` | `yes_holder` gets `yes_lock + no_lock` (= `qty × $1`) |
| `No` | `no_holder` gets `yes_lock + no_lock` |
| `Invalid` | each side gets back its own lock |

Settled funds go to `free` and are immediately withdrawable.

## Lifecycle (mechanics)

1. **Trading → Closed** at `trading_close` (clock). No new requests can outlive this (submit-time check: `deadline + ACCEPT_WINDOW < trading_close`).
2. **Closed → Proposed:** the contract's oracle sends `ProposeOutcome{Yes|No|Invalid}`. It is rejected (`InvalidState`) unless `now + dispute_window < resolution_deadline`, so the window always ends before the hard stop. `ListContract` enforces the same room at listing (`trading_close + dispute_window < resolution_deadline`), so a proposal made at `trading_close` always fits. A dispute timer is armed at `dispute_until = now + dispute_window`.
3. **Proposed → Final(proposed)** when the dispute timer fires with no dispute. The timer is deadline-matched: it finalizes only if it fires at or after the current `dispute_until`.
4. **Proposed → Disputed:** anyone sends `Dispute` before `dispute_until`, holding `DISPUTE_BOND`. One dispute per contract; it freezes the proposal.
5. **Disputed → Final(ruling):** the arbiter sends `Rule{outcome}`.
   - ruling ≠ proposal ⇒ disputer was right: bond returned.
   - ruling = proposal ⇒ bond forfeited to the treasury account (conservation holds: it moves account-to-account).
6. **Hard stop** at `resolution_deadline`. A timeout never turns a proposal into `Invalid`:
   - `Closed` (the oracle never proposed) ⇒ `Final(Invalid)`.
   - `Disputed` (the arbiter never ruled) ⇒ `Final(proposed)`, bond returned: the arbiter failed, not the disputer. No event announces the returned bond (spec gap, [engine.md §7](engine.md)).
   - `Proposed` can't reach it: step 2 and the `Resume` cap keep `dispute_until < resolution_deadline`.

## Disputed

The proposal is frozen; no funds move until `Rule` or the hard deadline, which finalizes the proposal and returns the bond. Positions remain fully collateralised the whole time, so a dispute delays money but can never lose it. A late dispute can't run out the clock to a refund either: an unruled dispute ends in the proposed outcome, not `Invalid`. The arbiter is a mocked trusted role here; in production it would be an escalation game (larger bonds per round) or a token vote — the engine interface (`Rule` from an authorised actor) stays the same.

## Delayed

- **Oracle silent.** Nothing happens until `resolution_deadline`, then `Final(Invalid)` refunds both sides. Money is never stuck: every unsettled position belongs to a contract with a pending hard-deadline timer (invariant I5). This is the liveness fallback for a trusted oracle; it does move expected value from winners to losers. Production: an oracle bond, and auto-escalation to the arbiter at a `proposal_deadline`.
- **Node down during the dispute window.** Downtime must not eat the time holders have to dispute. After a restart the node sequences `Resume` before anything else; before any due timer fires, every `Proposed` contract whose window ends before `now + RESUME_GRACE` gets `dispute_until = min(now + RESUME_GRACE, resolution_deadline − 1)`, a re-armed timer, and a re-emitted `OutcomeProposed`. The old timer becomes a no-op. Deadlines that only refund (lapse, expiry, hard deadline) fire normally.

## Ambiguous wording

The oracle (or the arbiter on dispute) resolves `Invalid`: each side gets back exactly what it put in. Ambiguity is reduced upstream by operator-listed contracts (P10) — requesters can't author the question text that MMs price against. Ambiguity found mid-life is only handled at resolution; production adds an operator `HaltContract` to stop trading at once.

## What a malicious oracle can and can't do

- **Can:** propose a wrong outcome — which anyone can dispute for the cost of a bond. The oracle itself risks nothing (production: an oracle bond slashed to a successful disputer).
- **Can:** stay silent — which delays settlement until the hard deadline resolves `Invalid` (see Delayed).
- **Can't:** propose so late that its window outlives the hard deadline, or turn a disputed proposal into `Invalid` by waiting.
- **Can't:** also be the operator, arbiter or treasury (`ListContract` rejects it), so it never judges its own proposal.
- **Can't:** move funds anywhere except the two holders of each position.
