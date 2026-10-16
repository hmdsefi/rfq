//! What the hub loop tells the HTTP API about the engine: whether it is taking commands, and the
//! newest `seq` the gateway saw on egress (`GET /v1/status`, docs/specs/read-api.md §3).
//!
//! The hub loop owns the [`Hub`](crate::hub::Hub) and stores its state here after every turn; the
//! HTTP threads only read. Two atomics, no lock: a reader may see the state of one turn and the
//! `seq` of the next, which is harmless for a status line.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct EngineStatus {
    up: AtomicBool,
    /// 0 for none: the engine's first `seq` is 1.
    last_seq: AtomicU64,
}

impl EngineStatus {
    /// Down, and no `seq` seen: like a hub that has not heard from the engine yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The hub loop's write path.
    pub fn store(&self, up: bool, last_seq: Option<u64>) {
        self.up.store(up, Ordering::Relaxed);
        self.last_seq.store(last_seq.unwrap_or(0), Ordering::Relaxed);
    }

    /// Whether the engine is up, and the newest `seq` seen on egress (`None` before any).
    pub fn snapshot(&self) -> (bool, Option<u64>) {
        let last = self.last_seq.load(Ordering::Relaxed);
        (self.up.load(Ordering::Relaxed), (last != 0).then_some(last))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_down_with_no_seq_and_shows_what_was_stored() {
        let s = EngineStatus::new();
        assert_eq!(s.snapshot(), (false, None));
        s.store(true, Some(42));
        assert_eq!(s.snapshot(), (true, Some(42)));
        s.store(false, Some(43));
        assert_eq!(s.snapshot(), (false, Some(43)));
        s.store(true, None);
        assert_eq!(s.snapshot(), (true, None));
    }
}
