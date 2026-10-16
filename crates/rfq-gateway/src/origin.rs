//! The Origin rule, shared by the WebSocket upgrade and the HTTP API (docs/specs/gateway.md §7,
//! docs/specs/read-api.md §9): a request from a browser page names its origin, and only a local
//! page may talk to the gateway.

/// Whether a request with this `Origin` header (or none) is allowed.
///
/// - No `Origin`: allowed. Browsers always send one on a WebSocket upgrade and on a cross-origin
///   request, so this comes from a program (a Node script, `curl`), which could send any Origin it
///   liked. The check protects against other web sites in the user's browser, which is all an
///   Origin check can do.
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
        .any(|host| match origin.strip_prefix(host) {
            Some("") => true,
            Some(rest) => rest.strip_prefix(':').is_some_and(|port| {
                (1..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit())
            }),
            None => false,
        })
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
}
