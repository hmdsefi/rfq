//! Errors of the application layer.

/// What a port returns on failure. Boxed, so the node is not generic over adapter error types.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Why the node loop stopped. Every variant is fatal for the running node: drop it and
/// [`start`](crate::Node::start) a new one, which replays the journal.
///
/// The message names only the failing side; the cause is the error's `source()`, so a reporter
/// that prints the whole chain (e.g. anyhow's `{:#}`) shows it exactly once.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("journal")]
    Journal(#[source] BoxError),
    #[error("transport")]
    Transport(#[source] BoxError),
    /// An earlier `poll` failed; the node no longer touches its ports.
    #[error("node halted by an earlier failure; restart it from the journal")]
    Halted,
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;

    #[test]
    fn node_error_display_does_not_repeat_its_source() {
        let e = NodeError::Journal("disk full".into());
        assert_eq!(e.to_string(), "journal");
        assert_eq!(e.source().map(|s| s.to_string()).as_deref(), Some("disk full"));
        let e = NodeError::Transport("egress closed".into());
        assert_eq!(e.to_string(), "transport");
        assert_eq!(
            e.source().map(|s| s.to_string()).as_deref(),
            Some("egress closed")
        );
    }
}
