# Task 11: Demo client, Docker Compose and crash-recovery script

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Gate A rulings applied here** ([decisions](../../reviews/spec-review.md#decisions)): #6 + #16 strict nonces (`last + 1` per actor) — the demo derives per-run account ids so scenario nonces start at 1, and re-syncs the two genesis roles' nonces by probing; #1 the withdrawal scenario is now `mm_cannot_withdraw_after_selection`.

**Files:**
- Create: `crates/rfq-demo/Cargo.toml`, `crates/rfq-demo/src/main.rs`, `docker/rust.Dockerfile`, `docker-compose.yml`, `scripts/demo.sh`
- Modify: `Cargo.toml` (members)

**Interfaces:**
- Consumes: `scenarios::*`, `codec::*`, `rfq_node::aeron::{connect, INGRESS, EGRESS}`.

- [ ] **Step 1: Write the demo client**

`crates/rfq-demo/Cargo.toml`:
```toml
[package]
name = "rfq-demo"
version = "0.1.0"
edition = "2021"

[dependencies]
rfq-core = { path = "../rfq-core" }
rfq-node = { path = "../rfq-node" }
anyhow = { workspace = true }
rusteron-archive = { version = "=<version from Task 0>", features = ["static", "precompile"] }
```
`crates/rfq-demo/src/main.rs`:
```rust
//! Runs every scenario against a live rfq-node over Aeron and checks the expected event order.
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail};
use rfq_core::codec::{decode, encode, ClientCommand, EventBatch};
use rfq_core::scenarios::{all, contains_in_order, Ctx, StepKind};
use rfq_core::testkit::{accounts::*, rejection};
use rfq_core::{AccountId, Action, Emitted, RejectReason};
use rfq_node::aeron::{connect, EGRESS, INGRESS};
use rusteron_archive::*;

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }

/// Scenario accounts get fresh ids on every run, so their strict nonces start at 1 even against a
/// long-lived node. Genesis roles are fixed by the node's genesis and keep their ids.
fn run_account(run: u64, a: AccountId) -> AccountId {
    if [OPERATOR, ARBITER, TREASURY].contains(&a) { a } else { AccountId(run * 1_000 + a.0) }
}

/// Maps the account ids inside an action. Only these two actions carry any.
fn remap(action: Action, f: impl Fn(AccountId) -> AccountId) -> Action {
    match action {
        Action::Deposit { to, amount } => Action::Deposit { to: f(to), amount },
        Action::ListMarket { id, description, oracle, trading_close, resolution_deadline, dispute_window } => {
            Action::ListMarket { id, description, oracle: f(oracle), trading_close, resolution_deadline, dispute_window }
        }
        other => other,
    }
}

/// Finds a genesis role's last nonce on a long-lived node without reading state. Strict nonces
/// answer `Duplicate` (too low) or `NonceGap` (too high) and consume nothing; the probe that lands
/// on `last + 1` is consumed. Returns that probe's nonce, which is the role's new `last`.
fn sync_nonce(mut probe: impl FnMut(u64) -> anyhow::Result<Option<RejectReason>>) -> anyhow::Result<u64> {
    // Once the doubling stops: lo <= last + 1 < hi.
    let (mut lo, mut hi) = (1u64, 1u64);
    loop {
        match probe(hi)? {
            Some(RejectReason::Duplicate) => (lo, hi) = (hi + 1, hi * 2),
            Some(RejectReason::NonceGap) => break,
            _ => return Ok(hi),
        }
    }
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match probe(mid)? {
            Some(RejectReason::Duplicate) => lo = mid + 1,
            Some(RejectReason::NonceGap) => hi = mid,
            _ => return Ok(mid),
        }
    }
    bail!("nonce search did not converge")
}

fn main() -> anyhow::Result<()> {
    let only = std::env::args().nth(1);
    let aeron = connect(&std::env::var("AERON_DIR").unwrap_or_else(|_| "/dev/shm/aeron".into()))?;
    let ipc = "aeron:ipc".into_c_string();
    let ingress = aeron.async_add_publication(&ipc, INGRESS)?.poll_blocking(Duration::from_secs(10))?;
    let egress = aeron
        .async_add_subscription(&ipc, EGRESS, Handlers::no_available_image_handler(), Handlers::no_unavailable_image_handler())?
        .poll_blocking(Duration::from_secs(10))?;

    // Polls egress until `until`, feeding every batch to `ctx` and `events`. Returns as soon as the
    // batch answering `want` arrives, with its seq and rejection reason.
    let drain = |events: &mut Vec<Emitted>, ctx: &mut Ctx, want: Option<(AccountId, u64)>, until: Instant| -> anyhow::Result<Option<(u64, Option<RejectReason>)>> {
        let mut found = None;
        while Instant::now() < until && found.is_none() {
            egress.poll_once(|msg: &[u8], _h: AeronHeader| {
                if let Ok(b) = decode::<EventBatch>(msg) {
                    ctx.observe(&b.events);
                    if want.is_some() && b.origin == want { found = Some((b.seq, rejection(&b.events))); }
                    events.extend(b.events);
                }
            }, 64)?;
            if want.is_none() { std::thread::sleep(Duration::from_millis(1)); }
        }
        Ok(found)
    };
    // Offers one command and waits for its batch.
    let send = |actor: AccountId, nonce: u64, action: Action, events: &mut Vec<Emitted>, ctx: &mut Ctx| -> anyhow::Result<(u64, Option<RejectReason>)> {
        let bytes = encode(&ClientCommand { actor, nonce, action });
        while ingress.offer(&bytes, Handlers::no_reserved_value_supplier_handler()) <= 0 {
            std::thread::yield_now();
        }
        drain(events, ctx, Some((actor, nonce)), Instant::now() + Duration::from_secs(5))?
            .ok_or_else(|| anyhow!("no response to {actor:?} nonce {nonce}"))
    };

    // Strict nonces (engine.md §2): each actor's next nonce is exactly `last + 1`.
    let run = now_ms();
    let mut nonces: BTreeMap<AccountId, u64> = BTreeMap::new();
    // OPERATOR and ARBITER keep their nonces across runs, so find them. A probe is a harmless
    // `Withdraw { amount: 0 }`: rejected `InvalidInput`, it changes nothing but the nonce.
    for role in [OPERATOR, ARBITER] {
        let last = sync_nonce(|n| Ok(send(role, n, Action::Withdraw { amount: 0 }, &mut Vec::new(), &mut Ctx::new(0))?.1))?;
        nonces.insert(role, last);
    }
    let mut failed = 0;

    for s in all().into_iter().filter(|s| only.as_deref().is_none_or(|o| o == s.name)) {
        let t0 = now_ms() + 500;
        let mut ctx = Ctx::new(t0);
        let mut events: Vec<Emitted> = Vec::new();
        println!("== {} ==", s.name);
        for step in &s.steps {
            let at = t0 + step.at;
            while now_ms() < at {
                drain(&mut events, &mut ctx, None, Instant::now() + Duration::from_millis(1))?;
            }
            if let StepKind::Send { label, actor, action } = &step.kind {
                let actor = run_account(run, *actor);
                let n = nonces.entry(actor).or_insert(0);
                *n += 1;
                let nonce = *n;
                let action = remap(action(&ctx), |a| run_account(run, a));
                let (seq, _) = send(actor, nonce, action, &mut events, &mut ctx)
                    .map_err(|e| anyhow!("{}: step {label}: {e}", s.name))?;
                ctx.record_label(*label, seq);
            }
        }
        drain(&mut events, &mut ctx, None, Instant::now() + Duration::from_millis(1_000))?;
        for e in &events {
            println!("  {:?} {:?}", e.audience, e.event);
        }
        match contains_in_order(&events, &s.expect) {
            Ok(()) => println!("PASS {}", s.name),
            Err(e) => { println!("FAIL {}: {e}", s.name); failed += 1; }
        }
    }
    if failed > 0 { bail!("{failed} scenario(s) failed") }
    Ok(())
}
```
**Accounts and nonces (Gate A #6 + #16).** The engine accepts only `nonce == last + 1` per actor, and the demo runs against a long-lived node, so it can't start every run at 1 for the same accounts. Per the ruling it derives per-run account ids (`run_account`, offset by the run's start time), so every scenario account starts at nonce 1 and nothing has to be read. The two genesis roles that send commands, `OPERATOR` and `ARBITER`, can't be renamed; `sync_nonce` finds their next nonce by probing (a fresh node costs one probe each). A rejected scenario command still consumes its nonce, so the per-actor counter stays in step with the engine.

The `rusteron` calls here (`poll_once`, `offer(bytes, handler)`, `Handlers::no_*_handler()`) predate Task 0: adjust them to the Verified API in transport-aeron.md as Task 10 does (`poll_fn`, `offer(&bytes)` with `is_retryable()`, `Handlers::NONE`). Declare `drain`/`send` `mut` only if the verified poll call needs `&mut self`.

`ctx.record_label` runs after the response arrives, so a step's own action closure must never refer to its own label. The scenario steps already follow that rule.

Add `"crates/rfq-demo"` to the workspace `members`.

- [ ] **Step 2: Docker files**

`docker/rust.Dockerfile`:
```dockerfile
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
ARG BIN
RUN cargo build --release --bin ${BIN} && cp target/release/${BIN} /app

FROM debian:bookworm-slim
COPY --from=build /app /usr/local/bin/app
ENTRYPOINT ["/usr/local/bin/app"]
```
If Task 0 needed apt packages, add them to the build stage.

`docker-compose.yml`:
```yaml
services:
  aeron:
    build: docker/aeron
    ipc: shareable
    shm_size: 512m
    volumes: [archive:/data/archive]
  engine:
    build: { context: ., dockerfile: docker/rust.Dockerfile, args: { BIN: rfq-node } }
    ipc: "service:aeron"
    environment: { RFQ_JOURNAL: archive, RFQ_HASH_EVERY_BATCH: "1" }
    depends_on: [aeron]
  demo:
    build: { context: ., dockerfile: docker/rust.Dockerfile, args: { BIN: rfq-demo } }
    ipc: "service:aeron"
    depends_on: [engine]
    profiles: [demo]
volumes:
  archive: {}
```

- [ ] **Step 3: Write the demo and crash-recovery script**

`scripts/demo.sh`:
```bash
#!/usr/bin/env bash
# Runs all scenarios over Aeron, then kills the engine and proves replay restores the same state.
set -euo pipefail
cd "$(dirname "$0")/.."

docker compose down -v --remove-orphans
docker compose up -d --build aeron engine
sleep 3

docker compose run --rm demo

before=$(docker compose logs engine | grep -E 'applied|replayed' | tail -1 | sed -E 's/.*seq=([0-9]+).*hash=([0-9a-f]+).*/\1 \2/')
echo "before crash: seq+hash = $before"

docker compose kill engine
docker compose up -d engine
sleep 3

after=$(docker compose logs engine | grep replayed | tail -1 | sed -E 's/.*seq=([0-9]+).*hash=([0-9a-f]+).*/\1 \2/')
echo "after replay: seq+hash = $after"

if [ "$before" = "$after" ]; then echo "RECOVERY OK"; else echo "RECOVERY MISMATCH"; exit 1; fi
```
```bash
chmod +x scripts/demo.sh
```

- [ ] **Step 4: Run it end to end**

Run: `./scripts/demo.sh`
Expected: six `PASS <scenario>` lines (happy_path, mm_cannot_withdraw_after_selection, no_quotes_expire, requester_lapses, disputed_ruled_invalid, oracle_silent_hard_deadline), then `RECOVERY OK`. The demo can also be re-run against the same engine (`docker compose run --rm demo`): scenario accounts are fresh per run and the genesis roles' nonces are re-synced.

If the order of `PositionSettled` and other events across scenarios gets interleaved, that's expected, because `contains_in_order` tolerates interleaving. A `FAIL` means a real ordering bug: debug it with `cargo test -p rfq-core --test scenarios` first.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/rfq-demo docker docker-compose.yml scripts
git commit -m "feat(demo): scenarios over Aeron in Docker with crash-recovery replay check"
```

