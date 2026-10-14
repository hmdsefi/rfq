//! The sequencer assigns the total order: consecutive `seq`, and a `time` that never goes back.
use rfq_domain::{Command, SequencedCommand, Timestamp};

/// Stamps commands with the next `seq` and the wall clock, clamped so time never moves backwards
/// (not even across a restart, because [`resume`](Sequencer::resume) starts from the journal).
#[derive(Debug)]
pub struct Sequencer {
    last_seq: u64,
    last_time: Timestamp,
}

impl Sequencer {
    /// Continues after the last journaled command (`0, 0` for an empty journal).
    pub fn resume(last_seq: u64, last_time: Timestamp) -> Self {
        Sequencer { last_seq, last_time }
    }

    /// `seq = last_seq + 1`, `time = max(last_time, wall_ms)`.
    pub fn stamp(&mut self, wall_ms: Timestamp, cmd: Command) -> SequencedCommand {
        self.last_seq += 1;
        self.last_time = self.last_time.max(wall_ms);
        SequencedCommand {
            seq: self.last_seq,
            time: self.last_time,
            cmd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequencer_time_is_monotonic() {
        let mut s = Sequencer::resume(10, 1_000);
        assert_eq!(s.stamp(900, Command::Tick).time, 1_000);
        let sc = s.stamp(2_000, Command::Tick);
        assert_eq!((sc.seq, sc.time), (12, 2_000));
    }
}
