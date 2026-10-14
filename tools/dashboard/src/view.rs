//! What the page renders: snapshot and trace DTOs, filtered for the current viewer with the
//! engine's own event audiences (engine.md §7), which is how quotes stay sealed.
use serde::{Deserialize, Serialize};

use rfq_domain::*;
use rfq_testkit::accounts::*;

use crate::session::{Applied, Session};

/// Who the page shows the world to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Viewer {
    Admin,
    Account(AccountId),
}

impl Viewer {
    /// Whether an event with this audience reaches the viewer.
    pub fn sees(self, audience: &Audience) -> bool {
        match (self, audience) {
            (Viewer::Admin, _) | (_, Audience::Public) => true,
            (Viewer::Account(me), Audience::Account(to)) => me == *to,
        }
    }

    /// Whether the viewer may see `owner`'s private data: their own, or everything as admin.
    pub fn owns(self, owner: AccountId) -> bool {
        match self {
            Viewer::Admin => true,
            Viewer::Account(me) => me == owner,
        }
    }
}

/// Every account the demo uses: id, display name, role.
pub const PARTICIPANTS: [(AccountId, &str, &str); 8] = [
    (OPERATOR, "Operator", "operator"),
    (ARBITER, "Arbiter", "arbiter"),
    (TREASURY, "Treasury", "treasury"),
    (ORACLE, "Oracle", "oracle"),
    (ALICE, "Alice", "requester"),
    (CAROL, "Carol", "requester"),
    (MM1, "MM1", "market_maker"),
    (MM2, "MM2", "market_maker"),
];

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub now: Timestamp,
    pub seq: u64,
    pub state_hash: String,
    pub invariants: Health,
    /// Admin only: it can be a sealed quote's expiry (`QuoteExpiry` timers), so it is `None` for
    /// everyone else.
    pub next_timer: Option<Timestamp>,
    /// Σ (free + reserved + escrowed) over the demo's accounts, for admin only (I1).
    pub total: Option<Amount>,
    pub accounts: Vec<AccountView>,
    pub requests: Vec<RequestView>,
    pub quotes: Vec<QuoteView>,
    /// Public, except each contract's `positions`: the engine announces fills only privately, so a
    /// non-admin viewer's list holds just the position ids they hold.
    pub contracts: Vec<Contract>,
    pub positions: Vec<PositionView>,
}

/// A position as the viewer may see it. The engine names both holders only in `PositionSettled`,
/// so until `settled` a non-admin viewer learns only their own side; the counterparty is `None`.
#[derive(Debug, Serialize)]
pub struct PositionView {
    pub id: PositionId,
    pub contract: ContractId,
    pub request: RequestId,
    pub qty: Qty,
    pub yes_holder: Option<AccountId>,
    pub yes_lock: Amount,
    pub no_holder: Option<AccountId>,
    pub no_lock: Amount,
    pub settled: bool,
}

/// A quote as the engine holds it (the same JSON fields as `Quote`), plus the amount its hold
/// reserves, so the page shows the engine's number instead of computing one.
#[derive(Debug, Serialize)]
pub struct QuoteView {
    #[serde(flatten)]
    pub quote: Quote,
    /// What the quote's hold reserves while it holds one, else `None`.
    ///
    /// This mirrors the engine's `quote_reservation` under `ReservationPolicy::FullHold`, the only
    /// policy: `seller_lock(leg.qty, price)`, on the leg's qty, not the quote's `size`. The engine
    /// keeps that amount on hold, unchanged, until the quote ends.
    pub reserved: Option<Amount>,
}

#[derive(Debug, Serialize)]
pub struct Health {
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AccountView {
    pub id: AccountId,
    pub name: &'static str,
    pub role: &'static str,
    pub balance: Balance,
}

#[derive(Debug, Serialize)]
pub struct RequestView {
    pub id: RequestId,
    /// `None` unless the viewer is the requester or admin.
    pub requester: Option<AccountId>,
    /// The full state for the requester and admin; `None` for everyone else.
    pub state: Option<RequestState>,
    /// `open` while quotes are taken, else `closed`: all a non-owner learns.
    pub public_state: &'static str,
    pub response_deadline: Timestamp,
    pub legs: Vec<LegView>,
    /// The presented picks, for the requester and admin while `Presented`.
    pub picks: Vec<Pick>,
}

#[derive(Debug, Serialize)]
pub struct LegView {
    pub contract: ContractId,
    pub side: Side,
    pub qty: Qty,
    /// The sealed limit: requester and admin only.
    pub limit_price: Option<Price>,
    /// Quotes on this leg the viewer may see: their own, or all for admin.
    pub quotes: Vec<QuoteId>,
    pub selected: Option<QuoteId>,
}

#[derive(Debug, Serialize)]
pub struct TraceEntry {
    pub seq: u64,
    pub time: Timestamp,
    /// `genesis`, `tick`, `resume`, the action name (`SubmitQuote`, …), or `hidden`.
    pub kind: &'static str,
    pub actor: Option<AccountId>,
    pub nonce: Option<u64>,
    /// The full command, when the viewer may see it.
    pub command: Option<Command>,
    pub events: Vec<Emitted>,
}

pub fn snapshot(s: &Session, viewer: Viewer) -> Snapshot {
    let e = s.engine();
    let invariants = match e.check_invariants() {
        Ok(()) => Health {
            ok: true,
            error: None,
        },
        Err(error) => Health {
            ok: false,
            error: Some(error),
        },
    };
    let positions = e
        .contracts()
        .flat_map(|c| c.positions.iter())
        .filter_map(|id| e.position(*id))
        .filter(|p| holds(viewer, p))
        .map(|p| position_view(p, viewer))
        .collect();
    let contracts = e
        .contracts()
        .map(|c| {
            let mut c = c.clone();
            c.positions
                .retain(|id| e.position(*id).is_some_and(|p| holds(viewer, p)));
            c
        })
        .collect();
    Snapshot {
        now: s.now(),
        seq: s.next_seq().saturating_sub(1),
        state_hash: hex(&e.state_hash()),
        invariants,
        next_timer: if viewer == Viewer::Admin {
            e.next_timer()
        } else {
            None
        },
        total: if viewer == Viewer::Admin {
            ledger_total(e)
        } else {
            None
        },
        accounts: PARTICIPANTS
            .iter()
            .filter(|(id, ..)| viewer.owns(*id))
            .map(|&(id, name, role)| AccountView {
                id,
                name,
                role,
                balance: e.balance(id),
            })
            .collect(),
        requests: e.requests().map(|r| request_view(e, r, viewer)).collect(),
        quotes: e
            .quotes()
            .filter(|q| viewer.owns(q.mm))
            .map(|q| quote_view(e, q))
            .collect(),
        contracts,
        positions,
    }
}

pub fn trace(applied: &[Applied], viewer: Viewer) -> Vec<TraceEntry> {
    applied
        .iter()
        .filter_map(|a| {
            let user = match &a.cmd {
                Command::User { actor, nonce, .. } => Some((*actor, *nonce)),
                _ => None,
            };
            let visible = user.is_none_or(|(actor, _)| viewer.owns(actor));
            let events: Vec<Emitted> = a
                .events
                .iter()
                .filter(|e| viewer.sees(&e.audience))
                .cloned()
                .collect();
            // Non-admins don't see other people's commands, nor clock ticks that told them nothing.
            let quiet_system = user.is_none() && viewer != Viewer::Admin;
            if events.is_empty() && (!visible || quiet_system) {
                return None;
            }
            Some(TraceEntry {
                seq: a.seq,
                time: a.time,
                kind: if visible { kind(&a.cmd) } else { "hidden" },
                actor: if visible {
                    user.map(|(actor, _)| actor)
                } else {
                    None
                },
                nonce: if visible {
                    user.map(|(_, nonce)| nonce)
                } else {
                    None
                },
                command: visible.then(|| a.cmd.clone()),
                events,
            })
        })
        .collect()
}

/// Whether the viewer holds either side of the position (admin holds them all).
fn holds(viewer: Viewer, p: &Position) -> bool {
    viewer.owns(p.yes_holder) || viewer.owns(p.no_holder)
}

fn position_view(p: &Position, viewer: Viewer) -> PositionView {
    // Admin owns every holder, so only a non-admin's counterparty is hidden, and only until settled.
    let shown = |holder: AccountId| (p.settled || viewer.owns(holder)).then_some(holder);
    PositionView {
        id: p.id,
        contract: p.contract,
        request: p.request,
        qty: p.qty,
        yes_holder: shown(p.yes_holder),
        yes_lock: p.yes_lock,
        no_holder: shown(p.no_holder),
        no_lock: p.no_lock,
        settled: p.settled,
    }
}

fn quote_view(e: &Engine, q: &Quote) -> QuoteView {
    // A new `ReservationPolicy` variant breaks this match at compile time: `reserved` must then
    // follow the engine's new `quote_reservation`.
    let reserved = match ReservationPolicy::FullHold {
        ReservationPolicy::FullHold => q.hold.and_then(|_| {
            let leg = e.request(q.request)?.legs.get(usize::try_from(q.leg).ok()?)?;
            seller_lock(leg.qty, q.price)
        }),
    };
    QuoteView {
        quote: q.clone(),
        reserved,
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn ledger_total(e: &Engine) -> Option<Amount> {
    PARTICIPANTS.iter().try_fold(0u64, |acc, (id, ..)| {
        let b = e.balance(*id);
        acc.checked_add(b.free)?
            .checked_add(b.reserved)?
            .checked_add(b.escrowed)
    })
}

fn request_view(e: &Engine, r: &Request, viewer: Viewer) -> RequestView {
    let mine = viewer.owns(r.requester);
    let sees_quote = |q: &QuoteId| e.quote(*q).is_some_and(|q| viewer.owns(q.mm));
    let legs = r
        .legs
        .iter()
        .map(|leg| LegView {
            contract: leg.contract,
            side: leg.side,
            qty: leg.qty,
            limit_price: mine.then_some(leg.limit_price),
            quotes: leg.quotes.iter().copied().filter(|q| sees_quote(q)).collect(),
            selected: leg.selected.filter(|q| mine || sees_quote(q)),
        })
        .collect();
    let picks = match r.state {
        RequestState::Presented { .. } if mine => r
            .legs
            .iter()
            .enumerate()
            .filter_map(|(i, leg)| {
                let q = e.quote(leg.selected?)?;
                Some(Pick {
                    leg: u32::try_from(i).ok()?,
                    quote: q.id,
                    price: q.price,
                })
            })
            .collect(),
        _ => Vec::new(),
    };
    RequestView {
        id: r.id,
        requester: mine.then_some(r.requester),
        state: mine.then_some(r.state),
        public_state: if r.state == RequestState::Open {
            "open"
        } else {
            "closed"
        },
        response_deadline: r.response_deadline,
        legs,
        picks,
    }
}

fn kind(cmd: &Command) -> &'static str {
    match cmd {
        Command::Genesis(_) => "genesis",
        Command::Tick => "tick",
        Command::Resume => "resume",
        Command::User { action, .. } => match action {
            Action::Deposit { .. } => "Deposit",
            Action::Withdraw { .. } => "Withdraw",
            Action::ListContract { .. } => "ListContract",
            Action::SubmitRequest { .. } => "SubmitRequest",
            Action::CancelRequest { .. } => "CancelRequest",
            Action::SubmitQuote { .. } => "SubmitQuote",
            Action::CancelQuote { .. } => "CancelQuote",
            Action::Accept { .. } => "Accept",
            Action::Reject { .. } => "Reject",
            Action::ProposeOutcome { .. } => "ProposeOutcome",
            Action::Dispute { .. } => "Dispute",
            Action::Rule { .. } => "Rule",
        },
    }
}
