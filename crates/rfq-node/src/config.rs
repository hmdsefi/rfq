//! Configuration from environment variables.
//!
//! | Variable | Values | Default |
//! |---|---|---|
//! | `AERON_DIR` | the media driver's directory | `/dev/shm/aeron` |
//! | `RFQ_JOURNAL` | `archive` (Aeron Archive) or `file:<path>` (append-only file) | `archive` |
//! | `RFQ_HASH_EVERY_BATCH` | `1` logs `applied seq=<n> hash=<hex>` after each batch, `0` does not | `0` |
use std::path::PathBuf;

use anyhow::bail;

pub const DEFAULT_AERON_DIR: &str = "/dev/shm/aeron";

/// Where the journal lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalKind {
    /// Recordings of the log stream in Aeron Archive.
    Archive,
    /// `FileJournal` at this path.
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub aeron_dir: String,
    pub journal: JournalKind,
    pub hash_every_batch: bool,
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
        let journal = match var("RFQ_JOURNAL").as_deref() {
            None | Some("archive") => JournalKind::Archive,
            Some(other) => match other.strip_prefix("file:") {
                Some(path) if !path.is_empty() => JournalKind::File(path.into()),
                _ => bail!("RFQ_JOURNAL must be `archive` or `file:<path>`, got {other:?}"),
            },
        };
        let hash_every_batch = match var("RFQ_HASH_EVERY_BATCH").as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            Some(other) => bail!("RFQ_HASH_EVERY_BATCH must be `0` or `1`, got {other:?}"),
        };
        Ok(Config {
            aeron_dir,
            journal,
            hash_every_batch,
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
    fn defaults() {
        assert_eq!(
            parse(&[]).unwrap(),
            Config {
                aeron_dir: "/dev/shm/aeron".into(),
                journal: JournalKind::Archive,
                hash_every_batch: false,
            }
        );
    }

    #[test]
    fn reads_every_variable() {
        let c = parse(&[
            ("AERON_DIR", "/tmp/aeron"),
            ("RFQ_JOURNAL", "file:/data/rfq.journal"),
            ("RFQ_HASH_EVERY_BATCH", "1"),
        ])
        .unwrap();
        assert_eq!(c.aeron_dir, "/tmp/aeron");
        assert_eq!(c.journal, JournalKind::File("/data/rfq.journal".into()));
        assert!(c.hash_every_batch);
    }

    #[test]
    fn journal_archive_and_hash_off_are_explicit_values() {
        let c = parse(&[("RFQ_JOURNAL", "archive"), ("RFQ_HASH_EVERY_BATCH", "0")]).unwrap();
        assert_eq!(c.journal, JournalKind::Archive);
        assert!(!c.hash_every_batch);
    }

    #[test]
    fn rejects_bad_values() {
        for bad in [
            ("RFQ_JOURNAL", "sqlite"),
            ("RFQ_JOURNAL", "file:"),
            ("RFQ_JOURNAL", ""),
            ("RFQ_HASH_EVERY_BATCH", "yes"),
            ("AERON_DIR", ""),
        ] {
            let err = parse(&[bad]).expect_err(&format!("{bad:?} accepted"));
            assert!(err.to_string().contains(bad.0), "{bad:?}: {err}");
        }
    }
}
