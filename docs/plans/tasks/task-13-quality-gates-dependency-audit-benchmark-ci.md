# Task 13: Quality gates (dependency audit, benchmark, CI)

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Files:**
- Create: `deny.toml`, `crates/rfq-core/benches/apply.rs`, `.github/workflows/ci.yml`
- Modify: `Cargo.toml` (criterion), `crates/rfq-core/Cargo.toml` (bench target), `docs/architecture/overview.md` §8 (measured numbers), `README.md` (CI badge)

**Interfaces:**
- Consumes: `Engine`, `testkit::{accounts, dollars, genesis, price}`.

- [ ] **Step 1: Dependency audit**

```bash
cargo install cargo-deny --locked
```
`deny.toml`:
```toml
[advisories]
version = 2
yanked = "deny"

[licenses]
version = 2
allow = ["MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Unicode-3.0", "Zlib"]

[bans]
multiple-versions = "warn"

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```
Run: `cargo deny check`
Expected: `advisories ok, bans ok, licenses ok, sources ok`. If a licence fails, add it only after checking that it's permissive, and note why in a comment. If an advisory fails, upgrade the crate, or add an `ignore` entry with the advisory id and the reason it doesn't apply.

- [ ] **Step 2: Write the benchmark**

Add `criterion = "0.5"` to `[workspace.dependencies]`. In `crates/rfq-core/Cargo.toml` add:
```toml
[dev-dependencies]
criterion = { workspace = true }

[[bench]]
name = "apply"
harness = false
```
`crates/rfq-core/benches/apply.rs`:
```rust
//! Engine throughput: replay a ~120k-command log of full RFQ rounds; cost of state_hash.
use std::collections::BTreeMap;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use rfq_core::testkit::{accounts::*, dollars, genesis, price};
use rfq_core::*;

const ROUNDS: u64 = 20_000; // 6 commands per round

struct Builder {
    engine: Engine,
    log: Vec<SequencedCommand>,
    seq: u64,
    nonces: BTreeMap<AccountId, u64>,
}

impl Builder {
    fn push(&mut self, time: Timestamp, cmd: Command) {
        self.seq += 1;
        let sc = SequencedCommand { seq: self.seq, time, cmd };
        self.engine.apply(&sc);
        self.log.push(sc);
    }

    /// Nonces are strict per actor: exactly `last + 1` (engine.md §2).
    fn user(&mut self, time: Timestamp, actor: AccountId, action: Action) {
        let n = self.nonces.entry(actor).or_insert(0);
        *n += 1;
        let nonce = *n;
        self.push(time, Command::User { actor, nonce, action });
    }
}

fn build_log() -> Vec<SequencedCommand> {
    let mut b = Builder { engine: Engine::new(), log: Vec::new(), seq: 0, nonces: BTreeMap::new() };
    b.push(0, Command::Genesis(genesis(Config::default())));
    for a in [ALICE, MM1, MM2] {
        b.user(0, OPERATOR, Action::Deposit { to: a, amount: dollars(1_000_000_000) });
    }
    let c = MarketId(1);
    b.user(0, OPERATOR, Action::ListMarket {
        id: c,
        description: "bench".into(),
        oracle: ORACLE,
        trading_close: u64::MAX / 4,
        resolution_deadline: u64::MAX / 2,
        dispute_window: 1,
    });
    let mut t = 0;
    for _ in 0..ROUNDS {
        t += 2_000;
        let r = RequestId(b.seq + 1);
        b.user(t, ALICE, Action::SubmitRequest {
            legs: vec![LegSpec { market: c, side: Side::BuyYes, qty: 10, limit_price: price(7_000) }],
            response_deadline: t + 1_000,
        });
        b.user(t, MM1, Action::SubmitQuote { request: r, leg: 0, price: price(6_100), size: 10, expires_at: t + 100_000 });
        let q = QuoteId(b.seq + 1);
        b.user(t, MM2, Action::SubmitQuote { request: r, leg: 0, price: price(6_000), size: 10, expires_at: t + 100_000 });
        b.push(t + 1_000, Command::Tick);
        b.user(t + 1_000, ALICE, Action::Accept { request: r, picks: vec![Pick { leg: 0, quote: q, price: price(6_000) }] });
    }
    // Each round is request, quote, quote, tick, accept: the last request is seq - 4.
    // If this fails, the workload is being silently rejected and the numbers are meaningless.
    assert_eq!(b.engine.request(RequestId(b.seq - 4)).map(|r| r.state), Some(RequestState::Filled));
    b.log
}

fn engine(c: &mut Criterion) {
    let log = build_log();
    let mut g = c.benchmark_group("engine");
    g.sample_size(10);
    g.throughput(Throughput::Elements(log.len() as u64));
    g.bench_function("replay_log", |bench| {
        bench.iter_batched(
            Engine::new,
            |mut e| {
                for sc in &log {
                    e.apply(sc);
                }
                e
            },
            BatchSize::LargeInput,
        )
    });
    let mut full = Engine::new();
    for sc in &log {
        full.apply(sc);
    }
    g.throughput(Throughput::Elements(1));
    g.bench_function("state_hash", |bench| bench.iter(|| full.state_hash()));
    g.finish();
}

criterion_group!(benches, engine);
criterion_main!(benches);
```

- [ ] **Step 3: Run it and record the numbers**

Run: `cargo bench -p rfq-core --bench apply`
Expected: criterion prints `replay_log` throughput in elements/s (commands per second) and the `state_hash` time.

Replace the first scaling bullet in `docs/architecture/overview.md` §8 with the measured value, keeping the machine and conditions honest:
```markdown
- **Measured:** `Engine::apply` replays a 120k-command log of full RFQ rounds at **<N> commands/s** on <machine> (single thread, release build, `overflow-checks` on); `state_hash` over that state takes <T> ms. Run `cargo bench -p rfq-core` to reproduce.
```
If the throughput is surprisingly low (under ~100k commands/s), profile before claiming anything: `cargo install flamegraph && cargo flamegraph --bench apply -p rfq-core -- --bench`.

- [ ] **Step 4: CI workflow**

`.github/workflows/ci.yml`:
```yaml
name: ci
on:
  push:
    branches: [main]
  pull_request:

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace
      - run: cargo bench -p rfq-core --no-run
  deny:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2
```
If Task 0 needed extra apt packages to build `rusteron`, add `- run: sudo apt-get update && sudo apt-get install -y <packages>` before the clippy step.

Add the badge as the README's second line:
```markdown
[![ci](https://github.com/hmdsefi/rfq/actions/workflows/ci.yml/badge.svg)](https://github.com/hmdsefi/rfq/actions/workflows/ci.yml)
```

- [ ] **Step 5: Commit, push, and check CI**

```bash
git add Cargo.toml crates/rfq-core deny.toml .github README.md docs/architecture/overview.md
git commit -m "ci: cargo-deny, engine benchmark, GitHub Actions"
git push
gh run watch --exit-status
```
Expected: both jobs pass.

