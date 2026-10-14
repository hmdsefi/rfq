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
        "summary": "Funded accounts and one contract with long windows. Drive everything by hand.",
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
        assert_eq!(
            out["snapshot"]["contracts"].as_array().expect("contracts").len(),
            1
        );
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
    }

    #[test]
    fn replay_check_matches_after_manual_play() {
        let mut app = None;
        let request = json!({"SubmitRequest": {"legs": [{"contract": 1000, "side": "BuyYes", "qty": 1000, "limit_price": 7000}], "response_deadline": 2500}});
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
