//! The Origin rule, shared by the WebSocket upgrade and the HTTP API (docs/specs/gateway.md §7,
//! docs/specs/read-api.md §9): a request from a browser page names its origin, and only a local
//! page may talk to the gateway. And the HTTP API's Host rule, which stops DNS rebinding.

/// Whether a request with this `Origin` header (or none) is allowed.
///
/// - No `Origin`: allowed. Browsers always send one on a WebSocket upgrade and on a cross-origin
///   request, so this comes from a program (a Node script, `curl`), which could send any Origin it
///   liked, or from a same-origin `GET`, which carries none: [`host_allowed`] covers that one. The
///   check protects against other web sites in the user's browser, which is all an Origin check
///   can do.
/// - `null`: allowed. It is a page opened from disk, but also a sandboxed iframe on any site, so
///   any site could reach the gateway that way: acceptable for a local demo with no
///   authentication (gateway.md §7), not for production.
/// - `http://localhost[:port]` and `http://127.0.0.1[:port]` (a port of 1 to 5 digits): allowed.
/// - Anything else: refused.
pub fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true; // not a browser
    };
    if origin == "null" {
        return true;
    }
    ["http://localhost", "http://127.0.0.1"]
        .iter()
        .any(|host| origin.strip_prefix(host).is_some_and(no_port_or_a_port))
}

/// Whether a request with this `Host` header (or none) may reach the HTTP API.
///
/// A page on another site can make the browser talk to the gateway by DNS rebinding: its name
/// (`evil.example`) is made to resolve to `127.0.0.1`, and its own same-origin `GET`s then carry
/// no `Origin` for [`origin_allowed`] to refuse. They do carry that name in `Host`. So `Host` must
/// name `localhost`, `127.0.0.1` or `[::1]` (in any case), with any port or none, so that a remapped
/// `RFQ_HTTP_PORT` still works. No `Host` passes: browsers always send one.
pub fn host_allowed(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return true; // not a browser
    };
    ["localhost", "127.0.0.1", "[::1]"].iter().any(|name| {
        host.get(..name.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(name))
            && no_port_or_a_port(&host[name.len()..])
    })
}

/// `""`, or `:` and a port of 1 to 5 digits: what may follow the host in an Origin or a Host.
fn no_port_or_a_port(rest: &str) -> bool {
    rest.is_empty()
        || rest
            .strip_prefix(':')
            .is_some_and(|port| (1..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_origins_are_allowed() {
        for origin in [
            None,
            Some("null"),
            Some("http://localhost"),
            Some("http://localhost:1234"),
            Some("http://127.0.0.1:5173"),
        ] {
            assert!(origin_allowed(origin), "{origin:?}");
        }
        for origin in [
            "https://evil.example",
            "http://localhost.evil.example",
            "http://localhost:80@evil.example",
            "http://127.0.0.1.nip.io",
            "http://localhost:",
            "http://localhost:123456",
            "https://localhost:1234",
            "http://192.168.1.2:8080",
            "NULL",
            "",
        ] {
            assert!(!origin_allowed(Some(origin)), "{origin:?}");
        }
    }

    #[test]
    fn only_local_hosts_are_allowed() {
        for host in [
            None,
            Some("localhost"),
            Some("localhost:18081"),
            Some("127.0.0.1"),
            Some("127.0.0.1:28081"),
            Some("[::1]"),
            Some("[::1]:18081"),
            Some("LOCALHOST:1"),
            Some("LocalHost"),
        ] {
            assert!(host_allowed(host), "{host:?}");
        }
        for host in [
            "evil.example",
            "evil.example:18081",
            "127.0.0.1.evil.example",
            "localhost.evil",
            "localhost.",
            "localhost:",
            "localhost:123456",
            "localhost:18081x",
            "localhost@evil.example",
            "0.0.0.0",
            "0.0.0.0:18081",
            "[::1]x",
            "::1",
            "",
        ] {
            assert!(!host_allowed(Some(host)), "{host:?}");
        }
    }
}
