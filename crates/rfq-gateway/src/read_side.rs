//! The replica thread (docs/specs/read-api.md §5, §7): feeds the gateway's [`ReadSide`] from the
//! journal. One thread owns the journal follower and nothing else touches it: `JournalFollower`
//! and rusteron's `Aeron` are not `Send`, so the thread connects its own Aeron client and opens
//! the follower inside itself ([`spawn`], feature `aeron`). The loop itself ([`run`]) is generic
//! over a [`Journal`], so its rules are tested without Aeron.
//!
//! The rules:
//! - **Following.** `poll` into [`ReadSide::apply_or_reset`], and sleep [`Timing::idle`] when a
//!   poll delivered nothing. `Ok(0)` only means "nothing from this call", never "caught up":
//!   the state is `catching_up` until [`Journal::caught_up`] is first true, then `following`.
//!   Normal lag, or an engine outage, does not flip it back; a rebuild or an error does.
//! - **A new journal.** `seq` 1 after a higher `seq` (`docker compose down -v`): `apply_or_reset`
//!   replaces the replica with an empty one, and the replica follows the new journal.
//! - **Any other sequence error, or a follower error:** the state is `unavailable` and the
//!   error is logged. The replica keeps serving what it has. After [`Timing::retry`] a new
//!   follower replays the journal from the start into a fresh replica, on the side. That replica
//!   takes over once it has reached the serving one's `last_seq`, and sooner if the journal is
//!   not the one the serving replica was built from: its batch at some `seq` differs, or the
//!   journal is fully delivered and shorter. The same follower then goes on feeding the
//!   [`ReadSide`] directly.
//! - **A dead Aeron client is fatal for the gateway,** as the hub loop's is: the thread logs it
//!   and exits the process with status 1 (docker-compose restarts it).
use std::convert::Infallible;
use std::time::Duration;

use rfq_domain::SequencedCommand;
use rfq_gateway::read_api::{ReadSide, ReplicaState};
use rfq_read::{Replica, TraceEntry};

/// What the replica thread follows: the journal's commands in order.
pub trait Journal {
    /// Delivers the next recorded commands to `sink`, and returns how many. `Err` ends this
    /// follower.
    fn poll(&mut self, sink: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<usize>;
    /// Whether everything the journal has recorded has been delivered.
    fn caught_up(&self) -> bool;
}

#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Sleep after a poll that delivered nothing.
    pub idle: Duration,
    /// Wait after a failure, before the next follower is opened.
    pub retry: Duration,
}

pub const TIMING: Timing = Timing {
    idle: Duration::from_millis(1),
    retry: Duration::from_secs(1),
};

/// Where a follower's commands go.
enum Feed {
    /// Into the [`ReadSide`].
    Live,
    /// Into a fresh replica, until it can take over (see the module doc).
    Rebuild(Box<Replica>),
}

/// A follower and where it feeds.
struct Session<J> {
    journal: J,
    feed: Feed,
}

/// Follows the journal until `alive` fails (the Aeron client is gone), which is the only way out.
///
/// `open` makes a new follower: the first one, and one after each failure. It runs on this
/// thread, so a follower that is not `Send` is fine.
pub fn run<J: Journal>(
    read: &ReadSide,
    timing: Timing,
    mut open: impl FnMut() -> anyhow::Result<J>,
    mut alive: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<Infallible> {
    let mut session: Option<Session<J>> = None;
    // Whether a follower has failed: the next one rebuilds on the side.
    let mut failed = false;
    loop {
        alive()?;
        let Some(current) = session.as_mut() else {
            match open() {
                Ok(journal) => {
                    // A replica that has nothing yet has nothing to keep serving while it rebuilds.
                    let feed = if failed && read.last_seq() > 0 {
                        Feed::Rebuild(Box::default())
                    } else {
                        transition(read, ReplicaState::CatchingUp);
                        Feed::Live
                    };
                    session = Some(Session { journal, feed });
                }
                Err(e) => {
                    fail(read, &e.context("open the journal follower"));
                    failed = true;
                    std::thread::sleep(timing.retry);
                }
            }
            continue;
        };
        match current.step(read) {
            Ok(true) => {}
            Ok(false) => std::thread::sleep(timing.idle),
            Err(e) => {
                fail(read, &e);
                failed = true;
                session = None; // closes the follower before the next one opens
                std::thread::sleep(timing.retry);
            }
        }
    }
}

impl<J: Journal> Session<J> {
    /// One poll, and what follows from it. Returns whether it delivered anything.
    fn step(&mut self, read: &ReadSide) -> anyhow::Result<bool> {
        let Session { journal, feed } = self;
        let mut error = None;
        let delivered = journal.poll(&mut |sc| {
            if error.is_none()
                && let Err(e) = apply(read, feed, &sc)
            {
                error = Some(e);
            }
        })?;
        if let Some(e) = error {
            return Err(e);
        }
        // Asked only when it matters: a rebuild waits for it, and so does the first `following`.
        if matches!(feed, Feed::Rebuild(_)) && journal.caught_up() {
            // Everything the journal holds is in the rebuilt replica, and it is behind the
            // serving one: the journal is not the one the serving replica was built from.
            swap(read, feed);
        }
        if matches!(feed, Feed::Live) && read.state() != ReplicaState::Following && journal.caught_up() {
            transition(read, ReplicaState::Following);
        }
        Ok(delivered > 0)
    }
}

/// One command into the replica, or into the rebuild.
fn apply(read: &ReadSide, feed: &mut Feed, sc: &SequencedCommand) -> anyhow::Result<()> {
    let Feed::Rebuild(fresh) = feed else {
        return Ok(read.apply_or_reset(sc)?);
    };
    if sc.seq == 1 && fresh.last_seq() >= 1 {
        **fresh = Replica::new(); // the journal begins again, as in `apply_or_reset`
    }
    fresh.apply(sc)?;
    if takes_over(read, fresh, sc.seq) {
        swap(read, feed);
    }
    Ok(())
}

/// Whether the rebuilt replica, which has just applied `seq`, should replace the serving one: it
/// has caught up with it, or its batch at `seq` is not the serving replica's, so they follow
/// different journals.
fn takes_over(read: &ReadSide, fresh: &Replica, seq: u64) -> bool {
    read.read(|serving, _| {
        fresh.last_seq() >= serving.last_seq() || entry_at(serving, seq) != entry_at(fresh, seq)
    })
}

fn entry_at(r: &Replica, seq: u64) -> Option<&TraceEntry> {
    r.trace_after(seq.saturating_sub(1))
        .next()
        .filter(|e| e.batch.seq == seq)
}

/// Puts the rebuilt replica in service, and the follower on the [`ReadSide`] directly.
fn swap(read: &ReadSide, feed: &mut Feed) {
    if let Feed::Rebuild(fresh) = std::mem::replace(feed, Feed::Live) {
        let was = read.last_seq();
        read.replace(*fresh);
        tracing::info!(was, last_seq = read.last_seq(), "replica rebuilt");
        // Following again is for `caught_up` to say.
        transition(read, ReplicaState::CatchingUp);
    }
}

fn fail(read: &ReadSide, error: &anyhow::Error) {
    tracing::warn!(error = %format_args!("{error:#}"), "replica follower failed");
    transition(read, ReplicaState::Unavailable);
}

/// Sets the replica's state, and logs a change.
fn transition(read: &ReadSide, to: ReplicaState) {
    if read.state() != to {
        read.set_state(to);
        tracing::info!(state = %to.as_str(), last_seq = read.last_seq(), "replica");
    }
}

#[cfg(feature = "aeron")]
mod aeron {
    use std::io;
    use std::sync::Arc;
    use std::thread::JoinHandle;

    use anyhow::{Context, ensure};
    use rfq_domain::SequencedCommand;
    use rfq_gateway::read_api::{ReadSide, ReplicaState};
    use rfq_infra::aeron::{self, JournalFollower};

    use super::{Journal, TIMING, run};

    impl Journal for JournalFollower {
        fn poll(&mut self, sink: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<usize> {
            Ok(JournalFollower::poll(self, sink)?)
        }

        fn caught_up(&self) -> bool {
            JournalFollower::caught_up(self)
        }
    }

    /// Marks the replica `unavailable` if the thread ends by a panic: the replica then still serves
    /// what it had, and says it is no longer followed.
    struct Dying(Arc<ReadSide>);

    impl Drop for Dying {
        fn drop(&mut self) {
            self.0.set_state(ReplicaState::Unavailable);
        }
    }

    /// Starts the replica thread. It connects its own Aeron client to the media driver at
    /// `aeron_dir`, and exits the process with status 1 if that fails or the client dies.
    pub fn spawn(aeron_dir: String, read: Arc<ReadSide>) -> io::Result<JoinHandle<()>> {
        std::thread::Builder::new().name("replica".into()).spawn(move || {
            let _dying = Dying(Arc::clone(&read));
            let Err(e) = follow(&aeron_dir, &read);
            tracing::error!("{e:#}");
            std::process::exit(1);
        })
    }

    fn follow(aeron_dir: &str, read: &ReadSide) -> anyhow::Result<std::convert::Infallible> {
        let client = aeron::connect(aeron_dir)
            .with_context(|| format!("connect the replica to the media driver at {aeron_dir}"))?;
        tracing::info!("replica connected (journal follower on stream 1005)");
        run(
            read,
            TIMING,
            || Ok(JournalFollower::open(&client)?),
            || {
                ensure!(
                    !client.is_closed(),
                    "the replica's Aeron client is closed (is the media driver gone?)"
                );
                Ok(())
            },
        )
    }
}

#[cfg(feature = "aeron")]
pub use aeron::spawn;

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::Arc;

    use anyhow::{anyhow, bail};
    use rfq_testkit::scenarios::{all, demo_config, run_in_memory};
    use rfq_testkit::{Harness, accounts::*};
    use serde_json::{Value, json};

    use super::*;

    const T0: u64 = 1_000;
    const NOW: Timing = Timing {
        idle: Duration::ZERO,
        retry: Duration::ZERO,
    };

    fn happy_log() -> Vec<SequencedCommand> {
        let s = all().into_iter().find(|s| s.name == "happy_path").unwrap();
        run_in_memory(&s, T0).0.log
    }

    /// A different journal: it starts with the same Genesis, then deposits $50 to Alice.
    fn other_log() -> Vec<SequencedCommand> {
        let mut h = Harness::new(demo_config());
        h.set_time(T0);
        for _ in 0..3 {
            h.fund(ALICE, 50);
        }
        h.log
    }

    enum Step {
        /// A poll that delivers these.
        Deliver(Vec<SequencedCommand>),
        /// A poll that delivers nothing.
        Idle,
        /// A poll that fails.
        Fail(&'static str),
        /// A poll that delivers nothing, after setting what `caught_up` says.
        CaughtUp(bool),
        /// A poll that delivers nothing, after running this.
        Check(Box<dyn Fn()>),
    }

    /// A follower that plays a script, one step per poll. When the last follower's script is over,
    /// the run ends (the fake client dies).
    struct Fake {
        steps: VecDeque<Step>,
        caught_up: Cell<bool>,
        last: bool,
        over: Rc<Cell<bool>>,
    }

    impl Journal for Fake {
        fn poll(&mut self, sink: &mut dyn FnMut(SequencedCommand)) -> anyhow::Result<usize> {
            let step = self.steps.pop_front();
            if self.steps.is_empty() && self.last {
                self.over.set(true);
            }
            match step {
                None | Some(Step::Idle) => Ok(0),
                Some(Step::Deliver(commands)) => {
                    let n = commands.len();
                    commands.into_iter().for_each(sink);
                    Ok(n)
                }
                Some(Step::Fail(why)) => bail!(why),
                Some(Step::CaughtUp(c)) => {
                    self.caught_up.set(c);
                    Ok(0)
                }
                Some(Step::Check(f)) => {
                    f();
                    Ok(0)
                }
            }
        }

        fn caught_up(&self) -> bool {
            self.caught_up.get()
        }
    }

    /// How a run went.
    struct Played {
        /// Why it ended (the script is over).
        end: anyhow::Error,
        /// The replica's state each time a follower was asked for.
        at_open: Vec<ReplicaState>,
    }

    /// Runs `followers` in turn (`Err` is an open that fails) against `read`.
    fn play(read: &ReadSide, followers: Vec<Result<Vec<Step>, &'static str>>) -> Played {
        let over = Rc::new(Cell::new(false));
        let queue = RefCell::new(VecDeque::from(followers));
        let at_open = RefCell::new(Vec::new());
        let open = || {
            at_open.borrow_mut().push(read.state());
            let Some(script) = queue.borrow_mut().pop_front() else {
                over.set(true);
                bail!("no follower left");
            };
            let last = queue.borrow().is_empty();
            let steps = match script {
                Ok(steps) => steps,
                Err(why) => {
                    over.set(last);
                    return Err(anyhow!(why));
                }
            };
            if last && steps.is_empty() {
                over.set(true);
            }
            Ok(Fake {
                steps: steps.into(),
                caught_up: Cell::new(false),
                last,
                over: Rc::clone(&over),
            })
        };
        let alive = || {
            if over.get() {
                bail!("the script is over");
            }
            Ok(())
        };
        let end = match run(read, NOW, open, alive) {
            Err(e) => e,
            Ok(never) => match never {},
        };
        Played {
            end,
            at_open: at_open.into_inner(),
        }
    }

    /// `check` as a step, with the replica to look at.
    fn check(read: &Arc<ReadSide>, f: impl Fn(&ReadSide) + 'static) -> Step {
        let read = Arc::clone(read);
        Step::Check(Box::new(move || f(&read)))
    }

    #[test]
    fn it_follows_the_journal_and_turns_following_only_when_the_follower_says_caught_up() {
        let log = happy_log();
        let read = ReadSide::new();
        assert_eq!(read.state(), ReplicaState::CatchingUp);
        let played = play(
            &read,
            vec![Ok(vec![
                Step::Idle, // nothing yet, and no word of being caught up
                Step::Deliver(log[..5].to_vec()),
                Step::Idle,
                check(&read, |r| {
                    assert_eq!(r.last_seq(), 5);
                    assert_eq!(
                        r.state(),
                        ReplicaState::CatchingUp,
                        "empty polls are not 'caught up'"
                    );
                }),
                Step::CaughtUp(true),
                Step::Deliver(log[5..9].to_vec()),
                check(&read, |r| {
                    assert_eq!(r.last_seq(), 9);
                    assert_eq!(r.state(), ReplicaState::Following);
                }),
                // Normal lag: the journal moves ahead, or the engine stops. Still following.
                Step::CaughtUp(false),
                Step::Deliver(log[9..].to_vec()),
                Step::Idle,
                Step::Idle,
            ])],
        );
        assert!(
            played.end.to_string().contains("script is over"),
            "{}",
            played.end
        );
        assert_eq!(read.last_seq(), log.len() as u64);
        assert_eq!(read.state(), ReplicaState::Following, "lag does not flip it back");
        // The replica is the node's: the same state hash as the log run through an engine.
        let mut direct = Replica::new();
        log.iter().for_each(|sc| direct.apply(sc).unwrap());
        assert_eq!(read.read(|r, _| r.state_hash()), direct.state_hash());
    }

    #[test]
    fn a_new_journal_resets_the_replica_and_it_goes_on_following() {
        let (log, other) = (happy_log(), other_log());
        let read = ReadSide::new();
        play(
            &read,
            vec![Ok(vec![
                Step::CaughtUp(true),
                Step::Deliver(log[..9].to_vec()),
                check(&read, |r| assert_eq!(r.last_seq(), 9)),
                // The log starts again at seq 1: a new journal (`docker compose down -v`).
                Step::Deliver(other[..2].to_vec()),
                check(&read, |r| {
                    assert_eq!(r.last_seq(), 2, "it starts over");
                    assert_eq!(
                        r.state(),
                        ReplicaState::Following,
                        "a new journal is not a failure"
                    );
                    r.read(|replica, _| {
                        assert_eq!(replica.engine().balance(ALICE).free, 50_000_000);
                        assert_eq!(
                            replica.engine().contracts().count(),
                            0,
                            "no row of the old journal"
                        );
                    });
                }),
                Step::Deliver(other[2..].to_vec()),
            ])],
        );
        assert_eq!(read.last_seq(), 4);
        assert_eq!(read.state(), ReplicaState::Following);
    }

    #[test]
    fn a_sequence_gap_is_unavailable_and_the_rebuild_takes_over_once_it_has_caught_up() {
        let log = happy_log();
        let read = ReadSide::new();
        let played = play(
            &read,
            vec![
                // The first follower skips seq 7: the replica keeps what it has.
                Ok(vec![
                    Step::CaughtUp(true),
                    Step::Deliver(log[..6].to_vec()),
                    check(&read, |r| assert_eq!(r.state(), ReplicaState::Following)),
                    Step::Deliver(vec![log[7].clone()]),
                ]),
                // The second replays from the start, on the side.
                Ok(vec![
                    check(&read, |r| {
                        assert_eq!(r.state(), ReplicaState::Unavailable);
                        assert_eq!(r.last_seq(), 6, "the old replica keeps serving");
                    }),
                    Step::Deliver(log[..4].to_vec()),
                    check(&read, |r| {
                        assert_eq!(r.last_seq(), 6, "not swapped in before it has caught up");
                        assert_eq!(r.state(), ReplicaState::Unavailable);
                    }),
                    Step::Deliver(log[4..6].to_vec()),
                    check(&read, |r| {
                        assert_eq!(r.last_seq(), 6);
                        assert_eq!(
                            r.state(),
                            ReplicaState::CatchingUp,
                            "swapped in, follower not yet caught up"
                        );
                    }),
                    Step::CaughtUp(true),
                    // From here the follower feeds the read side directly.
                    Step::Deliver(log[6..10].to_vec()),
                    check(&read, |r| {
                        assert_eq!(r.last_seq(), 10);
                        assert_eq!(r.state(), ReplicaState::Following);
                    }),
                    Step::Deliver(log[10..].to_vec()),
                ]),
            ],
        );
        assert!(
            played.end.to_string().contains("script is over"),
            "{}",
            played.end
        );
        assert_eq!(
            played.at_open,
            [ReplicaState::CatchingUp, ReplicaState::Unavailable]
        );
        assert_eq!(read.last_seq(), log.len() as u64);
        assert_eq!(read.state(), ReplicaState::Following);
    }

    #[test]
    fn a_follower_error_is_unavailable_and_a_failed_open_is_retried() {
        let log = happy_log();
        let read = ReadSide::new();
        play(
            &read,
            vec![
                Ok(vec![
                    Step::Deliver(log[..3].to_vec()),
                    Step::Fail("the Archive went away"),
                ]),
                Err("the Archive is not up yet"),
                Err("not yet"),
                Ok(vec![
                    check(&read, |r| {
                        assert_eq!(r.state(), ReplicaState::Unavailable);
                        assert_eq!(r.last_seq(), 3, "what it had is still served");
                    }),
                    Step::CaughtUp(true),
                    Step::Deliver(log[..8].to_vec()),
                ]),
            ],
        );
        assert_eq!(read.last_seq(), 8);
        assert_eq!(read.state(), ReplicaState::Following);
    }

    #[test]
    fn a_failed_open_at_start_is_unavailable_until_one_works() {
        let log = happy_log();
        let read = ReadSide::new();
        let played = play(
            &read,
            vec![
                Err("no Archive"),
                Ok(vec![
                    // Nothing was served yet, so nothing is rebuilt on the side: the follower is
                    // the replica's own, and it is catching up.
                    check(&read, |r| assert_eq!(r.state(), ReplicaState::CatchingUp)),
                    Step::Deliver(log[..2].to_vec()),
                    check(&read, |r| assert_eq!(r.last_seq(), 2, "fed directly")),
                    Step::CaughtUp(true),
                    Step::Deliver(log[2..4].to_vec()),
                ]),
            ],
        );
        assert_eq!(
            played.at_open,
            [ReplicaState::CatchingUp, ReplicaState::Unavailable]
        );
        assert_eq!((read.last_seq(), read.state()), (4, ReplicaState::Following));
    }

    #[test]
    fn a_failed_open_on_a_system_with_no_journal_yet_ends_following_with_nothing() {
        let read = ReadSide::new();
        play(
            &read,
            vec![
                Err("no Archive"),
                Ok(vec![Step::Idle, Step::CaughtUp(true), Step::Idle]),
            ],
        );
        assert_eq!((read.last_seq(), read.state()), (0, ReplicaState::Following));
    }

    #[test]
    fn a_rebuild_of_a_different_journal_takes_over_at_once() {
        let (log, other) = (happy_log(), other_log());
        let read = ReadSide::new();
        play(
            &read,
            vec![
                Ok(vec![Step::Deliver(log[..9].to_vec()), Step::Fail("gone")]),
                Ok(vec![
                    // Genesis is the same in both journals: not yet told apart.
                    Step::Deliver(other[..1].to_vec()),
                    check(&read, |r| {
                        assert_eq!(r.last_seq(), 9, "same Genesis: still the old one")
                    }),
                    // The first deposit differs: this is another journal, so serve it now, not at seq 9.
                    Step::Deliver(other[1..2].to_vec()),
                    check(&read, |r| {
                        assert_eq!(r.last_seq(), 2);
                        r.read(|replica, _| {
                            assert_eq!(replica.engine().balance(ALICE).free, 50_000_000);
                            assert_eq!(replica.engine().contracts().count(), 0);
                        });
                    }),
                    Step::Deliver(other[2..].to_vec()),
                ]),
            ],
        );
        assert_eq!(read.last_seq(), 4);
    }

    #[test]
    fn a_rebuild_that_is_fully_delivered_and_shorter_takes_over() {
        let log = happy_log();
        let read = ReadSide::new();
        play(
            &read,
            vec![
                Ok(vec![Step::Deliver(log[..9].to_vec()), Step::Fail("gone")]),
                Ok(vec![
                    // The journal is a prefix of what the old replica applied: it was cut.
                    Step::Deliver(log[..4].to_vec()),
                    check(&read, |r| assert_eq!(r.last_seq(), 9)),
                    Step::CaughtUp(true),
                    Step::Idle,
                    check(&read, |r| {
                        assert_eq!(r.last_seq(), 4, "everything the journal holds, and no more");
                        assert_eq!(r.state(), ReplicaState::Following);
                    }),
                ]),
            ],
        );
        assert_eq!(read.last_seq(), 4);
    }

    #[test]
    fn a_dead_client_ends_the_loop_with_its_error() {
        let read = ReadSide::new();
        let open = || {
            Ok(Fake {
                steps: VecDeque::new(),
                caught_up: Cell::new(false),
                last: false,
                over: Rc::new(Cell::new(false)),
            })
        };
        let err = run(&read, NOW, open, || bail!("the replica's Aeron client is closed")).unwrap_err();
        assert!(err.to_string().contains("client is closed"), "{err}");
    }

    #[test]
    fn the_replica_serves_while_it_rebuilds() {
        // What an HTTP read sees during a rebuild: the old rows, with `unavailable`.
        let log = happy_log();
        let read = ReadSide::new();
        play(
            &read,
            vec![
                Ok(vec![Step::Deliver(log[..9].to_vec()), Step::Fail("gone")]),
                Ok(vec![
                    Step::Deliver(log[..2].to_vec()),
                    check(&read, |r| {
                        let reply = rfq_gateway::read_api::get(
                            r,
                            &rfq_gateway::status::EngineStatus::new(),
                            "/v1/status",
                            "",
                            Duration::ZERO,
                        );
                        let body: Value = serde_json::from_str(&reply.body).unwrap();
                        assert_eq!(body["replica"]["state"], "unavailable");
                        assert_eq!(body["replica"]["last_seq"], 9);
                        assert_eq!(body["as_of_seq"], json!(9));
                    }),
                ]),
            ],
        );
    }
}
