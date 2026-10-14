use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::types::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContractState {
    Trading,
    Closed,
    Proposed {
        outcome: Outcome,
        dispute_until: Timestamp,
    },
    Disputed {
        proposed: Outcome,
    },
    Final(Outcome),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispute {
    pub disputer: AccountId,
    /// `Some` while the bond is held; `None` once returned or forfeited.
    pub hold: Option<HoldId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contract {
    pub id: ContractId,
    pub description: String,
    pub oracle: AccountId,
    pub trading_close: Timestamp,
    pub resolution_deadline: Timestamp,
    pub dispute_window: Duration,
    pub state: ContractState,
    pub dispute: Option<Dispute>,
    pub positions: Vec<PositionId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectCause {
    Declined,
    Lapsed,
    QuoteExpired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestState {
    Open,
    Presented { accept_by: Timestamp },
    Filled,
    Cancelled,
    Expired,
    Rejected(RejectCause),
}

impl RequestState {
    pub fn is_live(&self) -> bool {
        matches!(self, RequestState::Open | RequestState::Presented { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leg {
    pub contract: ContractId,
    pub side: Side,
    pub qty: Qty,
    pub limit_price: Price,
    /// Live quotes (Active or Selected) on this leg.
    pub quotes: BTreeSet<QuoteId>,
    pub selected: Option<QuoteId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub requester: AccountId,
    pub legs: Vec<Leg>,
    pub response_deadline: Timestamp,
    /// `Some` while live; consumed on accept, released on any other exit.
    pub hold: Option<HoldId>,
    pub state: RequestState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuoteState {
    Active,
    Selected,
    Filled,
    Replaced,
    Cancelled,
    Expired,
    Lost,
    Released,
}

impl QuoteState {
    pub fn is_live(&self) -> bool {
        matches!(self, QuoteState::Active | QuoteState::Selected)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub id: QuoteId,
    pub mm: AccountId,
    pub request: RequestId,
    pub leg: u32,
    pub price: Price,
    pub size: Qty,
    pub expires_at: Timestamp,
    pub hold: Option<HoldId>,
    pub state: QuoteState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub id: PositionId,
    pub contract: ContractId,
    pub request: RequestId,
    pub qty: Qty,
    pub yes_holder: AccountId,
    pub yes_lock: Amount,
    pub no_holder: AccountId,
    pub no_lock: Amount,
    pub settled: bool,
}

/// Variant order is the firing order for timers that share a timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TimerKey {
    QuoteExpiry(QuoteId),
    RequestDeadline(RequestId),
    AcceptBy(RequestId),
    ContractClose(ContractId),
    DisputeWindow(ContractId),
    HardDeadline(ContractId),
}
