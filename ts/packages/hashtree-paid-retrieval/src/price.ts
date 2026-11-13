/**
 * Millisat prices for whole blobs.
 *
 * Amounts of money are `bigint` everywhere in this package: the wire carries
 * signed 64-bit millisats and the arithmetic must match the Rust side exactly.
 */

export const U64_MAX = 2n ** 64n - 1n;

/** `msat` for every `perBytes` bytes, rounded up. */
export interface BytePrice {
  msat: bigint;
  perBytes: bigint;
}

/** What a provider charges, or the most a client accepts, for one blob. */
export interface Tariff {
  rate: BytePrice;
  /** Smallest price of any blob, covering per-request work. */
  minMsat: bigint;
}

export const FREE_TARIFF: Tariff = Object.freeze({
  rate: Object.freeze({ msat: 0n, perBytes: 1n }),
  minMsat: 0n,
});

/** Clamp to the unsigned 64-bit range the Rust ledger saturates in. */
export function saturate(value: bigint): bigint {
  if (value < 0n) return 0n;
  return value > U64_MAX ? U64_MAX : value;
}

/** Amount due for `bytes`, saturating instead of overflowing. */
export function amountMsat(rate: BytePrice, bytes: bigint): bigint {
  const perBytes = rate.perBytes > 1n ? rate.perBytes : 1n;
  return saturate((bytes * rate.msat + perBytes - 1n) / perBytes);
}

export function priceMsat(tariff: Tariff, bytes: bigint): bigint {
  const due = amountMsat(tariff.rate, bytes);
  return due > tariff.minMsat ? due : tariff.minMsat;
}

/** Whole sats needed to cover `msat`. */
export function satCeil(msat: bigint): bigint {
  return (msat + 999n) / 1000n;
}
