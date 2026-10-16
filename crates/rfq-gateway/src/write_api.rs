//! The HTTP write API's pure part (docs/specs/read-api.md §3, §4): the client message a `POST`
//! becomes, and the HTTP reply the hub's frames for that command fold into.
//!
//! Nothing here touches a socket or a thread: the binary's `http_write` opens the short-lived hub
//! connection and feeds what the hub sends back into an [`Answer`].
//!
//! - **Bodies** are JSON objects (an array or a bare value is refused: serde would otherwise read
//!   an array as the fields in order). The top-level fields are exactly those of the spec, and a
//!   field it does not name is refused; the legs and the `action` are the engine's own types, as
//!   on the WebSocket, and keep their own rules.
//! - **The answer** is the batch whose `origin` is `(actor, nonce)`, the nonce being the one the
//!   hub's `sent` frame for this command's id names. Another batch for the same actor, such as an
//!   earlier command's, is never taken: the hub queues one command per account at a time, and
//!   tells every connection with the account's view about each batch.
use rfq_domain::{
    AccountId, Action, Emitted, Event, LegSpec, Price, Qty, RejectReason, RequestId, Timestamp,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::protocol::{AccountBalance, ClientMsg, Origin, OutBatch, ServerMsg};
use crate::read_api::Reply;

/// `POST /v1/requests` (spec §3).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBody {
    actor: AccountId,
    nonce: Option<u64>,
    legs: Vec<LegSpec>,
    response_deadline: Timestamp,
}

/// `POST /v1/quotes`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuoteBody {
    actor: AccountId,
    nonce: Option<u64>,
    request: RequestId,
    leg: u32,
    price: Price,
    size: Qty,
    expires_at: Timestamp,
}

/// `POST /v1/commands`: any client action, in the WebSocket's JSON form. `Genesis`, `Tick` and
/// `Resume` are not an [`Action`], so they cannot be expressed, as on the WebSocket.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandBody {
    actor: AccountId,
    nonce: Option<u64>,
    action: Action,
}

/// Whether `path` is one of the `POST` routes.
pub fn is_write_route(path: &str) -> bool {
    matches!(path, "/v1/requests" | "/v1/quotes" | "/v1/commands")
}

/// A body that is a JSON object, as `T`; `Err` is the `bad request: …` sentence.
fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    if body.trim_ascii_start().first() != Some(&b'{') {
        return Err("bad request: the body must be a JSON object".into());
    }
    serde_json::from_slice(body).map_err(|e| format!("bad request: {e}"))
}

/// The client message for a `POST`: `/v1/requests` → `SubmitRequest`, `/v1/quotes` →
/// `SubmitQuote`, `/v1/commands` → the body's `action`. `id` is the command id to use. `Err` is
/// the `400` sentence.
pub fn command_for(path: &str, body: &[u8], id: &str) -> Result<(AccountId, ClientMsg), String> {
    let (actor, nonce, action) = match path {
        "/v1/requests" => {
            let b: RequestBody = parse(body)?;
            let action = Action::SubmitRequest {
                legs: b.legs,
                response_deadline: b.response_deadline,
            };
            (b.actor, b.nonce, action)
        }
        "/v1/quotes" => {
            let b: QuoteBody = parse(body)?;
            let action = Action::SubmitQuote {
                request: b.request,
                leg: b.leg,
                price: b.price,
                size: b.size,
                expires_at: b.expires_at,
            };
            (b.actor, b.nonce, action)
        }
        "/v1/commands" => {
            let b: CommandBody = parse(body)?;
            (b.actor, b.nonce, b.action)
        }
        _ => return Err(format!("bad request: {path} is not a write route")),
    };
    let command = ClientMsg::Command {
        id: id.to_owned(),
        actor,
        nonce,
        action,
    };
    Ok((actor, command))
}

/// Folds the hub's frames for one command into its HTTP reply. Feed it every frame from the
/// short-lived connection's outbox, in order, until [`on_frame`](Answer::on_frame) returns the
/// reply.
pub struct Answer {
    actor: AccountId,
    /// The command's id on the hub connection.
    id: String,
    /// The nonce the hub gave the command: from its `sent`.
    nonce: Option<u64>,
    /// The batch whose origin is `(actor, nonce)`, its chunks merged.
    batch: Option<OutBatch>,
}

impl Answer {
    pub fn new(actor: AccountId, id: String) -> Answer {
        Answer {
            actor,
            id,
            nonce: None,
            batch: None,
        }
    }

    /// Takes one frame. `Some(reply)` once the answer is complete: the command's `result`, or an
    /// `error` for its id. Every other frame (the `welcome` and `status`, another command's
    /// frames, batches that are not this command's) changes nothing.
    pub fn on_frame(&mut self, frame: &ServerMsg) -> Option<Reply> {
        match frame {
            ServerMsg::Sent { id, actor, nonce } if *id == self.id && *actor == self.actor => {
                self.nonce = Some(*nonce);
                None
            }
            ServerMsg::Batch(batch) => {
                self.keep(batch);
                None
            }
            ServerMsg::Result { id, seq, latency_us } if *id == self.id => {
                Some(self.answered(*seq, *latency_us))
            }
            ServerMsg::Error { id: Some(id), error } if *id == self.id => Some(self.failed(error)),
            _ => None,
        }
    }

    /// Keeps `b` if it is this command's batch. A second chunk with the same `seq` adds its
    /// events and replaces the balances (they travel once, on the last chunk), like the
    /// dashboard's recorder.
    fn keep(&mut self, b: &OutBatch) {
        let Some(nonce) = self.nonce else {
            return; // before `sent`: nothing is this command's yet
        };
        let own = Origin {
            actor: self.actor,
            nonce,
        };
        if b.origin != Some(own) {
            return;
        }
        match &mut self.batch {
            Some(kept) if kept.seq == b.seq => {
                kept.events.extend(b.events.iter().cloned());
                kept.balances.clone_from(&b.balances);
            }
            // A nonce is consumed once: a second batch for it cannot come. The first stays.
            Some(_) => {}
            None => self.batch = Some(b.clone()),
        }
    }

    /// The `result` came: `200`, or `422` if the engine refused the command.
    fn answered(&self, seq: u64, latency_us: u64) -> Reply {
        let (Some(nonce), Some(batch)) = (self.nonce, &self.batch) else {
            tracing::error!(id = self.id, seq, "a result without the command's batch");
            return Reply::error(500, "internal error: the result came without its batch");
        };
        let rejected = batch.events.iter().find_map(|e| match e.event {
            Event::CommandRejected { reason } => Some(reason),
            _ => None,
        });
        let applied = Applied {
            seq,
            nonce,
            latency_us,
            rejected,
            events: &batch.events,
            balances: &batch.balances,
        };
        match serde_json::to_string(&applied) {
            Ok(body) => Reply {
                status: if rejected.is_some() { 422 } else { 200 },
                body,
            },
            Err(e) => {
                tracing::error!(id = self.id, error = %e, "could not encode a write reply");
                Reply::error(500, "internal error")
            }
        }
    }

    /// The hub's `error` sentence for this command, as a status (spec §3).
    fn failed(&self, error: &str) -> Reply {
        let status = if error.starts_with("bad request") {
            400
        } else if error == "engine unavailable" || error.starts_with("nonce sync failed") {
            503
        } else if error == "ingress back-pressured, try again" {
            429
        } else if error.starts_with("outcome unknown") {
            504
        } else {
            tracing::warn!(id = self.id, error, "an error sentence with no status of its own");
            500
        };
        if status != 504 {
            return Reply::error(status, error);
        }
        // The nonce to resend the command with.
        let mut body = Map::new();
        body.insert("error".into(), error.into());
        body.insert("nonce".into(), self.nonce.map_or(Value::Null, Value::from));
        Reply {
            status,
            body: Value::Object(body).to_string(),
        }
    }
}

/// The body of a `200` and a `422`: the command's batch as its actor's view sees it.
#[derive(Serialize)]
struct Applied<'a> {
    seq: u64,
    nonce: u64,
    latency_us: u64,
    /// The `CommandRejected` reason, if the engine refused the command.
    rejected: Option<RejectReason>,
    events: &'a [Emitted],
    balances: &'a [AccountBalance],
}

#[cfg(test)]
mod tests {
    use rfq_app::BatchKind;
    use rfq_domain::{
        Action, Balance, ContractId, Emitted, Event, LegSpec, Pick, Price, QuoteId, RejectReason, RequestId,
        Side,
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::protocol::{AccountBalance, EngineState, Origin, OutBatch};
    use crate::view::View;

    const ALICE: AccountId = AccountId(10);
    const ID: &str = "http-7";

    fn price(bps: u16) -> Price {
        Price::new(bps).unwrap()
    }

    fn post(path: &str, body: &Value) -> Result<(AccountId, ClientMsg), String> {
        command_for(path, body.to_string().as_bytes(), ID)
    }

    // ---- command_for ----

    #[test]
    fn requests_become_submit_request() {
        let body = json!({"actor": 10, "legs": [
            {"contract": 7, "side": "BuyYes", "qty": 1_000, "limit_price": 7_000},
            {"contract": 8, "side": "BuyNo", "qty": 500, "limit_price": 3_000}],
            "response_deadline": 1_500});
        let action = Action::SubmitRequest {
            legs: vec![
                LegSpec {
                    contract: ContractId(7),
                    side: Side::BuyYes,
                    qty: 1_000,
                    limit_price: price(7_000),
                },
                LegSpec {
                    contract: ContractId(8),
                    side: Side::BuyNo,
                    qty: 500,
                    limit_price: price(3_000),
                },
            ],
            response_deadline: 1_500,
        };
        assert_eq!(
            post("/v1/requests", &body),
            Ok((
                ALICE,
                ClientMsg::Command {
                    id: ID.into(),
                    actor: ALICE,
                    nonce: None,
                    action: action.clone(),
                }
            ))
        );
        let mut with_nonce = body;
        with_nonce["nonce"] = json!(4);
        assert_eq!(
            post("/v1/requests", &with_nonce),
            Ok((
                ALICE,
                ClientMsg::Command {
                    id: ID.into(),
                    actor: ALICE,
                    nonce: Some(4),
                    action,
                }
            ))
        );
    }

    #[test]
    fn quotes_become_submit_quote() {
        let body = json!({"actor": 20, "nonce": 2, "request": 12, "leg": 0, "price": 6_200,
            "size": 1_000, "expires_at": 10_000});
        assert_eq!(
            post("/v1/quotes", &body),
            Ok((
                AccountId(20),
                ClientMsg::Command {
                    id: ID.into(),
                    actor: AccountId(20),
                    nonce: Some(2),
                    action: Action::SubmitQuote {
                        request: RequestId(12),
                        leg: 0,
                        price: price(6_200),
                        size: 1_000,
                        expires_at: 10_000,
                    },
                }
            ))
        );
    }

    #[test]
    fn commands_carry_any_client_action_in_the_websockets_json_form() {
        let cases = [
            (
                json!({"Deposit": {"to": 10, "amount": 5_000_000}}),
                Action::Deposit {
                    to: ALICE,
                    amount: 5_000_000,
                },
            ),
            (
                json!({"Accept": {"request": 12, "picks": [{"leg": 0, "quote": 14, "price": 6_000}]}}),
                Action::Accept {
                    request: RequestId(12),
                    picks: vec![Pick {
                        leg: 0,
                        quote: QuoteId(14),
                        price: price(6_000),
                    }],
                },
            ),
            (
                json!({"CancelQuote": {"quote": 14}}),
                Action::CancelQuote { quote: QuoteId(14) },
            ),
        ];
        for (action_json, action) in cases {
            let body = json!({"actor": 1, "action": action_json});
            assert_eq!(
                post("/v1/commands", &body),
                Ok((
                    AccountId(1),
                    ClientMsg::Command {
                        id: ID.into(),
                        actor: AccountId(1),
                        nonce: None,
                        action,
                    }
                )),
                "{body}"
            );
        }
    }

    #[test]
    fn bad_bodies_are_bad_requests() {
        let leg = json!({"contract": 7, "side": "BuyYes", "qty": 1, "limit_price": 7_000});
        let request = json!({"actor": 10, "legs": [leg], "response_deadline": 1_500});
        let quote = json!({"actor": 20, "request": 12, "leg": 0, "price": 6_200, "size": 1,
            "expires_at": 9});
        let command = json!({"actor": 1, "action": {"Withdraw": {"amount": 1}}});
        let without = |mut v: Value, key: &str| {
            v.as_object_mut().unwrap().remove(key);
            v
        };
        let with = |mut v: Value, key: &str, value: Value| {
            v[key] = value;
            v
        };
        let cases: Vec<(&str, &str, Vec<u8>)> = vec![
            (
                "missing actor",
                "/v1/requests",
                without(request.clone(), "actor").to_string().into(),
            ),
            (
                "missing legs",
                "/v1/requests",
                without(request.clone(), "legs").to_string().into(),
            ),
            (
                "missing deadline",
                "/v1/requests",
                without(request.clone(), "response_deadline").to_string().into(),
            ),
            (
                "unknown field",
                "/v1/requests",
                with(request.clone(), "colour", json!("red")).to_string().into(),
            ),
            (
                "bad side",
                "/v1/requests",
                with(
                    request.clone(),
                    "legs",
                    json!([{"contract": 7, "side": "Sideways", "qty": 1, "limit_price": 7_000}]),
                )
                .to_string()
                .into(),
            ),
            (
                "price 0",
                "/v1/requests",
                with(
                    request.clone(),
                    "legs",
                    json!([{"contract": 7, "side": "BuyYes", "qty": 1, "limit_price": 0}]),
                )
                .to_string()
                .into(),
            ),
            (
                "legs not an array",
                "/v1/requests",
                with(request.clone(), "legs", json!(7)).to_string().into(),
            ),
            (
                "actor as text",
                "/v1/requests",
                with(request.clone(), "actor", json!("10")).to_string().into(),
            ),
            (
                "negative actor",
                "/v1/requests",
                with(request, "actor", json!(-1)).to_string().into(),
            ),
            (
                "quote without actor",
                "/v1/quotes",
                without(quote.clone(), "actor").to_string().into(),
            ),
            (
                "quote without price",
                "/v1/quotes",
                without(quote.clone(), "price").to_string().into(),
            ),
            (
                "quote price 10000",
                "/v1/quotes",
                with(quote.clone(), "price", json!(10_000)).to_string().into(),
            ),
            (
                "quote unknown field",
                "/v1/quotes",
                with(quote.clone(), "note", json!(1)).to_string().into(),
            ),
            (
                "quote nonce as text",
                "/v1/quotes",
                with(quote, "nonce", json!("3")).to_string().into(),
            ),
            (
                "command without actor",
                "/v1/commands",
                without(command.clone(), "actor").to_string().into(),
            ),
            (
                "command without action",
                "/v1/commands",
                without(command.clone(), "action").to_string().into(),
            ),
            (
                "command unknown field",
                "/v1/commands",
                with(command.clone(), "id", json!("x")).to_string().into(),
            ),
            (
                "unknown action",
                "/v1/commands",
                with(command.clone(), "action", json!({"Mint": {"amount": 1}}))
                    .to_string()
                    .into(),
            ),
            (
                "Genesis cannot be sent",
                "/v1/commands",
                with(command.clone(), "action", json!({"Genesis": {}}))
                    .to_string()
                    .into(),
            ),
            (
                "Tick cannot be sent",
                "/v1/commands",
                with(command.clone(), "action", json!("Tick")).to_string().into(),
            ),
            (
                "Resume cannot be sent",
                "/v1/commands",
                with(command, "action", json!("Resume")).to_string().into(),
            ),
            ("an array", "/v1/commands", b"[]".to_vec()),
            (
                "an array shaped like the fields",
                "/v1/commands",
                br#"[1,null,{"Withdraw":{"amount":1}}]"#.to_vec(),
            ),
            ("null", "/v1/quotes", b"null".to_vec()),
            ("a string", "/v1/requests", br#""hello""#.to_vec()),
            ("a number", "/v1/requests", b"7".to_vec()),
            ("empty", "/v1/commands", Vec::new()),
            ("not JSON", "/v1/commands", b"hello".to_vec()),
            ("truncated", "/v1/commands", br#"{"actor":1,"action":"#.to_vec()),
            (
                "trailing garbage",
                "/v1/commands",
                br#"{"actor":1,"action":{"Withdraw":{"amount":1}}} {}"#.to_vec(),
            ),
            ("not UTF-8", "/v1/commands", vec![0xff, 0xfe, b'{']),
            (
                "deep nesting",
                "/v1/commands",
                format!("{}{}", "[".repeat(100_000), "]".repeat(100_000)).into_bytes(),
            ),
            (
                "deep nesting inside the action",
                "/v1/commands",
                format!(
                    r#"{{"actor":1,"action":{}{}}}"#,
                    "[".repeat(30_000),
                    "]".repeat(30_000)
                )
                .into_bytes(),
            ),
        ];
        for (what, path, body) in cases {
            match command_for(path, &body, ID) {
                Err(e) => assert!(e.starts_with("bad request: "), "{what}: {e}"),
                Ok(ok) => panic!("{what}: accepted as {ok:?}"),
            }
        }
    }

    #[test]
    fn only_the_three_post_routes_are_write_routes() {
        for path in ["/v1/requests", "/v1/quotes", "/v1/commands"] {
            assert!(is_write_route(path), "{path}");
        }
        for path in [
            "/v1/status",
            "/v1/accounts/10",
            "/v1/requests/",
            "/v1/commands/1",
            "/requests",
            "/v1/Requests",
            "",
        ] {
            assert!(!is_write_route(path), "{path}");
        }
        assert!(command_for("/v1/status", b"{}", ID).is_err());
    }

    // ---- Answer ----

    fn sent(nonce: u64) -> ServerMsg {
        ServerMsg::Sent {
            id: ID.into(),
            actor: ALICE,
            nonce,
        }
    }

    fn withdrawn() -> Emitted {
        Emitted::to(
            ALICE,
            Event::Withdrawn {
                account: ALICE,
                amount: 1,
            },
        )
    }

    fn rejected(reason: RejectReason) -> Emitted {
        Emitted::to(ALICE, Event::CommandRejected { reason })
    }

    fn balance(free: u64) -> Vec<AccountBalance> {
        vec![AccountBalance {
            account: ALICE,
            balance: Balance {
                free,
                reserved: 2,
                escrowed: 3,
            },
        }]
    }

    fn batch_of(
        actor: AccountId,
        nonce: u64,
        seq: u64,
        events: Vec<Emitted>,
        balances: Vec<AccountBalance>,
    ) -> ServerMsg {
        ServerMsg::Batch(OutBatch {
            seq,
            time: 9_000,
            kind: BatchKind::User,
            origin: Some(Origin { actor, nonce }),
            events,
            balances,
        })
    }

    fn batch(nonce: u64, seq: u64, events: Vec<Emitted>, balances: Vec<AccountBalance>) -> ServerMsg {
        batch_of(ALICE, nonce, seq, events, balances)
    }

    fn result(id: &str, seq: u64) -> ServerMsg {
        ServerMsg::Result {
            id: id.into(),
            seq,
            latency_us: 840,
        }
    }

    fn error(id: Option<&str>, text: &str) -> ServerMsg {
        ServerMsg::Error {
            id: id.map(Into::into),
            error: text.into(),
        }
    }

    /// Feeds `frames`; every frame but the last must leave the answer open.
    fn run(frames: &[ServerMsg]) -> Reply {
        let mut answer = Answer::new(ALICE, ID.into());
        let (last, rest) = frames.split_last().unwrap();
        for f in rest {
            assert_eq!(answer.on_frame(f), None, "answered too early, at {f:?}");
        }
        answer
            .on_frame(last)
            .expect("the last frame completes the answer")
    }

    fn json_of(reply: &Reply) -> Value {
        serde_json::from_str(&reply.body).unwrap_or_else(|e| panic!("{e}: {}", reply.body))
    }

    #[test]
    fn sent_batch_and_result_are_a_200_with_the_batch() {
        let reply = run(&[
            ServerMsg::Welcome {
                view: View::Account(ALICE),
                engine: EngineState::Up,
                now: 5,
            },
            ServerMsg::Status {
                engine: EngineState::Up,
                last_seq: Some(6),
            },
            sent(5),
            batch(5, 7, vec![withdrawn()], balance(100)),
            result(ID, 7),
        ]);
        assert_eq!(reply.status, 200);
        assert_eq!(
            json_of(&reply),
            json!({
                "seq": 7, "nonce": 5, "latency_us": 840, "rejected": null,
                "events": [{"audience": {"Account": 10}, "event": {"Withdrawn": {"account": 10, "amount": 1}}}],
                "balances": [{"account": 10, "balance": {"free": 100, "reserved": 2, "escrowed": 3}}]
            })
        );
    }

    /// A batch too big for one message arrives as chunks of one `seq`: their events add up, and
    /// the balances are the last chunk's.
    #[test]
    fn chunks_of_one_seq_are_merged() {
        let opened = |n: u64| Emitted::public(Event::QuoteExpired { quote: QuoteId(n) });
        let reply = run(&[
            sent(5),
            batch(5, 7, vec![opened(1), opened(2)], Vec::new()),
            batch(5, 7, vec![opened(3)], Vec::new()),
            batch(5, 7, Vec::new(), balance(100)),
            result(ID, 7),
        ]);
        assert_eq!(reply.status, 200);
        let body = json_of(&reply);
        assert_eq!(body["seq"], 7);
        let quotes: Vec<&Value> = body["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| &e["event"]["QuoteExpired"]["quote"])
            .collect();
        assert_eq!(quotes, [1, 2, 3]);
        assert_eq!(
            body["balances"],
            json!([{"account": 10, "balance": {"free": 100, "reserved": 2, "escrowed": 3}}])
        );
    }

    #[test]
    fn a_command_the_engine_refused_is_a_422_with_its_reason() {
        let reply = run(&[
            sent(5),
            batch(5, 7, vec![rejected(RejectReason::InsufficientFunds)], Vec::new()),
            result(ID, 7),
        ]);
        assert_eq!(reply.status, 422);
        let body = json_of(&reply);
        assert_eq!(body["rejected"], "InsufficientFunds");
        assert_eq!((&body["seq"], &body["nonce"]), (&json!(7), &json!(5)));
        assert_eq!(
            body["events"],
            json!([{"audience": {"Account": 10}, "event": {"CommandRejected": {"reason": "InsufficientFunds"}}}])
        );
    }

    #[test]
    fn each_error_sentence_has_its_status() {
        let outcome = "outcome unknown: the engine stopped answering after this command was sent; \
                       resend it with nonce 5 to retry safely (a repeat is rejected as Duplicate)";
        let cases: [(&str, u16); 8] = [
            ("bad request: expected value at line 1 column 1", 400),
            (
                "bad request: command too large for ingress (70000 bytes, limit 65536)",
                400,
            ),
            ("engine unavailable", 503),
            ("ingress back-pressured, try again", 429),
            ("nonce sync failed for account 10", 503),
            (outcome, 504),
            ("internal error: could not encode a frame", 500),
            ("something nobody wrote", 500),
        ];
        for (sentence, status) in cases {
            let reply = run(&[sent(5), error(Some(ID), sentence)]);
            assert_eq!(reply.status, status, "{sentence}");
            let body = json_of(&reply);
            assert_eq!(body["error"], sentence, "the hub's sentence is the body's error");
            if status == 504 {
                assert_eq!(body["nonce"], 5, "the retry nonce travels in the body");
            }
        }
        // An error before `sent` (the engine was down: the command was never offered).
        let reply = run(&[error(Some(ID), "engine unavailable")]);
        assert_eq!(json_of(&reply), json!({"error": "engine unavailable"}));
        assert_eq!(reply.status, 503);
    }

    /// The hub may send several commands' frames to this connection's account view: the answer
    /// takes only the batch of `(actor, nonce from sent)`.
    #[test]
    fn a_batch_with_another_nonce_for_the_same_actor_is_ignored() {
        let reply = run(&[
            sent(6),
            // The previous command of the same account, answered meanwhile: seq 7, nonce 5.
            batch(5, 7, vec![rejected(RejectReason::InsufficientFunds)], Vec::new()),
            batch(6, 8, vec![withdrawn()], balance(50)),
            // A later command's batch (nonce 7) is no part of this one either.
            batch(7, 9, vec![rejected(RejectReason::SelfTrade)], Vec::new()),
            result(ID, 8),
        ]);
        assert_eq!(reply.status, 200, "{}", reply.body);
        let body = json_of(&reply);
        assert_eq!(
            (&body["seq"], &body["nonce"], &body["rejected"]),
            (&json!(8), &json!(6), &Value::Null)
        );
        assert_eq!(body["events"].as_array().unwrap().len(), 1);
        assert_eq!(body["balances"][0]["balance"]["free"], 50);
    }

    #[test]
    fn a_batch_of_another_actor_or_without_an_origin_is_ignored() {
        let reply = run(&[
            sent(5),
            batch_of(
                AccountId(20),
                5,
                6,
                vec![rejected(RejectReason::SelfTrade)],
                Vec::new(),
            ),
            ServerMsg::Batch(OutBatch {
                seq: 7,
                time: 9_000,
                kind: BatchKind::Tick,
                origin: None,
                events: vec![rejected(RejectReason::InvalidState)],
                balances: Vec::new(),
            }),
            batch(5, 8, vec![withdrawn()], Vec::new()),
            result(ID, 8),
        ]);
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(json_of(&reply)["seq"], 8);
    }

    /// Before `sent` the nonce is not known, so no batch can be this command's.
    #[test]
    fn a_batch_before_sent_is_not_this_commands() {
        let reply = run(&[
            batch(5, 7, vec![rejected(RejectReason::InsufficientFunds)], Vec::new()),
            sent(5),
            batch(5, 8, vec![withdrawn()], Vec::new()),
            result(ID, 8),
        ]);
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(json_of(&reply)["seq"], 8);
    }

    #[test]
    fn frames_for_other_commands_and_other_kinds_are_ignored() {
        let mut answer = Answer::new(ALICE, ID.into());
        let ignored = [
            ServerMsg::Pong,
            ServerMsg::Status {
                engine: EngineState::Down,
                last_seq: None,
            },
            ServerMsg::Sent {
                id: "http-8".into(),
                actor: ALICE,
                nonce: 99,
            },
            result("http-8", 3),
            error(Some("http-8"), "engine unavailable"),
            error(None, "bad request: nope"),
        ];
        for f in &ignored {
            assert_eq!(answer.on_frame(f), None, "{f:?}");
        }
        // Another command's `sent` did not set the nonce.
        assert_eq!(answer.on_frame(&sent(5)), None);
        assert_eq!(
            answer.on_frame(&batch(99, 4, vec![withdrawn()], Vec::new())),
            None
        );
        assert_eq!(answer.on_frame(&batch(5, 6, vec![withdrawn()], Vec::new())), None);
        let reply = answer.on_frame(&result(ID, 6)).unwrap();
        assert_eq!((reply.status, json_of(&reply)["nonce"].clone()), (200, json!(5)));
    }
}
