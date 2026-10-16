//! `rfq-gateway`: the engine's client edge (docs/specs/gateway.md). Clients speak JSON over a
//! WebSocket; the gateway puts their commands on ingress and sends every egress batch back,
//! filtered for each connection's view.
//!
//! Four kinds of threads (gateway.md §9, read-api.md §2):
//! - the hub loop ([`hub_loop`]), which owns the [`Hub`](rfq_gateway::hub::Hub), the ingress
//!   publication and the egress subscription ([`aeron`]);
//! - the WebSocket acceptor, and one thread per connection ([`ws`]);
//! - the HTTP acceptor, and one thread per connection ([`http_server`]): `GET` reads the replica;
//!   `POST` runs a command through the hub loop's inbox like a WebSocket connection ([`http_write`]),
//!   with connection ids from the counter it shares with the WebSocket acceptor;
//! - the replica thread ([`read_side`]), which follows the journal into the replica the HTTP
//!   reads come from, with an Aeron client of its own.
//!
//! It reads its configuration from the environment ([`config`]). A failure of the Aeron client,
//! the media driver or a publication is fatal: it is logged and the process exits with status 1
//! (docker-compose restarts it). A dead engine is not: the gateway reports it to its clients as
//! `status down` and keeps serving.
//!
//! Built without the `aeron` feature, it only says so and exits with status 2.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::process::ExitCode;
use std::sync::Arc;

use rfq_gateway::read_api::{self, ReadSide};
use rfq_gateway::status::EngineStatus;
use rfq_gateway::write_api;

// Only the Aeron runtime uses these; they are tested either way.
#[cfg_attr(not(feature = "aeron"), allow(dead_code))]
mod config;
#[cfg_attr(not(feature = "aeron"), allow(dead_code))]
mod http_server;
#[cfg_attr(not(feature = "aeron"), allow(dead_code))]
mod http_write;
#[cfg_attr(not(feature = "aeron"), allow(dead_code))]
mod hub_loop;
#[cfg_attr(not(feature = "aeron"), allow(dead_code))]
mod read_side;
#[cfg_attr(not(feature = "aeron"), allow(dead_code))]
mod ws;

#[cfg(feature = "aeron")]
mod aeron;

/// The HTTP API's handler: `GET` reads the replica (`min_seq` waits at most
/// [`read_api::MIN_SEQ_WAIT`]); `POST` on a write route runs a command through the hub; any other
/// request is a `405` (a path the API has) or a `404`.
#[cfg_attr(not(feature = "aeron"), allow(dead_code))]
fn http_handler(
    read: Arc<ReadSide>,
    engine: Arc<EngineStatus>,
    write: Arc<http_write::Writer>,
) -> http_server::Handler {
    Arc::new(move |request| match request.method.as_str() {
        "GET" => read_api::get(
            &read,
            &engine,
            &request.path,
            &request.query,
            read_api::MIN_SEQ_WAIT,
        ),
        "POST" if write_api::is_write_route(&request.path) => write.post(request),
        _ => read_api::other_method(&request.path),
    })
}

#[cfg(not(feature = "aeron"))]
fn main() -> ExitCode {
    eprintln!(
        "rfq-gateway was built without Aeron support; build it with `--features aeron` \
         (docker/rust.Dockerfile does)"
    );
    ExitCode::from(2)
}

#[cfg(feature = "aeron")]
fn main() -> ExitCode {
    use std::io::IsTerminal;

    use tracing_subscriber::filter::LevelFilter;

    // `RUST_LOG` is one level for everything (`debug`, say); anything else means `info`.
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|l| l.parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    match run() {
        Ok(never) => match never {},
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "aeron")]
fn run() -> anyhow::Result<std::convert::Infallible> {
    use std::net::TcpListener;
    use std::sync::mpsc;

    use anyhow::Context;

    let config = config::Config::from_env()?;
    tracing::info!(?config, "starting rfq-gateway");
    let client = rfq_infra::aeron::connect(&config.aeron_dir)
        .with_context(|| format!("connect to the media driver at {}", config.aeron_dir))?;
    let venue = aeron::AeronVenue::open(client)?;
    let listener = TcpListener::bind(config.addr).with_context(|| format!("listen on {}", config.addr))?;
    tracing::info!(addr = %listener.local_addr()?, "gateway listening");
    let http = TcpListener::bind(config.http_addr)
        .with_context(|| format!("listen on {} for HTTP", config.http_addr))?;
    tracing::info!(addr = %http.local_addr()?, "http listening");

    let engine = Arc::new(EngineStatus::new());
    let read = ReadSide::new();
    read_side::spawn(config.aeron_dir.clone(), Arc::clone(&read)).context("start the replica thread")?;
    // The hub loop's inbox, and the one counter of connection ids, are shared by the WebSocket
    // acceptor and the HTTP writes: both enter the hub the same way (read-api.md §4).
    let ids = ws::ConnIds::default();
    let (inbox, items) = mpsc::sync_channel(ws::INBOX);
    let writer = http_write::Writer::new(inbox.clone(), ids.clone(), http_write::WAIT);
    let handler = http_handler(read, Arc::clone(&engine), Arc::new(writer));
    http_server::spawn(http, http_server::LIMITS, handler).context("start the HTTP server")?;

    ws::spawn(listener, inbox, ids).context("start the WebSocket acceptor")?;
    hub_loop::HubLoop::new(venue, items, engine).run()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    use rfq_testkit::scenarios::{all, run_in_memory};
    use serde_json::Value;

    use super::*;

    /// The real server and the real router over a replica that has run the happy path (14
    /// commands). No hub loop runs: the write handler's inbox has no reader (the round trips
    /// through a real hub loop are in `hub_loop`'s tests).
    fn start() -> std::net::SocketAddr {
        let happy = all().into_iter().find(|s| s.name == "happy_path").unwrap();
        let log = run_in_memory(&happy, 1_000).0.log;
        let read = ReadSide::new();
        log.iter().for_each(|sc| read.apply(sc).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (inbox, _stopped) = std::sync::mpsc::sync_channel(1);
        let writer = http_write::Writer::new(inbox, ws::ConnIds::default(), http_write::WAIT);
        let handler = http_handler(read, Arc::new(EngineStatus::new()), Arc::new(writer));
        http_server::spawn(listener, http_server::LIMITS, handler).unwrap();
        addr
    }

    /// The status, the JSON body (null if it is not JSON) and the head of one request's answer.
    fn call(addr: std::net::SocketAddr, request: &str) -> (u16, Value, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let body = serde_json::from_str(body).unwrap_or(Value::Null);
        (status, body, head.to_owned())
    }

    #[test]
    fn the_read_api_answers_over_a_real_socket() {
        let addr = start();
        let (status, body, head) = call(addr, "GET /v1/accounts/10/positions?min_seq=14 HTTP/1.1\r\n\r\n");
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["as_of_seq"], 14);
        assert_eq!(body["positions"][0]["side"], "Yes");
        assert!(head.contains("Connection: close"));

        let (status, body, _) = call(addr, "GET /v1/status HTTP/1.1\r\nOrigin: null\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(body["replica"]["last_seq"], 14);
        assert_eq!(body["engine"], "down", "no hub loop here");

        let (status, body, _) = call(addr, "GET /v1/contracts/999 HTTP/1.1\r\n\r\n");
        assert_eq!((status, body["error"].as_str()), (404, Some("no such contract")));
        assert_eq!(call(addr, "GET /v1/nope HTTP/1.1\r\n\r\n").0, 404);
        assert_eq!(call(addr, "GET /v1/accounts/ten HTTP/1.1\r\n\r\n").0, 400);
    }

    #[test]
    fn other_methods_and_foreign_origins_are_refused() {
        let addr = start();
        // A method a path does not serve is 405 if the API has the path, 404 if not.
        let post = |path: &str| format!("POST {path} HTTP/1.1\r\nContent-Length: 2\r\n\r\n{{}}");
        assert_eq!(
            call(addr, &post("/v1/status")).0,
            405,
            "POST is for the write routes"
        );
        assert_eq!(call(addr, &post("/v1/accounts/10")).0, 405);
        assert_eq!(call(addr, &post("/v1/nope")).0, 404);
        assert_eq!(call(addr, &post("/v1/requests/")).0, 404, "not the write route");
        assert_eq!(call(addr, "GET /v1/quotes HTTP/1.1\r\n\r\n").0, 405);
        assert_eq!(call(addr, "GET /v1/commands HTTP/1.1\r\n\r\n").0, 405);
        assert_eq!(call(addr, "DELETE /v1/contracts HTTP/1.1\r\n\r\n").0, 405);
        assert_eq!(call(addr, "DELETE /v1/requests HTTP/1.1\r\n\r\n").0, 405);
        assert_eq!(call(addr, "PUT /v1/commands HTTP/1.1\r\n\r\n").0, 405);
        assert_eq!(call(addr, "DELETE /v1/nope HTTP/1.1\r\n\r\n").0, 404);

        let evil = "GET /v1/status HTTP/1.1\r\nOrigin: http://evil.example\r\n\r\n";
        let (status, body, _) = call(addr, evil);
        assert_eq!(
            (status, body["error"].as_str()),
            (403, Some("origin not allowed"))
        );
        let preflight = "OPTIONS /v1/commands HTTP/1.1\r\nOrigin: http://localhost:3000\r\n\r\n";
        let (status, _, head) = call(addr, preflight);
        assert_eq!(status, 204);
        assert!(
            head.contains("Access-Control-Allow-Origin: http://localhost:3000"),
            "{head}"
        );
    }

    /// The write routes over a real socket, as far as they go without a hub loop: bad bodies, the
    /// foreign origin, and a hub loop that has stopped.
    #[test]
    fn the_write_routes_answer_over_a_real_socket() {
        let addr = start();
        let post = |path: &str, extra: &str, body: &str| {
            let request = format!(
                "POST {path} HTTP/1.1\r\n{extra}Content-Length: {}\r\n\r\n{body}",
                body.len()
            );
            call(addr, &request)
        };
        for (path, body, why) in [
            ("/v1/requests", "{}", "bad request: missing field"),
            (
                "/v1/quotes",
                r#"{"actor":20,"colour":"red"}"#,
                "bad request: unknown field",
            ),
            (
                "/v1/commands",
                "[]",
                "bad request: the body must be a JSON object",
            ),
            ("/v1/commands", "", "bad request: the body must be a JSON object"),
            ("/v1/commands", r#"{"actor":1,"action":"Tick"}"#, "bad request: "),
        ] {
            let (status, reply, _) = post(path, "", body);
            assert_eq!(status, 400, "{path} {body}: {reply}");
            assert!(
                reply["error"].as_str().unwrap().starts_with(why),
                "{path} {body}: {reply}"
            );
        }

        // A page's answer carries its origin, an error included.
        let (status, _, head) = post("/v1/requests", "Origin: http://localhost:3000\r\n", "{}");
        assert_eq!(status, 400);
        assert!(
            head.contains("Access-Control-Allow-Origin: http://localhost:3000"),
            "{head}"
        );

        // A foreign page never reaches the handler, however good its body.
        let good = r#"{"actor":10,"action":{"Withdraw":{"amount":1}}}"#;
        let (status, reply, _) = post("/v1/commands", "Origin: http://evil.example\r\n", good);
        assert_eq!(
            (status, reply["error"].as_str()),
            (403, Some("origin not allowed"))
        );

        // A good body with no hub loop behind the inbox: the gateway is going down.
        let (status, reply, _) = post("/v1/commands", "", good);
        assert_eq!(
            (status, reply["error"].as_str()),
            (503, Some("the gateway is stopping"))
        );
    }
}
