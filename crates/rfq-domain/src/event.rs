use serde::{Deserialize, Serialize};

use crate::command::Pick;
use crate::model::RejectCause;
use crate::types::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Audience {
    Public,
    Account(AccountId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    NotInitialized,
    AlreadyInitialized,
    Duplicate,
    NonceGap,
    Unauthorized,
    UnknownObject,
    InvalidState,
    InvalidInput,
    /// No longer produced: it answered a multi-leg request before multi-leg was built (2026-10-16).
    /// Kept, because events go out on the egress stream encoded by variant index: removing it would renumber the later ones.
    Unsupported,
    SelfTrade,
    InsufficientFunds,
    Overflow,
    Mismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Initialized,
    CommandRejected {
        reason: RejectReason,
    },
    Deposited {
        account: AccountId,
        amount: Amount,
    },
    Withdrawn {
        account: AccountId,
        amount: Amount,
    },
    MarketListed {
        market: MarketId,
    },
    MarketClosed {
        market: MarketId,
    },
    RequestOpened {
        request: RequestId,
        leg: u32,
        market: MarketId,
        side: Side,
        qty: Qty,
        response_deadline: Timestamp,
    },
    RequestCancelled {
        request: RequestId,
    },
    RequestExpired {
        request: RequestId,
    },
    RequestPresented {
        request: RequestId,
        picks: Vec<Pick>,
        accept_by: Timestamp,
    },
    RequestRejected {
        request: RequestId,
        cause: RejectCause,
    },
    RequestFilled {
        request: RequestId,
        positions: Vec<PositionId>,
    },
    QuoteSubmitted {
        quote: QuoteId,
        request: RequestId,
        price: Price,
        expires_at: Timestamp,
    },
    QuoteReplaced {
        old: QuoteId,
        new: QuoteId,
    },
    QuoteCancelled {
        quote: QuoteId,
    },
    QuoteExpired {
        quote: QuoteId,
    },
    QuoteLost {
        quote: QuoteId,
    },
    QuoteSelected {
        quote: QuoteId,
    },
    QuoteReleased {
        quote: QuoteId,
    },
    QuoteFilled {
        quote: QuoteId,
        position: PositionId,
    },
    OutcomeProposed {
        market: MarketId,
        outcome: Outcome,
        dispute_until: Timestamp,
    },
    Disputed {
        market: MarketId,
        disputer: AccountId,
    },
    Ruled {
        market: MarketId,
        outcome: Outcome,
        bond_returned: bool,
    },
    MarketFinalized {
        market: MarketId,
        outcome: Outcome,
    },
    PositionSettled {
        position: PositionId,
        yes_holder: AccountId,
        yes_payout: Amount,
        no_holder: AccountId,
        no_payout: Amount,
    },
    /// The hard deadline finalized a dispute the arbiter never ruled on, and returned the bond to
    /// the disputer. Sent to the disputer only. Not a `Ruled`: nobody ruled, the proposal stands.
    /// (Last variant, so the earlier variants keep their wire discriminants.)
    DisputeBondReturned {
        market: MarketId,
        disputer: AccountId,
        amount: Amount,
    },
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Initialized => "Initialized",
            Event::CommandRejected { .. } => "CommandRejected",
            Event::Deposited { .. } => "Deposited",
            Event::Withdrawn { .. } => "Withdrawn",
            Event::MarketListed { .. } => "MarketListed",
            Event::MarketClosed { .. } => "MarketClosed",
            Event::RequestOpened { .. } => "RequestOpened",
            Event::RequestCancelled { .. } => "RequestCancelled",
            Event::RequestExpired { .. } => "RequestExpired",
            Event::RequestPresented { .. } => "RequestPresented",
            Event::RequestRejected { .. } => "RequestRejected",
            Event::RequestFilled { .. } => "RequestFilled",
            Event::QuoteSubmitted { .. } => "QuoteSubmitted",
            Event::QuoteReplaced { .. } => "QuoteReplaced",
            Event::QuoteCancelled { .. } => "QuoteCancelled",
            Event::QuoteExpired { .. } => "QuoteExpired",
            Event::QuoteLost { .. } => "QuoteLost",
            Event::QuoteSelected { .. } => "QuoteSelected",
            Event::QuoteReleased { .. } => "QuoteReleased",
            Event::QuoteFilled { .. } => "QuoteFilled",
            Event::OutcomeProposed { .. } => "OutcomeProposed",
            Event::Disputed { .. } => "Disputed",
            Event::Ruled { .. } => "Ruled",
            Event::MarketFinalized { .. } => "MarketFinalized",
            Event::PositionSettled { .. } => "PositionSettled",
            Event::DisputeBondReturned { .. } => "DisputeBondReturned",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Emitted {
    pub audience: Audience,
    pub event: Event,
}

impl Emitted {
    pub fn public(event: Event) -> Self {
        Emitted {
            audience: Audience::Public,
            event,
        }
    }
    pub fn to(account: AccountId, event: Event) -> Self {
        Emitted {
            audience: Audience::Account(account),
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RejectCause;

    // Added beyond the brief: the global constraint says `name()` is exactly the variant name, and the
    // scenario tests compare against these strings. Debug output of a variant starts with its name.
    #[test]
    fn name_is_the_variant_name() {
        let (a, c, r, q, p) = (AccountId(1), MarketId(1), RequestId(1), QuoteId(1), PositionId(1));
        let price = Price::new(5_000).unwrap();
        let events = [
            Event::Initialized,
            Event::CommandRejected {
                reason: RejectReason::Duplicate,
            },
            Event::Deposited {
                account: a,
                amount: 1,
            },
            Event::Withdrawn {
                account: a,
                amount: 1,
            },
            Event::MarketListed { market: c },
            Event::MarketClosed { market: c },
            Event::RequestOpened {
                request: r,
                leg: 0,
                market: c,
                side: Side::BuyYes,
                qty: 1,
                response_deadline: 1,
            },
            Event::RequestCancelled { request: r },
            Event::RequestExpired { request: r },
            Event::RequestPresented {
                request: r,
                picks: Vec::new(),
                accept_by: 1,
            },
            Event::RequestRejected {
                request: r,
                cause: RejectCause::Declined,
            },
            Event::RequestFilled {
                request: r,
                positions: Vec::new(),
            },
            Event::QuoteSubmitted {
                quote: q,
                request: r,
                price,
                expires_at: 1,
            },
            Event::QuoteReplaced { old: q, new: q },
            Event::QuoteCancelled { quote: q },
            Event::QuoteExpired { quote: q },
            Event::QuoteLost { quote: q },
            Event::QuoteSelected { quote: q },
            Event::QuoteReleased { quote: q },
            Event::QuoteFilled {
                quote: q,
                position: p,
            },
            Event::OutcomeProposed {
                market: c,
                outcome: Outcome::Yes,
                dispute_until: 1,
            },
            Event::Disputed {
                market: c,
                disputer: a,
            },
            Event::Ruled {
                market: c,
                outcome: Outcome::No,
                bond_returned: true,
            },
            Event::MarketFinalized {
                market: c,
                outcome: Outcome::Invalid,
            },
            Event::PositionSettled {
                position: p,
                yes_holder: a,
                yes_payout: 1,
                no_holder: a,
                no_payout: 0,
            },
            Event::DisputeBondReturned {
                market: c,
                disputer: a,
                amount: 1,
            },
        ];
        assert_eq!(events.len(), 26);
        for e in &events {
            let debug = format!("{e:?}");
            let variant = debug.split([' ', '(', '{']).next().unwrap();
            assert_eq!(e.name(), variant);
        }
    }
}
