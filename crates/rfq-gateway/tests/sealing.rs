//! Sealing through the gateway (docs/specs/gateway.md §4; engine.md §7): what each view receives
//! of the real egress batches of every scripted scenario.
//!
//! The batches are built the way `rfq_app::Node::step` builds them: each command applied to an
//! engine (through the testkit's `Harness`), its events, its touched balances, its engine time, its
//! kind and origin, and published when it has an origin or events, or is a `Resume`. The scripts
//! are the testkit's scenarios, the dashboard's six attacks, and one script for the event kinds
//! those don't reach.
use std::collections::{BTreeMap, BTreeSet};

use rfq_app::{BatchKind, EventBatch};
use rfq_domain::*;
use rfq_gateway::protocol::{AccountBalance, Origin, OutBatch};
use rfq_gateway::view::{View, filter};
use rfq_testkit::accounts::*;
use rfq_testkit::scenarios::{self, ActionFn, Ctx, StepKind, demo_config};
use rfq_testkit::{Harness, dollars, price, rejection};

const T0: Timestamp = 1_000_000;

/// Every account view checked: the brief's four participants, and the genesis roles and oracle,
/// whose own commands' batches carry nothing but their origin.
const ACCOUNTS: [AccountId; 8] = [ALICE, CAROL, MM1, MM2, OPERATOR, ARBITER, TREASURY, ORACLE];

enum Nonce {
    Next,
    /// The actor's last used nonce again (a replay: `Duplicate`).
    Reuse,
    /// One past the next (`NonceGap`).
    Skip,
}

enum Move {
    Send {
        label: &'static str,
        actor: AccountId,
        nonce: Nonce,
        action: ActionFn,
    },
    Tick,
    Resume,
}

struct Script {
    name: &'static str,
    steps: Vec<(Duration, Move)>,
}

fn send(
    at: Duration,
    label: &'static str,
    actor: AccountId,
    f: impl Fn(&Ctx) -> Action + Send + Sync + 'static,
) -> (Duration, Move) {
    send_with(at, label, actor, Nonce::Next, f)
}

fn send_with(
    at: Duration,
    label: &'static str,
    actor: AccountId,
    nonce: Nonce,
    f: impl Fn(&Ctx) -> Action + Send + Sync + 'static,
) -> (Duration, Move) {
    (
        at,
        Move::Send {
            label,
            actor,
            nonce,
            action: Box::new(f),
        },
    )
}

/// Everyone funded with $1,000; one contract closing at +5 s, hard deadline at +15 s, 2 s window.
fn setup() -> Vec<(Duration, Move)> {
    let mut steps: Vec<(Duration, Move)> = [ALICE, MM1, MM2, CAROL]
        .into_iter()
        .map(|to| {
            send(0, "fund", OPERATOR, move |_| Action::Deposit {
                to,
                amount: dollars(1_000),
            })
        })
        .collect();
    steps.push(send(0, "list", OPERATOR, |c| Action::ListContract {
        id: c.contract(),
        description: "Will it rain in Paris on 2026-11-01?".into(),
        oracle: ORACLE,
        trading_close: c.at(5_000),
        resolution_deadline: c.at(15_000),
        dispute_window: 2_000,
    }));
    steps
}

/// Alice asks for 1000 YES with a sealed 0.70 limit; quotes due at +1.5 s.
fn request(at: Duration) -> (Duration, Move) {
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

fn quote(at: Duration, label: &'static str, mm: AccountId, bps: u16, expires: Duration) -> (Duration, Move) {
    send(at, label, mm, move |c| Action::SubmitQuote {
        request: c.request("req"),
        leg: 0,
        price: price(bps),
        size: 1_000,
        expires_at: c.at(expires),
    })
}

/// Setup, the request, MM1 at 0.62 and MM2 at 0.60, presented at +1.6 s.
fn quoted() -> Vec<(Duration, Move)> {
    let mut s = setup();
    s.push(request(100));
    s.push(quote(300, "q1", MM1, 6_200, 10_000));
    s.push(quote(400, "q2", MM2, 6_000, 10_000));
    s.push((1_600, Move::Tick));
    s
}

fn accept(at: Duration, label: &'static str) -> (Duration, Move) {
    send(at, label, ALICE, |c| Action::Accept {
        request: c.request("req"),
        picks: c.picks(c.request("req")),
    })
}

/// The dashboard's six attack presets (tools/dashboard/src/presets.rs), step for step.
fn attacks() -> Vec<Script> {
    let wrong_price = {
        let mut s = quoted();
        s.push(send(1_800, "bad_accept", ALICE, |c| {
            let request = c.request("req");
            let picks = c
                .picks(request)
                .into_iter()
                .map(|p| Pick {
                    price: price(6_100),
                    ..p
                })
                .collect();
            Action::Accept { request, picks }
        }));
        s.push(accept(1_900, "accept"));
        s
    };
    let double_accept = {
        let mut s = quoted();
        s.push(accept(1_800, "accept"));
        s.push(accept(1_900, "accept_again"));
        s
    };
    let replayed_nonce = {
        let mut s = setup();
        s.push(request(100));
        s.push(quote(300, "q1", MM1, 6_200, 10_000));
        s.push(send_with(400, "q1_replayed", MM1, Nonce::Reuse, |c| {
            Action::SubmitQuote {
                request: c.request("req"),
                leg: 0,
                price: price(5_000),
                size: 1_000,
                expires_at: c.at(10_000),
            }
        }));
        s.push(send_with(500, "q1_gap", MM1, Nonce::Skip, |c| {
            Action::CancelQuote { quote: c.quote("q1") }
        }));
        s.push(send(600, "q1_cancel", MM1, |c| Action::CancelQuote {
            quote: c.quote("q1"),
        }));
        s
    };
    let over_limit = {
        let mut s = setup();
        s.push(request(100));
        s.push(quote(300, "q_over", MM2, 7_500, 10_000));
        s.push((1_600, Move::Tick));
        s
    };
    let self_trade = {
        let mut s = setup();
        s.push(request(100));
        s.push(quote(300, "self_quote", ALICE, 6_000, 10_000));
        s
    };
    let cancel_others = {
        let mut s = setup();
        s.push(request(100));
        s.push(quote(300, "q1", MM1, 6_200, 10_000));
        s.push(send(400, "carol_cancel", CAROL, |c| Action::CancelQuote {
            quote: c.quote("q1"),
        }));
        s
    };
    vec![
        Script {
            name: "attack_accept_wrong_price",
            steps: wrong_price,
        },
        Script {
            name: "attack_double_accept",
            steps: double_accept,
        },
        Script {
            name: "attack_replayed_nonce",
            steps: replayed_nonce,
        },
        Script {
            name: "attack_over_limit_quote",
            steps: over_limit,
        },
        Script {
            name: "attack_self_trade",
            steps: self_trade,
        },
        Script {
            name: "attack_cancel_others_quote",
            steps: cancel_others,
        },
    ]
}

/// The event kinds the scenarios and attacks don't reach (`Withdrawn`, `RequestCancelled`,
/// `QuoteReplaced`, `QuoteExpired`, `DisputeBondReturned`), a nonce probe, `Resume` batches, and
/// timers that fire inside another account's command, so that one user batch carries events for
/// several accounts.
fn the_rest() -> Script {
    let mut s = setup();
    s.push(send(50, "withdraw", ALICE, |_| Action::Withdraw {
        amount: dollars(1),
    }));
    s.push(send(60, "probe", OPERATOR, |_| Action::Withdraw { amount: 0 }));
    s.push(request(100));
    s.push(send(200, "req2", CAROL, |c| Action::SubmitRequest {
        legs: vec![LegSpec {
            contract: c.contract(),
            side: Side::BuyNo,
            qty: 1_000,
            limit_price: price(5_000),
        }],
        response_deadline: c.at(1_500),
    }));
    s.push(send(250, "cancel_req2", CAROL, |c| Action::CancelRequest {
        request: c.request("req2"),
    }));
    s.push(quote(300, "q1", MM1, 6_200, 10_000));
    s.push(quote(350, "q1b", MM1, 6_100, 10_000));
    s.push(quote(400, "q2", MM2, 6_000, 1_000));
    // MM2's quote expires at +1 s; the timer fires inside Carol's command, not in a Tick.
    s.push(send(1_150, "carol_withdraw", CAROL, |_| Action::Withdraw {
        amount: dollars(1),
    }));
    // The deadline (+1.5 s) fires inside Alice's own command: her request is presented in it.
    s.push(send(1_600, "alice_withdraw", ALICE, |_| Action::Withdraw {
        amount: dollars(1),
    }));
    s.push(accept(1_800, "accept"));
    s.push((2_000, Move::Resume));
    s.push((5_100, Move::Tick));
    s.push(send(5_200, "propose", ORACLE, |c| Action::ProposeOutcome {
        contract: c.contract(),
        outcome: Outcome::Yes,
    }));
    s.push((5_300, Move::Resume));
    s.push(send(5_500, "dispute", CAROL, |c| Action::Dispute {
        contract: c.contract(),
    }));
    // The arbiter never rules: the hard deadline returns the bond, inside MM1's command.
    s.push(send(15_100, "mm1_withdraw", MM1, |_| Action::Withdraw {
        amount: dollars(1),
    }));
    Script {
        name: "every_other_event_kind",
        steps: s,
    }
}

fn scripts() -> Vec<Script> {
    let mut all: Vec<Script> = scenarios::all()
        .into_iter()
        .map(|s| Script {
            name: s.name,
            steps: s
                .steps
                .into_iter()
                .map(|step| match step.kind {
                    StepKind::Send { label, actor, action } => (
                        step.at,
                        Move::Send {
                            label,
                            actor,
                            nonce: Nonce::Next,
                            action,
                        },
                    ),
                    StepKind::Tick => (step.at, Move::Tick),
                })
                .collect(),
        })
        .collect();
    all.extend(attacks());
    all.push(the_rest());
    all
}

/// Runs `script` and returns the engine after it, with every batch the node would publish.
fn run(script: &Script) -> (Harness, Vec<EventBatch>) {
    let mut h = Harness::new(demo_config());
    let mut ctx = Ctx::new(T0);
    let mut last: BTreeMap<AccountId, u64> = BTreeMap::new();
    let mut batches = Vec::new();
    for (at, mv) in &script.steps {
        let time = T0 + at;
        h.set_time(time);
        let (cmd, kind, origin) = match mv {
            Move::Tick => (Command::Tick, BatchKind::Tick, None),
            Move::Resume => (Command::Resume, BatchKind::Resume, None),
            Move::Send {
                label,
                actor,
                nonce,
                action,
            } => {
                ctx.record_label(label, h.next_seq());
                let used = last.get(actor).copied().unwrap_or(0);
                let nonce = match nonce {
                    Nonce::Next => used + 1,
                    Nonce::Reuse => used,
                    Nonce::Skip => used + 2,
                };
                let cmd = Command::User {
                    actor: *actor,
                    nonce,
                    action: action(&ctx),
                };
                (cmd, BatchKind::User, Some((*actor, nonce)))
            }
        };
        let seq = h.next_seq();
        let events = h.submit(cmd);
        let balances = h.engine.take_touched_balances();
        if let Some((actor, nonce)) = origin
            && !matches!(
                rejection(&events),
                Some(RejectReason::Duplicate | RejectReason::NonceGap)
            )
        {
            last.insert(actor, nonce);
        }
        ctx.observe(&events);
        if origin.is_some() || !events.is_empty() || kind == BatchKind::Resume {
            batches.push(EventBatch {
                seq,
                time,
                kind,
                origin,
                events,
                balances,
            });
        }
    }
    (h, batches)
}

/// The accounts an event is about, read from its content and the engine's state, independently of
/// its audience tag; `None` for an event whose content is public.
fn about(h: &Harness, b: &EventBatch, e: &Event) -> Option<Vec<AccountId>> {
    let mm = |q: &QuoteId| h.engine.quote(*q).expect("quotes are never removed").mm;
    let requester = |r: &RequestId| {
        h.engine
            .request(*r)
            .expect("requests are never removed")
            .requester
    };
    match e {
        Event::QuoteSubmitted { quote, .. }
        | Event::QuoteCancelled { quote }
        | Event::QuoteExpired { quote }
        | Event::QuoteLost { quote }
        | Event::QuoteSelected { quote }
        | Event::QuoteReleased { quote }
        | Event::QuoteFilled { quote, .. } => Some(vec![mm(quote)]),
        Event::QuoteReplaced { old, new } => Some(vec![mm(old), mm(new)]),
        Event::RequestPresented { request, .. } | Event::RequestFilled { request, .. } => {
            Some(vec![requester(request)])
        }
        Event::DisputeBondReturned { disputer, .. } => Some(vec![*disputer]),
        Event::PositionSettled {
            yes_holder,
            no_holder,
            ..
        } => Some(vec![*yes_holder, *no_holder]),
        Event::Deposited { account, .. } | Event::Withdrawn { account, .. } => Some(vec![*account]),
        // A rejection goes to the command's sender.
        Event::CommandRejected { .. } => Some(b.origin.map(|(actor, _)| actor).into_iter().collect()),
        Event::Initialized
        | Event::ContractListed { .. }
        | Event::ContractClosed { .. }
        | Event::RequestOpened { .. }
        | Event::RequestCancelled { .. }
        | Event::RequestExpired { .. }
        | Event::RequestRejected { .. }
        | Event::OutcomeProposed { .. }
        | Event::Disputed { .. }
        | Event::Ruled { .. }
        | Event::ContractFinalized { .. } => None,
    }
}

fn unfiltered(b: &EventBatch) -> OutBatch {
    OutBatch {
        seq: b.seq,
        time: b.time,
        kind: b.kind,
        origin: b.origin.map(|(actor, nonce)| Origin { actor, nonce }),
        events: b.events.clone(),
        balances: b
            .balances
            .iter()
            .map(|(account, balance)| AccountBalance {
                account: *account,
                balance: *balance,
            })
            .collect(),
    }
}

/// The audiences an account may see.
fn may_see(me: AccountId, audience: Audience) -> bool {
    audience == Audience::Public || audience == Audience::Account(me)
}

#[test]
fn view_hides_what_engine_md_section_7_hides() {
    for script in scripts() {
        let (h, batches) = run(&script);
        assert!(!batches.is_empty(), "{}", script.name);
        for b in &batches {
            let at = format!("{} seq {}", script.name, b.seq);
            assert_eq!(
                filter(View::Admin, b),
                Some(unfiltered(b)),
                "{at}: admin gets everything, unchanged"
            );
            for me in ACCOUNTS {
                let at = format!("{at} as {me:?}");
                let Some(out) = filter(View::Account(me), b) else {
                    assert!(
                        b.events.iter().all(|e| !may_see(me, e.audience)),
                        "{at}: an event it may see was dropped"
                    );
                    assert!(
                        b.balances.iter().all(|(a, _)| *a != me),
                        "{at}: its balance was dropped"
                    );
                    assert!(
                        b.origin.is_none_or(|(actor, _)| actor != me),
                        "{at}: its own command's batch was dropped"
                    );
                    continue;
                };
                assert_eq!(
                    (out.seq, out.time, out.kind),
                    (b.seq, b.time, b.kind),
                    "{at}: seq, time and kind are public"
                );
                for e in &out.events {
                    assert!(
                        may_see(me, e.audience),
                        "{at}: got {e:?}, addressed to another account"
                    );
                    if let Some(owners) = about(&h, b, &e.event) {
                        assert!(owners.contains(&me), "{at}: got {e:?}, which is about {owners:?}");
                    }
                }
                let visible: Vec<Emitted> = b
                    .events
                    .iter()
                    .filter(|e| may_see(me, e.audience))
                    .cloned()
                    .collect();
                assert_eq!(out.events, visible, "{at}: everything it may see, in order");
                assert!(
                    out.balances.iter().all(|ab| ab.account == me),
                    "{at}: got another account's balance: {:?}",
                    out.balances
                );
                let own: Vec<AccountBalance> = b
                    .balances
                    .iter()
                    .filter(|(a, _)| *a == me)
                    .map(|(account, balance)| AccountBalance {
                        account: *account,
                        balance: *balance,
                    })
                    .collect();
                assert_eq!(out.balances, own, "{at}: its own balance");
                match b.origin {
                    Some((actor, nonce)) if actor == me => {
                        assert_eq!(out.origin, Some(Origin { actor, nonce }), "{at}: its own origin")
                    }
                    _ => assert_eq!(out.origin, None, "{at}: got another account's origin"),
                }
                assert!(
                    !out.events.is_empty() || !out.balances.is_empty() || out.origin.is_some(),
                    "{at}: a batch with nothing for it must be dropped, not sent empty"
                );
            }
        }
    }
}

/// The scripts reach every event kind the engine publishes, and the cases where sealing is
/// easiest to get wrong; otherwise the test above would prove less than it says.
#[test]
fn the_sealing_scripts_cover_every_published_event_kind() {
    let mut names = BTreeSet::new();
    let mut kinds = BTreeSet::new();
    let mut mixed_user_batch = false;
    let mut origin_only = false;
    let mut nonce_rejections = BTreeSet::new();
    for script in scripts() {
        let (_, batches) = run(&script);
        for b in &batches {
            names.extend(b.events.iter().map(|e| e.event.name()));
            kinds.insert(format!("{:?}", b.kind));
            if let Some((actor, _)) = b.origin {
                mixed_user_batch |= b
                    .events
                    .iter()
                    .any(|e| matches!(e.audience, Audience::Account(to) if to != actor));
                origin_only |= b.events.iter().all(|e| !may_see(actor, e.audience))
                    && b.balances.iter().all(|(a, _)| *a != actor);
            }
            if let Some(reason) = rejection(&b.events) {
                nonce_rejections.insert(format!("{reason:?}"));
            }
        }
    }
    let every_published_kind = [
        "CommandRejected",
        "Deposited",
        "Withdrawn",
        "ContractListed",
        "ContractClosed",
        "RequestOpened",
        "RequestCancelled",
        "RequestExpired",
        "RequestPresented",
        "RequestRejected",
        "RequestFilled",
        "QuoteSubmitted",
        "QuoteReplaced",
        "QuoteCancelled",
        "QuoteExpired",
        "QuoteLost",
        "QuoteSelected",
        "QuoteReleased",
        "QuoteFilled",
        "OutcomeProposed",
        "Disputed",
        "Ruled",
        "ContractFinalized",
        "PositionSettled",
        "DisputeBondReturned",
    ];
    // `Initialized` is genesis output, which the node never publishes.
    for kind in every_published_kind {
        assert!(names.contains(kind), "no script emits {kind}");
    }
    assert_eq!(names.len(), every_published_kind.len(), "{names:?}");
    for kind in ["User", "Tick", "Resume"] {
        assert!(kinds.contains(kind), "no {kind} batch");
    }
    assert!(
        mixed_user_batch,
        "no user batch carries an event for another account"
    );
    assert!(origin_only, "no batch reaches its sender by its origin alone");
    for reason in ["Duplicate", "NonceGap", "InvalidInput", "Unauthorized"] {
        assert!(nonce_rejections.contains(reason), "no {reason} rejection");
    }
}
