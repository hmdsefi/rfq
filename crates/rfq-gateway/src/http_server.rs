//! The HTTP server (docs/specs/read-api.md §3, §9): one acceptor thread, a thread per connection,
//! on `std::net` and `httparse`. It knows nothing of the API: a parsed [`Request`] goes to the
//! handler, whose [`Reply`] goes back.
//!
//! - **One request per connection.** Every answer carries `Connection: close` and the server then
//!   closes: what the client sent after its first request is not read as a second one.
//! - **Limits** ([`LIMITS`]): at most `max_connections` at once (the next gets `503`), of which at
//!   most `max_writes` are `POST`s (the next `POST` gets the same `503`, before its body is read:
//!   a write can wait long for its answer, and the other connections are left to reads), a head of
//!   at most [`MAX_HEAD`] bytes (`431`) that must complete within `head_timeout` (else the
//!   connection is closed, unanswered), a body of at most `max_body` bytes.
//! - **Bodies** only for `POST`, and only with a `Content-Length` (`411` without one, or with
//!   `Transfer-Encoding`; `413` over the limit, before the body is read; `408` if it stalls for
//!   `head_timeout`). `Expect: 100-continue` is honoured.
//! - **Host** (read-api.md §9): a request whose `Host` fails [`host_allowed`] (a name other than
//!   `localhost`, `127.0.0.1` or `[::1]`: DNS rebinding) gets `403` before anything else, and two
//!   `Host` headers are a `400`.
//! - **Origin** (read-api.md §9): a request whose `Origin` fails
//!   [`origin_allowed`] gets `403` and never reaches the handler. Every other answer to a request
//!   with an `Origin` carries `Access-Control-Allow-Origin` (that origin) and `Vary: Origin`, and
//!   `OPTIONS` is the preflight: `204` with the allowed methods and headers.
//! - **Closing.** After the answer the server shuts down its writing side and reads, for a moment,
//!   what the client is still sending: closing a socket with unread bytes sends a reset, which can
//!   destroy the answer (a `413` to a client that sent its whole body) before the client reads it.
//! - **Failures are per connection.** A handler that panics is a `500` and is logged; its
//!   connection's slot is freed however its thread ends.
//!
//! Each request is logged at info: `http method=… path=… status=… ms=…`, never a body.
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rfq_gateway::origin::{host_allowed, origin_allowed};
use rfq_gateway::protocol::MAX_TEXT;
use rfq_gateway::read_api::Reply;

/// The largest request head (request line and headers) read, in bytes.
pub const MAX_HEAD: usize = 16 * 1024;
/// Headers a request may have.
const MAX_HEADERS: usize = 64;
/// How long a reply may take to write to a client that does not read.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long, and how much, to read from a client after answering it (see the module doc).
const DRAIN: Duration = Duration::from_millis(200);
const DRAIN_MAX: usize = 1 << 20;
/// The same, for a connection refused for being over the limit, in the acceptor's own thread.
const DRAIN_REFUSED: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug)]
pub struct HttpLimits {
    /// Connections served at once; one thread each.
    pub max_connections: usize,
    /// `POST`s served at once, among those connections.
    pub max_writes: usize,
    /// How long a request's head may take to arrive; also how long its body may stall.
    pub head_timeout: Duration,
    /// The largest request body, in bytes.
    pub max_body: usize,
}

/// The gateway's limits (read-api.md §9): 64 connections, of which 48 writes, so that writes
/// waiting on the engine always leave 16 to reads; a head within 10 s; a 64 KiB body.
pub const LIMITS: HttpLimits = HttpLimits {
    max_connections: 64,
    max_writes: 48,
    head_timeout: Duration::from_secs(10),
    max_body: MAX_TEXT,
};
const _: () = assert!(
    LIMITS.max_writes < LIMITS.max_connections,
    "writes must leave room for reads"
);

/// One request, as the handler sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// The request target without its query.
    pub path: String,
    /// The raw query string after `?`; empty if none.
    pub query: String,
    /// The `Origin` header, if any. It has passed [`origin_allowed`].
    pub origin: Option<String>,
    /// The body; empty unless the method is `POST`.
    pub body: Vec<u8>,
    /// The client's address, for the logs (`0.0.0.0:0` if the socket cannot say).
    pub peer: SocketAddr,
}

pub type Handler = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

/// Starts the acceptor thread on `listener`. Each accepted connection gets a thread, up to
/// `limits.max_connections`.
pub fn spawn(listener: TcpListener, limits: HttpLimits, handler: Handler) -> io::Result<JoinHandle<()>> {
    spawn_counting(listener, limits, handler, Arc::new(AtomicUsize::new(0)))
}

/// [`spawn`], with the connections being served counted in `live`: the count the acceptor holds
/// against `limits.max_connections`, which a test can read.
fn spawn_counting(
    listener: TcpListener,
    limits: HttpLimits,
    handler: Handler,
    live: Arc<AtomicUsize>,
) -> io::Result<JoinHandle<()>> {
    // What the compile-time check of `LIMITS` says, for the limits a caller passes in.
    debug_assert!(
        limits.max_writes < limits.max_connections,
        "writes must leave room for reads: max_writes {} is not below max_connections {}",
        limits.max_writes,
        limits.max_connections
    );
    std::thread::Builder::new()
        .name("http-accept".into())
        .spawn(move || accept(&listener, limits, &handler, &live))
}

fn accept(listener: &TcpListener, limits: HttpLimits, handler: &Handler, live: &Arc<AtomicUsize>) {
    let writes = Arc::new(AtomicUsize::new(0));
    let mut next = 0u64;
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                // Out of file descriptors, or a connection reset before it was accepted.
                tracing::warn!(error = %e, "http accept failed");
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        if live.load(Ordering::Relaxed) >= limits.max_connections {
            tracing::warn!(
                peer = %peer_of(&stream),
                max = limits.max_connections,
                "http refused: too many connections"
            );
            refuse(&mut stream);
            continue;
        }
        next += 1;
        let guard = Live::enter(live);
        let handler = Arc::clone(handler);
        let writes = Arc::clone(&writes);
        let spawned = std::thread::Builder::new()
            .name(format!("http-{next}"))
            .spawn(move || {
                let _guard = guard;
                connection(stream, limits, &writes, &*handler);
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "http refused: no thread");
        }
    }
}

/// Counts a connection's thread (or a write) while it lives.
struct Live(Arc<AtomicUsize>);

impl Live {
    fn enter(count: &Arc<AtomicUsize>) -> Live {
        count.fetch_add(1, Ordering::Relaxed);
        Live(Arc::clone(count))
    }

    /// [`enter`](Live::enter), unless `max` are counted already. Connection threads race for
    /// it, so the check and the count are one step.
    fn try_enter(count: &Arc<AtomicUsize>, max: usize) -> Option<Live> {
        count
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < max).then_some(n + 1)
            })
            .ok()?;
        Some(Live(Arc::clone(count)))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn peer_of(stream: &TcpStream) -> String {
    stream
        .peer_addr()
        .map_or_else(|_| "-".to_owned(), |peer| peer.to_string())
}

/// Answers a connection over the limit, from the acceptor's own thread: a short write, then a
/// short read of what the client sent, so that closing does not reset the answer away.
fn refuse(stream: &mut TcpStream) {
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    let reply = Reply::error(503, "too many connections");
    let _ = write_reply(stream, &reply, Cors::None);
    finish(stream, DRAIN_REFUSED);
}

/// What the answer says of the request's `Origin` (read-api.md §9).
#[derive(Clone, Copy)]
enum Cors<'a> {
    /// No `Origin`: a program, not a page.
    None,
    /// An allowed origin: echoed, with `Vary`.
    Origin(&'a str),
    /// The answer to a preflight: the same, and what a page may send.
    Preflight(&'a str),
}

fn write_reply(stream: &mut TcpStream, reply: &Reply, cors: Cors<'_>) -> io::Result<()> {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(256 + reply.body.len());
    let _ = write!(out, "HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
    // A 204 has no body, so no `Content-Length` and no `Content-Type` either.
    let has_body = reply.status != 204;
    if has_body {
        let _ = write!(
            out,
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            reply.body.len()
        );
    }
    out.push_str("Connection: close\r\n");
    if let Cors::Origin(origin) | Cors::Preflight(origin) = cors {
        let _ = write!(out, "Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n");
    }
    if let Cors::Preflight(_) = cors {
        out.push_str(
            "Access-Control-Allow-Methods: GET, POST\r\n\
             Access-Control-Allow-Headers: content-type\r\n\
             Access-Control-Max-Age: 600\r\n",
        );
    }
    out.push_str("\r\n");
    if has_body {
        out.push_str(&reply.body);
    }
    stream.write_all(out.as_bytes())?;
    stream.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// Shuts down the writing side, so the client sees the end of the answer, and reads what it
/// still sends for at most `drain` (and [`DRAIN_MAX`] bytes), so that closing does not reset it.
fn finish(stream: &mut TcpStream, drain: Duration) {
    let _ = stream.shutdown(Shutdown::Write);
    let until = Instant::now() + drain;
    let mut sink = [0u8; 16 * 1024];
    let mut total = 0usize;
    while total < DRAIN_MAX {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return;
        }
        match stream.read(&mut sink) {
            Ok(0) => return,
            Ok(n) => total += n,
            Err(e) if is_timeout(&e) => {}
            Err(_) => return,
        }
    }
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    )
}

/// One connection, from its first byte to its close.
fn connection(
    mut stream: TcpStream,
    limits: HttpLimits,
    writes: &Arc<AtomicUsize>,
    handler: &(dyn Fn(&Request) -> Reply + Send + Sync),
) {
    let started = Instant::now();
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    match serve(&mut stream, &limits, writes, handler) {
        Some(served) => {
            let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            tracing::info!(
                method = %served.method,
                path = %served.path,
                status = served.status,
                ms,
                "http"
            );
            finish(&mut stream, DRAIN);
        }
        None => tracing::debug!(peer = %peer_of(&stream), "http connection closed without a request"),
    }
}

/// A request that was answered, for the log.
struct Served {
    method: String,
    path: String,
    status: u16,
}

/// Why a connection gets no handler call.
enum Failure {
    /// The client went away, or its head timed out: nothing to answer.
    Closed,
    /// Answer this and close.
    Refused(Reply),
}

/// Reads one request, has it answered, and writes the answer. `None` if the connection ended
/// before there was a request to answer. A `POST` is counted in `writes` while it is served.
fn serve(
    stream: &mut TcpStream,
    limits: &HttpLimits,
    writes: &Arc<AtomicUsize>,
    handler: &(dyn Fn(&Request) -> Reply + Send + Sync),
) -> Option<Served> {
    let (buf, head) = match read_head(stream, limits.head_timeout) {
        Ok(read) => read,
        Err(Failure::Closed) => return None,
        Err(Failure::Refused(reply)) => {
            let _ = write_reply(stream, &reply, Cors::None);
            return Some(Served {
                method: "-".into(),
                path: "-".into(),
                status: reply.status,
            });
        }
    };
    let Head { method, path, .. } = &head;
    let respond = |stream: &mut TcpStream, reply: &Reply, cors: Cors<'_>| {
        let _ = write_reply(stream, reply, cors);
        Some(Served {
            method: method.clone(),
            path: path.clone(),
            status: reply.status,
        })
    };

    if !host_allowed(head.host.as_deref()) {
        return respond(stream, &Reply::error(403, "host not allowed"), Cors::None);
    }
    if let Some(origin) = head.origin.as_deref()
        && !origin_allowed(Some(origin))
    {
        return respond(stream, &Reply::error(403, "origin not allowed"), Cors::None);
    }
    let cors = head.origin.as_deref().map_or(Cors::None, Cors::Origin);
    if head.method == "OPTIONS" {
        let preflight = head.origin.as_deref().map_or(Cors::None, Cors::Preflight);
        let reply = Reply {
            status: 204,
            body: String::new(),
        };
        return respond(stream, &reply, preflight);
    }
    // A write's slot is taken before its body is read, and held until it is answered.
    let (_write, body) = if head.method == "POST" {
        let Some(write) = Live::try_enter(writes, limits.max_writes) else {
            tracing::warn!(
                peer = %peer_of(stream),
                max = limits.max_writes,
                "http refused: too many writes"
            );
            return respond(stream, &Reply::error(503, "too many connections"), cors);
        };
        match read_body(stream, &buf, &head, limits) {
            Ok(body) => (Some(write), body),
            Err(Failure::Closed) => return None,
            Err(Failure::Refused(reply)) => return respond(stream, &reply, cors),
        }
    } else {
        (None, Vec::new())
    };

    let request = Request {
        method: head.method.clone(),
        path: head.path.clone(),
        query: head.query.clone(),
        origin: head.origin.clone(),
        body,
        peer: stream
            .peer_addr()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0))),
    };
    let reply = catch_unwind(AssertUnwindSafe(|| handler(&request))).unwrap_or_else(|_| {
        tracing::error!(method = %request.method, path = %request.path, "http handler panicked");
        Reply::error(500, "internal error")
    });
    respond(stream, &reply, cors)
}

/// What a request's head says, owned.
struct Head {
    /// How many bytes of the read buffer the head takes; the body, if any, starts there.
    len: usize,
    method: String,
    path: String,
    query: String,
    host: Option<String>,
    origin: Option<String>,
    length: Length,
    /// A `Transfer-Encoding` header was given (chunked bodies are not supported).
    chunked: bool,
    expect_continue: bool,
}

enum Length {
    Absent,
    Valid(u64),
    /// Not a number, or two headers that disagree.
    Invalid,
}

/// Reads until the head is complete, within `timeout`. Returns what was read, which may include
/// the start of the body.
fn read_head(stream: &mut TcpStream, timeout: Duration) -> Result<(Vec<u8>, Head), Failure> {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        match parse_head(&buf) {
            Ok(Some(head)) => return Ok((buf, head)),
            Ok(None) => {}
            Err(reply) => return Err(Failure::Refused(reply)),
        }
        if buf.len() >= MAX_HEAD {
            let message = format!("request head over {MAX_HEAD} bytes");
            return Err(Failure::Refused(Reply::error(431, &message)));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return Err(Failure::Closed);
        }
        let want = (MAX_HEAD - buf.len()).min(chunk.len());
        match stream.read(&mut chunk[..want]) {
            Ok(0) => return Err(Failure::Closed),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if is_timeout(&e) => {}
            Err(_) => return Err(Failure::Closed),
        }
    }
}

/// `Ok(None)` while the head is incomplete; `Err` is the answer to a head that cannot be one.
fn parse_head(buf: &[u8]) -> Result<Option<Head>, Reply> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut headers);
    let len = match req.parse(buf) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(httparse::Error::TooManyHeaders) => {
            let message = format!("more than {MAX_HEADERS} headers");
            return Err(Reply::error(431, &message));
        }
        Err(e) => return Err(Reply::error(400, &format!("bad request: {e}"))),
    };
    let (Some(method), Some(target)) = (req.method, req.path) else {
        return Err(Reply::error(400, "bad request"));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut head = Head {
        len,
        method: method.to_owned(),
        path: path.to_owned(),
        query: query.to_owned(),
        host: None,
        origin: None,
        length: Length::Absent,
        chunked: false,
        expect_continue: false,
    };
    for header in req.headers.iter() {
        let name = header.name;
        if name.eq_ignore_ascii_case("host") {
            if head.host.is_some() {
                return Err(Reply::error(400, "bad request: more than one Host header"));
            }
            let value = std::str::from_utf8(header.value).unwrap_or("<not UTF-8>");
            head.host = Some(value.trim().to_owned());
        } else if name.eq_ignore_ascii_case("origin") {
            if head.origin.is_none() {
                let value = std::str::from_utf8(header.value).unwrap_or("<not UTF-8>");
                head.origin = Some(value.to_owned());
            }
        } else if name.eq_ignore_ascii_case("content-length") {
            head.length = merge(&head.length, content_length(header.value));
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            head.chunked = true;
        } else if name.eq_ignore_ascii_case("expect") {
            head.expect_continue = header.value.trim_ascii().eq_ignore_ascii_case(b"100-continue");
        }
    }
    Ok(Some(head))
}

/// A `Content-Length` value: decimal digits and nothing else. More digits than a `u64` is a
/// length no body limit allows.
fn content_length(value: &[u8]) -> Length {
    let digits = value.trim_ascii();
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Length::Invalid;
    }
    let parsed = std::str::from_utf8(digits).ok().and_then(|s| s.parse().ok());
    Length::Valid(parsed.unwrap_or(u64::MAX))
}

/// Several `Content-Length` headers are one if they agree, and invalid if not.
fn merge(so_far: &Length, next: Length) -> Length {
    match (so_far, next) {
        (Length::Absent, next) => next,
        (Length::Valid(a), Length::Valid(b)) if *a == b => Length::Valid(b),
        _ => Length::Invalid,
    }
}

/// The body of a `POST`: `Content-Length` bytes, after what `buf` already holds of it.
fn read_body(
    stream: &mut TcpStream,
    buf: &[u8],
    head: &Head,
    limits: &HttpLimits,
) -> Result<Vec<u8>, Failure> {
    let refuse = |status, message: &str| Err(Failure::Refused(Reply::error(status, message)));
    if head.chunked {
        return refuse(411, "chunked bodies are not supported: send a Content-Length");
    }
    let declared = match head.length {
        Length::Absent => return refuse(411, "a Content-Length is required"),
        Length::Invalid => return refuse(400, "the Content-Length is not a number"),
        Length::Valid(n) => n,
    };
    let len = match usize::try_from(declared) {
        Ok(len) if len <= limits.max_body => len,
        _ => {
            let message = format!("request body over the limit of {} bytes", limits.max_body);
            return refuse(413, &message);
        }
    };
    let mut body = buf[head.len..].to_vec();
    body.truncate(len);
    if body.len() == len {
        return Ok(body);
    }
    if head.expect_continue && stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
        return Err(Failure::Closed);
    }
    let deadline = Instant::now() + limits.head_timeout;
    let mut chunk = [0u8; 8192];
    while body.len() < len {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return refuse(408, "the request body did not arrive in time");
        }
        if stream.set_read_timeout(Some(left)).is_err() {
            return Err(Failure::Closed);
        }
        let want = (len - body.len()).min(chunk.len());
        match stream.read(&mut chunk[..want]) {
            Ok(0) => return Err(Failure::Closed),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(e) if is_timeout(&e) => {}
            Err(_) => return Err(Failure::Closed),
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Condvar, Mutex};

    use serde_json::{Value, json};

    use super::*;

    fn limits() -> HttpLimits {
        HttpLimits {
            max_connections: 8,
            max_writes: 6,
            head_timeout: Duration::from_secs(5),
            max_body: 1024,
        }
    }

    /// Answers with the request as JSON, except `/boom` (a panic) and `/status/<n>` (that status).
    fn echo(request: &Request) -> Reply {
        if request.path == "/boom" {
            panic!("a handler that fails");
        }
        if let Some(status) = request.path.strip_prefix("/status/") {
            return Reply::error(status.parse().unwrap(), "as asked");
        }
        let body = json!({
            "method": request.method,
            "path": request.path,
            "query": request.query,
            "origin": request.origin,
            "body": String::from_utf8_lossy(&request.body),
        });
        Reply {
            status: 200,
            body: body.to_string(),
        }
    }

    /// The server's address, and its count of the connections being served.
    fn start_counted(limits: HttpLimits, handler: Handler) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let live = Arc::new(AtomicUsize::new(0));
        spawn_counting(listener, limits, handler, Arc::clone(&live)).unwrap();
        (addr, live)
    }

    fn start_with(limits: HttpLimits, handler: Handler) -> std::net::SocketAddr {
        start_counted(limits, handler).0
    }

    /// Waits until `want` connections are being served. A connection's slot is freed when its
    /// thread sees the client close, just after the client has read the end of the answer, and a
    /// client cannot tell when that is: a test that needs a slot free waits for the count.
    fn wait_for_live(live: &AtomicUsize, want: usize, what: &str) {
        let started = Instant::now();
        while live.load(Ordering::SeqCst) != want {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "{what}: {} connections live, not {want}",
                live.load(Ordering::SeqCst)
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn start(limits: HttpLimits) -> std::net::SocketAddr {
        start_with(limits, Arc::new(echo))
    }

    /// What the server answered: the status, the headers (lower-cased names), the body.
    #[derive(Debug)]
    struct Response {
        raw: String,
        status: u16,
        headers: BTreeMap<String, String>,
        body: String,
    }

    impl Response {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers.get(name).map(String::as_str)
        }

        fn json(&self) -> Value {
            serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {:?}", self.body))
        }
    }

    fn parse_response(raw: Vec<u8>) -> Response {
        let raw = String::from_utf8(raw).unwrap();
        let (head, body) = raw
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("no head in {raw:?}"));
        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap();
        let status = status_line
            .strip_prefix("HTTP/1.1 ")
            .unwrap_or_else(|| panic!("{status_line:?}"))
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let headers = lines
            .map(|l| {
                let (k, v) = l.split_once(": ").unwrap_or_else(|| panic!("{l:?}"));
                (k.to_ascii_lowercase(), v.to_owned())
            })
            .collect();
        Response {
            status,
            headers,
            body: body.to_owned(),
            raw: raw.clone(),
        }
    }

    fn connect(addr: std::net::SocketAddr) -> TcpStream {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream
    }

    /// Reads until the server closes the connection: the answer is complete only then, since
    /// `Connection: close` is the end of it.
    fn read_all(stream: &mut TcpStream) -> Response {
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .expect("the server closes the connection");
        parse_response(raw)
    }

    /// One request on its own connection.
    fn exchange(addr: std::net::SocketAddr, request: &str) -> Response {
        let mut stream = connect(addr);
        stream.write_all(request.as_bytes()).unwrap();
        read_all(&mut stream)
    }

    fn get(path: &str, extra: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{extra}\r\n")
    }

    fn post(path: &str, content_length: Option<usize>, extra: &str) -> String {
        let length = content_length.map_or_else(String::new, |n| format!("Content-Length: {n}\r\n"));
        format!("POST {path} HTTP/1.1\r\nHost: localhost\r\n{length}{extra}\r\n")
    }

    /// Whether the writes of `waiting_writes` may go on, and what wakes them.
    type Gate = Arc<(Mutex<bool>, Condvar)>;

    /// A handler whose `POST`s wait, as writes do for a hung engine or an actor held behind an
    /// unresolved nonce: each counts itself in `entered`, then waits until the gate is opened.
    fn waiting_writes() -> (Handler, Gate, Arc<AtomicUsize>) {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let entered = Arc::new(AtomicUsize::new(0));
        let handler: Handler = {
            let (gate, entered) = (Arc::clone(&gate), Arc::clone(&entered));
            Arc::new(move |request: &Request| {
                if request.method == "POST" {
                    entered.fetch_add(1, Ordering::SeqCst);
                    let (open, opened) = &*gate;
                    let mut open = open.lock().unwrap();
                    while !*open {
                        open = opened.wait(open).unwrap();
                    }
                }
                echo(request)
            })
        };
        (handler, gate, entered)
    }

    /// Sends `n` writes and waits until all are in the handler.
    fn hold_writes(addr: std::net::SocketAddr, entered: &AtomicUsize, n: usize) -> Vec<TcpStream> {
        let held = (0..n)
            .map(|_| {
                let mut stream = connect(addr);
                let write = format!("{}{{}}", post("/v1/commands", Some(2), ""));
                stream.write_all(write.as_bytes()).unwrap();
                stream
            })
            .collect();
        let started = Instant::now();
        while entered.load(Ordering::SeqCst) < n {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "the writes never reached the handler"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        held
    }

    #[test]
    fn one_request_is_answered_with_connection_close_and_the_connection_closes() {
        let addr = start(limits());
        let r = exchange(addr, &get("/v1/status?min_seq=3&x", ""));
        assert_eq!(r.status, 200);
        assert!(r.raw.starts_with("HTTP/1.1 200 OK\r\n"), "{}", r.raw);
        assert_eq!(r.header("connection"), Some("close"));
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert_eq!(
            r.header("content-length"),
            Some(r.body.len().to_string().as_str())
        );
        assert_eq!(
            r.json(),
            json!({"method": "GET", "path": "/v1/status", "query": "min_seq=3&x", "origin": null, "body": ""})
        );
        assert!(
            r.header("access-control-allow-origin").is_none(),
            "no Origin, no CORS headers"
        );
        assert!(r.header("vary").is_none());

        // A second request on the same connection is not read: one answer, then the close.
        let mut stream = connect(addr);
        let two = format!("{}{}", get("/first", ""), get("/second", ""));
        stream.write_all(two.as_bytes()).unwrap();
        let r = read_all(&mut stream);
        assert_eq!(r.raw.matches("HTTP/1.1 ").count(), 1, "{}", r.raw);
        assert_eq!(r.json()["path"], "/first");
    }

    #[test]
    fn the_handlers_status_decides_the_status_line() {
        let addr = start(limits());
        for (status, line) in [
            (400, "400 Bad Request"),
            (404, "404 Not Found"),
            (405, "405 Method Not Allowed"),
            (422, "422 Unprocessable Content"),
            (429, "429 Too Many Requests"),
            (503, "503 Service Unavailable"),
            (504, "504 Gateway Timeout"),
        ] {
            let r = exchange(addr, &get(&format!("/status/{status}"), ""));
            assert!(r.raw.starts_with(&format!("HTTP/1.1 {line}\r\n")), "{}", r.raw);
            assert_eq!(r.json(), json!({"error": "as asked"}));
        }
    }

    #[test]
    fn a_foreign_origin_gets_403_and_never_reaches_the_handler() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let addr = start_with(
            limits(),
            Arc::new(move |r| {
                counted.fetch_add(1, Ordering::Relaxed);
                echo(r)
            }),
        );
        for origin in [
            "http://evil.example",
            "http://localhost.evil.example",
            "https://localhost",
            "",
        ] {
            let r = exchange(addr, &get("/v1/status", &format!("Origin: {origin}\r\n")));
            assert_eq!(r.status, 403, "{origin:?}");
            assert_eq!(r.json(), json!({"error": "origin not allowed"}));
            assert!(r.header("access-control-allow-origin").is_none(), "{origin:?}");
        }
        // Not even a preflight, and not a POST (its body is never read or acted on).
        let r = exchange(
            addr,
            "OPTIONS /v1/commands HTTP/1.1\r\nOrigin: http://evil.example\r\n\r\n",
        );
        assert_eq!(r.status, 403);
        let r = exchange(
            addr,
            &post("/v1/commands", Some(2), "Origin: http://evil.example\r\n"),
        );
        assert_eq!(r.status, 403);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    /// DNS rebinding: a page whose name resolves to 127.0.0.1 sends same-origin `GET`s with no
    /// `Origin`, but with its own name in `Host`.
    #[test]
    fn a_foreign_host_gets_403_before_anything_else_and_never_reaches_the_handler() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let addr = start_with(
            limits(),
            Arc::new(move |r| {
                counted.fetch_add(1, Ordering::Relaxed);
                echo(r)
            }),
        );
        let request = |method: &str, host: &str, extra: &str| {
            format!("{method} /v1/trace?view=admin HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n")
        };
        for (what, raw) in [
            ("a GET", request("GET", "evil.example:18081", "")),
            ("no port", request("GET", "evil.example", "")),
            ("a lookalike", request("GET", "127.0.0.1.evil.example:18081", "")),
            // Checked before the Origin rule: an allowed Origin does not let it through.
            (
                "an allowed Origin",
                request("GET", "evil.example:18081", "Origin: http://localhost:18081\r\n"),
            ),
            (
                "a preflight",
                request("OPTIONS", "evil.example:18081", "Origin: null\r\n"),
            ),
            (
                "a POST",
                request("POST", "evil.example:18081", "Content-Length: 2\r\n"),
            ),
        ] {
            let r = exchange(addr, &raw);
            assert_eq!(r.status, 403, "{what}");
            assert_eq!(r.json(), json!({"error": "host not allowed"}), "{what}");
            assert!(r.header("access-control-allow-origin").is_none(), "{what}");
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0, "no handler call");

        // A local name with any port, and no Host at all (HTTP/1.0, a program), reach the handler.
        for host in ["127.0.0.1:1234", "localhost", "[::1]:18081", "LOCALHOST:28081"] {
            let r = exchange(addr, &request("GET", host, ""));
            assert_eq!(r.status, 200, "{host}");
        }
        assert_eq!(exchange(addr, "GET /v1/status HTTP/1.0\r\n\r\n").status, 200);
        assert_eq!(calls.load(Ordering::Relaxed), 5);

        // Two Host headers are not a request.
        let r = exchange(
            addr,
            "GET /v1/status HTTP/1.1\r\nHost: localhost\r\nHost: evil.example\r\n\r\n",
        );
        assert_eq!(r.status, 400);
        assert_eq!(
            r.json(),
            json!({"error": "bad request: more than one Host header"})
        );
        assert_eq!(calls.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn an_allowed_origin_is_echoed_with_vary() {
        let addr = start(limits());
        for origin in [
            "null",
            "http://localhost:5173",
            "http://127.0.0.1",
            "http://127.0.0.1:18081",
        ] {
            let r = exchange(addr, &get("/v1/status", &format!("Origin: {origin}\r\n")));
            assert_eq!(r.status, 200, "{origin}");
            assert_eq!(r.header("access-control-allow-origin"), Some(origin));
            assert_eq!(r.header("vary"), Some("Origin"));
            assert_eq!(r.json()["origin"], origin, "the handler sees it too");
        }
        // The headers are on errors too: a page must be able to read them.
        let r = exchange(addr, &get("/status/404", "Origin: null\r\n"));
        assert_eq!(r.status, 404);
        assert_eq!(r.header("access-control-allow-origin"), Some("null"));
        assert_eq!(r.header("vary"), Some("Origin"));
    }

    #[test]
    fn options_from_an_allowed_origin_is_a_preflight_204() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let addr = start_with(
            limits(),
            Arc::new(move |r| {
                counted.fetch_add(1, Ordering::Relaxed);
                echo(r)
            }),
        );
        let r = exchange(
            addr,
            "OPTIONS /v1/commands HTTP/1.1\r\nHost: localhost\r\nOrigin: http://localhost:5173\r\n\
             Access-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: content-type\r\n\r\n",
        );
        assert!(r.raw.starts_with("HTTP/1.1 204 No Content\r\n"), "{}", r.raw);
        assert_eq!(
            r.header("access-control-allow-origin"),
            Some("http://localhost:5173")
        );
        assert_eq!(r.header("vary"), Some("Origin"));
        assert_eq!(r.header("access-control-allow-methods"), Some("GET, POST"));
        assert_eq!(r.header("access-control-allow-headers"), Some("content-type"));
        assert_eq!(r.header("access-control-max-age"), Some("600"));
        assert_eq!(r.header("connection"), Some("close"));
        assert!(r.header("content-length").is_none() && r.header("content-type").is_none());
        assert_eq!(r.body, "");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "the server answers a preflight itself"
        );

        let r = exchange(addr, "OPTIONS /v1/commands HTTP/1.1\r\nOrigin: null\r\n\r\n");
        assert_eq!(r.status, 204);
        assert_eq!(r.header("access-control-allow-origin"), Some("null"));
    }

    #[test]
    fn a_post_body_reaches_the_handler_whole() {
        let addr = start(limits());
        let body = r#"{"actor": 10, "legs": []}"#;
        let r = exchange(
            addr,
            &format!("{}{body}", post("/v1/requests", Some(body.len()), "")),
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["method"], "POST");
        assert_eq!(r.json()["body"], body);

        // The body may arrive after the head, in pieces.
        let mut stream = connect(addr);
        stream
            .write_all(post("/v1/quotes", Some(10), "").as_bytes())
            .unwrap();
        std::thread::sleep(Duration::from_millis(30));
        stream.write_all(b"01234").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        stream.write_all(b"56789 and more that is not the body").unwrap();
        let r = read_all(&mut stream);
        assert_eq!(r.json()["body"], "0123456789");

        // Exactly the limit is fine, an empty body too, and a GET's Content-Length is not read.
        let full = "x".repeat(1024);
        let r = exchange(addr, &format!("{}{full}", post("/v1/commands", Some(1024), "")));
        assert_eq!(
            (r.status, r.json()["body"].as_str().map(str::len)),
            (200, Some(1024))
        );
        let r = exchange(addr, &post("/v1/commands", Some(0), ""));
        assert_eq!((r.status, r.json()["body"].as_str()), (200, Some("")));
        let r = exchange(addr, "GET /v1/status HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello");
        assert_eq!((r.status, r.json()["body"].as_str()), (200, Some("")));
    }

    #[test]
    fn expect_100_continue_is_honoured() {
        let addr = start(limits());
        let mut stream = connect(addr);
        stream
            .write_all(post("/v1/commands", Some(5), "Expect: 100-continue\r\n").as_bytes())
            .unwrap();
        let mut interim = [0u8; 25];
        stream.read_exact(&mut interim).unwrap();
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        stream.write_all(b"hello").unwrap();
        let r = read_all(&mut stream);
        assert_eq!((r.status, r.json()["body"].as_str()), (200, Some("hello")));

        // A body over the limit is refused without asking for it.
        let r = exchange(
            addr,
            &post("/v1/commands", Some(5_000), "Expect: 100-continue\r\n"),
        );
        assert_eq!(r.status, 413);
        assert!(!r.raw.contains("100 Continue"));
    }

    #[test]
    fn a_body_over_the_limit_is_413_even_when_the_client_sends_it_all() {
        let addr = start(limits());
        // The whole 64 KiB body goes out before the answer is read: the 413 must survive.
        let big = "y".repeat(64 * 1024);
        let r = exchange(
            addr,
            &format!("{}{big}", post("/v1/commands", Some(big.len()), "")),
        );
        assert_eq!(r.status, 413);
        assert!(r.json()["error"].as_str().unwrap().contains("1024"), "{}", r.body);
        // Refused on the Content-Length alone: the body need not exist.
        let r = exchange(addr, &post("/v1/commands", Some(10_000_000), ""));
        assert_eq!(r.status, 413);
        let r = exchange(
            addr,
            &post(
                "/v1/commands",
                None,
                "Content-Length: 99999999999999999999999\r\n",
            ),
        );
        assert_eq!(r.status, 413);
        // One byte over.
        let r = exchange(addr, &post("/v1/commands", Some(1025), ""));
        assert_eq!(r.status, 413);
    }

    #[test]
    fn a_post_without_content_length_is_411() {
        let addr = start(limits());
        let r = exchange(addr, &post("/v1/requests", None, ""));
        assert_eq!(r.status, 411);
        assert!(r.json()["error"].as_str().unwrap().contains("Content-Length"));
        // No chunked bodies.
        let r = exchange(
            addr,
            &post("/v1/requests", None, "Transfer-Encoding: chunked\r\n"),
        );
        assert_eq!(r.status, 411);
        let r = exchange(
            addr,
            &post("/v1/requests", Some(5), "Transfer-Encoding: chunked\r\n"),
        );
        assert_eq!(r.status, 411);
        // A GET needs none.
        assert_eq!(exchange(addr, &get("/v1/status", "")).status, 200);
    }

    #[test]
    fn a_bad_content_length_is_400() {
        let addr = start(limits());
        for value in ["abc", "-1", "+5", "1.5", "", "5, 6"] {
            let r = exchange(
                addr,
                &post("/v1/commands", None, &format!("Content-Length: {value}\r\n")),
            );
            assert_eq!(r.status, 400, "{value:?}");
        }
        // Two different lengths: refused. Two equal ones are one.
        let r = exchange(addr, &post("/v1/commands", Some(5), "Content-Length: 6\r\n"));
        assert_eq!(r.status, 400);
        let r = exchange(
            addr,
            &format!("{}hello", post("/v1/commands", Some(5), "Content-Length: 5\r\n")),
        );
        assert_eq!(r.status, 200);
    }

    #[test]
    fn a_head_that_never_completes_is_closed_after_the_head_timeout() {
        let addr = start(HttpLimits {
            head_timeout: Duration::from_millis(200),
            ..limits()
        });
        // Half a head, then nothing.
        let mut stream = connect(addr);
        stream.write_all(b"GET /v1/status HTTP/1.1\r\nHost: loc").unwrap();
        let started = Instant::now();
        let mut rest = Vec::new();
        let n = stream.read_to_end(&mut rest).expect("closed, not reset");
        assert_eq!(
            n,
            0,
            "closed without an answer: {:?}",
            String::from_utf8_lossy(&rest)
        );
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(180), "closed too early: {took:?}");
        assert!(took < Duration::from_secs(2), "closed too late: {took:?}");

        // Nothing at all.
        let mut stream = connect(addr);
        let started = Instant::now();
        assert_eq!(stream.read_to_end(&mut Vec::new()).unwrap(), 0);
        assert!(started.elapsed() >= Duration::from_millis(180));
    }

    #[test]
    fn a_head_trickled_in_is_still_cut_off_at_the_head_timeout() {
        let addr = start(HttpLimits {
            head_timeout: Duration::from_millis(300),
            ..limits()
        });
        let mut stream = connect(addr);
        stream.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
        let started = Instant::now();
        let closed = loop {
            // A byte every 50 ms never completes the head, and never lets one read time out.
            if stream.write_all(b"X").is_err() {
                break true;
            }
            std::thread::sleep(Duration::from_millis(50));
            let mut buf = [0u8; 1];
            match stream.read(&mut buf) {
                Ok(0) => break true,
                Ok(_) => panic!("an answer to half a head"),
                Err(_) => {}
            }
            assert!(started.elapsed() < Duration::from_secs(3), "never closed");
        };
        assert!(closed);
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_stalled_body_is_a_408() {
        let addr = start(HttpLimits {
            head_timeout: Duration::from_millis(200),
            ..limits()
        });
        let mut stream = connect(addr);
        stream
            .write_all(post("/v1/commands", Some(10), "").as_bytes())
            .unwrap();
        stream.write_all(b"abc").unwrap();
        let started = Instant::now();
        let r = read_all(&mut stream);
        assert_eq!(r.status, 408);
        assert!(started.elapsed() >= Duration::from_millis(180));
    }

    #[test]
    fn the_connection_over_the_limit_gets_503_and_a_slot_frees_when_a_client_leaves() {
        let addr = start(HttpLimits {
            max_connections: 2,
            max_writes: 1,
            ..limits()
        });
        // Two idle connections fill the server.
        let idle: Vec<TcpStream> = (0..2).map(|_| connect(addr)).collect();
        let r = exchange(addr, &get("/v1/status", ""));
        assert_eq!(r.status, 503);
        assert_eq!(r.json(), json!({"error": "too many connections"}));
        assert_eq!(r.header("connection"), Some("close"));
        // Still full, and still refusing, however it is asked.
        assert_eq!(exchange(addr, &post("/v1/commands", Some(2), "")).status, 503);

        // One client leaves: its slot is free again (its thread sees the close).
        drop(idle);
        let started = Instant::now();
        loop {
            let r = exchange(addr, &get("/v1/status", ""));
            if r.status == 200 {
                break;
            }
            assert_eq!(r.status, 503);
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "the slots never freed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Writes that wait (a hung engine, an actor held behind an unresolved nonce) take at most
    /// `max_writes` of the connections: the rest are always there for reads.
    #[test]
    fn writes_over_their_cap_get_503_and_reads_still_get_in() {
        let (handler, gate, entered) = waiting_writes();
        let (addr, live) = start_counted(
            HttpLimits {
                max_connections: 4,
                max_writes: 2,
                ..limits()
            },
            handler,
        );
        let write = |extra: &str| format!("{}{{}}", post("/v1/commands", Some(2), extra));

        // Two writes that wait fill the writes' share.
        let held = hold_writes(addr, &entered, 2);

        // The next write is refused at once, as a connection over the limit is, and a page can
        // read the refusal.
        let r = exchange(addr, &write(""));
        assert_eq!(r.status, 503);
        assert_eq!(r.json(), json!({"error": "too many connections"}));
        assert_eq!(r.header("connection"), Some("close"));
        let r = exchange(addr, &write("Origin: null\r\n"));
        assert_eq!(r.status, 503);
        assert_eq!(r.header("access-control-allow-origin"), Some("null"));
        assert_eq!(
            entered.load(Ordering::SeqCst),
            2,
            "a refused write never reaches the handler"
        );

        // The two connections left are the reads': one idle, one answered. The gate is still
        // closed, so the two writes keep their slots; but a refused write's connection frees its
        // own only when its thread sees the client close, just after `exchange` has returned. Wait
        // until only the writes are counted, so that `idle` is let in (not silently refused) and
        // the `GET` after it has the last slot, however the threads are scheduled.
        wait_for_live(&live, 2, "the refused writes' connections");
        let idle = connect(addr);
        wait_for_live(&live, 3, "the idle connection");
        let r = exchange(addr, &get("/v1/status", ""));
        assert_eq!(r.status, 200, "{}", r.raw);
        drop(idle);

        // Let the writes through: they are answered, and their slots are free again.
        let (open, opened) = &*gate;
        *open.lock().unwrap() = true;
        opened.notify_all();
        for mut stream in held {
            assert_eq!(read_all(&mut stream).status, 200);
        }
        let started = Instant::now();
        loop {
            let r = exchange(addr, &write(""));
            if r.status == 200 {
                break;
            }
            assert_eq!(r.status, 503);
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "the write slots never freed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A `POST` takes its write slot before its body is read, and gives it back however the
    /// request ends early: refused with a `411`, `413`, `400` or `408`, or left by a client in the
    /// middle of its body. With one write slot, a slot not given back refuses every `POST` after.
    #[test]
    fn a_write_that_ends_early_gives_its_slot_back() {
        let handled = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&handled);
        let addr = start_with(
            HttpLimits {
                max_writes: 1,
                head_timeout: Duration::from_millis(500),
                ..limits()
            },
            Arc::new(move |request| {
                if request.method == "POST" {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
                echo(request)
            }),
        );
        let half = format!("{}abc", post("/v1/commands", Some(10), ""));
        // What the client sends, and the status it is answered with (`None`: it leaves first).
        let cases = [
            ("no Content-Length", post("/v1/commands", None, ""), Some(411)),
            (
                "a chunked body",
                post("/v1/commands", None, "Transfer-Encoding: chunked\r\n"),
                Some(411),
            ),
            (
                "a body over the limit",
                post("/v1/commands", Some(5_000), ""),
                Some(413),
            ),
            (
                "a Content-Length that is not a number",
                post("/v1/commands", None, "Content-Length: abc\r\n"),
                Some(400),
            ),
            ("a body that never completes", half.clone(), Some(408)),
            ("a client that leaves mid-body", half, None),
        ];
        for (before, (what, request, answer)) in cases.into_iter().enumerate() {
            let mut stream = connect(addr);
            stream.write_all(request.as_bytes()).unwrap();
            match answer {
                Some(status) => assert_eq!(read_all(&mut stream).status, status, "{what}"),
                None => drop(stream),
            }

            // The slot is given back when the connection's thread ends, which the client cannot
            // see: a `POST` straight after may be refused a few times, but gets through.
            let started = Instant::now();
            loop {
                let r = exchange(addr, &format!("{}{{}}", post("/v1/commands", Some(2), "")));
                if r.status == 200 {
                    break;
                }
                assert_eq!(r.status, 503, "{what}: {}", r.raw);
                assert_eq!(r.json(), json!({"error": "too many connections"}), "{what}");
                assert!(
                    started.elapsed() < Duration::from_secs(3),
                    "{what}: the write slot was never given back"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(
                handled.load(Ordering::SeqCst),
                before + 1,
                "{what}: only the normal POSTs reach the handler"
            );
        }
    }

    /// What is not a write served takes no write slot: with the writes at their cap, a preflight
    /// is still answered, and so is a `POST` that is refused for its `Origin` or `Host` (a `403`,
    /// not the `503` of a write over its cap).
    #[test]
    fn a_preflight_and_a_forbidden_post_are_answered_with_the_writes_at_their_cap() {
        let (handler, gate, entered) = waiting_writes();
        let addr = start_with(
            HttpLimits {
                max_writes: 1,
                ..limits()
            },
            handler,
        );
        let held = hold_writes(addr, &entered, 1);

        // At the cap: another write is refused.
        let r = exchange(addr, &format!("{}{{}}", post("/v1/commands", Some(2), "")));
        assert_eq!(r.status, 503);

        let r = exchange(
            addr,
            "OPTIONS /v1/commands HTTP/1.1\r\nHost: localhost\r\nOrigin: http://localhost:5173\r\n\
             Access-Control-Request-Method: POST\r\n\r\n",
        );
        assert_eq!(r.status, 204, "{}", r.raw);
        assert_eq!(
            r.header("access-control-allow-origin"),
            Some("http://localhost:5173")
        );
        assert_eq!(r.header("access-control-allow-methods"), Some("GET, POST"));

        let r = exchange(
            addr,
            &post("/v1/commands", Some(2), "Origin: http://evil.example\r\n"),
        );
        assert_eq!(r.status, 403, "{}", r.raw);
        assert_eq!(r.json(), json!({"error": "origin not allowed"}));

        let r = exchange(
            addr,
            "POST /v1/commands HTTP/1.1\r\nHost: evil.example\r\nContent-Length: 2\r\n\r\n{}",
        );
        assert_eq!(r.status, 403, "{}", r.raw);
        assert_eq!(r.json(), json!({"error": "host not allowed"}));

        assert_eq!(
            entered.load(Ordering::SeqCst),
            1,
            "only the held write reached the handler"
        );
        let (open, opened) = &*gate;
        *open.lock().unwrap() = true;
        opened.notify_all();
        for mut stream in held {
            assert_eq!(read_all(&mut stream).status, 200);
        }
    }

    /// `LIMITS` is checked at compile time; the limits a server is started with are checked when
    /// it starts (in a debug build, as the tests are): writes that may take every connection
    /// leave none for reads.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "writes must leave room for reads")]
    fn limits_that_let_writes_take_every_connection_are_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let limits = HttpLimits {
            max_connections: 4,
            max_writes: 4,
            ..limits()
        };
        let _ = spawn(listener, limits, Arc::new(echo));
    }

    #[test]
    fn a_finished_request_frees_its_slot() {
        // One connection, for reads only: writes must leave room for reads.
        let addr = start(HttpLimits {
            max_connections: 1,
            max_writes: 0,
            ..limits()
        });
        for _ in 0..20 {
            let r = exchange(addr, &get("/v1/status", ""));
            // The slot is freed when the connection's thread ends, just after the close; a very
            // quick next client may see the previous one still counted, once in a while.
            assert!(r.status == 200 || r.status == 503, "{}", r.raw);
        }
        let started = Instant::now();
        while exchange(addr, &get("/v1/status", "")).status != 200 {
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }

    #[test]
    fn a_head_over_16_kib_is_431_and_a_malformed_one_is_400() {
        let addr = start(limits());
        let r = exchange(
            addr,
            &get("/v1/status", &format!("X-Filler: {}\r\n", "a".repeat(20_000))),
        );
        assert_eq!(r.status, 431);
        let many: String = (0..100).map(|i| format!("X-{i}: 1\r\n")).collect();
        assert_eq!(
            exchange(addr, &get("/v1/status", &many)).status,
            431,
            "too many headers"
        );

        for garbage in [
            "garbage\r\n\r\n",
            "GET\r\n\r\n",
            "GET /v1/status HTTP/9.9\r\n\r\n",
            "\x01\x02\r\n\r\n",
        ] {
            let r = exchange(addr, garbage);
            assert_eq!(r.status, 400, "{garbage:?}");
            assert!(r.json()["error"].is_string());
        }
    }

    #[test]
    fn a_panicking_handler_is_a_500_and_the_server_goes_on() {
        // One connection, for reads only: writes must leave room for reads.
        let addr = start(HttpLimits {
            max_connections: 1,
            max_writes: 0,
            ..limits()
        });
        let r = exchange(addr, &get("/boom", ""));
        assert_eq!(r.status, 500);
        assert_eq!(r.json(), json!({"error": "internal error"}));
        // Its slot was freed (one connection at a time here).
        let started = Instant::now();
        while exchange(addr, &get("/v1/status", "")).status != 200 {
            assert!(started.elapsed() < Duration::from_secs(2), "the slot leaked");
        }
    }

    #[test]
    fn http_1_0_and_odd_methods_reach_the_handler() {
        let addr = start(limits());
        let r = exchange(addr, "GET /v1/status HTTP/1.0\r\n\r\n");
        assert_eq!(r.status, 200);
        assert_eq!(r.header("connection"), Some("close"));
        for method in ["DELETE", "PUT", "PATCH"] {
            let r = exchange(addr, &format!("{method} /v1/status HTTP/1.1\r\n\r\n"));
            assert_eq!(r.status, 200, "{method}");
            assert_eq!(r.json()["method"], method);
        }
        // A path with no query, and an empty query.
        let r = exchange(addr, &get("/v1/status?", ""));
        assert_eq!(
            (r.json()["path"].as_str(), r.json()["query"].as_str()),
            (Some("/v1/status"), Some(""))
        );
    }
}
