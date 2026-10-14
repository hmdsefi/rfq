//! Append-only file journal: the local fallback when Aeron Archive is not available.
//!
//! Record format: `[len: u32 LE][crc32(body): u32 LE][body]`, where `body` is
//! [`codec::encode`](crate::codec::encode)`(&SequencedCommand)`. `wait_durable` is `fsync`.
//!
//! Recovery: a scan stops at the first record that is incomplete or fails its checksum and
//! truncates the file there. Such a tail can only come from a crash before `wait_durable`
//! returned, so its commands were never applied or published. Like a database WAL, the scan
//! cannot tell a torn tail from corruption in the middle of the file.
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rfq_app::{BoxError, Journal};
use rfq_domain::SequencedCommand;

use crate::codec::{CodecError, decode, encode};

/// `len` + `crc32`.
const HEADER: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum FileJournalError {
    #[error("open journal {path:?}: {error}")]
    Open { path: PathBuf, error: io::Error },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("journal record at byte {offset} passes its checksum but does not decode: {error}")]
    Corrupt { offset: u64, error: CodecError },
    #[error("journal record of {0} bytes does not fit the u32 length prefix")]
    TooLarge(usize),
}

#[derive(Debug)]
pub struct FileJournal {
    file: File,
    /// Byte offset just past the last good record: the next append goes here.
    end: u64,
    /// Everything before this offset has been fsynced by this handle.
    synced: u64,
}

impl FileJournal {
    /// Opens or creates the journal at `path` and truncates any torn tail, so appends continue
    /// right after the last good record.
    pub fn open(path: &Path) -> Result<Self, FileJournalError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| FileJournalError::Open {
                path: path.to_owned(),
                error,
            })?;
        sync_parent_dir(path)?;
        let mut j = FileJournal {
            file,
            end: 0,
            synced: 0,
        };
        j.scan(&mut |_| {})?;
        Ok(j)
    }

    /// Feeds every good record to `f` in order, then truncates whatever follows the last one.
    fn scan(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> Result<(), FileJournalError> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        let mut rest = bytes.as_slice();
        while let Some((body, after)) = next_record(rest) {
            let offset = (bytes.len() - rest.len()) as u64;
            f(decode(body).map_err(|error| FileJournalError::Corrupt { offset, error })?);
            rest = after;
        }
        let good = (bytes.len() - rest.len()) as u64;
        if !rest.is_empty() {
            tracing::warn!(good, dropped = rest.len(), "truncating torn journal tail");
            self.file.set_len(good)?;
        }
        self.end = good;
        self.file.seek(SeekFrom::Start(good))?;
        Ok(())
    }

    fn write_record(&mut self, sc: &SequencedCommand) -> Result<u64, FileJournalError> {
        let body = encode(sc);
        let len = u32::try_from(body.len()).map_err(|_| FileJournalError::TooLarge(body.len()))?;
        let mut rec = Vec::with_capacity(HEADER + body.len());
        rec.extend(len.to_le_bytes());
        rec.extend(crc32fast::hash(&body).to_le_bytes());
        rec.extend(&body);
        self.file.write_all(&rec)?;
        self.end += rec.len() as u64;
        Ok(self.end)
    }

    fn sync_to(&mut self, pos: u64) -> Result<(), FileJournalError> {
        if pos > self.synced {
            self.file.sync_data()?;
            self.synced = self.end;
        }
        Ok(())
    }
}

impl Journal for FileJournal {
    /// The position is the byte offset just past the new record. After an `Err` the file may end
    /// in a partial record: stop using this handle (reopening truncates it).
    fn append(&mut self, sc: &SequencedCommand) -> Result<u64, BoxError> {
        Ok(self.write_record(sc)?)
    }

    fn wait_durable(&mut self, pos: u64) -> Result<(), BoxError> {
        Ok(self.sync_to(pos)?)
    }

    fn replay(&mut self, f: &mut dyn FnMut(SequencedCommand)) -> Result<(), BoxError> {
        Ok(self.scan(f)?)
    }
}

/// Splits the first complete record whose checksum matches off `bytes`: `(body, rest)`.
fn next_record(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = bytes.split_first_chunk::<4>()?;
    let (crc, rest) = rest.split_first_chunk::<4>()?;
    let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
    // Every body holds at least the codec version byte. A zero length is a zero-filled tail
    // (file extended, data never written), whose checksum would otherwise match: crc32("") == 0.
    if len == 0 {
        return None;
    }
    let (body, rest) = rest.split_at_checked(len)?;
    (crc32fast::hash(body) == u32::from_le_bytes(*crc)).then_some((body, rest))
}

/// Makes the file's directory entry durable, so a newly created journal survives a crash.
fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use rfq_app::{Journal, Sequencer};
    use rfq_domain::Command;

    use super::*;

    #[test]
    fn file_journal_truncates_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let mut j = FileJournal::open(&path).unwrap();
        let mut s = Sequencer::resume(0, 0);
        for _ in 0..3 {
            let pos = j.append(&s.stamp(1, Command::Tick)).unwrap();
            j.wait_durable(pos).unwrap();
        }
        drop(j);
        // Simulate a crash mid-write: append half a header.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&[7, 0])
            .unwrap();
        let mut j = FileJournal::open(&path).unwrap();
        let mut seen = Vec::new();
        j.replay(&mut |sc| seen.push(sc.seq)).unwrap();
        assert_eq!(seen, vec![1, 2, 3]);
        let pos = j.append(&s.stamp(2, Command::Tick)).unwrap();
        j.wait_durable(pos).unwrap();
        let mut seen = Vec::new();
        FileJournal::open(&path)
            .unwrap()
            .replay(&mut |sc| seen.push(sc.seq))
            .unwrap();
        assert_eq!(seen, vec![1, 2, 3, 4], "torn tail truncated, appends continue");
    }

    // Added beyond the brief: a crash can extend the file without writing its data, leaving
    // zeros. Eight zero bytes read as `len = 0, crc = 0`, and crc32 of an empty body is 0.
    #[test]
    fn zero_filled_tail_is_torn_not_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let mut j = FileJournal::open(&path).unwrap();
        let mut s = Sequencer::resume(0, 0);
        let pos = j.append(&s.stamp(1, Command::Tick)).unwrap();
        j.wait_durable(pos).unwrap();
        drop(j);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&[0; 64])
            .unwrap();
        let mut seen = Vec::new();
        FileJournal::open(&path)
            .unwrap()
            .replay(&mut |sc| seen.push(sc.seq))
            .unwrap();
        assert_eq!(seen, vec![1]);
    }
}
