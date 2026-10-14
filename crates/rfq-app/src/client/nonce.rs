//! Finding an actor's next nonce on a long-lived node, without reading its state.
//!
//! Nonces are strict per actor (only `last + 1` is accepted). A client that does not know an
//! actor's `last` (`rfq-demo` for the genesis roles, which keep their nonces across demo runs; the
//! gateway for any actor it has not seen since it started) finds it by sending probes.
//!
//! A probe is `Withdraw { amount: 0 }` with a guessed nonce. The engine checks the nonce first
//! (`Engine::apply_user`): `nonce <= last` is `Duplicate`, `nonce > last + 1` is `NonceGap`, and
//! neither consumes anything. A probe on exactly `last + 1` passes that check and **consumes the
//! nonce**, and only then is the zero amount refused with `InvalidInput`. So exactly one probe
//! (the last one) is consumed, its nonce is the actor's new `last`, and nothing but the nonce
//! changes.
//!
//! [`NonceSearch`] is the search as a state machine (one answer in, the next guess out), for a
//! client that waits for answers on its own event loop; [`sync_nonce`] drives it with a blocking
//! probe function.
use rfq_domain::{Action, RejectReason};

/// Why a nonce search failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NonceError {
    /// A probe got an answer other than the three nonce answers (for example `NotInitialized`,
    /// or no rejection at all).
    #[error("unexpected answer to a nonce probe: {0:?}")]
    UnexpectedAnswer(Option<RejectReason>),
    /// The guesses doubled past `u64::MAX` without finding a free nonce.
    #[error("nonce search overflowed u64")]
    Overflow,
    /// The answers contradict each other: no nonce below `below` can be free.
    #[error("nonce search found no free nonce below {below}: the probe answers are inconsistent")]
    Inconsistent { below: u64 },
}

/// How the engine answered a probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// `Duplicate`: the nonce is at most `last`.
    TooLow,
    /// `NonceGap`: the nonce is above `last + 1`.
    TooHigh,
    /// `InvalidInput`: the nonce was `last + 1` and is now consumed.
    Taken,
}

impl Probe {
    /// Classifies the rejection a probe got. Anything but the three expected answers (for
    /// example `NotInitialized`, or no rejection at all) is an error.
    pub fn from_rejection(rejection: Option<RejectReason>) -> Result<Probe, NonceError> {
        match rejection {
            Some(RejectReason::Duplicate) => Ok(Probe::TooLow),
            Some(RejectReason::NonceGap) => Ok(Probe::TooHigh),
            Some(RejectReason::InvalidInput) => Ok(Probe::Taken),
            other => Err(NonceError::UnexpectedAnswer(other)),
        }
    }
}

/// The probe command: refused only after its nonce has been checked and consumed.
pub fn probe_action() -> Action {
    Action::Withdraw { amount: 0 }
}

/// What to do after an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Search {
    /// Probe this nonce next.
    Probe(u64),
    /// The search is over: this nonce was taken, and it is the actor's `last` from now on.
    Found(u64),
}

/// The search over guesses. Doubles the guess while it is too low, then bisects, so it costs
/// about `2 log2(last)` probes, and one on a fresh actor.
///
/// Start with [`guess`](NonceSearch::guess), probe it, and feed each answer to
/// [`answer`](NonceSearch::answer) until it returns [`Search::Found`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NonceSearch {
    /// Every nonce below `lo` is used.
    lo: u64,
    /// While doubling, the outstanding guess. Once a probe was too high, `last + 1` is in `lo..hi`.
    hi: u64,
    /// The nonce whose answer is awaited.
    guess: u64,
    doubling: bool,
}

impl Default for NonceSearch {
    fn default() -> Self {
        NonceSearch::new()
    }
}

impl NonceSearch {
    pub fn new() -> NonceSearch {
        NonceSearch {
            lo: 1,
            hi: 1,
            guess: 1,
            doubling: true,
        }
    }

    /// The nonce to probe now: the one whose answer [`answer`](NonceSearch::answer) expects.
    pub fn guess(&self) -> u64 {
        self.guess
    }

    /// Takes the engine's answer to [`guess`](NonceSearch::guess) and says what to do next.
    pub fn answer(&mut self, probe: Probe) -> Result<Search, NonceError> {
        let guess = self.guess;
        match probe {
            Probe::Taken => return Ok(Search::Found(guess)),
            Probe::TooLow if self.doubling => {
                // `hi` is a power of two at most 2^63 here, so `hi + 1` cannot overflow.
                self.lo = self.hi + 1;
                self.hi = self.hi.checked_mul(2).ok_or(NonceError::Overflow)?;
                self.guess = self.hi;
                return Ok(Search::Probe(self.guess));
            }
            Probe::TooHigh if self.doubling => self.doubling = false,
            // Bisecting: `guess` is in `lo..hi`, so `guess + 1` cannot overflow.
            Probe::TooLow => self.lo = guess + 1,
            Probe::TooHigh => self.hi = guess,
        }
        if self.lo < self.hi {
            self.guess = self.lo + (self.hi - self.lo) / 2;
            Ok(Search::Probe(self.guess))
        } else {
            Err(NonceError::Inconsistent { below: self.hi })
        }
    }
}

/// Sends probes (through `probe`, which returns the engine's answer for a nonce) until one is
/// [`Probe::Taken`], and returns that nonce: the actor's `last` from now on. See [`NonceSearch`].
pub fn sync_nonce<E: From<NonceError>>(mut probe: impl FnMut(u64) -> Result<Probe, E>) -> Result<u64, E> {
    let mut search = NonceSearch::new();
    let mut nonce = search.guess();
    loop {
        match search.answer(probe(nonce)?)? {
            Search::Probe(next) => nonce = next,
            Search::Found(last) => return Ok(last),
        }
    }
}

#[cfg(test)]
mod tests {
    use rfq_domain::{Command, ONE_DOLLAR};
    use rfq_testkit::accounts::{ALICE, OPERATOR};
    use rfq_testkit::scenarios::demo_config;
    use rfq_testkit::{Harness, rejection};

    use super::*;

    /// The engine's strict nonce rule, alone.
    struct StrictNonce {
        last: u64,
    }

    impl StrictNonce {
        fn probe(&mut self, n: u64) -> Probe {
            if n <= self.last {
                Probe::TooLow
            } else if n == self.last + 1 {
                self.last = n;
                Probe::Taken
            } else {
                Probe::TooHigh
            }
        }
    }

    #[test]
    fn finds_and_takes_last_plus_one_for_every_start() {
        for last in 0..=1_100u64 {
            let mut engine = StrictNonce { last };
            let mut probes = 0u32;
            let mut taken = 0;
            let found = sync_nonce(|n| {
                probes += 1;
                let p = engine.probe(n);
                taken += u32::from(p == Probe::Taken);
                Ok::<_, NonceError>(p)
            })
            .unwrap();
            assert_eq!(found, last + 1, "last = {last}");
            assert_eq!(engine.last, last + 1, "exactly the found nonce is consumed");
            assert_eq!(taken, 1, "last = {last}");
            // Doubling up to the first power of two above `last + 1`, then bisecting below it.
            let bound = 2 * (u64::BITS - (last + 1).leading_zeros()) + 1;
            assert!(probes <= bound, "last = {last}: {probes} probes > {bound}");
        }
    }

    #[test]
    fn a_fresh_role_costs_one_probe() {
        let mut probes = Vec::new();
        let found = sync_nonce(|n| {
            probes.push(n);
            Ok::<_, NonceError>(StrictNonce { last: 0 }.probe(n))
        })
        .unwrap();
        assert_eq!((found, probes), (1, vec![1]));
    }

    #[test]
    fn inconsistent_answers_are_an_error() {
        assert_eq!(
            sync_nonce(|_| Ok::<_, NonceError>(Probe::TooHigh)),
            Err(NonceError::Inconsistent { below: 1 }),
            "nonce 1 can never be a gap"
        );
        assert!(
            sync_nonce(|n| Ok::<_, NonceError>(if n < 4 { Probe::TooLow } else { Probe::TooHigh })).is_err()
        );
    }

    #[test]
    fn only_the_three_nonce_answers_are_accepted() {
        assert_eq!(
            Probe::from_rejection(Some(RejectReason::Duplicate)).unwrap(),
            Probe::TooLow
        );
        assert_eq!(
            Probe::from_rejection(Some(RejectReason::NonceGap)).unwrap(),
            Probe::TooHigh
        );
        assert_eq!(
            Probe::from_rejection(Some(RejectReason::InvalidInput)).unwrap(),
            Probe::Taken
        );
        assert!(Probe::from_rejection(None).is_err());
        assert!(Probe::from_rejection(Some(RejectReason::NotInitialized)).is_err());
    }

    /// The error messages are the ones rfq-demo printed before the search moved here.
    #[test]
    fn errors_read_as_sentences() {
        assert_eq!(
            NonceError::UnexpectedAnswer(Some(RejectReason::NotInitialized)).to_string(),
            "unexpected answer to a nonce probe: Some(NotInitialized)"
        );
        assert_eq!(NonceError::Overflow.to_string(), "nonce search overflowed u64");
        assert_eq!(
            NonceError::Inconsistent { below: 4 }.to_string(),
            "nonce search found no free nonce below 4: the probe answers are inconsistent"
        );
    }

    /// A search that is only ever too low runs out of `u64` instead of looping or panicking.
    #[test]
    fn endless_too_low_answers_overflow_cleanly() {
        let mut probes = 0u32;
        let result = sync_nonce(|_| {
            probes += 1;
            Ok::<_, NonceError>(Probe::TooLow)
        });
        assert_eq!(result, Err(NonceError::Overflow));
        assert_eq!(probes, 64, "1, 2, 4, …, 2^63");
    }

    /// The state machine, driven one answer at a time, makes the same guesses as `sync_nonce`.
    #[test]
    fn the_state_machine_guesses_like_the_blocking_search() {
        for last in [0u64, 1, 2, 3, 6, 7, 8, 100, 1_023, 1_024] {
            let mut blocking = Vec::new();
            let mut engine = StrictNonce { last };
            sync_nonce(|n| {
                blocking.push(n);
                Ok::<_, NonceError>(engine.probe(n))
            })
            .unwrap();

            let mut engine = StrictNonce { last };
            let mut search = NonceSearch::new();
            let mut stepped = vec![search.guess()];
            loop {
                match search.answer(engine.probe(search.guess())).unwrap() {
                    Search::Probe(n) => {
                        assert_eq!(n, search.guess());
                        stepped.push(n);
                    }
                    Search::Found(found) => {
                        assert_eq!(found, last + 1);
                        break;
                    }
                }
            }
            assert_eq!(stepped, blocking, "last = {last}");
        }
    }

    /// The probe design checked against the real engine: the search consumes exactly one nonce,
    /// changes no balance, and the role's next command with `found + 1` is accepted.
    #[test]
    fn probes_on_a_real_engine_consume_one_nonce_and_change_nothing_else() {
        for used in [0u64, 1, 2, 3, 7, 8, 9, 40] {
            let mut h = Harness::new(demo_config());
            for _ in 0..used {
                h.fund(ALICE, 1); // OPERATOR commands with nonces 1..=used
            }
            let found = sync_nonce(|nonce| {
                let out = h.submit(Command::User {
                    actor: OPERATOR,
                    nonce,
                    action: probe_action(),
                });
                Probe::from_rejection(rejection(&out))
            })
            .unwrap();
            assert_eq!(found, used + 1);
            assert_eq!(h.engine.balance(ALICE).free, used * ONE_DOLLAR);
            let out = h.submit(Command::User {
                actor: OPERATOR,
                nonce: found + 1,
                action: Action::Deposit {
                    to: ALICE,
                    amount: ONE_DOLLAR,
                },
            });
            assert_eq!(rejection(&out), None, "used = {used}");
            assert_eq!(h.engine.balance(ALICE).free, (used + 1) * ONE_DOLLAR);
        }
    }
}
