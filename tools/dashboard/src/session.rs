//! One engine plus what the sequencer adds around it: the command log, a forward-only clock and
//! per-actor nonces. Like `rfq_testkit::Harness`, except that it never panics: invariant results
//! are reported to the page, not asserted.
use std::collections::BTreeMap;
use std::fmt;

use rfq_domain::*;
use rfq_testkit::genesis;

/// One sequenced command and everything it emitted.
#[derive(Clone, Debug)]
pub struct Applied {
    pub seq: u64,
    pub time: Timestamp,
    pub cmd: Command,
    pub events: Vec<Emitted>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionError {
    TimeBackwards { now: Timestamp, requested: Timestamp },
    NoTimer,
    Overflow,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::TimeBackwards { now, requested } => {
                write!(
                    f,
                    "time can't go backwards (now {now} ms, requested {requested} ms)"
                )
            }
            SessionError::NoTimer => write!(f, "no timer is pending"),
            SessionError::Overflow => write!(f, "number too large"),
        }
    }
}

/// The state rebuilt from the log in a fresh engine, compared with the live one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayCheck {
    pub commands: usize,
    pub live: [u8; 32],
    pub replayed: [u8; 32],
}

impl ReplayCheck {
    pub fn matches(&self) -> bool {
        self.live == self.replayed
    }
}

pub struct Session {
    engine: Engine,
    log: Vec<SequencedCommand>,
    seq: u64,
    time: Timestamp,
    nonces: BTreeMap<AccountId, u64>,
}

impl Session {
    /// A fresh engine with `Genesis` applied at seq 1, time 0.
    pub fn new(config: Config) -> (Session, Applied) {
        let mut s = Session {
            engine: Engine::new(),
            log: Vec::new(),
            seq: 0,
            time: 0,
            nonces: BTreeMap::new(),
        };
        let applied = s.submit(Command::Genesis(genesis(config)));
        (s, applied)
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn log(&self) -> &[SequencedCommand] {
        &self.log
    }

    pub fn now(&self) -> Timestamp {
        self.time
    }

    /// The seq (and so the `RequestId`/`QuoteId`) the next command will get.
    pub fn next_seq(&self) -> u64 {
        self.seq + 1
    }

    /// The last nonce the engine consumed for `actor` (0 before its first command).
    pub fn last_nonce(&self, actor: AccountId) -> u64 {
        self.nonces.get(&actor).copied().unwrap_or(0)
    }

    pub fn set_time(&mut self, t: Timestamp) -> Result<(), SessionError> {
        if t < self.time {
            return Err(SessionError::TimeBackwards {
                now: self.time,
                requested: t,
            });
        }
        self.time = t;
        Ok(())
    }

    /// A user command with the actor's next nonce.
    pub fn act(&mut self, actor: AccountId, action: Action) -> Applied {
        let nonce = self.last_nonce(actor).saturating_add(1);
        self.raw(actor, nonce, action)
    }

    /// A user command with an explicit nonce (attack presets). The nonce counts as used unless
    /// the engine answered `Duplicate` or `NonceGap`, which consume nothing.
    pub fn raw(&mut self, actor: AccountId, nonce: u64, action: Action) -> Applied {
        let applied = self.submit(Command::User { actor, nonce, action });
        let unconsumed = applied.events.iter().any(|e| {
            matches!(
                e.event,
                Event::CommandRejected {
                    reason: RejectReason::Duplicate | RejectReason::NonceGap
                }
            )
        });
        if !unconsumed {
            self.nonces.insert(actor, nonce);
        }
        applied
    }

    pub fn tick(&mut self) -> Applied {
        self.submit(Command::Tick)
    }

    /// Moves the clock forward by `ms` and ticks, so every timer due by then fires.
    pub fn advance(&mut self, ms: Duration) -> Result<Applied, SessionError> {
        let t = self.time.checked_add(ms).ok_or(SessionError::Overflow)?;
        self.set_time(t)?;
        Ok(self.tick())
    }

    /// Moves the clock to the engine's next timer (never backwards) and ticks.
    pub fn tick_next_timer(&mut self) -> Result<Applied, SessionError> {
        let due = self.engine.next_timer().ok_or(SessionError::NoTimer)?;
        self.set_time(due.max(self.time))?;
        Ok(self.tick())
    }

    /// Rebuilds the state from the log in a fresh engine; the in-memory twin of demo.sh's check.
    pub fn replay_check(&self) -> ReplayCheck {
        let mut fresh = Engine::new();
        for sc in &self.log {
            fresh.apply(sc);
        }
        ReplayCheck {
            commands: self.log.len(),
            live: self.engine.state_hash(),
            replayed: fresh.state_hash(),
        }
    }

    fn submit(&mut self, cmd: Command) -> Applied {
        self.seq += 1;
        let sc = SequencedCommand {
            seq: self.seq,
            time: self.time,
            cmd,
        };
        let events = self.engine.apply(&sc);
        let applied = Applied {
            seq: sc.seq,
            time: sc.time,
            cmd: sc.cmd.clone(),
            events,
        };
        self.log.push(sc);
        applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_testkit::accounts::*;
    use rfq_testkit::scenarios::demo_config;
    use rfq_testkit::{dollars, names};

    fn deposit(to: AccountId) -> Action {
        Action::Deposit {
            to,
            amount: dollars(10),
        }
    }

    fn nonce_of(cmd: &Command) -> u64 {
        match cmd {
            Command::User { nonce, .. } => *nonce,
            other => panic!("not a user command: {other:?}"),
        }
    }

    #[test]
    fn genesis_is_seq_1_at_time_0() {
        let (s, applied) = Session::new(demo_config());
        assert_eq!((applied.seq, applied.time), (1, 0));
        assert_eq!(names(&applied.events), vec!["Initialized"]);
        assert_eq!(s.next_seq(), 2);
        assert_eq!(s.log().len(), 1);
    }

    #[test]
    fn act_assigns_nonces_per_actor() {
        let (mut s, _) = Session::new(demo_config());
        let n: Vec<u64> = (0..3)
            .map(|_| nonce_of(&s.act(OPERATOR, deposit(ALICE)).cmd))
            .collect();
        assert_eq!(n, vec![1, 2, 3]);
        assert_eq!(s.last_nonce(OPERATOR), 3);
        assert_eq!(s.last_nonce(ALICE), 0);
    }

    #[test]
    fn duplicate_and_gap_nonces_are_not_consumed() {
        let (mut s, _) = Session::new(demo_config());
        s.act(OPERATOR, deposit(ALICE));
        let dup = s.raw(OPERATOR, 1, deposit(ALICE));
        assert_eq!(names(&dup.events), vec!["CommandRejected"]);
        let gap = s.raw(OPERATOR, 3, deposit(ALICE));
        assert_eq!(names(&gap.events), vec!["CommandRejected"]);
        assert_eq!(s.last_nonce(OPERATOR), 1);
        let next = s.act(OPERATOR, deposit(ALICE));
        assert_eq!(nonce_of(&next.cmd), 2);
        assert_eq!(names(&next.events), vec!["Deposited"]);
    }

    #[test]
    fn time_never_goes_backwards() {
        let (mut s, _) = Session::new(demo_config());
        assert_eq!(s.set_time(5), Ok(()));
        assert_eq!(
            s.set_time(4),
            Err(SessionError::TimeBackwards { now: 5, requested: 4 })
        );
        assert_eq!(s.now(), 5);
    }

    #[test]
    fn advance_overflow_is_an_error() {
        let (mut s, _) = Session::new(demo_config());
        s.set_time(1).expect("forward");
        assert_eq!(s.advance(u64::MAX).map(|a| a.seq), Err(SessionError::Overflow));
        assert_eq!(s.now(), 1);
    }

    #[test]
    fn advance_ticks_at_the_new_time() {
        let (mut s, _) = Session::new(demo_config());
        let a = s.advance(250).expect("advance");
        assert_eq!((a.time, s.now()), (250, 250));
        assert!(matches!(a.cmd, Command::Tick));
    }

    #[test]
    fn next_timer_without_timers_is_an_error() {
        let (mut s, _) = Session::new(demo_config());
        assert_eq!(s.tick_next_timer().map(|a| a.seq), Err(SessionError::NoTimer));
    }

    #[test]
    fn replay_rebuilds_the_same_state() {
        let (mut s, _) = Session::new(demo_config());
        s.act(OPERATOR, deposit(ALICE));
        s.raw(OPERATOR, 1, deposit(ALICE));
        s.advance(10).expect("advance");
        let check = s.replay_check();
        assert_eq!(check.commands, 4);
        assert!(check.matches());
    }

    #[test]
    fn errors_read_as_sentences() {
        let e = SessionError::TimeBackwards { now: 5, requested: 4 };
        assert_eq!(
            e.to_string(),
            "time can't go backwards (now 5 ms, requested 4 ms)"
        );
        assert_eq!(SessionError::NoTimer.to_string(), "no timer is pending");
        assert_eq!(SessionError::Overflow.to_string(), "number too large");
    }
}
