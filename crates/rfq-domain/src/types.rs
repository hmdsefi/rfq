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
id_type!(AccountId, MarketId, RequestId, QuoteId, PositionId, HoldId);

/// Price of the side the requester buys, in basis points of $1, strictly inside (0, 10_000).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct Price(u16);

impl Price {
    pub const fn new(bps: u16) -> Option<Price> {
        if bps >= 1 && bps <= 9_999 {
            Some(Price(bps))
        } else {
            None
        }
    }
    pub fn bps(self) -> u16 {
        self.0
    }
    /// Price of the opposite side: `10_000 - bps`.
    pub fn complement(self) -> Price {
        Price(10_000 - self.0)
    }
}

impl TryFrom<u16> for Price {
    type Error = &'static str;
    fn try_from(bps: u16) -> Result<Self, Self::Error> {
        Price::new(bps).ok_or("price must be within 1..=9999 bps")
    }
}

impl From<Price> for u16 {
    fn from(p: Price) -> u16 {
        p.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Side {
    BuyYes,
    BuyNo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Outcome {
    Yes,
    No,
    Invalid,
}

/// Collateral the buyer of `qty` shares at `price` locks: `qty × bps × 100` micro-USD.
pub fn buyer_lock(qty: Qty, price: Price) -> Option<Amount> {
    qty.checked_mul(u64::from(price.0))?.checked_mul(MICROS_PER_BP)
}

/// Collateral the counterparty locks: the buyer lock at the complement price.
pub fn seller_lock(qty: Qty, price: Price) -> Option<Amount> {
    buyer_lock(qty, price.complement())
}

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
            assert_eq!(
                buyer_lock(1_000, p).unwrap() + seller_lock(1_000, p).unwrap(),
                1_000 * ONE_DOLLAR
            );
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
