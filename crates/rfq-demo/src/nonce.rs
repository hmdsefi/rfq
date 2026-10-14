//! Finding a genesis role's next nonce on a long-lived node, without reading its state.
//!
//! Nonces are strict per actor (only `last + 1` is accepted). Scenario accounts are fresh on every
//! run, so their nonces start at 1, but `OPERATOR` and `ARBITER` are fixed by the node's genesis and
//! keep their nonces across demo runs. The demo finds their `last` by sending probes.
//!
//! A probe is `Withdraw { amount: 0 }` with a guessed nonce. The engine checks the nonce first
//! (`Engine::apply_user`): `nonce <= last` is `Duplicate`, `nonce > last + 1` is `NonceGap`, and
//! neither consumes anything. A probe on exactly `last + 1` passes that check and **consumes the
//! nonce**, and only then is the zero amount refused with `InvalidInput`. So exactly one probe
//! (the last one) is consumed, its nonce is the role's new `last`, and nothing but the nonce
//! changes.
use anyhow::{Context, bail};
use rfq_domain::{Action, RejectReason};

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
    pub fn from_rejection(rejection: Option<RejectReason>) -> anyhow::Result<Probe> {
        match rejection {
            Some(RejectReason::Duplicate) => Ok(Probe::TooLow),
            Some(RejectReason::NonceGap) => Ok(Probe::TooHigh),
            Some(RejectReason::InvalidInput) => Ok(Probe::Taken),
            other => bail!("unexpected answer to a nonce probe: {other:?}"),
        }
    }
}

/// The probe command: refused only after its nonce has been checked and consumed.
pub fn probe_action() -> Action {
    Action::Withdraw { amount: 0 }
}

/// Sends probes (through `probe`, which returns the engine's answer for a nonce) until one is
/// [`Probe::Taken`], and returns that nonce: the role's `last` from now on. Doubles the guess while
/// it is too low, then bisects, so it costs about `2 log2(last)` probes, and one on a fresh node.
pub fn sync_nonce(mut probe: impl FnMut(u64) -> anyhow::Result<Probe>) -> anyhow::Result<u64> {
    // Every nonce below `lo` is used. Once a probe is too high, `last + 1` is in `lo..hi`.
    let mut lo = 1u64;
    let mut hi = 1u64;
    loop {
        match probe(hi)? {
            Probe::Taken => return Ok(hi),
            Probe::TooLow => {
                lo = hi + 1;
                hi = hi.checked_mul(2).context("nonce search overflowed u64")?;
            }
            Probe::TooHigh => break,
        }
    }
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match probe(mid)? {
            Probe::Taken => return Ok(mid),
            Probe::TooLow => lo = mid + 1,
            Probe::TooHigh => hi = mid,
        }
    }
    bail!("nonce search found no free nonce below {hi}: the probe answers are inconsistent")
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
                Ok(p)
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
            Ok(StrictNonce { last: 0 }.probe(n))
        })
        .unwrap();
        assert_eq!((found, probes), (1, vec![1]));
    }

    #[test]
    fn inconsistent_answers_are_an_error() {
        assert!(
            sync_nonce(|_| Ok(Probe::TooHigh)).is_err(),
            "nonce 1 can never be a gap"
        );
        assert!(sync_nonce(|n| Ok(if n < 4 { Probe::TooLow } else { Probe::TooHigh })).is_err());
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
