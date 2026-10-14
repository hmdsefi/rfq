//! `Journal` adapters: [`FileJournal`] (append-only file, the local fallback) and [`MemJournal`]
//! (tests). The Aeron Archive journal comes with the `aeron` feature.
mod file;
mod mem;

pub use file::{FileJournal, FileJournalError};
pub use mem::MemJournal;
