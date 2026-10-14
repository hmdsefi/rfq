# Task 1: Workspace and core types

> Part of the [implementation roadmap](../2026-10-13-rfq-engine-plan.md). Its **Global Constraints** and **Layered structure** table apply to this task and supersede crate names/paths written here.

**Files:**
- Create: `Cargo.toml`, `crates/rfq-core/Cargo.toml`, `crates/rfq-core/src/lib.rs`, `crates/rfq-core/src/types.rs`

**Interfaces:**
- Produces: `Amount`, `Qty`, `Timestamp`, `Duration` (all `u64`), `ONE_DOLLAR`, ids `AccountId/ContractId/RequestId/QuoteId/PositionId/HoldId(pub u64)`, `Price::new(u16) -> Option<Price>`, `Price::bps()`, `Price::complement()`, `Side::{BuyYes, BuyNo}`, `Outcome::{Yes, No, Invalid}`, `buyer_lock(Qty, Price) -> Option<Amount>`, `seller_lock(Qty, Price) -> Option<Amount>`.

- [ ] **Step 1: Create the workspace**

`Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["crates/rfq-core"]

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
bincode = "1.3"
sha2 = "0.10"
proptest = "1"
```
`crates/rfq-core/Cargo.toml`:
```toml
[package]
name = "rfq-core"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { workspace = true }
bincode = { workspace = true }
sha2 = { workspace = true }

[dev-dependencies]
proptest = { workspace = true }
```
Append to the root `Cargo.toml`:
```toml
# Unchecked overflow must never silently wrap money, even in optimised builds.
[profile.release]
overflow-checks = true

[profile.bench]
overflow-checks = true
```
`clippy.toml` (workspace root):
```toml
allow-unwrap-in-tests = true
```
`crates/rfq-core/src/lib.rs`:
```rust
//! Deterministic RFQ engine: no I/O, no clock, no randomness.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod types;

pub use types::*;
```
Later tasks rewrite `lib.rs` with more modules. **Keep these attributes at the top every time.**

- [ ] **Step 2: Write the failing tests**

`crates/rfq-core/src/types.rs`: start with only the test module.
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_bounds() {
        assert!(Price::new(0).is_none());
        assert!(Price::new(10_000).is_none());
        assert_eq!(Price::new(1).unwrap().bps(), 1);
        assert_eq!(Price::new(9_999).unwrap().bps(), 9_999);
    }

    #[test]
    fn locks_sum_to_one_dollar_per_share() {
        for bps in [1u16, 37, 5_000, 6_000, 9_999] {
            let p = Price::new(bps).unwrap();
            assert_eq!(buyer_lock(1_000, p).unwrap() + seller_lock(1_000, p).unwrap(), 1_000 * ONE_DOLLAR);
        }
    }

    #[test]
    fn spec_example_60_cents() {
        let p = Price::new(6_000).unwrap();
        assert_eq!(buyer_lock(1_000, p), Some(600 * ONE_DOLLAR));
        assert_eq!(seller_lock(1_000, p), Some(400 * ONE_DOLLAR));
    }

    #[test]
    fn overflow_is_none() {
        assert_eq!(buyer_lock(u64::MAX, Price::new(2).unwrap()), None);
    }

    #[test]
    fn deserializing_out_of_range_price_fails() {
        let bytes = bincode::serialize(&0u16).unwrap();
        assert!(bincode::deserialize::<Price>(&bytes).is_err());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p rfq-core`
Expected: FAIL to compile (`Price`, `buyer_lock` not found).

- [ ] **Step 4: Implement the types**

Prepend to `crates/rfq-core/src/types.rs`:
```rust
use serde::{Deserialize, Serialize};

/// Money in micro-USD. `$1 = 1_000_000`.
pub type Amount = u64;
/// Number of shares; each share pays $1 to the winning side.
pub type Qty = u64;
/// Milliseconds since the Unix epoch. Only the sequencer stamps time.
pub type Timestamp = u64;
/// Milliseconds.
pub type Duration = u64;

pub const ONE_DOLLAR: Amount = 1_000_000;
const MICROS_PER_BP: Amount = 100;

macro_rules! id_type {
    ($($name:ident),*) => {$(
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub u64);
    )*};
}
id_type!(AccountId, ContractId, RequestId, QuoteId, PositionId, HoldId);

/// Price of the side the requester buys, in basis points of $1, strictly inside (0, 10_000).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct Price(u16);

impl Price {
    pub const fn new(bps: u16) -> Option<Price> {
        if bps >= 1 && bps <= 9_999 { Some(Price(bps)) } else { None }
    }
    pub fn bps(self) -> u16 { self.0 }
    /// Price of the opposite side: `10_000 - bps`.
    pub fn complement(self) -> Price { Price(10_000 - self.0) }
}

impl TryFrom<u16> for Price {
    type Error = &'static str;
    fn try_from(bps: u16) -> Result<Self, Self::Error> {
        Price::new(bps).ok_or("price must be within 1..=9999 bps")
    }
}

impl From<Price> for u16 {
    fn from(p: Price) -> u16 { p.0 }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Side { BuyYes, BuyNo }

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Outcome { Yes, No, Invalid }

/// Collateral the buyer of `qty` shares at `price` locks: `qty × bps × 100` micro-USD.
pub fn buyer_lock(qty: Qty, price: Price) -> Option<Amount> {
    qty.checked_mul(u64::from(price.0))?.checked_mul(MICROS_PER_BP)
}

/// Collateral the counterparty locks: the buyer lock at the complement price.
pub fn seller_lock(qty: Qty, price: Price) -> Option<Amount> {
    buyer_lock(qty, price.complement())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rfq-core && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`
Expected: 5 passed; clippy and fmt are clean.

- [ ] **Step 6: Commit and push**

```bash
git add Cargo.toml clippy.toml crates/rfq-core
git commit -m "feat(core): money, price and id types with exact collateral math; strict lints"
git push
```

