# Adversarial Behaviour

Part of the failure-mode notes ([failure-modes.md](failure-modes.md), D3): what each kind of attacker can try against a permissionless venue, and what stops it. Races and partial failures are in failure-modes.md.

`#n` = finding in [spec-review.md](../reviews/spec-review.md). **Residual** = accepted risk, with its production defence.

| Attacker | Attack | Defence |
|---|---|---|
| Requester | Spam requests to tie up MM attention/capital (#12) | Must hold `qty × limit` per request (P4), and every leg is at least `MIN_REQUEST_NOTIONAL` ($1). |
| Requester | Accept a different price than shown | `Accept` must echo quote ids **and** prices; mismatch ⇒ rejected. |
| Requester | Free option: accept only if the market moved its way, else reject or lapse (#10) | **Residual**, inherent to RFQ: bounded by a short `ACCEPT_WINDOW`, and firm quotes stop the MM racing it. Grows with days-long windows. Production: reject/lapse fee to the selected MM; per-requester reject-rate limits. |
| Requester + MM | Shadow RFQ with the same public fields to read the best honest price, then reject (#11) | **Residual**, as above: sealed bids are only as strong as the cost of a shadow RFQ. |
| MM | Spam quotes | Each quote holds full collateral; one active quote per MM per leg. |
| MM | Probe the sealed limit with quote submissions (#2) | `SubmitQuote` doesn't check the limit; selection drops over-limit quotes and `QuoteLost` doesn't say why. |
| MM | Grow the timer index with far-future expiries (#14) | `expires_at` clamped to `response_deadline + ACCEPT_WINDOW`. |
| MM | Undercut by 1 bp after seeing others (#25) | Sealed bids; quote events go only to their owner (`audience`). Egress, Archive and gateways are venue-operated. Production: encrypted or commit-reveal quotes. |
| MM | Win with a quote that expires right after selection | Not eligible unless firm through the whole acceptance window. |
| MM | Win, then cancel in the window to void the RFQ (#1) | Selected quotes are firm (P5): cancelling a `Selected` quote ⇒ `InvalidState`. |
| Either | Self-trade | `mm ≠ requester` rejected. Sybil accounts can't be prevented in a permissionless venue — they only trade with themselves at their own cost. |
| Oracle | Propose a wrong outcome (#8) | Dispute window + bond + arbiter. **Residual:** a wrong proposal costs the oracle nothing. Production: oracle bond per proposal, slashed to the disputer when overturned. |
| Oracle | Stay silent so the contract resolves `Invalid` (#7) | **Residual:** `Invalid` at the hard deadline is the liveness fallback for a trusted (mocked) oracle. Production: oracle bond + auto-escalation to the arbiter at a `proposal_deadline`. |
| Oracle | Propose late so the window outlives the hard deadline (#20) | `ProposeOutcome` needs `now + dispute_window < resolution_deadline`. |
| Oracle / arbiter | Judge its own proposal; trade its own contract; years-long or zero-window listings (#22) | `ListContract` rejects an oracle that is operator, arbiter or treasury, a zero `dispute_window`, and a window that cannot end before `resolution_deadline`. **Residual:** insider trading, `MAX_HORIZON`, `MIN_DISPUTE_WINDOW` are production listing policy. |
| Anyone | Dispute spam (#9) | Bond; one dispute per contract; bond forfeited if the ruling upholds the proposal. **Residual:** a flat $100 bond can freeze any amount of escrow until the arbiter rules. Production: bond scaled to open interest, or an escalation ladder. |
| Anyone | Forge `actor`, or inject `Tick`/`Genesis`/`Resume` (#6) | The gateway authenticates `actor` (out of scope per the brief); `ClientCommand` can't express system commands. Strict nonces: no `u64::MAX` lockout. |
| Anyone | Flood ingress with invalid commands, each one logged (#13) | Malformed or over-64 KiB messages never reach the journal or the engine, but the node warn-logs each drop, unthrottled (a log flood is a residual risk). **Residual:** a well-formed but invalid command is sequenced and journaled. Production stops floods at the gateway (per-actor rate limits, pre-validation) before sequencing; the gateway is out of scope. |
| Colluders | 10⁶ tiny positions so one finalisation stalls the single writer (#23) | Each position costs at least `MIN_REQUEST_NOTIONAL` of escrow. **Residual:** production settles in bounded batches or per position (`Claim`). |
| Anyone | Exploit wording found ambiguous mid-life (#24) | **Residual:** resolved `Invalid` at resolution. Production: operator `HaltContract`. |
