//! Splitting an egress batch so that each message fits the transport's maximum message length.
//!
//! The codec never limits what the node produces (ruling R21), but an Aeron publication rejects a
//! message longer than its max message length. An [`EventBatch`] over that limit is published as
//! several chunks with the same `seq`, `time`, `kind` and `origin`; their events, concatenated in
//! order, are the original events. The batch's `balances` travel once, on the last chunk, so a
//! consumer that needs the whole batch joins consecutive chunks with equal `seq`.
use rfq_app::EventBatch;

use crate::codec::encoded_len;

/// Splits `batch` into chunks whose encodings are at most `max_bytes` each, filling every chunk
/// greedily in event order. A batch that already fits, or has no events, is returned as the only
/// chunk, balances and all. A single event too big for any chunk travels alone, in a chunk over
/// `max_bytes`; the transport's publish of that chunk then fails.
///
/// The balances go on the last chunk when they fit there. When they do not, they travel in a final
/// chunk of their own, with no events. If the balances alone are over `max_bytes`, that chunk is
/// over it too, and the transport's publish of it fails, like an oversized event.
pub fn split_batch(batch: &EventBatch, max_bytes: usize) -> Vec<EventBatch> {
    if batch.events.is_empty() || encoded_len(batch) <= max_bytes {
        return vec![batch.clone()];
    }
    let empty = EventBatch {
        seq: batch.seq,
        time: batch.time,
        kind: batch.kind,
        origin: batch.origin,
        events: Vec::new(),
        balances: Vec::new(),
    };
    // bincode (fixint) writes a Vec as a fixed-width length followed by its elements, so a chunk's
    // size is the empty batch's size plus its events' sizes plus its balances' sizes. `encoded_len`
    // counts the version byte, which a message carries once, not once per event.
    let base = encoded_len(&empty);
    let balances_len = encoded_len(&batch.balances) - encoded_len(&empty.balances);
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
    if size + balances_len <= max_bytes {
        current.balances = batch.balances.clone();
        size += balances_len;
        debug_assert_eq!(size, encoded_len(&current));
        chunks.push(current);
    } else {
        chunks.push(current);
        chunks.push(EventBatch {
            balances: batch.balances.clone(),
            ..empty
        });
    }
    chunks
}

#[cfg(test)]
mod tests {
    use rfq_app::BatchKind;
    use rfq_domain::{AccountId, Balance, Emitted, Event};

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

    fn balances(n: u64) -> Vec<(AccountId, Balance)> {
        (0..n)
            .map(|i| {
                (
                    AccountId(i),
                    Balance {
                        free: 100 + i,
                        reserved: 10,
                        escrowed: 1,
                    },
                )
            })
            .collect()
    }

    fn batch(n: u64) -> EventBatch {
        EventBatch {
            seq: 42,
            time: 9_000,
            kind: BatchKind::User,
            origin: Some((AccountId(7), 3)),
            events: (0..n).map(deposit).collect(),
            balances: balances(2),
        }
    }

    /// Every chunk keeps `seq`, `time`, `kind` and `origin`; the chunks' events concatenate to the
    /// original; only the last chunk has the balances, and they equal the original's.
    fn assert_partition(original: &EventBatch, chunks: &[EventBatch]) {
        for c in chunks {
            assert_eq!(
                (c.seq, c.time, c.kind, c.origin),
                (original.seq, original.time, original.kind, original.origin)
            );
        }
        let joined: Vec<Emitted> = chunks.iter().flat_map(|c| c.events.clone()).collect();
        assert_eq!(joined, original.events);
        let (last, rest) = chunks.split_last().expect("at least one chunk");
        assert!(
            rest.iter().all(|c| c.balances.is_empty()),
            "balances only on the last chunk"
        );
        assert_eq!(last.balances, original.balances);
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
        assert_eq!(split_batch(&b, 1), vec![b.clone()], "balances and all");
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
            // (A last chunk of balances alone has no event to move.)
            let Some(next) = pair[1].events.first() else {
                continue;
            };
            let mut grown = pair[0].clone();
            grown.events.push(next.clone());
            assert!(encode(&grown).len() > max);
        }
    }

    #[test]
    fn event_larger_than_the_limit_travels_alone() {
        let b = batch(3);
        let one_event_batch = EventBatch {
            events: vec![b.events[0].clone()],
            balances: Vec::new(),
            ..b.clone()
        };
        let max = encode(&one_event_batch).len() - 1;
        let chunks = split_batch(&b, max);
        // The three events, then the balances, which do not fit on the last event's chunk.
        assert_eq!(chunks.len(), 4);
        assert!(chunks[..3].iter().all(|c| c.events.len() == 1));
        assert_partition(&b, &chunks);
    }

    #[test]
    fn split_batch_keeps_balances_on_the_last_chunk() {
        let b = batch(100);
        let max = encode(&b).len() / 7;
        let chunks = split_batch(&b, max);
        assert!(chunks.len() >= 7, "only {} chunks", chunks.len());
        assert_partition(&b, &chunks);
        for c in &chunks {
            let bytes = encode(c);
            assert!(bytes.len() <= max, "chunk of {} bytes > {max}", bytes.len());
            assert_eq!(decode::<EventBatch>(&bytes).unwrap(), *c);
        }
        assert_eq!(chunks.last().map(|c| c.balances.clone()), Some(balances(2)));
    }

    #[test]
    fn balances_ride_on_the_last_chunks_events_when_they_fit() {
        let b = batch(100);
        let max = encode(&b).len() / 4;
        let chunks = split_batch(&b, max);
        let last = chunks.last().expect("a chunk");
        assert!(
            !last.events.is_empty(),
            "no chunk of balances alone when they fit"
        );
        assert_partition(&b, &chunks);
    }

    /// `max` that holds all three events but not the balances too.
    fn three_events_but_no_room_for_balances(b: &EventBatch) -> usize {
        encode(&EventBatch {
            balances: Vec::new(),
            ..b.clone()
        })
        .len()
    }

    #[test]
    fn balances_that_do_not_fit_with_the_last_events_get_a_final_chunk_of_their_own() {
        let b = batch(3);
        let max = three_events_but_no_room_for_balances(&b);
        assert!(encode(&b).len() > max);
        let chunks = split_batch(&b, max);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].events, b.events);
        assert!(chunks[1].events.is_empty());
        assert!(chunks.iter().all(|c| encode(c).len() <= max));
        assert_partition(&b, &chunks);
    }

    #[test]
    fn balances_over_the_limit_travel_alone_on_the_last_chunk() {
        let b = EventBatch {
            balances: balances(50),
            ..batch(3)
        };
        let max = three_events_but_no_room_for_balances(&b);
        let chunks = split_batch(&b, max);
        assert_eq!(chunks.len(), 2);
        assert!(encode(&chunks[0]).len() <= max);
        assert!(
            encode(&chunks[1]).len() > max,
            "the transport's publish of it fails"
        );
        assert_partition(&b, &chunks);
    }
}
