use serde::{Deserialize, Serialize};

use crate::types::*;

/// How much collateral a live quote reserves. The decision for days-long quotes (D5).
///
/// Data, not a generic: the choice is part of `Genesis`, so it is in the log and replay stays
/// deterministic. A new variant must change several engine sites. `Engine::quote_reservation` lists
/// them, and each is a compile-time tripwire, so the compiler forces a decision at each listed site.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReservationPolicy {
    /// Each quote holds its full counterparty collateral `qty × (1 − p)` until it ends.
    #[default]
    FullHold,
}

/// When and how quotes are compared (D5).
///
/// Data, not a generic: the choice is part of `Genesis`, so it is in the log and replay stays
/// deterministic. A new variant must change several engine sites. `Engine::on_request_deadline` lists
/// them, and each is a compile-time tripwire, so the compiler forces a decision at each listed site.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelectionPolicy {
    /// Quotes are sealed until `response_deadline`; the best eligible one per leg is presented once.
    #[default]
    SealedAtDeadline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub accept_window: Duration,
    pub min_response: Duration,
    pub max_response: Duration,
    pub max_qty: Qty,
    pub dispute_bond: Amount,
    pub resume_grace: Duration,
    pub min_request_notional: Amount,
    pub reservation: ReservationPolicy,
    pub selection: SelectionPolicy,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            accept_window: 30_000,
            min_response: 1_000,
            max_response: 7 * 24 * 3_600_000,
            max_qty: 1_000_000_000,
            dispute_bond: 100 * ONE_DOLLAR,
            resume_grace: 600_000,
            min_request_notional: ONE_DOLLAR,
            reservation: ReservationPolicy::default(),
            selection: SelectionPolicy::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Genesis {
    pub operator: AccountId,
    pub arbiter: AccountId,
    pub treasury: AccountId,
    pub config: Config,
}

/// The most legs one request may have (engine.md §8). More, or none, is `InvalidInput`.
///
/// A request's legs all live in one engine and one command validates and escrows every one of them,
/// so the cap bounds the work of that command (and the size of one `RequestOpened` burst).
pub const MAX_LEGS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegSpec {
    pub contract: ContractId,
    pub side: Side,
    pub qty: Qty,
    pub limit_price: Price,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pick {
    pub leg: u32,
    pub quote: QuoteId,
    pub price: Price,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Deposit {
        to: AccountId,
        amount: Amount,
    },
    Withdraw {
        amount: Amount,
    },
    ListContract {
        id: ContractId,
        description: String,
        oracle: AccountId,
        trading_close: Timestamp,
        resolution_deadline: Timestamp,
        dispute_window: Duration,
    },
    SubmitRequest {
        legs: Vec<LegSpec>,
        response_deadline: Timestamp,
    },
    CancelRequest {
        request: RequestId,
    },
    SubmitQuote {
        request: RequestId,
        leg: u32,
        price: Price,
        size: Qty,
        expires_at: Timestamp,
    },
    CancelQuote {
        quote: QuoteId,
    },
    Accept {
        request: RequestId,
        picks: Vec<Pick>,
    },
    Reject {
        request: RequestId,
    },
    ProposeOutcome {
        contract: ContractId,
        outcome: Outcome,
    },
    Dispute {
        contract: ContractId,
    },
    Rule {
        contract: ContractId,
        outcome: Outcome,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Genesis(Genesis),
    Tick,
    Resume,
    User {
        actor: AccountId,
        nonce: u64,
        action: Action,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequencedCommand {
    pub seq: u64,
    pub time: Timestamp,
    pub cmd: Command,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Added beyond the brief: pins the Global Constraints defaults, field by field.
    #[test]
    fn default_config_matches_global_constraints() {
        assert_eq!(
            Config::default(),
            Config {
                accept_window: 30_000,
                min_response: 1_000,
                max_response: 604_800_000,
                max_qty: 1_000_000_000,
                dispute_bond: 100_000_000,
                resume_grace: 600_000,
                min_request_notional: 1_000_000,
                reservation: ReservationPolicy::FullHold,
                selection: SelectionPolicy::SealedAtDeadline,
            }
        );
    }

    // The D5 seams are data in `Genesis` (so they are in the log): they must survive the codec.
    #[test]
    fn genesis_with_policies_round_trips_through_serde() {
        let genesis = Genesis {
            operator: AccountId(1),
            arbiter: AccountId(2),
            treasury: AccountId(3),
            config: Config {
                reservation: ReservationPolicy::FullHold,
                selection: SelectionPolicy::SealedAtDeadline,
                ..Config::default()
            },
        };
        let bytes = bincode::serialize(&genesis).expect("genesis serializes");
        let back: Genesis = bincode::deserialize(&bytes).expect("genesis deserializes");
        assert_eq!(back, genesis);
        assert_eq!(back.config.reservation, ReservationPolicy::FullHold);
        assert_eq!(back.config.selection, SelectionPolicy::SealedAtDeadline);
    }

    #[test]
    fn policy_defaults_are_full_hold_and_sealed_at_deadline() {
        assert_eq!(ReservationPolicy::default(), ReservationPolicy::FullHold);
        assert_eq!(SelectionPolicy::default(), SelectionPolicy::SealedAtDeadline);
    }
}
