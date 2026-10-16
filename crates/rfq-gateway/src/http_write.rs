//! HTTP writes through the hub (docs/specs/read-api.md §4): the short-lived hub connection a
//! `POST` opens, so the hub needs no HTTP code.
//!
//! A `POST` on a write route ([`write_api::is_write_route`]) takes the next id from the shared
//! [`ConnIds`] and enters the hub loop's inbox exactly as a WebSocket connection would, with its
//! own outbox: `Open`, a `hello` with the actor's account view, and the command (id
//! `http-<conn>`). The nonce book, the per-account queue, back pressure, the unresolved-nonce hold
//! and `outcome unknown` are the hub's, the same as for a WebSocket command. The handler then
//! feeds what the hub sends back to the connection's outbox into a [`write_api::Answer`], until it
//! has the reply or [`WAIT`] has passed (`504`). If it gives up (the wait runs out, a frame does not
//! decode, the hub drops the outbox) after the hub sent the command, the command may be on ingress:
//! the answer is `504 {"error": "outcome unknown: …", "nonce": N}`, the nonce to resend it with.
//!
//! The connection is always closed (`Close`), whichever way the handler ends: an answer, an error
//! sentence, the wait running out, the hub dropping the outbox, or a panic. A client that has gone
//! away is not noticed: the handler is a plain function of the request, and its command, already
//! on its way to the engine, is answered into the void; what matters is that the hub forgets the
//! connection when the handler returns, and so drops whatever of it is still queued.
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::{Duration, Instant};

use rfq_gateway::hub::ConnId;
use rfq_gateway::protocol::{ClientMsg, MAX_TEXT, ServerMsg, too_large};
use rfq_gateway::read_api::Reply;
use rfq_gateway::view::View;
use rfq_gateway::write_api::{self, Answer};

use crate::http_server::Request;
use crate::ws::{ConnIds, Inbox, InboxItem, OUTBOX};

/// How long a `POST` waits for its command's answer: the dashboard's command timeout, which
/// outlasts the hub's 60 s unresolved-nonce hold (spec §4).
pub const WAIT: Duration = Duration::from_secs(75);

/// The HTTP write handler: `POST`s turned into hub connections.
pub struct Writer {
    inbox: Inbox,
    ids: ConnIds,
    wait: Duration,
}

impl Writer {
    /// A writer that enters the hub loop through `inbox`, numbering its connections from `ids`
    /// (shared with the WebSocket acceptor), and waits at most `wait` for each answer.
    pub fn new(inbox: Inbox, ids: ConnIds, wait: Duration) -> Writer {
        Writer { inbox, ids, wait }
    }

    /// Runs the command of a `POST` on a write route, and answers with its batch.
    pub fn post(&self, request: &Request) -> Reply {
        let conn = self.ids.next();
        let id = format!("http-{}", conn.0);
        let (actor, command) = match write_api::command_for(&request.path, &request.body, &id) {
            Ok(parsed) => parsed,
            Err(sentence) => return Reply::error(400, &sentence),
        };
        let Some(hello) = encode(&ClientMsg::Hello {
            view: View::Account(actor),
        }) else {
            return internal();
        };
        let Some(command) = encode(&command) else {
            return internal();
        };
        // The frame is longer than the body (its type and id are added). The hub refuses one over
        // the limit without naming a command, so no answer to this id would ever come.
        if command.len() > MAX_TEXT {
            return Reply::error(400, &too_large(command.len()));
        }

        let (outbox, frames) = sync_channel(OUTBOX);
        let open = InboxItem::Open {
            peer: request.peer,
            outbox,
        };
        if self.inbox.send((conn, open)).is_err() {
            return stopping();
        }
        let _closing = Closing {
            conn,
            inbox: &self.inbox,
        };
        for text in [hello, command] {
            if self.inbox.send((conn, InboxItem::Text(text))).is_err() {
                return stopping();
            }
        }

        let mut answer = Answer::new(actor, id);
        let deadline = Instant::now() + self.wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match frames.recv_timeout(left) {
                Ok(text) => match serde_json::from_str::<ServerMsg>(&text) {
                    Ok(frame) => {
                        if let Some(reply) = answer.on_frame(&frame) {
                            return reply;
                        }
                    }
                    Err(e) => {
                        tracing::error!(conn = conn.0, error = %e, "a frame from the hub did not decode");
                        return gave_up(&answer, "a frame from the gateway did not decode", internal());
                    }
                },
                Err(RecvTimeoutError::Timeout) => {
                    tracing::warn!(
                        conn = conn.0,
                        wait_s = self.wait.as_secs(),
                        "no answer from the hub"
                    );
                    let unsent = Reply::error(504, "no answer from the gateway");
                    return gave_up(&answer, "no answer from the gateway", unsent);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // The hub loop dropped this outbox: it did not keep up (or is going down).
                    tracing::warn!(conn = conn.0, "the hub closed a write's connection");
                    let unsent = Reply::error(500, "internal error: the gateway closed the connection");
                    return gave_up(&answer, "the gateway closed the connection", unsent);
                }
            }
        }
    }
}

/// Tells the hub loop that the connection is closed, however the handler ends (a panic included).
struct Closing<'a> {
    conn: ConnId,
    inbox: &'a Inbox,
}

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        let close = InboxItem::Close {
            why: "http request done".into(),
        };
        // Ignored: if the hub loop has stopped, nobody needs to know.
        let _ = self.inbox.send((self.conn, close));
    }
}

fn encode(msg: &ClientMsg) -> Option<String> {
    match serde_json::to_string(msg) {
        Ok(text) => Some(text),
        Err(e) => {
            tracing::error!(error = %e, "could not encode a client message");
            None
        }
    }
}

/// The answer when the handler gives up on its command for `reason`. Once the hub has sent it (its
/// nonce is known), the command may be on ingress: `504` with the nonce to resend it with, as for
/// the hub's own `outcome unknown` (spec §3). Before that, `unsent`.
fn gave_up(answer: &Answer, reason: &str, unsent: Reply) -> Reply {
    let Some(nonce) = answer.nonce() else {
        return unsent;
    };
    let body = serde_json::json!({"error": format!("outcome unknown: {reason}"), "nonce": nonce});
    Reply {
        status: 504,
        body: body.to_string(),
    }
}

fn internal() -> Reply {
    Reply::error(500, "internal error")
}

/// The hub loop is gone: the process is ending.
fn stopping() -> Reply {
    Reply::error(503, "the gateway is stopping")
}

#[cfg(test)]
mod tests {
    use rfq_domain::AccountId;
    use rfq_gateway::protocol::ServerMsg;
    use serde_json::{Value, json};

    use super::*;

    /// Every way the handler gives up (the wait, a frame that does not decode, the hub dropping
    /// the outbox): once the hub has sent the command it may be on ingress, so the answer is an
    /// unknown outcome with the nonce to resend it with; before that, what it always was.
    #[test]
    fn giving_up_after_sent_is_an_unknown_outcome_with_the_nonce() {
        let mut answer = Answer::new(AccountId(10), "http-1".into());
        assert_eq!(answer.nonce(), None);
        let unsent = || Reply::error(500, "internal error: the gateway closed the connection");
        assert_eq!(
            gave_up(&answer, "the gateway closed the connection", unsent()),
            unsent()
        );

        let sent = ServerMsg::Sent {
            id: "http-1".into(),
            actor: AccountId(10),
            nonce: 7,
        };
        assert_eq!(answer.on_frame(&sent), None);
        assert_eq!(answer.nonce(), Some(7));
        for reason in [
            "no answer from the gateway",
            "a frame from the gateway did not decode",
            "the gateway closed the connection",
        ] {
            let reply = gave_up(&answer, reason, unsent());
            assert_eq!(reply.status, 504, "{reason}");
            assert_eq!(
                serde_json::from_str::<Value>(&reply.body).unwrap(),
                json!({"error": format!("outcome unknown: {reason}"), "nonce": 7})
            );
        }
    }
}
