//! Protocol DTOs exchanged between clients and the node: what may enter, and what is published.
use rfq_domain::{AccountId, Action, Balance, Command, Emitted, Timestamp};
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

/// Which kind of command produced an [`EventBatch`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BatchKind {
    Genesis,
    Tick,
    Resume,
    User,
}

/// One egress message per applied command that has an origin or events, and per `Resume`, which is
/// always published so clients see every restart and stall (docs/specs/gateway.md §2).
///
/// It carries the whole output of the command: its events, the engine time it ran at, and the
/// balance of every account it changed. Nothing here is state: replay produces the same values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventBatch {
    pub seq: u64,
    /// The sequencer's stamp of the command (`SequencedCommand::time`), in wall-clock ms: engine time.
    pub time: Timestamp,
    /// Which command this was.
    pub kind: BatchKind,
    /// `(actor, nonce)` of the user command that produced these events; `None` for system commands
    /// (`Genesis`, `Tick`, `Resume`).
    ///
    /// It identifies the sender, so it has an audience of its own: a gateway forwards it only to
    /// that actor, like an `Account(actor)` audience, and strips it for everyone else. Otherwise
    /// every subscriber would learn, say, the requester behind a public `RequestOpened`. Egress is
    /// venue-internal, the same trust boundary as event audiences (docs/specs/engine.md §7).
    pub origin: Option<(AccountId, u64)>,
    pub events: Vec<Emitted>,
    /// Every account this command changed (its timers included), with its balance after the
    /// command, in account order. Empty when the command changed no balance, a rejected one for
    /// example. A gateway filters it per viewer, like `origin`: an account sees only its own.
    pub balances: Vec<(AccountId, Balance)>,
}
