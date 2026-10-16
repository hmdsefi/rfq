//! The WebSocket server (docs/specs/gateway.md §7, §9): an acceptor thread, and one thread per
//! connection. It knows nothing of the protocol beyond frame sizes: text goes to the hub loop's
//! inbox, and what the hub sends comes back through the connection's outbox.
//!
//! - **Upgrade.** Only from an Origin that [`origin_allowed`] accepts (`null`,
//!   `http://localhost[:port]`, `http://127.0.0.1[:port]`, or none: see `rfq_gateway::origin` for
//!   why each is safe enough); any other Origin gets HTTP 403. The HTTP API applies the same rule.
//! - **Each connection's thread** alternates between writing its outbox and reading one frame,
//!   with a socket timeout of [`POLL`], so a frame from the hub waits at most that long.
//! - **Sizes.** A text frame over [`MAX_TEXT`] (64 KiB, the engine's ingress limit) is answered
//!   with an `error` frame and discarded; the connection stays open. tungstenite itself refuses
//!   frames and messages over [`MAX_FRAME`] (1 MiB): it reports that after reading only the
//!   frame header, so the rest of the stream can no longer be read in step. Such a connection
//!   gets the `error` frame and is closed (1009, message too big). A client that keeps sending
//!   for longer than the 200 ms drain may lose that error frame to the TCP reset.
//! - **Ping, pong and the close handshake** are tungstenite's.
//! - **The inbox is bounded** ([`INBOX`] items): a connection's thread blocks on a full one, so
//!   a client that sends faster than the hub loop works is slowed down by its own TCP window.
//! - **Failures are per connection.** An I/O or protocol error, or a client too slow to read
//!   ([`OUTBOX`] frames behind, or [`MAX_WRITE_BUFFER`] unsent), closes that connection only,
//!   and the hub loop gets its `Close`.
use std::io::{self, ErrorKind, Read};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rfq_gateway::hub::ConnId;
use rfq_gateway::origin::origin_allowed;
use rfq_gateway::protocol::{MAX_TEXT, ServerMsg, encode_server, too_large};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::http::StatusCode;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tungstenite::{Error as WsError, HandshakeError, Message, WebSocket};

/// How long a connection's thread waits for a frame before it looks at its outbox again.
pub const POLL: Duration = Duration::from_millis(5);
/// How long a client may take to send its upgrade request.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// tungstenite's own frame and message limit. Text between [`MAX_TEXT`] and this is answered
/// with an `error` frame; beyond it the connection is closed.
pub const MAX_FRAME: usize = 1 << 20;
/// Frames the hub may queue for one connection. A client this far behind is closed.
pub const OUTBOX: usize = 8_192;
/// Bytes tungstenite may hold for a client that does not read. Past it the client is closed.
/// Also the largest single frame the gateway can send (a batch near Aeron's max message).
pub const MAX_WRITE_BUFFER: usize = 64 << 20;
/// Connections open at once; more are refused (one thread each).
const MAX_CONNECTIONS: usize = 256;
/// Items the hub loop's inbox holds; the connections' threads block when it is full.
pub const INBOX: usize = 4_096;

/// What a connection's thread tells the hub loop.
#[derive(Debug)]
pub enum InboxItem {
    /// The upgrade succeeded. The hub loop sends this connection's frames to `outbox`; dropping
    /// it closes the connection.
    Open {
        peer: SocketAddr,
        outbox: SyncSender<String>,
    },
    /// A text frame of at most [`MAX_TEXT`] bytes.
    Text(String),
    /// A frame the server does not pass on (over [`MAX_TEXT`], or binary), already discarded.
    /// The hub loop answers it with this `error` sentence, in order with the replies to the
    /// connection's earlier frames.
    Refused(String),
    /// The connection is closed, and why. Nothing more comes from it.
    Close { why: String },
}

/// The hub loop's inbox: bounded, and every send blocks while it is full.
pub type Inbox = SyncSender<(ConnId, InboxItem)>;

/// Starts the acceptor thread on `listener`. Each accepted connection gets an id and a thread.
pub fn spawn(listener: TcpListener, inbox: Inbox) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("ws-accept".into())
        .spawn(move || accept(&listener, &inbox))
}

fn accept(listener: &TcpListener, inbox: &Inbox) {
    let live = Arc::new(AtomicUsize::new(0));
    let mut next = 0u64;
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                // Out of file descriptors, or a connection reset before it was accepted.
                tracing::warn!(error = %e, "ws accept failed");
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        if live.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            tracing::warn!(peer, max = MAX_CONNECTIONS, "ws refused: too many connections");
            continue;
        }
        next += 1;
        let conn = ConnId(next);
        let guard = Live::enter(&live);
        let inbox = inbox.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("ws-{next}"))
            .spawn(move || {
                let _guard = guard;
                connection(conn, stream, &inbox);
            });
        if let Err(e) = spawned {
            tracing::warn!(peer, error = %e, "ws refused: no thread");
        }
    }
}

/// Counts a connection's thread while it lives.
struct Live(Arc<AtomicUsize>);

impl Live {
    fn enter(count: &Arc<AtomicUsize>) -> Live {
        count.fetch_add(1, Ordering::Relaxed);
        Live(Arc::clone(count))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One connection, from the upgrade to the close.
fn connection(conn: ConnId, stream: TcpStream, inbox: &Inbox) {
    let Ok(peer) = stream.peer_addr() else {
        return;
    };
    let mut ws = match handshake(stream) {
        Ok(ws) => ws,
        Err(why) => {
            tracing::info!(%peer, why, "ws refused");
            return;
        }
    };
    let (outbox, frames) = sync_channel(OUTBOX);
    if inbox.send((conn, InboxItem::Open { peer, outbox })).is_err() {
        return; // the hub loop has stopped: the process is ending
    }
    let mut closing = Closing {
        conn,
        inbox,
        why: None,
    };
    closing.why = Some(serve(&mut ws, conn, &frames, inbox));
}

/// Tells the hub loop that the connection closed, however its thread ends (a panic included).
struct Closing<'a> {
    conn: ConnId,
    inbox: &'a Inbox,
    why: Option<String>,
}

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        let why = self
            .why
            .take()
            .unwrap_or_else(|| "the connection's thread failed".into());
        // Ignored: if the hub loop has stopped, nobody needs to know.
        let _ = self.inbox.send((self.conn, InboxItem::Close { why }));
    }
}

/// The upgrade, with the Origin check. The socket gets its [`POLL`] timeouts once it is a
/// WebSocket.
fn handshake(stream: TcpStream) -> Result<WebSocket<TcpStream>, String> {
    let timeouts = |s: &TcpStream, d: Duration| {
        s.set_read_timeout(Some(d))?;
        s.set_write_timeout(Some(d))
    };
    timeouts(&stream, HANDSHAKE_TIMEOUT).map_err(|e| e.to_string())?;
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    let mut refused: Option<String> = None;
    // tungstenite's `Callback` fixes this signature, the large error type included.
    #[allow(clippy::result_large_err)]
    let check = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        let origin = req
            .headers()
            .get("origin")
            .map(|v| v.to_str().unwrap_or("<not ASCII>"));
        if origin_allowed(origin) {
            return Ok(resp);
        }
        refused = Some(format!("origin {:?} not allowed", origin.unwrap_or_default()));
        let mut forbidden = ErrorResponse::new(Some("origin not allowed\n".into()));
        *forbidden.status_mut() = StatusCode::FORBIDDEN;
        Err(forbidden)
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME))
        .max_write_buffer_size(MAX_WRITE_BUFFER);
    match tungstenite::accept_hdr_with_config(stream, check, Some(config)) {
        Ok(ws) => {
            timeouts(ws.get_ref(), POLL).map_err(|e| e.to_string())?;
            Ok(ws)
        }
        Err(HandshakeError::Interrupted(_)) => Err("no upgrade request in time".into()),
        Err(HandshakeError::Failure(e)) => Err(refused.unwrap_or_else(|| e.to_string())),
    }
}

/// Pumps one open connection until it closes; returns why it closed.
fn serve(ws: &mut WebSocket<TcpStream>, conn: ConnId, frames: &Receiver<String>, inbox: &Inbox) -> String {
    loop {
        // What the hub sent. A write that would block stays in tungstenite's buffer.
        loop {
            match frames.try_recv() {
                Ok(frame) => match ws.write(Message::text(frame)) {
                    Ok(()) => {}
                    Err(e) if would_block(&e) => {}
                    Err(e) => return failed(ws, &e),
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    close(ws, CloseCode::Policy, "too slow to read");
                    return "the client is too slow to read".into();
                }
            }
        }
        match ws.flush() {
            Ok(()) => {}
            Err(e) if would_block(&e) => {}
            Err(e) => return failed(ws, &e),
        }

        // One frame from the client, or nothing within POLL. What it says goes to the hub loop,
        // refusals included, so the replies keep the order of the client's frames.
        let item = match ws.read() {
            Ok(Message::Text(text)) if text.len() > MAX_TEXT => {
                tracing::debug!(conn = conn.0, len = text.len(), "ws frame too large");
                Some(InboxItem::Refused(too_large(text.len())))
            }
            Ok(Message::Text(text)) => Some(InboxItem::Text(text.as_str().to_owned())),
            Ok(Message::Binary(_)) => Some(InboxItem::Refused(
                "bad request: binary frames are not supported; send JSON text".into(),
            )),
            // tungstenite answers pings itself.
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => None,
            // tungstenite has queued the reply; reading on sends it and ends the handshake.
            Ok(Message::Close(_)) => {
                finish(ws);
                return "closed by the client".into();
            }
            Err(e) if would_block(&e) => None,
            // After any other read error, tungstenite is not read again (see `abort`).
            Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => return "closed by the client".into(),
            Err(WsError::Capacity(e)) => {
                reply_error(ws, format!("bad request: message too large ({e}); closing"));
                abort(ws, CloseCode::Size, "message too large");
                return format!("frame over the {MAX_FRAME}-byte limit: {e}");
            }
            Err(WsError::Utf8(_)) => {
                reply_error(ws, "bad request: a text frame must be UTF-8; closing".into());
                abort(ws, CloseCode::Invalid, "text is not UTF-8");
                return "text frame not UTF-8".into();
            }
            Err(e @ WsError::Protocol(_)) => {
                abort(ws, CloseCode::Protocol, "protocol error");
                return e.to_string();
            }
            Err(e) => return e.to_string(),
        };
        if let Some(item) = item
            && inbox.send((conn, item)).is_err()
        {
            close(ws, CloseCode::Away, "the gateway is stopping");
            return "the gateway is stopping".into();
        }
    }
}

fn would_block(e: &WsError) -> bool {
    matches!(e, WsError::Io(io) if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted))
}

/// After a write or flush error. Returns the reason for the log.
fn failed(ws: &mut WebSocket<TcpStream>, e: &WsError) -> String {
    match e {
        WsError::WriteBufferFull(_) => {
            close(ws, CloseCode::Policy, "too slow to read");
            "the client is too slow to read".into()
        }
        WsError::ConnectionClosed | WsError::AlreadyClosed => "closed by the client".into(),
        _ => e.to_string(),
    }
}

/// Sends an `error` frame straight from this thread: only just before closing the connection,
/// when the order of replies no longer matters.
fn reply_error(ws: &mut WebSocket<TcpStream>, error: String) {
    let frame = encode_server(&ServerMsg::Error { id: None, error });
    // A failure shows up on the next read or write.
    let _ = ws.write(Message::text(frame)).and_then(|()| ws.flush());
}

/// Starts the close handshake and gives the client a moment to answer; never blocks for long.
fn close(ws: &mut WebSocket<TcpStream>, code: CloseCode, reason: &'static str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    match ws.close(Some(frame)) {
        Ok(()) => finish(ws),
        Err(e) if would_block(&e) => finish(ws),
        Err(_) => {}
    }
}

/// Closes after a read error. tungstenite may be out of step with the stream then (after a
/// `Capacity` error it has read only the frame's header, and reading on would take the rest as
/// a frame, or trip its own assertion), so it is never read again. The close frame goes out,
/// then a FIN, and the raw socket is drained for at most 200 ms: closing a socket with unread
/// bytes sends a reset, which can destroy the frames just sent before the client reads them.
fn abort(ws: &mut WebSocket<TcpStream>, code: CloseCode, reason: &'static str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _ = ws.close(Some(frame));
    let stream = ws.get_mut();
    let _ = stream.shutdown(Shutdown::Write);
    let mut sink = [0u8; 16 * 1024];
    let until = Instant::now() + Duration::from_millis(200);
    while Instant::now() < until {
        match stream.read(&mut sink) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(_) => return,
        }
    }
}

/// Drives the close handshake to its end, or until the connection fails, for at most 200 ms:
/// flushes what is queued (the close frame, or the reply to the client's) and reads on,
/// discarding, so no unread bytes are left to turn the TCP close into a reset.
fn finish(ws: &mut WebSocket<TcpStream>) {
    let until = Instant::now() + Duration::from_millis(200);
    while Instant::now() < until {
        let _ = ws.flush();
        match ws.read() {
            Err(e) if would_block(&e) => {}
            Err(_) => return,
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::sync::mpsc::{self, RecvTimeoutError};

    use serde_json::{Value, json};
    use tungstenite::ClientRequestBuilder;
    use tungstenite::client::client_with_config;
    use tungstenite::http::Uri;

    use super::*;

    /// A server on an ephemeral port, with a fake hub loop that echoes every text frame back to
    /// its connection, and closes a connection (drops its outbox) when it sends `bye`. Returns
    /// the port and a log of what reached the inbox.
    fn server() -> (u16, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (inbox, items) = mpsc::sync_channel::<(ConnId, InboxItem)>(64);
        spawn(listener, inbox).unwrap();
        let (log, seen) = mpsc::channel();
        std::thread::spawn(move || {
            let mut outboxes = BTreeMap::new();
            for (conn, item) in items {
                match item {
                    InboxItem::Open { outbox, .. } => {
                        let _ = log.send(format!("open {}", conn.0));
                        outboxes.insert(conn, outbox);
                    }
                    InboxItem::Refused(error) => {
                        let _ = log.send(format!("refused {} {error}", conn.0));
                        if let Some(outbox) = outboxes.get(&conn) {
                            let _ = outbox.send(encode_server(&ServerMsg::Error { id: None, error }));
                        }
                    }
                    InboxItem::Text(text) if text == "bye" => {
                        outboxes.remove(&conn);
                    }
                    InboxItem::Text(text) => {
                        let _ = log.send(format!("text {} {} bytes", conn.0, text.len()));
                        if let Some(outbox) = outboxes.get(&conn) {
                            let _ = outbox.send(text);
                        }
                    }
                    InboxItem::Close { why } => {
                        let _ = log.send(format!("close {} {why}", conn.0));
                        outboxes.remove(&conn);
                    }
                }
            }
        });
        (port, seen)
    }

    type Client = WebSocket<TcpStream>;

    fn connect(port: u16, origin: Option<&str>) -> Result<Client, String> {
        let uri: Uri = format!("ws://127.0.0.1:{port}/").parse().unwrap();
        let mut request = ClientRequestBuilder::new(uri);
        if let Some(origin) = origin {
            request = request.with_header("Origin", origin);
        }
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // The client may send more than the server accepts.
        let config = WebSocketConfig::default()
            .max_frame_size(None)
            .max_message_size(None);
        match client_with_config(request, stream, Some(config)) {
            Ok((ws, _)) => Ok(ws),
            Err(HandshakeError::Failure(WsError::Http(resp))) => {
                Err(format!("HTTP {}", resp.status().as_u16()))
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// The next text frame from the server, as JSON or as a string.
    fn next_text(ws: &mut Client) -> String {
        loop {
            match ws.read().unwrap() {
                Message::Text(t) => return t.as_str().to_owned(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("expected text, got {other:?}"),
            }
        }
    }

    fn next_json(ws: &mut Client) -> Value {
        serde_json::from_str(&next_text(ws)).unwrap()
    }

    /// Waits for the next log line that starts with `prefix`, skipping others.
    fn expect_log(seen: &Receiver<String>, prefix: &str) -> String {
        loop {
            match seen.recv_timeout(Duration::from_secs(5)) {
                Ok(line) if line.starts_with(prefix) => return line,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    panic!("no {prefix:?} in the inbox")
                }
            }
        }
    }

    #[test]
    fn a_local_origin_connects_and_an_evil_one_gets_403() {
        let (port, seen) = server();
        let mut ws = connect(port, Some("http://localhost:1234")).expect("a local page connects");
        let open = expect_log(&seen, "open ");

        assert_eq!(
            connect(port, Some("https://evil.example")).err().as_deref(),
            Some("HTTP 403")
        );
        connect(port, None).expect("a program without an Origin connects");
        connect(port, Some("null")).expect("a page opened from disk connects");

        // The first connection is unaffected.
        ws.send(Message::text(r#"{"type":"ping"}"#)).unwrap();
        assert_eq!(next_text(&mut ws), r#"{"type":"ping"}"#);
        assert!(open.starts_with("open "));
    }

    #[test]
    fn a_text_frame_reaches_the_inbox_and_the_reply_comes_back() {
        let (port, seen) = server();
        let mut ws = connect(port, Some("http://127.0.0.1:8000")).unwrap();
        let open = expect_log(&seen, "open ");
        let conn = open.trim_start_matches("open ");
        ws.send(Message::text(r#"{"type":"hello","view":"admin"}"#))
            .unwrap();
        assert_eq!(expect_log(&seen, "text "), format!("text {conn} 31 bytes"));
        assert_eq!(next_text(&mut ws), r#"{"type":"hello","view":"admin"}"#);

        ws.close(None).unwrap();
        assert_eq!(
            expect_log(&seen, "close "),
            format!("close {conn} closed by the client")
        );
    }

    #[test]
    fn a_65_kib_frame_gets_an_error_frame_and_the_connection_stays_open() {
        let (port, seen) = server();
        let mut ws = connect(port, None).unwrap();
        expect_log(&seen, "open ");

        ws.send(Message::text("x".repeat(65 * 1024))).unwrap();
        assert_eq!(
            next_json(&mut ws),
            json!({"type": "error", "id": null,
                   "error": "bad request: message too large (66560 bytes, at most 65536)"})
        );
        // Exactly 64 KiB is still allowed, and the connection still works.
        ws.send(Message::text("y".repeat(64 * 1024))).unwrap();
        assert_eq!(next_text(&mut ws).len(), 64 * 1024);
        assert_eq!(expect_log(&seen, "text ").split(' ').nth(2), Some("65536"));

        // A binary frame gets an error too.
        ws.send(Message::binary(vec![1, 2, 3])).unwrap();
        assert_eq!(next_json(&mut ws)["type"], "error");
        ws.send(Message::text("still here")).unwrap();
        assert_eq!(next_text(&mut ws), "still here");
    }

    /// Over tungstenite's limit the stream cannot be read in step any more: the client gets the
    /// error frame, then a close (1009).
    #[test]
    fn a_frame_over_the_library_limit_gets_an_error_then_a_close() {
        let (port, seen) = server();
        let mut ws = connect(port, None).unwrap();
        expect_log(&seen, "open ");
        ws.send(Message::text("z".repeat(MAX_FRAME + 1))).unwrap();
        let error = next_json(&mut ws);
        assert_eq!(error["type"], "error");
        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .starts_with("bad request: message too large"),
            "{error}"
        );
        match ws.read() {
            Ok(Message::Close(Some(frame))) => assert_eq!(frame.code, CloseCode::Size),
            other => panic!("expected a close, got {other:?}"),
        }
        assert!(expect_log(&seen, "close ").contains("limit"));
    }

    /// The hub loop drops a slow connection's outbox; the connection is then closed.
    #[test]
    fn a_dropped_outbox_closes_the_connection() {
        let (port, seen) = server();
        let mut ws = connect(port, None).unwrap();
        expect_log(&seen, "open ");
        ws.send(Message::text("bye")).unwrap();
        match ws.read() {
            Ok(Message::Close(Some(frame))) => assert_eq!(frame.code, CloseCode::Policy),
            other => panic!("expected a close, got {other:?}"),
        }
        assert!(expect_log(&seen, "close ").contains("too slow"));
    }
}
