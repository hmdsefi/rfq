//! Views: what each connection receives (docs/specs/gateway.md §4). This is where quotes stay
//! sealed (engine.md §7): an account sees only `Public` events and its own, only its own balances,
//! and the `origin` of its own commands only.
use rfq_app::EventBatch;
use rfq_domain::{AccountId, Audience, Emitted};
use serde::{Deserialize, Serialize};

use crate::protocol::{AccountBalance, Origin, OutBatch};

/// Who a connection sees the venue as. JSON: `"admin"` or `{"account": 10}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    /// The venue operator: everything (gateway.md §7).
    Admin,
    /// One participant.
    Account(AccountId),
}

/// The batch as `view` sees it, or `None` when nothing of it is for `view`.
///
/// - Events: `admin` gets all; an account gets `Public` ones and those addressed to it.
/// - `origin` names the sender, so only the sender gets it (engine.md §7); everyone else gets
///   `null`.
/// - Balances: `admin` gets all; an account gets only its own.
/// - `seq`, `time` and `kind` are public. An account gets a batch only when an event, a balance or
///   the origin is left for it, so it always gets its own command's batch, even one with no
///   events.
pub fn filter(view: View, b: &EventBatch) -> Option<OutBatch> {
    let sees = |a: &Audience| match (view, a) {
        (View::Admin, _) | (_, Audience::Public) => true,
        (View::Account(me), Audience::Account(to)) => me == *to,
    };
    let events: Vec<Emitted> = b.events.iter().filter(|e| sees(&e.audience)).cloned().collect();
    let balances: Vec<AccountBalance> = b
        .balances
        .iter()
        .filter(|(a, _)| matches!(view, View::Admin) || view == View::Account(*a))
        .map(|(a, bal)| AccountBalance {
            account: *a,
            balance: *bal,
        })
        .collect();
    let origin = b
        .origin
        .filter(|(actor, _)| matches!(view, View::Admin) || view == View::Account(*actor))
        .map(|(actor, nonce)| Origin { actor, nonce });
    let keep = matches!(view, View::Admin) || !events.is_empty() || !balances.is_empty() || origin.is_some();
    keep.then_some(OutBatch {
        seq: b.seq,
        time: b.time,
        kind: b.kind,
        origin,
        events,
        balances,
    })
}

#[cfg(test)]
mod tests {
    use rfq_app::BatchKind;
    use rfq_domain::{
        Balance, Event, MarketId, Outcome, PositionId, Price, QuoteId, RejectCause, RejectReason, RequestId,
        Side,
    };

    use super::*;

    const ALICE: AccountId = AccountId(10);
    const MM1: AccountId = AccountId(20);
    const CAROL: AccountId = AccountId(30);

    /// One event of every kind. The `match` is a tripwire: a new `Event` variant does not compile
    /// here until it is added to the list.
    fn every_event() -> Vec<Event> {
        let (a, c, r, q, p) = (ALICE, MarketId(1), RequestId(2), QuoteId(3), PositionId(4));
        let price = Price::new(5_000).unwrap();
        let events = vec![
            Event::Initialized,
            Event::CommandRejected {
                reason: RejectReason::InvalidState,
            },
            Event::Deposited {
                account: a,
                amount: 1,
            },
            Event::Withdrawn {
                account: a,
                amount: 1,
            },
            Event::MarketListed { market: c },
            Event::MarketClosed { market: c },
            Event::RequestOpened {
                request: r,
                leg: 0,
                market: c,
                side: Side::BuyYes,
                qty: 1,
                response_deadline: 1,
            },
            Event::RequestCancelled { request: r },
            Event::RequestExpired { request: r },
            Event::RequestPresented {
                request: r,
                picks: Vec::new(),
                accept_by: 1,
            },
            Event::RequestRejected {
                request: r,
                cause: RejectCause::Declined,
            },
            Event::RequestFilled {
                request: r,
                positions: vec![p],
            },
            Event::QuoteSubmitted {
                quote: q,
                request: r,
                price,
                expires_at: 1,
            },
            Event::QuoteReplaced { old: q, new: q },
            Event::QuoteCancelled { quote: q },
            Event::QuoteExpired { quote: q },
            Event::QuoteLost { quote: q },
            Event::QuoteSelected { quote: q },
            Event::QuoteReleased { quote: q },
            Event::QuoteFilled {
                quote: q,
                position: p,
            },
            Event::OutcomeProposed {
                market: c,
                outcome: Outcome::Yes,
                dispute_until: 1,
            },
            Event::Disputed {
                market: c,
                disputer: a,
            },
            Event::Ruled {
                market: c,
                outcome: Outcome::No,
                bond_returned: true,
            },
            Event::MarketFinalized {
                market: c,
                outcome: Outcome::Invalid,
            },
            Event::PositionSettled {
                position: p,
                yes_holder: a,
                yes_payout: 1,
                no_holder: MM1,
                no_payout: 0,
            },
            Event::DisputeBondReturned {
                market: c,
                disputer: a,
                amount: 1,
            },
        ];
        for e in &events {
            match e {
                Event::Initialized
                | Event::CommandRejected { .. }
                | Event::Deposited { .. }
                | Event::Withdrawn { .. }
                | Event::MarketListed { .. }
                | Event::MarketClosed { .. }
                | Event::RequestOpened { .. }
                | Event::RequestCancelled { .. }
                | Event::RequestExpired { .. }
                | Event::RequestPresented { .. }
                | Event::RequestRejected { .. }
                | Event::RequestFilled { .. }
                | Event::QuoteSubmitted { .. }
                | Event::QuoteReplaced { .. }
                | Event::QuoteCancelled { .. }
                | Event::QuoteExpired { .. }
                | Event::QuoteLost { .. }
                | Event::QuoteSelected { .. }
                | Event::QuoteReleased { .. }
                | Event::QuoteFilled { .. }
                | Event::OutcomeProposed { .. }
                | Event::Disputed { .. }
                | Event::Ruled { .. }
                | Event::MarketFinalized { .. }
                | Event::PositionSettled { .. }
                | Event::DisputeBondReturned { .. } => {}
            }
        }
        assert_eq!(events.len(), 26);
        events
    }

    fn batch(
        origin: Option<(AccountId, u64)>,
        events: Vec<Emitted>,
        balances: Vec<(AccountId, Balance)>,
    ) -> EventBatch {
        EventBatch {
            seq: 9,
            time: 1_234,
            kind: BatchKind::User,
            origin,
            events,
            balances,
        }
    }

    fn bal(free: u64) -> Balance {
        Balance {
            free,
            reserved: 0,
            escrowed: 0,
        }
    }

    #[test]
    fn every_event_kind_reaches_exactly_its_audience() {
        for event in every_event() {
            for audience in [Audience::Public, Audience::Account(ALICE), Audience::Account(MM1)] {
                let e = Emitted {
                    audience,
                    event: event.clone(),
                };
                let b = batch(None, vec![e.clone()], Vec::new());
                let admin = filter(View::Admin, &b).expect("admin gets every batch");
                assert_eq!(admin.events, vec![e.clone()], "{e:?}");
                for me in [ALICE, MM1, CAROL] {
                    let should = audience == Audience::Public || audience == Audience::Account(me);
                    let got = filter(View::Account(me), &b);
                    if should {
                        let got = got.unwrap_or_else(|| panic!("{me:?} must get {e:?}"));
                        assert_eq!(got.events, vec![e.clone()]);
                        assert_eq!((got.seq, got.time, got.kind), (9, 1_234, BatchKind::User));
                    } else {
                        assert_eq!(got, None, "{me:?} must not get {e:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn origin_goes_only_to_the_sender() {
        let b = batch(
            Some((ALICE, 7)),
            vec![Emitted::public(Event::RequestCancelled {
                request: RequestId(2),
            })],
            Vec::new(),
        );
        let origin = Some(Origin {
            actor: ALICE,
            nonce: 7,
        });
        assert_eq!(filter(View::Admin, &b).map(|o| o.origin), Some(origin));
        assert_eq!(filter(View::Account(ALICE), &b).map(|o| o.origin), Some(origin));
        assert_eq!(filter(View::Account(MM1), &b).map(|o| o.origin), Some(None));
    }

    #[test]
    fn balances_go_only_to_their_account() {
        let b = batch(None, Vec::new(), vec![(ALICE, bal(1)), (MM1, bal(2))]);
        let all = |v| filter(v, &b).map(|o| o.balances);
        assert_eq!(
            all(View::Admin),
            Some(vec![
                AccountBalance {
                    account: ALICE,
                    balance: bal(1)
                },
                AccountBalance {
                    account: MM1,
                    balance: bal(2)
                },
            ])
        );
        assert_eq!(
            all(View::Account(MM1)),
            Some(vec![AccountBalance {
                account: MM1,
                balance: bal(2)
            }])
        );
        assert_eq!(all(View::Account(CAROL)), None, "nothing for Carol");
    }

    /// Alice's command whose only event went to another account (an operator deposit, say) still
    /// reaches her, by its origin; and a batch with nothing at all still reaches the admin.
    #[test]
    fn an_account_always_gets_its_own_command_batch() {
        let b = batch(
            Some((ALICE, 3)),
            vec![Emitted::to(
                MM1,
                Event::Deposited {
                    account: MM1,
                    amount: 5,
                },
            )],
            vec![(MM1, bal(5))],
        );
        let own = filter(View::Account(ALICE), &b).expect("her own command");
        assert_eq!(
            own.origin,
            Some(Origin {
                actor: ALICE,
                nonce: 3
            })
        );
        assert!(own.events.is_empty() && own.balances.is_empty());
        assert_eq!(filter(View::Account(CAROL), &b), None);

        let resume = EventBatch {
            kind: BatchKind::Resume,
            ..batch(None, Vec::new(), Vec::new())
        };
        assert!(filter(View::Admin, &resume).is_some());
        assert_eq!(filter(View::Account(ALICE), &resume), None);
    }

    #[test]
    fn views_read_and_write_the_spec_json() {
        assert_eq!(serde_json::to_string(&View::Admin).unwrap(), r#""admin""#);
        assert_eq!(
            serde_json::to_string(&View::Account(ALICE)).unwrap(),
            r#"{"account":10}"#
        );
        assert_eq!(
            serde_json::from_str::<View>(r#"{"account":20}"#).unwrap(),
            View::Account(MM1)
        );
        assert!(serde_json::from_str::<View>(r#""Admin""#).is_err());
        assert!(serde_json::from_str::<View>(r#"{"account":-1}"#).is_err());
    }
}
