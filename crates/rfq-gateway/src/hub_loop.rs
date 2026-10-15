//! The hub loop (gateway.md §9): the one thread that owns the [`Hub`], the venue (the ingress
//! publication and the egress subscription, see [`Venue`]) and the outbox of every open
//! connection.
//!
//! Each turn:
//! 1. drains the WebSocket inbox (at most [`MAX_INBOX_PER_TURN`] items, so egress is never
//!    starved: the node gives up on a subscriber that stops reading);
//! 2. polls egress until it is drained (at most [`MAX_POLLS_PER_TURN`] polls), and feeds each
//!    batch (or chunk of one) to the hub;
//! 3. feeds `Ingress { connected }` when the venue's connection changes, the first connection at
//!    start-up included (the hub starts down);
//! 4. feeds a `Tick` every [`TICK`];
//!
//! and performs each effect in order: `Offer` puts the command on ingress, `Send` queues a text
//! frame on its connection's outbox. A turn with nothing to do ends with a 1 ms sleep.
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
//!   publication or subscription), or the WebSocket server has stopped.
use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::net::SocketAddr;

use anyhow::bail;
use rfq_app::{ClientCommand, EventBatch};
use rfq_gateway::hub::{ConnId, Effect, Hub, Input, OfferFailure};
use rfq_gateway::protocol::{ServerMsg, encode_server};
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
    pub fn new(venue: V, inbox: Receiver<(ConnId, InboxItem)>) -> Self {
        let now = Instant::now();
        HubLoop {
            hub: Hub::new(),
            venue,
            inbox,
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
                Err(TryRecvError::Disconnected) => bail!("the WebSocket server has stopped"),
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
    use std::net::SocketAddr;
    use std::sync::mpsc::{self, Sender, sync_channel};

    use rfq_app::BatchKind;
    use rfq_domain::{AccountId, Emitted, Event, RejectReason};
    use serde_json::{Value, json};

    use super::*;

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
        inbox: Sender<(ConnId, InboxItem)>,
    }

    impl Rig {
        fn new() -> Rig {
            let (inbox, items) = mpsc::channel();
            Rig {
                hub: HubLoop::new(FakeVenue::default(), items),
                inbox,
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

    #[test]
    fn a_failed_venue_or_a_stopped_server_is_fatal() {
        let mut rig = Rig::new();
        rig.hub.venue.fail = true;
        assert!(rig.hub.turn().is_err());

        let mut rig = Rig::new();
        drop(rig.inbox);
        let err = rig.hub.turn().unwrap_err();
        assert!(err.to_string().contains("WebSocket server"), "{err}");
    }

    #[test]
    fn views_read_well_in_the_log() {
        assert_eq!(view_name(View::Admin), "admin");
        assert_eq!(view_name(View::Account(ALICE)), "account 10");
    }
}
