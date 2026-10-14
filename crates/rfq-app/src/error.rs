//! Errors of the application layer.

/// What a port returns on failure. Boxed, so the node is not generic over adapter error types.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Why the node loop stopped. Both variants are fatal for the running node: drop it and
/// [`start`](crate::Node::start) a new one, which replays the journal.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("journal: {0}")]
    Journal(#[source] BoxError),
    #[error("transport: {0}")]
    Transport(#[source] BoxError),
}
