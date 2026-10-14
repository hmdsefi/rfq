//! Splitting an egress batch so that each message fits the transport's maximum message length.
//!
//! The codec never limits what the node produces (ruling R21), but an Aeron publication rejects a
//! message longer than its max message length. An [`EventBatch`] over that limit is published as
//! several chunks with the same `seq` and `origin`; their events, concatenated in order, are the
//! original events. A consumer that needs the whole batch joins consecutive chunks with equal `seq`.
use rfq_app::EventBatch;

use crate::codec::encoded_len;

/// Splits `batch` into chunks whose encodings are at most `max_bytes` each, filling every chunk
/// greedily in event order. A batch that already fits, or has no events, is returned as the only
/// chunk. A single event too big for any chunk travels alone, in a chunk over `max_bytes`; the
/// transport's publish of that chunk then fails.
pub fn split_batch(batch: &EventBatch, max_bytes: usize) -> Vec<EventBatch> {
    if batch.events.is_empty() || encoded_len(batch) <= max_bytes {
        return vec![batch.clone()];
    }
    let empty = EventBatch {
        seq: batch.seq,
        origin: batch.origin,
        events: Vec::new(),
    };
    // bincode (fixint) writes a Vec as a fixed-width length followed by its elements, so a chunk's
    // size is the empty batch's size plus its events' sizes. `encoded_len` counts the version byte,
    // which a message carries once, not once per event.
    let base = encoded_len(&empty);
    let mut chunks = Vec::new();
    let mut current = empty.clone();
    let mut size = base;
    for event in &batch.events {
        let len = encoded_len(event) - 1;
        if !current.events.is_empty() && size + len > max_bytes {
            debug_assert_eq!(size, encoded_len(&current));
            chunks.push(std::mem::replace(&mut current, empty.clone()));
            size = base;
        }
        current.events.push(event.clone());
        size += len;
    }
    debug_assert_eq!(size, encoded_len(&current));
    chunks.push(current);
    chunks
}

#[cfg(test)]
mod tests {
    use rfq_domain::{AccountId, Emitted, Event};

    use super::*;
    use crate::codec::{decode, encode};

    fn deposit(i: u64) -> Emitted {
        Emitted::to(
            AccountId(i),
            Event::Deposited {
                account: AccountId(i),
                amount: i,
            },
        )
    }

    fn batch(n: u64) -> EventBatch {
        EventBatch {
            seq: 42,
            origin: Some((AccountId(7), 3)),
            events: (0..n).map(deposit).collect(),
        }
    }

    /// Every chunk keeps `seq` and `origin`, and the chunks' events concatenate to the original.
    fn assert_partition(original: &EventBatch, chunks: &[EventBatch]) {
        for c in chunks {
            assert_eq!((c.seq, c.origin), (original.seq, original.origin));
        }
        let joined: Vec<Emitted> = chunks.iter().flat_map(|c| c.events.clone()).collect();
        assert_eq!(joined, original.events);
    }

    #[test]
    fn batch_that_fits_is_one_identical_chunk() {
        let b = batch(5);
        let max = encode(&b).len();
        assert_eq!(split_batch(&b, max), vec![b.clone()]);
    }

    #[test]
    fn batch_without_events_is_kept() {
        let b = batch(0);
        assert_eq!(split_batch(&b, 1), vec![b.clone()]);
    }

    #[test]
    fn oversized_batch_splits_into_chunks_that_each_fit() {
        let b = batch(100);
        let full = encode(&b).len();
        let max = full / 7;
        let chunks = split_batch(&b, max);
        assert!(chunks.len() >= 7, "only {} chunks", chunks.len());
        for c in &chunks {
            let bytes = encode(c);
            assert!(bytes.len() <= max, "chunk of {} bytes > {max}", bytes.len());
            assert!(!c.events.is_empty());
            assert_eq!(decode::<EventBatch>(&bytes).unwrap(), *c);
        }
        assert_partition(&b, &chunks);
    }

    #[test]
    fn chunks_are_filled_greedily() {
        let b = batch(100);
        let max = encode(&b).len() / 4;
        let chunks = split_batch(&b, max);
        assert!(chunks.len() > 1);
        // Moving the next chunk's first event into a chunk would make that chunk too big.
        for pair in chunks.windows(2) {
            let mut grown = pair[0].clone();
            grown.events.push(pair[1].events[0].clone());
            assert!(encode(&grown).len() > max);
        }
    }

    #[test]
    fn event_larger_than_the_limit_travels_alone() {
        let b = batch(3);
        let one_event_batch = EventBatch {
            events: vec![b.events[0].clone()],
            ..b.clone()
        };
        let max = encode(&one_event_batch).len() - 1;
        let chunks = split_batch(&b, max);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.events.len() == 1));
        assert_partition(&b, &chunks);
    }
}
