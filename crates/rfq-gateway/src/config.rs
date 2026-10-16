//! Configuration from environment variables.
//!
//! | Variable | Values | Default |
//! |---|---|---|
//! | `AERON_DIR` | the media driver's directory | `/dev/shm/aeron` |
//! | `RFQ_GATEWAY_ADDR` | the WebSocket listen address, `ip:port` | `127.0.0.1:18080` |
//! | `RFQ_HTTP_ADDR` | the HTTP API's listen address, `ip:port` | `127.0.0.1:18081` |
//! | `RUST_LOG` | the log level: `error`, `warn`, `info`, `debug` or `trace` | `info` |
//!
//! The gateway listens on `127.0.0.1` only (gateway.md §7, read-api.md §9). In Docker it binds
//! `0.0.0.0:18080` and `0.0.0.0:18081` inside the container, and docker-compose publishes those
//! ports on the host's `127.0.0.1` only. (18080, not 8080: 8080 is often taken on a developer's
//! machine.)
use std::net::SocketAddr;

use anyhow::{Context, bail};

pub const DEFAULT_AERON_DIR: &str = "/dev/shm/aeron";
pub const DEFAULT_ADDR: &str = "127.0.0.1:18080";
pub const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:18081";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub aeron_dir: String,
    pub addr: SocketAddr,
    /// The HTTP API (read-api.md).
    pub http_addr: SocketAddr,
}

impl Config {
    /// Reads the process environment. A variable that is not valid Unicode counts as unset.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// Reads the variables through `var`, which returns `None` for an unset one.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let aeron_dir = var("AERON_DIR").unwrap_or_else(|| DEFAULT_AERON_DIR.to_owned());
        if aeron_dir.is_empty() {
            bail!("AERON_DIR is set but empty");
        }
        let addr = var("RFQ_GATEWAY_ADDR").unwrap_or_else(|| DEFAULT_ADDR.to_owned());
        let addr = addr
            .parse()
            .with_context(|| format!("RFQ_GATEWAY_ADDR must be `ip:port`, got {addr:?}"))?;
        let http_addr = var("RFQ_HTTP_ADDR").unwrap_or_else(|| DEFAULT_HTTP_ADDR.to_owned());
        let http_addr = http_addr
            .parse()
            .with_context(|| format!("RFQ_HTTP_ADDR must be `ip:port`, got {http_addr:?}"))?;
        Ok(Config {
            aeron_dir,
            addr,
            http_addr,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn parse(vars: &[(&str, &str)]) -> anyhow::Result<Config> {
        let vars: BTreeMap<String, String> =
            vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_vars(|k| vars.get(k).cloned())
    }

    #[test]
    fn defaults_listen_on_localhost_only() {
        let c = parse(&[]).unwrap();
        assert_eq!(c.aeron_dir, "/dev/shm/aeron");
        assert_eq!(c.addr, "127.0.0.1:18080".parse::<SocketAddr>().unwrap());
        assert!(c.addr.ip().is_loopback());
        assert_eq!(c.http_addr, "127.0.0.1:18081".parse::<SocketAddr>().unwrap());
        assert!(c.http_addr.ip().is_loopback());
    }

    #[test]
    fn reads_every_variable() {
        let c = parse(&[
            ("AERON_DIR", "/tmp/aeron"),
            ("RFQ_GATEWAY_ADDR", "0.0.0.0:8080"),
            ("RFQ_HTTP_ADDR", "0.0.0.0:8081"),
        ])
        .unwrap();
        assert_eq!(c.aeron_dir, "/tmp/aeron");
        assert_eq!(c.addr, "0.0.0.0:8080".parse::<SocketAddr>().unwrap());
        assert_eq!(c.http_addr, "0.0.0.0:8081".parse::<SocketAddr>().unwrap());
        let c = parse(&[("RFQ_GATEWAY_ADDR", "[::1]:9000")]).unwrap();
        assert_eq!(c.addr.port(), 9000);
        assert_eq!(c.http_addr.port(), 18_081, "each address has its own variable");
        let c = parse(&[("RFQ_HTTP_ADDR", "[::1]:9001")]).unwrap();
        assert_eq!(c.http_addr.port(), 9001);
        assert_eq!(c.addr.port(), 18_080);
    }

    #[test]
    fn rejects_bad_values() {
        for bad in [
            ("RFQ_GATEWAY_ADDR", "localhost:8080"),
            ("RFQ_GATEWAY_ADDR", "127.0.0.1"),
            ("RFQ_GATEWAY_ADDR", ""),
            ("RFQ_HTTP_ADDR", "localhost:8081"),
            ("RFQ_HTTP_ADDR", "127.0.0.1"),
            ("RFQ_HTTP_ADDR", ""),
            ("AERON_DIR", ""),
        ] {
            let err = parse(&[bad]).expect_err(&format!("{bad:?} accepted"));
            assert!(err.to_string().contains(bad.0), "{bad:?}: {err}");
        }
    }
}
