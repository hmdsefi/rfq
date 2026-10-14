//! Configuration from environment variables.
//!
//! | Variable | Values | Default |
//! |---|---|---|
//! | `AERON_DIR` | the media driver's directory | `/dev/shm/aeron` |
//! | `RFQ_GATEWAY_ADDR` | the WebSocket listen address, `ip:port` | `127.0.0.1:8080` |
//! | `RUST_LOG` | the log level: `error`, `warn`, `info`, `debug` or `trace` | `info` |
//!
//! The gateway listens on `127.0.0.1` only (gateway.md §7). In Docker it binds `0.0.0.0:8080`
//! inside the container, and docker-compose publishes that port on the host's `127.0.0.1` only.
use std::net::SocketAddr;

use anyhow::{Context, bail};

pub const DEFAULT_AERON_DIR: &str = "/dev/shm/aeron";
pub const DEFAULT_ADDR: &str = "127.0.0.1:8080";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub aeron_dir: String,
    pub addr: SocketAddr,
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
        Ok(Config { aeron_dir, addr })
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
        assert_eq!(c.addr, "127.0.0.1:8080".parse::<SocketAddr>().unwrap());
        assert!(c.addr.ip().is_loopback());
    }

    #[test]
    fn reads_every_variable() {
        let c = parse(&[("AERON_DIR", "/tmp/aeron"), ("RFQ_GATEWAY_ADDR", "0.0.0.0:8080")]).unwrap();
        assert_eq!(c.aeron_dir, "/tmp/aeron");
        assert_eq!(c.addr, "0.0.0.0:8080".parse::<SocketAddr>().unwrap());
        let c = parse(&[("RFQ_GATEWAY_ADDR", "[::1]:9000")]).unwrap();
        assert_eq!(c.addr.port(), 9000);
    }

    #[test]
    fn rejects_bad_values() {
        for bad in [
            ("RFQ_GATEWAY_ADDR", "localhost:8080"),
            ("RFQ_GATEWAY_ADDR", "127.0.0.1"),
            ("RFQ_GATEWAY_ADDR", ""),
            ("AERON_DIR", ""),
        ] {
            let err = parse(&[bad]).expect_err(&format!("{bad:?} accepted"));
            assert!(err.to_string().contains(bad.0), "{bad:?}: {err}");
        }
    }
}
