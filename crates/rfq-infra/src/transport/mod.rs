//! `Transport` adapters: [`MemTransport`] (tests). The Aeron transport comes with the `aeron`
//! feature; byte-level adapters decode ingress with [`crate::codec::decode_ingress`].
mod mem;

pub use mem::MemTransport;
