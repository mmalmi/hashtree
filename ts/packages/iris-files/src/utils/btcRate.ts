/**
 * Bitcoin exchange rate helper with 24-hour caching
 * Fetches BTC/USD rate from Coinbase and caches in localStorage
 */

const CACHE_KEY = 'btc-usd-rate';
const CACHE_TTL_MS = 24 * 60 * 60 * 1000; // 24 hours

interface CachedRate {
  rate: number; // USD per BTC
  timestamp: number;
}

let inFlightRequest: Promise<number> | null = null;

/**
 * Get the cached rate from localStorage
 */
function getCachedRate(): CachedRate | null {
  try {
    const cached = localStorage.getItem(CACHE_KEY);
    if (!cached) return null;
    return JSON.parse(cached) as CachedRate;
  } catch {
    return null;
  }
}

/**
 * Save rate to localStorage cache
 */
function setCachedRate(rate: number): void {
  try {
    const cached: CachedRate = {
      rate,
      timestamp: Date.now(),
    };
    localStorage.setItem(CACHE_KEY, JSON.stringify(cached));
  } catch {
    // Ignore localStorage errors
  }
}

/**
 * Fetch BTC/USD rate from Coinbase API
 */
async function fetchFromCoinbase(): Promise<number> {
  const response = await fetch('https://api.coinbase.com/v2/prices/BTC-USD/spot');
  if (!response.ok) throw new Error(`Coinbase: ${response.status}`);
  const data = await response.json();
  const rate = parseFloat(data.data.amount);
  if (isNaN(rate) || rate <= 0) throw new Error('Coinbase: invalid rate');
  return rate;
}

/**
 * Fetch BTC/USD rate from CoinGecko API
 */
async function fetchFromCoinGecko(): Promise<number> {
  const response = await fetch('https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd');
  if (!response.ok) throw new Error(`CoinGecko: ${response.status}`);
  const data = await response.json();
  const rate = data.bitcoin?.usd;
  if (typeof rate !== 'number' || rate <= 0) throw new Error('CoinGecko: invalid rate');
  return rate;
}

/**
 * Fetch BTC/USD rate from Kraken API
 */
async function fetchFromKraken(): Promise<number> {
  const response = await fetch('https://api.kraken.com/0/public/Ticker?pair=XBTUSD');
  if (!response.ok) throw new Error(`Kraken: ${response.status}`);
  const data = await response.json();
  const rate = parseFloat(data.result?.XXBTZUSD?.c?.[0]);
  if (isNaN(rate) || rate <= 0) throw new Error('Kraken: invalid rate');
  return rate;
}

/**
 * Fetch BTC/USD rate with fallbacks
 */
async function fetchRate(): Promise<number> {
  const sources = [fetchFromCoinbase, fetchFromCoinGecko, fetchFromKraken];

  for (const fetchFn of sources) {
    try {
      return await fetchFn();
    } catch (e) {
      console.warn('[btcRate]', e instanceof Error ? e.message : e);
    }
  }

  throw new Error('All BTC rate sources failed');
}

/**
 * Get BTC/USD exchange rate (USD per BTC)
 * Returns cached value if less than 24 hours old, otherwise fetches fresh
 * Falls back to stale cache if all APIs fail
 */
export async function getBtcUsdRate(): Promise<number> {
  // Check cache first
  const cached = getCachedRate();
  if (cached && Date.now() - cached.timestamp < CACHE_TTL_MS) {
    return cached.rate;
  }

  // Deduplicate concurrent requests
  if (inFlightRequest) {
    return inFlightRequest;
  }

  inFlightRequest = (async () => {
    try {
      const rate = await fetchRate();
      setCachedRate(rate);
      return rate;
    } catch (e) {
      // If all APIs fail, use stale cache as fallback
      if (cached) {
        console.warn('[btcRate] Using stale cached rate from', new Date(cached.timestamp).toLocaleString());
        return cached.rate;
      }
      throw e;
    } finally {
      inFlightRequest = null;
    }
  })();

  return inFlightRequest;
}

/**
 * Convert USD to satoshis
 * @param usd - Amount in USD
 * @param btcUsdRate - BTC/USD exchange rate (USD per BTC)
 * @returns Amount in satoshis
 */
export function usdToSats(usd: number, btcUsdRate: number): number {
  if (btcUsdRate <= 0) return 0;
  const btc = usd / btcUsdRate;
  const sats = Math.round(btc * 100_000_000);
  return sats;
}

/**
 * Convert satoshis to USD
 * @param sats - Amount in satoshis
 * @param btcUsdRate - BTC/USD exchange rate (USD per BTC)
 * @returns Amount in USD
 */
export function satsToUsd(sats: number, btcUsdRate: number): number {
  if (btcUsdRate <= 0) return 0;
  const btc = sats / 100_000_000;
  return btc * btcUsdRate;
}
