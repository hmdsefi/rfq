//! The hub: the gateway's state machine (docs/specs/gateway.md §5, §6, §9).
//!
//! One [`Input`] in (a connection opened or closed, a client's text frame, an egress batch, a
//! change of the ingress connection, a clock tick), the [`Effect`]s to perform out, in order:
//! offer a command on ingress, or send a text frame to a connection. No I/O and no clock of its
//! own: the caller passes the time with each input.
//!
//! - **Commands.** Each actor has at most one command on ingress at a time; the next one waits
//!   until the first has its batch, so nonces never race (gateway.md §5). A command takes its
//!   explicit nonce, else the nonce book's `last + 1`, else it waits for a probe search. A
//!   command whose encoding is over the node's ingress limit is refused on arrival: the node
//!   would drop it unseen.
//! - **Batches.** Every egress batch goes to every connection through its view's filter; the
//!   sender then gets its `result`. A probe's batch is the gateway's own business: its origin and
//!   its rejection are never forwarded (what timers added to it still is).
//! - **Engine state** (gateway.md §6). Down when the ingress publication is not connected or a
//!   command has had no batch for 3 s; every command in flight or waiting then fails, and its
//!   actor's nonce is forgotten. Up again once the publication is connected and the engine has
//!   shown it works since the failure: a batch that came while connected, or a restarted node's
//!   `Resume`, which may be polled just before the connection is seen. Any other batch polled
//!   while disconnected is stale (published before the failure) and does not count. So after a
//!   restart a client sees `down`, then the `Resume` batch, then `up`. At start-up nothing has
//!   failed, so the first connection is enough.
//! - **Outcome unknown.** A command already offered when the engine goes down may still be
//!   applied. Its sender is told so, with the nonce to resend it with (a repeat is a harmless
//!   `Duplicate`), and still gets its `result` if the batch comes later. A command never offered
//!   is `engine unavailable`.
//! - **Liveness.** Down with ingress connected, nothing would ever publish (commands are
//!   refused), so about every second the hub offers a probe that the engine always rejects as
//!   `Duplicate` (nonce 0). Its batch is internal, and brings the engine back up.
//!
//! Logging (gateway.md §8): each command sent, each result, each engine status change, each
//! nonce sync.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rfq_app::client::nonce::probe_action;
use rfq_app::{BatchKind, ClientCommand, EventBatch};
use rfq_domain::{AccountId, Action, Audience, Emitted, Event, RejectReason, Timestamp};
use rfq_infra::codec::{MAX_INGRESS, encoded_len};

use crate::nonce_book::{NonceBook, SyncStep};
use crate::protocol::{ClientMsg, EngineState, ServerMsg, decode_client, encode_server};
use crate::view::{View, filter};

/// A command with no batch for longer than this (µs) means the engine is down (gateway.md §6).
pub const COMMAND_TIMEOUT_US: u64 = 3_000_000;
/// How many commands one actor may have waiting behind the one on ingress. More are pushed back.
pub const MAX_QUEUED: usize = 1_024;
/// Probes in flight when the engine went down, remembered so that a late answer is still known
/// for a probe and not forwarded. Past this many, a late answer is forwarded like any batch.
const MAX_ORPHAN_PROBES: usize = 1_024;
/// Commands whose outcome is unknown, remembered so that a late batch still gives the sender its
/// `result`. Past this many, a late batch is forwarded without one.
const MAX_UNKNOWN: usize = 1_024;
/// How often (µs) a liveness probe goes out while the engine is down and ingress is connected.
pub const LIVENESS_EVERY_US: u64 = 1_000_000;
/// The liveness probe's actor: the demo genesis operator. Any actor would do: nonce 0 is never
/// above an actor's last nonce, so the engine always answers `Duplicate` and consumes nothing.
pub const LIVENESS_ACTOR: AccountId = AccountId(1);

/// Never offered: the engine was down, or the command was still waiting its turn.
const UNAVAILABLE: &str = "engine unavailable";
const BACK_PRESSURED: &str = "ingress back-pressured, try again";

/// For a command already offered when the engine stopped answering: it may still be applied.
fn unknown_outcome(nonce: u64) -> String {
    format!(
        "outcome unknown: the engine stopped answering after this command was sent; resend it with \
         nonce {nonce} to retry safely (a repeat is rejected as Duplicate)"
    )
}

/// For a command whose encoding is over the node's ingress limit: the node would drop it unseen.
fn too_large_for_ingress(len: usize) -> String {
    format!("bad request: command too large for ingress ({len} bytes, limit {MAX_INGRESS})")
}

/// A WebSocket connection, numbered by the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// A connection opened. Its view is `admin` until its first `hello`.
    Open(ConnId),
    /// A connection closed. Its waiting commands are dropped, and so are the results of those
    /// already on ingress.
    Close(ConnId),
    /// A text frame from a connection.
    Text(ConnId, String),
    /// An egress batch (or one chunk of a split batch).
    Batch(EventBatch),
    /// The ingress publication's connection changed.
    Ingress { connected: bool },
    /// Time passed (the caller sends one every 100 ms or so).
    Tick,
    /// The `Effect::Offer` of `(actor, nonce)` could not be put on ingress, for `why`. The engine
    /// never saw it, so the nonce is still free.
    OfferFailed {
        actor: AccountId,
        nonce: u64,
        why: OfferFailure,
    },
}

/// Why an offer failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfferFailure {
    /// The encoded command (this many bytes) is over what the publication takes.
    TooLarge(usize),
    /// Ingress stayed back-pressured; the client may try again.
    BackPressure,
    /// Nobody subscribes to ingress: the engine is gone.
    Unconnected,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Put this command on ingress. If that fails, report it with [`Input::OfferFailed`].
    Offer(ClientCommand),
    /// Send this text frame to this connection.
    Send(ConnId, String),
}

/// A client's command waiting for its turn.
struct Waiting {
    conn: ConnId,
    id: String,
    nonce: Option<u64>,
    action: Action,
    /// When the gateway received it, on the caller's monotonic clock (µs): `latency_us` runs
    /// from here (gateway.md §3), including any wait behind the actor's previous command or a
    /// nonce sync.
    received_at: u64,
}

enum Flight {
    /// A client's command. `conn` is `None` once the sender has gone: its result is dropped.
    Command {
        conn: Option<ConnId>,
        id: String,
        received_at: u64,
    },
    /// A nonce probe (`Withdraw { amount: 0 }`), never reported to clients.
    Probe,
}

/// The one command of an actor that is on ingress, waiting for its batch.
struct InFlight {
    nonce: u64,
    /// When it was offered, on the caller's monotonic clock (µs). The 3 s timeout runs from here.
    sent_at: u64,
    what: Flight,
}

/// A command that was offered when the engine stopped answering. Its batch may still come.
struct Unknown {
    conn: ConnId,
    id: String,
    received_at: u64,
}

/// Liveness probes (see [`LIVENESS_EVERY_US`]).
#[derive(Default)]
struct Liveness {
    /// Offered and not yet answered (or refused).
    outstanding: u32,
    last_offered_at: Option<u64>,
}

#[derive(Default)]
struct Actor {
    in_flight: Option<InFlight>,
    /// FIFO. While a probe search runs, its front is the command that started it.
    waiting: VecDeque<Waiting>,
}

pub struct Hub {
    views: BTreeMap<ConnId, View>,
    /// Actors with a command in flight or waiting; no entry when idle.
    actors: BTreeMap<AccountId, Actor>,
    book: NonceBook,
    engine: EngineState,
    /// Whether the ingress publication is connected (a node subscribes to it).
    connected: bool,
    /// Nothing has failed since the engine was last seen working: at start-up, and after a
    /// batch that came while connected, or a `Resume`. Up needs it, besides the connection.
    fresh: bool,
    last_seq: Option<u64>,
    /// The last batch, and whose it was. A batch too big for one message arrives as chunks with
    /// the same seq, time, kind and origin (`rfq_infra::transport::split_batch`): the first chunk
    /// tells whose it is, and the later ones answer nothing a second time.
    current: Option<(Chunk, Whose)>,
    /// `(actor, nonce)` of probes that were in flight when the engine went down, until a later
    /// command's batch shows that no answer to them can still come.
    orphan_probes: BTreeSet<(AccountId, u64)>,
    /// Commands in flight when the engine went down, by `(actor, nonce)`, until their batch
    /// comes, a later command's batch shows it never will, or the client resends the nonce.
    unknown: BTreeMap<(AccountId, u64), Unknown>,
    liveness: Liveness,
}

/// What the chunks of one batch have in common.
type Chunk = (u64, Timestamp, BatchKind, Option<(AccountId, u64)>);

fn chunk(b: &EventBatch) -> Chunk {
    (b.seq, b.time, b.kind, b.origin)
}

/// Whose command an egress batch answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Whose {
    /// A client's command in flight; its first chunk answers it.
    Command,
    /// A probe in flight; `answered` once the chunk with its rejection (the last event) came.
    Probe { answered: bool },
    /// A probe that was in flight when the engine went down: it stays internal too.
    OrphanProbe,
    /// A liveness probe: internal, and never news for the nonce book.
    Liveness,
    /// A command that was in flight when the engine went down: its sender gets the result now.
    Unknown,
    /// Anyone else's command (another client of the node), or a system command.
    Other,
}

impl Default for Hub {
    fn default() -> Self {
        Hub::new()
    }
}

impl Hub {
    pub fn new() -> Hub {
        Hub {
            views: BTreeMap::new(),
            actors: BTreeMap::new(),
            book: NonceBook::new(),
            engine: EngineState::Down,
            connected: false,
            fresh: true,
            last_seq: None,
            current: None,
            orphan_probes: BTreeSet::new(),
            unknown: BTreeMap::new(),
            liveness: Liveness::default(),
        }
    }

    /// One input in, the effects to perform, in order. `now_us` is the gateway's monotonic clock
    /// (latency and timeouts); `wall_ms` is its wall clock (`welcome.now`).
    pub fn handle(&mut self, input: Input, now_us: u64, wall_ms: u64) -> Vec<Effect> {
        let mut out = Vec::new();
        match input {
            Input::Open(conn) => {
                self.views.insert(conn, View::Admin);
            }
            Input::Close(conn) => self.close(conn),
            Input::Text(conn, text) => self.text(conn, &text, now_us, wall_ms, &mut out),
            Input::Batch(b) => self.batch(&b, now_us, &mut out),
            Input::Ingress { connected } => self.ingress(connected, &mut out),
            Input::Tick => self.tick(now_us, &mut out),
            Input::OfferFailed { actor, nonce, why } => {
                self.offer_failed(actor, nonce, why, now_us, &mut out)
            }
        }
        out
    }

    /// The view of an open connection.
    pub fn view(&self, conn: ConnId) -> Option<View> {
        self.views.get(&conn).copied()
    }

    fn close(&mut self, conn: ConnId) {
        self.views.remove(&conn);
        self.unknown.retain(|_, u| u.conn != conn);
        for a in self.actors.values_mut() {
            a.waiting.retain(|w| w.conn != conn);
            if let Some(InFlight {
                what: Flight::Command { conn: sender, .. },
                ..
            }) = &mut a.in_flight
                && *sender == Some(conn)
            {
                *sender = None;
            }
        }
    }

    fn text(&mut self, conn: ConnId, text: &str, now_us: u64, wall_ms: u64, out: &mut Vec<Effect>) {
        let Some(view) = self.views.get_mut(&conn) else {
            tracing::debug!(conn = conn.0, "text from a connection that is not open");
            return;
        };
        match decode_client(text) {
            Err(error) => send(out, conn, &ServerMsg::Error { id: None, error }),
            Ok(ClientMsg::Hello { view: new }) => {
                *view = new;
                let welcome = ServerMsg::Welcome {
                    view: new,
                    engine: self.engine,
                    now: wall_ms,
                };
                send(out, conn, &welcome);
                send(out, conn, &self.status());
            }
            Ok(ClientMsg::Ping) => send(out, conn, &ServerMsg::Pong),
            Ok(ClientMsg::Command {
                id,
                actor,
                nonce,
                action,
            }) => {
                // The node drops an ingress message over MAX_INGRESS unseen, and a JSON frame
                // within 64 KiB can encode larger (a struct may be written as an array). The size
                // does not depend on the nonce (fixed-width integers), so check it now.
                let cmd = ClientCommand {
                    actor,
                    nonce: nonce.unwrap_or(0),
                    action,
                };
                let len = encoded_len(&cmd);
                if len > MAX_INGRESS {
                    send(out, conn, &error(Some(id), &too_large_for_ingress(len)));
                    return;
                }
                let waiting = Waiting {
                    conn,
                    id,
                    nonce,
                    action: cmd.action,
                    received_at: now_us,
                };
                self.command(actor, waiting, now_us, out);
            }
        }
    }

    fn command(&mut self, actor: AccountId, w: Waiting, now_us: u64, out: &mut Vec<Effect>) {
        if self.engine == EngineState::Down {
            send(out, w.conn, &error(Some(w.id), UNAVAILABLE));
            return;
        }
        let a = self.actors.entry(actor).or_default();
        if a.waiting.len() >= MAX_QUEUED {
            send(out, w.conn, &error(Some(w.id), BACK_PRESSURED));
            return;
        }
        a.waiting.push_back(w);
        self.advance(actor, now_us, out);
    }

    /// Unless `actor` has something on ingress, puts its next waiting command there: with its
    /// explicit nonce, or the book's next one, or, for an actor the book doesn't know, the first
    /// probe of a nonce search.
    fn advance(&mut self, actor: AccountId, now_us: u64, out: &mut Vec<Effect>) {
        let Some(a) = self.actors.get_mut(&actor) else {
            return;
        };
        if a.in_flight.is_some() {
            return;
        }
        let Some(front) = a.waiting.front() else {
            self.actors.remove(&actor);
            return;
        };
        let Some(nonce) = front.nonce.or_else(|| self.book.next(actor)) else {
            let nonce = self.book.start_sync(actor);
            tracing::debug!(actor = actor.0, nonce, "nonce probe");
            out.push(Effect::Offer(ClientCommand {
                actor,
                nonce,
                action: probe_action(),
            }));
            a.in_flight = Some(InFlight {
                nonce,
                sent_at: now_us,
                what: Flight::Probe,
            });
            return;
        };
        let Some(Waiting {
            conn,
            id,
            action,
            received_at,
            ..
        }) = a.waiting.pop_front()
        else {
            return;
        };
        tracing::info!(
            conn = conn.0,
            actor = actor.0,
            nonce,
            action = action_name(&action),
            "command"
        );
        // A resend of an unknown command's nonce takes it over: whichever batch comes first for
        // `(actor, nonce)` answers the resend.
        self.unknown.remove(&(actor, nonce));
        out.push(Effect::Offer(ClientCommand { actor, nonce, action }));
        send(
            out,
            conn,
            &ServerMsg::Sent {
                id: id.clone(),
                actor,
                nonce,
            },
        );
        a.in_flight = Some(InFlight {
            nonce,
            sent_at: now_us,
            what: Flight::Command {
                conn: Some(conn),
                id,
                received_at,
            },
        });
    }

    fn batch(&mut self, b: &EventBatch, now_us: u64, out: &mut Vec<Effect>) {
        let (first_chunk, whose) = match self.current {
            Some((last, whose)) if last == chunk(b) => (false, whose),
            _ => (true, self.whose(b)),
        };
        if let Some(origin) = b.origin
            && whose != Whose::Liveness
        {
            self.book.observe(b.seq, origin, &b.events);
        }
        match whose {
            Whose::Probe { .. } | Whose::OrphanProbe | Whose::Liveness => {
                if let Some(rest) = without_probe(b) {
                    self.forward(&rest, out);
                }
            }
            Whose::Command | Whose::Unknown | Whose::Other => self.forward(b, out),
        }
        let whose = match (whose, b.origin) {
            (Whose::Command, Some((actor, _))) if first_chunk => {
                self.command_answered(actor, b.seq, now_us, out);
                whose
            }
            (Whose::Unknown, Some(origin)) if first_chunk => {
                if let Some(u) = self.unknown.remove(&origin) {
                    let latency_us = now_us.saturating_sub(u.received_at);
                    tracing::info!(seq = b.seq, latency_us, "result (outcome was unknown)");
                    self.reply(
                        out,
                        u.conn,
                        &ServerMsg::Result {
                            id: u.id,
                            seq: b.seq,
                            latency_us,
                        },
                    );
                }
                whose
            }
            // The probe's answer is its rejection, the last event: a leading chunk has none.
            (Whose::Probe { answered: false }, Some((actor, _))) => match rejection_to(actor, &b.events) {
                Some(reason) => {
                    self.probe_answered(actor, b.seq, reason, now_us, out);
                    Whose::Probe { answered: true }
                }
                None => whose,
            },
            _ => whose,
        };
        self.current = Some((chunk(b), whose));

        self.last_seq = Some(self.last_seq.map_or(b.seq, |last| last.max(b.seq)));
        // A batch shows the engine working only if it came while ingress is connected, or if it
        // is a node's `Resume`: its first output after a start, which the gateway may poll just
        // before it sees the connection. Any other batch polled while disconnected was published
        // before the failure, so it is stale and no sign of recovery (it is still forwarded).
        if self.connected || b.kind == BatchKind::Resume {
            self.fresh = true;
            if self.connected && self.engine == EngineState::Down {
                self.set_engine(EngineState::Up, "a batch arrived", out);
            }
        }
    }

    /// Whose command the first chunk of `b` answers. The gateway's own command in flight comes
    /// first, so a stale orphan can never swallow it.
    fn whose(&mut self, b: &EventBatch) -> Whose {
        let Some((actor, nonce)) = b.origin else {
            return Whose::Other;
        };
        let flight = self
            .actors
            .get(&actor)
            .and_then(|a| a.in_flight.as_ref())
            .filter(|f| f.nonce == nonce);
        match flight.map(|f| &f.what) {
            Some(what) => {
                // Ingress is one publication, in order: once a command sent after the engine
                // went down has its batch, any answer to an older command has come (or never
                // will).
                self.orphan_probes.clear();
                self.unknown.clear();
                match what {
                    Flight::Command { .. } => Whose::Command,
                    Flight::Probe => Whose::Probe { answered: false },
                }
            }
            None if self.orphan_probes.remove(&(actor, nonce)) => Whose::OrphanProbe,
            None if (actor, nonce) == (LIVENESS_ACTOR, 0) && self.liveness.outstanding > 0 => {
                self.liveness.outstanding -= 1;
                Whose::Liveness
            }
            None if self.unknown.contains_key(&(actor, nonce)) => Whose::Unknown,
            None => Whose::Other,
        }
    }

    /// Sends `b` to every connection, filtered for its view; once per view, not per connection.
    fn forward(&self, b: &EventBatch, out: &mut Vec<Effect>) {
        let mut frames: BTreeMap<View, Option<String>> = BTreeMap::new();
        for (&conn, &view) in &self.views {
            let frame = frames
                .entry(view)
                .or_insert_with(|| filter(view, b).map(|ob| encode_server(&ServerMsg::Batch(ob))));
            if let Some(frame) = frame {
                out.push(Effect::Send(conn, frame.clone()));
            }
        }
    }

    /// The batch of `actor`'s command in flight came: its result to the sender, then the next.
    fn command_answered(&mut self, actor: AccountId, seq: u64, now_us: u64, out: &mut Vec<Effect>) {
        let Some(a) = self.actors.get_mut(&actor) else {
            return;
        };
        let Some(InFlight {
            what:
                Flight::Command {
                    conn,
                    id,
                    received_at,
                    ..
                },
            ..
        }) = a.in_flight.take()
        else {
            return;
        };
        let latency_us = now_us.saturating_sub(received_at);
        tracing::info!(seq, latency_us, "result");
        if let Some(conn) = conn {
            self.reply(out, conn, &ServerMsg::Result { id, seq, latency_us });
        }
        self.advance(actor, now_us, out);
    }

    fn probe_answered(
        &mut self,
        actor: AccountId,
        seq: u64,
        reason: RejectReason,
        now_us: u64,
        out: &mut Vec<Effect>,
    ) {
        if let Some(a) = self.actors.get_mut(&actor) {
            a.in_flight = None;
        }
        match self.book.answer(actor, seq, Some(reason)) {
            Some(SyncStep::Probe(nonce)) => {
                tracing::debug!(actor = actor.0, nonce, "nonce probe");
                out.push(Effect::Offer(ClientCommand {
                    actor,
                    nonce,
                    action: probe_action(),
                }));
                if let Some(a) = self.actors.get_mut(&actor) {
                    a.in_flight = Some(InFlight {
                        nonce,
                        sent_at: now_us,
                        what: Flight::Probe,
                    });
                }
            }
            Some(SyncStep::Synced { last, probes }) => {
                tracing::info!(actor = actor.0, last, probes, "nonce sync");
                self.advance(actor, now_us, out);
            }
            Some(SyncStep::Failed(e)) => {
                tracing::warn!(actor = actor.0, error = %e, "nonce sync failed");
                self.fail_waiting(actor, &format!("nonce sync failed for account {}", actor.0), out);
            }
            // Not syncing after all: start over with whatever the book knows.
            None => self.advance(actor, now_us, out),
        }
    }

    /// Fails every command waiting for `actor` with `why`.
    fn fail_waiting(&mut self, actor: AccountId, why: &str, out: &mut Vec<Effect>) {
        let Some(a) = self.actors.get_mut(&actor) else {
            return;
        };
        let waiting = std::mem::take(&mut a.waiting);
        if a.in_flight.is_none() {
            self.actors.remove(&actor);
        }
        for w in waiting {
            self.reply(out, w.conn, &error(Some(w.id), why));
        }
    }

    fn ingress(&mut self, connected: bool, out: &mut Vec<Effect>) {
        self.connected = connected;
        match (connected, self.engine) {
            (false, EngineState::Up) => self.go_down("the ingress publication is not connected", out),
            (true, EngineState::Down) if self.fresh => {
                self.set_engine(EngineState::Up, "ingress connected", out)
            }
            _ => {}
        }
    }

    fn tick(&mut self, now_us: u64, out: &mut Vec<Effect>) {
        if self.engine == EngineState::Up {
            let late = self
                .actors
                .values()
                .filter_map(|a| a.in_flight.as_ref())
                .any(|f| now_us.saturating_sub(f.sent_at) > COMMAND_TIMEOUT_US);
            if late {
                self.go_down("no batch within 3 s of a command", out);
            }
        }
        // Down with a node connected (a hung engine, or one whose `Resume` was missed): commands
        // are refused, so nothing would ever publish. A liveness probe finds out when it works.
        let due = self
            .liveness
            .last_offered_at
            .is_none_or(|at| now_us.saturating_sub(at) >= LIVENESS_EVERY_US);
        if self.engine == EngineState::Down && self.connected && due {
            tracing::debug!("liveness probe");
            self.liveness.last_offered_at = Some(now_us);
            self.liveness.outstanding = self.liveness.outstanding.saturating_add(1);
            out.push(Effect::Offer(ClientCommand {
                actor: LIVENESS_ACTOR,
                nonce: 0,
                action: probe_action(),
            }));
        }
    }

    fn offer_failed(
        &mut self,
        actor: AccountId,
        nonce: u64,
        why: OfferFailure,
        now_us: u64,
        out: &mut Vec<Effect>,
    ) {
        let flight = self
            .actors
            .get_mut(&actor)
            .and_then(|a| a.in_flight.take_if(|f| f.nonce == nonce));
        match flight.map(|f| f.what) {
            Some(Flight::Command { conn, id, .. }) => {
                tracing::warn!(actor = actor.0, nonce, ?why, "offer to ingress failed");
                let sentence = match why {
                    OfferFailure::TooLarge(len) => too_large_for_ingress(len),
                    OfferFailure::BackPressure => BACK_PRESSURED.to_string(),
                    OfferFailure::Unconnected => UNAVAILABLE.to_string(),
                };
                if let Some(conn) = conn {
                    self.reply(out, conn, &error(Some(id), &sentence));
                }
                if why == OfferFailure::Unconnected {
                    // Never offer the next one into a dead ingress: it was never offered either.
                    self.fail_waiting(actor, UNAVAILABLE, out);
                } else {
                    self.advance(actor, now_us, out);
                }
            }
            Some(Flight::Probe) => {
                tracing::warn!(
                    actor = actor.0,
                    nonce,
                    ?why,
                    "offer to ingress failed during a nonce sync"
                );
                self.book.forget(actor);
                let sentence = match why {
                    OfferFailure::TooLarge(_) => format!("nonce sync failed for account {}", actor.0),
                    OfferFailure::BackPressure => BACK_PRESSURED.to_string(),
                    OfferFailure::Unconnected => UNAVAILABLE.to_string(),
                };
                self.fail_waiting(actor, &sentence, out);
            }
            None if (actor, nonce) == (LIVENESS_ACTOR, 0) => {
                self.liveness.outstanding = self.liveness.outstanding.saturating_sub(1);
            }
            None => {}
        }
        // Whatever it was, an unconnected ingress means the engine is gone.
        if why == OfferFailure::Unconnected {
            self.ingress(false, out);
        }
    }

    /// The engine is down: every command in flight or waiting fails, and the actors with one in
    /// flight are forgotten by the nonce book, since that nonce may or may not have been consumed.
    /// A command already offered may still be applied: its sender learns the outcome is unknown
    /// and how to retry safely, and still gets the `result` if its batch comes. A command never
    /// offered is `engine unavailable`.
    fn go_down(&mut self, why: &str, out: &mut Vec<Effect>) {
        self.fresh = false;
        for (actor, a) in std::mem::take(&mut self.actors) {
            if let Some(f) = a.in_flight {
                self.book.forget(actor);
                match f.what {
                    Flight::Command {
                        conn: Some(conn),
                        id,
                        received_at,
                    } => {
                        self.reply(out, conn, &error(Some(id.clone()), &unknown_outcome(f.nonce)));
                        if self.unknown.len() < MAX_UNKNOWN {
                            self.unknown.insert(
                                (actor, f.nonce),
                                Unknown {
                                    conn,
                                    id,
                                    received_at,
                                },
                            );
                        }
                    }
                    Flight::Command { conn: None, .. } => {}
                    Flight::Probe => {
                        if self.orphan_probes.len() < MAX_ORPHAN_PROBES {
                            self.orphan_probes.insert((actor, f.nonce));
                        }
                    }
                }
            }
            for w in a.waiting {
                self.reply(out, w.conn, &error(Some(w.id), UNAVAILABLE));
            }
        }
        self.set_engine(EngineState::Down, why, out);
    }

    fn set_engine(&mut self, state: EngineState, why: &str, out: &mut Vec<Effect>) {
        self.engine = state;
        let status = match state {
            EngineState::Up => "up",
            EngineState::Down => "down",
        };
        tracing::info!(status, why, "engine");
        let frame = encode_server(&self.status());
        for &conn in self.views.keys() {
            out.push(Effect::Send(conn, frame.clone()));
        }
    }

    fn status(&self) -> ServerMsg {
        ServerMsg::Status {
            engine: self.engine,
            last_seq: self.last_seq,
        }
    }

    /// Sends `msg` to `conn` if it is still open.
    fn reply(&self, out: &mut Vec<Effect>, conn: ConnId, msg: &ServerMsg) {
        if self.views.contains_key(&conn) {
            send(out, conn, msg);
        }
    }
}

fn send(out: &mut Vec<Effect>, conn: ConnId, msg: &ServerMsg) {
    out.push(Effect::Send(conn, encode_server(msg)));
}

fn error(id: Option<String>, why: &str) -> ServerMsg {
    ServerMsg::Error {
        id,
        error: why.to_string(),
    }
}

/// The rejection of `actor`'s command in `events`, if any (timers never reject).
fn rejection_to(actor: AccountId, events: &[Emitted]) -> Option<RejectReason> {
    events.iter().find_map(|e| match e.event {
        Event::CommandRejected { reason } if e.audience == Audience::Account(actor) => Some(reason),
        _ => None,
    })
}

/// A probe's batch without the probe: no origin, no rejection. What is left is what timers did
/// in the same apply, which their viewers must still get; `None` if nothing is left.
fn without_probe(b: &EventBatch) -> Option<EventBatch> {
    let prober = b.origin.map(|(actor, _)| Audience::Account(actor));
    let events: Vec<Emitted> = b
        .events
        .iter()
        .filter(|e| !(matches!(e.event, Event::CommandRejected { .. }) && Some(e.audience) == prober))
        .cloned()
        .collect();
    (!events.is_empty() || !b.balances.is_empty()).then(|| EventBatch {
        seq: b.seq,
        time: b.time,
        kind: b.kind,
        origin: None,
        events,
        balances: b.balances.clone(),
    })
}

fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Deposit { .. } => "Deposit",
        Action::Withdraw { .. } => "Withdraw",
        Action::ListContract { .. } => "ListContract",
        Action::SubmitRequest { .. } => "SubmitRequest",
        Action::CancelRequest { .. } => "CancelRequest",
        Action::SubmitQuote { .. } => "SubmitQuote",
        Action::CancelQuote { .. } => "CancelQuote",
        Action::Accept { .. } => "Accept",
        Action::Reject { .. } => "Reject",
        Action::ProposeOutcome { .. } => "ProposeOutcome",
        Action::Dispute { .. } => "Dispute",
        Action::Rule { .. } => "Rule",
    }
}

#[cfg(test)]
mod tests {
    use rfq_app::BatchKind;
    use rfq_domain::{Balance, Event, QuoteId, RejectReason};
    use serde_json::{Value, json};

    use super::*;
    use crate::nonce_book::NonceState;
    use crate::protocol::MAX_TEXT;

    const ALICE: AccountId = AccountId(10);
    const MM1: AccountId = AccountId(20);
    const OPERATOR: AccountId = AccountId(1);
    /// The gateway's wall clock, in ms.
    const WALL: u64 = 1_700_000_000_000;

    /// A hub whose engine is up: the ingress publication connected at start-up.
    fn up() -> Hub {
        let mut hub = Hub::new();
        hub.handle(Input::Ingress { connected: true }, 0, WALL);
        hub
    }

    /// Opens connection `c`, with `view` if given (else the default, admin).
    fn open(hub: &mut Hub, c: u64, view: Option<Value>) {
        hub.handle(Input::Open(ConnId(c)), 0, WALL);
        if let Some(view) = view {
            text(hub, c, json!({"type": "hello", "view": view}), 0);
        }
    }

    fn text(hub: &mut Hub, c: u64, msg: Value, now_us: u64) -> Vec<Effect> {
        hub.handle(Input::Text(ConnId(c), msg.to_string()), now_us, WALL)
    }

    fn command(hub: &mut Hub, c: u64, id: &str, actor: AccountId, now_us: u64) -> Vec<Effect> {
        text(
            hub,
            c,
            json!({"type": "command", "id": id, "actor": actor.0, "action": {"Withdraw": {"amount": 1}}}),
            now_us,
        )
    }

    fn batch(hub: &mut Hub, b: EventBatch, now_us: u64) -> Vec<Effect> {
        hub.handle(Input::Batch(b), now_us, WALL)
    }

    fn user_batch(seq: u64, origin: (AccountId, u64), events: Vec<Emitted>) -> EventBatch {
        EventBatch {
            seq,
            time: seq.saturating_add(5_000),
            kind: BatchKind::User,
            origin: Some(origin),
            events,
            balances: Vec::new(),
        }
    }

    fn failed(hub: &mut Hub, actor: AccountId, nonce: u64, why: OfferFailure) -> Vec<Effect> {
        hub.handle(Input::OfferFailed { actor, nonce, why }, 0, WALL)
    }

    fn unknown_outcome(nonce: u64) -> String {
        format!(
            "outcome unknown: the engine stopped answering after this command was sent; resend \
             it with nonce {nonce} to retry safely (a repeat is rejected as Duplicate)"
        )
    }

    /// The liveness probe: always `Duplicate`, so it consumes nothing.
    fn liveness() -> ClientCommand {
        probe(OPERATOR, 0)
    }

    fn resume(seq: u64) -> EventBatch {
        EventBatch {
            seq,
            time: seq.saturating_add(5_000),
            kind: BatchKind::Resume,
            origin: None,
            events: Vec::new(),
            balances: Vec::new(),
        }
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

    fn rejected(actor: AccountId, reason: RejectReason) -> Emitted {
        Emitted::to(actor, Event::CommandRejected { reason })
    }

    fn bal(free: u64) -> Balance {
        Balance {
            free,
            reserved: 0,
            escrowed: 0,
        }
    }

    /// Makes `actor` known to the hub with `last` (its command seen on egress).
    fn known(hub: &mut Hub, seq: u64, actor: AccountId, last: u64) {
        batch(hub, user_batch(seq, (actor, last), vec![withdrawn(actor)]), 0);
        assert_eq!(hub.book.state(actor), NonceState::Known { last });
    }

    /// The frames sent to connection `c`, as JSON.
    fn frames(effects: &[Effect], c: u64) -> Vec<Value> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Send(to, text) if *to == ConnId(c) => Some(serde_json::from_str(text).unwrap()),
                _ => None,
            })
            .collect()
    }

    fn types(frames: &[Value]) -> Vec<&str> {
        frames.iter().map(|f| f["type"].as_str().unwrap()).collect()
    }

    fn offers(effects: &[Effect]) -> Vec<ClientCommand> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Offer(c) => Some(c.clone()),
                Effect::Send(..) => None,
            })
            .collect()
    }

    fn sends(effects: &[Effect]) -> usize {
        effects.iter().filter(|e| matches!(e, Effect::Send(..))).count()
    }

    fn withdraw(actor: AccountId, nonce: u64) -> ClientCommand {
        ClientCommand {
            actor,
            nonce,
            action: Action::Withdraw { amount: 1 },
        }
    }

    fn probe(actor: AccountId, nonce: u64) -> ClientCommand {
        ClientCommand {
            actor,
            nonce,
            action: Action::Withdraw { amount: 0 },
        }
    }

    #[test]
    fn hub_sends_a_command_with_the_next_nonce_and_reports_its_result_with_latency() {
        let mut hub = up();
        open(&mut hub, 1, None);
        known(&mut hub, 20, ALICE, 4);

        let fx = command(&mut hub, 1, "c1", ALICE, 10_000);
        assert_eq!(offers(&fx), vec![withdraw(ALICE, 5)]);
        assert!(
            matches!(fx.first(), Some(Effect::Offer(_))),
            "on ingress, then reported"
        );
        assert_eq!(
            frames(&fx, 1),
            vec![json!({"type": "sent", "id": "c1", "actor": 10, "nonce": 5})]
        );

        let mut b = user_batch(21, (ALICE, 5), vec![withdrawn(ALICE)]);
        b.balances = vec![(ALICE, bal(7))];
        let fx = batch(&mut hub, b, 10_840);
        let got = frames(&fx, 1);
        assert_eq!(types(&got), ["batch", "result"], "the batch, then its result");
        assert_eq!(got[0]["seq"], 21);
        assert_eq!(got[0]["origin"], json!({"actor": 10, "nonce": 5}));
        assert_eq!(
            got[0]["balances"],
            json!([{"account": 10, "balance": {"free": 7, "reserved": 0, "escrowed": 0}}])
        );
        assert_eq!(
            got[1],
            json!({"type": "result", "id": "c1", "seq": 21, "latency_us": 840})
        );
        assert!(offers(&fx).is_empty());
        assert_eq!(hub.book.next(ALICE), Some(6));
    }

    #[test]
    fn hub_queues_a_second_command_for_the_same_actor_until_the_first_has_its_batch() {
        let mut hub = up();
        open(&mut hub, 1, None);
        known(&mut hub, 20, ALICE, 4);
        known(&mut hub, 21, MM1, 9);

        assert_eq!(
            offers(&command(&mut hub, 1, "c1", ALICE, 100)),
            vec![withdraw(ALICE, 5)]
        );
        let fx = command(&mut hub, 1, "c2", ALICE, 200);
        assert!(fx.is_empty(), "c2 waits for c1's batch: {fx:?}");
        // Another actor is not held up.
        assert_eq!(
            offers(&command(&mut hub, 1, "m1", MM1, 300)),
            vec![withdraw(MM1, 10)]
        );

        let fx = batch(
            &mut hub,
            user_batch(22, (ALICE, 5), vec![withdrawn(ALICE)]),
            1_000,
        );
        assert_eq!(
            offers(&fx),
            vec![withdraw(ALICE, 6)],
            "c2 goes once c1 has its batch"
        );
        let got = frames(&fx, 1);
        assert_eq!(types(&got), ["batch", "result", "sent"]);
        assert_eq!(got[1]["id"], "c1");
        assert_eq!(
            got[2],
            json!({"type": "sent", "id": "c2", "actor": 10, "nonce": 6})
        );

        let fx = batch(
            &mut hub,
            user_batch(23, (ALICE, 6), vec![withdrawn(ALICE)]),
            1_500,
        );
        assert_eq!(
            frames(&fx, 1)[1],
            json!({"type": "result", "id": "c2", "seq": 23, "latency_us": 1_300}),
            "latency from receiving c2 (at 200), which waited behind c1"
        );
    }

    /// `latency_us` runs from the gateway receiving the command (gateway.md §3), including the
    /// time it waits behind a nonce sync or behind the actor's previous command.
    #[test]
    fn latency_runs_from_receiving_the_command() {
        let mut hub = up();
        open(&mut hub, 1, None);
        // ALICE is unknown: c1 waits for a probe, c2 waits behind c1.
        assert_eq!(
            offers(&command(&mut hub, 1, "c1", ALICE, 1_000)),
            vec![probe(ALICE, 1)]
        );
        assert!(offers(&command(&mut hub, 1, "c2", ALICE, 1_100)).is_empty());
        let fx = batch(
            &mut hub,
            user_batch(40, (ALICE, 1), vec![rejected(ALICE, RejectReason::InvalidInput)]),
            1_400,
        );
        assert_eq!(offers(&fx), vec![withdraw(ALICE, 2)], "synced: c1 goes at 1 400");

        let fx = batch(
            &mut hub,
            user_batch(41, (ALICE, 2), vec![withdrawn(ALICE)]),
            2_000,
        );
        assert_eq!(
            frames(&fx, 1)[1],
            json!({"type": "result", "id": "c1", "seq": 41, "latency_us": 1_000}),
            "from 1 000 (received), not 1 400 (offered)"
        );
        assert_eq!(offers(&fx), vec![withdraw(ALICE, 3)], "c2 goes at 2 000");
        let fx = batch(
            &mut hub,
            user_batch(42, (ALICE, 3), vec![withdrawn(ALICE)]),
            2_500,
        );
        assert_eq!(
            frames(&fx, 1)[1],
            json!({"type": "result", "id": "c2", "seq": 42, "latency_us": 1_400}),
            "from 1 100 (received), not 2 000 (offered)"
        );
    }

    #[test]
    fn hub_filters_batches_per_connection_view() {
        let mut hub = up();
        open(&mut hub, 1, Some(json!("admin")));
        open(&mut hub, 2, Some(json!({"account": 10})));
        open(&mut hub, 3, Some(json!({"account": 20})));
        open(&mut hub, 4, None);
        let b = EventBatch {
            seq: 30,
            time: 9_000,
            kind: BatchKind::User,
            origin: Some((ALICE, 2)),
            events: vec![
                Emitted::to(MM1, Event::QuoteExpired { quote: QuoteId(3) }),
                withdrawn(ALICE),
            ],
            balances: vec![(ALICE, bal(1)), (MM1, bal(2))],
        };
        let fx = batch(&mut hub, b.clone(), 0);
        let expect = |view| {
            let out = crate::view::filter(view, &b).map(ServerMsg::Batch);
            out.map(|m| serde_json::from_str::<Value>(&encode_server(&m)).unwrap())
                .into_iter()
                .collect::<Vec<_>>()
        };
        assert_eq!(frames(&fx, 1), expect(View::Admin));
        assert_eq!(frames(&fx, 4), expect(View::Admin), "admin until the first hello");
        assert_eq!(frames(&fx, 2), expect(View::Account(ALICE)));
        assert_eq!(frames(&fx, 3), expect(View::Account(MM1)));
        assert_eq!(
            frames(&fx, 3)[0]["origin"],
            Value::Null,
            "MM1 doesn't learn who sent it"
        );
        assert_eq!(frames(&fx, 3)[0]["balances"].as_array().unwrap().len(), 1);

        // A batch with nothing for MM1 doesn't reach it at all.
        let fx = batch(&mut hub, user_batch(31, (ALICE, 3), vec![withdrawn(ALICE)]), 0);
        assert!(frames(&fx, 3).is_empty());
        assert_eq!(types(&frames(&fx, 2)), ["batch"]);
        // Switching views takes effect for the next batch.
        text(&mut hub, 3, json!({"type": "hello", "view": {"account": 10}}), 0);
        let fx = batch(&mut hub, user_batch(32, (ALICE, 4), vec![withdrawn(ALICE)]), 0);
        assert_eq!(types(&frames(&fx, 3)), ["batch"]);
    }

    /// Simulates the engine's answer to a probe: the strict nonce rule with `last`.
    fn answer(seq: u64, offer: &ClientCommand, last: &mut u64) -> EventBatch {
        assert_eq!(offer.action, Action::Withdraw { amount: 0 }, "a probe");
        let reason = if offer.nonce <= *last {
            RejectReason::Duplicate
        } else if offer.nonce == *last + 1 {
            *last = offer.nonce;
            RejectReason::InvalidInput
        } else {
            RejectReason::NonceGap
        };
        user_batch(
            seq,
            (offer.actor, offer.nonce),
            vec![rejected(offer.actor, reason)],
        )
    }

    #[test]
    fn hub_never_forwards_a_probe_batch() {
        let mut hub = up();
        open(&mut hub, 1, None);
        open(&mut hub, 2, Some(json!({"account": 10})));
        let mut engine_last = 6;

        let fx = command(&mut hub, 2, "c1", ALICE, 0);
        assert_eq!(offers(&fx), vec![probe(ALICE, 1)], "unknown: probe first");
        assert_eq!(sends(&fx), 0, "probes are never reported");
        assert_eq!(hub.book.state(ALICE), NonceState::Syncing);

        let mut seq = 40;
        let mut probes = vec![1];
        let sent = loop {
            seq += 1;
            let p = probe(ALICE, *probes.last().unwrap());
            let fx = batch(&mut hub, answer(seq, &p, &mut engine_last), 0);
            assert!(frames(&fx, 1).is_empty(), "admin sees no probe: {fx:?}");
            assert!(
                frames(&fx, 2).iter().all(|f| f["type"] != "batch"),
                "nor does the actor: {fx:?}"
            );
            let offered = offers(&fx);
            assert_eq!(offered.len(), 1, "one probe or the command at a time: {fx:?}");
            if offered[0].action == (Action::Withdraw { amount: 0 }) {
                assert_eq!(sends(&fx), 0, "{fx:?}");
                probes.push(offered[0].nonce);
            } else {
                break (offered[0].clone(), frames(&fx, 2));
            }
        };
        assert_eq!(probes, [1, 2, 4, 8, 6, 7], "rfq-demo's search for last = 6");
        assert_eq!(engine_last, 7, "exactly one probe consumed a nonce");
        assert_eq!(sent.0, withdraw(ALICE, 8));
        assert_eq!(
            sent.1,
            vec![json!({"type": "sent", "id": "c1", "actor": 10, "nonce": 8})]
        );

        let fx = batch(
            &mut hub,
            user_batch(seq + 1, (ALICE, 8), vec![withdrawn(ALICE)]),
            0,
        );
        assert_eq!(types(&frames(&fx, 2)), ["batch", "result"]);
    }

    /// Timers that fire inside a probe's apply are real output for other accounts: they still
    /// reach their viewers, without the probe's origin or rejection.
    #[test]
    fn timer_events_in_a_probe_batch_still_reach_their_viewers() {
        let mut hub = up();
        open(&mut hub, 1, None);
        open(&mut hub, 2, Some(json!({"account": 10})));
        open(&mut hub, 3, Some(json!({"account": 20})));
        command(&mut hub, 2, "c1", ALICE, 0);
        let expiry = Emitted::to(MM1, Event::QuoteExpired { quote: QuoteId(3) });
        let mut b = user_batch(
            41,
            (ALICE, 1),
            vec![expiry.clone(), rejected(ALICE, RejectReason::Duplicate)],
        );
        b.balances = vec![(MM1, bal(9))];
        let fx = batch(&mut hub, b, 0);

        let admin = frames(&fx, 1);
        assert_eq!(types(&admin), ["batch"]);
        assert_eq!(admin[0]["origin"], Value::Null);
        assert_eq!(
            admin[0]["events"],
            json!([{"audience": {"Account": 20}, "event": {"QuoteExpired": {"quote": 3}}}])
        );
        assert_eq!(admin[0]["balances"].as_array().unwrap().len(), 1);
        assert_eq!(types(&frames(&fx, 3)), ["batch"]);
        assert!(frames(&fx, 2).is_empty(), "nothing of the probe for Alice");
        assert_eq!(offers(&fx), vec![probe(ALICE, 2)], "the search goes on");
    }

    #[test]
    fn in_flight_command_fails_when_the_engine_goes_down() {
        let mut hub = up();
        open(&mut hub, 1, None);
        open(&mut hub, 2, Some(json!({"account": 20})));
        known(&mut hub, 20, ALICE, 4);
        command(&mut hub, 1, "c1", ALICE, 0);
        command(&mut hub, 1, "c2", ALICE, 0);

        let fx = hub.handle(Input::Ingress { connected: false }, 10, WALL);
        assert!(offers(&fx).is_empty());
        assert_eq!(
            frames(&fx, 1),
            vec![
                json!({"type": "error", "id": "c1", "error": unknown_outcome(5)}),
                json!({"type": "error", "id": "c2", "error": "engine unavailable"}),
                json!({"type": "status", "engine": "down", "last_seq": 20}),
            ],
            "c1 was offered: it may still be applied; c2 never was"
        );
        assert_eq!(
            frames(&fx, 2),
            vec![json!({"type": "status", "engine": "down", "last_seq": 20})]
        );
        assert_eq!(
            hub.book.state(ALICE),
            NonceState::Unknown,
            "nonce 5 may or may not have been consumed"
        );

        // After the restart, the actor is probed again, and nothing old comes back.
        hub.handle(Input::Ingress { connected: true }, 20, WALL);
        batch(
            &mut hub,
            EventBatch {
                seq: 21,
                time: 6_000,
                kind: BatchKind::Resume,
                origin: None,
                events: Vec::new(),
                balances: Vec::new(),
            },
            30,
        );
        assert_eq!(
            offers(&command(&mut hub, 1, "c3", ALICE, 40)),
            vec![probe(ALICE, 1)]
        );
    }

    #[test]
    fn a_command_while_down_is_refused_not_queued() {
        let mut hub = Hub::new();
        open(&mut hub, 1, None);
        let fx = command(&mut hub, 1, "c1", ALICE, 0);
        assert!(offers(&fx).is_empty());
        assert_eq!(
            frames(&fx, 1),
            vec![json!({"type": "error", "id": "c1", "error": "engine unavailable"})]
        );
        let fx = hub.handle(Input::Ingress { connected: true }, 0, WALL);
        assert!(offers(&fx).is_empty(), "c1 was not queued");
        assert_eq!(types(&frames(&fx, 1)), ["status"]);
    }

    #[test]
    fn first_batch_after_down_marks_up_and_broadcasts_status() {
        let mut hub = up();
        open(&mut hub, 1, None);
        let fx = hub.handle(Input::Ingress { connected: false }, 0, WALL);
        assert_eq!(
            frames(&fx, 1),
            vec![json!({"type": "status", "engine": "down", "last_seq": null})]
        );
        let fx = hub.handle(Input::Ingress { connected: true }, 0, WALL);
        assert!(fx.is_empty(), "connected alone is not up: {fx:?}");
        assert_eq!(
            types(&frames(&command(&mut hub, 1, "c1", ALICE, 0), 1)),
            ["error"]
        );

        let resume = EventBatch {
            seq: 30,
            time: 6_000,
            kind: BatchKind::Resume,
            origin: None,
            events: Vec::new(),
            balances: Vec::new(),
        };
        let fx = batch(&mut hub, resume, 0);
        let got = frames(&fx, 1);
        assert_eq!(types(&got), ["batch", "status"], "the Resume first, then up");
        assert_eq!(got[0]["kind"], "Resume");
        assert_eq!(got[1], json!({"type": "status", "engine": "up", "last_seq": 30}));
        assert_eq!(
            offers(&command(&mut hub, 1, "c2", ALICE, 0)),
            vec![probe(ALICE, 1)]
        );
    }

    #[test]
    fn a_pending_command_times_out_after_3s() {
        let mut hub = up();
        open(&mut hub, 1, None);
        known(&mut hub, 20, ALICE, 4);
        command(&mut hub, 1, "c1", ALICE, 1_000_000);
        assert!(
            hub.handle(Input::Tick, 4_000_000, WALL).is_empty(),
            "3 s is not older than 3 s"
        );
        let fx = hub.handle(Input::Tick, 4_000_001, WALL);
        assert_eq!(
            frames(&fx, 1),
            vec![
                json!({"type": "error", "id": "c1", "error": unknown_outcome(5)}),
                json!({"type": "status", "engine": "down", "last_seq": 20}),
            ]
        );
        assert_eq!(hub.book.state(ALICE), NonceState::Unknown);
        let fx = hub.handle(Input::Tick, 9_000_000, WALL);
        assert!(sends(&fx) == 0, "down once: {fx:?}");

        // The late batch is the engine back: c1's sender still gets its result, the engine is up
        // again, and the book learns the nonce it consumed.
        let fx = batch(
            &mut hub,
            user_batch(21, (ALICE, 5), vec![withdrawn(ALICE)]),
            9_500_000,
        );
        let got = frames(&fx, 1);
        assert_eq!(types(&got), ["batch", "result", "status"]);
        assert_eq!(
            got[1],
            json!({"type": "result", "id": "c1", "seq": 21, "latency_us": 8_500_000})
        );
        assert_eq!(hub.book.state(ALICE), NonceState::Known { last: 5 });
    }

    #[test]
    fn bad_text_gets_an_error_and_the_connection_stays_open() {
        let mut hub = up();
        open(&mut hub, 1, None);
        for bad in [
            "nope".to_string(),
            r#"{"type":"subscribe"}"#.to_string(),
            r#"{"type":"command","id":"c1","actor":10,"action":{"SubmitQuote":{"request":1,"leg":0,"price":0,"size":1,"expires_at":1}}}"#.to_string(),
            "x".repeat(70 * 1024),
        ] {
            let fx = hub.handle(Input::Text(ConnId(1), bad), 0, WALL);
            let got = frames(&fx, 1);
            assert_eq!(types(&got), ["error"]);
            assert_eq!(got[0]["id"], Value::Null);
            assert!(got[0]["error"].as_str().unwrap().starts_with("bad request: "), "{got:?}");
            assert!(offers(&fx).is_empty());
        }
        let fx = text(&mut hub, 1, json!({"type": "ping"}), 0);
        assert_eq!(frames(&fx, 1), vec![json!({"type": "pong"})]);
        // Text from a connection the hub doesn't know is ignored.
        assert!(
            hub.handle(Input::Text(ConnId(9), "nope".into()), 0, WALL)
                .is_empty()
        );
    }

    #[test]
    fn close_drops_queued_commands_and_results() {
        let mut hub = up();
        open(&mut hub, 1, None);
        open(&mut hub, 2, None);
        known(&mut hub, 20, ALICE, 4);
        command(&mut hub, 1, "c1", ALICE, 0);
        command(&mut hub, 1, "c2", ALICE, 0);
        command(&mut hub, 2, "c3", ALICE, 0);

        assert!(hub.handle(Input::Close(ConnId(1)), 0, WALL).is_empty());
        let fx = batch(&mut hub, user_batch(21, (ALICE, 5), vec![withdrawn(ALICE)]), 0);
        assert!(
            frames(&fx, 1).is_empty(),
            "nothing to a closed connection, not even c1's result"
        );
        assert_eq!(
            offers(&fx),
            vec![withdraw(ALICE, 6)],
            "c2 was dropped, c3 goes next"
        );
        assert_eq!(
            frames(&fx, 2)[1],
            json!({"type": "sent", "id": "c3", "actor": 10, "nonce": 6})
        );
    }

    #[test]
    fn hello_gets_welcome_then_status() {
        let mut hub = up();
        hub.handle(Input::Open(ConnId(1)), 0, WALL);
        let fx = text(&mut hub, 1, json!({"type": "hello", "view": {"account": 10}}), 0);
        assert_eq!(
            frames(&fx, 1),
            vec![
                json!({"type": "welcome", "view": {"account": 10}, "engine": "up", "now": WALL}),
                json!({"type": "status", "engine": "up", "last_seq": null}),
            ]
        );
        let mut down = Hub::new();
        down.handle(Input::Open(ConnId(1)), 0, WALL);
        let fx = down.handle(
            Input::Text(ConnId(1), r#"{"type":"hello","view":"admin"}"#.into()),
            0,
            7,
        );
        assert_eq!(
            frames(&fx, 1)[0],
            json!({"type": "welcome", "view": "admin", "engine": "down", "now": 7})
        );
    }

    /// The gateway starts with the ingress publication unconnected: down. When it connects (the
    /// node is running), the engine is up; nothing failed, so there is no `Resume` to wait for.
    /// After a failure it is different: a restarted node's `Resume` may be polled just before
    /// the connection is seen, and it still counts, so `up` follows the connection.
    #[test]
    fn the_first_connection_at_start_up_is_up() {
        let mut hub = Hub::new();
        open(&mut hub, 1, None);
        let fx = hub.handle(Input::Ingress { connected: false }, 0, WALL);
        assert!(fx.is_empty(), "already down: {fx:?}");
        let fx = hub.handle(Input::Ingress { connected: true }, 0, WALL);
        assert_eq!(
            frames(&fx, 1),
            vec![json!({"type": "status", "engine": "up", "last_seq": null})]
        );

        let mut hub = up();
        open(&mut hub, 1, None);
        hub.handle(Input::Ingress { connected: false }, 0, WALL);
        let fx = batch(&mut hub, resume(5), 0);
        assert_eq!(
            types(&frames(&fx, 1)),
            ["batch"],
            "the Resume first; not up while disconnected"
        );
        let fx = hub.handle(Input::Ingress { connected: true }, 0, WALL);
        assert_eq!(
            frames(&fx, 1),
            vec![json!({"type": "status", "engine": "up", "last_seq": 5})]
        );
    }

    /// After the disconnect, the gateway may still poll batches the old node published before it
    /// died. They are forwarded, but they are no sign of recovery: when a node connects again,
    /// the engine stays down until that node's `Resume`, so a client sees `down`, then the
    /// `Resume` batch, then `up`.
    #[test]
    fn a_stale_batch_after_the_disconnect_does_not_mark_up() {
        let mut hub = up();
        open(&mut hub, 1, None);
        let fx = hub.handle(Input::Ingress { connected: false }, 0, WALL);
        assert_eq!(types(&frames(&fx, 1)), ["status"]);

        let fx = batch(&mut hub, user_batch(7, (MM1, 1), vec![withdrawn(MM1)]), 0);
        assert_eq!(types(&frames(&fx, 1)), ["batch"], "forwarded, but no status");
        let fx = hub.handle(Input::Ingress { connected: true }, 0, WALL);
        assert!(fx.is_empty(), "still down: {fx:?}");
        assert_eq!(
            frames(&command(&mut hub, 1, "c1", ALICE, 0), 1),
            vec![json!({"type": "error", "id": "c1", "error": "engine unavailable"})]
        );

        let got = frames(&batch(&mut hub, resume(8), 0), 1);
        assert_eq!(types(&got), ["batch", "status"]);
        assert_eq!(got[0]["kind"], "Resume");
        assert_eq!(got[1], json!({"type": "status", "engine": "up", "last_seq": 8}));
    }

    /// An explicit nonce (attack scripts) is sent as given, for a known or an unknown actor, and
    /// the book follows the engine's answer.
    #[test]
    fn an_explicit_nonce_is_sent_as_given() {
        let mut hub = up();
        open(&mut hub, 1, None);
        known(&mut hub, 20, ALICE, 4);
        let replay = json!({"type": "command", "id": "r", "actor": 10, "nonce": 4,
            "action": {"Withdraw": {"amount": 1}}});
        let fx = text(&mut hub, 1, replay, 0);
        assert_eq!(offers(&fx), vec![withdraw(ALICE, 4)]);
        assert_eq!(
            frames(&fx, 1),
            vec![json!({"type": "sent", "id": "r", "actor": 10, "nonce": 4})]
        );
        let fx = batch(
            &mut hub,
            user_batch(21, (ALICE, 4), vec![rejected(ALICE, RejectReason::Duplicate)]),
            0,
        );
        assert_eq!(types(&frames(&fx, 1)), ["batch", "result"]);
        assert_eq!(hub.book.state(ALICE), NonceState::Known { last: 4 });
        assert_eq!(
            offers(&command(&mut hub, 1, "c", ALICE, 0)),
            vec![withdraw(ALICE, 5)]
        );

        let gap = json!({"type": "command", "id": "g", "actor": 20, "nonce": 3,
            "action": {"Withdraw": {"amount": 1}}});
        assert_eq!(
            offers(&text(&mut hub, 1, gap, 0)),
            vec![withdraw(MM1, 3)],
            "no probe"
        );
    }

    /// When the engine refuses the nonce the book chose, the book missed some egress: the actor's
    /// next command probes again.
    #[test]
    fn a_refused_chosen_nonce_resyncs_the_actor() {
        let mut hub = up();
        open(&mut hub, 1, None);
        known(&mut hub, 20, ALICE, 4);
        command(&mut hub, 1, "c1", ALICE, 0);
        command(&mut hub, 1, "c2", ALICE, 0);
        let fx = batch(
            &mut hub,
            user_batch(21, (ALICE, 5), vec![rejected(ALICE, RejectReason::Duplicate)]),
            0,
        );
        assert_eq!(types(&frames(&fx, 1)), ["batch", "result"]);
        assert_eq!(offers(&fx), vec![probe(ALICE, 1)], "c2 waits for a fresh sync");
    }

    #[test]
    fn a_failed_nonce_sync_fails_the_actors_commands() {
        let mut hub = up();
        open(&mut hub, 1, None);
        command(&mut hub, 1, "c1", ALICE, 0);
        command(&mut hub, 1, "c2", ALICE, 0);
        let fx = batch(
            &mut hub,
            user_batch(
                21,
                (ALICE, 1),
                vec![rejected(ALICE, RejectReason::NotInitialized)],
            ),
            0,
        );
        assert_eq!(
            frames(&fx, 1),
            vec![
                json!({"type": "error", "id": "c1", "error": "nonce sync failed for account 10"}),
                json!({"type": "error", "id": "c2", "error": "nonce sync failed for account 10"}),
            ]
        );
        assert!(offers(&fx).is_empty());
        assert_eq!(hub.book.state(ALICE), NonceState::Unknown);
        assert_eq!(
            offers(&command(&mut hub, 1, "c3", ALICE, 0)),
            vec![probe(ALICE, 1)]
        );
    }

    /// An egress batch too big for one message arrives as chunks with one `seq` and `origin`; the
    /// balances may come last, alone. Each chunk is forwarded; the result goes out once.
    #[test]
    fn the_chunks_of_one_batch_get_one_result() {
        let mut hub = up();
        open(&mut hub, 1, None);
        known(&mut hub, 20, ALICE, 4);
        command(&mut hub, 1, "c1", ALICE, 0);
        command(&mut hub, 1, "c2", ALICE, 0);
        let first = user_batch(21, (ALICE, 5), vec![withdrawn(ALICE)]);
        let last = EventBatch {
            events: Vec::new(),
            balances: vec![(ALICE, bal(3))],
            ..first.clone()
        };
        let fx = batch(&mut hub, first, 0);
        assert_eq!(types(&frames(&fx, 1)), ["batch", "result", "sent"]);
        let fx = batch(&mut hub, last, 0);
        assert_eq!(types(&frames(&fx, 1)), ["batch"], "no second result");
        assert!(offers(&fx).is_empty());
        assert_eq!(
            hub.book.state(ALICE),
            NonceState::Known { last: 5 },
            "counted once; c2 (nonce 6) has no batch yet"
        );

        // A probe's chunks: the balances that come after its answer are not a probe of their own.
        let mut hub = up();
        open(&mut hub, 1, None);
        command(&mut hub, 1, "p", MM1, 0);
        batch(
            &mut hub,
            user_batch(30, (MM1, 1), vec![rejected(MM1, RejectReason::Duplicate)]),
            0,
        );
        let tail = EventBatch {
            events: Vec::new(),
            balances: vec![(ALICE, bal(3))],
            ..user_batch(30, (MM1, 1), Vec::new())
        };
        let fx = batch(&mut hub, tail, 0);
        let got = frames(&fx, 1);
        assert_eq!(types(&got), ["batch"], "timer balances still reach their viewers");
        assert_eq!(got[0]["origin"], Value::Null);
        assert_eq!(hub.book.state(MM1), NonceState::Syncing, "and the search goes on");
        assert!(offers(&fx).is_empty());
    }

    /// The same explicit nonce twice (an attack script replaying): the first command's later
    /// chunk has the second command's origin, but it is not the second command's answer.
    #[test]
    fn a_later_chunk_never_answers_the_next_command() {
        let mut hub = up();
        open(&mut hub, 1, None);
        known(&mut hub, 20, ALICE, 4);
        let replay = |id: &str| {
            json!({"type": "command", "id": id, "actor": 10, "nonce": 3,
                "action": {"Withdraw": {"amount": 1}}})
        };
        text(&mut hub, 1, replay("r1"), 0);
        text(&mut hub, 1, replay("r2"), 0);
        let first = user_batch(21, (ALICE, 3), vec![rejected(ALICE, RejectReason::Duplicate)]);
        let fx = batch(&mut hub, first.clone(), 0);
        assert_eq!(types(&frames(&fx, 1)), ["batch", "result", "sent"]);
        let tail = EventBatch {
            events: Vec::new(),
            balances: vec![(MM1, bal(3))],
            ..first
        };
        let fx = batch(&mut hub, tail, 0);
        assert_eq!(types(&frames(&fx, 1)), ["batch"], "r2 has no answer yet");
        let fx = batch(
            &mut hub,
            user_batch(22, (ALICE, 3), vec![rejected(ALICE, RejectReason::Duplicate)]),
            0,
        );
        let got = frames(&fx, 1);
        assert_eq!(types(&got), ["batch", "result"]);
        assert_eq!(got[1]["id"], "r2");
    }

    /// A probe's batch split in two: timers' events first, the probe's rejection last. The search
    /// waits for the rejection, and neither chunk shows the probe.
    #[test]
    fn a_split_probe_batch_is_answered_by_its_rejection() {
        let mut hub = up();
        open(&mut hub, 1, None);
        command(&mut hub, 1, "c1", ALICE, 0);
        let expiry = Emitted::to(MM1, Event::QuoteExpired { quote: QuoteId(3) });
        let lead = user_batch(41, (ALICE, 1), vec![expiry]);
        let fx = batch(&mut hub, lead.clone(), 0);
        let got = frames(&fx, 1);
        assert_eq!(types(&got), ["batch"]);
        assert_eq!(got[0]["origin"], Value::Null);
        assert!(offers(&fx).is_empty(), "no answer yet");
        let last = EventBatch {
            events: vec![rejected(ALICE, RejectReason::InvalidInput)],
            ..lead
        };
        let fx = batch(&mut hub, last, 0);
        assert!(frames(&fx, 1).iter().all(|f| f["type"] != "batch"), "{fx:?}");
        assert_eq!(
            offers(&fx),
            vec![withdraw(ALICE, 2)],
            "synced at last = 1: c1 goes"
        );
        assert_eq!(hub.book.state(ALICE), NonceState::Known { last: 1 });
    }

    /// A probe in flight when the engine goes down may still be answered, by a slow engine. That
    /// answer stays internal too, and the book learns from it. A command sent after the restart
    /// with the same nonce is still answered as itself.
    #[test]
    fn a_probe_answered_after_the_engine_went_down_stays_internal() {
        let mut hub = up();
        open(&mut hub, 1, None);
        command(&mut hub, 1, "c1", ALICE, 0);
        command(&mut hub, 1, "m1", MM1, 0);
        let fx = hub.handle(Input::Tick, 3_000_001, WALL);
        assert_eq!(types(&frames(&fx, 1)), ["error", "error", "status"]);

        // ALICE's probe (nonce 1) was taken: the batch is internal, and ALICE is known.
        let fx = batch(
            &mut hub,
            user_batch(50, (ALICE, 1), vec![rejected(ALICE, RejectReason::InvalidInput)]),
            3_100_000,
        );
        assert_eq!(types(&frames(&fx, 1)), ["status"], "up, and nothing of the probe");
        assert_eq!(hub.book.state(ALICE), NonceState::Known { last: 1 });

        // MM1's probe (nonce 1) never came. A new search for MM1 sends nonce 1 again: its answer
        // is the new probe's, not the orphan's.
        let fx = command(&mut hub, 1, "m2", MM1, 3_200_000);
        assert_eq!(offers(&fx), vec![probe(MM1, 1)]);
        let fx = batch(
            &mut hub,
            user_batch(51, (MM1, 1), vec![rejected(MM1, RejectReason::InvalidInput)]),
            3_300_000,
        );
        assert_eq!(offers(&fx), vec![withdraw(MM1, 2)], "synced, and m2 goes");
        assert!(hub.orphan_probes.is_empty(), "no older answer can come now");
    }

    /// A node started on a wiped journal counts seqs from 1 again: its `Resume` may reuse the seq
    /// of the last batch the gateway saw, a probe's. It is a batch of its own, not a chunk.
    #[test]
    fn a_reused_seq_is_not_a_chunk() {
        let mut hub = up();
        open(&mut hub, 1, None);
        command(&mut hub, 1, "c1", ALICE, 0);
        batch(
            &mut hub,
            user_batch(2, (ALICE, 1), vec![rejected(ALICE, RejectReason::Duplicate)]),
            0,
        );
        let resume = EventBatch {
            seq: 2,
            time: 99_000,
            kind: BatchKind::Resume,
            origin: None,
            events: Vec::new(),
            balances: Vec::new(),
        };
        let got = frames(&batch(&mut hub, resume, 0), 1);
        assert_eq!(types(&got), ["batch"]);
        assert_eq!(got[0]["kind"], "Resume");
    }

    #[test]
    fn a_refused_offer_is_reported_and_frees_the_actor() {
        let mut hub = up();
        open(&mut hub, 1, None);
        known(&mut hub, 20, ALICE, 4);
        command(&mut hub, 1, "c1", ALICE, 0);
        command(&mut hub, 1, "c2", ALICE, 0);
        let fx = failed(&mut hub, ALICE, 5, OfferFailure::BackPressure);
        let got = frames(&fx, 1);
        assert_eq!(
            got[0],
            json!({"type": "error", "id": "c1", "error": "ingress back-pressured, try again"})
        );
        assert_eq!(
            offers(&fx),
            vec![withdraw(ALICE, 5)],
            "nonce 5 was never sequenced: c2 gets it"
        );
        // A stale report (not the command in flight) changes nothing.
        assert!(failed(&mut hub, ALICE, 4, OfferFailure::BackPressure).is_empty());
    }

    #[test]
    fn a_full_queue_pushes_back() {
        let mut hub = up();
        open(&mut hub, 1, None);
        known(&mut hub, 20, ALICE, 4);
        for i in 0..=MAX_QUEUED {
            let fx = command(&mut hub, 1, &format!("c{i}"), ALICE, 0);
            assert!(frames(&fx, 1).iter().all(|f| f["type"] != "error"), "c{i}");
        }
        let fx = command(&mut hub, 1, "over", ALICE, 0);
        assert_eq!(
            frames(&fx, 1),
            vec![json!({"type": "error", "id": "over", "error": "ingress back-pressured, try again"})]
        );
    }

    /// A JSON frame within 64 KiB can still encode above the node's ingress limit: serde reads a
    /// struct from an array too, so a pick can be `[0,1,1]` (8 bytes of JSON, 14 encoded). The
    /// node would drop it unseen and the gateway would time out; it is refused before it is
    /// offered instead, with no `sent` and no nonce probe.
    #[test]
    fn a_command_too_large_for_ingress_is_refused_before_it_is_offered() {
        let mut hub = up();
        open(&mut hub, 1, None);
        let picks = vec!["[0,1,1]"; 8_181].join(",");
        let frame = format!(
            r#"{{"type":"command","id":"big","actor":10,"action":{{"Accept":{{"request":1,"picks":[{picks}]}}}}}}"#
        );
        assert!(frame.len() <= MAX_TEXT, "{} bytes of JSON", frame.len());
        let fx = hub.handle(Input::Text(ConnId(1), frame), 0, WALL);
        assert!(offers(&fx).is_empty(), "never offered: {:?}", offers(&fx));
        assert_eq!(
            frames(&fx, 1),
            vec![json!({"type": "error", "id": "big",
                "error": "bad request: command too large for ingress (114571 bytes, limit 65536)"})]
        );
        // Nothing is held up.
        assert_eq!(
            offers(&command(&mut hub, 1, "c", ALICE, 0)),
            vec![probe(ALICE, 1)]
        );
    }

    /// Every offer failure gets its own sentence; only back pressure says "try again".
    #[test]
    fn an_offer_failure_says_why() {
        let mut hub = up();
        open(&mut hub, 1, None);
        known(&mut hub, 20, ALICE, 4);
        known(&mut hub, 21, MM1, 9);
        for id in ["c1", "c2", "c3", "c4"] {
            command(&mut hub, 1, id, ALICE, 0);
        }
        command(&mut hub, 1, "m1", MM1, 0);

        let fx = failed(&mut hub, ALICE, 5, OfferFailure::TooLarge(70_000));
        assert_eq!(
            frames(&fx, 1),
            vec![
                json!({"type": "error", "id": "c1",
                    "error": "bad request: command too large for ingress (70000 bytes, limit 65536)"}),
                json!({"type": "sent", "id": "c2", "actor": 10, "nonce": 5}),
            ]
        );
        let fx = failed(&mut hub, ALICE, 5, OfferFailure::BackPressure);
        assert_eq!(frames(&fx, 1)[0]["error"], "ingress back-pressured, try again");
        assert_eq!(offers(&fx), vec![withdraw(ALICE, 5)], "c3 gets the free nonce");

        // Unconnected: c3 never reached ingress, nor will c4 (it is not offered into a dead
        // ingress); m1 did, so its outcome is unknown.
        let fx = failed(&mut hub, ALICE, 5, OfferFailure::Unconnected);
        assert!(offers(&fx).is_empty(), "{:?}", offers(&fx));
        assert_eq!(
            frames(&fx, 1),
            vec![
                json!({"type": "error", "id": "c3", "error": "engine unavailable"}),
                json!({"type": "error", "id": "c4", "error": "engine unavailable"}),
                json!({"type": "error", "id": "m1", "error": unknown_outcome(10)}),
                json!({"type": "status", "engine": "down", "last_seq": 21}),
            ]
        );
    }

    /// A client that resends an unknown command with its nonce takes it over: the original's
    /// batch, if it comes, answers the resend; the resend's own `Duplicate` answers nothing more.
    #[test]
    fn a_resend_with_the_same_nonce_takes_over_an_unknown_command() {
        let mut hub = up();
        open(&mut hub, 1, None);
        known(&mut hub, 20, ALICE, 4);
        command(&mut hub, 1, "c1", ALICE, 0);
        hub.handle(Input::Tick, 3_000_001, WALL);
        batch(&mut hub, resume(21), 3_100_000);
        let resend = json!({"type": "command", "id": "r1", "actor": 10, "nonce": 5,
            "action": {"Withdraw": {"amount": 1}}});
        assert_eq!(
            offers(&text(&mut hub, 1, resend, 3_200_000)),
            vec![withdraw(ALICE, 5)]
        );

        let fx = batch(
            &mut hub,
            user_batch(22, (ALICE, 5), vec![withdrawn(ALICE)]),
            3_300_000,
        );
        let got = frames(&fx, 1);
        assert_eq!(types(&got), ["batch", "result"]);
        assert_eq!(got[1]["id"], "r1");
        let fx = batch(
            &mut hub,
            user_batch(23, (ALICE, 5), vec![rejected(ALICE, RejectReason::Duplicate)]),
            3_400_000,
        );
        assert_eq!(types(&frames(&fx, 1)), ["batch"], "c1's entry is gone");
    }

    /// Down by a timeout with ingress still connected: a hung engine. Nothing else would ever
    /// publish (commands are refused while down), so the hub offers a liveness probe about every
    /// second: `Withdraw { amount: 0 }` as the operator with nonce 0, always `Duplicate`, which
    /// consumes nothing. Its batch is internal, and it brings the engine back up.
    #[test]
    fn a_timeout_down_recovers_by_itself_through_a_liveness_probe() {
        let mut hub = up();
        open(&mut hub, 1, None);
        open(&mut hub, 2, Some(json!({"account": 1})));
        known(&mut hub, 20, ALICE, 4);
        command(&mut hub, 1, "c1", ALICE, 0);

        let fx = hub.handle(Input::Tick, 3_000_001, WALL);
        assert_eq!(types(&frames(&fx, 1)), ["error", "status"]);
        assert_eq!(offers(&fx), vec![liveness()], "the first probe at once");
        assert!(offers(&hub.handle(Input::Tick, 3_500_000, WALL)).is_empty());
        assert_eq!(
            offers(&hub.handle(Input::Tick, 4_000_001, WALL)),
            vec![liveness()],
            "then every second"
        );

        let answer = |seq| {
            user_batch(
                seq,
                (OPERATOR, 0),
                vec![rejected(OPERATOR, RejectReason::Duplicate)],
            )
        };
        let fx = batch(&mut hub, answer(21), 4_100_000);
        assert_eq!(
            frames(&fx, 1),
            vec![json!({"type": "status", "engine": "up", "last_seq": 21})],
            "up, and no batch: the probe is the gateway's own"
        );
        assert_eq!(
            frames(&fx, 2),
            vec![json!({"type": "status", "engine": "up", "last_seq": 21})],
            "not even to the operator's view"
        );
        assert_eq!(
            hub.book.state(OPERATOR),
            NonceState::Unknown,
            "the book learns nothing"
        );
        // The second probe's answer comes after up: internal too.
        assert!(batch(&mut hub, answer(22), 4_200_000).is_empty());
        assert!(
            offers(&hub.handle(Input::Tick, 9_000_000, WALL)).is_empty(),
            "up: no probes"
        );
    }

    /// No liveness probe while up, nor while ingress is not connected (nobody would read it).
    /// Once ingress connects again, a probe finds out whether the node there works, even if its
    /// `Resume` was missed.
    #[test]
    fn liveness_probes_only_while_down_and_connected() {
        let mut hub = up();
        open(&mut hub, 1, None);
        assert!(hub.handle(Input::Tick, 5_000_000, WALL).is_empty(), "up and idle");
        hub.handle(Input::Ingress { connected: false }, 5_000_000, WALL);
        assert!(offers(&hub.handle(Input::Tick, 6_000_000, WALL)).is_empty());
        assert!(offers(&hub.handle(Input::Tick, 7_000_000, WALL)).is_empty());
        hub.handle(Input::Ingress { connected: true }, 7_500_000, WALL);
        assert_eq!(
            offers(&hub.handle(Input::Tick, 8_000_000, WALL)),
            vec![liveness()]
        );
        // A refused probe is simply forgotten.
        assert!(failed(&mut hub, OPERATOR, 0, OfferFailure::BackPressure).is_empty());
    }

    /// Odd but possible inputs: none panics, and the hub keeps working.
    #[test]
    fn odd_inputs_never_panic() {
        let mut hub = Hub::new();
        let odd = [
            Input::Close(ConnId(7)),
            Input::Tick,
            Input::OfferFailed {
                actor: ALICE,
                nonce: 1,
                why: OfferFailure::Unconnected,
            },
            Input::OfferFailed {
                actor: OPERATOR,
                nonce: 0,
                why: OfferFailure::TooLarge(usize::MAX),
            },
            Input::Ingress { connected: true },
            Input::Open(ConnId(1)),
            Input::Open(ConnId(1)),
            Input::Text(ConnId(1), String::new()),
            Input::Batch(user_batch(u64::MAX, (ALICE, u64::MAX), vec![withdrawn(ALICE)])),
            Input::Batch(user_batch(0, (ALICE, 0), Vec::new())),
            Input::Text(
                ConnId(1),
                r#"{"type":"command","id":"x","actor":10,"action":{"Withdraw":{"amount":1}}}"#.into(),
            ),
            Input::Text(
                ConnId(1),
                r#"{"type":"command","id":"y","actor":18446744073709551615,"nonce":18446744073709551615,"action":{"Withdraw":{"amount":1}}}"#.into(),
            ),
            Input::Tick,
            Input::Close(ConnId(1)),
            Input::Close(ConnId(1)),
        ];
        for (i, input) in odd.into_iter().enumerate() {
            // The clock may even step back.
            let now = if i % 2 == 0 { 5_000_000 } else { 1 };
            hub.handle(input, now, WALL);
        }
        open(&mut hub, 2, None);
        let fx = text(&mut hub, 2, json!({"type": "ping"}), 0);
        assert_eq!(frames(&fx, 2), vec![json!({"type": "pong"})]);
    }
}
