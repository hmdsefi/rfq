//! Protocol DTOs exchanged between clients and the node: what may enter, and what is published.
use rfq_domain::{AccountId, Action, Command, Emitted};
use serde::{Deserialize, Serialize};

/// What a client may put on ingress. It cannot express `Genesis`, `Tick` or `Resume`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCommand {
    pub actor: AccountId,
    pub nonce: u64,
    pub action: Action,
}

impl ClientCommand {
    pub fn into_command(self) -> Command {
        Command::User {
            actor: self.actor,
            nonce: self.nonce,
            action: self.action,
        }
    }
}

/// One egress message per applied command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventBatch {
    pub seq: u64,
    /// `(actor, nonce)` of the user command that produced these events; `None` for ticks.
    pub origin: Option<(AccountId, u64)>,
    pub events: Vec<Emitted>,
}
