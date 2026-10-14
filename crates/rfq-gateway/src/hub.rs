//! The hub: the gateway's state machine (docs/specs/gateway.md §5, §6, §9).
//!
//! One [`Input`] in (a connection opened or closed, a client's text frame, an egress batch, a
//! change of the ingress connection, a clock tick), the [`Effect`]s to perform out, in order:
//! offer a command on ingress, or send a text frame to a connection. No I/O and no clock of its
//! own: the caller passes the time with each input.
//!
//! - **Commands.** Each actor has at most one command on ingress at a time; the next one waits
//!   until the first has its batch, so nonces never race (gateway.md §5). A command takes its
//!   explicit nonce, else the nonce book's `last + 1`, else it waits for a probe search.
//! - **Batches.** Every egress batch goes to every connection through its view's filter; the
//!   sender then gets its `result`. A probe's batch is the gateway's own business: its origin and
//!   its rejection are never forwarded (what timers added to it still is).
//! - **Engine state** (gateway.md §6). Down when the ingress publication is not connected or a
//!   command has had no batch for 3 s; every command in flight or waiting then fails, and its
//!   actor's nonce is forgotten. Up again once the publication is connected and a batch has
//!   arrived since the failure (a restarted node's `Resume`, in either order). At start-up nothing
//!   has failed, so the first connection is enough.
//!
//! Logging (gateway.md §8): each command sent, each result, each engine status change, each
//! nonce sync.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rfq_app::client::nonce::probe_action;
use rfq_app::{BatchKind, ClientCommand, EventBatch};
use rfq_domain::{AccountId, Action, Audience, Emitted, Event, RejectReason, Timestamp};

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

const UNAVAILABLE: &str = "engine unavailable";
const BACK_PRESSURED: &str = "ingress back-pressured, try again";

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
    /// The `Effect::Offer` of `(actor, nonce)` could not be put on ingress (back pressure). The
    /// engine never saw it, so the nonce is still free.
    OfferFailed { actor: AccountId, nonce: u64 },
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
}

enum Flight {
    /// A client's command. `conn` is `None` once the sender has gone: its result is dropped.
    Command { conn: Option<ConnId>, id: String },
    /// A nonce probe (`Withdraw { amount: 0 }`), never reported to clients.
    Probe,
}

/// The one command of an actor that is on ingress, waiting for its batch.
struct InFlight {
    nonce: u64,
    /// When it was offered, on the caller's monotonic clock (µs).
    sent_at: u64,
    what: Flight,
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
    /// batch. Up needs it, besides the connection.
    fresh: bool,
    last_seq: Option<u64>,
    /// The last batch, and whose it was. A batch too big for one message arrives as chunks with
    /// the same seq, time, kind and origin (`rfq_infra::transport::split_batch`): the first chunk
    /// tells whose it is, and the later ones answer nothing a second time.
    current: Option<(Chunk, Whose)>,
    /// `(actor, nonce)` of probes that were in flight when the engine went down, until a later
    /// command's batch shows that no answer to them can still come.
    orphan_probes: BTreeSet<(AccountId, u64)>,
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
            Input::OfferFailed { actor, nonce } => self.offer_failed(actor, nonce, now_us, &mut out),
        }
        out
    }

    fn close(&mut self, conn: ConnId) {
        self.views.remove(&conn);
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
                let waiting = Waiting {
                    conn,
                    id,
                    nonce,
                    action,
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
        let Some(Waiting { conn, id, action, .. }) = a.waiting.pop_front() else {
            return;
        };
        tracing::info!(
            conn = conn.0,
            actor = actor.0,
            nonce,
            action = action_name(&action),
            "command"
        );
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
            what: Flight::Command { conn: Some(conn), id },
        });
    }

    fn batch(&mut self, b: &EventBatch, now_us: u64, out: &mut Vec<Effect>) {
        let (first_chunk, whose) = match self.current {
            Some((last, whose)) if last == chunk(b) => (false, whose),
            _ => (true, self.whose(b)),
        };
        if let Some(origin) = b.origin {
            self.book.observe(b.seq, origin, &b.events);
        }
        match whose {
            Whose::Probe { .. } | Whose::OrphanProbe => {
                if let Some(rest) = without_probe(b) {
                    self.forward(&rest, out);
                }
            }
            Whose::Command | Whose::Other => self.forward(b, out),
        }
        let whose = match (whose, b.origin) {
            (Whose::Command, Some((actor, _))) if first_chunk => {
                self.command_answered(actor, b.seq, now_us, out);
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
        self.fresh = true;
        if self.connected && self.engine == EngineState::Down {
            self.set_engine(EngineState::Up, "a batch arrived", out);
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
                // went down has its batch, any answer to an older probe has come (or never will).
                self.orphan_probes.clear();
                match what {
                    Flight::Command { .. } => Whose::Command,
                    Flight::Probe => Whose::Probe { answered: false },
                }
            }
            None if self.orphan_probes.remove(&(actor, nonce)) => Whose::OrphanProbe,
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
            sent_at,
            what: Flight::Command { conn, id },
            ..
        }) = a.in_flight.take()
        else {
            return;
        };
        let latency_us = now_us.saturating_sub(sent_at);
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
        if self.engine == EngineState::Down {
            return;
        }
        let late = self
            .actors
            .values()
            .filter_map(|a| a.in_flight.as_ref())
            .any(|f| now_us.saturating_sub(f.sent_at) > COMMAND_TIMEOUT_US);
        if late {
            self.go_down("no batch within 3 s of a command", out);
        }
    }

    fn offer_failed(&mut self, actor: AccountId, nonce: u64, now_us: u64, out: &mut Vec<Effect>) {
        let Some(a) = self.actors.get_mut(&actor) else {
            return;
        };
        if !a.in_flight.as_ref().is_some_and(|f| f.nonce == nonce) {
            return;
        }
        let Some(f) = a.in_flight.take() else {
            return;
        };
        match f.what {
            Flight::Command { conn, id } => {
                tracing::warn!(actor = actor.0, nonce, "ingress back-pressured");
                if let Some(conn) = conn {
                    self.reply(out, conn, &error(Some(id), BACK_PRESSURED));
                }
                self.advance(actor, now_us, out);
            }
            Flight::Probe => {
                tracing::warn!(
                    actor = actor.0,
                    nonce,
                    "ingress back-pressured during a nonce sync"
                );
                self.book.forget(actor);
                self.fail_waiting(actor, BACK_PRESSURED, out);
            }
        }
    }

    /// The engine is down: every command in flight or waiting fails, and the actors with one in
    /// flight are forgotten by the nonce book, since that nonce may or may not have been consumed.
    fn go_down(&mut self, why: &str, out: &mut Vec<Effect>) {
        self.fresh = false;
        for (actor, a) in std::mem::take(&mut self.actors) {
            if let Some(f) = a.in_flight {
                self.book.forget(actor);
                match f.what {
                    Flight::Command { conn: Some(conn), id } => {
                        self.reply(out, conn, &error(Some(id), UNAVAILABLE));
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

    const ALICE: AccountId = AccountId(10);
    const MM1: AccountId = AccountId(20);
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
            json!({"type": "result", "id": "c2", "seq": 23, "latency_us": 500}),
            "latency from sending c2"
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
                json!({"type": "error", "id": "c1", "error": "engine unavailable"}),
                json!({"type": "error", "id": "c2", "error": "engine unavailable"}),
                json!({"type": "status", "engine": "down", "last_seq": 20}),
            ]
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
                json!({"type": "error", "id": "c1", "error": "engine unavailable"}),
                json!({"type": "status", "engine": "down", "last_seq": 20}),
            ]
        );
        assert_eq!(hub.book.state(ALICE), NonceState::Unknown);
        assert!(hub.handle(Input::Tick, 9_000_000, WALL).is_empty(), "down once");

        // The late batch is the engine back: no result (c1 already failed), but up again, and
        // the book learns the nonce it consumed.
        let fx = batch(
            &mut hub,
            user_batch(21, (ALICE, 5), vec![withdrawn(ALICE)]),
            9_500_000,
        );
        assert_eq!(types(&frames(&fx, 1)), ["batch", "status"]);
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
        // A Resume that came before the connection still counts.
        let mut hub = up();
        hub.handle(Input::Ingress { connected: false }, 0, WALL);
        batch(&mut hub, user_batch(5, (MM1, 1), vec![withdrawn(MM1)]), 0);
        open(&mut hub, 1, None);
        let fx = hub.handle(Input::Ingress { connected: true }, 0, WALL);
        assert_eq!(
            frames(&fx, 1),
            vec![json!({"type": "status", "engine": "up", "last_seq": 5})]
        );
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
        let fx = hub.handle(
            Input::OfferFailed {
                actor: ALICE,
                nonce: 5,
            },
            0,
            WALL,
        );
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
        assert!(
            hub.handle(
                Input::OfferFailed {
                    actor: ALICE,
                    nonce: 4
                },
                0,
                WALL
            )
            .is_empty()
        );
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

    /// Odd but possible inputs: none panics, and the hub keeps working.
    #[test]
    fn odd_inputs_never_panic() {
        let mut hub = Hub::new();
        let odd = [
            Input::Close(ConnId(7)),
            Input::Tick,
            Input::OfferFailed { actor: ALICE, nonce: 1 },
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
