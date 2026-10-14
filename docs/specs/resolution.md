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
2. **Closed → Proposed:** the contract's oracle sends `ProposeOutcome{Yes|No|Invalid}`. A dispute timer is armed at `now + dispute_window`.
3. **Proposed → Final(proposed)** when the dispute timer fires with no dispute.
4. **Proposed → Disputed:** anyone sends `Dispute`, holding `DISPUTE_BOND`. One dispute per contract; it freezes the proposal.
5. **Disputed → Final(ruling):** the arbiter sends `Rule{outcome}`.
   - ruling ≠ proposal ⇒ disputer was right: bond returned.
   - ruling = proposal ⇒ bond forfeited to the treasury account (conservation holds: it moves account-to-account).
6. **Hard stop:** at `resolution_deadline`, any contract not yet `Final` becomes `Final(Invalid)`; a pending dispute bond is returned.

## Disputed

The proposal is frozen; no funds move until `Rule` or the hard deadline. Positions remain fully collateralised the whole time, so a dispute delays money but can never lose it. The arbiter is a mocked trusted role here; in production it would be an escalation game (larger bonds per round) or a token vote — the engine interface (`Rule` from an authorised actor) stays the same.

## Delayed

If the oracle never proposes, nothing happens until `resolution_deadline`, then `Final(Invalid)` refunds both sides. Money is never stuck: every unsettled position belongs to a contract with a pending hard-deadline timer (invariant I5).

## Ambiguous wording

The oracle (or the arbiter on dispute) resolves `Invalid`: each side gets back exactly what it put in. Ambiguity is reduced upstream by operator-listed contracts (P10) — requesters can't author the question text that MMs price against.

## What a malicious oracle can and can't do

- **Can:** propose a wrong outcome — which anyone can dispute for the cost of a bond.
- **Can:** stay silent — which only delays settlement until the hard deadline refunds everyone.
- **Can't:** move funds anywhere except the two holders of each position.
