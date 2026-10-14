# Task 11: Demo client, Docker Compose and crash-recovery script

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

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

use anyhow::bail;
use rfq_core::codec::{decode, encode, ClientCommand, EventBatch};
use rfq_core::scenarios::{all, contains_in_order, Ctx, StepKind};
use rfq_core::{AccountId, Emitted};
use rfq_node::aeron::{connect, EGRESS, INGRESS};
use rusteron_archive::*;

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }

fn main() -> anyhow::Result<()> {
    let only = std::env::args().nth(1);
    let aeron = connect(&std::env::var("AERON_DIR").unwrap_or_else(|_| "/dev/shm/aeron".into()))?;
    let ipc = "aeron:ipc".into_c_string();
    let ingress = aeron.async_add_publication(&ipc, INGRESS)?.poll_blocking(Duration::from_secs(10))?;
    let egress = aeron
        .async_add_subscription(&ipc, EGRESS, Handlers::no_available_image_handler(), Handlers::no_unavailable_image_handler())?
        .poll_blocking(Duration::from_secs(10))?;
    let nonce_base = now_ms() * 1_000; // strictly above any previous run's nonces
    let mut nonces: BTreeMap<AccountId, u64> = BTreeMap::new();
    let mut failed = 0;

    for s in all().into_iter().filter(|s| only.as_deref().map_or(true, |o| o == s.name)) {
        let t0 = now_ms() + 500;
        let mut ctx = Ctx::new(t0);
        let mut events: Vec<Emitted> = Vec::new();
        let mut drain = |events: &mut Vec<Emitted>, ctx: &mut Ctx, want: Option<(AccountId, u64)>, until: Instant| -> anyhow::Result<Option<u64>> {
            let mut found = None;
            while Instant::now() < until && found.is_none() {
                egress.poll_once(|msg: &[u8], _h: AeronHeader| {
                    if let Ok(b) = decode::<EventBatch>(msg) {
                        ctx.observe(&b.events);
                        if want.is_some() && b.origin == want { found = Some(b.seq); }
                        events.extend(b.events);
                    }
                }, 64)?;
                if want.is_none() { std::thread::sleep(Duration::from_millis(1)); }
            }
            Ok(found)
        };
        println!("== {} ==", s.name);
        for step in &s.steps {
            let at = t0 + step.at;
            while now_ms() < at {
                drain(&mut events, &mut ctx, None, Instant::now() + Duration::from_millis(1))?;
            }
            if let StepKind::Send { label, actor, action } = &step.kind {
                let n = nonces.entry(*actor).or_insert(nonce_base);
                *n += 1;
                let cmd = ClientCommand { actor: *actor, nonce: *n, action: action(&ctx) };
                let bytes = encode(&cmd);
                while ingress.offer(&bytes, Handlers::no_reserved_value_supplier_handler()) <= 0 {
                    std::thread::yield_now();
                }
                match drain(&mut events, &mut ctx, Some((*actor, *n)), Instant::now() + Duration::from_secs(5))? {
                    Some(seq) => ctx.record_label(*label, seq),
                    None => bail!("{}: no response to step {label}", s.name),
                }
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
Expected: six `PASS <scenario>` lines (happy_path, mm_withdraws_after_selection, no_quotes_expire, requester_lapses, disputed_ruled_invalid, oracle_silent_hard_deadline), then `RECOVERY OK`.

If the order of `PositionSettled` and other events across scenarios gets interleaved, that's expected, because `contains_in_order` tolerates interleaving. A `FAIL` means a real ordering bug: debug it with `cargo test -p rfq-core --test scenarios` first.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/rfq-demo docker docker-compose.yml scripts
git commit -m "feat(demo): scenarios over Aeron in Docker with crash-recovery replay check"
```

