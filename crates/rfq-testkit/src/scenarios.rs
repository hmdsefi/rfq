//! The happy path and the failure paths from the brief, as timed scripts.
//! The same scripts run in memory (tests) and over Aeron (rfq-demo).
use std::collections::BTreeMap;

use rfq_domain::*;

use crate::{Harness, accounts::*, dollars, price};

/// Short windows so the Aeron demo runs in seconds.
pub fn demo_config() -> Config {
    Config {
        accept_window: 2_000,
        min_response: 500,
        max_response: 60_000,
        max_qty: 1_000_000,
        dispute_bond: dollars(100),
        resume_grace: 2_000,
        min_request_notional: ONE_DOLLAR,
        reservation: ReservationPolicy::FullHold,
        selection: SelectionPolicy::SealedAtDeadline,
    }
}

pub struct Ctx {
    t0: Timestamp,
    labels: BTreeMap<&'static str, u64>,
    presented: BTreeMap<RequestId, Vec<Pick>>,
}

impl Ctx {
    pub fn new(t0: Timestamp) -> Self {
        Ctx {
            t0,
            labels: BTreeMap::new(),
            presented: BTreeMap::new(),
        }
    }
    pub fn at(&self, offset: Duration) -> Timestamp {
        self.t0 + offset
    }
    /// One contract per scenario run, keyed by its start time so reruns never collide.
    pub fn contract(&self) -> ContractId {
        ContractId(self.t0)
    }
    /// The run's `k`th extra market (`k` in 1..=9), for a multi-leg scenario: `ContractId(t0 * 10 + k)`.
    ///
    /// Distinct runs never collide: two start times that differ give `t0 * 10 + k` that differ (`k` is
    /// below 10), and for real millisecond clocks it can't equal another run's own `ContractId(t0)`
    /// either (that would need two start times a factor of 10 apart). The value stays below 2^53
    /// (a millisecond timestamp is about 1.8e12, so about 1.8e13 here), so the dashboard page can
    /// hold it in a JavaScript number. The run's scope owns these ids too (`rfq-demo/src/scope.rs`).
    pub fn contract_n(&self, k: u64) -> ContractId {
        assert!((1..=9).contains(&k), "a run has extra markets 1..=9, not {k}");
        ContractId(self.t0 * 10 + k)
    }
    pub fn record_label(&mut self, label: &'static str, seq: u64) {
        self.labels.insert(label, seq);
    }
    pub fn seq(&self, label: &str) -> u64 {
        *self
            .labels
            .get(label)
            .unwrap_or_else(|| panic!("label {label} has no seq yet"))
    }
    pub fn request(&self, label: &str) -> RequestId {
        RequestId(self.seq(label))
    }
    pub fn quote(&self, label: &str) -> QuoteId {
        QuoteId(self.seq(label))
    }
    pub fn picks(&self, request: RequestId) -> Vec<Pick> {
        self.presented.get(&request).cloned().unwrap_or_default()
    }
    pub fn observe(&mut self, events: &[Emitted]) {
        for e in events {
            if let Event::RequestPresented { request, picks, .. } = &e.event {
                self.presented.insert(*request, picks.clone());
            }
        }
    }
}

pub type ActionFn = Box<dyn Fn(&Ctx) -> Action + Send + Sync>;

pub enum StepKind {
    Send {
        label: &'static str,
        actor: AccountId,
        action: ActionFn,
    },
    Tick,
}

pub struct Step {
    /// Offset from the scenario start, in ms.
    pub at: Duration,
    pub kind: StepKind,
}

pub struct Scenario {
    pub name: &'static str,
    pub steps: Vec<Step>,
    /// Event names that must appear in this order (other events may be interleaved). A rejection
    /// can pin its reason: `"CommandRejected(InvalidState)"` (see [`contains_in_order`]).
    pub expect: Vec<&'static str>,
}

fn send(
    at: Duration,
    label: &'static str,
    actor: AccountId,
    f: impl Fn(&Ctx) -> Action + Send + Sync + 'static,
) -> Step {
    Step {
        at,
        kind: StepKind::Send {
            label,
            actor,
            action: Box::new(f),
        },
    }
}

fn tick(at: Duration) -> Step {
    Step {
        at,
        kind: StepKind::Tick,
    }
}

/// Deposits $1,000 for Alice, both market makers and Carol.
fn fund() -> Vec<Step> {
    [
        ("fund_alice", ALICE),
        ("fund_mm1", MM1),
        ("fund_mm2", MM2),
        ("fund_carol", CAROL),
    ]
    .into_iter()
    .map(|(label, to)| {
        send(0, label, OPERATOR, move |_| Action::Deposit {
            to,
            amount: dollars(1_000),
        })
    })
    .collect()
}

/// The operator lists a market that closes at +5 s, with a hard deadline at +15 s.
fn list(label: &'static str, id: fn(&Ctx) -> ContractId, description: &'static str) -> Step {
    send(0, label, OPERATOR, move |c| Action::ListContract {
        id: id(c),
        description: description.into(),
        oracle: ORACLE,
        trading_close: c.at(5_000),
        resolution_deadline: c.at(15_000),
        dispute_window: 2_000,
    })
}

/// Funds everyone and lists the scenario's contract (closes at +5 s, hard deadline at +15 s).
fn setup() -> Vec<Step> {
    let mut steps = fund();
    steps.push(list(
        "list",
        |c| c.contract(),
        "Will it rain in Paris on 2026-11-01?",
    ));
    steps
}

/// Funds everyone and lists three markets (Paris, London, Rome), each closing at +5 s.
fn three_markets() -> Vec<Step> {
    let mut steps = fund();
    steps.push(list(
        "list_paris",
        |c| c.contract_n(1),
        "Will it rain in Paris on 2026-11-01?",
    ));
    steps.push(list(
        "list_london",
        |c| c.contract_n(2),
        "Will it rain in London on 2026-11-01?",
    ));
    steps.push(list(
        "list_rome",
        |c| c.contract_n(3),
        "Will it rain in Rome on 2026-11-01?",
    ));
    steps
}

/// Request 1000 YES (limit 0.70), quotes at 0.62 and 0.60, presented at +1.5 s.
fn quoted() -> Vec<Step> {
    let mut s = setup();
    s.push(send(100, "req", ALICE, |c| Action::SubmitRequest {
        legs: vec![LegSpec {
            contract: c.contract(),
            side: Side::BuyYes,
            qty: 1_000,
            limit_price: price(7_000),
        }],
        response_deadline: c.at(1_500),
    }));
    s.push(send(300, "q1", MM1, |c| Action::SubmitQuote {
        request: c.request("req"),
        leg: 0,
        price: price(6_200),
        size: 1_000,
        expires_at: c.at(10_000),
    }));
    s.push(send(400, "q2", MM2, |c| Action::SubmitQuote {
        request: c.request("req"),
        leg: 0,
        price: price(6_000),
        size: 1_000,
        expires_at: c.at(10_000),
    }));
    s.push(tick(1_600));
    s
}

fn accept(at: Duration) -> Step {
    send(at, "accept", ALICE, |c| Action::Accept {
        request: c.request("req"),
        picks: c.picks(c.request("req")),
    })
}

fn filled() -> Vec<Step> {
    let mut s = quoted();
    s.push(accept(2_000));
    s
}

/// One request for 400 YES on each of the three markets (limit 0.70 each: $840 held), deadline +1.5 s.
fn three_leg_request() -> Step {
    send(100, "req", ALICE, |c| Action::SubmitRequest {
        legs: (1..=3)
            .map(|k| LegSpec {
                contract: c.contract_n(k),
                side: Side::BuyYes,
                qty: 400,
                limit_price: price(7_000),
            })
            .collect(),
        response_deadline: c.at(1_500),
    })
}

/// `mm` quotes leg `leg` of the request at `bps`, firm to +10 s (the engine clamps it to the accept window).
fn leg_quote(at: Duration, label: &'static str, mm: AccountId, leg: u32, bps: u16) -> Step {
    send(at, label, mm, move |c| Action::SubmitQuote {
        request: c.request("req"),
        leg,
        price: price(bps),
        size: 400,
        expires_at: c.at(10_000),
    })
}

pub fn all() -> Vec<Scenario> {
    let happy = {
        let mut s = filled();
        s.push(tick(5_100));
        s.push(send(5_200, "propose", ORACLE, |c| Action::ProposeOutcome {
            contract: c.contract(),
            outcome: Outcome::Yes,
        }));
        s.push(tick(7_300));
        Scenario {
            name: "happy_path",
            steps: s,
            expect: vec![
                "RequestOpened",
                "QuoteSubmitted",
                "QuoteSubmitted",
                "QuoteLost",
                "QuoteSelected",
                "RequestPresented",
                "QuoteFilled",
                "RequestFilled",
                "ContractClosed",
                "OutcomeProposed",
                "ContractFinalized",
                "PositionSettled",
            ],
        }
    };
    // The selected quote is firm (RFC P5): MM2's cancel inside the window is rejected and the accept fills.
    let withdraw = {
        let mut s = quoted();
        s.push(send(1_800, "cancel", MM2, |c| Action::CancelQuote {
            quote: c.quote("q2"),
        }));
        s.push(accept(1_900));
        Scenario {
            name: "mm_cannot_withdraw_after_selection",
            steps: s,
            expect: vec![
                "RequestPresented",
                "CommandRejected(InvalidState)",
                "QuoteFilled",
                "RequestFilled",
            ],
        }
    };
    let no_quotes = {
        let mut s = setup();
        s.push(send(100, "req", ALICE, |c| Action::SubmitRequest {
            legs: vec![LegSpec {
                contract: c.contract(),
                side: Side::BuyYes,
                qty: 1_000,
                limit_price: price(7_000),
            }],
            response_deadline: c.at(1_500),
        }));
        s.push(tick(1_600));
        Scenario {
            name: "no_quotes_expire",
            steps: s,
            expect: vec!["RequestOpened", "RequestExpired"],
        }
    };
    let lapse = {
        let mut s = quoted();
        s.push(tick(3_600));
        Scenario {
            name: "requester_lapses",
            steps: s,
            expect: vec!["RequestPresented", "QuoteReleased", "RequestRejected"],
        }
    };
    let disputed = {
        let mut s = filled();
        s.push(tick(5_100));
        s.push(send(5_200, "propose", ORACLE, |c| Action::ProposeOutcome {
            contract: c.contract(),
            outcome: Outcome::Yes,
        }));
        s.push(send(5_500, "dispute", CAROL, |c| Action::Dispute {
            contract: c.contract(),
        }));
        s.push(send(6_000, "rule", ARBITER, |c| Action::Rule {
            contract: c.contract(),
            outcome: Outcome::Invalid,
        }));
        Scenario {
            name: "disputed_ruled_invalid",
            steps: s,
            expect: vec![
                "RequestFilled",
                "OutcomeProposed",
                "Disputed",
                "Ruled",
                "ContractFinalized",
                "PositionSettled",
            ],
        }
    };
    let silent = {
        let mut s = filled();
        s.push(tick(15_100));
        Scenario {
            name: "oracle_silent_hard_deadline",
            steps: s,
            expect: vec![
                "RequestFilled",
                "ContractClosed",
                "ContractFinalized",
                "PositionSettled",
            ],
        }
    };
    // Multi-leg (engine.md §8): three markets, one request, a quote on every leg, and one accept fills
    // all three legs. MM1 backs legs 0 and 2, MM2 backs leg 1.
    let multi_leg_happy = {
        let mut s = three_markets();
        s.push(three_leg_request());
        s.push(leg_quote(300, "q1", MM1, 0, 6_200));
        s.push(leg_quote(400, "q2", MM2, 1, 6_000));
        s.push(leg_quote(500, "q3", MM1, 2, 6_400));
        s.push(tick(1_600));
        s.push(accept(2_000));
        Scenario {
            name: "multi_leg_happy",
            steps: s,
            expect: vec![
                "RequestOpened",
                "RequestOpened",
                "RequestOpened",
                "QuoteSubmitted",
                "QuoteSubmitted",
                "QuoteSubmitted",
                "QuoteSelected",
                "QuoteSelected",
                "QuoteSelected",
                "RequestPresented",
                "QuoteFilled",
                "QuoteFilled",
                "QuoteFilled",
                "RequestFilled",
            ],
        }
    };
    // Leg 2 of 3 fails: legs 1 and 3 are quoted, leg 2 is not, so at the deadline the whole request
    // expires and the requester's and both MMs' holds are all released in that one command.
    let multi_leg_leg2_fails = {
        let mut s = three_markets();
        s.push(three_leg_request());
        s.push(leg_quote(300, "q1", MM1, 0, 6_200));
        s.push(leg_quote(400, "q3", MM2, 2, 6_000));
        s.push(tick(1_600));
        Scenario {
            name: "multi_leg_leg2_fails",
            steps: s,
            expect: vec![
                "RequestOpened",
                "RequestOpened",
                "RequestOpened",
                "QuoteSubmitted",
                "QuoteSubmitted",
                "QuoteLost",
                "QuoteLost",
                "RequestExpired",
            ],
        }
    };
    vec![
        happy,
        withdraw,
        no_quotes,
        lapse,
        disputed,
        silent,
        multi_leg_happy,
        multi_leg_leg2_fails,
    ]
}

/// Runs a scenario on a fresh in-memory engine. Returns the harness and every emitted event.
pub fn run_in_memory(s: &Scenario, t0: Timestamp) -> (Harness, Vec<Emitted>) {
    let mut h = Harness::new(demo_config());
    let mut ctx = Ctx::new(t0);
    let mut events = Vec::new();
    for step in &s.steps {
        h.set_time(t0 + step.at);
        let out = match &step.kind {
            StepKind::Tick => h.submit(Command::Tick),
            StepKind::Send { label, actor, action } => {
                ctx.record_label(label, h.next_seq());
                let a = action(&ctx);
                h.user(*actor, a)
            }
        };
        ctx.observe(&out);
        events.extend(out);
    }
    (h, events)
}

/// Whether `events` contains the `expect`ed events in this order (other events may be interleaved).
/// An expectation is an event name, or `CommandRejected(<RejectReason>)` to pin a rejection's reason.
pub fn contains_in_order(events: &[Emitted], expect: &[&str]) -> Result<(), String> {
    let mut it = events.iter();
    for want in expect {
        if !it.any(|got| matches(&got.event, want)) {
            return Err(format!("expected event {want} (in order {expect:?})"));
        }
    }
    Ok(())
}

fn matches(event: &Event, want: &str) -> bool {
    match event {
        Event::CommandRejected { reason } => {
            want == event.name() || want == format!("{}({reason:?})", event.name())
        }
        other => want == other.name(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(reason: RejectReason) -> Emitted {
        Emitted::public(Event::CommandRejected { reason })
    }

    #[test]
    fn an_expectation_can_pin_the_rejection_reason() {
        let events = [rejected(RejectReason::InvalidState)];
        assert_eq!(contains_in_order(&events, &["CommandRejected"]), Ok(()));
        assert_eq!(
            contains_in_order(&events, &["CommandRejected(InvalidState)"]),
            Ok(())
        );
        assert!(
            contains_in_order(
                &[rejected(RejectReason::Unauthorized)],
                &["CommandRejected(InvalidState)"]
            )
            .is_err()
        );
    }
}
