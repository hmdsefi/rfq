//! The WebSocket protocol (docs/specs/gateway.md §3): text frames, one JSON object each, tagged by
//! `type`.
//!
//! Ids are plain numbers, prices are bps, amounts are integer micro-USD and times are engine ms.
//! The engine's own types (`Action`, `Emitted`, `Balance`, `BatchKind`) keep their serde shape, so
//! `Action` and `Event` are externally tagged (`{"Deposit": {"to": 10, "amount": 5}}`).
use rfq_app::BatchKind;
use rfq_domain::{AccountId, Action, Balance, Emitted, Timestamp};
use serde::{Deserialize, Serialize};

use crate::view::View;

/// The largest text frame a client may send, in bytes: the engine's ingress limit (gateway.md §7).
pub const MAX_TEXT: usize = 64 * 1024;

/// A message from a client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Sets this connection's view. Until the first `hello` it is `admin`.
    Hello {
        view: View,
    },
    /// Puts `ClientCommand { actor, nonce, action }` on ingress, with the gateway's next nonce for
    /// `actor` unless the client names one.
    Command {
        id: String,
        actor: AccountId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nonce: Option<u64>,
        action: Action,
    },
    Ping,
}

/// A message to a client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// The reply to `hello`. `now` is the gateway's wall clock in ms, so a client can align its
    /// schedule with engine time.
    Welcome {
        view: View,
        engine: EngineState,
        now: u64,
    },
    /// Accepted by the gateway and offered to ingress, with this nonce; an `error` (for example
    /// back pressure) may still follow.
    Sent {
        id: String,
        actor: AccountId,
        nonce: u64,
    },
    /// One engine output, filtered for this connection's view.
    Batch(OutBatch),
    /// To the sender only, right after the batch of its command. `latency_us` runs from the
    /// gateway receiving the command to receiving its batch, on the gateway's clock, so it
    /// includes any wait behind the actor's previous command or a nonce sync.
    Result {
        id: String,
        seq: u64,
        latency_us: u64,
    },
    /// On every change of the engine's state, and after `welcome`.
    Status {
        engine: EngineState,
        last_seq: Option<u64>,
    },
    /// A sentence (gateway.md §3), one of:
    /// - `bad request: …`: a frame that does not decode, `bad request: message too large …` for a
    ///   frame over 64 KiB, or `bad request: command too large for ingress (N bytes, limit 65536)`;
    /// - `engine unavailable`: the command was never offered;
    /// - `outcome unknown: … resend it with nonce N to retry safely …`: it was offered when the
    ///   engine stopped answering (gateway.md §6);
    /// - `ingress back-pressured, try again`;
    /// - `nonce sync failed for account N`.
    Error {
        id: Option<String>,
        error: String,
    },
    Pong,
}

/// Whether the engine is taking commands (gateway.md §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineState {
    Up,
    Down,
}

/// An [`EventBatch`](rfq_app::EventBatch) as one view sees it (see [`crate::view::filter`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutBatch {
    pub seq: u64,
    /// Engine time: the sequencer's stamp of the command, in ms.
    pub time: Timestamp,
    pub kind: BatchKind,
    /// The command's sender, for that sender's view only.
    pub origin: Option<Origin>,
    pub events: Vec<Emitted>,
    pub balances: Vec<AccountBalance>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub actor: AccountId,
    pub nonce: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountBalance {
    pub account: AccountId,
    pub balance: Balance,
}

/// Reads a client's text frame. Never panics: anything that is not one of the messages above,
/// or is longer than [`MAX_TEXT`], is an `Err` with a `bad request: …` sentence for the client.
pub fn decode_client(text: &str) -> Result<ClientMsg, String> {
    if text.len() > MAX_TEXT {
        return Err(too_large(text.len()));
    }
    serde_json::from_str(text).map_err(|e| format!("bad request: {e}"))
}

/// The error sentence for a client frame of `len` bytes, over [`MAX_TEXT`].
pub fn too_large(len: usize) -> String {
    format!("bad request: message too large ({len} bytes, at most {MAX_TEXT})")
}

/// The text frame for `msg`.
pub fn encode_server(msg: &ServerMsg) -> String {
    // These types serialize to JSON without fail (no maps, no non-string keys); the fallback is
    // there so that a bug can only cost one frame, never the gateway.
    serde_json::to_string(msg).unwrap_or_else(|e| {
        tracing::error!(error = %e, "could not encode a frame");
        r#"{"type":"error","id":null,"error":"internal error: could not encode a frame"}"#.to_string()
    })
}

#[cfg(test)]
mod tests {
    use rfq_app::BatchKind;
    use rfq_domain::{
        AccountId, Action, Audience, Balance, Emitted, Event, LegSpec, MarketId, Outcome, Pick, Price,
        QuoteId, RejectReason, RequestId, Side,
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::view::View;

    fn price(bps: u16) -> Price {
        Price::new(bps).unwrap()
    }

    fn decoded(text: &str) -> ClientMsg {
        decode_client(text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    #[test]
    fn every_client_message_decodes_from_the_spec_json() {
        assert_eq!(
            decoded(r#"{"type":"hello","view":"admin"}"#),
            ClientMsg::Hello { view: View::Admin }
        );
        assert_eq!(
            decoded(r#"{"type":"hello","view":{"account":10}}"#),
            ClientMsg::Hello {
                view: View::Account(AccountId(10))
            }
        );
        assert_eq!(
            decoded(
                r#"{"type":"command","id":"c1","actor":10,"action":{"Deposit":{"to":10,"amount":5000000}}}"#
            ),
            ClientMsg::Command {
                id: "c1".into(),
                actor: AccountId(10),
                nonce: None,
                action: Action::Deposit {
                    to: AccountId(10),
                    amount: 5_000_000
                },
            }
        );
        assert_eq!(
            decoded(
                r#"{"type":"command","id":"c2","actor":20,"nonce":3,"action":{"CancelQuote":{"quote":12}}}"#
            ),
            ClientMsg::Command {
                id: "c2".into(),
                actor: AccountId(20),
                nonce: Some(3),
                action: Action::CancelQuote { quote: QuoteId(12) },
            }
        );
        assert_eq!(decoded(r#"{"type":"ping"}"#), ClientMsg::Ping);
    }

    /// The engine's `Action` keeps its serde shape (externally tagged), so every action a client
    /// can send decodes from the obvious JSON.
    #[test]
    fn every_action_decodes_in_its_engine_shape() {
        let cases: Vec<(Value, Action)> = vec![
            (json!({"Withdraw": {"amount": 0}}), Action::Withdraw { amount: 0 }),
            (
                json!({"ListMarket": {"id": 7, "description": "rain?", "oracle": 4,
                    "trading_close": 5000, "resolution_deadline": 15000, "dispute_window": 2000}}),
                Action::ListMarket {
                    id: MarketId(7),
                    description: "rain?".into(),
                    oracle: AccountId(4),
                    trading_close: 5_000,
                    resolution_deadline: 15_000,
                    dispute_window: 2_000,
                },
            ),
            (
                json!({"SubmitRequest": {"legs": [{"market": 7, "side": "BuyYes", "qty": 1000,
                    "limit_price": 7000}], "response_deadline": 1500}}),
                Action::SubmitRequest {
                    legs: vec![LegSpec {
                        market: MarketId(7),
                        side: Side::BuyYes,
                        qty: 1_000,
                        limit_price: price(7_000),
                    }],
                    response_deadline: 1_500,
                },
            ),
            (
                json!({"CancelRequest": {"request": 12}}),
                Action::CancelRequest {
                    request: RequestId(12),
                },
            ),
            (
                json!({"SubmitQuote": {"request": 12, "leg": 0, "price": 6200, "size": 1000,
                    "expires_at": 10000}}),
                Action::SubmitQuote {
                    request: RequestId(12),
                    leg: 0,
                    price: price(6_200),
                    size: 1_000,
                    expires_at: 10_000,
                },
            ),
            (
                json!({"Accept": {"request": 12, "picks": [{"leg": 0, "quote": 14, "price": 6000}]}}),
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
                json!({"Reject": {"request": 12}}),
                Action::Reject {
                    request: RequestId(12),
                },
            ),
            (
                json!({"ProposeOutcome": {"market": 7, "outcome": "Yes"}}),
                Action::ProposeOutcome {
                    market: MarketId(7),
                    outcome: Outcome::Yes,
                },
            ),
            (
                json!({"Dispute": {"market": 7}}),
                Action::Dispute { market: MarketId(7) },
            ),
            (
                json!({"Rule": {"market": 7, "outcome": "Invalid"}}),
                Action::Rule {
                    market: MarketId(7),
                    outcome: Outcome::Invalid,
                },
            ),
        ];
        for (action_json, action) in cases {
            let text = json!({"type": "command", "id": "x", "actor": 10, "action": action_json}).to_string();
            assert_eq!(
                decoded(&text),
                ClientMsg::Command {
                    id: "x".into(),
                    actor: AccountId(10),
                    nonce: None,
                    action,
                },
                "{text}"
            );
        }
    }

    #[test]
    fn bad_input_is_a_bad_request_not_a_panic() {
        let huge = "x".repeat(70 * 1024);
        let huge_command = json!({"type": "command", "id": "big", "actor": 1, "action": {"ListMarket": {
            "id": 7, "description": huge, "oracle": 4, "trading_close": 1,
            "resolution_deadline": 2, "dispute_window": 1}}})
        .to_string();
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        // Inside a message, where an internally tagged enum buffers the whole value first.
        let deep_inside = format!(
            r#"{{"type":"command","id":"c","actor":1,"action":{}{}}}"#,
            "[".repeat(30_000),
            "]".repeat(30_000)
        );
        let cases = [
            ("unknown type", r#"{"type":"subscribe","view":"admin"}"#.to_string()),
            ("no type", r#"{"view":"admin"}"#.to_string()),
            ("hello without view", r#"{"type":"hello"}"#.to_string()),
            ("unknown view", r#"{"type":"hello","view":"root"}"#.to_string()),
            ("command without id", r#"{"type":"command","actor":10,"action":{"Withdraw":{"amount":1}}}"#.to_string()),
            ("command without action", r#"{"type":"command","id":"c","actor":10}"#.to_string()),
            ("command without actor", r#"{"type":"command","id":"c","action":{"Withdraw":{"amount":1}}}"#.to_string()),
            ("unknown action", r#"{"type":"command","id":"c","actor":10,"action":{"Mint":{"amount":1}}}"#.to_string()),
            ("action without fields", r#"{"type":"command","id":"c","actor":10,"action":{"Withdraw":{}}}"#.to_string()),
            (
                "price 0",
                r#"{"type":"command","id":"c","actor":20,"action":{"SubmitQuote":{"request":12,"leg":0,"price":0,"size":1000,"expires_at":9}}}"#.to_string(),
            ),
            (
                "price 10000",
                r#"{"type":"command","id":"c","actor":20,"action":{"SubmitQuote":{"request":12,"leg":0,"price":10000,"size":1000,"expires_at":9}}}"#.to_string(),
            ),
            ("negative amount", r#"{"type":"command","id":"c","actor":10,"action":{"Withdraw":{"amount":-1}}}"#.to_string()),
            ("amount over u64", r#"{"type":"command","id":"c","actor":10,"action":{"Withdraw":{"amount":18446744073709551616}}}"#.to_string()),
            ("nonce as text", r#"{"type":"command","id":"c","actor":10,"nonce":"3","action":{"Withdraw":{"amount":1}}}"#.to_string()),
            ("truncated JSON", r#"{"type":"hello","view":"#.to_string()),
            ("not JSON", "hello".to_string()),
            ("empty", String::new()),
            ("null", "null".to_string()),
            ("array", "[]".to_string()),
            ("trailing garbage", r#"{"type":"ping"} {"type":"ping"}"#.to_string()),
            ("70 KiB command", huge_command),
            ("70 KiB of anything", huge.clone()),
            ("deep nesting", deep),
            ("deep nesting inside a command", deep_inside),
        ];
        for (what, text) in cases {
            match decode_client(&text) {
                Err(e) => assert!(e.starts_with("bad request: "), "{what}: {e}"),
                Ok(msg) => panic!("{what}: decoded {msg:?}"),
            }
        }
    }

    #[test]
    fn a_frame_over_64_kib_is_refused_as_too_large() {
        let at_limit = format!(r#"{{"type":"ping","pad":"{}"}}"#, " ".repeat(MAX_TEXT - 24));
        assert_eq!(at_limit.len(), MAX_TEXT);
        assert_eq!(decode_client(&at_limit), Ok(ClientMsg::Ping));
        let over = format!("{at_limit} ");
        let e = decode_client(&over).unwrap_err();
        assert!(e.starts_with("bad request: message too large"), "{e}");
    }

    fn encoded(msg: &ServerMsg) -> Value {
        serde_json::from_str(&encode_server(msg)).unwrap()
    }

    #[test]
    fn every_server_message_encodes_to_the_spec_json() {
        assert_eq!(
            encoded(&ServerMsg::Welcome {
                view: View::Admin,
                engine: EngineState::Up,
                now: 1_700_000_000_000,
            }),
            json!({"type": "welcome", "view": "admin", "engine": "up", "now": 1_700_000_000_000u64})
        );
        assert_eq!(
            encoded(&ServerMsg::Welcome {
                view: View::Account(AccountId(10)),
                engine: EngineState::Down,
                now: 5,
            }),
            json!({"type": "welcome", "view": {"account": 10}, "engine": "down", "now": 5})
        );
        assert_eq!(
            encoded(&ServerMsg::Sent {
                id: "c1".into(),
                actor: AccountId(10),
                nonce: 7,
            }),
            json!({"type": "sent", "id": "c1", "actor": 10, "nonce": 7})
        );
        assert_eq!(
            encoded(&ServerMsg::Batch(OutBatch {
                seq: 12,
                time: 1_000,
                kind: BatchKind::User,
                origin: Some(Origin {
                    actor: AccountId(10),
                    nonce: 7,
                }),
                events: vec![
                    Emitted::public(Event::RequestOpened {
                        request: RequestId(12),
                        leg: 0,
                        market: MarketId(7),
                        side: Side::BuyYes,
                        qty: 1_000,
                        response_deadline: 2_500,
                    }),
                    Emitted::to(
                        AccountId(10),
                        Event::CommandRejected {
                            reason: RejectReason::InsufficientFunds,
                        },
                    ),
                ],
                balances: vec![AccountBalance {
                    account: AccountId(10),
                    balance: Balance {
                        free: 1,
                        reserved: 2,
                        escrowed: 3,
                    },
                }],
            })),
            json!({
                "type": "batch", "seq": 12, "time": 1000, "kind": "User",
                "origin": {"actor": 10, "nonce": 7},
                "events": [
                    {"audience": "Public", "event": {"RequestOpened": {"request": 12, "leg": 0,
                        "market": 7, "side": "BuyYes", "qty": 1000, "response_deadline": 2500}}},
                    {"audience": {"Account": 10}, "event": {"CommandRejected": {"reason": "InsufficientFunds"}}}
                ],
                "balances": [{"account": 10, "balance": {"free": 1, "reserved": 2, "escrowed": 3}}]
            })
        );
        assert_eq!(
            encoded(&ServerMsg::Batch(OutBatch {
                seq: 13,
                time: 2_000,
                kind: BatchKind::Resume,
                origin: None,
                events: Vec::new(),
                balances: Vec::new(),
            })),
            json!({"type": "batch", "seq": 13, "time": 2000, "kind": "Resume", "origin": null,
                "events": [], "balances": []})
        );
        assert_eq!(
            encoded(&ServerMsg::Result {
                id: "c1".into(),
                seq: 12,
                latency_us: 840,
            }),
            json!({"type": "result", "id": "c1", "seq": 12, "latency_us": 840})
        );
        assert_eq!(
            encoded(&ServerMsg::Status {
                engine: EngineState::Up,
                last_seq: Some(12),
            }),
            json!({"type": "status", "engine": "up", "last_seq": 12})
        );
        assert_eq!(
            encoded(&ServerMsg::Status {
                engine: EngineState::Down,
                last_seq: None,
            }),
            json!({"type": "status", "engine": "down", "last_seq": null})
        );
        assert_eq!(
            encoded(&ServerMsg::Error {
                id: Some("c1".into()),
                error: "engine unavailable".into(),
            }),
            json!({"type": "error", "id": "c1", "error": "engine unavailable"})
        );
        assert_eq!(
            encoded(&ServerMsg::Error {
                id: None,
                error: "bad request: x".into(),
            }),
            json!({"type": "error", "id": null, "error": "bad request: x"})
        );
        assert_eq!(encoded(&ServerMsg::Pong), json!({"type": "pong"}));
    }

    /// What the gateway sends, a client (or a test) reads back unchanged.
    #[test]
    fn server_messages_round_trip() {
        let msgs = [
            ServerMsg::Pong,
            ServerMsg::Error {
                id: None,
                error: "e".into(),
            },
            ServerMsg::Batch(OutBatch {
                seq: 1,
                time: 2,
                kind: BatchKind::Tick,
                origin: None,
                events: vec![Emitted {
                    audience: Audience::Account(AccountId(3)),
                    event: Event::QuoteExpired { quote: QuoteId(4) },
                }],
                balances: Vec::new(),
            }),
        ];
        for m in msgs {
            assert_eq!(serde_json::from_str::<ServerMsg>(&encode_server(&m)).unwrap(), m);
        }
    }
}
