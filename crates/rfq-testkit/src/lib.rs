//! Drives an Engine the way the sequencer would. Used by tests, scenarios and the demo.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::collections::BTreeMap;

use rfq_domain::*;

pub mod scenarios;

pub mod accounts {
    use rfq_domain::AccountId;
    pub const OPERATOR: AccountId = AccountId(1);
    pub const ARBITER: AccountId = AccountId(2);
    pub const TREASURY: AccountId = AccountId(3);
    pub const ORACLE: AccountId = AccountId(4);
    pub const ALICE: AccountId = AccountId(10);
    pub const MM1: AccountId = AccountId(20);
    pub const MM2: AccountId = AccountId(21);
    pub const CAROL: AccountId = AccountId(30);
}
use accounts::*;

pub fn genesis(config: Config) -> Genesis {
    Genesis {
        operator: OPERATOR,
        arbiter: ARBITER,
        treasury: TREASURY,
        config,
    }
}

pub fn dollars(x: u64) -> Amount {
    x * ONE_DOLLAR
}

pub fn price(bps: u16) -> Price {
    Price::new(bps).expect("valid test price")
}

pub fn rejection(events: &[Emitted]) -> Option<RejectReason> {
    events.iter().find_map(|e| match e.event {
        Event::CommandRejected { reason } => Some(reason),
        _ => None,
    })
}

pub fn names(events: &[Emitted]) -> Vec<&'static str> {
    events.iter().map(|e| e.event.name()).collect()
}

/// Sealed bids: every `Quote*` event goes only to that quote's MM, and `RequestPresented` /
/// `RequestFilled` go only to the requester. The request-level events that open or end a request
/// without a fill (`RequestOpened`, `Cancelled`, `Expired`, `Rejected`) are public.
/// `DisputeBondReturned` goes only to its disputer.
///
/// Call it with the engine *after* the command that produced `out`: quotes and requests are never
/// removed, so their owners can always be looked up.
pub fn assert_sealed(h: &Harness, out: &[Emitted]) {
    for e in out {
        let quotes: Vec<QuoteId> = match &e.event {
            Event::QuoteSubmitted { quote, .. }
            | Event::QuoteCancelled { quote }
            | Event::QuoteExpired { quote }
            | Event::QuoteLost { quote }
            | Event::QuoteSelected { quote }
            | Event::QuoteReleased { quote }
            | Event::QuoteFilled { quote, .. } => vec![*quote],
            Event::QuoteReplaced { old, new } => vec![*old, *new],
            other => {
                assert!(
                    !other.name().starts_with("Quote"),
                    "unhandled quote event {other:?}"
                );
                Vec::new()
            }
        };
        for q in quotes {
            let mm = h.engine.quote(q).expect("quote exists").mm;
            assert_eq!(
                e.audience,
                Audience::Account(mm),
                "{:?} must reach only its MM",
                e.event
            );
        }
        if let Event::RequestPresented { request, .. } | Event::RequestFilled { request, .. } = &e.event {
            let requester = h.engine.request(*request).expect("request exists").requester;
            assert_eq!(
                e.audience,
                Audience::Account(requester),
                "{:?} must reach only the requester",
                e.event
            );
        }
        if matches!(
            e.event,
            Event::RequestOpened { .. }
                | Event::RequestCancelled { .. }
                | Event::RequestExpired { .. }
                | Event::RequestRejected { .. }
        ) {
            assert_eq!(e.audience, Audience::Public, "{:?} is request-level", e.event);
        }
        if let Event::DisputeBondReturned { disputer, .. } = e.event {
            assert_eq!(
                e.audience,
                Audience::Account(disputer),
                "{:?} must reach only the disputer",
                e.event
            );
        }
    }
}

pub struct Harness {
    pub engine: Engine,
    pub log: Vec<SequencedCommand>,
    seq: u64,
    time: Timestamp,
    nonces: BTreeMap<AccountId, u64>,
}

impl Harness {
    /// Fresh engine with `Genesis` applied at seq 1, time 0.
    pub fn new(config: Config) -> Self {
        let mut h = Harness {
            engine: Engine::new(),
            log: Vec::new(),
            seq: 0,
            time: 0,
            nonces: BTreeMap::new(),
        };
        let out = h.submit(Command::Genesis(genesis(config)));
        assert_eq!(names(&out), vec!["Initialized"]);
        h
    }

    pub fn now(&self) -> Timestamp {
        self.time
    }

    /// The seq (and therefore the RequestId/QuoteId) the next command will get.
    pub fn next_seq(&self) -> u64 {
        self.seq + 1
    }

    pub fn set_time(&mut self, t: Timestamp) -> &mut Self {
        assert!(t >= self.time, "time must not go backwards");
        self.time = t;
        self
    }

    pub fn submit(&mut self, cmd: Command) -> Vec<Emitted> {
        self.seq += 1;
        let sc = SequencedCommand {
            seq: self.seq,
            time: self.time,
            cmd,
        };
        let out = self.engine.apply(&sc);
        if let Err(e) = self.engine.check_invariants() {
            panic!("invariant violated after seq {}: {e}", self.seq);
        }
        self.log.push(sc);
        out
    }

    pub fn user(&mut self, actor: AccountId, action: Action) -> Vec<Emitted> {
        let n = self.nonces.entry(actor).or_insert(0);
        *n += 1;
        let nonce = *n;
        self.submit(Command::User { actor, nonce, action })
    }

    pub fn tick(&mut self, t: Timestamp) -> Vec<Emitted> {
        self.set_time(t);
        self.submit(Command::Tick)
    }

    pub fn fund(&mut self, to: AccountId, usd: u64) {
        let out = self.user(
            OPERATOR,
            Action::Deposit {
                to,
                amount: dollars(usd),
            },
        );
        assert_eq!(rejection(&out), None);
    }
}
