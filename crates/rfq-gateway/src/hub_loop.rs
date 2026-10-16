//! The hub loop (gateway.md §9): the one thread that owns the [`Hub`], the venue (the ingress
//! publication and the egress subscription, see [`Venue`]) and the outbox of every open
//! connection.
//!
//! Each turn:
//! 1. drains the inbox (at most [`MAX_INBOX_PER_TURN`] items, so egress is never starved: the
//!    node gives up on a subscriber that stops reading). The WebSocket connections and the HTTP
//!    writes (`http_write`: a short-lived connection per `POST`) both enter it;
//! 2. polls egress until it is drained (at most [`MAX_POLLS_PER_TURN`] polls), and feeds each
//!    batch (or chunk of one) to the hub;
//! 3. feeds `Ingress { connected }` when the venue's connection changes, the first connection at
//!    start-up included (the hub starts down);
//! 4. feeds a `Tick` every [`TICK`];
//!
//! and performs each effect in order: `Offer` puts the command on ingress, `Send` queues a text
//! frame on its connection's outbox. A turn with nothing to do ends with a 1 ms sleep.
//!
//! After each turn the hub's engine state and newest `seq` go to the shared
//! [`EngineStatus`], which the HTTP API reads (`GET /v1/status`).
//!
//! - **Back pressure.** Each turn has [`OFFER_BUDGET`] for retrying back-pressured offers, all
//!   offers together. An offer still back-pressured when it runs out is refused: the hub gets
//!   `OfferFailed { BackPressure }` and the sender `ingress back-pressured, try again`. Later
//!   offers in that turn try once. So a stuck ingress holds a turn for at most the budget.
//! - **Unconnected ingress** at the moment of an offer means the node is gone: the hub gets
//!   `OfferFailed { Unconnected }`; that command was never offered (`engine unavailable`), and
//!   the hub goes down.
//! - **Too large** for the publication: `OfferFailed { TooLarge }`, a bad request. (The hub
//!   already refuses anything over the node's ingress limit, far below the publication's.)
//! - **A slow client** whose outbox is full is closed (its outbox dropped), never waited for.
//! - **Fatal** (`Err` out of [`HubLoop::run`]): the venue fails (a dead driver, a closed
//!   publication or subscription). A closed inbox is fatal too, but only as a guard: the inbox has
//!   two long-lived senders, the WebSocket acceptor and the HTTP writer, each held for the
//!   process's life, so every sender being gone does not happen in practice.
use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::net::SocketAddr;

use anyhow::bail;
use rfq_app::{ClientCommand, EventBatch};
use rfq_gateway::hub::{ConnId, Effect, Hub, Input, OfferFailure};
use rfq_gateway::protocol::{EngineState, ServerMsg, encode_server};
use rfq_gateway::status::EngineStatus;
use rfq_gateway::view::View;

use crate::ws::InboxItem;

/// How often the hub gets a `Tick` (command timeouts).
pub const TICK: Duration = Duration::from_millis(100);
/// Sleep after a turn that did nothing.
const IDLE: Duration = Duration::from_millis(1);
/// How long one turn may spend retrying back-pressured offers, all of them together.
pub const OFFER_BUDGET: Duration = Duration::from_millis(50);
/// Inbox items taken per turn.
pub const MAX_INBOX_PER_TURN: usize = 1_024;
/// Egress polls per turn at most, while egress still has fragments.
pub const MAX_POLLS_PER_TURN: usize = 1_024;

/// The engine's side of the gateway: ingress out, egress in.
pub trait Venue {
    /// Offers `cmd` on ingress, retrying back pressure for up to `patience`. `Err` is fatal.
    fn offer(&mut self, cmd: &ClientCommand, patience: Duration) -> anyhow::Result<Offered>;
    /// Polls egress once and hands each whole batch (or chunk) to `sink`. Returns how many
    /// fragments it read. `Err` is fatal.
    fn poll(&mut self, sink: &mut dyn FnMut(EventBatch)) -> anyhow::Result<usize>;
    /// Whether a node is there: it subscribes to ingress and publishes egress.
    fn connected(&self) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offered {
    Sent,
    /// Still back-pressured after the patience ran out.
    BackPressured,
    /// Nobody subscribes to ingress.
    NotConnected,
    /// The encoded command (this many bytes) is over the publication's max message.
    TooLarge(usize),
}

pub struct HubLoop<V> {
    hub: Hub,
    venue: V,
    inbox: Receiver<(ConnId, InboxItem)>,
    /// What the HTTP API reports of the engine: the hub's state, stored after every turn.
    status: Arc<EngineStatus>,
    outboxes: BTreeMap<ConnId, SyncSender<String>>,
    /// Each open connection's peer, for the close log.
    peers: BTreeMap<ConnId, SocketAddr>,
    /// The venue's connection as last fed to the hub.
    connected: bool,
    /// The origin of the hub's monotonic clock (`now_us`).
    epoch: Instant,
    last_tick: Instant,
    /// Until when this turn may retry back-pressured offers ([`OFFER_BUDGET`]).
    offer_deadline: Instant,
}

impl<V: Venue> HubLoop<V> {
    pub fn new(venue: V, inbox: Receiver<(ConnId, InboxItem)>, status: Arc<EngineStatus>) -> Self {
        let now = Instant::now();
        HubLoop {
            hub: Hub::new(),
            venue,
            inbox,
            status,
            outboxes: BTreeMap::new(),
            peers: BTreeMap::new(),
            connected: false,
            epoch: now,
            last_tick: now,
            offer_deadline: now,
        }
    }

    /// Runs turns until a fatal error.
    pub fn run(mut self) -> anyhow::Result<Infallible> {
        loop {
            if !self.turn()? {
                std::thread::sleep(IDLE);
            }
        }
    }

    /// One turn. Returns whether it had anything to do.
    pub fn turn(&mut self) -> anyhow::Result<bool> {
        let mut busy = false;
        self.offer_deadline = Instant::now() + OFFER_BUDGET;
        for _ in 0..MAX_INBOX_PER_TURN {
            match self.inbox.try_recv() {
                Ok((conn, item)) => {
                    busy = true;
                    self.inbox_item(conn, item)?;
                }
                Err(TryRecvError::Empty) => break,
                // A guard: the WebSocket acceptor and the HTTP writer hold senders for good.
                Err(TryRecvError::Disconnected) => bail!("every inbox sender is gone"),
            }
        }

        for _ in 0..MAX_POLLS_PER_TURN {
            let mut batches = Vec::new();
            if self.venue.poll(&mut |b| batches.push(b))? == 0 {
                break;
            }
            busy = true;
            for b in batches {
                self.feed(Input::Batch(b))?;
            }
        }

        let connected = self.venue.connected();
        if connected != self.connected {
            self.connected = connected;
            self.feed(Input::Ingress { connected })?;
        }

        if self.last_tick.elapsed() >= TICK {
            self.last_tick = Instant::now();
            self.feed(Input::Tick)?;
        }
        self.status
            .store(self.hub.engine() == EngineState::Up, self.hub.last_seq());
        Ok(busy)
    }

    fn inbox_item(&mut self, conn: ConnId, item: InboxItem) -> anyhow::Result<()> {
        match item {
            InboxItem::Open { peer, outbox } => {
                tracing::info!(%peer, conn = conn.0, "ws open");
                self.outboxes.insert(conn, outbox);
                self.peers.insert(conn, peer);
                self.feed(Input::Open(conn))
            }
            InboxItem::Text(text) => self.feed(Input::Text(conn, text)),
            InboxItem::Refused(error) => {
                let frame = encode_server(&ServerMsg::Error { id: None, error });
                let more = self.send(conn, frame);
                self.perform(more)
            }
            InboxItem::Close { why } => {
                let view = self.hub.view(conn).map_or_else(|| "-".to_string(), view_name);
                let peer = self
                    .peers
                    .remove(&conn)
                    .map_or_else(|| "-".to_string(), |p| p.to_string());
                tracing::info!(peer = %peer, conn = conn.0, view, why, "ws close");
                self.outboxes.remove(&conn);
                self.feed(Input::Close(conn))
            }
        }
    }

    /// Feeds one input to the hub and performs its effects.
    fn feed(&mut self, input: Input) -> anyhow::Result<()> {
        let effects = self.handle(input);
        self.perform(effects)
    }

    fn handle(&mut self, input: Input) -> Vec<Effect> {
        let now_us = u64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.hub.handle(input, now_us, wall_ms())
    }

    /// Performs effects in order. What a failed offer causes (an `error` for its sender, the next
    /// command of the actor) is performed after the effects already queued, so the sender sees
    /// `sent` before the `error`.
    fn perform(&mut self, effects: Vec<Effect>) -> anyhow::Result<()> {
        let mut queue = VecDeque::from(effects);
        while let Some(effect) = queue.pop_front() {
            match effect {
                Effect::Send(conn, frame) => {
                    let more = self.send(conn, frame);
                    queue.extend(more);
                }
                Effect::Offer(cmd) => {
                    let patience = self.offer_deadline.saturating_duration_since(Instant::now());
                    let why = match self.venue.offer(&cmd, patience)? {
                        Offered::Sent => continue,
                        Offered::BackPressured => {
                            // It retried until the budget ran out: the rest of the turn tries once.
                            self.offer_deadline = Instant::now();
                            OfferFailure::BackPressure
                        }
                        Offered::NotConnected => {
                            self.connected = false;
                            OfferFailure::Unconnected
                        }
                        Offered::TooLarge(len) => OfferFailure::TooLarge(len),
                    };
                    let failed = Input::OfferFailed {
                        actor: cmd.actor,
                        nonce: cmd.nonce,
                        why,
                    };
                    let more = self.handle(failed);
                    queue.extend(more);
                }
            }
        }
        Ok(())
    }

    /// Queues `frame` on `conn`'s outbox. A connection too slow to keep up is closed: its outbox
    /// is dropped (its thread then closes the socket) and the hub forgets it.
    fn send(&mut self, conn: ConnId, frame: String) -> Vec<Effect> {
        let Some(outbox) = self.outboxes.get(&conn) else {
            return Vec::new(); // closed meanwhile; its `Close` is on the way
        };
        match outbox.try_send(frame) {
            Ok(()) => Vec::new(),
            Err(TrySendError::Full(_)) => {
                let view = self.hub.view(conn).map_or_else(|| "-".to_string(), view_name);
                tracing::warn!(conn = conn.0, view, "ws client too slow to read; closing it");
                self.outboxes.remove(&conn);
                self.handle(Input::Close(conn))
            }
            Err(TrySendError::Disconnected(_)) => {
                self.outboxes.remove(&conn);
                Vec::new()
            }
        }
    }
}

/// `admin`, or `account 10`, for the logs.
fn view_name(view: View) -> String {
    match view {
        View::Admin => "admin".into(),
        View::Account(a) => format!("account {}", a.0),
    }
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::mpsc::sync_channel;
    use std::thread::JoinHandle;

    use rfq_app::BatchKind;
    use rfq_domain::{AccountId, Action, Balance, Emitted, Event, RejectReason};
    use rfq_gateway::protocol::MAX_TEXT;
    use serde_json::{Value, json};

    use super::*;
    use crate::http_server;
    use crate::http_write::Writer;
    use crate::ws::{ConnIds, INBOX};

    const ALICE: AccountId = AccountId(10);
    const MM1: AccountId = AccountId(20);

    #[derive(Default)]
    struct FakeVenue {
        connected: bool,
        /// Every offer, with the patience it was given.
        offered: Vec<(ClientCommand, Duration)>,
        /// How the next offers go; `Sent` once empty.
        answers: VecDeque<Offered>,
        egress: VecDeque<EventBatch>,
        polls: usize,
        fail: bool,
    }

    impl Venue for FakeVenue {
        fn offer(&mut self, cmd: &ClientCommand, patience: Duration) -> anyhow::Result<Offered> {
            self.offered.push((cmd.clone(), patience));
            Ok(self.answers.pop_front().unwrap_or(Offered::Sent))
        }
        /// One batch per poll, like a poll that reads one message's fragments.
        fn poll(&mut self, sink: &mut dyn FnMut(EventBatch)) -> anyhow::Result<usize> {
            if self.fail {
                bail!("the egress subscription is closed");
            }
            self.polls += 1;
            Ok(self.egress.pop_front().map_or(0, |b| {
                sink(b);
                1
            }))
        }
        fn connected(&self) -> bool {
            self.connected
        }
    }

    struct Rig {
        hub: HubLoop<FakeVenue>,
        /// A bounded inbox, like the server's, so HTTP writes can enter it.
        inbox: SyncSender<(ConnId, InboxItem)>,
        status: Arc<EngineStatus>,
        /// The connection ids of everything that opens a hub connection in a test.
        ids: ConnIds,
    }

    impl Rig {
        fn new() -> Rig {
            let (inbox, items) = sync_channel(INBOX);
            let status = Arc::new(EngineStatus::new());
            Rig {
                hub: HubLoop::new(FakeVenue::default(), items, Arc::clone(&status)),
                inbox,
                status,
                ids: ConnIds::default(),
            }
        }

        /// Opens connection `c` with an outbox of `capacity` frames.
        fn open(&mut self, c: u64, capacity: usize) -> Receiver<String> {
            let (outbox, frames) = sync_channel(capacity);
            let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
            self.inbox
                .send((ConnId(c), InboxItem::Open { peer, outbox }))
                .unwrap();
            frames
        }

        fn text(&mut self, c: u64, msg: Value) {
            self.inbox
                .send((ConnId(c), InboxItem::Text(msg.to_string())))
                .unwrap();
        }

        fn turn(&mut self) {
            self.hub.turn().unwrap();
        }
    }

    fn drain(frames: &Receiver<String>) -> Vec<Value> {
        frames
            .try_iter()
            .map(|f| serde_json::from_str(&f).unwrap())
            .collect()
    }

    fn types(frames: &[Value]) -> Vec<&str> {
        frames.iter().map(|f| f["type"].as_str().unwrap()).collect()
    }

    fn withdraw(id: &str, actor: AccountId, nonce: u64) -> Value {
        json!({"type": "command", "id": id, "actor": actor.0, "nonce": nonce,
               "action": {"Withdraw": {"amount": 1}}})
    }

    fn answer(seq: u64, actor: AccountId, nonce: u64) -> EventBatch {
        EventBatch {
            seq,
            time: 9_000,
            kind: BatchKind::User,
            origin: Some((actor, nonce)),
            events: vec![Emitted::to(
                actor,
                Event::Withdrawn {
                    account: actor,
                    amount: 1,
                },
            )],
            balances: Vec::new(),
        }
    }

    #[test]
    fn the_first_connection_marks_the_engine_up_and_a_command_gets_its_result() {
        let mut rig = Rig::new();
        let frames = rig.open(1, 64);
        rig.text(1, json!({"type": "hello", "view": "admin"}));
        rig.turn();
        let got = drain(&frames);
        assert_eq!(types(&got), ["welcome", "status"]);
        assert_eq!(got[0]["engine"], "down", "no node yet");

        rig.hub.venue.connected = true;
        rig.turn();
        assert_eq!(
            drain(&frames),
            vec![json!({"type": "status", "engine": "up", "last_seq": null})]
        );

        rig.text(1, withdraw("c1", ALICE, 5));
        rig.turn();
        assert_eq!(rig.hub.venue.offered.len(), 1);
        assert_eq!(rig.hub.venue.offered[0].0.nonce, 5);
        assert!(rig.hub.venue.offered[0].1 <= OFFER_BUDGET);
        assert_eq!(types(&drain(&frames)), ["sent"]);

        rig.hub.venue.egress.push_back(answer(7, ALICE, 5));
        rig.turn();
        let got = drain(&frames);
        assert_eq!(types(&got), ["batch", "result"]);
        assert_eq!(got[1]["id"], "c1");
        assert!(got[1]["latency_us"].is_u64());
    }

    /// The sender sees `sent`, then the `error`. A turn has one budget for retrying back
    /// pressure: once an offer has used it up, the others in that turn try once; the next turn
    /// has a fresh budget.
    #[test]
    fn back_pressure_is_reported_after_sent_within_one_budget_per_turn() {
        let mut rig = Rig::new();
        rig.hub.venue.connected = true;
        let frames = rig.open(1, 64);
        rig.turn();
        rig.hub.venue.answers = VecDeque::from([Offered::BackPressured, Offered::BackPressured]);
        rig.text(1, withdraw("c1", ALICE, 5));
        rig.text(1, withdraw("m1", MM1, 1));
        rig.turn();
        let got = drain(&frames);
        assert_eq!(types(&got), ["status", "sent", "error", "sent", "error"]);
        assert_eq!(
            got[2],
            json!({"type": "error", "id": "c1", "error": "ingress back-pressured, try again"})
        );
        assert_eq!(got[4]["id"], "m1");
        let patience: Vec<Duration> = rig.hub.venue.offered.iter().map(|o| o.1).collect();
        assert!(
            patience[0] > OFFER_BUDGET / 2 && patience[0] <= OFFER_BUDGET,
            "the first offer has the turn's budget: {patience:?}"
        );
        assert_eq!(patience[1], Duration::ZERO, "the budget is spent");

        rig.hub.venue.offered.clear();
        rig.text(1, withdraw("c2", ALICE, 5));
        rig.turn();
        assert_eq!(types(&drain(&frames)), ["sent"]);
        assert!(
            rig.hub.venue.offered[0].1 > OFFER_BUDGET / 2,
            "a new turn, a new budget"
        );
    }

    /// The venue refusing a command as too large is a bad request, not "try again".
    #[test]
    fn too_large_for_the_publication_is_a_bad_request() {
        let mut rig = Rig::new();
        rig.hub.venue.connected = true;
        let frames = rig.open(1, 64);
        rig.turn();
        drain(&frames);
        rig.hub.venue.answers = VecDeque::from([Offered::TooLarge(70_000)]);
        rig.text(1, withdraw("c1", ALICE, 5));
        rig.turn();
        let got = drain(&frames);
        assert_eq!(types(&got), ["sent", "error"]);
        assert_eq!(
            got[1]["error"],
            "bad request: command too large for ingress (70000 bytes, limit 65536)"
        );
    }

    /// Each turn polls egress until it has nothing more (here one batch per poll).
    #[test]
    fn egress_is_polled_until_drained() {
        let mut rig = Rig::new();
        rig.hub.venue.connected = true;
        let frames = rig.open(1, 64);
        rig.turn();
        drain(&frames);
        for seq in 10..13 {
            rig.hub.venue.egress.push_back(answer(seq, MM1, seq));
        }
        rig.hub.venue.polls = 0;
        rig.turn();
        assert_eq!(types(&drain(&frames)), ["batch", "batch", "batch"]);
        assert_eq!(rig.hub.venue.polls, 4, "three batches, then an empty poll");
    }

    #[test]
    fn an_unconnected_ingress_takes_the_engine_down() {
        let mut rig = Rig::new();
        rig.hub.venue.connected = true;
        let frames = rig.open(1, 64);
        rig.turn();
        drain(&frames);
        rig.hub.venue.answers = VecDeque::from([Offered::NotConnected]);
        rig.text(1, withdraw("c1", ALICE, 5));
        rig.turn();
        let got = drain(&frames);
        assert_eq!(types(&got), ["sent", "error", "status"]);
        assert_eq!(got[1]["error"], "engine unavailable");
        assert_eq!(got[2]["engine"], "down");
        // The venue still says connected: the hub hears it again on the next turn, but stays down
        // until the engine shows it works (a batch).
        rig.turn();
        assert!(drain(&frames).is_empty());
        rig.hub.venue.egress.push_back(answer(8, MM1, 1));
        rig.turn();
        assert_eq!(types(&drain(&frames)), ["batch", "status"]);
    }

    #[test]
    fn a_command_without_a_batch_times_out_on_the_tick() {
        let mut rig = Rig::new();
        rig.hub.venue.connected = true;
        let frames = rig.open(1, 64);
        rig.turn();
        assert_eq!(types(&drain(&frames)), ["status"]);
        rig.text(1, withdraw("c1", ALICE, 5));
        rig.turn();
        assert_eq!(types(&drain(&frames)), ["sent"]);
        // 4 s pass.
        rig.hub.epoch = rig.hub.epoch.checked_sub(Duration::from_secs(4)).unwrap();
        rig.hub.last_tick = rig.hub.last_tick.checked_sub(TICK).unwrap();
        rig.turn();
        let got = drain(&frames);
        assert_eq!(types(&got), ["error", "status"]);
        assert!(
            got[0]["error"].as_str().unwrap().starts_with("outcome unknown"),
            "{got:?}"
        );
        // Down with ingress connected: a liveness probe goes out, and its answer brings the
        // engine back up with no client traffic, and without showing the probe to anyone.
        let probe = rig.hub.venue.offered.last().unwrap().0.clone();
        assert_eq!((probe.actor, probe.nonce), (AccountId(1), 0));
        rig.hub.venue.egress.push_back(EventBatch {
            events: vec![Emitted::to(
                AccountId(1),
                Event::CommandRejected {
                    reason: RejectReason::Duplicate,
                },
            )],
            ..answer(9, AccountId(1), 0)
        });
        rig.turn();
        assert_eq!(
            drain(&frames),
            vec![json!({"type": "status", "engine": "up", "last_seq": 9})]
        );
    }

    #[test]
    fn a_slow_connection_is_closed_not_waited_for() {
        let mut rig = Rig::new();
        rig.hub.venue.connected = true;
        let slow = rig.open(1, 1);
        let fast = rig.open(2, 64);
        // welcome and status: the second does not fit.
        rig.text(1, json!({"type": "hello", "view": "admin"}));
        rig.turn();
        assert!(
            !rig.hub.outboxes.contains_key(&ConnId(1)),
            "its outbox is full: closed"
        );
        assert_eq!(rig.hub.hub.view(ConnId(1)), None);
        assert_eq!(types(&drain(&slow)), ["welcome"]);
        assert!(matches!(slow.try_recv(), Err(TryRecvError::Disconnected)));
        assert_eq!(types(&drain(&fast)), ["status"], "the engine came up meanwhile");
        rig.text(2, json!({"type": "ping"}));
        rig.turn();
        assert_eq!(types(&drain(&fast)), ["pong"]);
    }

    /// A frame the server refused (too large, binary) is answered in order with the replies to
    /// the frames before and after it.
    #[test]
    fn refusals_are_answered_in_order() {
        let mut rig = Rig::new();
        let frames = rig.open(1, 64);
        rig.inbox
            .send((ConnId(1), InboxItem::Text("nope".into())))
            .unwrap();
        rig.inbox
            .send((
                ConnId(1),
                InboxItem::Refused("bad request: message too large".into()),
            ))
            .unwrap();
        rig.text(1, json!({"type": "ping"}));
        rig.turn();
        let got = drain(&frames);
        assert_eq!(types(&got), ["error", "error", "pong"]);
        assert!(
            got[0]["error"]
                .as_str()
                .unwrap()
                .starts_with("bad request: expected")
        );
        assert_eq!(
            got[1],
            json!({"type": "error", "id": null, "error": "bad request: message too large"})
        );
    }

    #[test]
    fn a_closed_connection_is_forgotten() {
        let mut rig = Rig::new();
        let frames = rig.open(1, 64);
        rig.text(1, json!({"type": "hello", "view": {"account": 10}}));
        rig.turn();
        rig.inbox
            .send((ConnId(1), InboxItem::Close { why: "test".into() }))
            .unwrap();
        rig.turn();
        assert!(rig.hub.outboxes.is_empty());
        assert_eq!(rig.hub.hub.view(ConnId(1)), None);
        drain(&frames);
        rig.hub.venue.connected = true;
        rig.turn();
        assert!(drain(&frames).is_empty(), "nothing to a closed connection");
    }

    /// A closed inbox is a guard, not a path the process takes: the WebSocket acceptor and the
    /// HTTP writer each hold a sender for the process's life.
    #[test]
    fn a_failed_venue_or_a_closed_inbox_is_fatal() {
        let mut rig = Rig::new();
        rig.hub.venue.fail = true;
        assert!(rig.hub.turn().is_err());

        let mut rig = Rig::new();
        drop(rig.inbox);
        let err = rig.hub.turn().unwrap_err();
        assert_eq!(err.to_string(), "every inbox sender is gone");
    }

    /// The HTTP API's `GET /v1/status` reads what the hub loop stores after each turn.
    #[test]
    fn the_engine_status_follows_the_hub_after_each_turn() {
        let mut rig = Rig::new();
        assert_eq!(
            rig.status.snapshot(),
            (false, None),
            "nothing is known before a turn"
        );
        rig.hub.venue.connected = true;
        let _frames = rig.open(1, 64);
        rig.turn();
        assert_eq!(
            rig.status.snapshot(),
            (true, None),
            "the first connection marks the engine up"
        );

        // Anyone's batch moves `engine_last_seq`, a command of ours or not.
        rig.hub.venue.egress.push_back(answer(7, ALICE, 5));
        rig.turn();
        assert_eq!(rig.status.snapshot(), (true, Some(7)));

        rig.hub.venue.connected = false;
        rig.turn();
        assert_eq!(
            rig.status.snapshot(),
            (false, Some(7)),
            "down, and the last seq stays"
        );
    }

    // ---- HTTP writes (read-api.md §4): the real HTTP server and write handler, the real hub
    // loop on this thread, a client in its own thread ----

    /// How long a POST may wait in a test that is not about the wait.
    const LONG_WAIT: Duration = Duration::from_secs(20);

    impl Rig {
        /// Marks the engine up: the venue is connected, and the hub has heard so.
        fn engine_up(&mut self) {
            self.hub.venue.connected = true;
            self.turn();
        }

        /// A real HTTP server on an ephemeral port whose `POST`s enter this rig's inbox, each
        /// waiting at most `wait` for its answer.
        fn http(&self, wait: Duration) -> SocketAddr {
            let writer = Writer::new(self.inbox.clone(), self.ids.clone(), wait);
            let handler: http_server::Handler = Arc::new(move |request| writer.post(request));
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            http_server::spawn(listener, http_server::LIMITS, handler).unwrap();
            addr
        }

        /// A WebSocket-style connection, numbered from the shared counter, with `view`.
        fn ws(&mut self, view: Value) -> (u64, Receiver<String>) {
            let conn = self.ids.next().0;
            let frames = self.open(conn, 64);
            self.text(conn, json!({"type": "hello", "view": view}));
            (conn, frames)
        }

        /// Turns the hub loop until `done` holds, for at most 5 s.
        fn turn_until(&mut self, what: &str, done: impl Fn(&Rig) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                self.turn();
                if done(self) {
                    return;
                }
                assert!(Instant::now() < deadline, "gave up waiting for {what}");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        /// Until the hub has opened `conn` with Alice's view, which is its hello; and a moment
        /// more, so that the command its client sends right after is queued too.
        fn turn_until_alices_post(&mut self, conn: u64) {
            self.turn_until("the POST's hub connection", |r| {
                r.hub.hub.view(ConnId(conn)) == Some(View::Account(ALICE))
            });
            std::thread::sleep(Duration::from_millis(20));
            self.turn();
        }

        fn offered(&self) -> usize {
            self.hub.venue.offered.len()
        }
    }

    /// The raw request for a `POST` of `body`.
    fn post_text(path: &str, body: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// A client in its own thread: one `POST`, and the status and JSON body it got back.
    fn post(addr: SocketAddr, path: &str, body: &Value) -> JoinHandle<(u16, Value)> {
        let request = post_text(path, &body.to_string());
        std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            let mut raw = String::new();
            stream.read_to_string(&mut raw).unwrap();
            let (head, body) = raw.split_once("\r\n\r\n").unwrap();
            let status = head.split(' ').nth(1).unwrap().parse().unwrap();
            (status, serde_json::from_str(body).unwrap_or(Value::Null))
        })
    }

    fn withdraw_body(actor: AccountId, nonce: u64, amount: u64) -> Value {
        json!({"actor": actor.0, "nonce": nonce, "action": {"Withdraw": {"amount": amount}}})
    }

    fn quote_body(actor: AccountId, nonce: u64) -> Value {
        json!({"actor": actor.0, "nonce": nonce, "request": 7, "leg": 0, "price": 6_200,
               "size": 1_000, "expires_at": 10_000})
    }

    /// The batch of `actor`'s command `nonce`: a withdrawal of `amount`, and the balance after it.
    fn withdrew(seq: u64, actor: AccountId, nonce: u64, amount: u64) -> EventBatch {
        EventBatch {
            events: vec![Emitted::to(
                actor,
                Event::Withdrawn {
                    account: actor,
                    amount,
                },
            )],
            balances: vec![(
                actor,
                Balance {
                    free: 100 - amount,
                    reserved: 0,
                    escrowed: 0,
                },
            )],
            ..answer(seq, actor, nonce)
        }
    }

    fn amount_of(body: &Value) -> &Value {
        &body["events"][0]["event"]["Withdrawn"]["amount"]
    }

    #[test]
    fn post_command_returns_its_batch() {
        let mut rig = Rig::new();
        rig.engine_up();
        let addr = rig.http(LONG_WAIT);
        let client = post(addr, "/v1/commands", &withdraw_body(ALICE, 5, 1));

        rig.turn_until("the command on ingress", |r| r.offered() == 1);
        let (command, _) = &rig.hub.venue.offered[0];
        assert_eq!(
            (command.actor, command.nonce, &command.action),
            (ALICE, 5, &Action::Withdraw { amount: 1 })
        );
        // The test answers the venue's offer with the node's batch.
        rig.hub.venue.egress.push_back(withdrew(7, ALICE, 5, 1));
        rig.turn_until("the answer", |_| client.is_finished());
        let (status, body) = client.join().unwrap();

        assert_eq!(status, 200, "{body}");
        assert_eq!(
            (&body["seq"], &body["nonce"], &body["rejected"]),
            (&json!(7), &json!(5), &Value::Null)
        );
        assert!(body["latency_us"].is_u64(), "{body}");
        assert_eq!(
            body["events"],
            json!([{"audience": {"Account": 10}, "event": {"Withdrawn": {"account": 10, "amount": 1}}}])
        );
        assert_eq!(
            body["balances"],
            json!([{"account": 10, "balance": {"free": 99, "reserved": 0, "escrowed": 0}}])
        );
        // Its short-lived hub connection is gone.
        rig.turn_until("the hub connection closed", |r| r.hub.outboxes.is_empty());
        assert_eq!(rig.hub.hub.view(ConnId(1)), None);
    }

    /// The command the hub offers is the one the body says, through each of the three routes.
    #[test]
    fn each_route_offers_its_command() {
        let mut rig = Rig::new();
        rig.engine_up();
        let addr = rig.http(LONG_WAIT);
        let request = json!({"actor": 10, "nonce": 5, "legs": [{"market": 7, "side": "BuyYes",
            "qty": 1_000, "limit_price": 7_000}], "response_deadline": 1_500});
        let routes = [
            ("/v1/requests", request, 5),
            ("/v1/quotes", quote_body(ALICE, 6), 6),
            ("/v1/commands", withdraw_body(ALICE, 7, 1), 7),
        ];
        for (n, (path, body, nonce)) in routes.into_iter().enumerate() {
            let client = post(addr, path, &body);
            rig.turn_until("the command on ingress", |r| r.offered() == n + 1);
            rig.hub
                .venue
                .egress
                .push_back(withdrew(10 + nonce, ALICE, nonce, 1));
            rig.turn_until("the answer", |_| client.is_finished());
            assert_eq!(client.join().unwrap().0, 200, "{path}");
        }
        let actions: Vec<(u64, &str)> = rig
            .hub
            .venue
            .offered
            .iter()
            .map(|(c, _)| {
                let name = match c.action {
                    Action::SubmitRequest { .. } => "SubmitRequest",
                    Action::SubmitQuote { .. } => "SubmitQuote",
                    Action::Withdraw { .. } => "Withdraw",
                    _ => "?",
                };
                (c.nonce, name)
            })
            .collect();
        assert_eq!(
            actions,
            [(5, "SubmitRequest"), (6, "SubmitQuote"), (7, "Withdraw")]
        );
    }

    #[test]
    fn post_when_the_engine_is_down_is_503() {
        let mut rig = Rig::new(); // the hub starts down, and nothing has connected
        let addr = rig.http(LONG_WAIT);
        let client = post(addr, "/v1/commands", &withdraw_body(ALICE, 5, 1));
        rig.turn_until("the answer", |_| client.is_finished());
        let (status, body) = client.join().unwrap();
        assert_eq!((status, body), (503, json!({"error": "engine unavailable"})));
        assert_eq!(rig.offered(), 0, "never offered");
        rig.turn_until("the hub connection closed", |r| r.hub.outboxes.is_empty());
    }

    /// A command the engine refuses (its nonce is consumed) is a 422 with the reason.
    #[test]
    fn a_refused_command_is_422_with_its_reason() {
        let mut rig = Rig::new();
        rig.engine_up();
        let addr = rig.http(LONG_WAIT);
        let client = post(addr, "/v1/quotes", &quote_body(ALICE, 5));
        rig.turn_until("the command on ingress", |r| r.offered() == 1);
        rig.hub.venue.egress.push_back(EventBatch {
            events: vec![Emitted::to(
                ALICE,
                Event::CommandRejected {
                    reason: RejectReason::UnknownObject,
                },
            )],
            ..answer(7, ALICE, 5)
        });
        rig.turn_until("the answer", |_| client.is_finished());
        let (status, body) = client.join().unwrap();
        assert_eq!(status, 422, "{body}");
        assert_eq!(
            (&body["rejected"], &body["seq"], &body["nonce"]),
            (&json!("UnknownObject"), &json!(7), &json!(5))
        );
    }

    /// Review Focus 3: two writers for one account. The hub queues them; each answer takes only
    /// the batch of its own `(actor, nonce)`, though the other's batch goes to its connection too.
    #[test]
    fn two_posts_for_one_account_each_get_their_own_seq() {
        let mut rig = Rig::new();
        rig.engine_up();
        let addr = rig.http(LONG_WAIT);
        let first = post(addr, "/v1/commands", &withdraw_body(ALICE, 5, 1));
        rig.turn_until("the first command on ingress", |r| r.offered() == 1);
        let second = post(addr, "/v1/quotes", &quote_body(ALICE, 6));
        rig.turn_until_alices_post(2);
        assert_eq!(rig.offered(), 1, "one command per account on ingress at a time");

        rig.hub.venue.egress.push_back(withdrew(7, ALICE, 5, 1));
        rig.turn_until("the first answer", |_| first.is_finished());
        let (status, body) = first.join().unwrap();
        assert_eq!(
            (status, &body["seq"], &body["nonce"]),
            (200, &json!(7), &json!(5))
        );
        assert_eq!(amount_of(&body), 1);

        rig.turn_until("the second command on ingress", |r| r.offered() == 2);
        let (command, _) = &rig.hub.venue.offered[1];
        assert_eq!((command.actor, command.nonce), (ALICE, 6));
        assert!(matches!(command.action, Action::SubmitQuote { .. }));
        rig.hub.venue.egress.push_back(withdrew(8, ALICE, 6, 2));
        rig.turn_until("the second answer", |_| second.is_finished());
        let (status, body) = second.join().unwrap();
        assert_eq!(
            (status, &body["seq"], &body["nonce"]),
            (200, &json!(8), &json!(6))
        );
        assert_eq!(amount_of(&body), 2, "its own batch, not the first's");
        assert_eq!(body["balances"][0]["balance"]["free"], 98);
    }

    /// The same, with a WebSocket-style connection for the account: first behind it, then ahead of
    /// it. The WebSocket's connection gets its own `result` only.
    #[test]
    fn a_post_and_a_websocket_command_for_one_account_each_get_their_own_seq() {
        let mut rig = Rig::new();
        rig.engine_up();
        let addr = rig.http(LONG_WAIT);
        let (ws, frames) = rig.ws(json!({"account": 10}));

        // The WebSocket's command is ahead, the POST's behind it.
        rig.text(ws, withdraw("w1", ALICE, 5));
        rig.turn_until("the WebSocket command on ingress", |r| r.offered() == 1);
        let post1 = post(addr, "/v1/commands", &withdraw_body(ALICE, 6, 2));
        rig.turn_until_alices_post(ws + 1);
        rig.hub.venue.egress.push_back(withdrew(7, ALICE, 5, 1));
        rig.turn_until("the POST's command on ingress", |r| r.offered() == 2);
        rig.hub.venue.egress.push_back(withdrew(8, ALICE, 6, 2));
        rig.turn_until("the POST's answer", |_| post1.is_finished());
        let (status, body) = post1.join().unwrap();
        assert_eq!(
            (status, &body["seq"], &body["nonce"]),
            (200, &json!(8), &json!(6))
        );
        assert_eq!(amount_of(&body), 2);

        // The POST's command is ahead, the WebSocket's behind it.
        let post2 = post(addr, "/v1/commands", &withdraw_body(ALICE, 7, 3));
        rig.turn_until("the second POST's command on ingress", |r| r.offered() == 3);
        rig.text(ws, withdraw("w2", ALICE, 8));
        rig.turn();
        rig.hub.venue.egress.push_back(withdrew(9, ALICE, 7, 3));
        rig.turn_until("the second POST's answer", |_| post2.is_finished());
        let (status, body) = post2.join().unwrap();
        assert_eq!(
            (status, &body["seq"], &body["nonce"]),
            (200, &json!(9), &json!(7))
        );
        assert_eq!(amount_of(&body), 3);
        rig.turn_until("the WebSocket's second command on ingress", |r| r.offered() == 4);
        rig.hub.venue.egress.push_back(withdrew(10, ALICE, 8, 1));
        rig.turn();

        let got = drain(&frames);
        let results: Vec<(&str, u64)> = got
            .iter()
            .filter(|f| f["type"] == "result")
            .map(|f| (f["id"].as_str().unwrap(), f["seq"].as_u64().unwrap()))
            .collect();
        assert_eq!(results, [("w1", 7), ("w2", 10)], "no result for a POST's command");
        let batches: Vec<u64> = got
            .iter()
            .filter(|f| f["type"] == "batch")
            .map(|f| f["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(batches, [7, 8, 9, 10], "it sees every batch of its account");
    }

    /// Review Focus 5: the client goes away while its command waits. The handler cannot know, so
    /// the command still runs, and the answer goes nowhere; but its hub connection is closed all
    /// the same, and the actor's queue does not hold on to it.
    #[test]
    fn a_client_that_goes_away_still_closes_its_hub_connection() {
        let mut rig = Rig::new();
        rig.engine_up();
        let addr = rig.http(LONG_WAIT);
        let first = post(addr, "/v1/commands", &withdraw_body(ALICE, 5, 1));
        rig.turn_until("the first command on ingress", |r| r.offered() == 1);

        // The second client sends its command (queued behind the first's), then closes its socket.
        let mut gone = TcpStream::connect(addr).unwrap();
        gone.write_all(post_text("/v1/commands", &withdraw_body(ALICE, 6, 2).to_string()).as_bytes())
            .unwrap();
        rig.turn_until_alices_post(2);
        drop(gone);
        assert!(
            rig.hub.outboxes.contains_key(&ConnId(2)),
            "the hub cannot know yet"
        );

        rig.hub.venue.egress.push_back(withdrew(7, ALICE, 5, 1));
        rig.turn_until("the first answer", |_| first.is_finished());
        assert_eq!(first.join().unwrap().0, 200);
        rig.turn_until("the second command on ingress", |r| r.offered() == 2);
        rig.hub.venue.egress.push_back(withdrew(8, ALICE, 6, 2));
        rig.turn_until("both hub connections closed", |r| r.hub.outboxes.is_empty());
        assert_eq!(
            (rig.hub.hub.view(ConnId(1)), rig.hub.hub.view(ConnId(2))),
            (None, None)
        );

        // Nothing of Alice's is left on the hub: a new command for her goes straight to ingress.
        let third = post(addr, "/v1/commands", &withdraw_body(ALICE, 7, 3));
        rig.turn_until("the third command on ingress", |r| r.offered() == 3);
        rig.hub.venue.egress.push_back(withdrew(9, ALICE, 7, 3));
        rig.turn_until("the third answer", |_| third.is_finished());
        let (status, body) = third.join().unwrap();
        assert_eq!(
            (status, &body["seq"], &body["nonce"]),
            (200, &json!(9), &json!(7))
        );
    }

    /// No batch within the wait, for a command that was sent: it may be on ingress, so 504 with
    /// the nonce to resend it with; and the hub connection is closed all the same.
    #[test]
    fn the_wait_times_out_with_504() {
        let mut rig = Rig::new();
        rig.engine_up();
        let wait = Duration::from_millis(300);
        let addr = rig.http(wait);
        let started = Instant::now();
        let client = post(addr, "/v1/commands", &withdraw_body(ALICE, 5, 1));
        rig.turn_until("the command on ingress", |r| r.offered() == 1);
        rig.turn_until("the answer", |_| client.is_finished());
        assert!(started.elapsed() >= wait, "it waited the whole {wait:?}");
        let (status, body) = client.join().unwrap();
        assert_eq!(
            (status, body),
            (
                504,
                json!({"error": "outcome unknown: no answer from the gateway", "nonce": 5})
            )
        );
        rig.turn_until("the hub connection closed", |r| r.hub.outboxes.is_empty());
        assert_eq!(rig.hub.hub.view(ConnId(1)), None);
    }

    /// A command still queued behind another of its actor's when the wait runs out was never
    /// sent: it has no nonce to give, and the 504 says only that no answer came.
    #[test]
    fn a_command_never_sent_times_out_with_504_and_no_nonce() {
        let mut rig = Rig::new();
        rig.engine_up();
        let addr = rig.http(Duration::from_millis(300));
        let first = post(addr, "/v1/commands", &withdraw_body(ALICE, 5, 1));
        rig.turn_until("the first command on ingress", |r| r.offered() == 1);
        let second = post(
            addr,
            "/v1/commands",
            &json!({"actor": ALICE.0, "action": {"Withdraw": {"amount": 2}}}),
        );
        rig.turn_until_alices_post(2);
        rig.turn_until("both answers", |_| first.is_finished() && second.is_finished());
        assert_eq!(rig.offered(), 1, "the second command never left the queue");
        let (status, body) = first.join().unwrap();
        assert_eq!((status, &body["nonce"]), (504, &json!(5)), "{body}");
        let (status, body) = second.join().unwrap();
        assert_eq!(
            (status, body),
            (504, json!({"error": "no answer from the gateway"}))
        );
    }

    /// The hub gives up on a command the engine never answered: 504 with the nonce to resend with.
    #[test]
    fn an_unknown_outcome_is_504_with_the_nonce() {
        let mut rig = Rig::new();
        rig.engine_up();
        let addr = rig.http(LONG_WAIT);
        let client = post(addr, "/v1/commands", &withdraw_body(ALICE, 5, 1));
        rig.turn_until("the command on ingress", |r| r.offered() == 1);
        // 4 s pass with no batch.
        rig.hub.epoch = rig.hub.epoch.checked_sub(Duration::from_secs(4)).unwrap();
        rig.hub.last_tick = rig.hub.last_tick.checked_sub(TICK).unwrap();
        rig.turn_until("the answer", |_| client.is_finished());
        let (status, body) = client.join().unwrap();
        assert_eq!(status, 504, "{body}");
        assert_eq!(body["nonce"], 5);
        assert!(
            body["error"].as_str().unwrap().starts_with("outcome unknown"),
            "{body}"
        );
    }

    #[test]
    fn a_bad_body_is_400_and_never_reaches_the_hub() {
        let mut rig = Rig::new();
        rig.engine_up();
        let addr = rig.http(LONG_WAIT);
        let client = post(addr, "/v1/requests", &json!({"legs": []}));
        rig.turn_until("the answer", |_| client.is_finished());
        let (status, body) = client.join().unwrap();
        assert_eq!(status, 400);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("bad request: missing field"),
            "{body}"
        );
        rig.turn();
        assert!(rig.hub.outboxes.is_empty() && rig.offered() == 0);
    }

    /// A body within the 64 KiB limit becomes a longer frame (its type and id are added), which
    /// the hub refuses without naming a command: the handler must not wait for that.
    #[test]
    fn a_body_that_only_just_fits_is_refused_at_once() {
        let mut rig = Rig::new();
        rig.engine_up();
        let addr = rig.http(LONG_WAIT);
        let listing = |description: String| {
            json!({"actor": 1, "action": {"ListMarket": {"id": 1, "description": description,
                "oracle": 4, "trading_close": 1, "resolution_deadline": 2, "dispute_window": 1}}})
        };
        let bare = listing(String::new()).to_string().len();
        let body = listing("x".repeat(MAX_TEXT - bare));
        assert_eq!(
            body.to_string().len(),
            MAX_TEXT,
            "the body is just within the server's limit"
        );
        let client = post(addr, "/v1/commands", &body);
        rig.turn_until("the answer", |_| client.is_finished());
        let (status, body) = client.join().unwrap();
        assert_eq!(status, 400, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("bad request: message too large"),
            "{body}"
        );
        rig.turn_until("no hub connection left", |r| r.hub.outboxes.is_empty());
        assert_eq!(rig.offered(), 0);
    }

    #[test]
    fn views_read_well_in_the_log() {
        assert_eq!(view_name(View::Admin), "admin");
        assert_eq!(view_name(View::Account(ALICE)), "account 10");
    }
}
