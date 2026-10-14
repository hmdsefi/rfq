//! `Transport` adapters: [`MemTransport`] (tests). The Aeron transport comes with the `aeron`
//! feature; byte-level adapters decode ingress with [`crate::codec::decode_ingress`] and split
//! oversized egress batches with [`split_batch`].
mod mem;
mod split;

pub use mem::MemTransport;
pub use split::split_batch;
