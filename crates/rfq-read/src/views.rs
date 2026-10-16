//! The API's read views (docs/specs/read-api.md §3): plain structs serialized as the spec's JSON,
//! and the functions that build them from an [`Engine`].
//!
//! What each one shows is decided here, so sealing lives in one place: an account's views hold only
//! that account's data, the public one only what public events already show, and a request's live
//! quote set appears nowhere. Every list is in id order (the engine's maps are `BTreeMap`s).
use rfq_domain::{
    AccountId, Amount, Balance, Duration, Engine, Market, MarketId, MarketState, Outcome, Position,
    PositionId, Price, Qty, QuoteId, QuoteState, RequestId, RequestState, Side, Timestamp,
};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct AccountView {
    pub account: AccountId,
    pub balance: Balance,
    pub last_nonce: u64,
}

/// Which side of a position an account holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Holding {
    Yes,
    No,
}

/// One position as one of its holders sees it: its own side and stake, no counterparty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PositionView {
    pub id: PositionId,
    pub market: MarketId,
    pub request: RequestId,
    pub qty: Qty,
    pub side: Holding,
    /// What this account locked (`yes_lock` for the YES holder, `no_lock` for the NO holder).
    pub stake: Amount,
    pub settled: bool,
    /// What finalization paid this account; `None` until the position is settled.
    pub payout: Option<Amount>,
}

/// The quote chosen for a leg when its request was presented: all the requester is told of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Selected {
    pub quote: QuoteId,
    pub price: Price,
}

/// A leg of a request, as the requester sees it. Never carries the leg's quote set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct OwnLegView {
    pub market: MarketId,
    pub side: Side,
    pub qty: Qty,
    pub limit_price: Price,
    /// `Some` once the request is presented, else `None`.
    pub selected: Option<Selected>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OwnRequestView {
    pub id: RequestId,
    pub state: RequestState,
    pub response_deadline: Timestamp,
    pub legs: Vec<OwnLegView>,
}

/// A leg as `RequestOpened` already shows it to everyone: no limit price.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PublicLegView {
    pub market: MarketId,
    pub side: Side,
    pub qty: Qty,
}

/// An open request as everyone sees it: no requester, no limit, no quotes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PublicRequestView {
    pub id: RequestId,
    pub response_deadline: Timestamp,
    pub legs: Vec<PublicLegView>,
}

/// A quote as its maker sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct OwnQuoteView {
    pub id: QuoteId,
    pub request: RequestId,
    pub leg: u32,
    pub price: Price,
    pub size: Qty,
    pub expires_at: Timestamp,
    pub state: QuoteState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MarketView {
    pub id: MarketId,
    pub description: String,
    pub oracle: AccountId,
    pub trading_close: Timestamp,
    pub resolution_deadline: Timestamp,
    pub dispute_window: Duration,
    pub state: MarketState,
}

/// The ledger balance (zero for an account the engine never saw) and the nonce of the account's
/// last consumed command (0 for none).
pub fn account(e: &Engine, a: AccountId) -> AccountView {
    AccountView {
        account: a,
        balance: e.balance(a),
        last_nonce: e.last_nonce(a),
    }
}

/// The positions `a` holds a side of, in id order.
pub fn positions_of(e: &Engine, a: AccountId) -> Vec<PositionView> {
    e.positions()
        .flat_map(|p| {
            let yes = (p.yes_holder == a).then(|| position_view(e, p, Holding::Yes));
            let no = (p.no_holder == a).then(|| position_view(e, p, Holding::No));
            yes.into_iter().chain(no)
        })
        .collect()
}

fn position_view(e: &Engine, p: &Position, side: Holding) -> PositionView {
    let stake = match side {
        Holding::Yes => p.yes_lock,
        Holding::No => p.no_lock,
    };
    PositionView {
        id: p.id,
        market: p.market,
        request: p.request,
        qty: p.qty,
        side,
        stake,
        settled: p.settled,
        payout: payout(e, p, side),
    }
}

/// What `finalize` paid the holder of `side`, once the position is settled: the whole pot
/// (`yes_lock + no_lock`) to the winning side and nothing to the other; on `Invalid`, each side its
/// own lock back.
fn payout(e: &Engine, p: &Position, side: Holding) -> Option<Amount> {
    if !p.settled {
        return None;
    }
    let MarketState::Final(outcome) = e.market(p.market)?.state else {
        return None;
    };
    let pot = p.yes_lock.checked_add(p.no_lock)?;
    Some(match (outcome, side) {
        (Outcome::Yes, Holding::Yes) | (Outcome::No, Holding::No) => pot,
        (Outcome::Yes, Holding::No) | (Outcome::No, Holding::Yes) => 0,
        (Outcome::Invalid, Holding::Yes) => p.yes_lock,
        (Outcome::Invalid, Holding::No) => p.no_lock,
    })
}

/// The requests `a` made, in any state, in id order. A leg's `selected` is set once the request was
/// presented; its live quote set is never shown (quotes are sealed until the deadline).
pub fn requests_of(e: &Engine, a: AccountId) -> Vec<OwnRequestView> {
    e.requests()
        .filter(|r| r.requester == a)
        .map(|r| OwnRequestView {
            id: r.id,
            state: r.state,
            response_deadline: r.response_deadline,
            legs: r
                .legs
                .iter()
                .map(|l| OwnLegView {
                    market: l.market,
                    side: l.side,
                    qty: l.qty,
                    limit_price: l.limit_price,
                    selected: l.selected.and_then(|quote| {
                        e.quote(quote).map(|q| Selected {
                            quote,
                            price: q.price,
                        })
                    }),
                })
                .collect(),
        })
        .collect()
}

/// The quotes `a` made, in any state, in id order.
pub fn quotes_of(e: &Engine, a: AccountId) -> Vec<OwnQuoteView> {
    e.quotes()
        .filter(|q| q.mm == a)
        .map(|q| OwnQuoteView {
            id: q.id,
            request: q.request,
            leg: q.leg,
            price: q.price,
            size: q.size,
            expires_at: q.expires_at,
            state: q.state,
        })
        .collect()
}

/// The requests still `Open`, in id order, with public fields only. With `market`, those with a
/// leg on it.
pub fn open_requests(e: &Engine, market: Option<MarketId>) -> Vec<PublicRequestView> {
    e.requests()
        .filter(|r| r.state == RequestState::Open)
        .filter(|r| market.is_none_or(|c| r.legs.iter().any(|l| l.market == c)))
        .map(|r| PublicRequestView {
            id: r.id,
            response_deadline: r.response_deadline,
            legs: r
                .legs
                .iter()
                .map(|l| PublicLegView {
                    market: l.market,
                    side: l.side,
                    qty: l.qty,
                })
                .collect(),
        })
        .collect()
}

/// Every listed market, in id order.
pub fn markets(e: &Engine) -> Vec<MarketView> {
    e.markets().map(market_view).collect()
}

pub fn market(e: &Engine, id: MarketId) -> Option<MarketView> {
    e.market(id).map(market_view)
}

fn market_view(c: &Market) -> MarketView {
    MarketView {
        id: c.id,
        description: c.description.clone(),
        oracle: c.oracle,
        trading_close: c.trading_close,
        resolution_deadline: c.resolution_deadline,
        dispute_window: c.dispute_window,
        state: c.state,
    }
}
