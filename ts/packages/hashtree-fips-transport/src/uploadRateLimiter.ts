type UploadRateLimiterConfig = {
  bytesPerSecond?: number | null;
  now?: () => number;
};

type UploadReservation = {
  allowed: boolean;
  delayMs: number;
};

function normalizeBytesPerSecond(value?: number | null): number | null {
  if (!Number.isFinite(value) || !value || value <= 0) {
    return null;
  }
  return Math.floor(value);
}

export class UploadRateLimiter {
  private bytesPerSecond: number | null;
  private availableBytes: number;
  private lastRefillMs: number;
  private readonly now: () => number;

  constructor(config: UploadRateLimiterConfig = {}) {
    this.now = config.now ?? (() => performance.now());
    this.bytesPerSecond = normalizeBytesPerSecond(config.bytesPerSecond);
    this.availableBytes = this.bytesPerSecond ?? Number.POSITIVE_INFINITY;
    this.lastRefillMs = this.now();
  }

  setBytesPerSecond(bytesPerSecond?: number | null): void {
    const nowMs = this.now();
    this.refill(nowMs);
    this.bytesPerSecond = normalizeBytesPerSecond(bytesPerSecond);
    this.availableBytes = this.bytesPerSecond
      ? Math.min(this.availableBytes, this.bytesPerSecond)
      : Number.POSITIVE_INFINITY;
    this.lastRefillMs = nowMs;
  }

  getBytesPerSecond(): number | null {
    return this.bytesPerSecond;
  }

  reserve(byteLength: number): UploadReservation {
    if (byteLength <= 0) {
      return { allowed: true, delayMs: 0 };
    }

    const limit = this.bytesPerSecond;
    if (!limit) {
      return { allowed: true, delayMs: 0 };
    }

    const nowMs = this.now();
    this.refill(nowMs);

    if (this.availableBytes >= byteLength) {
      this.availableBytes = Math.max(0, this.availableBytes - byteLength);
      return { allowed: true, delayMs: 0 };
    }

    const missingBytes = byteLength - this.availableBytes;
    return {
      allowed: false,
      delayMs: Math.max(4, Math.ceil((missingBytes / limit) * 1000)),
    };
  }

  refund(byteLength: number): void {
    this.refill(this.now());
    if (this.bytesPerSecond && byteLength > 0) {
      this.availableBytes = Math.min(this.bytesPerSecond, this.availableBytes + byteLength);
    }
  }

  private refill(nowMs: number): void {
    const limit = this.bytesPerSecond;
    if (!limit) {
      this.availableBytes = Number.POSITIVE_INFINITY;
      this.lastRefillMs = nowMs;
      return;
    }

    const elapsedMs = Math.max(0, nowMs - this.lastRefillMs);
    this.lastRefillMs = nowMs;
    this.availableBytes = Math.min(
      limit,
      this.availableBytes + (elapsedMs * limit) / 1000,
    );
  }
}
