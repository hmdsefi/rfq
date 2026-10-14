use serde::{Deserialize, Serialize};

use crate::types::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub accept_window: Duration,
    pub min_response: Duration,
    pub max_response: Duration,
    pub max_qty: Qty,
    pub dispute_bond: Amount,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            accept_window: 30_000,
            min_response: 1_000,
            max_response: 7 * 24 * 3_600_000,
            max_qty: 1_000_000_000,
            dispute_bond: 100 * ONE_DOLLAR,
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
