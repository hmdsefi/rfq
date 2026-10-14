//! The nonce book (docs/specs/gateway.md §5): each actor's last used nonce, as the engine's output
//! shows it.
//!
//! Nonces are strict per actor: the engine takes only `last + 1`. The book learns `last` from every
//! egress batch with an origin (whoever sent it), and finds it by probing for an actor it has never
//! seen ([`rfq_app::client::nonce`], as `rfq-demo` does).
use std::collections::BTreeMap;

use rfq_app::client::nonce::{NonceError, NonceSearch, Probe, Search};
use rfq_domain::{AccountId, Audience, Emitted, Event, RejectReason};

/// What the book knows about an actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonceState {
    /// Its next command takes `last + 1`.
    Known { last: u64 },
    /// A probe search is running.
    Syncing,
    /// Never seen, or forgotten: its next command needs a probe search first.
    Unknown,
}

/// What to do after a probe's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncStep {
    /// Send the next probe, with this nonce.
    Probe(u64),
    /// Found: `last` was taken by the last probe, after `probes` probes. The actor is known.
    Synced { last: u64, probes: u32 },
    /// The answers make no sense (the engine is not initialized, say). The actor is unknown again.
    Failed(NonceError),
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    /// `seq` is the batch `last` was learnt from: the chunks of one split batch share it.
    Known {
        last: u64,
        seq: u64,
    },
    Syncing {
        search: NonceSearch,
        probes: u32,
    },
}

#[derive(Debug, Default)]
pub struct NonceBook {
    actors: BTreeMap<AccountId, Entry>,
}

impl NonceBook {
    pub fn new() -> NonceBook {
        NonceBook::default()
    }

    pub fn state(&self, actor: AccountId) -> NonceState {
        match self.actors.get(&actor) {
            Some(Entry::Known { last, .. }) => NonceState::Known { last: *last },
            Some(Entry::Syncing { .. }) => NonceState::Syncing,
            None => NonceState::Unknown,
        }
    }

    /// The nonce `actor`'s next command takes: `last + 1` for a known actor, else `None`.
    pub fn next(&self, actor: AccountId) -> Option<u64> {
        match self.actors.get(&actor) {
            Some(Entry::Known { last, .. }) => last.checked_add(1),
            _ => None,
        }
    }

    /// Starts a probe search for `actor` (forgetting what the book knew) and returns the first
    /// probe's nonce.
    pub fn start_sync(&mut self, actor: AccountId) -> u64 {
        let search = NonceSearch::new();
        self.actors.insert(actor, Entry::Syncing { search, probes: 1 });
        search.guess()
    }

    /// Takes the engine's answer to `actor`'s outstanding probe (the rejection in the probe's
    /// batch, at `seq`). `None` when `actor` is not syncing.
    pub fn answer(
        &mut self,
        actor: AccountId,
        seq: u64,
        rejection: Option<RejectReason>,
    ) -> Option<SyncStep> {
        let Some(Entry::Syncing { search, probes }) = self.actors.get_mut(&actor) else {
            return None;
        };
        let step = Probe::from_rejection(rejection).and_then(|p| search.answer(p));
        Some(match step {
            Ok(Search::Probe(nonce)) => {
                *probes = probes.saturating_add(1);
                SyncStep::Probe(nonce)
            }
            Ok(Search::Found(last)) => {
                let probes = *probes;
                self.actors.insert(actor, Entry::Known { last, seq });
                SyncStep::Synced { last, probes }
            }
            Err(e) => {
                self.actors.remove(&actor);
                SyncStep::Failed(e)
            }
        })
    }

    /// Follows the engine's answer to the command `origin = (actor, n)`, from its batch at `seq`.
    ///
    /// The nonce check runs before anything else in the command, and only it rejects with
    /// `Duplicate` or `NonceGap`; neither consumes the nonce. Any other outcome, a rejection
    /// included, consumed `n`, which is the actor's `last` from then on. (Timers fire in the same
    /// apply, before the command, so their events may come first: the batch's events are not
    /// always exactly the one rejection.)
    ///
    /// - A refusal of exactly the nonce the book would choose (`last + 1`) means the book is out
    ///   of step with the engine (it missed some egress): the actor becomes unknown, so its next
    ///   command probes again. Refusals of other nonces (explicit replays and gaps) change
    ///   nothing.
    /// - An oversized batch arrives as chunks that share `seq`, with the command's own events
    ///   last. The book advances on the first chunk and undoes it (unknown) if a later chunk
    ///   carries the refusal.
    /// - A syncing actor is left to its probe search.
    pub fn observe(&mut self, seq: u64, origin: (AccountId, u64), events: &[Emitted]) {
        let (actor, n) = origin;
        let refused = events.iter().any(|e| {
            e.audience == Audience::Account(actor)
                && matches!(
                    e.event,
                    Event::CommandRejected {
                        reason: RejectReason::Duplicate | RejectReason::NonceGap
                    }
                )
        });
        match self.actors.get(&actor).copied() {
            Some(Entry::Syncing { .. }) => {}
            Some(Entry::Known { last, seq: learnt }) if refused => {
                let same_batch = learnt == seq && last == n;
                if same_batch || last.checked_add(1) == Some(n) {
                    self.actors.remove(&actor);
                }
            }
            Some(Entry::Known { last, seq: learnt }) if learnt == seq && last == n => {}
            None if refused => {}
            Some(Entry::Known { .. }) | None => {
                self.actors.insert(actor, Entry::Known { last: n, seq });
            }
        }
    }

    /// Back to unknown. The hub calls it when the engine goes down with a command of `actor` in
    /// flight: that nonce may or may not have been consumed.
    pub fn forget(&mut self, actor: AccountId) {
        self.actors.remove(&actor);
    }
}

#[cfg(test)]
mod tests {
    use rfq_app::client::nonce::{NonceError, Probe, sync_nonce};
    use rfq_domain::{Event, QuoteId, RejectReason};

    use super::*;

    const ALICE: AccountId = AccountId(10);
    const MM1: AccountId = AccountId(20);

    fn rejected(actor: AccountId, reason: RejectReason) -> Emitted {
        Emitted::to(actor, Event::CommandRejected { reason })
    }

    fn withdrawn(actor: AccountId) -> Emitted {
        Emitted::to(
            actor,
            Event::Withdrawn {
                account: actor,
                amount: 1,
            },
        )
    }

    /// A timer's event that fired in the same apply, before the command's own events.
    fn expired(mm: AccountId) -> Emitted {
        Emitted::to(mm, Event::QuoteExpired { quote: QuoteId(3) })
    }

    #[test]
    fn nonce_book_advances_on_consumed_nonces_only() {
        let mut book = NonceBook::new();
        assert_eq!(book.state(ALICE), NonceState::Unknown);
        assert_eq!(book.next(ALICE), None);

        book.observe(5, (ALICE, 4), &[withdrawn(ALICE)]);
        assert_eq!(book.state(ALICE), NonceState::Known { last: 4 });
        assert_eq!(book.next(ALICE), Some(5));

        // A replay and a gap consume nothing, also with a timer's events in front of them.
        book.observe(6, (ALICE, 2), &[rejected(ALICE, RejectReason::Duplicate)]);
        book.observe(7, (ALICE, 9), &[rejected(ALICE, RejectReason::NonceGap)]);
        book.observe(
            8,
            (ALICE, 4),
            &[expired(MM1), rejected(ALICE, RejectReason::Duplicate)],
        );
        assert_eq!(book.state(ALICE), NonceState::Known { last: 4 });

        // Any other rejection comes after the nonce check, which consumed the nonce.
        book.observe(9, (ALICE, 5), &[rejected(ALICE, RejectReason::InvalidState)]);
        assert_eq!(book.state(ALICE), NonceState::Known { last: 5 });
        book.observe(
            10,
            (ALICE, 6),
            &[expired(MM1), rejected(ALICE, RejectReason::InsufficientFunds)],
        );
        assert_eq!(book.next(ALICE), Some(7));

        // An actor learnt from egress alone (another client's command) is known from then on.
        book.observe(11, (MM1, 41), &[expired(MM1)]);
        assert_eq!(book.next(MM1), Some(42));
        // An unknown actor stays unknown when its command consumed nothing.
        let carol = AccountId(30);
        book.observe(12, (carol, 3), &[rejected(carol, RejectReason::NonceGap)]);
        assert_eq!(book.state(carol), NonceState::Unknown);
    }

    /// The engine's strict nonce rule, alone, answering as `Engine::apply_user` does.
    struct StrictNonce {
        last: u64,
    }

    impl StrictNonce {
        fn answer(&mut self, n: u64) -> RejectReason {
            if n <= self.last {
                RejectReason::Duplicate
            } else if n == self.last + 1 {
                self.last = n;
                RejectReason::InvalidInput // the probe's `Withdraw { amount: 0 }`
            } else {
                RejectReason::NonceGap
            }
        }
    }

    #[test]
    fn nonce_book_probes_an_unknown_actor_like_rfq_demo() {
        for last in [0u64, 1, 2, 5, 6, 7, 8, 9, 63, 64, 65, 1_000] {
            // rfq-demo's probes for this `last`.
            let mut engine = StrictNonce { last };
            let mut demo = Vec::new();
            sync_nonce(|n| {
                demo.push(n);
                Probe::from_rejection(Some(engine.answer(n)))
            })
            .unwrap();

            let mut engine = StrictNonce { last };
            let mut book = NonceBook::new();
            let mut probes = vec![book.start_sync(ALICE)];
            let mut seq = 100;
            let found = loop {
                assert_eq!(book.state(ALICE), NonceState::Syncing);
                assert_eq!(book.next(ALICE), None, "no nonce while syncing");
                seq += 1;
                let nonce = *probes.last().unwrap();
                match book.answer(ALICE, seq, Some(engine.answer(nonce))) {
                    Some(SyncStep::Probe(n)) => probes.push(n),
                    Some(SyncStep::Synced { last, probes: count }) => {
                        assert_eq!(count as usize, probes.len());
                        break last;
                    }
                    other => panic!("last = {last}: {other:?}"),
                }
            };
            assert_eq!(probes, demo, "last = {last}: the same probes as rfq-demo");
            assert_eq!(
                found,
                last + 1,
                "the search ends at Taken: the found nonce is consumed"
            );
            assert_eq!(book.state(ALICE), NonceState::Known { last: last + 1 });
            assert_eq!(book.next(ALICE), Some(last + 2));
        }
    }

    #[test]
    fn a_failed_search_leaves_the_actor_unknown() {
        let mut book = NonceBook::new();
        assert_eq!(book.start_sync(ALICE), 1);
        assert_eq!(
            book.answer(ALICE, 1, Some(RejectReason::NotInitialized)),
            Some(SyncStep::Failed(NonceError::UnexpectedAnswer(Some(
                RejectReason::NotInitialized
            ))))
        );
        assert_eq!(book.state(ALICE), NonceState::Unknown);
        // Nonce 1 can never be a gap: the answers are inconsistent.
        book.start_sync(ALICE);
        assert!(matches!(
            book.answer(ALICE, 2, Some(RejectReason::NonceGap)),
            Some(SyncStep::Failed(NonceError::Inconsistent { .. }))
        ));
        assert_eq!(book.state(ALICE), NonceState::Unknown);
        // An answer for an actor that is not syncing is not an answer.
        assert_eq!(book.answer(MM1, 3, Some(RejectReason::Duplicate)), None);
    }

    /// The gateway sends `last + 1` for a known actor. When the engine refuses exactly that nonce
    /// as a replay or a gap, the book is out of step with the engine (it missed some egress): the
    /// actor goes back to unknown, so its next command probes again instead of failing forever.
    #[test]
    fn a_refused_expected_nonce_means_the_book_is_out_of_step() {
        let mut book = NonceBook::new();
        book.observe(1, (ALICE, 4), &[withdrawn(ALICE)]);
        book.observe(2, (ALICE, 5), &[rejected(ALICE, RejectReason::Duplicate)]);
        assert_eq!(book.state(ALICE), NonceState::Unknown);

        book.observe(3, (MM1, 4), &[withdrawn(MM1)]);
        book.observe(4, (MM1, 5), &[rejected(MM1, RejectReason::NonceGap)]);
        assert_eq!(book.state(MM1), NonceState::Unknown);
    }

    /// An egress batch too big for one message arrives as chunks with one `seq` and `origin`
    /// (`rfq_infra::transport::split_batch`); the command's own events come last. The book counts
    /// the command once, and a leading chunk of timer events cannot make it advance for good.
    #[test]
    fn the_chunks_of_one_batch_count_once() {
        let mut book = NonceBook::new();
        book.observe(1, (ALICE, 4), &[withdrawn(ALICE)]);
        // Consumed: timer events first, then the command's, then the balances alone.
        book.observe(2, (ALICE, 5), &[expired(MM1)]);
        book.observe(2, (ALICE, 5), &[withdrawn(ALICE)]);
        book.observe(2, (ALICE, 5), &[]);
        assert_eq!(book.state(ALICE), NonceState::Known { last: 5 });
        // Not consumed: the leading chunk looked consumed, the last one says it was a replay.
        book.observe(3, (ALICE, 6), &[expired(MM1)]);
        book.observe(3, (ALICE, 6), &[rejected(ALICE, RejectReason::Duplicate)]);
        assert_eq!(book.state(ALICE), NonceState::Unknown);
    }

    #[test]
    fn a_syncing_actor_ignores_other_batches_and_forget_resets_anyone() {
        let mut book = NonceBook::new();
        book.start_sync(ALICE);
        book.observe(1, (ALICE, 9), &[withdrawn(ALICE)]);
        assert_eq!(book.state(ALICE), NonceState::Syncing);
        book.forget(ALICE);
        assert_eq!(book.state(ALICE), NonceState::Unknown);

        book.observe(2, (MM1, 3), &[withdrawn(MM1)]);
        book.forget(MM1);
        assert_eq!(book.state(MM1), NonceState::Unknown);
        assert_eq!(book.next(MM1), None);
    }

    /// No arithmetic on client-chosen numbers can overflow.
    #[test]
    fn the_largest_nonce_has_no_next() {
        let mut book = NonceBook::new();
        book.observe(1, (ALICE, u64::MAX), &[withdrawn(ALICE)]);
        assert_eq!(book.next(ALICE), None);
        book.observe(2, (ALICE, u64::MAX), &[rejected(ALICE, RejectReason::Duplicate)]);
        assert_eq!(book.state(ALICE), NonceState::Known { last: u64::MAX });
    }
}
