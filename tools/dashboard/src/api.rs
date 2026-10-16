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
    Reset {
        mode: String,
    },
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
    Act {
        actor: AccountId,
        action: Action,
    },
    Raw {
        actor: AccountId,
        nonce: u64,
        action: Action,
    },
    Advance {
        ms: Duration,
    },
    NextTimer,
    View {
        viewer: Viewer,
    },
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
    // The chosen view stays in place across a reset (admin when there was no session yet).
    let viewer = slot.as_ref().map_or(Viewer::Admin, |app| app.viewer);
    if let Request::Reset { mode } = &req {
        *slot = Some(App::start(mode, viewer)?);
    }
    if slot.is_none() {
        *slot = Some(App::start("sandbox", viewer)?);
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
        // Only a manual op that took effect takes the script over: a refused one changed nothing.
        Request::Advance { ms } => {
            let a = app.session.advance(ms).map_err(|e| e.to_string())?;
            app.manual();
            app.applied.push(a);
        }
        Request::NextTimer => {
            let a = app.session.tick_next_timer().map_err(|e| e.to_string())?;
            app.manual();
            app.applied.push(a);
        }
        Request::View { viewer } => app.viewer = viewer,
        Request::ReplayCheck => replay = Some(app.session.replay_check()),
    }
    let new_seqs = app
        .applied
        .get(before..)
        .unwrap_or_default()
        .iter()
        .map(|a| a.seq)
        .collect();
    app.render(new_seqs, replay)
}

impl App {
    /// A fresh session in `mode`: `"sandbox"` (set up at once) or a preset (loaded, not run),
    /// shown to `viewer`.
    pub fn start(mode: &str, viewer: Viewer) -> Result<App, String> {
        let (session, genesis) = Session::new(demo_config());
        let mut app = App {
            mode: mode.to_string(),
            session,
            applied: vec![genesis],
            runner: None,
            taken_over: false,
            viewer,
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
                verdict: r.verdict().map(|v| Verdict {
                    pass: v.is_ok(),
                    detail: v.err(),
                }),
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
        out["trace"]
            .as_array()
            .and_then(|t| t.last())
            .cloned()
            .expect("a trace entry")
    }

    fn quote(price: u64) -> Value {
        json!({"SubmitQuote": {"request": 6, "leg": 0, "price": price, "size": 1000, "expires_at": 11000}})
    }

    /// Runs `n` scripted steps of the loaded preset.
    fn steps(app: &mut Option<App>, n: usize) -> Value {
        let mut out = Value::Null;
        for _ in 0..n {
            out = call(app, json!({"op": "step"}));
            assert!(out["error"].is_null(), "{out}");
        }
        out
    }

    fn view_as(app: &mut Option<App>, viewer: Value) -> Value {
        let out = call(app, json!({"op": "view", "viewer": viewer}));
        assert!(out["error"].is_null(), "{out}");
        out
    }

    fn array<'a>(v: &'a Value, what: &str) -> &'a Vec<Value> {
        v.as_array()
            .unwrap_or_else(|| panic!("{what} is not an array: {v}"))
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
        // The listed market's close is pending, and admin is allowed to see it.
        assert!(out["snapshot"]["next_timer"].is_u64(), "{out}");
    }

    #[test]
    fn the_catalogue_lists_the_sandbox_and_fourteen_presets() {
        let out = call(&mut None, json!({"op": "presets"}));
        let list = out["presets"].as_array().expect("presets");
        assert_eq!(list.len(), 15);
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
            assert_eq!(
                out["preset"]["verdict"]["pass"], true,
                "{}: {}",
                p["name"], out["preset"]
            );
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
        let out = call(
            &mut app,
            json!({"op": "act", "actor": 10, "action": {"Accept": {"request": id, "picks": []}}}),
        );
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
        assert!(out["snapshot"]["next_timer"].is_null());
        assert_eq!(out["snapshot"]["positions"], json!([]));
        assert_eq!(out["snapshot"]["markets"][0]["positions"], json!([]));
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
        assert!(
            mm1["snapshot"]["quotes"]
                .as_array()
                .expect("quotes")
                .iter()
                .all(|q| q["mm"] == 20)
        );

        let alice = call(&mut app, json!({"op": "view", "viewer": {"account": 10}}));
        assert_eq!(alice["snapshot"]["quotes"], json!([]));
        assert_eq!(alice["snapshot"]["requests"][0]["legs"][0]["limit_price"], 7000);
        let names: Vec<String> = alice["trace"]
            .as_array()
            .expect("trace")
            .iter()
            .flat_map(event_names)
            .collect();
        assert!(!names.iter().any(|n| n == "QuoteSubmitted"), "{names:?}");

        let admin = call(&mut app, json!({"op": "view", "viewer": "admin"}));
        assert!(admin["snapshot"]["total"].is_u64());

        // (a) Nobody else's command is shown: not its body, its actor, nor its nonce. Admin says
        // whose each command was, so this can't pass by there being nothing to hide.
        let by_seq: std::collections::BTreeMap<u64, &Value> = array(&admin["trace"], "trace")
            .iter()
            .map(|e| (e["seq"].as_u64().expect("seq"), e))
            .collect();
        for (viewer, view) in [(20u64, &mm1), (10, &alice)] {
            let mut hidden = 0;
            for entry in array(&view["trace"], "trace") {
                let real = by_seq[&entry["seq"].as_u64().expect("seq")];
                if ["genesis", "tick", "resume"].contains(&real["kind"].as_str().expect("kind")) {
                    continue;
                }
                if real["actor"] == viewer {
                    assert_eq!(entry["actor"], viewer);
                    assert!(entry["command"].is_object() && entry["nonce"].is_u64(), "{entry}");
                } else {
                    hidden += 1;
                    assert_eq!(entry["kind"], "hidden", "viewer {viewer}: {entry}");
                    for field in ["command", "actor", "nonce"] {
                        assert!(entry[field].is_null(), "viewer {viewer} saw {field}: {entry}");
                    }
                }
            }
            assert!(hidden > 0, "viewer {viewer} had nothing hidden from it");
        }

        // (c) MM1 lost, so no position is MM1's and the market's list shows none; Alice sees only
        // hers; admin sees the whole list.
        assert_eq!(mm1["snapshot"]["positions"], json!([]));
        assert_eq!(mm1["snapshot"]["markets"][0]["positions"], json!([]));
        let hers: Vec<&Value> = array(&alice["snapshot"]["positions"], "positions")
            .iter()
            .map(|p| &p["id"])
            .collect();
        assert_eq!(hers.len(), 1);
        let listed: Vec<&Value> = array(&alice["snapshot"]["markets"][0]["positions"], "list")
            .iter()
            .collect();
        assert_eq!(listed, hers);
        assert!(!array(&admin["snapshot"]["markets"][0]["positions"], "list").is_empty());
    }

    #[test]
    fn an_open_request_hides_competing_quotes_and_timers() {
        let mut app = None;
        call(&mut app, json!({"op": "reset", "mode": "happy_path"}));
        // Funds, listing, request, both quotes; the deadline tick has not run.
        let out = steps(&mut app, 8);
        assert_eq!(out["preset"]["next"], "tick");

        let admin = view_as(&mut app, json!("admin"));
        assert!(admin["snapshot"]["next_timer"].is_u64(), "{admin}");
        assert_eq!(array(&admin["snapshot"]["quotes"], "quotes").len(), 2);
        assert_eq!(
            array(&admin["snapshot"]["requests"][0]["legs"][0]["quotes"], "ids").len(),
            2
        );

        let mm1 = view_as(&mut app, json!({"account": 20}));
        let quotes = array(&mm1["snapshot"]["quotes"], "quotes");
        assert_eq!(quotes.len(), 1, "{mm1}");
        assert!(quotes.iter().all(|q| q["mm"] == 20));
        let leg = &mm1["snapshot"]["requests"][0]["legs"][0];
        assert_eq!(leg["quotes"], json!([quotes[0]["id"]]));
        assert!(mm1["snapshot"]["next_timer"].is_null(), "{mm1}");

        let alice = view_as(&mut app, json!({"account": 10}));
        assert_eq!(alice["snapshot"]["quotes"], json!([]));
        assert_eq!(alice["snapshot"]["requests"][0]["legs"][0]["quotes"], json!([]));
        assert_eq!(alice["snapshot"]["requests"][0]["legs"][0]["limit_price"], 7000);
        assert!(alice["snapshot"]["next_timer"].is_null(), "{alice}");
    }

    #[test]
    fn a_counterparty_stays_hidden_until_the_position_settles() {
        let mut app = None;
        call(&mut app, json!({"op": "reset", "mode": "happy_path"}));
        // Through Alice's accept: the position exists but is not settled.
        steps(&mut app, 10);

        let admin = view_as(&mut app, json!("admin"));
        let position = &admin["snapshot"]["positions"][0];
        assert_eq!(
            (&position["yes_holder"], &position["no_holder"]),
            (&json!(10), &json!(21))
        );
        assert_eq!(position["settled"], false);
        let id = position["id"].clone();
        assert_eq!(admin["snapshot"]["markets"][0]["positions"], json!([id]));

        // Alice (YES) and MM2 (NO) each see only themselves; the other side is null.
        let alice = view_as(&mut app, json!({"account": 10}));
        let p = &alice["snapshot"]["positions"][0];
        assert_eq!(
            (&p["id"], &p["yes_holder"], &p["no_holder"]),
            (&id, &json!(10), &Value::Null)
        );
        assert_eq!(p["settled"], false);
        assert!(
            p["yes_lock"].is_u64() && p["no_lock"].is_u64() && p["qty"].is_u64(),
            "{p}"
        );
        assert_eq!(alice["snapshot"]["markets"][0]["positions"], json!([id]));
        let mm2 = view_as(&mut app, json!({"account": 21}));
        let p = &mm2["snapshot"]["positions"][0];
        assert_eq!(
            (&p["id"], &p["yes_holder"], &p["no_holder"]),
            (&id, &Value::Null, &json!(21))
        );
        assert_eq!(mm2["snapshot"]["markets"][0]["positions"], json!([id]));

        // Everyone else learns nothing about the fill: no position, and no id on the market.
        for account in [20, 30, 999] {
            let other = view_as(&mut app, json!({"account": account}));
            assert_eq!(other["snapshot"]["positions"], json!([]), "account {account}");
            assert_eq!(
                other["snapshot"]["markets"][0]["positions"],
                json!([]),
                "account {account}"
            );
        }

        // Once it settles, the engine names both holders to both.
        steps(&mut app, 3);
        for account in [10, 21] {
            let v = view_as(&mut app, json!({"account": account}));
            let p = &v["snapshot"]["positions"][0];
            assert_eq!(
                (&p["yes_holder"], &p["no_holder"]),
                (&json!(10), &json!(21)),
                "{p}"
            );
            assert_eq!(p["settled"], true);
        }
    }

    #[test]
    fn a_failed_manual_op_does_not_take_over_the_preset() {
        let mut app = None;
        call(&mut app, json!({"op": "reset", "mode": "happy_path"}));
        steps(&mut app, 1);
        // Nothing is listed yet, so no timer is pending; and the clock can't overflow.
        let none = call(&mut app, json!({"op": "next_timer"}));
        assert_eq!(none["error"], "no timer is pending");
        let huge = call(&mut app, json!({"op": "advance", "ms": u64::MAX}));
        assert_eq!(huge["error"], "number too large");
        let out = call(&mut app, json!({"op": "step"}));
        assert!(out["error"].is_null(), "{out}");
        assert_eq!(out["preset"]["taken_over"], false);
        assert_eq!(out["preset"]["step"], 2);
    }

    #[test]
    fn reset_keeps_the_viewer() {
        let mut app = None;
        assert_eq!(
            call(&mut app, json!({"op": "reset", "mode": "happy_path"}))["viewer"],
            "admin"
        );
        view_as(&mut app, json!({"account": 10}));
        let out = call(&mut app, json!({"op": "reset", "mode": "no_quotes_expire"}));
        assert_eq!(out["viewer"], json!({"account": 10}));
        assert_eq!(out["mode"], "no_quotes_expire");
        assert_eq!(out["snapshot"]["accounts"].as_array().expect("accounts").len(), 1);
        // A refused reset leaves the session, and so the viewer, as it was.
        let bad = call(&mut app, json!({"op": "reset", "mode": "nope"}));
        assert_eq!(bad["error"], "unknown preset: nope");
        assert_eq!(
            call(&mut app, json!({"op": "state"}))["viewer"],
            json!({"account": 10})
        );
        view_as(&mut app, json!("admin"));
        assert_eq!(
            call(&mut app, json!({"op": "reset", "mode": "sandbox"}))["viewer"],
            "admin"
        );
    }

    #[test]
    fn a_quote_shows_the_hold_the_engine_took_on_the_leg_qty() {
        let mut app = None;
        let request = json!({"SubmitRequest": {"legs": [{"market": 1000, "side": "BuyYes", "qty": 1000, "limit_price": 7000}], "response_deadline": 2500}});
        let opened = call(&mut app, json!({"op": "act", "actor": 10, "action": request}));
        assert_eq!(
            event_names(&last_entry(&opened)),
            vec!["RequestOpened"],
            "{opened}"
        );
        let id = opened["snapshot"]["requests"][0]["id"].clone();
        // Oversized: 2,000 offered on a 1,000 leg. The engine reserves on the leg's qty.
        let quote = json!({"SubmitQuote": {"request": id, "leg": 0, "price": 6000, "size": 2000, "expires_at": 2400}});
        let quoted = call(&mut app, json!({"op": "act", "actor": 20, "action": quote}));
        assert_eq!(
            event_names(&last_entry(&quoted)),
            vec!["QuoteSubmitted"],
            "{quoted}"
        );

        let mm1 = view_as(&mut app, json!({"account": 20}));
        let q = &mm1["snapshot"]["quotes"][0];
        assert_eq!(q["size"], 2000, "{q}");
        // 1,000 × (1 − 0.60) = $400, not 2,000 × (1 − 0.60) = $800.
        assert_eq!(q["reserved"], 400_000_000, "{q}");
        assert_eq!(
            q["reserved"], mm1["snapshot"]["accounts"][0]["balance"]["reserved"],
            "{mm1}"
        );

        // Once the quote ends it holds nothing, and the page has nothing to show.
        let cancel = json!({"CancelQuote": {"quote": q["id"]}});
        let out = call(&mut app, json!({"op": "act", "actor": 20, "action": cancel}));
        assert!(
            event_names(&last_entry(&out)).contains(&"QuoteCancelled".to_string()),
            "{out}"
        );
        let q = &out["snapshot"]["quotes"][0];
        assert!(q.is_object() && q["reserved"].is_null(), "{q}");
        assert_eq!(out["snapshot"]["accounts"][0]["balance"]["reserved"], 0);
    }

    #[test]
    fn replay_check_matches_after_manual_play() {
        let mut app = None;
        let request = json!({"SubmitRequest": {"legs": [{"market": 1000, "side": "BuyYes", "qty": 1000, "limit_price": 7000}], "response_deadline": 2500}});
        let opened = call(&mut app, json!({"op": "act", "actor": 10, "action": request}));
        assert_eq!(
            event_names(&last_entry(&opened)),
            vec!["RequestOpened"],
            "{opened}"
        );
        call(&mut app, json!({"op": "advance", "ms": 2000}));
        let out = call(&mut app, json!({"op": "replay_check"}));
        assert_eq!(out["replay"]["matches"], true);
        assert_eq!(out["replay"]["live"], out["snapshot"]["state_hash"]);
    }
}
