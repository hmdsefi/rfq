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
    pub next_timer: Option<Timestamp>,
    /// Σ (free + reserved + escrowed) over the demo's accounts, for admin only (I1).
    pub total: Option<Amount>,
    pub accounts: Vec<AccountView>,
    pub requests: Vec<RequestView>,
    pub quotes: Vec<Quote>,
    pub contracts: Vec<Contract>,
    pub positions: Vec<Position>,
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
        .filter(|p| viewer.owns(p.yes_holder) || viewer.owns(p.no_holder))
        .cloned()
        .collect();
    Snapshot {
        now: s.now(),
        seq: s.next_seq().saturating_sub(1),
        state_hash: hex(&e.state_hash()),
        invariants,
        next_timer: e.next_timer(),
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
        quotes: e.quotes().filter(|q| viewer.owns(q.mm)).cloned().collect(),
        contracts: e.contracts().cloned().collect(),
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
