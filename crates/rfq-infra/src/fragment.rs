//! Reassembly of Aeron message fragments, without Aeron.
//!
//! A publication splits a message longer than its MTU payload into fragments: the first carries
//! [`BEGIN`], the last [`END`], and a message that fits in one frame carries both
//! ([`UNFRAGMENTED`]). On `aeron:ipc` the default MTU is 1408 bytes, so ordinary commands and
//! event batches can arrive in several fragments. Fragments of one session arrive in order, but
//! a subscription may interleave sessions, so partial messages are kept per session.
//!
//! This mirrors Aeron's `FragmentAssembler`, with two differences: messages over a byte limit are
//! dropped instead of buffered (ingress caps them at [`MAX_INGRESS`](crate::codec::MAX_INGRESS)),
//! and the caller sees every raw fragment, so it can track stream positions itself (the Archive
//! replay stops at a byte position, which may fall inside a message torn by a crash).
use std::collections::BTreeMap;

/// Frame flag of a message's first fragment.
pub const BEGIN: u8 = 0x80;
/// Frame flag of a message's last fragment.
pub const END: u8 = 0x40;
/// Flags of a message that fits in one fragment.
pub const UNFRAGMENTED: u8 = BEGIN | END;

#[derive(Debug)]
enum Partial {
    Assembling(Vec<u8>),
    /// Over the limit: ignore the rest of this message.
    Discarding,
}

/// Joins fragments into whole messages. A session's partial message is replaced by its next
/// `BEGIN` or unfragmented message; fragments that do not follow a `BEGIN` are dropped.
#[derive(Debug)]
pub struct Reassembler {
    limit: usize,
    partial: BTreeMap<i32, Partial>,
}

impl Reassembler {
    /// Delivers only messages of at most `limit` bytes (`usize::MAX` for trusted streams).
    pub fn new(limit: usize) -> Self {
        Reassembler {
            limit,
            partial: BTreeMap::new(),
        }
    }

    /// Feeds one fragment of `session` with frame `flags`; calls `emit` once if it completes a
    /// message.
    pub fn on_fragment(&mut self, session: i32, flags: u8, data: &[u8], emit: &mut dyn FnMut(&[u8])) {
        match flags & UNFRAGMENTED {
            UNFRAGMENTED => {
                self.partial.remove(&session);
                if self.fits(session, data) {
                    emit(data);
                }
            }
            BEGIN => {
                let state = if self.fits(session, data) {
                    Partial::Assembling(data.to_vec())
                } else {
                    Partial::Discarding
                };
                self.partial.insert(session, state);
            }
            middle_or_end => {
                let limit = self.limit;
                let Some(state) = self.partial.get_mut(&session) else {
                    return; // no BEGIN seen for this message
                };
                if let Partial::Assembling(buf) = state {
                    if buf.len().saturating_add(data.len()) <= limit {
                        buf.extend_from_slice(data);
                    } else {
                        tracing::warn!(session, limit, "dropping fragmented message over the size limit");
                        *state = Partial::Discarding;
                    }
                }
                if middle_or_end == END
                    && let Some(Partial::Assembling(buf)) = self.partial.remove(&session)
                {
                    emit(&buf);
                }
            }
        }
    }

    fn fits(&self, session: i32, data: &[u8]) -> bool {
        let fits = data.len() <= self.limit;
        if !fits {
            tracing::warn!(
                session,
                len = data.len(),
                limit = self.limit,
                "dropping message over the size limit"
            );
        }
        fits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIDDLE: u8 = 0;

    fn feed(r: &mut Reassembler, frags: &[(i32, u8, &[u8])]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for &(session, flags, data) in frags {
            r.on_fragment(session, flags, data, &mut |m| out.push(m.to_vec()));
        }
        out
    }

    #[test]
    fn unfragmented_message_passes_through() {
        let mut r = Reassembler::new(1024);
        assert_eq!(feed(&mut r, &[(1, UNFRAGMENTED, b"abc")]), vec![b"abc".to_vec()]);
    }

    #[test]
    fn begin_middle_end_are_joined() {
        let mut r = Reassembler::new(1024);
        let out = feed(&mut r, &[(1, BEGIN, b"ab"), (1, MIDDLE, b"cd"), (1, END, b"ef")]);
        assert_eq!(out, vec![b"abcdef".to_vec()]);
    }

    #[test]
    fn sessions_are_reassembled_independently() {
        let mut r = Reassembler::new(1024);
        let out = feed(
            &mut r,
            &[
                (1, BEGIN, b"a1"),
                (2, BEGIN, b"b1"),
                (2, END, b"b2"),
                (1, END, b"a2"),
            ],
        );
        assert_eq!(out, vec![b"b1b2".to_vec(), b"a1a2".to_vec()]);
    }

    #[test]
    fn fragments_without_a_begin_are_dropped() {
        let mut r = Reassembler::new(1024);
        let out = feed(
            &mut r,
            &[(1, MIDDLE, b"x"), (1, END, b"y"), (1, UNFRAGMENTED, b"ok")],
        );
        assert_eq!(out, vec![b"ok".to_vec()]);
    }

    #[test]
    fn a_new_begin_discards_a_stale_partial() {
        let mut r = Reassembler::new(1024);
        let out = feed(&mut r, &[(1, BEGIN, b"stale"), (1, BEGIN, b"ne"), (1, END, b"w")]);
        assert_eq!(out, vec![b"new".to_vec()]);
    }

    #[test]
    fn an_unfragmented_message_discards_a_stale_partial() {
        let mut r = Reassembler::new(1024);
        let out = feed(
            &mut r,
            &[(1, BEGIN, b"stale"), (1, UNFRAGMENTED, b"ok"), (1, END, b"tail")],
        );
        assert_eq!(out, vec![b"ok".to_vec()]);
    }

    #[test]
    fn message_over_the_limit_is_dropped_and_the_next_one_still_arrives() {
        let mut r = Reassembler::new(4);
        let out = feed(
            &mut r,
            &[
                (1, BEGIN, b"abc"),
                (1, MIDDLE, b"def"),
                (1, END, b"g"),
                (1, UNFRAGMENTED, b"12345"),
                (1, BEGIN, b"ab"),
                (1, END, b"cd"),
            ],
        );
        assert_eq!(out, vec![b"abcd".to_vec()]);
    }

    #[test]
    fn flag_bits_other_than_begin_and_end_are_ignored() {
        let mut r = Reassembler::new(1024);
        let eos = 0x20;
        let out = feed(&mut r, &[(1, BEGIN | eos, b"a"), (1, END | eos, b"b")]);
        assert_eq!(out, vec![b"ab".to_vec()]);
    }
}
