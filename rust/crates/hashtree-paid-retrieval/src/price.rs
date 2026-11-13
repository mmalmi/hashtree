//! Millisat prices for whole blobs.

/// `msat` for every `per_bytes` bytes, rounded up. The same shape as the
/// byte price used by fips pay-to-relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BytePrice {
    pub msat: u64,
    pub per_bytes: u64,
}

impl BytePrice {
    pub const FREE: Self = Self {
        msat: 0,
        per_bytes: 1,
    };

    pub const fn new(msat: u64, per_bytes: u64) -> Self {
        Self { msat, per_bytes }
    }

    /// Amount due for `bytes`, saturating instead of overflowing.
    pub fn amount_msat(&self, bytes: u64) -> u64 {
        let per_bytes = u128::from(self.per_bytes.max(1));
        let due = (u128::from(bytes) * u128::from(self.msat)).div_ceil(per_bytes);
        u64::try_from(due).unwrap_or(u64::MAX)
    }
}

/// What a provider charges, or the most a client accepts, for one blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tariff {
    pub rate: BytePrice,
    /// Smallest price of any blob, covering per-request work.
    pub min_msat: u64,
}

impl Tariff {
    pub const FREE: Self = Self {
        rate: BytePrice::FREE,
        min_msat: 0,
    };

    pub fn price_msat(&self, bytes: u64) -> u64 {
        self.rate.amount_msat(bytes).max(self.min_msat)
    }
}

/// Whole sats needed to cover `msat`.
pub fn sat_ceil(msat: u64) -> u64 {
    msat.div_ceil(1_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_round_up_and_respect_the_minimum() {
        let tariff = Tariff {
            rate: BytePrice::new(1, 1024),
            min_msat: 5,
        };
        assert_eq!(tariff.price_msat(0), 5);
        assert_eq!(tariff.price_msat(1), 5);
        assert_eq!(tariff.price_msat(1024 * 6), 6);
        assert_eq!(tariff.price_msat(1024 * 6 + 1), 7);
        assert_eq!(Tariff::FREE.price_msat(16 << 20), 0);
        assert_eq!(BytePrice::new(u64::MAX, 1).amount_msat(u64::MAX), u64::MAX);
        assert_eq!(BytePrice::new(3, 0).amount_msat(2), 6);
        assert_eq!(sat_ceil(1), 1);
        assert_eq!(sat_ceil(1_000), 1);
        assert_eq!(sat_ceil(1_001), 2);
    }
}
