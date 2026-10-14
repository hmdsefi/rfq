use rfq_domain::*;
use rfq_testkit::{accounts::*, *};

const CLOSE: Timestamp = 100_000;
const HARD: Timestamp = 500_000;
const WINDOW: Duration = 10_000;

/// MM2's quote at 0.60 for ALICE's 1000 YES (limit 0.70) is presented; accept_by = 40_000.
fn presented(cfg: Config) -> (Harness, ContractId, RequestId, QuoteId) {
    let mut h = Harness::new(cfg);
    for a in [ALICE, MM2, CAROL] {
        h.fund(a, 1_000);
    }
    let c = ContractId(1);
    h.user(
        OPERATOR,
        Action::ListContract {
            id: c,
            description: "q".into(),
            oracle: ORACLE,
            trading_close: CLOSE,
            resolution_deadline: HARD,
            dispute_window: WINDOW,
        },
    );
    let r = RequestId(h.next_seq());
    h.user(
        ALICE,
        Action::SubmitRequest {
            legs: vec![LegSpec {
                contract: c,
                side: Side::BuyYes,
                qty: 1_000,
                limit_price: price(7_000),
            }],
            response_deadline: 10_000,
        },
    );
    let q = QuoteId(h.next_seq());
    h.user(
        MM2,
        Action::SubmitQuote {
            request: r,
            leg: 0,
            price: price(6_000),
            size: 1_000,
            expires_at: 90_000,
        },
    );
    h.tick(10_000);
    (h, c, r, q)
}

/// ALICE holds 1000 YES bought at 0.60 from MM2; contract closed.
fn filled_with(cfg: Config) -> (Harness, ContractId) {
    let (mut h, c, r, q) = presented(cfg);
    let out = h.user(
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
    assert_eq!(rejection(&out), None);
    h.tick(CLOSE);
    (h, c)
}

fn filled() -> (Harness, ContractId) {
    filled_with(Config::default())
}

fn free(h: &Harness, a: AccountId) -> Amount {
    h.engine.balance(a).free
}

/// Short grace so the uncapped extension is visible below HARD.
fn short_grace() -> Config {
    Config {
        resume_grace: 20_000,
        ..Config::default()
    }
}

#[test]
fn yes_pays_yes_holder_after_window() {
    let (mut h, c) = filled();
    assert_eq!(
        rejection(&h.user(
            ORACLE,
            Action::ProposeOutcome {
                contract: c,
                outcome: Outcome::Yes
            }
        )),
        None
    );
    let out = h.tick(CLOSE + WINDOW);
    assert_eq!(
        names(&out),
        vec!["ContractFinalized", "PositionSettled", "PositionSettled"]
    );
    assert_eq!(free(&h, ALICE), dollars(1_400));
    assert_eq!(free(&h, MM2), dollars(600));
    assert_eq!(
        h.engine.balance(ALICE).escrowed + h.engine.balance(MM2).escrowed,
        0
    );
}

#[test]
fn no_pays_no_holder() {
    let (mut h, c) = filled();
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::No,
        },
    );
    h.tick(CLOSE + WINDOW);
    assert_eq!(free(&h, ALICE), dollars(400));
    assert_eq!(free(&h, MM2), dollars(1_600));
}

#[test]
fn propose_rules() {
    let mut h = Harness::new(Config::default());
    let c = ContractId(1);
    h.user(
        OPERATOR,
        Action::ListContract {
            id: c,
            description: "q".into(),
            oracle: ORACLE,
            trading_close: CLOSE,
            resolution_deadline: HARD,
            dispute_window: WINDOW,
        },
    );
    let out = h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    assert_eq!(
        rejection(&out),
        Some(RejectReason::InvalidState),
        "trading still open"
    );
    h.tick(CLOSE);
    let out = h.user(
        ALICE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    assert_eq!(rejection(&out), Some(RejectReason::Unauthorized));
}

#[test]
fn propose_too_late_rejected() {
    // The dispute window must end strictly before the hard deadline.
    let (mut h, c) = filled();
    h.set_time(HARD - WINDOW);
    let out = h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(h.engine.contract(c).unwrap().state, ContractState::Closed);

    let (mut h, c) = filled();
    h.set_time(HARD - WINDOW - 1);
    assert_eq!(
        rejection(&h.user(
            ORACLE,
            Action::ProposeOutcome {
                contract: c,
                outcome: Outcome::Yes
            }
        )),
        None,
        "last moment that fits"
    );
    let out = h.tick(HARD - 1);
    assert_eq!(names(&out)[0], "ContractFinalized");
    assert_eq!(
        h.engine.contract(c).unwrap().state,
        ContractState::Final(Outcome::Yes)
    );
}

#[test]
fn dispute_upheld_forfeits_bond_to_treasury() {
    let (mut h, c) = filled();
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    assert_eq!(rejection(&h.user(CAROL, Action::Dispute { contract: c })), None);
    assert_eq!(h.engine.balance(CAROL).reserved, dollars(100));
    let out = h.user(
        ARBITER,
        Action::Rule {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    assert_eq!(names(&out)[0], "Ruled");
    assert_eq!(free(&h, CAROL), dollars(900));
    assert_eq!(free(&h, TREASURY), dollars(100));
    assert_eq!(free(&h, ALICE), dollars(1_400));
}

#[test]
fn dispute_overturned_returns_bond_and_invalid_refunds() {
    let (mut h, c) = filled();
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    h.user(CAROL, Action::Dispute { contract: c });
    h.user(
        ARBITER,
        Action::Rule {
            contract: c,
            outcome: Outcome::Invalid,
        },
    );
    assert_eq!(free(&h, CAROL), dollars(1_000));
    assert_eq!(free(&h, ALICE), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(1_000));
}

#[test]
fn dispute_rules() {
    let (mut h, c) = filled();
    assert_eq!(
        rejection(&h.user(CAROL, Action::Dispute { contract: c })),
        Some(RejectReason::InvalidState),
        "nothing proposed"
    );
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    h.user(CAROL, Action::Dispute { contract: c });
    assert_eq!(
        rejection(&h.user(ALICE, Action::Dispute { contract: c })),
        Some(RejectReason::InvalidState),
        "one dispute per contract"
    );
    assert_eq!(
        rejection(&h.user(
            CAROL,
            Action::Rule {
                contract: c,
                outcome: Outcome::No
            }
        )),
        Some(RejectReason::Unauthorized)
    );
}

#[test]
fn dispute_after_window_rejected() {
    let (mut h, c) = filled();
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    h.set_time(CLOSE + WINDOW);
    let out = h.user(CAROL, Action::Dispute { contract: c });
    assert_eq!(rejection(&out), Some(RejectReason::InvalidState));
    assert_eq!(free(&h, CAROL), dollars(1_000));
}

#[test]
fn oracle_silent_hard_deadline_refunds() {
    let (mut h, c) = filled();
    let out = h.tick(HARD);
    assert_eq!(names(&out)[0], "ContractFinalized");
    assert_eq!(
        h.engine.contract(c).unwrap().state,
        ContractState::Final(Outcome::Invalid)
    );
    assert_eq!(free(&h, ALICE), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(1_000));
}

#[test]
fn arbiter_silent_hard_deadline_returns_bond() {
    let (mut h, c) = filled();
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    h.user(CAROL, Action::Dispute { contract: c });
    let out = h.tick(HARD);
    assert_eq!(names(&out)[..2], ["DisputeBondReturned", "ContractFinalized"]);
    // A timeout never turns a proposal into Invalid: the proposal stands, the bond comes back.
    assert_eq!(
        h.engine.contract(c).unwrap().state,
        ContractState::Final(Outcome::Yes)
    );
    assert_eq!(free(&h, ALICE), dollars(1_400));
    assert_eq!(free(&h, MM2), dollars(600));
    assert_eq!(free(&h, CAROL), dollars(1_000));
}

/// The bond moves back to `free` with an event, so the disputer learns it from egress. It is not a
/// `Ruled`: nobody ruled, and the proposal stands.
#[test]
fn hard_deadline_announces_the_returned_bond_to_the_disputer() {
    let (mut h, c) = filled();
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    h.user(CAROL, Action::Dispute { contract: c });
    assert_eq!(free(&h, CAROL), dollars(900), "bond held");
    let out = h.tick(HARD);
    let returned: Vec<_> = out
        .iter()
        .filter(|e| e.event.name() == "DisputeBondReturned")
        .collect();
    assert_eq!(
        returned,
        [&Emitted::to(
            CAROL,
            Event::DisputeBondReturned {
                contract: c,
                disputer: CAROL,
                amount: dollars(100),
            }
        )],
        "once, to the disputer only, with the bond amount"
    );
    assert!(!names(&out).contains(&"Ruled"), "nobody ruled");
    assert_sealed(&h, &out);
    assert_eq!(free(&h, CAROL), dollars(1_000));
}

#[test]
fn resume_extends_open_dispute_window() {
    // A wrong proposal at CLOSE; the node is down from before the window ended until CLOSE + 50_000.
    let (mut h, c) = filled_with(short_grace());
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::No,
        },
    );
    h.set_time(CLOSE + 50_000);
    let out = h.submit(Command::Resume);
    let until = CLOSE + 50_000 + 20_000;
    assert_eq!(
        out,
        vec![Emitted::public(Event::OutcomeProposed {
            contract: c,
            outcome: Outcome::No,
            dispute_until: until
        })],
        "re-announced; the old timer (CLOSE + WINDOW) fired as a stale no-op",
    );
    assert_eq!(
        h.engine.contract(c).unwrap().state,
        ContractState::Proposed {
            outcome: Outcome::No,
            dispute_until: until
        }
    );
    assert_eq!(
        rejection(&h.user(ALICE, Action::Dispute { contract: c })),
        None,
        "holders still get a sequenced chance"
    );

    // Near the hard deadline the extension is capped at HARD - 1, so the window still ends first.
    let (mut h, c) = filled_with(short_grace());
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    h.set_time(HARD - 5_000);
    let out = h.submit(Command::Resume);
    assert_eq!(
        out,
        vec![Emitted::public(Event::OutcomeProposed {
            contract: c,
            outcome: Outcome::Yes,
            dispute_until: HARD - 1
        })]
    );
    let out = h.tick(HARD - 1);
    assert_eq!(names(&out)[0], "ContractFinalized");
    assert_eq!(
        free(&h, ALICE),
        dollars(1_400),
        "the proposal stands; the hard deadline never sees it"
    );
}

#[test]
fn resume_does_not_extend_accept_window() {
    // A lapse only refunds, so after downtime it fires normally, before anything else.
    let (mut h, _, r, q) = presented(short_grace());
    h.set_time(45_000); // down across accept_by = 40_000
    let out = h.submit(Command::Resume);
    assert_eq!(names(&out), vec!["QuoteReleased", "RequestRejected"]);
    assert_eq!(
        h.engine.request(r).unwrap().state,
        RequestState::Rejected(RejectCause::Lapsed)
    );
    assert_eq!(h.engine.quote(q).unwrap().state, QuoteState::Released);
    assert_eq!(free(&h, ALICE), dollars(1_000));
    assert_eq!(free(&h, MM2), dollars(1_000));
}

// Controller ruling R20: a contract whose dispute window cannot fit before the hard deadline could
// only ever finalize Invalid, so it is rejected at listing.
#[test]
fn list_contract_requires_room_to_resolve() {
    let mut h = Harness::new(Config::default());
    let mut list = |id: u64, window: Duration| {
        rejection(&h.user(
            OPERATOR,
            Action::ListContract {
                id: ContractId(id),
                description: "q".into(),
                oracle: ORACLE,
                trading_close: CLOSE,
                resolution_deadline: HARD,
                dispute_window: window,
            },
        ))
    };
    assert_eq!(
        list(1, HARD - CLOSE),
        Some(RejectReason::InvalidInput),
        "window ends exactly at the deadline"
    );
    assert_eq!(
        list(2, HARD - CLOSE + 1),
        Some(RejectReason::InvalidInput),
        "window outlives the deadline"
    );
    assert_eq!(
        list(3, Duration::MAX),
        Some(RejectReason::InvalidInput),
        "close + window overflows"
    );
    assert_eq!(list(4, HARD - CLOSE - 1), None, "last window that fits");
}

#[test]
fn resume_never_shortens_a_window() {
    // now + grace is still before the window's end, so there is nothing to extend.
    let (mut h, c) = filled_with(Config {
        resume_grace: 5_000,
        ..Config::default()
    });
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    h.set_time(CLOSE + 1);
    assert_eq!(h.submit(Command::Resume), vec![]);
    assert_eq!(
        h.engine.contract(c).unwrap().state,
        ContractState::Proposed {
            outcome: Outcome::Yes,
            dispute_until: CLOSE + WINDOW
        }
    );
    let out = h.tick(CLOSE + WINDOW);
    assert_eq!(names(&out)[0], "ContractFinalized");
}

#[test]
fn position_settled_goes_to_both_holders_with_their_payouts() {
    let (mut h, c) = filled();
    let position = h.engine.contract(c).unwrap().positions[0];
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    let settled = |yes_payout, no_payout| Event::PositionSettled {
        position,
        yes_holder: ALICE,
        yes_payout,
        no_holder: MM2,
        no_payout,
    };
    assert_eq!(
        h.tick(CLOSE + WINDOW),
        vec![
            Emitted::public(Event::ContractFinalized {
                contract: c,
                outcome: Outcome::Yes
            }),
            Emitted::to(ALICE, settled(dollars(1_000), 0)),
            Emitted::to(MM2, settled(dollars(1_000), 0)),
        ]
    );
    assert!(h.engine.position(position).unwrap().settled);

    // Invalid: each side's own lock comes back (ALICE locked 600, MM2 locked 400).
    let (mut h, c) = filled();
    assert_eq!(
        h.tick(HARD),
        vec![
            Emitted::public(Event::ContractFinalized {
                contract: c,
                outcome: Outcome::Invalid
            }),
            Emitted::to(ALICE, settled(dollars(600), dollars(400))),
            Emitted::to(MM2, settled(dollars(600), dollars(400))),
        ]
    );
}

#[test]
fn nothing_moves_after_final_or_twice() {
    let (mut h, c) = filled();
    let propose = |h: &mut Harness| {
        h.user(
            ORACLE,
            Action::ProposeOutcome {
                contract: c,
                outcome: Outcome::Yes,
            },
        )
    };
    assert_eq!(rejection(&propose(&mut h)), None);
    assert_eq!(
        rejection(&propose(&mut h)),
        Some(RejectReason::InvalidState),
        "one proposal per contract"
    );
    assert_eq!(
        rejection(&h.user(
            ARBITER,
            Action::Rule {
                contract: c,
                outcome: Outcome::No
            }
        )),
        Some(RejectReason::InvalidState),
        "nothing disputed"
    );
    h.tick(CLOSE + WINDOW);
    assert_eq!(rejection(&propose(&mut h)), Some(RejectReason::InvalidState));
    assert_eq!(
        rejection(&h.user(CAROL, Action::Dispute { contract: c })),
        Some(RejectReason::InvalidState)
    );
    assert_eq!(
        rejection(&h.user(
            ARBITER,
            Action::Rule {
                contract: c,
                outcome: Outcome::No
            }
        )),
        Some(RejectReason::InvalidState)
    );
    // The hard deadline later finds a final contract and does nothing.
    assert_eq!(h.tick(HARD), vec![]);
    assert_eq!(free(&h, ALICE), dollars(1_400));
    assert_eq!(free(&h, MM2), dollars(600));
    assert_eq!(free(&h, CAROL), dollars(1_000));
    assert_eq!(free(&h, TREASURY), 0);
}

#[test]
fn dispute_needs_the_bond_and_unknown_contracts_are_unknown() {
    let (mut h, c) = filled();
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    let pauper = AccountId(99);
    h.fund(pauper, 99); // one dollar short of the $100 bond
    let out = h.user(pauper, Action::Dispute { contract: c });
    assert_eq!(rejection(&out), Some(RejectReason::InsufficientFunds));
    let out = h.user(AccountId(98), Action::Dispute { contract: c });
    assert_eq!(
        rejection(&out),
        Some(RejectReason::InsufficientFunds),
        "no account at all"
    );
    assert_eq!(
        h.engine.contract(c).unwrap().state,
        ContractState::Proposed {
            outcome: Outcome::Yes,
            dispute_until: CLOSE + WINDOW
        },
        "a failed dispute freezes nothing"
    );
    assert_eq!(h.engine.balance(pauper).free, dollars(99));
    let out = h.user(
        CAROL,
        Action::Dispute {
            contract: ContractId(77),
        },
    );
    assert_eq!(rejection(&out), Some(RejectReason::UnknownObject));
}

#[test]
fn zero_bond_dispute_from_an_unknown_account_is_rejected_not_a_panic() {
    let (mut h, c) = filled_with(Config {
        dispute_bond: 0,
        ..Config::default()
    });
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    let out = h.user(AccountId(98), Action::Dispute { contract: c });
    assert_eq!(rejection(&out), Some(RejectReason::InsufficientFunds));
    // A known account can dispute for free, and the zero-value hold is released cleanly.
    assert_eq!(rejection(&h.user(CAROL, Action::Dispute { contract: c })), None);
    h.user(
        ARBITER,
        Action::Rule {
            contract: c,
            outcome: Outcome::No,
        },
    );
    assert_eq!(
        h.engine.contract(c).unwrap().state,
        ContractState::Final(Outcome::No)
    );
}

fn list_second_contract(h: &mut Harness, id: u64, close: Timestamp, hard: Timestamp) -> ContractId {
    let c = ContractId(id);
    let out = h.user(
        OPERATOR,
        Action::ListContract {
            id: c,
            description: "q".into(),
            oracle: ORACLE,
            trading_close: close,
            resolution_deadline: hard,
            dispute_window: WINDOW,
        },
    );
    assert_eq!(rejection(&out), None);
    c
}

// Controller ruling R22: `Resume` only touches open dispute windows. A disputed contract (its bond
// held for the arbiter) and a closed one (nothing proposed yet) are left exactly as they were.
#[test]
fn resume_leaves_disputed_and_closed_untouched() {
    let (mut h, disputed) = filled_with(short_grace());
    let closed = list_second_contract(&mut h, 2, CLOSE + 1_000, HARD);
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: disputed,
            outcome: Outcome::Yes,
        },
    );
    assert_eq!(
        rejection(&h.user(CAROL, Action::Dispute { contract: disputed })),
        None
    );
    h.tick(CLOSE + 1_000);
    assert_eq!(h.engine.contract(closed).unwrap().state, ContractState::Closed);
    let before = (
        h.engine.contract(disputed).unwrap().clone(),
        h.engine.contract(closed).unwrap().clone(),
    );

    // Down from before the old window's end (CLOSE + WINDOW) until well after it.
    h.set_time(CLOSE + 50_000);
    let out = h.submit(Command::Resume);
    assert_eq!(out, vec![], "no event for a disputed or a closed contract");

    assert_eq!(
        (
            h.engine.contract(disputed).unwrap().clone(),
            h.engine.contract(closed).unwrap().clone(),
        ),
        before,
        "neither contract changed"
    );
    assert_eq!(
        h.engine.contract(disputed).unwrap().state,
        ContractState::Disputed {
            proposed: Outcome::Yes
        }
    );
    assert!(
        h.engine
            .contract(disputed)
            .unwrap()
            .dispute
            .unwrap()
            .hold
            .is_some(),
        "the bond is still held"
    );
    assert_eq!(h.engine.balance(CAROL).reserved, dollars(100));
    assert_eq!(free(&h, CAROL), dollars(900));

    // Both still resolve normally afterwards.
    let out = h.user(
        ARBITER,
        Action::Rule {
            contract: disputed,
            outcome: Outcome::No,
        },
    );
    assert_eq!(names(&out)[0], "Ruled");
    let out = h.tick(HARD);
    assert_eq!(
        out,
        vec![Emitted::public(Event::ContractFinalized {
            contract: closed,
            outcome: Outcome::Invalid
        })],
        "an unresolved contract still ends Invalid at the hard deadline"
    );
}

// Controller ruling R22: every open window is extended, not just the first, and the events come out
// in contract-id order (the engine iterates a BTreeMap) regardless of listing or proposal order.
#[test]
fn resume_extends_every_proposed_contract() {
    let mut h = Harness::new(short_grace());
    let (late, early) = (ContractId(7), ContractId(3));
    // Listed 7 before 3 and proposed 7 before 3, so id order is not insertion order.
    for (id, hard) in [(late, HARD), (early, CLOSE + 30_000)] {
        h.user(
            OPERATOR,
            Action::ListContract {
                id,
                description: "q".into(),
                oracle: ORACLE,
                trading_close: CLOSE,
                resolution_deadline: hard,
                dispute_window: WINDOW,
            },
        );
    }
    h.tick(CLOSE);
    for (id, outcome) in [(late, Outcome::Yes), (early, Outcome::No)] {
        let out = h.user(
            ORACLE,
            Action::ProposeOutcome {
                contract: id,
                outcome,
            },
        );
        assert_eq!(rejection(&out), None);
    }

    // Both windows (ending CLOSE + WINDOW) elapsed while the node was down.
    let resume_at = CLOSE + 15_000;
    h.set_time(resume_at);
    let out = h.submit(Command::Resume);
    assert_eq!(
        out,
        vec![
            // 3's hard deadline is CLOSE + 30_000 < now + grace, so its extension is capped just below it.
            Emitted::public(Event::OutcomeProposed {
                contract: early,
                outcome: Outcome::No,
                dispute_until: CLOSE + 30_000 - 1
            }),
            Emitted::public(Event::OutcomeProposed {
                contract: late,
                outcome: Outcome::Yes,
                dispute_until: resume_at + 20_000
            }),
        ]
    );

    // Each window then closes at its own extended end, and each contract keeps its own proposal.
    assert_eq!(
        h.tick(CLOSE + 30_000 - 1),
        vec![Emitted::public(Event::ContractFinalized {
            contract: early,
            outcome: Outcome::No
        })]
    );
    assert_eq!(
        h.tick(resume_at + 20_000),
        vec![Emitted::public(Event::ContractFinalized {
            contract: late,
            outcome: Outcome::Yes
        })]
    );
}

#[test]
fn yes_proposed_no_ruled_pays_no_holder_and_returns_bond() {
    let (mut h, c) = filled();
    h.user(
        ORACLE,
        Action::ProposeOutcome {
            contract: c,
            outcome: Outcome::Yes,
        },
    );
    h.user(CAROL, Action::Dispute { contract: c });
    let out = h.user(
        ARBITER,
        Action::Rule {
            contract: c,
            outcome: Outcome::No,
        },
    );
    assert_eq!(
        out[0],
        Emitted::public(Event::Ruled {
            contract: c,
            outcome: Outcome::No,
            bond_returned: true
        })
    );
    assert_eq!(
        h.engine.contract(c).unwrap().state,
        ContractState::Final(Outcome::No)
    );
    // ALICE (YES, locked $600) loses it; MM2 (NO, locked $400) wins the whole $1000 pot.
    assert_eq!(free(&h, ALICE), dollars(400));
    assert_eq!(free(&h, MM2), dollars(1_600));
    // The disputer was right that the proposal was wrong: the bond comes back, the treasury gets nothing.
    assert_eq!(free(&h, CAROL), dollars(1_000));
    assert_eq!(h.engine.balance(CAROL).reserved, 0);
    assert_eq!(free(&h, TREASURY), 0);
    for a in [ALICE, MM2] {
        assert_eq!(h.engine.balance(a).escrowed, 0);
    }
}
