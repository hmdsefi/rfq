//! The API's read views over engines the testkit scenarios built: expected rows, the JSON field
//! names, and what stays hidden (read-api.md §3, §9: an account sees only its own data, the
//! public endpoint only public fields, and a request's quote set is never shown).
use std::collections::BTreeSet;

use rfq_domain::*;
use rfq_read::{
    AccountView, Holding, OwnLegView, OwnQuoteView, OwnRequestView, PositionView, PublicLegView,
    PublicRequestView, Selected, account, contract, contracts, open_requests, positions_of, quotes_of,
    requests_of,
};
use rfq_testkit::accounts::*;
use rfq_testkit::scenarios::{Scenario, Step, StepKind, all, demo_config, run_in_memory};
use rfq_testkit::{Harness, dollars, price, rejection};
use serde_json::{Value, json};

/// The scenarios' start time, in engine milliseconds.
const T0: Timestamp = 1_000;

fn scenario(name: &str) -> Scenario {
    all()
        .into_iter()
        .find(|s| s.name == name)
        .expect("scenario exists")
}

fn is_send(step: &Step, want: &str) -> bool {
    matches!(&step.kind, StepKind::Send { label, .. } if *label == want)
}

fn is_tick(step: &Step) -> bool {
    matches!(step.kind, StepKind::Tick)
}

/// The engine after `s` has run its steps up to and including the first one `stop` selects.
fn engine_through(mut s: Scenario, stop: impl Fn(&Step) -> bool) -> Engine {
    let n = s.steps.iter().position(stop).expect("a step matches") + 1;
    s.steps.truncate(n);
    run_in_memory(&s, T0).0.engine
}

/// The engine after the whole scenario.
fn engine_after(name: &str) -> Engine {
    run_in_memory(&scenario(name), T0).0.engine
}

/// Every object key in a JSON value, at any depth.
fn all_keys(v: &Value) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let mut stack = vec![v];
    while let Some(v) = stack.pop() {
        match v {
            Value::Object(m) => {
                keys.extend(m.keys().cloned());
                stack.extend(m.values());
            }
            Value::Array(a) => stack.extend(a),
            _ => {}
        }
    }
    keys
}

fn keys_of(v: &Value) -> BTreeSet<&str> {
    v.as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect()
}

fn set(keys: &[&'static str]) -> BTreeSet<&'static str> {
    keys.iter().copied().collect()
}

fn only_position(e: &Engine) -> &Position {
    let mut it = e.positions();
    let p = it.next().expect("one position");
    assert!(it.next().is_none(), "exactly one position");
    p
}

// ---- positions_of ----

#[test]
fn positions_of_after_happy_path() {
    let e = engine_after("happy_path");
    let p = only_position(&e);

    // Alice bought 1,000 YES at 0.60 (she locks $600; MM2 locks $400) and YES won: she is paid the $1,000 pot.
    let alice = positions_of(&e, ALICE);
    assert_eq!(
        alice,
        vec![PositionView {
            id: p.id,
            contract: ContractId(T0),
            request: RequestId(7),
            qty: 1_000,
            side: Holding::Yes,
            stake: 600_000_000,
            settled: true,
            payout: Some(1_000_000_000),
        }]
    );
    let mm2 = positions_of(&e, MM2);
    assert_eq!(
        mm2,
        vec![PositionView {
            id: p.id,
            contract: ContractId(T0),
            request: RequestId(7),
            qty: 1_000,
            side: Holding::No,
            stake: 400_000_000,
            settled: true,
            payout: Some(0),
        }]
    );
    assert_eq!(
        positions_of(&e, MM1),
        vec![],
        "MM1 lost the selection and holds nothing"
    );
    assert_eq!(positions_of(&e, CAROL), vec![]);

    // The JSON is the spec's, and shows no counterparty.
    let json = serde_json::to_value(&alice).unwrap();
    assert_eq!(
        json,
        json!([{
            "id": p.id.0, "contract": T0, "request": 7, "qty": 1_000, "side": "Yes",
            "stake": 600_000_000u64, "settled": true, "payout": 1_000_000_000u64
        }])
    );
    assert_eq!(
        keys_of(&json[0]),
        set(&[
            "id", "contract", "request", "qty", "side", "stake", "settled", "payout"
        ])
    );
}

#[test]
fn positions_of_after_an_invalid_resolution_refunds_each_stake() {
    let e = engine_after("disputed_ruled_invalid");
    assert_eq!(
        e.contract(ContractId(T0)).unwrap().state,
        ContractState::Final(Outcome::Invalid)
    );

    let alice = positions_of(&e, ALICE);
    assert_eq!(alice.len(), 1);
    assert_eq!(
        (alice[0].side, alice[0].stake, alice[0].settled),
        (Holding::Yes, dollars(600), true)
    );
    assert_eq!(alice[0].payout, Some(dollars(600)), "each gets its own lock back");

    let mm2 = positions_of(&e, MM2);
    assert_eq!(mm2.len(), 1);
    assert_eq!(
        (mm2[0].side, mm2[0].stake, mm2[0].settled),
        (Holding::No, dollars(400), true)
    );
    assert_eq!(mm2[0].payout, Some(dollars(400)));
}

#[test]
fn positions_of_after_a_no_resolution_pays_the_no_holder() {
    // The happy path, with the oracle proposing NO.
    let mut s = scenario("happy_path");
    let propose = s.steps.iter().position(|st| is_send(st, "propose")).unwrap();
    s.steps[propose].kind = StepKind::Send {
        label: "propose",
        actor: ORACLE,
        action: Box::new(|c| Action::ProposeOutcome {
            contract: c.contract(),
            outcome: Outcome::No,
        }),
    };
    let e = run_in_memory(&s, T0).0.engine;
    assert_eq!(
        e.contract(ContractId(T0)).unwrap().state,
        ContractState::Final(Outcome::No)
    );

    assert_eq!(
        positions_of(&e, ALICE)[0].payout,
        Some(0),
        "the YES holder is paid nothing"
    );
    assert_eq!(
        positions_of(&e, MM2)[0].payout,
        Some(dollars(1_000)),
        "the NO holder takes the pot"
    );
}

#[test]
fn positions_are_unsettled_with_no_payout_before_resolution() {
    let e = engine_through(scenario("happy_path"), |s| is_send(s, "accept"));
    let p = only_position(&e);
    assert!(!p.settled);

    let alice = positions_of(&e, ALICE);
    assert_eq!(alice.len(), 1);
    assert_eq!((alice[0].side, alice[0].stake), (Holding::Yes, dollars(600)));
    assert_eq!((alice[0].settled, alice[0].payout), (false, None));

    let mm2 = positions_of(&e, MM2);
    assert_eq!((mm2[0].side, mm2[0].stake), (Holding::No, dollars(400)));
    assert_eq!((mm2[0].settled, mm2[0].payout), (false, None));
    assert_eq!(serde_json::to_value(&mm2).unwrap()[0]["payout"], Value::Null);
}

#[test]
fn a_multi_leg_fill_gives_one_position_per_leg_in_id_order() {
    let e = engine_after("multi_leg_happy");

    // Alice buys YES on all three markets: MM1 backs legs 0 and 2, MM2 backs leg 1 (400 shares each).
    let alice = positions_of(&e, ALICE);
    assert_eq!(alice.len(), 3);
    assert!(alice.windows(2).all(|w| w[0].id < w[1].id), "id order");
    assert_eq!(
        alice.iter().map(|p| p.stake).collect::<Vec<_>>(),
        [248_000_000, 240_000_000, 256_000_000]
    );
    assert!(
        alice
            .iter()
            .all(|p| p.side == Holding::Yes && !p.settled && p.payout.is_none())
    );

    let mm1 = positions_of(&e, MM1);
    assert_eq!(mm1.len(), 2);
    assert!(mm1.windows(2).all(|w| w[0].id < w[1].id));
    assert_eq!(
        mm1.iter().map(|p| p.stake).collect::<Vec<_>>(),
        [152_000_000, 144_000_000]
    );
    assert!(mm1.iter().all(|p| p.side == Holding::No));
    assert_eq!(positions_of(&e, MM2).len(), 1);
}

// ---- requests_of ----

#[test]
fn requests_of_hides_the_quote_set_and_shows_the_selection_once_presented() {
    // Both quotes are in, the deadline has not passed.
    let before = engine_through(scenario("happy_path"), |s| is_send(s, "q2"));
    let views = requests_of(&before, ALICE);
    assert_eq!(
        views,
        vec![OwnRequestView {
            id: RequestId(7),
            state: RequestState::Open,
            response_deadline: T0 + 1_500,
            legs: vec![OwnLegView {
                contract: ContractId(T0),
                side: Side::BuyYes,
                qty: 1_000,
                limit_price: price(7_000),
                selected: None,
            }],
        }]
    );
    let json = serde_json::to_value(&views).unwrap();
    let legs = json[0]["legs"].as_array().unwrap();
    assert!(!legs.is_empty());
    for leg in legs {
        assert_eq!(
            leg.get("selected"),
            Some(&Value::Null),
            "a present key, with a null"
        );
    }
    assert!(
        !all_keys(&json).contains("quotes"),
        "the live quote set is never exposed"
    );
    assert_eq!(
        keys_of(&json[0]),
        set(&["id", "state", "response_deadline", "legs"]),
        "no requester, no hold"
    );
    assert_eq!(
        keys_of(&legs[0]),
        set(&["contract", "side", "qty", "limit_price", "selected"])
    );

    // The deadline has passed: the best quote (MM2's, 0.60) is selected and presented.
    let after = engine_through(scenario("happy_path"), is_tick);
    let q2 = after.quotes().find(|q| q.mm == MM2).expect("MM2's quote");
    let views = requests_of(&after, ALICE);
    assert_eq!(
        views[0].state,
        RequestState::Presented {
            accept_by: T0 + 1_500 + 2_000
        }
    );
    assert_eq!(
        views[0].legs[0].selected,
        Some(Selected {
            quote: q2.id,
            price: price(6_000)
        })
    );
    let json = serde_json::to_value(&views).unwrap();
    assert_eq!(
        json[0]["legs"][0]["selected"],
        json!({ "quote": q2.id.0, "price": 6_000 })
    );
    assert!(!all_keys(&json).contains("quotes"));
    // Not even the selected quote's maker is named.
    assert_eq!(keys_of(&json[0]["legs"][0]["selected"]), set(&["quote", "price"]));

    // Only the requester's own requests.
    assert_eq!(requests_of(&after, MM2), vec![]);
    assert_eq!(requests_of(&after, CAROL), vec![]);
}

#[test]
fn requests_of_shows_each_legs_selection_on_a_multi_leg_request() {
    let e = engine_through(scenario("multi_leg_happy"), is_tick);
    let views = requests_of(&e, ALICE);
    assert_eq!(views.len(), 1);
    let legs = &views[0].legs;
    assert_eq!(legs.len(), 3);
    let selected: Vec<u16> = legs
        .iter()
        .map(|l| l.selected.expect("every leg is selected").price.bps())
        .collect();
    assert_eq!(
        selected,
        [6_200, 6_000, 6_400],
        "q1, q2 and q3 are each leg's only quote"
    );
    let quote_ids: Vec<QuoteId> = legs.iter().map(|l| l.selected.unwrap().quote).collect();
    assert_eq!(
        quote_ids.iter().collect::<BTreeSet<_>>().len(),
        3,
        "three distinct quotes"
    );
}

#[test]
fn requests_of_lists_every_request_of_the_account_in_id_order() {
    let mut h = Harness::new(demo_config());
    h.fund(ALICE, 1_000);
    h.set_time(1_000);
    h.user(
        OPERATOR,
        Action::ListContract {
            id: ContractId(1),
            description: "x".into(),
            oracle: ORACLE,
            trading_close: 60_000,
            resolution_deadline: 120_000,
            dispute_window: 2_000,
        },
    );
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(RequestId(h.next_seq()));
        let out = h.user(
            ALICE,
            Action::SubmitRequest {
                legs: vec![LegSpec {
                    contract: ContractId(1),
                    side: Side::BuyNo,
                    qty: 100,
                    limit_price: price(5_000),
                }],
                response_deadline: 3_000,
            },
        );
        assert_eq!(rejection(&out), None);
    }
    let views = requests_of(&h.engine, ALICE);
    assert_eq!(views.iter().map(|v| v.id).collect::<Vec<_>>(), ids);
    assert_eq!(views[0].legs[0].side, Side::BuyNo);
}

// ---- open_requests ----

#[test]
fn open_requests_shows_only_public_fields() {
    let mut h = Harness::new(demo_config());
    h.fund(ALICE, 1_000);
    h.fund(CAROL, 1_000);
    h.set_time(1_000);
    let (c1, c2) = (ContractId(1), ContractId(2));
    for id in [c1, c2] {
        let out = h.user(
            OPERATOR,
            Action::ListContract {
                id,
                description: "x".into(),
                oracle: ORACLE,
                trading_close: 60_000,
                resolution_deadline: 120_000,
                dispute_window: 2_000,
            },
        );
        assert_eq!(rejection(&out), None);
    }
    let leg = |contract| LegSpec {
        contract,
        side: Side::BuyYes,
        qty: 100,
        limit_price: price(7_000),
    };
    let submit = |h: &mut Harness, who: AccountId, legs: Vec<LegSpec>, response_deadline: Timestamp| {
        let id = RequestId(h.next_seq());
        let out = h.user(
            who,
            Action::SubmitRequest {
                legs,
                response_deadline,
            },
        );
        assert_eq!(rejection(&out), None);
        id
    };
    let a = submit(&mut h, ALICE, vec![leg(c1)], 3_000);
    let b = submit(&mut h, ALICE, vec![leg(c2)], 3_000);
    let c = submit(&mut h, CAROL, vec![leg(c1), leg(c2)], 3_000);
    let d = submit(&mut h, ALICE, vec![leg(c1)], 1_500);
    h.user(ALICE, Action::CancelRequest { request: b });
    h.tick(2_000); // `d`'s deadline passes with no quotes: it expires.
    assert_eq!(h.engine.request(b).unwrap().state, RequestState::Cancelled);
    assert_eq!(h.engine.request(d).unwrap().state, RequestState::Expired);

    let ids = |views: &[PublicRequestView]| views.iter().map(|v| v.id).collect::<Vec<_>>();

    // Only the open ones, in id order, with their public legs.
    let everything = open_requests(&h.engine, None);
    assert_eq!(ids(&everything), vec![a, c]);
    assert_eq!(
        everything[0],
        PublicRequestView {
            id: a,
            response_deadline: 3_000,
            legs: vec![PublicLegView {
                contract: c1,
                side: Side::BuyYes,
                qty: 100
            }],
        }
    );
    assert_eq!(everything[1].legs.len(), 2);

    // A request matches a contract if any of its legs is on it.
    assert_eq!(ids(&open_requests(&h.engine, Some(c1))), vec![a, c]);
    assert_eq!(ids(&open_requests(&h.engine, Some(c2))), vec![c]);
    assert_eq!(open_requests(&h.engine, Some(ContractId(99))), vec![]);

    // The serialized keys are exactly the public ones.
    let json = serde_json::to_value(&everything).unwrap();
    for request in json.as_array().unwrap() {
        assert_eq!(keys_of(request), set(&["id", "response_deadline", "legs"]));
        for leg in request["legs"].as_array().unwrap() {
            assert_eq!(keys_of(leg), set(&["contract", "side", "qty"]));
        }
    }
    let keys = all_keys(&json);
    for hidden in ["requester", "limit_price", "quotes", "selected", "state", "hold"] {
        assert!(!keys.contains(hidden), "{hidden} must not be public");
    }
}

#[test]
fn a_presented_request_is_no_longer_open() {
    let quoted = engine_through(scenario("happy_path"), |s| is_send(s, "q2"));
    assert_eq!(open_requests(&quoted, None).len(), 1);
    assert_eq!(open_requests(&quoted, Some(ContractId(T0))).len(), 1);

    let presented = engine_through(scenario("happy_path"), is_tick);
    assert_eq!(open_requests(&presented, None), vec![]);
}

// ---- quotes_of ----

#[test]
fn quotes_of_lists_the_mm_own_quotes_with_their_states() {
    let before = engine_through(scenario("happy_path"), |s| is_send(s, "q2"));
    let mm1 = quotes_of(&before, MM1);
    let q1 = before.quotes().find(|q| q.mm == MM1).unwrap().id;
    assert_eq!(
        mm1,
        vec![OwnQuoteView {
            id: q1,
            request: RequestId(7),
            leg: 0,
            price: price(6_200),
            size: 1_000,
            // MM1 asked for T0 + 10 s; the engine clamps it to the end of the accept window.
            expires_at: T0 + 1_500 + 2_000,
            state: QuoteState::Active,
        }]
    );
    assert_eq!(quotes_of(&before, MM2)[0].state, QuoteState::Active);

    // After the deadline MM2's 0.60 beats MM1's 0.62: MM1's quote is Lost, MM2's is Selected.
    let after = engine_through(scenario("happy_path"), is_tick);
    assert_eq!(quotes_of(&after, MM1)[0].state, QuoteState::Lost);
    assert_eq!(quotes_of(&after, MM2)[0].state, QuoteState::Selected);
    assert_eq!(quotes_of(&after, MM2)[0].price, price(6_000));

    // After the accept, MM2's is Filled.
    let filled = engine_through(scenario("happy_path"), |s| is_send(s, "accept"));
    assert_eq!(quotes_of(&filled, MM2)[0].state, QuoteState::Filled);

    // Only the maker's own: the requester and a bystander see none.
    assert_eq!(quotes_of(&after, ALICE), vec![]);
    assert_eq!(quotes_of(&after, CAROL), vec![]);

    let json = serde_json::to_value(quotes_of(&after, MM1)).unwrap();
    assert_eq!(
        keys_of(&json[0]),
        set(&["id", "request", "leg", "price", "size", "expires_at", "state"])
    );
    assert_eq!(json[0]["state"], "Lost");
    assert_eq!(json[0]["price"], 6_200);
}

#[test]
fn quotes_of_lists_an_mms_quotes_across_legs_in_id_order() {
    let e = engine_through(scenario("multi_leg_happy"), |s| is_send(s, "q3"));
    let mm1 = quotes_of(&e, MM1);
    assert_eq!(mm1.len(), 2, "MM1 quoted legs 0 and 2");
    assert!(mm1[0].id < mm1[1].id);
    assert_eq!(mm1.iter().map(|q| q.leg).collect::<Vec<_>>(), [0, 2]);
    assert_eq!(quotes_of(&e, MM2).len(), 1);
}

// ---- account ----

#[test]
fn account_is_zero_for_an_unknown_account() {
    let unknown = AccountId(999);
    let expected = AccountView {
        account: unknown,
        balance: Balance::default(),
        last_nonce: 0,
    };
    assert_eq!(account(&Engine::new(), unknown), expected);
    assert_eq!(account(&engine_after("happy_path"), unknown), expected);
    assert_eq!(
        serde_json::to_value(account(&Engine::new(), unknown)).unwrap(),
        json!({ "account": 999, "balance": { "free": 0, "reserved": 0, "escrowed": 0 }, "last_nonce": 0 })
    );
}

#[test]
fn account_shows_the_ledger_balance_and_the_last_nonce() {
    let e = engine_after("happy_path");
    // Alice: $1,000 in, $600 locked, then $1,000 paid out. Her two commands were nonces 1 and 2.
    assert_eq!(
        account(&e, ALICE),
        AccountView {
            account: ALICE,
            balance: Balance {
                free: dollars(1_400),
                reserved: 0,
                escrowed: 0
            },
            last_nonce: 2,
        }
    );
    // The operator sent four deposits and the listing.
    assert_eq!(account(&e, OPERATOR).last_nonce, 5);
    assert_eq!(account(&e, MM2).balance.free, dollars(600));

    // Mid-flight, money shows as reserved and escrowed.
    let held = engine_through(scenario("happy_path"), |s| is_send(s, "q2"));
    assert_eq!(
        account(&held, ALICE).balance.reserved,
        dollars(700),
        "the limit price's lock"
    );
    assert_eq!(account(&held, MM2).balance.reserved, dollars(400));
    let filled = engine_through(scenario("happy_path"), |s| is_send(s, "accept"));
    assert_eq!(account(&filled, ALICE).balance.escrowed, dollars(600));
}

// ---- contracts ----

#[test]
fn contracts_and_contract_show_terms_and_state() {
    let e = engine_after("multi_leg_happy");
    let all = contracts(&e);
    let ids: Vec<ContractId> = all.iter().map(|c| c.id).collect();
    assert_eq!(
        ids,
        [
            ContractId(T0 * 10 + 1),
            ContractId(T0 * 10 + 2),
            ContractId(T0 * 10 + 3)
        ]
    );

    let paris = &all[0];
    assert_eq!(paris.description, "Will it rain in Paris on 2026-11-01?");
    assert_eq!(paris.oracle, ORACLE);
    assert_eq!(paris.trading_close, T0 + 5_000);
    assert_eq!(paris.resolution_deadline, T0 + 15_000);
    assert_eq!(paris.dispute_window, 2_000);
    assert_eq!(paris.state, ContractState::Trading);

    let rome = contract(&e, ContractId(T0 * 10 + 3)).expect("Rome is listed");
    assert_eq!(rome.description, "Will it rain in Rome on 2026-11-01?");
    assert_eq!(&rome, &all[2]);
    assert_eq!(contract(&e, ContractId(7)), None);

    let json = serde_json::to_value(paris).unwrap();
    assert_eq!(
        json,
        json!({
            "id": T0 * 10 + 1,
            "description": "Will it rain in Paris on 2026-11-01?",
            "oracle": 4,
            "trading_close": T0 + 5_000,
            "resolution_deadline": T0 + 15_000,
            "dispute_window": 2_000,
            "state": "Trading",
        })
    );

    // The state follows the contract through resolution.
    let proposed = engine_through(scenario("happy_path"), |s| is_send(s, "propose"));
    assert!(matches!(
        contract(&proposed, ContractId(T0)).unwrap().state,
        ContractState::Proposed {
            outcome: Outcome::Yes,
            ..
        }
    ));
    let finalized = engine_after("happy_path");
    assert_eq!(
        contract(&finalized, ContractId(T0)).unwrap().state,
        ContractState::Final(Outcome::Yes)
    );
    assert_eq!(contracts(&Engine::new()), vec![]);
}
