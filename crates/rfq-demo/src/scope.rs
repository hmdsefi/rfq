//! What belongs to one scenario run on a shared, long-lived node.
//!
//! The demo runs every scenario against one node, and egress carries every event the node
//! produces. While one scenario runs, the timers of earlier ones keep firing: their contracts
//! close, finalize and settle. Those events have the same names as the ones a scenario expects,
//! so checking the expected order against all of egress could pass on another scenario's events.
//!
//! So each run gets objects of its own. Its contract is `ContractId(t0)` (from
//! [`Ctx::contract`](rfq_testkit::scenarios::Ctx::contract)), and its accounts get ids derived
//! from the same `t0` ([`Scope::account`]), so their strict nonces start at 1. Its requests and
//! quotes are identified by the `seq` of the commands that created them. [`Scope::owns`] keeps
//! exactly the events about those objects, and the order is checked against those alone.
use std::collections::BTreeSet;

use rfq_app::EventBatch;
use rfq_domain::{AccountId, Action, ContractId, Emitted, Event};
use rfq_testkit::accounts::{ARBITER, OPERATOR, TREASURY};

/// Fixed by the node's genesis, so they keep their ids and are shared by every scenario.
pub const GENESIS_ROLES: [AccountId; 3] = [OPERATOR, ARBITER, TREASURY];

/// Room per run for the well-known scenario account ids (`rfq_testkit::accounts`, all below 100).
const ACCOUNTS_PER_RUN: u64 = 100;

pub struct Scope {
    /// The run's start time: its contract id, and the base of its account ids.
    key: u64,
    accounts: BTreeSet<AccountId>,
    /// `(actor, nonce)` of every command this run sent.
    origins: BTreeSet<(AccountId, u64)>,
    /// The `seq`s those commands got. Request and quote ids are `seq`s, so the run's requests and
    /// quotes are exactly the ones whose id is in here.
    seqs: BTreeSet<u64>,
}

impl Scope {
    /// The scope of the run whose `Ctx` starts at `t0`.
    pub fn new(t0: u64) -> Self {
        Scope {
            key: t0,
            accounts: BTreeSet::new(),
            origins: BTreeSet::new(),
            seqs: BTreeSet::new(),
        }
    }

    /// The run's own id for a scenario account (genesis roles keep theirs), remembered as one of
    /// the run's accounts.
    pub fn account(&mut self, a: AccountId) -> AccountId {
        if GENESIS_ROLES.contains(&a) {
            return a;
        }
        assert!(
            a.0 < ACCOUNTS_PER_RUN,
            "scenario account {a:?} has no room in a run"
        );
        let own = AccountId(
            self.key
                .checked_mul(ACCOUNTS_PER_RUN)
                .and_then(|base| base.checked_add(a.0))
                .expect("a millisecond timestamp times 100 fits in u64"),
        );
        self.accounts.insert(own);
        own
    }

    /// Records a command this run is about to send, so its batch counts as the run's.
    pub fn sent(&mut self, origin: (AccountId, u64)) {
        self.origins.insert(origin);
    }

    /// Whether event `e` of `batch` is about this run's objects. Call it for every event of every
    /// batch, in egress order: the batch of one of the run's commands adds its `seq`.
    pub fn owns(&mut self, batch: &EventBatch, e: &Emitted) -> bool {
        let own_command = batch.origin.is_some_and(|o| self.origins.contains(&o));
        if own_command {
            self.seqs.insert(batch.seq);
        }
        let contract = ContractId(self.key);
        let account = |a: &AccountId| self.accounts.contains(a);
        let object = |id: u64| self.seqs.contains(&id);
        // No wildcard: a new event must be classified here.
        match &e.event {
            Event::Initialized => false,
            // Only the command's own handling rejects; timers never do.
            Event::CommandRejected { .. } => own_command,
            Event::Deposited { account: a, .. } | Event::Withdrawn { account: a, .. } => account(a),
            Event::ContractListed { contract: c }
            | Event::ContractClosed { contract: c }
            | Event::OutcomeProposed { contract: c, .. }
            | Event::Disputed { contract: c, .. }
            | Event::Ruled { contract: c, .. }
            | Event::ContractFinalized { contract: c, .. } => *c == contract,
            Event::RequestOpened { request, .. }
            | Event::RequestCancelled { request }
            | Event::RequestExpired { request }
            | Event::RequestPresented { request, .. }
            | Event::RequestRejected { request, .. }
            | Event::RequestFilled { request, .. } => object(request.0),
            Event::QuoteSubmitted { quote, .. }
            | Event::QuoteCancelled { quote }
            | Event::QuoteExpired { quote }
            | Event::QuoteLost { quote }
            | Event::QuoteSelected { quote }
            | Event::QuoteReleased { quote }
            | Event::QuoteFilled { quote, .. } => object(quote.0),
            Event::QuoteReplaced { old, new } => object(old.0) || object(new.0),
            Event::PositionSettled {
                yes_holder,
                no_holder,
                ..
            } => account(yes_holder) || account(no_holder),
        }
    }
}

/// `action` with every account id it carries mapped through `f`. Only `Deposit` and
/// `ListContract` carry any; the match has no wildcard, so a new action must be looked at here.
pub fn remap(action: Action, mut f: impl FnMut(AccountId) -> AccountId) -> Action {
    match action {
        Action::Deposit { to, amount } => Action::Deposit { to: f(to), amount },
        Action::ListContract {
            id,
            description,
            oracle,
            trading_close,
            resolution_deadline,
            dispute_window,
        } => Action::ListContract {
            id,
            description,
            oracle: f(oracle),
            trading_close,
            resolution_deadline,
            dispute_window,
        },
        a @ (Action::Withdraw { .. }
        | Action::SubmitRequest { .. }
        | Action::CancelRequest { .. }
        | Action::SubmitQuote { .. }
        | Action::CancelQuote { .. }
        | Action::Accept { .. }
        | Action::Reject { .. }
        | Action::ProposeOutcome { .. }
        | Action::Dispute { .. }
        | Action::Rule { .. }) => a,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rfq_domain::{Command, Outcome, RequestId};
    use rfq_testkit::accounts::{ALICE, ORACLE};
    use rfq_testkit::scenarios::{Ctx, StepKind, all, contains_in_order, demo_config, run_in_memory};
    use rfq_testkit::{Harness, names};

    use super::*;

    #[test]
    fn accounts_are_per_run_and_genesis_roles_are_shared() {
        let mut a = Scope::new(1_000);
        let mut b = Scope::new(2_000);
        assert_eq!(a.account(ALICE), AccountId(100_010));
        assert_ne!(a.account(ALICE), b.account(ALICE));
        assert_ne!(a.account(ORACLE), b.account(ORACLE));
        for role in GENESIS_ROLES {
            assert_eq!(a.account(role), role);
        }
    }

    #[test]
    fn remap_maps_deposit_target_and_oracle_only() {
        let f = |a: AccountId| AccountId(a.0 + 1_000);
        assert_eq!(
            remap(Action::Deposit { to: ALICE, amount: 5 }, f),
            Action::Deposit {
                to: AccountId(1_010),
                amount: 5
            }
        );
        let listed = remap(
            Action::ListContract {
                id: ContractId(9),
                description: "d".into(),
                oracle: ORACLE,
                trading_close: 1,
                resolution_deadline: 2,
                dispute_window: 3,
            },
            f,
        );
        assert!(matches!(
            listed,
            Action::ListContract {
                id: ContractId(9),
                oracle: AccountId(1_004),
                ..
            }
        ));
        let cancel = Action::CancelRequest {
            request: RequestId(10),
        };
        assert_eq!(remap(cancel.clone(), f), cancel);
    }

    #[test]
    fn another_runs_events_are_not_owned() {
        let mut scope = Scope::new(1_000);
        let alice = scope.account(ALICE);
        scope.sent((alice, 1));
        let ours = EventBatch {
            seq: 7,
            origin: Some((alice, 1)),
            events: vec![],
        };
        let tick = EventBatch {
            seq: 8,
            origin: None,
            events: vec![],
        };
        let theirs = EventBatch {
            seq: 9,
            origin: Some((AccountId(42), 1)),
            events: vec![],
        };
        let public = |event| Emitted::public(event);
        let rejected = public(Event::CommandRejected {
            reason: rfq_domain::RejectReason::InvalidState,
        });
        assert!(scope.owns(&ours, &rejected));
        assert!(!scope.owns(&theirs, &rejected));
        // Request 7 was created by our command (seq 7); request 9 by someone else's.
        assert!(scope.owns(
            &tick,
            &public(Event::RequestExpired {
                request: RequestId(7)
            })
        ));
        assert!(!scope.owns(
            &tick,
            &public(Event::RequestExpired {
                request: RequestId(9)
            })
        ));
        let finalized = |c| {
            public(Event::ContractFinalized {
                contract: ContractId(c),
                outcome: Outcome::Yes,
            })
        };
        assert!(scope.owns(&tick, &finalized(1_000)));
        assert!(!scope.owns(&tick, &finalized(2_000)));
        assert!(!scope.owns(&tick, &public(Event::Initialized)));
    }

    /// One scenario run on the shared engine.
    struct Run {
        ctx: Ctx,
        scope: Scope,
        own: Vec<Emitted>,
    }

    /// The demo's situation, in memory: all six scenarios on one engine, started 3 s apart so
    /// that their timers interleave, with a `Tick` whenever a timer is due (as the node does).
    /// Every event must belong to exactly one run, and each run's own events must pass its check.
    #[test]
    fn scopes_partition_a_shared_engine_and_every_scenario_passes_on_its_own_events() {
        let scenarios = all();
        let mut h = Harness::new(demo_config());
        let mut nonces: BTreeMap<AccountId, u64> = BTreeMap::new();
        let mut runs: Vec<Run> = (0..scenarios.len() as u64)
            .map(|i| {
                let t0 = 1_000_000 + i * 3_000;
                Run {
                    ctx: Ctx::new(t0),
                    scope: Scope::new(t0),
                    own: Vec::new(),
                }
            })
            .collect();
        let mut plan: Vec<(u64, usize, usize)> = Vec::new();
        for (i, s) in scenarios.iter().enumerate() {
            for (j, step) in s.steps.iter().enumerate() {
                plan.push((runs[i].ctx.at(step.at), i, j));
            }
        }
        plan.sort(); // by time, then scenario, then step: each scenario keeps its step order

        let mut events = 0;
        let mut deliver = |runs: &mut [Run], batch: EventBatch| {
            for e in &batch.events {
                let owners: Vec<usize> = runs
                    .iter_mut()
                    .enumerate()
                    .filter_map(|(i, r)| r.scope.owns(&batch, e).then_some(i))
                    .collect();
                assert_eq!(
                    owners.len(),
                    1,
                    "{e:?} (seq {}) is owned by runs {owners:?}",
                    batch.seq
                );
                runs[owners[0]].own.push(e.clone());
                events += 1;
            }
            for r in runs.iter_mut() {
                r.ctx.observe(&batch.events);
            }
        };
        let tick_until = |h: &mut Harness,
                          runs: &mut [Run],
                          until: u64,
                          deliver: &mut dyn FnMut(&mut [Run], EventBatch)| {
            while let Some(due) = h.engine.next_timer()
                && due <= until
            {
                let seq = h.next_seq();
                let events = h.tick(due.max(h.now()));
                deliver(
                    runs,
                    EventBatch {
                        seq,
                        origin: None,
                        events,
                    },
                );
            }
        };
        for &(at, i, j) in &plan {
            tick_until(&mut h, &mut runs, at, &mut deliver);
            h.set_time(at);
            let StepKind::Send { label, actor, action } = &scenarios[i].steps[j].kind else {
                continue; // a Tick step only waits
            };
            let run = &mut runs[i];
            let actor = run.scope.account(*actor);
            let action = remap(action(&run.ctx), |a| run.scope.account(a));
            let nonce = {
                let n = nonces.entry(actor).or_insert(0);
                *n += 1;
                *n
            };
            run.scope.sent((actor, nonce));
            let seq = h.next_seq();
            let events = h.submit(Command::User { actor, nonce, action });
            deliver(
                &mut runs,
                EventBatch {
                    seq,
                    origin: Some((actor, nonce)),
                    events,
                },
            );
            runs[i].ctx.record_label(label, seq);
        }
        tick_until(&mut h, &mut runs, u64::MAX, &mut deliver);

        let owned: usize = runs.iter().map(|r| r.own.len()).sum();
        assert_eq!(owned, events);
        for (s, r) in scenarios.iter().zip(&runs) {
            contains_in_order(&r.own, &s.expect).unwrap_or_else(|e| panic!("{}: {e}", s.name));
            // Everything the scenario produces on an engine of its own happens here too, in the
            // same order (the shared run has more: e.g. later timers of a scenario that ended).
            let (_, alone) = run_in_memory(s, 1_000_000);
            contains_in_order(&r.own, &names(&alone)).unwrap_or_else(|e| panic!("{}: {e}", s.name));
        }
    }
}
