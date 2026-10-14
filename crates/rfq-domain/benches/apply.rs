//! Engine throughput: replay a ~100k-command log of full RFQ rounds; cost of state_hash.
use std::collections::BTreeMap;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use rfq_domain::*;
use rfq_testkit::{accounts::*, dollars, genesis, price};

const ROUNDS: u64 = 20_000; // 5 commands per round: request, quote, quote, tick, accept

struct Builder {
    engine: Engine,
    log: Vec<SequencedCommand>,
    seq: u64,
    nonces: BTreeMap<AccountId, u64>,
}

impl Builder {
    fn push(&mut self, time: Timestamp, cmd: Command) {
        self.seq += 1;
        let sc = SequencedCommand {
            seq: self.seq,
            time,
            cmd,
        };
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
    let mut b = Builder {
        engine: Engine::new(),
        log: Vec::new(),
        seq: 0,
        nonces: BTreeMap::new(),
    };
    b.push(0, Command::Genesis(genesis(Config::default())));
    for a in [ALICE, MM1, MM2] {
        b.user(
            0,
            OPERATOR,
            Action::Deposit {
                to: a,
                amount: dollars(1_000_000_000),
            },
        );
    }
    let c = ContractId(1);
    b.user(
        0,
        OPERATOR,
        Action::ListContract {
            id: c,
            description: "bench".into(),
            oracle: ORACLE,
            trading_close: u64::MAX / 4,
            resolution_deadline: u64::MAX / 2,
            dispute_window: 1,
        },
    );
    let mut t = 0;
    for _ in 0..ROUNDS {
        t += 2_000;
        let r = RequestId(b.seq + 1);
        b.user(
            t,
            ALICE,
            Action::SubmitRequest {
                legs: vec![LegSpec {
                    contract: c,
                    side: Side::BuyYes,
                    qty: 10,
                    limit_price: price(7_000),
                }],
                response_deadline: t + 1_000,
            },
        );
        b.user(
            t,
            MM1,
            Action::SubmitQuote {
                request: r,
                leg: 0,
                price: price(6_100),
                size: 10,
                expires_at: t + 100_000,
            },
        );
        let q = QuoteId(b.seq + 1);
        b.user(
            t,
            MM2,
            Action::SubmitQuote {
                request: r,
                leg: 0,
                price: price(6_000),
                size: 10,
                expires_at: t + 100_000,
            },
        );
        b.push(t + 1_000, Command::Tick);
        b.user(
            t + 1_000,
            ALICE,
            Action::Accept {
                request: r,
                picks: vec![Pick {
                    leg: 0,
                    quote: q,
                    price: price(6_000),
                }],
            },
        );
    }
    // Each round is request, quote, quote, tick, accept: the last request is seq - 4.
    // If this fails, the workload is being silently rejected and the numbers are meaningless.
    assert_eq!(
        b.engine.request(RequestId(b.seq - 4)).map(|r| r.state),
        Some(RequestState::Filled)
    );
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
