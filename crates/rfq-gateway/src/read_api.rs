//! The read side's shared state and the `GET` router (docs/specs/read-api.md §3, §6, §7).
//!
//! [`ReadSide`] is the [`Replica`] behind a lock, fed by the replica thread (the journal follower)
//! and read by the HTTP threads: a write lock per applied command, a short read lock per `GET`.
//! [`get`] routes one `GET` to the views of `rfq-read`: pure apart from the `min_seq` wait, so the
//! whole API is tested here without a socket.
//!
//! Every `200` body is a JSON object with `as_of_seq`, the replica's `last_seq` at the read, plus
//! the fields the spec names. Any read takes `?min_seq=N` and waits up to a deadline for the
//! replica to reach it (read-your-writes), else answers `503`. A bad id or query value is `400`
//! and is refused before any wait.
//!
//! The locks survive a panic in the thread that held them (a poisoned lock is taken as it is):
//! the replica thread marks the replica `unavailable` when it dies, and reads go on serving what
//! it had.
use std::fmt::Write as _;
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use rfq_app::ClientCommand;
use rfq_domain::{AccountId, ContractId, SequencedCommand};
use rfq_read::{Replica, ReplicaError, TraceEntry, views};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::hub::{is_probe, without_probe};
use crate::protocol::OutBatch;
use crate::status::EngineStatus;
use crate::view::{View, filter};

/// What the replica thread says of the replica (`GET /v1/status`, spec §3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaState {
    /// Replaying the journal: the replica may be far behind. The state at start-up.
    #[default]
    CatchingUp,
    /// Has caught up with the journal once, and follows it.
    Following,
    /// The follower failed: the replica serves what it had, and is being rebuilt.
    Unavailable,
}

impl ReplicaState {
    /// The JSON and log spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ReplicaState::CatchingUp => "catching_up",
            ReplicaState::Following => "following",
            ReplicaState::Unavailable => "unavailable",
        }
    }
}

struct Inner {
    replica: Replica,
    state: ReplicaState,
}

/// The replica, shared between the thread that feeds it and the threads that read it.
pub struct ReadSide {
    inner: RwLock<Inner>,
    /// The replica's `last_seq`, set under `inner`'s write lock by everything that changes it. It
    /// is what [`wait_for`](ReadSide::wait_for) waits on, with `changed`.
    published: Mutex<u64>,
    changed: Condvar,
}

impl ReadSide {
    /// An empty replica, `catching_up`.
    pub fn new() -> Arc<ReadSide> {
        Arc::new(ReadSide {
            inner: RwLock::new(Inner {
                replica: Replica::new(),
                state: ReplicaState::default(),
            }),
            published: Mutex::new(0),
            changed: Condvar::new(),
        })
    }

    /// The follower thread's write path: apply one command, notify waiters.
    pub fn apply(&self, sc: &SequencedCommand) -> Result<(), ReplicaError> {
        let mut inner = self.write();
        let applied = inner.replica.apply(sc);
        self.publish(&inner);
        drop(inner);
        self.changed.notify_all();
        applied
    }

    /// [`apply`](ReadSide::apply), except that `seq` 1 after a `seq` already applied is a new
    /// journal (`docker compose down -v`, spec §7): the replica is first replaced with an empty
    /// one, so no row of the old journal is served. The replica thread uses this.
    pub fn apply_or_reset(&self, sc: &SequencedCommand) -> Result<(), ReplicaError> {
        let mut inner = self.write();
        let was = inner.replica.last_seq();
        if sc.seq == 1 && was >= 1 {
            tracing::info!(was, "replica reset: a new journal starts at seq 1");
            inner.replica = Replica::new();
        }
        let applied = inner.replica.apply(sc);
        self.publish(&inner);
        drop(inner);
        self.changed.notify_all();
        applied
    }

    pub fn state(&self) -> ReplicaState {
        self.read_lock().state
    }

    pub fn set_state(&self, s: ReplicaState) {
        self.write().state = s;
    }

    /// Swap in a rebuilt replica (spec §5 step 7), and notify waiters.
    pub fn replace(&self, r: Replica) {
        let mut inner = self.write();
        inner.replica = r;
        self.publish(&inner);
        drop(inner);
        self.changed.notify_all();
    }

    pub fn last_seq(&self) -> u64 {
        self.read_lock().replica.last_seq()
    }

    /// Runs `f` on the replica and its state under a read lock.
    pub fn read<T>(&self, f: impl FnOnce(&Replica, ReplicaState) -> T) -> T {
        let inner = self.read_lock();
        f(&inner.replica, inner.state)
    }

    /// Waits until `last_seq() >= seq` or `timeout`; true if reached.
    pub fn wait_for(&self, seq: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut published = self.published.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if *published >= seq {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            published = self
                .changed
                .wait_timeout(published, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Records the replica's `last_seq` for `wait_for`. Called with `inner` locked for writing.
    fn publish(&self, inner: &Inner) {
        *self.published.lock().unwrap_or_else(PoisonError::into_inner) = inner.replica.last_seq();
    }

    fn read_lock(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One HTTP answer: a status and a JSON body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    /// `{"error": message}`.
    pub fn error(status: u16, message: &str) -> Reply {
        let mut o = Map::new();
        o.insert("error".into(), message.into());
        Reply::object(status, o)
    }

    fn object(status: u16, fields: Map<String, Value>) -> Reply {
        Reply {
            status,
            body: Value::Object(fields).to_string(),
        }
    }
}

/// How long a read waits for `?min_seq=N` (spec §3).
pub const MIN_SEQ_WAIT: Duration = Duration::from_secs(2);
/// The trace page size when `limit` is not given.
pub const TRACE_DEFAULT: usize = 500;
/// The most trace entries one page holds; a larger `limit` is this.
pub const TRACE_MAX: usize = 1_000;

/// Routes one `GET`: `path` without its query, and the raw query string. Waits at most
/// `min_seq_wait` for `min_seq`; everything else is pure.
pub fn get(read: &ReadSide, engine: &EngineStatus, path: &str, query: &str, min_seq_wait: Duration) -> Reply {
    let (what, min_seq) = match parse(path, query) {
        Ok(parsed) => parsed,
        Err(reply) => return reply,
    };
    if let Some(seq) = min_seq
        && !read.wait_for(seq, min_seq_wait)
    {
        let mut o = Map::new();
        o.insert("error".into(), "replica behind".into());
        o.insert("as_of_seq".into(), read.last_seq().into());
        return Reply::object(503, o);
    }
    read.read(|replica, state| answer(replica, state, engine, &what))
}

/// The answer to any method but `GET` on `path`: `405` for a path the API has, else `404`.
pub fn other_method(path: &str) -> Reply {
    match route(path) {
        Err(Miss::NotFound) => not_found(),
        Ok(_) | Err(Miss::WrongMethod | Miss::BadId(_)) => Reply::error(405, "method not allowed"),
    }
}

fn not_found() -> Reply {
    Reply::error(404, "not found")
}

// ---- routing ----

enum Route {
    Status,
    Account(AccountId),
    Positions(AccountId),
    Requests(AccountId),
    Quotes(AccountId),
    OpenRequests,
    Contracts,
    Contract(ContractId),
    Trace,
}

/// Why a path is not a `GET` route.
enum Miss {
    NotFound,
    /// A path of the API that `GET` does not serve (the `POST` routes).
    WrongMethod,
    /// An id segment that is not a number; what it should have been.
    BadId(&'static str),
}

fn route(path: &str) -> Result<Route, Miss> {
    let Some(rest) = path.strip_prefix("/v1/") else {
        return Err(Miss::NotFound);
    };
    let account = |id: &str| number(id).map(AccountId).ok_or(Miss::BadId("account id"));
    let segments: Vec<&str> = rest.split('/').collect();
    match segments.as_slice() {
        ["status"] => Ok(Route::Status),
        ["accounts", id] => Ok(Route::Account(account(id)?)),
        ["accounts", id, "positions"] => Ok(Route::Positions(account(id)?)),
        ["accounts", id, "requests"] => Ok(Route::Requests(account(id)?)),
        ["accounts", id, "quotes"] => Ok(Route::Quotes(account(id)?)),
        ["requests"] => Ok(Route::OpenRequests),
        ["contracts"] => Ok(Route::Contracts),
        ["contracts", id] => number(id)
            .map(|id| Route::Contract(ContractId(id)))
            .ok_or(Miss::BadId("contract id")),
        ["trace"] => Ok(Route::Trace),
        ["quotes"] | ["commands"] => Err(Miss::WrongMethod),
        _ => Err(Miss::NotFound),
    }
}

/// A read, validated.
enum Read {
    Status,
    Account(AccountId),
    Positions(AccountId),
    Requests(AccountId),
    Quotes(AccountId),
    Open {
        contract: Option<ContractId>,
    },
    Contracts,
    Contract(ContractId),
    Trace {
        view: View,
        after_seq: u64,
        limit: usize,
    },
}

/// The read `path` and `query` ask for, and its `min_seq`. `Err` is the `400`, `404` or `405` that
/// answers them.
fn parse(path: &str, query: &str) -> Result<(Read, Option<u64>), Reply> {
    let route = route(path).map_err(|miss| match miss {
        Miss::NotFound => not_found(),
        Miss::WrongMethod => Reply::error(405, "method not allowed"),
        Miss::BadId(what) => Reply::error(400, &format!("the {what} must be a number")),
    })?;
    let query = Query::new(query);
    let min_seq = query.number("min_seq")?;
    let read = match route {
        Route::Status => Read::Status,
        Route::Account(a) => Read::Account(a),
        Route::Positions(a) => Read::Positions(a),
        Route::Requests(a) => Read::Requests(a),
        Route::Quotes(a) => Read::Quotes(a),
        Route::OpenRequests => {
            if query.get("state") != Some("open") {
                return Err(Reply::error(
                    400,
                    "state=open is required: only open requests are listed",
                ));
            }
            Read::Open {
                contract: query.number("contract")?.map(ContractId),
            }
        }
        Route::Contracts => Read::Contracts,
        Route::Contract(id) => Read::Contract(id),
        Route::Trace => {
            let view = match query.get("view") {
                Some("admin") => View::Admin,
                Some(account) => View::Account(AccountId(number(account).ok_or_else(bad_view)?)),
                None => return Err(bad_view()),
            };
            let limit = match query.number("limit")? {
                None => TRACE_DEFAULT,
                Some(0) => return Err(Reply::error(400, "limit must be at least 1")),
                Some(n) => usize::try_from(n).map_or(TRACE_MAX, |n| n.min(TRACE_MAX)),
            };
            Read::Trace {
                view,
                after_seq: query.number("after_seq")?.unwrap_or(0),
                limit,
            }
        }
    };
    Ok((read, min_seq))
}

fn bad_view() -> Reply {
    Reply::error(400, "view is required: \"admin\" or an account id")
}

/// A non-negative integer in decimal digits and nothing else (no sign, no space).
fn number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A raw query string, `a=1&b=2`. Values are not percent-decoded: every value the API takes is a
/// number, `open` or `admin`. A key given twice means its first value.
struct Query<'a>(Vec<(&'a str, &'a str)>);

impl<'a> Query<'a> {
    fn new(raw: &'a str) -> Self {
        Query(
            raw.split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
                .collect(),
        )
    }

    fn get(&self, key: &str) -> Option<&'a str> {
        self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    /// The key's value as a number: `None` if absent, `400` if it is not one.
    fn number(&self, key: &str) -> Result<Option<u64>, Reply> {
        self.get(key).map_or(Ok(None), |v| {
            number(v)
                .map(Some)
                .ok_or_else(|| Reply::error(400, &format!("{key} must be a non-negative integer")))
        })
    }
}

// ---- answers ----

fn answer(replica: &Replica, state: ReplicaState, engine: &EngineStatus, read: &Read) -> Reply {
    let e = replica.engine();
    let as_of = replica.last_seq();
    match read {
        Read::Status => status(replica, state, engine),
        Read::Account(a) => flat(as_of, &views::account(e, *a)),
        Read::Positions(a) => rows(as_of, "positions", &views::positions_of(e, *a)),
        Read::Requests(a) => rows(as_of, "requests", &views::requests_of(e, *a)),
        Read::Quotes(a) => rows(as_of, "quotes", &views::quotes_of(e, *a)),
        Read::Open { contract } => rows(as_of, "requests", &views::open_requests(e, *contract)),
        Read::Contracts => rows(as_of, "contracts", &views::contracts(e)),
        Read::Contract(id) => match views::contract(e, *id) {
            Some(c) => flat(as_of, &c),
            None => Reply::error(404, "no such contract"),
        },
        Read::Trace {
            view,
            after_seq,
            limit,
        } => trace(replica, *view, *after_seq, *limit),
    }
}

/// `{as_of_seq, <key>: <rows>}`.
fn rows<T: Serialize>(as_of: u64, key: &str, rows: &T) -> Reply {
    match serde_json::to_value(rows) {
        Ok(rows) => {
            let mut o = Map::new();
            o.insert("as_of_seq".into(), as_of.into());
            o.insert(key.into(), rows);
            Reply::object(200, o)
        }
        Err(e) => internal(&e),
    }
}

/// The view's own fields at the top level, and `as_of_seq`.
fn flat<T: Serialize>(as_of: u64, view: &T) -> Reply {
    match serde_json::to_value(view) {
        Ok(Value::Object(mut o)) => {
            o.insert("as_of_seq".into(), as_of.into());
            Reply::object(200, o)
        }
        Ok(other) => internal(&format!("expected an object, got {other}")),
        Err(e) => internal(&e),
    }
}

fn internal(e: &dyn std::fmt::Display) -> Reply {
    tracing::error!(error = %e, "could not encode a read reply");
    Reply::error(500, "internal error")
}

/// `{as_of_seq, engine, engine_last_seq, replica: {state, last_seq, state_hash}}`.
fn status(replica: &Replica, state: ReplicaState, engine: &EngineStatus) -> Reply {
    let (up, engine_last_seq) = engine.snapshot();
    let as_of = replica.last_seq();
    let mut hash = String::with_capacity(64);
    for byte in replica.state_hash() {
        let _ = write!(hash, "{byte:02x}");
    }
    let mut r = Map::new();
    r.insert("state".into(), state.as_str().into());
    r.insert("last_seq".into(), as_of.into());
    r.insert("state_hash".into(), hash.into());
    let mut o = Map::new();
    o.insert("as_of_seq".into(), as_of.into());
    o.insert("engine".into(), (if up { "up" } else { "down" }).into());
    o.insert("engine_last_seq".into(), engine_last_seq.into());
    o.insert("replica".into(), Value::Object(r));
    Reply::object(200, o)
}

/// A trace entry: the batch as the view sees it, at the top level, and the command behind it.
#[derive(Serialize)]
struct Entry<'a> {
    #[serde(flatten)]
    batch: OutBatch,
    command: Option<&'a ClientCommand>,
}

/// The entry as `view` sees it, or `None` when nothing of it is for `view`.
///
/// The gateway's own probes (`hub::is_probe`) are not what the WebSocket shows, so they are not
/// shown here: the batch goes through the hub's `without_probe` first (no origin, no rejection
/// of the prober; what timers did in the same apply stays), and its command is `null`. That is
/// the same for every view, and before the view's `filter`. Then the batch is the view's, and
/// the command is the admin's always, an account's only when it is the account's own.
fn seen(view: View, entry: &TraceEntry) -> Option<Entry<'_>> {
    let probe_free;
    let (batch, command) = match &entry.command {
        Some(command) if is_probe(command) => {
            probe_free = without_probe(&entry.batch)?;
            (&probe_free, None)
        }
        command => (&entry.batch, command.as_ref()),
    };
    Some(Entry {
        batch: filter(view, batch)?,
        command: match view {
            View::Admin => command,
            View::Account(a) => command.filter(|c| c.actor == a),
        },
    })
}

/// Entries with `seq > after_seq`, ascending, at most `limit` that `view` sees (spec §6).
///
/// `next_after_seq` is the last returned `seq` if the page is full and the view sees something
/// after it, else `null`. The look-ahead is what keeps an account's pages honest: a full page is
/// never followed by an empty one, and a page is never empty while the view still has entries.
fn trace(replica: &Replica, view: View, after_seq: u64, limit: usize) -> Reply {
    let mut entries = Vec::new();
    let mut rest = replica.trace_after(after_seq);
    while entries.len() < limit {
        let Some(entry) = rest.next() else {
            break;
        };
        entries.extend(seen(view, entry));
    }
    let next = if entries.len() == limit && rest.any(|e| seen(view, e).is_some()) {
        entries.last().map(|e| e.batch.seq)
    } else {
        None
    };
    match serde_json::to_value(&entries) {
        Ok(entries) => {
            let mut o = Map::new();
            o.insert("as_of_seq".into(), replica.last_seq().into());
            o.insert("entries".into(), entries);
            o.insert("next_after_seq".into(), next.into());
            Reply::object(200, o)
        }
        Err(e) => internal(&e),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rfq_app::client::nonce::probe_action;
    use rfq_domain::{AccountId, Action, Command, ContractId, LegSpec, RequestId, Side, Timestamp};
    use rfq_testkit::accounts::*;
    use rfq_testkit::scenarios::{all, demo_config, run_in_memory};
    use rfq_testkit::{Harness, price};
    use serde_json::{Value, json};

    use super::*;
    use crate::view::{View, filter};

    /// The scenarios' start time, in engine milliseconds (the contract is `ContractId(T0)`).
    const T0: Timestamp = 1_000;
    const WAIT: Duration = Duration::from_millis(100);

    fn happy_log() -> Vec<SequencedCommand> {
        let s = all().into_iter().find(|s| s.name == "happy_path").unwrap();
        run_in_memory(&s, T0).0.log
    }

    /// A read side that has applied the first `n` commands of `log`.
    fn read_side(log: &[SequencedCommand], n: usize) -> Arc<ReadSide> {
        let read = ReadSide::new();
        for sc in &log[..n] {
            read.apply(sc).unwrap();
        }
        read
    }

    fn call(read: &ReadSide, engine: &EngineStatus, target: &str, wait: Duration) -> (u16, Value) {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let reply = get(read, engine, path, query, wait);
        let body = serde_json::from_str(&reply.body).expect("a JSON body");
        (reply.status, body)
    }

    /// `GET target` with no engine status; 200 expected.
    fn ok(read: &ReadSide, target: &str) -> Value {
        let (status, body) = call(read, &EngineStatus::new(), target, WAIT);
        assert_eq!(status, 200, "{target}: {body}");
        assert!(body["as_of_seq"].is_u64(), "{target}: {body}");
        body
    }

    fn error(read: &ReadSide, target: &str) -> (u16, String) {
        let (status, body) = call(read, &EngineStatus::new(), target, WAIT);
        let message = body["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{target}: {body}"));
        (status, message.to_owned())
    }

    // ---- the read routes ----

    #[test]
    fn every_read_route_answers_200_with_as_of_seq_and_its_rows() {
        let log = happy_log();
        let read = read_side(&log, log.len());
        let as_of = log.len() as u64;
        let (alice, mm2) = (ALICE.0, MM2.0);

        // The ledger: Alice put up $600 for the position, and YES won the $1,000 pot.
        let body = ok(&read, &format!("/v1/accounts/{alice}"));
        assert_eq!(body["as_of_seq"], as_of);
        assert_eq!(body["account"], alice);
        assert_eq!(
            body["balance"],
            json!({"free": 1_400_000_000u64, "reserved": 0, "escrowed": 0})
        );
        assert_eq!(body["last_nonce"], 2, "her request and her accept");

        let body = ok(&read, &format!("/v1/accounts/{alice}/positions"));
        assert_eq!(body["as_of_seq"], as_of);
        let positions = body["positions"].as_array().unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(
            positions[0],
            json!({"id": positions[0]["id"], "contract": T0, "request": 7, "qty": 1_000,
                   "side": "Yes", "stake": 600_000_000u64, "settled": true,
                   "payout": 1_000_000_000u64})
        );

        let body = ok(&read, &format!("/v1/accounts/{alice}/requests"));
        let requests = body["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["id"], 7);
        assert_eq!(requests[0]["state"], "Filled");
        assert_eq!(requests[0]["legs"][0]["limit_price"], 7_000);
        assert_eq!(requests[0]["legs"][0]["selected"]["price"], 6_000);

        // MM2's quote (the command at seq 9) won; MM1's was lost.
        let body = ok(&read, &format!("/v1/accounts/{mm2}/quotes"));
        assert_eq!(body["as_of_seq"], as_of);
        assert_eq!(
            body["quotes"],
            json!([{"id": 9, "request": 7, "leg": 0, "price": 6_000, "size": 1_000,
                    "expires_at": body["quotes"][0]["expires_at"], "state": "Filled"}])
        );
        let body = ok(&read, &format!("/v1/accounts/{}/quotes", MM1.0));
        assert_eq!(body["quotes"][0]["state"], "Lost");

        let body = ok(&read, "/v1/contracts");
        assert_eq!(body["as_of_seq"], as_of);
        let contracts = body["contracts"].as_array().unwrap();
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0]["id"], T0);
        assert_eq!(contracts[0]["oracle"], ORACLE.0);
        assert_eq!(contracts[0]["state"], json!({"Final": "Yes"}));

        let body = ok(&read, &format!("/v1/contracts/{T0}"));
        assert_eq!(body["as_of_seq"], as_of);
        assert_eq!(body["id"], T0, "the ContractView fields sit at the top level");
        assert_eq!(body["description"], "Will it rain in Paris on 2026-11-01?");
        assert_eq!(body["trading_close"], T0 + 5_000);
        assert_eq!(body["dispute_window"], 2_000);

        // An account the engine never saw: zero balance, no rows.
        let body = ok(&read, "/v1/accounts/999");
        assert_eq!(body["balance"], json!({"free": 0, "reserved": 0, "escrowed": 0}));
        assert_eq!(body["last_nonce"], 0);
        assert_eq!(ok(&read, "/v1/accounts/999/positions")["positions"], json!([]));
    }

    #[test]
    fn open_requests_show_public_fields_only_and_filter_by_contract() {
        let log = happy_log();
        // Seq 9: Alice's request (seq 7) is open, with both quotes in.
        let read = read_side(&log, 9);

        let body = ok(&read, "/v1/requests?state=open");
        assert_eq!(body["as_of_seq"], 9);
        assert_eq!(
            body["requests"],
            json!([{"id": 7, "response_deadline": T0 + 1_500,
                    "legs": [{"contract": T0, "side": "BuyYes", "qty": 1_000}]}]),
            "no requester, no limit price, no quotes"
        );
        assert_eq!(
            ok(&read, &format!("/v1/requests?state=open&contract={T0}"))["requests"][0]["id"],
            7
        );
        assert_eq!(
            ok(&read, "/v1/requests?contract=5&state=open")["requests"],
            json!([])
        );

        // Past the deadline the request is presented, no longer open.
        let read = read_side(&log, 10);
        assert_eq!(ok(&read, "/v1/requests?state=open")["requests"], json!([]));
    }

    #[test]
    fn status_has_the_spec_shape() {
        let log = happy_log();
        let read = read_side(&log, 9);
        let engine = EngineStatus::new();

        let (status, body) = call(&read, &engine, "/v1/status", WAIT);
        assert_eq!(status, 200);
        let hash: String = read.read(|r, _| r.state_hash().iter().map(|b| format!("{b:02x}")).collect());
        assert_eq!(hash.len(), 64);
        assert_eq!(
            body,
            json!({"as_of_seq": 9, "engine": "down", "engine_last_seq": null,
                   "replica": {"state": "catching_up", "last_seq": 9, "state_hash": hash}})
        );

        engine.store(true, Some(12));
        read.set_state(ReplicaState::Following);
        let (_, body) = call(&read, &engine, "/v1/status", WAIT);
        assert_eq!(body["engine"], "up");
        assert_eq!(body["engine_last_seq"], 12);
        assert_eq!(body["replica"]["state"], "following");

        read.set_state(ReplicaState::Unavailable);
        let (_, body) = call(&read, &engine, "/v1/status", WAIT);
        assert_eq!(body["replica"]["state"], "unavailable");
        assert_eq!(body["replica"]["last_seq"], 9, "what it had stays served");
    }

    // ---- the trace ----

    fn entry_seqs(body: &Value) -> Vec<u64> {
        body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect()
    }

    /// Every page of `target` (which has no `after_seq`) at `limit`, until `next_after_seq` is null.
    fn pages(read: &ReadSide, target: &str, limit: usize) -> Vec<Value> {
        let mut pages: Vec<Value> = Vec::new();
        let mut after = 0u64;
        loop {
            let body = ok(read, &format!("{target}&after_seq={after}&limit={limit}"));
            let next = body["next_after_seq"].as_u64();
            pages.push(body);
            match next {
                Some(n) => {
                    assert!(n > after, "next_after_seq must advance: {n} after {after}");
                    after = n;
                }
                None => return pages,
            }
            assert!(pages.len() < 1_000, "paging does not end");
        }
    }

    #[test]
    fn trace_admin_pages_in_order_with_commands() {
        let log = happy_log();
        let read = read_side(&log, log.len());

        // Two at a time, ascending, each page pointing at the second entry.
        let body = ok(&read, "/v1/trace?view=admin&limit=2");
        assert_eq!(body["as_of_seq"], log.len() as u64);
        assert_eq!(entry_seqs(&body), [1, 2]);
        assert_eq!(body["next_after_seq"], 2);
        // An entry is the OutBatch's fields at the top level, plus the command.
        let genesis = &body["entries"][0];
        assert_eq!(genesis["kind"], "Genesis");
        assert_eq!(genesis["command"], Value::Null, "Genesis has no client command");
        assert_eq!(genesis["origin"], Value::Null);
        let deposit = &body["entries"][1];
        assert_eq!(
            deposit["command"],
            json!({"actor": OPERATOR.0, "nonce": 1,
                   "action": {"Deposit": {"to": ALICE.0, "amount": 1_000_000_000u64}}})
        );
        assert_eq!(deposit["origin"], json!({"actor": OPERATOR.0, "nonce": 1}));
        for key in ["seq", "time", "kind", "origin", "events", "balances", "command"] {
            assert!(deposit.get(key).is_some(), "{key}");
        }

        // The next pages reach the end, which says null.
        let pages = pages(&read, "/v1/trace?view=admin", 2);
        let seqs: Vec<u64> = pages.iter().flat_map(entry_seqs).collect();
        assert_eq!(
            seqs,
            (1..=log.len() as u64).collect::<Vec<_>>(),
            "all of them, in order"
        );
        assert!(pages.iter().all(|p| entry_seqs(p).len() <= 2));
        assert_eq!(pages.last().unwrap()["next_after_seq"], Value::Null);
        assert!(
            pages[..pages.len() - 1].iter().all(|p| entry_seqs(p).len() == 2),
            "only the last page may be short"
        );

        // `after_seq` is exclusive; one page holds all of it by default; a page that ends exactly
        // at the end has nothing after it.
        let body = ok(&read, "/v1/trace?view=admin&after_seq=12");
        assert_eq!(entry_seqs(&body), [13, 14]);
        assert_eq!(body["next_after_seq"], Value::Null);
        let body = ok(&read, "/v1/trace?view=admin&after_seq=12&limit=2");
        assert_eq!(entry_seqs(&body), [13, 14]);
        assert_eq!(
            body["next_after_seq"],
            Value::Null,
            "a full page with nothing after it"
        );
        let body = ok(&read, "/v1/trace?view=admin&after_seq=14");
        assert_eq!(body["entries"], json!([]));
        assert_eq!(body["next_after_seq"], Value::Null);

        // The trace is the batches the node published; a `limit` over the maximum is the maximum.
        assert_eq!(
            entry_seqs(&ok(&read, "/v1/trace?view=admin&limit=5000")).len(),
            log.len()
        );
    }

    /// Nothing of most of this journal is for MM1: deposits to the others, and MM2's quote.
    fn noisy_log() -> Vec<SequencedCommand> {
        let mut h = Harness::new(demo_config());
        h.set_time(T0);
        for _ in 0..20 {
            h.fund(ALICE, 50);
        }
        h.fund(MM1, 1_000);
        for _ in 0..20 {
            h.fund(MM2, 50);
        }
        h.user(
            OPERATOR,
            Action::ListContract {
                id: ContractId(T0),
                description: "Will it rain in Paris on 2026-11-01?".into(),
                oracle: ORACLE,
                trading_close: T0 + 5_000,
                resolution_deadline: T0 + 15_000,
                dispute_window: 2_000,
            },
        );
        for _ in 0..20 {
            h.fund(CAROL, 10);
        }
        let request = RequestId(h.next_seq());
        h.user(
            ALICE,
            Action::SubmitRequest {
                legs: vec![LegSpec {
                    contract: ContractId(T0),
                    side: Side::BuyYes,
                    qty: 1_000,
                    limit_price: price(7_000),
                }],
                response_deadline: T0 + 1_500,
            },
        );
        for (mm, bps) in [(MM2, 6_000), (MM1, 6_200)] {
            h.user(
                mm,
                Action::SubmitQuote {
                    request,
                    leg: 0,
                    price: price(bps),
                    size: 1_000,
                    expires_at: T0 + 10_000,
                },
            );
        }
        for _ in 0..20 {
            h.fund(CAROL, 1);
        }
        h.fund(MM1, 1);
        // Invisible to MM1 again, so that its last visible entry is not the journal's last.
        for _ in 0..5 {
            h.fund(CAROL, 1);
        }
        h.log
    }

    /// The `seq` of `actor`'s first `SubmitQuote` in `log`: the quote's id.
    fn quote_by(log: &[SequencedCommand], actor: AccountId) -> u64 {
        log.iter()
            .find(|sc| {
                matches!(&sc.cmd, Command::User { actor: a, action: Action::SubmitQuote { .. }, .. } if *a == actor)
            })
            .map(|sc| sc.seq)
            .expect("a quote")
    }

    #[test]
    fn trace_for_an_account_skips_what_it_cannot_see_and_still_advances() {
        let log = noisy_log();
        let read = read_side(&log, log.len());
        let mm1 = MM1.0;

        // What MM1's view holds, by the WebSocket's own filter over the trace.
        let visible: Vec<u64> = read.read(|r, _| {
            r.trace_after(0)
                .filter(|e| filter(View::Account(MM1), &e.batch).is_some())
                .map(|e| e.batch.seq)
                .collect()
        });
        assert!(visible.len() >= 5, "{visible:?}");
        assert!(
            visible.len() * 5 < log.len(),
            "most batches are invisible to MM1: {} of {}",
            visible.len(),
            log.len()
        );

        for limit in [1, 2, 3] {
            let pages = pages(&read, &format!("/v1/trace?view={mm1}"), limit);
            let seqs: Vec<u64> = pages.iter().flat_map(entry_seqs).collect();
            assert_eq!(seqs, visible, "limit {limit}: exactly what MM1 may see, in order");
            for (i, page) in pages.iter().enumerate() {
                let n = entry_seqs(page).len();
                assert!(n >= 1, "limit {limit}: page {i} is empty while more is visible");
                let last = i + 1 == pages.len();
                // A page that is not the last is full and points at its last entry.
                assert_eq!(n == limit && !last, page["next_after_seq"].is_u64(), "page {i}");
                if !last {
                    assert_eq!(n, limit);
                    assert_eq!(page["next_after_seq"], *entry_seqs(page).last().unwrap());
                }
            }
            assert_eq!(pages.last().unwrap()["next_after_seq"], Value::Null);
        }

        // From the middle of a long invisible stretch, the first page is the next visible entry.
        let after = visible[1] + 1; // inside the deposits to MM2
        let body = ok(&read, &format!("/v1/trace?view={mm1}&after_seq={after}&limit=1"));
        let next_visible = *visible.iter().find(|s| **s > after).unwrap();
        assert!(next_visible > after + 1, "a gap to cross");
        assert_eq!(entry_seqs(&body), [next_visible]);
        // After the last visible entry: empty and done, though invisible batches follow it.
        let last = *visible.last().unwrap();
        assert!(
            last < log.len() as u64,
            "invisible batches follow the last visible one"
        );
        let body = ok(&read, &format!("/v1/trace?view={mm1}&after_seq={last}"));
        assert_eq!(body["entries"], json!([]));
        assert_eq!(body["next_after_seq"], Value::Null);

        // Sealing: no MM2 quote, nobody else's balance, a command only on MM1's own batches.
        let everything = ok(&read, &format!("/v1/trace?view={mm1}"));
        let entries = everything["entries"].as_array().unwrap();
        assert_eq!(entries.len(), visible.len());
        let (mm1_quote, mm2_quote) = (quote_by(&log, MM1), quote_by(&log, MM2));
        assert!(
            !everything.to_string().contains(&format!("\"quote\":{mm2_quote}")),
            "nothing names MM2's quote"
        );
        let (mut quotes, mut own) = (0, 0);
        for e in entries {
            for ev in e["events"].as_array().unwrap() {
                if let Some(q) = ev["event"].get("QuoteSubmitted") {
                    quotes += 1;
                    assert_eq!(q["quote"], mm1_quote, "only MM1's own quote");
                }
            }
            for b in e["balances"].as_array().unwrap() {
                assert_eq!(b["account"], mm1, "only MM1's balance");
            }
            match (&e["origin"], &e["command"]) {
                (Value::Null, Value::Null) => {}
                (origin, command) => {
                    own += 1;
                    assert_eq!(origin["actor"], mm1);
                    assert_eq!(command["actor"], mm1, "a command only on MM1's own batches");
                }
            }
        }
        assert_eq!((quotes, own), (1, 1), "MM1's quote, its only command");
        // The operator's deposit to MM1 reaches MM1's view, but not as MM1's command.
        let deposit = entries
            .iter()
            .find(|e| e["seq"] == 22)
            .expect("the deposit to MM1");
        assert_eq!(deposit["command"], Value::Null);
        assert_eq!(deposit["origin"], Value::Null);
        assert_eq!(deposit["balances"].as_array().unwrap().len(), 1);

        // The admin view of the same journal has it all, with every command.
        let admin: BTreeSet<u64> = pages(&read, "/v1/trace?view=admin", 1000)
            .iter()
            .flat_map(entry_seqs)
            .collect();
        assert_eq!(admin.len(), log.len());
    }

    #[test]
    fn an_account_sees_its_own_command_and_the_admin_sees_them_all() {
        let log = happy_log();
        let read = read_side(&log, log.len());
        let alice = ok(&read, &format!("/v1/trace?view={}", ALICE.0));
        let commands: Vec<&Value> = alice["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| &e["command"])
            .filter(|c| !c.is_null())
            .collect();
        assert_eq!(commands.len(), 2, "her request and her accept");
        assert!(commands.iter().all(|c| c["actor"] == ALICE.0));
        assert_eq!(
            commands[0]["action"]["SubmitRequest"]["legs"][0]["limit_price"],
            7_000
        );

        let admin = ok(&read, "/v1/trace?view=admin");
        let actors: BTreeSet<u64> = admin["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["command"]["actor"].as_u64())
            .collect();
        assert_eq!(
            actors,
            BTreeSet::from([OPERATOR.0, ALICE.0, MM1.0, MM2.0, ORACLE.0])
        );
    }

    // ---- min_seq ----

    #[test]
    fn min_seq_waits_then_answers() {
        let log = happy_log();
        let read = read_side(&log, 9);
        let writer = {
            let (read, next) = (Arc::clone(&read), log[9].clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                read.apply(&next).unwrap();
            })
        };
        let started = Instant::now();
        let (status, body) = call(&read, &EngineStatus::new(), "/v1/status?min_seq=10", MIN_SEQ_WAIT);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["as_of_seq"], 10);
        assert!(
            started.elapsed() >= Duration::from_millis(40),
            "it waited for the write"
        );
        assert!(started.elapsed() < MIN_SEQ_WAIT);
        writer.join().unwrap();

        // Already there: no wait. Zero: always there.
        let started = Instant::now();
        ok(&read, "/v1/status?min_seq=10");
        ok(&read, "/v1/status?min_seq=3");
        ok(&read, "/v1/accounts/10?min_seq=0");
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn min_seq_times_out_with_503() {
        let log = happy_log();
        let read = read_side(&log, 9);
        let started = Instant::now();
        let (status, body) = call(&read, &EngineStatus::new(), "/v1/accounts/10?min_seq=10", WAIT);
        assert_eq!(status, 503);
        assert_eq!(body, json!({"error": "replica behind", "as_of_seq": 9}));
        assert!(started.elapsed() >= WAIT);
        assert!(started.elapsed() < WAIT * 10);
        // Any read takes it.
        for target in [
            "/v1/contracts?min_seq=99",
            "/v1/requests?state=open&min_seq=99",
            "/v1/trace?view=admin&min_seq=99",
            "/v1/status?min_seq=99",
        ] {
            let (status, _) = call(&read, &EngineStatus::new(), target, WAIT);
            assert_eq!(status, 503, "{target}");
        }
    }

    #[test]
    fn wait_for_is_woken_by_apply_and_replace() {
        let log = happy_log();
        let read = read_side(&log, 3);
        assert!(read.wait_for(3, Duration::ZERO));
        assert!(!read.wait_for(4, Duration::from_millis(10)));

        let waiter = {
            let read = Arc::clone(&read);
            std::thread::spawn(move || read.wait_for(8, MIN_SEQ_WAIT))
        };
        std::thread::sleep(Duration::from_millis(20));
        // A rebuilt replica that is further along wakes it too.
        let mut rebuilt = Replica::new();
        for sc in &log[..8] {
            rebuilt.apply(sc).unwrap();
        }
        read.replace(rebuilt);
        assert!(waiter.join().unwrap());
        assert_eq!(read.last_seq(), 8);
    }

    // ---- a new journal ----

    #[test]
    fn a_new_journal_resets_the_replica() {
        let log = happy_log();
        let read = ReadSide::new();
        // The first journal: Genesis, four deposits, the listing, Alice's request.
        for sc in &log[..7] {
            read.apply_or_reset(sc).unwrap();
        }
        assert_eq!(read.last_seq(), 7);
        assert_eq!(
            ok(&read, "/v1/contracts")["contracts"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            ok(&read, "/v1/requests?state=open")["requests"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // Alice's request holds $700 of her $1,000.
        assert_eq!(
            ok(&read, "/v1/accounts/10")["balance"],
            json!({"free": 300_000_000u64, "reserved": 700_000_000u64, "escrowed": 0})
        );

        // `docker compose down -v`: a fresh journal starts at seq 1 while the replica is at 7.
        let fresh = noisy_log();
        read.apply_or_reset(&fresh[0]).unwrap();
        assert_eq!(read.last_seq(), 1, "the replica starts over");
        let body = ok(&read, "/v1/status");
        assert_eq!(body["as_of_seq"], 1);
        assert_eq!(body["replica"]["last_seq"], 1);
        assert_eq!(
            ok(&read, "/v1/contracts")["contracts"],
            json!([]),
            "no old row is served"
        );
        assert_eq!(ok(&read, "/v1/requests?state=open")["requests"], json!([]));
        assert_eq!(ok(&read, "/v1/accounts/10")["balance"]["free"], 0);
        assert_eq!(
            ok(&read, "/v1/trace?view=admin")["entries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // And it follows the new journal.
        read.apply_or_reset(&fresh[1]).unwrap();
        assert_eq!(read.last_seq(), 2);
        assert_eq!(ok(&read, "/v1/accounts/10")["balance"]["free"], 50_000_000u64);
        // Anything else out of sequence is an error and changes nothing.
        let err = read.apply_or_reset(&fresh[5]).unwrap_err();
        assert_eq!(
            err,
            ReplicaError::OutOfSequence {
                expected: 3,
                found: 6
            }
        );
        assert_eq!(read.last_seq(), 2);
    }

    #[test]
    fn plain_apply_never_resets() {
        let log = happy_log();
        let read = read_side(&log, 5);
        let err = read.apply(&log[0]).unwrap_err();
        assert_eq!(
            err,
            ReplicaError::OutOfSequence {
                expected: 6,
                found: 1
            }
        );
        assert_eq!(read.last_seq(), 5);
        // `apply_or_reset` resets only on seq 1.
        let err = read.apply_or_reset(&log[2]).unwrap_err();
        assert_eq!(
            err,
            ReplicaError::OutOfSequence {
                expected: 6,
                found: 3
            }
        );
        assert_eq!(read.last_seq(), 5);
    }

    #[test]
    fn a_waiter_is_not_fooled_by_a_reset() {
        let log = happy_log();
        let read = read_side(&log, 5);
        read.apply_or_reset(&log[0]).unwrap(); // seq 1 again: a new journal
        assert_eq!(read.last_seq(), 1);
        assert!(!read.wait_for(5, Duration::from_millis(20)), "the old 5 is gone");
        assert!(read.wait_for(1, Duration::ZERO));
    }

    // ---- errors ----

    #[test]
    fn bad_ids_and_values_are_400() {
        let log = happy_log();
        let read = read_side(&log, log.len());
        for target in [
            "/v1/accounts/abc",
            "/v1/accounts/-1",
            "/v1/accounts/+5",
            "/v1/accounts/1.5",
            "/v1/accounts/99999999999999999999999",
            "/v1/accounts/abc/positions",
            "/v1/accounts/abc/requests",
            "/v1/accounts/abc/quotes",
            "/v1/contracts/xyz",
            "/v1/status?min_seq=soon",
            "/v1/status?min_seq=-1",
            "/v1/accounts/10?min_seq=",
            "/v1/requests",
            "/v1/requests?state=filled",
            "/v1/requests?state=",
            "/v1/requests?contract=1",
            "/v1/requests?state=open&contract=abc",
            "/v1/trace",
            "/v1/trace?after_seq=1",
            "/v1/trace?view=bob",
            "/v1/trace?view=",
            "/v1/trace?view=-3",
            "/v1/trace?view=admin&limit=0",
            "/v1/trace?view=admin&limit=many",
            "/v1/trace?view=admin&after_seq=-1",
            "/v1/trace?view=admin&after_seq=x",
        ] {
            let (status, message) = error(&read, target);
            assert_eq!(status, 400, "{target}: {message}");
            assert!(!message.is_empty());
        }
        // A bad value is refused before any wait.
        let started = Instant::now();
        let (status, _) = call(
            &read,
            &EngineStatus::new(),
            "/v1/accounts/x?min_seq=999",
            MIN_SEQ_WAIT,
        );
        assert_eq!(status, 400);
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn unknown_paths_and_contracts_are_404() {
        let log = happy_log();
        let read = read_side(&log, log.len());
        for target in [
            "/",
            "/v1",
            "/v1/",
            "/v2/status",
            "/v1/nope",
            "/v1/accounts",
            "/v1/accounts/10/nope",
            "/v1/accounts/10/positions/extra",
            "/v1/contracts/1/extra",
            "/v1/status/",
            "/status",
            "v1/status",
            "",
        ] {
            let (status, _) = error(&read, target);
            assert_eq!(status, 404, "{target:?}");
        }
        assert_eq!(
            call(&read, &EngineStatus::new(), "/v1/contracts/999", WAIT),
            (404, json!({"error": "no such contract"}))
        );
    }

    #[test]
    fn a_post_only_path_is_405_for_get_and_any_other_method_is_405_or_404() {
        let read = read_side(&happy_log(), 3);
        for target in ["/v1/quotes", "/v1/commands"] {
            let (status, _) = error(&read, target);
            assert_eq!(status, 405, "GET {target}");
        }
        for path in [
            "/v1/status",
            "/v1/accounts/10",
            "/v1/accounts/10/positions",
            "/v1/accounts/abc",
            "/v1/requests",
            "/v1/quotes",
            "/v1/commands",
            "/v1/contracts",
            "/v1/contracts/1",
            "/v1/trace",
        ] {
            let reply = other_method(path);
            assert_eq!(reply.status, 405, "{path}");
            assert_eq!(
                serde_json::from_str::<Value>(&reply.body).unwrap()["error"],
                "method not allowed"
            );
        }
        for path in ["/", "/v1/nope", "/v1/accounts", "/v1/status/"] {
            assert_eq!(other_method(path).status, 404, "{path}");
        }
    }

    // ---- the gateway's probes ----

    /// The gateway's own probes among real traffic: a nonce-sync probe by MM1 (a `Withdraw` of 0,
    /// refused `InvalidInput` after consuming its nonce), the liveness probe (actor 1, nonce 0,
    /// `Duplicate`), a real `Withdraw`, and a probe in whose apply the timers fire (Alice's
    /// request is presented when MM1's probe arrives after its deadline).
    fn probe_log() -> Vec<SequencedCommand> {
        let mut h = Harness::new(demo_config());
        h.set_time(T0);
        for to in [ALICE, MM1, MM2] {
            h.fund(to, 1_000);
        }
        h.user(
            OPERATOR,
            Action::ListContract {
                id: ContractId(T0),
                description: "Will it rain in Paris on 2026-11-01?".into(),
                oracle: ORACLE,
                trading_close: T0 + 5_000,
                resolution_deadline: T0 + 15_000,
                dispute_window: 2_000,
            },
        );
        h.user(MM1, probe_action()); // a nonce-sync probe
        h.submit(Command::User {
            actor: OPERATOR,
            nonce: 0,
            action: probe_action(),
        }); // the liveness probe
        h.user(ALICE, Action::Withdraw { amount: 1_000_000 }); // a real withdrawal
        let request = RequestId(h.next_seq());
        h.user(
            ALICE,
            Action::SubmitRequest {
                legs: vec![LegSpec {
                    contract: ContractId(T0),
                    side: Side::BuyYes,
                    qty: 1_000,
                    limit_price: price(7_000),
                }],
                response_deadline: T0 + 1_500,
            },
        );
        for (mm, bps) in [(MM2, 6_000), (MM1, 6_200)] {
            h.user(
                mm,
                Action::SubmitQuote {
                    request,
                    leg: 0,
                    price: price(bps),
                    size: 1_000,
                    expires_at: T0 + 10_000,
                },
            );
        }
        h.set_time(T0 + 1_600);
        h.user(MM1, probe_action()); // the deadline has passed: the timers fire in this apply
        h.log
    }

    /// The seqs of the probes in `log`, in order.
    fn probe_seqs(log: &[SequencedCommand]) -> Vec<u64> {
        log.iter()
            .filter(|sc| matches!(&sc.cmd, Command::User { action, .. } if *action == probe_action()))
            .map(|sc| sc.seq)
            .collect()
    }

    fn is_probe_command(command: &Value) -> bool {
        command["action"] == json!({"Withdraw": {"amount": 0}})
    }

    fn rejections(entry: &Value) -> Vec<&Value> {
        entry["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event"].get("CommandRejected").is_some())
            .collect()
    }

    #[test]
    fn the_admin_trace_hides_the_gateways_probes() {
        let log = probe_log();
        let read = read_side(&log, log.len());
        let probes = probe_seqs(&log);
        assert_eq!(
            probes.len(),
            3,
            "nonce-sync, liveness, and the one with timers: {probes:?}"
        );
        let (nonce_sync, liveness) = (probes[0], probes[1]);

        // The replica has them as the journal does: an origin, a command, a rejection.
        read.read(|r, _| {
            let raw = |seq: u64| {
                r.trace_after(seq - 1)
                    .next()
                    .filter(|e| e.batch.seq == seq)
                    .unwrap()
            };
            for (seq, reason) in [
                (nonce_sync, rfq_domain::RejectReason::InvalidInput),
                (liveness, rfq_domain::RejectReason::Duplicate),
            ] {
                let entry = raw(seq);
                assert!(
                    entry.batch.origin.is_some() && entry.command.is_some(),
                    "seq {seq}"
                );
                assert!(
                    entry.batch.events.iter().any(|e| matches!(
                        e.event,
                        rfq_domain::Event::CommandRejected { reason: got } if got == reason
                    )),
                    "seq {seq} is rejected {reason:?}"
                );
            }
        });

        let body = ok(&read, "/v1/trace?view=admin&limit=1000");
        let seqs = entry_seqs(&body);
        for quiet in [nonce_sync, liveness] {
            assert!(
                !seqs.contains(&quiet),
                "the probe at seq {quiet} is not in the trace: {seqs:?}"
            );
        }
        // Nothing in the trace has the probe's command, and a probe's rejection is nowhere.
        for entry in body["entries"].as_array().unwrap() {
            assert!(!is_probe_command(&entry["command"]), "seq {}", entry["seq"]);
            for rejected in rejections(entry) {
                assert_ne!(
                    entry["origin"],
                    Value::Null,
                    "a rejection with no origin: {rejected}"
                );
            }
        }
        // Everything else is there, real commands included.
        let expected: Vec<u64> = (1..=log.len() as u64)
            .filter(|s| ![nonce_sync, liveness].contains(s))
            .collect();
        assert_eq!(seqs, expected);
        let withdraw = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["command"]["action"].get("Withdraw").is_some())
            .expect("the real withdrawal");
        assert_eq!(
            withdraw["command"]["action"],
            json!({"Withdraw": {"amount": 1_000_000}})
        );
        assert_eq!(withdraw["origin"]["actor"], ALICE.0);

        // Paging crosses the hidden entries as it crosses the ones a view cannot see.
        let paged: Vec<u64> = pages(&read, "/v1/trace?view=admin", 3)
            .iter()
            .flat_map(entry_seqs)
            .collect();
        assert_eq!(paged, expected);
    }

    #[test]
    fn a_probe_in_whose_apply_a_timer_fired_keeps_the_timers_events_without_origin_or_command() {
        let log = probe_log();
        let read = read_side(&log, log.len());
        let with_timers = *probe_seqs(&log).last().unwrap();

        let raw_balances = read.read(|r, _| {
            let entry = r.trace_after(with_timers - 1).next().unwrap();
            assert_eq!(entry.batch.seq, with_timers);
            assert!(entry.batch.origin.is_some(), "the journal has the probe's origin");
            let balances: Vec<Value> = entry
                .batch
                .balances
                .iter()
                .map(|(account, balance)| json!({"account": account.0, "balance": balance}))
                .collect();
            Value::Array(balances)
        });
        let body = ok(
            &read,
            &format!("/v1/trace?view=admin&after_seq={}", with_timers - 1),
        );
        let entry = &body["entries"][0];
        assert_eq!(entry["seq"], with_timers);
        assert_eq!(entry["origin"], Value::Null);
        assert_eq!(entry["command"], Value::Null);
        assert!(
            rejections(entry).is_empty(),
            "the prober's rejection is removed: {entry}"
        );
        let names: Vec<String> = entry["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                e["event"]
                    .as_object()
                    .map_or_else(|| e["event"].to_string(), |o| o.keys().next().unwrap().clone())
            })
            .collect();
        assert!(
            names.contains(&"RequestPresented".to_owned()),
            "what the timer did stays: {names:?}"
        );
        assert_eq!(entry["balances"], raw_balances, "balances are kept");
    }

    #[test]
    fn the_probers_own_view_does_not_see_the_probe() {
        let log = probe_log();
        let read = read_side(&log, log.len());
        let probes = probe_seqs(&log);
        let (nonce_sync, with_timers) = (probes[0], probes[2]);

        // MM1 sent the nonce-sync probe and the one with timers; neither is MM1's command now.
        let body = ok(&read, &format!("/v1/trace?view={}", MM1.0));
        let seqs = entry_seqs(&body);
        assert!(
            !seqs.contains(&nonce_sync),
            "MM1 does not see its own probe: {seqs:?}"
        );
        for entry in body["entries"].as_array().unwrap() {
            assert!(!is_probe_command(&entry["command"]), "seq {}", entry["seq"]);
            assert!(
                rejections(entry).is_empty(),
                "no rejection for MM1: seq {}",
                entry["seq"]
            );
        }
        // What the timers did to MM1 in the probe's apply is still its to see, with no origin.
        let timers = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["seq"] == with_timers)
            .expect("MM1's quote was lost or selected when the timers fired");
        assert_eq!(timers["origin"], Value::Null);
        assert_eq!(timers["command"], Value::Null);

        // MM1's real commands (its funding is the operator's; its quote is its own) are still its.
        let own: Vec<&Value> = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| !e["command"].is_null())
            .collect();
        assert_eq!(own.len(), 1, "its quote");
        assert!(own[0]["command"]["action"].get("SubmitQuote").is_some());

        // Nor does another account, or the operator who sent the liveness probe, see one.
        for who in [ALICE.0, OPERATOR.0] {
            let body = ok(&read, &format!("/v1/trace?view={who}"));
            for entry in body["entries"].as_array().unwrap() {
                assert!(
                    !is_probe_command(&entry["command"]),
                    "view {who}, seq {}",
                    entry["seq"]
                );
                assert!(
                    rejections(entry)
                        .iter()
                        .all(|r| r["audience"] != json!({"Account": 21}))
                );
            }
        }
        let operator = ok(&read, &format!("/v1/trace?view={}", OPERATOR.0));
        assert!(
            !entry_seqs(&operator).contains(&probes[1]),
            "the liveness probe is not the operator's either"
        );
    }

    #[test]
    fn bodies_are_json_objects_and_the_replica_is_not_touched() {
        let log = happy_log();
        let read = read_side(&log, log.len());
        let before = read.read(|r, _| r.state_hash());
        for target in [
            "/v1/status",
            "/v1/accounts/10",
            "/v1/trace?view=admin",
            "/v1/contracts",
        ] {
            assert!(ok(&read, target).is_object(), "{target}");
        }
        assert_eq!(read.read(|r, _| r.state_hash()), before);
        // The ids are plain numbers, as on the WebSocket.
        let body = ok(&read, "/v1/accounts/10/positions");
        assert!(body["positions"][0]["contract"].is_u64());
        let _ = AccountId(1);
    }
}
