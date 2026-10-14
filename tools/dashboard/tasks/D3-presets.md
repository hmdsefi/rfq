# Task D3: Presets (brief scenarios, attacks, sandbox, runner)

**Goal:**
- The six brief scenarios, taken unchanged from `rfq_testkit::scenarios::all()`, plus six attack scripts and the sandbox setup, all as steppable presets.
- A `Runner` that plays them one step at a time against a `Session` and gives the same PASS or FAIL verdict as the tests (`contains_in_order`).

**Files:**
- Create: `tools/dashboard/src/presets.rs`
- Modify: `tools/dashboard/src/lib.rs` (add `pub mod presets;`)
- Test: unit tests inside `src/presets.rs`

**Interfaces:**
- Consumes:
  - from D2: `Session`, `Applied` and `SessionError`, with `Session::{set_time, act, raw, tick, next_seq, last_nonce, engine, replay_check}`;
  - from the testkit: `rfq_testkit::scenarios::{all, demo_config, contains_in_order, Ctx, Scenario, StepKind}`, `rfq_testkit::{dollars, price}`, `rfq_testkit::accounts::*`.
- Produces, used by D4:
  - `pub const T0: Timestamp = 1_000;`
  - `pub enum Nonce { Next, Reuse, Skip }`
  - `pub enum Move { Send { label: &'static str, actor: AccountId, nonce: Nonce, action: ActionFn }, Tick }`
  - `pub struct Script { pub at: Duration, pub mv: Move }`
  - `pub enum PresetKind { Brief, Attack }` (Serialize, snake_case)
  - `pub struct Preset { pub name: &'static str, pub kind: PresetKind, pub summary: &'static str, pub steps: Vec<Script>, pub expect: Vec<&'static str> }`
  - `pub fn all() -> Vec<Preset>`, `pub fn by_name(&str) -> Option<Preset>`, `pub fn sandbox() -> Vec<Script>`
  - `pub struct Runner` with:
    - `new(Preset) -> Runner`
    - `preset(&self) -> &Preset`
    - `position(&self) -> usize`
    - `finished(&self) -> bool`
    - `next_label(&self) -> Option<&'static str>`
    - `step(&mut self, &mut Session) -> Result<Option<Applied>, SessionError>`
    - `verdict(&self) -> Option<Result<(), String>>`

## Steps

- [ ] **Step 1: Write the failing tests.** Create `src/presets.rs` with only this test module, and add `pub mod presets;` to `src/lib.rs` after `pub mod session;`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rfq_testkit::accounts::*;
    use rfq_testkit::dollars;
    use rfq_testkit::scenarios::demo_config;

    fn run(p: Preset) -> (Session, Runner) {
        let (mut s, _) = Session::new(demo_config());
        let mut r = Runner::new(p);
        while r.step(&mut s).expect("preset step").is_some() {}
        (s, r)
    }

    fn preset(name: &str) -> Preset {
        by_name(name).unwrap_or_else(|| panic!("no preset {name}"))
    }

    #[test]
    fn catalogue_has_six_brief_and_six_attack_presets() {
        let all = all();
        let brief: Vec<&str> =
            all.iter().filter(|p| p.kind == PresetKind::Brief).map(|p| p.name).collect();
        let attack = all.iter().filter(|p| p.kind == PresetKind::Attack).count();
        assert_eq!(
            brief,
            vec![
                "happy_path",
                "mm_cannot_withdraw_after_selection",
                "no_quotes_expire",
                "requester_lapses",
                "disputed_ruled_invalid",
                "oracle_silent_hard_deadline",
            ]
        );
        assert_eq!(attack, 6);
        assert!(all.iter().all(|p| !p.summary.is_empty() && !p.expect.is_empty()));
    }

    #[test]
    fn every_preset_passes_and_replays_to_the_same_hash() {
        for p in all() {
            let name = p.name;
            let (s, r) = run(p);
            assert!(r.finished(), "{name} did not finish");
            assert_eq!(r.verdict(), Some(Ok(())), "{name}");
            assert!(s.replay_check().matches(), "{name} replay");
            assert_eq!(s.engine().check_invariants(), Ok(()), "{name} invariants");
        }
    }

    #[test]
    fn verdict_is_none_until_the_script_ends() {
        let (mut s, _) = Session::new(demo_config());
        let mut r = Runner::new(preset("happy_path"));
        assert_eq!(r.verdict(), None);
        assert_eq!(r.next_label(), Some("fund_alice"));
        r.step(&mut s).expect("step");
        assert_eq!(r.position(), 1);
        assert_eq!(r.verdict(), None);
    }

    #[test]
    fn double_accept_escrows_once() {
        let (s, _) = run(preset("attack_double_accept"));
        // Alice buys 1000 YES at 0.60: she escrows exactly $600, once.
        assert_eq!(s.engine().balance(ALICE).escrowed, dollars(600));
    }

    #[test]
    fn over_limit_quote_never_wins() {
        let (s, _) = run(preset("attack_over_limit_quote"));
        let selected: Vec<AccountId> = s
            .engine()
            .quotes()
            .filter(|q| q.state == QuoteState::Selected)
            .map(|q| q.mm)
            .collect();
        assert_eq!(selected, vec![MM1]);
    }

    #[test]
    fn replayed_nonces_consume_nothing() {
        let (s, _) = run(preset("attack_replayed_nonce"));
        // quote = nonce 1, the replay and the gap are rejected, the cancel is nonce 2.
        assert_eq!(s.last_nonce(MM1), 2);
    }

    #[test]
    fn sandbox_funds_four_accounts_and_lists_one_contract() {
        let (mut s, _) = Session::new(demo_config());
        let mut ctx = Ctx::new(T0);
        for script in sandbox() {
            play(&mut s, &mut ctx, &script).expect("sandbox step");
        }
        for a in [ALICE, CAROL, MM1, MM2] {
            assert_eq!(s.engine().balance(a).free, dollars(1_000));
        }
        assert_eq!(s.engine().contracts().count(), 1);
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail.**

Run: `cd tools/dashboard && cargo test presets`
Expected: compile errors (`Preset`, `Runner`, `all` and `play` are not defined yet).

- [ ] **Step 3: Implement.** Put this above the test module in `src/presets.rs`:

```rust
//! Steppable scripts: the six brief scenarios from `rfq_testkit::scenarios` (the same scripts the
//! tests and the Aeron demo run, untouched), six attacks, and the sandbox setup.
use serde::Serialize;

use rfq_domain::*;
use rfq_testkit::accounts::*;
use rfq_testkit::scenarios::{self, Ctx, StepKind};
use rfq_testkit::{dollars, price};

use crate::session::{Applied, Session, SessionError};

/// Every preset's clock starts here, so its contract id (`ContractId(T0)`) is stable.
pub const T0: Timestamp = 1_000;

pub type ActionFn = Box<dyn Fn(&Ctx) -> Action + Send + Sync>;

/// Which nonce a scripted command uses. Only the attack scripts use anything but `Next`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nonce {
    /// The actor's next nonce.
    Next,
    /// The nonce the actor used last: a replay.
    Reuse,
    /// One past the next nonce: a gap.
    Skip,
}

pub enum Move {
    Send { label: &'static str, actor: AccountId, nonce: Nonce, action: ActionFn },
    Tick,
}

pub struct Script {
    /// Offset from `T0`, in ms.
    pub at: Duration,
    pub mv: Move,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetKind {
    Brief,
    Attack,
}

pub struct Preset {
    pub name: &'static str,
    pub kind: PresetKind,
    pub summary: &'static str,
    pub steps: Vec<Script>,
    /// Event names that must appear in this order (`contains_in_order`).
    pub expect: Vec<&'static str>,
}

pub fn all() -> Vec<Preset> {
    let mut presets: Vec<Preset> = scenarios::all().into_iter().map(from_testkit).collect();
    presets.extend(attacks());
    presets
}

pub fn by_name(name: &str) -> Option<Preset> {
    all().into_iter().find(|p| p.name == name)
}

/// Funded accounts and one contract with long windows, for manual play.
pub fn sandbox() -> Vec<Script> {
    setup_with(60_000, 120_000, 5_000)
}

/// Plays a preset one step at a time and checks its expectations at the end.
pub struct Runner {
    preset: Preset,
    ctx: Ctx,
    next: usize,
    events: Vec<Emitted>,
}

impl Runner {
    pub fn new(preset: Preset) -> Self {
        Runner { preset, ctx: Ctx::new(T0), next: 0, events: Vec::new() }
    }

    pub fn preset(&self) -> &Preset {
        &self.preset
    }

    /// How many steps have run.
    pub fn position(&self) -> usize {
        self.next
    }

    pub fn finished(&self) -> bool {
        self.next >= self.preset.steps.len()
    }

    /// The next step's label (`"tick"` for a clock tick), for the "next:" hint.
    pub fn next_label(&self) -> Option<&'static str> {
        self.preset.steps.get(self.next).map(|s| match &s.mv {
            Move::Send { label, .. } => *label,
            Move::Tick => "tick",
        })
    }

    /// Runs the next step. `Ok(None)` once the script is finished.
    pub fn step(&mut self, s: &mut Session) -> Result<Option<Applied>, SessionError> {
        let Some(script) = self.preset.steps.get(self.next) else {
            return Ok(None);
        };
        let applied = play(s, &mut self.ctx, script)?;
        self.events.extend(applied.events.iter().cloned());
        self.next += 1;
        Ok(Some(applied))
    }

    /// `contains_in_order` over everything this run emitted, once the script is finished.
    pub fn verdict(&self) -> Option<Result<(), String>> {
        self.finished()
            .then(|| scenarios::contains_in_order(&self.events, &self.preset.expect))
    }
}

/// Plays one scripted step at `T0 + at`.
pub(crate) fn play(s: &mut Session, ctx: &mut Ctx, script: &Script) -> Result<Applied, SessionError> {
    let at = T0.checked_add(script.at).ok_or(SessionError::Overflow)?;
    s.set_time(at)?;
    let applied = match &script.mv {
        Move::Tick => s.tick(),
        Move::Send { label, actor, nonce, action } => {
            ctx.record_label(label, s.next_seq());
            let a = action(ctx);
            let last = s.last_nonce(*actor);
            match nonce {
                Nonce::Next => s.act(*actor, a),
                Nonce::Reuse => s.raw(*actor, last, a),
                Nonce::Skip => s.raw(*actor, last.saturating_add(2), a),
            }
        }
    };
    ctx.observe(&applied.events);
    Ok(applied)
}

fn from_testkit(s: scenarios::Scenario) -> Preset {
    let steps = s
        .steps
        .into_iter()
        .map(|step| Script {
            at: step.at,
            mv: match step.kind {
                StepKind::Send { label, actor, action } => {
                    Move::Send { label, actor, nonce: Nonce::Next, action }
                }
                StepKind::Tick => Move::Tick,
            },
        })
        .collect();
    Preset { name: s.name, kind: PresetKind::Brief, summary: brief_summary(s.name), steps, expect: s.expect }
}

fn brief_summary(name: &str) -> &'static str {
    match name {
        "happy_path" => "Two MMs quote, the best price is presented, Alice accepts, the oracle proposes YES and the contract settles.",
        "mm_cannot_withdraw_after_selection" => "MM2 tries to cancel its selected quote inside the accept window: rejected, the quote is firm, Alice's accept fills.",
        "no_quotes_expire" => "Nobody quotes: at the deadline the request expires and Alice's hold is released.",
        "requester_lapses" => "Alice never answers: at accept_by the request is rejected and every hold is released.",
        "disputed_ruled_invalid" => "Carol disputes the proposed outcome with a bond; the arbiter rules INVALID and the positions are refunded.",
        "oracle_silent_hard_deadline" => "The oracle never proposes: at the hard deadline the contract resolves INVALID and refunds everyone.",
        _ => "Scenario from rfq-testkit.",
    }
}

fn send(
    at: Duration,
    label: &'static str,
    actor: AccountId,
    f: impl Fn(&Ctx) -> Action + Send + Sync + 'static,
) -> Script {
    send_with(at, label, actor, Nonce::Next, f)
}

fn send_with(
    at: Duration,
    label: &'static str,
    actor: AccountId,
    nonce: Nonce,
    f: impl Fn(&Ctx) -> Action + Send + Sync + 'static,
) -> Script {
    Script { at, mv: Move::Send { label, actor, nonce, action: Box::new(f) } }
}

fn tick(at: Duration) -> Script {
    Script { at, mv: Move::Tick }
}

/// Everyone funded with $1,000; one contract closing at `close`, hard deadline at `hard`.
fn setup_with(close: Duration, hard: Duration, window: Duration) -> Vec<Script> {
    let mut steps: Vec<Script> =
        [("fund_alice", ALICE), ("fund_mm1", MM1), ("fund_mm2", MM2), ("fund_carol", CAROL)]
            .into_iter()
            .map(|(label, to)| {
                send(0, label, OPERATOR, move |_| Action::Deposit { to, amount: dollars(1_000) })
            })
            .collect();
    steps.push(send(0, "list", OPERATOR, move |c| Action::ListContract {
        id: c.contract(),
        description: "Will it rain in Paris on 2026-11-01?".into(),
        oracle: ORACLE,
        trading_close: c.at(close),
        resolution_deadline: c.at(hard),
        dispute_window: window,
    }));
    steps
}

/// The testkit's setup: close at +5 s, hard deadline at +15 s, 2 s dispute window.
fn setup() -> Vec<Script> {
    setup_with(5_000, 15_000, 2_000)
}

/// Alice asks for 1000 YES with a sealed 0.70 limit; quotes due at +1.5 s.
fn request(at: Duration) -> Script {
    send(at, "req", ALICE, |c| Action::SubmitRequest {
        legs: vec![LegSpec {
            contract: c.contract(),
            side: Side::BuyYes,
            qty: 1_000,
            limit_price: price(7_000),
        }],
        response_deadline: c.at(1_500),
    })
}

fn quote(at: Duration, label: &'static str, mm: AccountId, bps: u16) -> Script {
    send(at, label, mm, move |c| Action::SubmitQuote {
        request: c.request("req"),
        leg: 0,
        price: price(bps),
        size: 1_000,
        expires_at: c.at(10_000),
    })
}

/// Setup, the request, MM1 at 0.62 and MM2 at 0.60, presented at +1.6 s.
fn quoted() -> Vec<Script> {
    let mut s = setup();
    s.push(request(100));
    s.push(quote(300, "q1", MM1, 6_200));
    s.push(quote(400, "q2", MM2, 6_000));
    s.push(tick(1_600));
    s
}

fn accept(at: Duration, label: &'static str) -> Script {
    send(at, label, ALICE, |c| Action::Accept {
        request: c.request("req"),
        picks: c.picks(c.request("req")),
    })
}

fn attacks() -> Vec<Preset> {
    let wrong_price = {
        let mut s = quoted();
        s.push(send(1_800, "bad_accept", ALICE, |c| {
            let request = c.request("req");
            let picks = c.picks(request).into_iter().map(|p| Pick { price: price(6_100), ..p }).collect();
            Action::Accept { request, picks }
        }));
        s.push(accept(1_900, "accept"));
        Preset {
            name: "attack_accept_wrong_price",
            kind: PresetKind::Attack,
            summary: "Alice accepts at 0.61 instead of the presented 0.60: rejected (Mismatch); the quote stays firm and her real accept fills.",
            steps: s,
            expect: vec!["RequestPresented", "CommandRejected(Mismatch)", "QuoteFilled", "RequestFilled"],
        }
    };
    let double_accept = {
        let mut s = quoted();
        s.push(accept(1_800, "accept"));
        s.push(accept(1_900, "accept_again"));
        Preset {
            name: "attack_double_accept",
            kind: PresetKind::Attack,
            summary: "Alice accepts twice: the second is rejected (InvalidState); escrow happens exactly once.",
            steps: s,
            expect: vec!["RequestFilled", "CommandRejected(InvalidState)"],
        }
    };
    let replayed_nonce = {
        let mut s = setup();
        s.push(request(100));
        s.push(quote(300, "q1", MM1, 6_200));
        s.push(send_with(400, "q1_replayed", MM1, Nonce::Reuse, |c| Action::SubmitQuote {
            request: c.request("req"),
            leg: 0,
            price: price(5_000),
            size: 1_000,
            expires_at: c.at(10_000),
        }));
        s.push(send_with(500, "q1_gap", MM1, Nonce::Skip, |c| Action::CancelQuote {
            quote: c.quote("q1"),
        }));
        s.push(send(600, "q1_cancel", MM1, |c| Action::CancelQuote { quote: c.quote("q1") }));
        Preset {
            name: "attack_replayed_nonce",
            kind: PresetKind::Attack,
            summary: "MM1 replays a used nonce, then skips one: Duplicate, then NonceGap, and neither consumes anything; its next real command still works.",
            steps: s,
            expect: vec![
                "QuoteSubmitted",
                "CommandRejected(Duplicate)",
                "CommandRejected(NonceGap)",
                "QuoteCancelled",
            ],
        }
    };
    let over_limit = {
        let mut s = setup();
        s.push(request(100));
        s.push(quote(300, "q1", MM1, 6_200));
        s.push(quote(400, "q_over", MM2, 7_500));
        s.push(tick(1_600));
        Preset {
            name: "attack_over_limit_quote",
            kind: PresetKind::Attack,
            summary: "MM2 quotes 0.75 above Alice's sealed 0.70 limit: accepted as Active, never selected; MM1's 0.62 wins.",
            steps: s,
            expect: vec!["QuoteSubmitted", "QuoteSubmitted", "QuoteLost", "QuoteSelected", "RequestPresented"],
        }
    };
    let self_trade = {
        let mut s = setup();
        s.push(request(100));
        s.push(quote(300, "self_quote", ALICE, 6_000));
        Preset {
            name: "attack_self_trade",
            kind: PresetKind::Attack,
            summary: "Alice quotes on her own request: rejected (SelfTrade).",
            steps: s,
            expect: vec!["RequestOpened", "CommandRejected(SelfTrade)"],
        }
    };
    let cancel_others = {
        let mut s = setup();
        s.push(request(100));
        s.push(quote(300, "q1", MM1, 6_200));
        s.push(send(400, "carol_cancel", CAROL, |c| Action::CancelQuote { quote: c.quote("q1") }));
        Preset {
            name: "attack_cancel_others_quote",
            kind: PresetKind::Attack,
            summary: "Carol tries to cancel MM1's quote: rejected (Unauthorized); the quote stays Active.",
            steps: s,
            expect: vec!["QuoteSubmitted", "CommandRejected(Unauthorized)"],
        }
    };
    vec![wrong_price, double_accept, replayed_nonce, over_limit, self_trade, cancel_others]
}
```

- [ ] **Step 4: Run the tests.**

Run: `cd tools/dashboard && cargo test`
Expected: all D2 and D3 tests pass.

**If an attack's `expect` fails, find out why before changing anything.** Print the event names with `rfq_testkit::names`. If the engine emits the right events in a different order (for example `QuoteSelected` before `QuoteLost`), fix the expectation to the engine's order and say so in the report. If the engine gives a **different rejection reason** from the one pinned here (Mismatch, InvalidState, Duplicate, NonceGap, SelfTrade, Unauthorized), stop and report it: the attack table in DESIGN.md §5 is taken from the engine's own tests, so a mismatch means the script is wrong, not the engine.

- [ ] **Step 5: Run the gates.**

Run: `cd tools/dashboard && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: clean. Run `cargo fmt` if needed.

- [ ] **Step 6: Commit.**

```bash
git add tools/dashboard/src/lib.rs tools/dashboard/src/presets.rs
git commit -m "feat(dashboard): brief scenarios, attack scripts and sandbox as steppable presets"
```
