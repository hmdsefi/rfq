//! In-memory journal for tests: "durable" as soon as it is appended.
use rfq_app::{BoxError, Journal};
use rfq_domain::SequencedCommand;

#[derive(Clone, Debug, Default)]
pub struct MemJournal {
    entries: Vec<SequencedCommand>,
}

impl MemJournal {
    pub fn entries(&self) -> &[SequencedCommand] {
        &self.entries
    }
}

impl Journal for MemJournal {
    /// The position is the number of entries after the append.
    fn append(&mut self, sc: &SequencedCommand) -> Result<u64, BoxError> {
        self.entries.push(sc.clone());
        Ok(self.entries.len() as u64)
    }

    fn wait_durable(&mut self, _pos: u64) -> Result<(), BoxError> {
        Ok(())
    }

    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> Result<(), BoxError> {
        self.entries.iter().cloned().for_each(f);
        Ok(())
    }
}
