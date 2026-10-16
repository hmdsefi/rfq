# Task D4: View filter, JSON protocol, FFI

**Goal:** the page's only entry point. It takes one JSON request and returns one JSON response, which holds the state filtered for the current viewer by the engine's own event audiences. It never panics on input. The crate also gets three WASM exports, with no `unsafe` blocks.

**Files:**
- Create: `tools/dashboard/src/view.rs`, `tools/dashboard/src/api.rs`, `tools/dashboard/src/ffi.rs`
- Modify: `tools/dashboard/src/lib.rs` (module list)
- Test: unit tests inside `src/api.rs` and `src/ffi.rs`

**Interfaces:**
- Consumes:
  - D2: `Session`, `Applied`, `ReplayCheck`;
  - D3: `presets::{all, by_name, sandbox, play, Runner, PresetKind, T0}`;
  - `rfq_testkit::scenarios::{Ctx, demo_config}`, `rfq_testkit::accounts::*`.
- Produces, used by D5–D7:
  - Rust: `api::handle(&mut Option<App>, &str) -> String` and `view::{Viewer, Snapshot, TraceEntry, snapshot, trace, hex, PARTICIPANTS}`.
  - The WASM ABI:
    - `in_buf(len: u32) -> *mut u8`
    - `handle() -> u32`
    - `out_ptr() -> *const u8`
    - plus the default `memory` export.
  - **The JSON protocol (the contract the page codes against):**
    - Requests (`op` is snake_case):
      - `{"op":"presets"}`
      - `{"op":"reset","mode":"sandbox"|<preset>}`
      - `{"op":"state"}`
      - `{"op":"step"}`
      - `{"op":"run_all"}`
      - `{"op":"take_over"}`
      - `{"op":"act","actor":20,"action":{"SubmitQuote":{"request":7,"leg":0,"price":6000,"size":1000,"expires_at":11000}}}`
      - `{"op":"raw","actor":20,"nonce":3,"action":{…}}`
      - `{"op":"advance","ms":1000}`
      - `{"op":"next_timer"}`
      - `{"op":"view","viewer":"admin"|{"account":10}}`
      - `{"op":"replay_check"}`
    - Actions use the engine's serde shape (externally tagged). Ids are plain numbers, prices are bps (1..=9999), and amounts are micro-USD.
    - `presets` returns `{"presets":[{"name","kind":"sandbox"|"brief"|"attack","summary","steps","expect"}]}`.
    - Every other op returns `{"mode","viewer","snapshot","trace","preset","new_seqs","replay"?}`:
      - `snapshot`: `{now, seq, state_hash, invariants:{ok,error}, next_timer, total, accounts:[{id,name,role,balance:{free,reserved,escrowed}}], requests:[{id,requester,state,public_state,response_deadline,legs:[{market,side,qty,limit_price,quotes,selected}],picks}], quotes:[Quote], markets:[Market], positions:[Position]}`
      - `trace`: `[{seq,time,kind,actor,nonce,command,events:[{audience,event}]}]`
      - `preset`: `null` or `{name,kind,summary,step,steps,next,finished,taken_over,expect,verdict:null|{pass,detail}}`
      - `new_seqs`: the seqs this request appended.
      - `replay` (only for `replay_check`): `{commands,live,replayed,matches}`
    - Errors return `{"error":"<sentence>"}`.

## Steps

- [ ] **Step 1: Write `src/view.rs`.** These are pure functions, and D4's API tests exercise them.

```rust
//! What the page renders: snapshot and trace DTOs, filtered for the current viewer with the
//! engine's own event audiences (engine.md §7), which is how quotes stay sealed.
use serde::{Deserialize, Serialize};

use rfq_domain::*;
use rfq_testkit::accounts::*;

use crate::session::{Applied, Session};

/// Who the page shows the world to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Viewer {
    Admin,
    Account(AccountId),
}

impl Viewer {
    /// Whether an event with this audience reaches the viewer.
    pub fn sees(self, audience: &Audience) -> bool {
        match (self, audience) {
            (Viewer::Admin, _) | (_, Audience::Public) => true,
            (Viewer::Account(me), Audience::Account(to)) => me == *to,
        }
    }

    /// Whether the viewer may see `owner`'s private data: their own, or everything as admin.
    pub fn owns(self, owner: AccountId) -> bool {
        match self {
            Viewer::Admin => true,
            Viewer::Account(me) => me == owner,
        }
    }
}

/// Every account the demo uses: id, display name, role.
pub const PARTICIPANTS: [(AccountId, &str, &str); 8] = [
    (OPERATOR, "Operator", "operator"),
    (ARBITER, "Arbiter", "arbiter"),
    (TREASURY, "Treasury", "treasury"),
    (ORACLE, "Oracle", "oracle"),
    (ALICE, "Alice", "requester"),
    (CAROL, "Carol", "requester"),
    (MM1, "MM1", "market_maker"),
    (MM2, "MM2", "market_maker"),
];

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub now: Timestamp,
    pub seq: u64,
    pub state_hash: String,
    pub invariants: Health,
    pub next_timer: Option<Timestamp>,
    /// Σ (free + reserved + escrowed) over the demo's accounts, for admin only (I1).
    pub total: Option<Amount>,
    pub accounts: Vec<AccountView>,
    pub requests: Vec<RequestView>,
    pub quotes: Vec<Quote>,
    pub markets: Vec<Market>,
    pub positions: Vec<Position>,
}

#[derive(Debug, Serialize)]
pub struct Health {
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AccountView {
    pub id: AccountId,
    pub name: &'static str,
    pub role: &'static str,
    pub balance: Balance,
}

#[derive(Debug, Serialize)]
pub struct RequestView {
    pub id: RequestId,
    /// `None` unless the viewer is the requester or admin.
    pub requester: Option<AccountId>,
    /// The full state for the requester and admin; `None` for everyone else.
    pub state: Option<RequestState>,
    /// `open` while quotes are taken, else `closed`: all a non-owner learns.
    pub public_state: &'static str,
    pub response_deadline: Timestamp,
    pub legs: Vec<LegView>,
    /// The presented picks, for the requester and admin while `Presented`.
    pub picks: Vec<Pick>,
}

#[derive(Debug, Serialize)]
pub struct LegView {
    pub market: MarketId,
    pub side: Side,
    pub qty: Qty,
    /// The sealed limit: requester and admin only.
    pub limit_price: Option<Price>,
    /// Quotes on this leg the viewer may see: their own, or all for admin.
    pub quotes: Vec<QuoteId>,
    pub selected: Option<QuoteId>,
}

#[derive(Debug, Serialize)]
pub struct TraceEntry {
    pub seq: u64,
    pub time: Timestamp,
    /// `genesis`, `tick`, `resume`, the action name (`SubmitQuote`, …), or `hidden`.
    pub kind: &'static str,
    pub actor: Option<AccountId>,
    pub nonce: Option<u64>,
    /// The full command, when the viewer may see it.
    pub command: Option<Command>,
    pub events: Vec<Emitted>,
}

pub fn snapshot(s: &Session, viewer: Viewer) -> Snapshot {
    let e = s.engine();
    let invariants = match e.check_invariants() {
        Ok(()) => Health { ok: true, error: None },
        Err(error) => Health { ok: false, error: Some(error) },
    };
    let positions = e
        .markets()
        .flat_map(|c| c.positions.iter())
        .filter_map(|id| e.position(*id))
        .filter(|p| viewer.owns(p.yes_holder) || viewer.owns(p.no_holder))
        .cloned()
        .collect();
    Snapshot {
        now: s.now(),
        seq: s.next_seq().saturating_sub(1),
        state_hash: hex(&e.state_hash()),
        invariants,
        next_timer: e.next_timer(),
        total: if viewer == Viewer::Admin { ledger_total(e) } else { None },
        accounts: PARTICIPANTS
            .iter()
            .filter(|(id, ..)| viewer.owns(*id))
            .map(|&(id, name, role)| AccountView { id, name, role, balance: e.balance(id) })
            .collect(),
        requests: e.requests().map(|r| request_view(e, r, viewer)).collect(),
        quotes: e.quotes().filter(|q| viewer.owns(q.mm)).cloned().collect(),
        markets: e.markets().cloned().collect(),
        positions,
    }
}

pub fn trace(applied: &[Applied], viewer: Viewer) -> Vec<TraceEntry> {
    applied
        .iter()
        .filter_map(|a| {
            let user = match &a.cmd {
                Command::User { actor, nonce, .. } => Some((*actor, *nonce)),
                _ => None,
            };
            let visible = user.is_none_or(|(actor, _)| viewer.owns(actor));
            let events: Vec<Emitted> =
                a.events.iter().filter(|e| viewer.sees(&e.audience)).cloned().collect();
            // Non-admins don't see other people's commands, nor clock ticks that told them nothing.
            let quiet_system = user.is_none() && viewer != Viewer::Admin;
            if events.is_empty() && (!visible || quiet_system) {
                return None;
            }
            Some(TraceEntry {
                seq: a.seq,
                time: a.time,
                kind: if visible { kind(&a.cmd) } else { "hidden" },
                actor: if visible { user.map(|(actor, _)| actor) } else { None },
                nonce: if visible { user.map(|(_, nonce)| nonce) } else { None },
                command: visible.then(|| a.cmd.clone()),
                events,
            })
        })
        .collect()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn ledger_total(e: &Engine) -> Option<Amount> {
    PARTICIPANTS.iter().try_fold(0u64, |acc, (id, ..)| {
        let b = e.balance(*id);
        acc.checked_add(b.free)?.checked_add(b.reserved)?.checked_add(b.escrowed)
    })
}

fn request_view(e: &Engine, r: &Request, viewer: Viewer) -> RequestView {
    let mine = viewer.owns(r.requester);
    let sees_quote = |q: &QuoteId| e.quote(*q).is_some_and(|q| viewer.owns(q.mm));
    let legs = r
        .legs
        .iter()
        .map(|leg| LegView {
            market: leg.market,
            side: leg.side,
            qty: leg.qty,
            limit_price: mine.then_some(leg.limit_price),
            quotes: leg.quotes.iter().copied().filter(|q| sees_quote(q)).collect(),
            selected: leg.selected.filter(|q| mine || sees_quote(q)),
        })
        .collect();
    let picks = match r.state {
        RequestState::Presented { .. } if mine => r
            .legs
            .iter()
            .enumerate()
            .filter_map(|(i, leg)| {
                let q = e.quote(leg.selected?)?;
                Some(Pick { leg: u32::try_from(i).ok()?, quote: q.id, price: q.price })
            })
            .collect(),
        _ => Vec::new(),
    };
    RequestView {
        id: r.id,
        requester: mine.then_some(r.requester),
        state: mine.then_some(r.state),
        public_state: if r.state == RequestState::Open { "open" } else { "closed" },
        response_deadline: r.response_deadline,
        legs,
        picks,
    }
}

fn kind(cmd: &Command) -> &'static str {
    match cmd {
        Command::Genesis(_) => "genesis",
        Command::Tick => "tick",
        Command::Resume => "resume",
        Command::User { action, .. } => match action {
            Action::Deposit { .. } => "Deposit",
            Action::Withdraw { .. } => "Withdraw",
            Action::ListMarket { .. } => "ListMarket",
            Action::SubmitRequest { .. } => "SubmitRequest",
            Action::CancelRequest { .. } => "CancelRequest",
            Action::SubmitQuote { .. } => "SubmitQuote",
            Action::CancelQuote { .. } => "CancelQuote",
            Action::Accept { .. } => "Accept",
            Action::Reject { .. } => "Reject",
            Action::ProposeOutcome { .. } => "ProposeOutcome",
            Action::Dispute { .. } => "Dispute",
            Action::Rule { .. } => "Rule",
        },
    }
}
```

- [ ] **Step 2: Write the failing API tests.** Create `src/api.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(app: &mut Option<App>, req: Value) -> Value {
        serde_json::from_str(&handle(app, &req.to_string())).expect("response is JSON")
    }

    /// Event names of one trace entry (`"Initialized"`, `"CommandRejected"`, …).
    fn event_names(entry: &Value) -> Vec<String> {
        entry["events"]
            .as_array()
            .expect("events")
            .iter()
            .map(|e| match &e["event"] {
                Value::String(s) => s.clone(),
                Value::Object(m) => m.keys().next().cloned().unwrap_or_default(),
                _ => String::new(),
            })
            .collect()
    }

    fn last_entry(out: &Value) -> Value {
        out["trace"].as_array().and_then(|t| t.last()).cloned().expect("a trace entry")
    }

    fn quote(price: u64) -> Value {
        json!({"SubmitQuote": {"request": 6, "leg": 0, "price": price, "size": 1000, "expires_at": 11000}})
    }

    #[test]
    fn unknown_op_is_an_error() {
        assert!(call(&mut None, json!({"op": "fly"}))["error"].is_string());
    }

    #[test]
    fn malformed_json_is_an_error() {
        let out: Value = serde_json::from_str(&handle(&mut None, "{not json")).expect("JSON");
        assert!(out["error"].as_str().expect("error").starts_with("bad request"));
    }

    #[test]
    fn reset_to_an_unknown_preset_is_an_error() {
        let out = call(&mut None, json!({"op": "reset", "mode": "nope"}));
        assert_eq!(out["error"], "unknown preset: nope");
    }

    #[test]
    fn the_first_call_starts_the_sandbox() {
        let out = call(&mut None, json!({"op": "state"}));
        assert_eq!(out["mode"], "sandbox");
        assert_eq!(out["snapshot"]["accounts"].as_array().expect("accounts").len(), 8);
        assert_eq!(out["snapshot"]["markets"].as_array().expect("markets").len(), 1);
        assert!(out["preset"].is_null());
        assert_eq!(out["snapshot"]["invariants"]["ok"], true);
    }

    #[test]
    fn the_catalogue_lists_the_sandbox_and_twelve_presets() {
        let out = call(&mut None, json!({"op": "presets"}));
        let list = out["presets"].as_array().expect("presets");
        assert_eq!(list.len(), 13);
        assert_eq!(list[0]["kind"], "sandbox");
    }

    #[test]
    fn run_all_passes_every_preset() {
        let mut app = None;
        let catalogue = call(&mut app, json!({"op": "presets"}));
        for p in catalogue["presets"].as_array().expect("presets") {
            if p["kind"] == "sandbox" {
                continue;
            }
            call(&mut app, json!({"op": "reset", "mode": p["name"]}));
            let out = call(&mut app, json!({"op": "run_all"}));
            assert_eq!(out["preset"]["verdict"]["pass"], true, "{}: {}", p["name"], out["preset"]);
        }
    }

    #[test]
    fn price_out_of_range_is_an_error() {
        let mut app = None;
        let out = call(&mut app, json!({"op": "act", "actor": 20, "action": quote(0)}));
        assert!(out["error"].as_str().expect("error").contains("price"), "{out}");
    }

    #[test]
    fn accept_on_expired_request_is_a_rejection() {
        let mut app = None;
        call(&mut app, json!({"op": "reset", "mode": "no_quotes_expire"}));
        let done = call(&mut app, json!({"op": "run_all"}));
        let id = done["snapshot"]["requests"][0]["id"].clone();
        let out = call(&mut app, json!({"op": "act", "actor": 10, "action": {"Accept": {"request": id, "picks": []}}}));
        assert!(out["error"].is_null(), "{out}");
        assert_eq!(event_names(&last_entry(&out)), vec!["CommandRejected"]);
    }

    #[test]
    fn manual_op_mid_preset_takes_over() {
        let mut app = None;
        call(&mut app, json!({"op": "reset", "mode": "happy_path"}));
        call(&mut app, json!({"op": "step"}));
        let out = call(&mut app, json!({"op": "advance", "ms": 100}));
        assert_eq!(out["preset"]["taken_over"], true);
        assert!(call(&mut app, json!({"op": "step"}))["error"].is_string());
    }

    #[test]
    fn step_without_a_preset_is_an_error() {
        assert!(call(&mut None, json!({"op": "step"}))["error"].is_string());
    }

    #[test]
    fn advance_overflow_is_an_error() {
        let out = call(&mut None, json!({"op": "advance", "ms": u64::MAX}));
        assert_eq!(out["error"], "number too large");
    }

    #[test]
    fn raw_with_max_nonce_does_not_trap() {
        let mut app = None;
        let out = call(
            &mut app,
            json!({"op": "raw", "actor": 20, "nonce": u64::MAX, "action": {"Withdraw": {"amount": 1}}}),
        );
        assert!(out["error"].is_null(), "{out}");
        assert_eq!(event_names(&last_entry(&out)), vec!["CommandRejected"]);
    }

    #[test]
    fn view_as_unknown_account_is_empty() {
        let out = call(&mut None, json!({"op": "view", "viewer": {"account": 999}}));
        assert!(out["error"].is_null(), "{out}");
        assert_eq!(out["snapshot"]["accounts"], json!([]));
        assert_eq!(out["snapshot"]["quotes"], json!([]));
        assert!(out["snapshot"]["total"].is_null());
    }

    #[test]
    fn views_hide_what_the_audiences_hide() {
        let mut app = None;
        call(&mut app, json!({"op": "reset", "mode": "happy_path"}));
        call(&mut app, json!({"op": "run_all"}));

        let mm1 = call(&mut app, json!({"op": "view", "viewer": {"account": 20}}));
        for entry in mm1["trace"].as_array().expect("trace") {
            for e in entry["events"].as_array().expect("events") {
                let a = &e["audience"];
                assert!(a == "Public" || *a == json!({"Account": 20}), "MM1 saw {e}");
            }
        }
        let leg = &mm1["snapshot"]["requests"][0]["legs"][0];
        assert!(leg["limit_price"].is_null());
        assert!(mm1["snapshot"]["requests"][0]["requester"].is_null());
        assert!(mm1["snapshot"]["quotes"].as_array().expect("quotes").iter().all(|q| q["mm"] == 20));

        let alice = call(&mut app, json!({"op": "view", "viewer": {"account": 10}}));
        assert_eq!(alice["snapshot"]["quotes"], json!([]));
        assert_eq!(alice["snapshot"]["requests"][0]["legs"][0]["limit_price"], 7000);
        let names: Vec<String> = alice["trace"].as_array().expect("trace").iter().flat_map(event_names).collect();
        assert!(!names.iter().any(|n| n == "QuoteSubmitted"), "{names:?}");

        let admin = call(&mut app, json!({"op": "view", "viewer": "admin"}));
        assert!(admin["snapshot"]["total"].is_u64());
    }

    #[test]
    fn replay_check_matches_after_manual_play() {
        let mut app = None;
        let request = json!({"SubmitRequest": {"legs": [{"market": 1000, "side": "BuyYes", "qty": 1000, "limit_price": 7000}], "response_deadline": 2500}});
        let opened = call(&mut app, json!({"op": "act", "actor": 10, "action": request}));
        assert_eq!(event_names(&last_entry(&opened)), vec!["RequestOpened"], "{opened}");
        call(&mut app, json!({"op": "advance", "ms": 2000}));
        let out = call(&mut app, json!({"op": "replay_check"}));
        assert_eq!(out["replay"]["matches"], true);
        assert_eq!(out["replay"]["live"], out["snapshot"]["state_hash"]);
    }
}
```

> Note on `the_first_call_starts_the_sandbox` and `replay_check_matches_after_manual_play`: the sandbox runs Genesis, then 4 deposits and 1 listing at `T0 = 1000`, so the market id is `1000` and Alice's request is seq 7. If `RequestOpened` is not the only event (for example the engine also reports the hold), assert `contains` instead and note it in the report. Never weaken an assertion about what a viewer must *not* see.

- [ ] **Step 3: Run the tests and watch them fail.**

Run: `cd tools/dashboard && cargo test api`
Expected: compile errors (`handle` and `App` are missing).

- [ ] **Step 4: Implement `src/api.rs`** above the tests:

```rust
//! The page's single entry point: one JSON request in, one JSON response out. It never panics on
//! input; anything wrong comes back as `{"error": "<message>"}`.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use rfq_domain::*;
use rfq_testkit::scenarios::{Ctx, demo_config};

use crate::presets::{self, PresetKind, Runner, T0};
use crate::session::{Applied, ReplayCheck, Session};
use crate::view::{self, Viewer};

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// A fresh session: `"sandbox"` or a preset name.
    Reset { mode: String },
    /// The preset catalogue.
    Presets,
    /// The current state, unchanged.
    State,
    /// The loaded preset's next step.
    Step,
    /// Every remaining step of the loaded preset.
    RunAll,
    /// Stop the script and continue by hand from here.
    TakeOver,
    Act { actor: AccountId, action: Action },
    Raw { actor: AccountId, nonce: u64, action: Action },
    Advance { ms: Duration },
    NextTimer,
    View { viewer: Viewer },
    ReplayCheck,
}

/// Everything one page session holds.
pub struct App {
    mode: String,
    session: Session,
    applied: Vec<Applied>,
    runner: Option<Runner>,
    taken_over: bool,
    viewer: Viewer,
}

#[derive(Serialize)]
struct Out<'a> {
    mode: &'a str,
    viewer: Viewer,
    snapshot: view::Snapshot,
    trace: Vec<view::TraceEntry>,
    preset: Option<PresetStatus>,
    new_seqs: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replay: Option<ReplayOut>,
}

#[derive(Serialize)]
struct PresetStatus {
    name: &'static str,
    kind: PresetKind,
    summary: &'static str,
    step: usize,
    steps: usize,
    next: Option<&'static str>,
    finished: bool,
    taken_over: bool,
    expect: Vec<&'static str>,
    verdict: Option<Verdict>,
}

#[derive(Serialize)]
struct Verdict {
    pass: bool,
    detail: Option<String>,
}

#[derive(Serialize)]
struct ReplayOut {
    commands: usize,
    live: String,
    replayed: String,
    matches: bool,
}

pub fn handle(app: &mut Option<App>, input: &str) -> String {
    let result = serde_json::from_str::<Request>(input)
        .map_err(|e| format!("bad request: {e}"))
        .and_then(|req| dispatch(app, req));
    match result {
        Ok(value) => value,
        Err(error) => json!({ "error": error }),
    }
    .to_string()
}

fn dispatch(slot: &mut Option<App>, req: Request) -> Result<Value, String> {
    if let Request::Presets = req {
        return Ok(json!({ "presets": catalogue() }));
    }
    if let Request::Reset { mode } = &req {
        *slot = Some(App::start(mode)?);
    }
    if slot.is_none() {
        *slot = Some(App::start("sandbox")?);
    }
    let app = slot.as_mut().ok_or_else(|| "no session".to_string())?;
    let before = app.applied.len();
    let mut replay = None;
    match req {
        Request::Presets | Request::Reset { .. } | Request::State => {}
        Request::Step => {
            if !app.step()? {
                return Err("the preset has finished; reset to run it again".into());
            }
        }
        Request::RunAll => while app.step()? {},
        Request::TakeOver => app.taken_over = app.runner.is_some(),
        Request::Act { actor, action } => {
            app.manual();
            let a = app.session.act(actor, action);
            app.applied.push(a);
        }
        Request::Raw { actor, nonce, action } => {
            app.manual();
            let a = app.session.raw(actor, nonce, action);
            app.applied.push(a);
        }
        Request::Advance { ms } => {
            app.manual();
            let a = app.session.advance(ms).map_err(|e| e.to_string())?;
            app.applied.push(a);
        }
        Request::NextTimer => {
            app.manual();
            let a = app.session.tick_next_timer().map_err(|e| e.to_string())?;
            app.applied.push(a);
        }
        Request::View { viewer } => app.viewer = viewer,
        Request::ReplayCheck => replay = Some(app.session.replay_check()),
    }
    let new_seqs = app.applied.get(before..).unwrap_or_default().iter().map(|a| a.seq).collect();
    app.render(new_seqs, replay)
}

impl App {
    /// A fresh session in `mode`: `"sandbox"` (set up at once) or a preset (loaded, not run).
    pub fn start(mode: &str) -> Result<App, String> {
        let (session, genesis) = Session::new(demo_config());
        let mut app = App {
            mode: mode.to_string(),
            session,
            applied: vec![genesis],
            runner: None,
            taken_over: false,
            viewer: Viewer::Admin,
        };
        if mode == "sandbox" {
            let mut ctx = Ctx::new(T0);
            for script in presets::sandbox() {
                let a = presets::play(&mut app.session, &mut ctx, &script).map_err(|e| e.to_string())?;
                app.applied.push(a);
            }
        } else {
            let preset = presets::by_name(mode).ok_or_else(|| format!("unknown preset: {mode}"))?;
            app.runner = Some(Runner::new(preset));
        }
        Ok(app)
    }

    /// Runs the next scripted step; `Ok(false)` when the script is already finished.
    fn step(&mut self) -> Result<bool, String> {
        if self.taken_over {
            return Err("the script was taken over; reset to run it again".into());
        }
        let runner = self
            .runner
            .as_mut()
            .ok_or_else(|| "no preset is loaded; pick one first".to_string())?;
        match runner.step(&mut self.session).map_err(|e| e.to_string())? {
            Some(a) => {
                self.applied.push(a);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// A manual command while a script is still running takes over: the script can't resume,
    /// because the presenter may have moved the clock past its next step.
    fn manual(&mut self) {
        if self.runner.as_ref().is_some_and(|r| !r.finished()) {
            self.taken_over = true;
        }
    }

    fn render(&self, new_seqs: Vec<u64>, replay: Option<ReplayCheck>) -> Result<Value, String> {
        let preset = self.runner.as_ref().map(|r| {
            let p = r.preset();
            PresetStatus {
                name: p.name,
                kind: p.kind,
                summary: p.summary,
                step: r.position(),
                steps: p.steps.len(),
                next: if self.taken_over { None } else { r.next_label() },
                finished: r.finished(),
                taken_over: self.taken_over,
                expect: p.expect.clone(),
                verdict: r.verdict().map(|v| Verdict { pass: v.is_ok(), detail: v.err() }),
            }
        });
        let out = Out {
            mode: &self.mode,
            viewer: self.viewer,
            snapshot: view::snapshot(&self.session, self.viewer),
            trace: view::trace(&self.applied, self.viewer),
            preset,
            new_seqs,
            replay: replay.map(|c| ReplayOut {
                commands: c.commands,
                live: view::hex(&c.live),
                replayed: view::hex(&c.replayed),
                matches: c.matches(),
            }),
        };
        serde_json::to_value(out).map_err(|e| format!("could not encode the state: {e}"))
    }
}

fn catalogue() -> Value {
    let mut list = vec![json!({
        "name": "sandbox",
        "kind": "sandbox",
        "summary": "Funded accounts and one market with long windows. Drive everything by hand.",
        "steps": 0,
        "expect": [],
    })];
    list.extend(presets::all().into_iter().map(|p| {
        json!({ "name": p.name, "kind": p.kind, "summary": p.summary, "steps": p.steps.len(), "expect": p.expect })
    }));
    Value::Array(list)
}
```

- [ ] **Step 5: Write `src/ffi.rs`** (with its test):

```rust
//! The three WASM exports the page calls. The buffers live in thread-locals, so there are no
//! `unsafe` blocks; the only items the `unsafe_code` lint flags are the `#[unsafe(no_mangle)]`
//! export attributes, which `lib.rs` allows for this module alone.
use std::cell::RefCell;

use crate::api::{self, App};

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Sizes the input buffer to `len` bytes; the page writes its JSON request there.
#[unsafe(no_mangle)]
pub extern "C" fn in_buf(len: u32) -> *mut u8 {
    let len = usize::try_from(len).unwrap_or(0);
    INPUT.with(|b| {
        let mut b = b.borrow_mut();
        b.clear();
        b.resize(len, 0);
        b.as_mut_ptr()
    })
}

/// Handles the request in the input buffer. Returns the response length; read it at `out_ptr`.
#[unsafe(no_mangle)]
pub extern "C" fn handle() -> u32 {
    let input = INPUT.with(|b| String::from_utf8(b.borrow().clone()));
    let out = match input {
        Ok(text) => APP.with(|app| api::handle(&mut app.borrow_mut(), &text)),
        Err(_) => r#"{"error":"the request is not UTF-8"}"#.to_string(),
    };
    let len = u32::try_from(out.len()).unwrap_or(0);
    OUTPUT.with(|o| *o.borrow_mut() = out.into_bytes());
    len
}

/// Where the last response starts.
#[unsafe(no_mangle)]
pub extern "C" fn out_ptr() -> *const u8 {
    OUTPUT.with(|o| o.borrow().as_ptr())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(request: &[u8]) -> String {
        let len = u32::try_from(request.len()).expect("small request");
        in_buf(len);
        INPUT.with(|b| b.borrow_mut().copy_from_slice(request));
        let n = usize::try_from(handle()).expect("fits");
        OUTPUT.with(|o| String::from_utf8(o.borrow()[..n].to_vec()).expect("UTF-8"))
    }

    #[test]
    fn handle_reads_the_input_buffer_and_fills_the_output_buffer() {
        let out = roundtrip(br#"{"op":"state"}"#);
        assert!(out.contains(r#""mode":"sandbox""#), "{out}");
    }

    #[test]
    fn invalid_utf8_is_an_error_not_a_trap() {
        assert_eq!(roundtrip(&[0xff, 0xfe]), r#"{"error":"the request is not UTF-8"}"#);
    }
}
```

- [ ] **Step 6: Register the modules.** Make `src/lib.rs`'s module list exactly:

```rust
pub mod api;
#[allow(unsafe_code)] // `#[unsafe(no_mangle)]` export attributes only; no unsafe blocks.
mod ffi;
pub mod presets;
pub mod session;
pub mod view;
```

- [ ] **Step 7: Run the tests and the gates.**

Run: `cd tools/dashboard && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: every D2–D4 test passes and the lints are clean. Then confirm there are no `unsafe` blocks:

Run: `grep -rn "unsafe {" tools/dashboard/src || echo "no unsafe blocks"`
Expected: `no unsafe blocks`

- [ ] **Step 8: Commit.**

```bash
git add tools/dashboard/src/lib.rs tools/dashboard/src/view.rs tools/dashboard/src/api.rs tools/dashboard/src/ffi.rs
git commit -m "feat(dashboard): JSON protocol with audience-filtered views and a 3-function WASM ABI"
```
