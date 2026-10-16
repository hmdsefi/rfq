//! The egress rule: which applied command yields an [`EventBatch`], and what it holds. Shared by
//! the node, which publishes the batch, and by readers that regenerate it from the journal.
use rfq_domain::{AccountId, Balance, Command, Emitted, SequencedCommand};

use crate::protocol::{BatchKind, EventBatch};

/// The batch the node publishes for an applied command, or `None` when it publishes nothing:
/// a batch for every user command (it has an origin), for every `Resume`, and for any other
/// command that emitted events (gateway.md §2.3).
///
/// `events` and `balances` are what the engine returned for `sc` (`Engine::apply` and
/// `Engine::take_touched_balances`); they are carried over unchanged.
pub fn batch_of(
    sc: &SequencedCommand,
    events: Vec<Emitted>,
    balances: Vec<(AccountId, Balance)>,
) -> Option<EventBatch> {
    let (kind, origin) = match &sc.cmd {
        Command::User { actor, nonce, .. } => (BatchKind::User, Some((*actor, *nonce))),
        Command::Tick => (BatchKind::Tick, None),
        Command::Resume => (BatchKind::Resume, None),
        Command::Genesis(_) => (BatchKind::Genesis, None),
    };
    // Every Resume is published, so clients see each restart and stall (gateway.md §2.3).
    (origin.is_some() || !events.is_empty() || kind == BatchKind::Resume).then_some(EventBatch {
        seq: sc.seq,
        time: sc.time,
        kind,
        origin,
        events,
        balances,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_domain::{Action, Event, Genesis, Timestamp};
    use rfq_testkit::{accounts::*, dollars, genesis, scenarios::demo_config};

    fn sc(seq: u64, time: Timestamp, cmd: Command) -> SequencedCommand {
        SequencedCommand { seq, time, cmd }
    }

    fn user(seq: u64, actor: AccountId, nonce: u64) -> SequencedCommand {
        sc(
            seq,
            1_000,
            Command::User {
                actor,
                nonce,
                action: Action::Withdraw { amount: dollars(1) },
            },
        )
    }

    fn some_event() -> Emitted {
        Emitted::public(Event::Initialized)
    }

    fn some_balances() -> Vec<(AccountId, Balance)> {
        vec![
            (
                ALICE,
                Balance {
                    free: dollars(9),
                    reserved: 1,
                    escrowed: 2,
                },
            ),
            (MM1, Balance::default()),
        ]
    }

    #[test]
    fn a_user_command_always_has_a_batch_with_its_origin() {
        let b = batch_of(&user(7, ALICE, 3), Vec::new(), Vec::new()).expect("a user command publishes");
        assert_eq!(b.kind, BatchKind::User);
        assert_eq!(b.origin, Some((ALICE, 3)));
        assert!(b.events.is_empty());
    }

    #[test]
    fn a_tick_with_no_events_has_no_batch() {
        assert_eq!(
            batch_of(&sc(8, 2_000, Command::Tick), Vec::new(), Vec::new()),
            None
        );
    }

    #[test]
    fn a_tick_with_events_has_one() {
        let b = batch_of(&sc(8, 2_000, Command::Tick), vec![some_event()], Vec::new())
            .expect("a tick that emitted publishes");
        assert_eq!(b.kind, BatchKind::Tick);
        assert_eq!(b.origin, None);
        assert_eq!(b.events, vec![some_event()]);
    }

    #[test]
    fn a_resume_always_has_a_batch() {
        let b =
            batch_of(&sc(9, 3_000, Command::Resume), Vec::new(), Vec::new()).expect("every Resume publishes");
        assert_eq!(b.kind, BatchKind::Resume);
        assert_eq!(b.origin, None);
        assert!(b.events.is_empty());
    }

    #[test]
    fn genesis_with_its_event_has_a_batch() {
        let g: Genesis = genesis(demo_config());
        let b = batch_of(&sc(1, 0, Command::Genesis(g)), vec![some_event()], Vec::new())
            .expect("genesis that emitted publishes");
        assert_eq!(b.kind, BatchKind::Genesis);
        assert_eq!(b.origin, None);
        assert_eq!(b.events, vec![some_event()]);
    }

    #[test]
    fn the_batch_carries_seq_time_and_balances_unchanged() {
        let events = vec![some_event(), some_event()];
        let b =
            batch_of(&user(42, MM1, 5), events.clone(), some_balances()).expect("a user command publishes");
        assert_eq!(
            b,
            EventBatch {
                seq: 42,
                time: 1_000,
                kind: BatchKind::User,
                origin: Some((MM1, 5)),
                events,
                balances: some_balances(),
            }
        );
    }
}
