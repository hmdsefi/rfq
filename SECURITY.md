# Security Policy

## Supported versions

Only the `main` branch is supported. There are no releases yet.

## Reporting a vulnerability

Please **do not open a public issue** for a security problem. Report it privately by either route:

- email **hdyousefi@gmail.com** with the subject `SECURITY: rfq`;
- or use GitHub's private vulnerability reporting (Security tab → "Report a vulnerability"), if it is enabled for this repository.

Include:
- the affected component (`rfq-domain`, `rfq-node`, `rfq-gateway`, the Docker setup, …);
- the commit hash;
- steps or a script to reproduce;
- what an attacker gains: for example funds lost, duplicated or stuck, sealed quotes or limits revealed, or a crash.

You'll get an acknowledgement within a few days. Fixes go to `main`, and the report is credited unless you prefer otherwise.

## Security model, in brief

What the design defends against, and what it leaves out on purpose, is written down. Please read it before reporting:

- [docs/specs/adversarial.md](docs/specs/adversarial.md): what a malicious requester, market maker, oracle or bystander can try, and the defence or the accepted residual risk.
- [docs/specs/failure-modes.md](docs/specs/failure-modes.md): races and partial failures, each with its test.
- [docs/specs/gateway.md](docs/specs/gateway.md) §7: the gateway's trust boundary. It binds to `127.0.0.1`, has an Origin allowlist, caps frames, and has no authentication: the exercise puts auth out of scope, and production would fix the actor per authenticated session.

Out of scope:
- authentication and KYC;
- real chain or payment integration;
- the demo dashboard (`tools/dashboard/`), which is a presentation aid and not part of the product.
