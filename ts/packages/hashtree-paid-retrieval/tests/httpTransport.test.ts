import { describe, expect, it } from 'vitest';
import type { Hash } from '@hashtree/core';
import {
  HttpPaidBlobTransport,
  MAX_MESSAGE_BYTES,
  PaidBlobRoute,
  encodeRequest,
} from '../src/index.js';
import { FakeSeller, FakeWallet, MINT, TARIFF } from './support/fakes.js';

const GATEWAY = 'https://gateway.test';
const POLICY = { maxTariff: TARIFF, mints: [MINT], topupSat: 5n, maxTokenFeeSat: 0n };

type Fetch = typeof fetch;
interface Call {
  url: string;
  init: RequestInit;
}

/** A gateway in front of the test seller, as `fetch` sees it. */
function gateway(seller: FakeSeller, calls: Call[] = []): Fetch {
  return async (input, init = {}) => {
    calls.push({ url: String(input), init });
    const reply = await seller.handle(undefined, init.body as Uint8Array);
    return new Response(new Uint8Array(reply), {
      status: 200,
      headers: { 'Content-Type': 'application/octet-stream', 'Cache-Control': 'no-store' },
    });
  };
}

function answering(respond: () => Response): { fetch: Fetch; calls: number } {
  const state = {
    calls: 0,
    fetch: (async () => {
      state.calls += 1;
      return respond();
    }) as Fetch,
  };
  return state;
}

/** A `fetch` that answers only by failing when its signal aborts. */
function hanging(seen: AbortSignal[] = []): Fetch {
  return (_input, init) => new Promise((_resolve, reject) => {
    const signal = init?.signal as AbortSignal;
    seen.push(signal);
    signal.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
  });
}

function route(fetchImplementation: Fetch, wallet: FakeWallet, timeoutMs = 10_000): PaidBlobRoute {
  return new PaidBlobRoute({
    provider: 'gateway',
    transport: new HttpPaidBlobTransport(GATEWAY, fetchImplementation),
    wallet,
    policy: { ...POLICY, requestTimeoutMs: timeoutMs },
  });
}

async function setup(): Promise<{ seller: FakeSeller; wallet: FakeWallet; hash: Hash; data: Uint8Array }> {
  const seller = new FakeSeller();
  const blob = await seller.add(1, 4_096);
  return { seller, wallet: new FakeWallet(seller.mint, 21n), ...blob };
}

const GET = encodeRequest({
  type: 'get',
  hash: { algorithm: 'sha256', digest: new Uint8Array(32) },
  account: new Uint8Array(32),
  maxMsat: 0n,
});

describe('HttpPaidBlobTransport', () => {
  it('pays for and fetches a blob through a gateway', async () => {
    const { seller, wallet, hash, data } = await setup();
    const calls: Call[] = [];
    const paid = route(gateway(seller, calls), wallet);

    const reply = await paid.read({ hash, htl: 0 });
    expect(reply.type === 'data' && reply.data).toEqual(data);
    expect([wallet.payments, seller.received]).toEqual([1, 5n]);
    expect(paid.accountState()).toMatchObject({ creditedMsat: 5_000n, balanceMsat: 4_000n });

    expect(calls.map((call) => call.url), 'get, pay, get').toEqual(
      Array(3).fill(`${GATEWAY}/paid-blob`),
    );
    expect(calls[0].init).toMatchObject({
      method: 'POST',
      headers: { 'Content-Type': 'application/octet-stream' },
      cache: 'no-store',
      credentials: 'omit',
      redirect: 'error',
      referrerPolicy: 'no-referrer',
    });
  });

  it('adds the endpoint below the base URL it is given', () => {
    const fetchImplementation = gateway(new FakeSeller());
    const url = (base: string | URL): string => new HttpPaidBlobTransport(base, fetchImplementation).url;
    expect(url('https://gateway.test/')).toBe('https://gateway.test/paid-blob');
    expect(url('http://localhost:8080/htree//')).toBe('http://localhost:8080/htree/paid-blob');
    expect(url(new URL('https://gateway.test/a#b'))).toBe('https://gateway.test/a/paid-blob');
    expect(() => url('gateway.test')).toThrow('paid route gateway URL is invalid');
    expect(() => url('ftp://gateway.test')).toThrow('must be http or https');
  });

  it('treats any status but 200 as a failure, never as a miss', async () => {
    const { wallet, hash } = await setup();
    for (const status of [204, 402, 404, 500]) {
      const gatewayDown = answering(() => new Response(null, { status }));
      await expect(new HttpPaidBlobTransport(GATEWAY, gatewayDown.fetch).exchange(GET))
        .rejects.toThrow(`paid route gateway answered HTTP ${status}`);
      await expect(route(gatewayDown.fetch, wallet).read({ hash, htl: 0 }))
        .rejects.toThrow(`HTTP ${status}`);
      expect(gatewayDown.calls, 'and does not retry by itself').toBe(2);
    }
    expect(wallet.payments).toBe(0);
  });

  it('fails when the request cannot be made', async () => {
    const { wallet, hash } = await setup();
    const offline = answering(() => {
      throw new TypeError('Failed to fetch');
    });
    await expect(new HttpPaidBlobTransport(GATEWAY, offline.fetch).exchange(GET))
      .rejects.toThrow('paid route gateway request failed: Failed to fetch');
    await expect(route(offline.fetch, wallet).read({ hash, htl: 0 }))
      .rejects.toThrow('request failed');
    expect(offline.calls).toBe(2);
  });

  it('fails when the request is aborted', async () => {
    const { wallet, hash } = await setup();
    const transport = new HttpPaidBlobTransport(GATEWAY, hanging());
    const cancel = new AbortController();
    const waiting = transport.exchange(GET, cancel.signal);
    cancel.abort();
    await expect(waiting).rejects.toThrow('paid route gateway request was aborted');

    // Already aborted: the gateway is not contacted at all.
    const unused = answering(() => new Response(null, { status: 200 }));
    await expect(new HttpPaidBlobTransport(GATEWAY, unused.fetch).exchange(GET, cancel.signal))
      .rejects.toThrow('request was aborted');
    expect(unused.calls).toBe(0);

    // The route's deadline aborts the request it gave up on.
    const seen: AbortSignal[] = [];
    await expect(route(hanging(seen), wallet, 20).read({ hash, htl: 0 }))
      .rejects.toThrow('paid route timed out; availability is unknown');
    expect(seen.map((signal) => signal.aborted)).toEqual([true]);
  });

  it('stops reading a reply when the request is aborted', async () => {
    let cancelled = false;
    let chunks = 0;
    // One chunk arrives, then the body stalls.
    const stalled = new ReadableStream<Uint8Array>({
      pull: (controller) => {
        chunks += 1;
        if (chunks === 1) controller.enqueue(new Uint8Array(1024));
        return chunks === 1 ? undefined : new Promise<void>(() => undefined);
      },
      cancel: () => {
        cancelled = true;
      },
    }, { highWaterMark: 0 });
    const cancel = new AbortController();
    const transport = new HttpPaidBlobTransport(GATEWAY, async () => {
      setTimeout(() => cancel.abort(), 5);
      return new Response(stalled, { status: 200 });
    });
    await expect(transport.exchange(GET, cancel.signal)).rejects.toThrow('request was aborted');
    expect(cancelled).toBe(true);
  });

  it('refuses a reply larger than a message can be', async () => {
    const declared = answering(() => new Response(new Uint8Array(8), {
      status: 200,
      headers: { 'Content-Length': String(MAX_MESSAGE_BYTES + 1) },
    }));
    await expect(new HttpPaidBlobTransport(GATEWAY, declared.fetch).exchange(GET))
      .rejects.toThrow('paid route gateway reply is larger than a paid-retrieval message');

    // No declared length: the body is given up as soon as it is too long.
    const megabyte = new Uint8Array(1024 * 1024);
    let sent = 0;
    let cancelled = false;
    const undeclared = answering(() => new Response(new ReadableStream<Uint8Array>({
      pull: (controller) => {
        sent += 1;
        controller.enqueue(megabyte);
      },
      cancel: () => {
        cancelled = true;
      },
    }, { highWaterMark: 0 }), { status: 200 }));
    await expect(new HttpPaidBlobTransport(GATEWAY, undeclared.fetch).exchange(GET))
      .rejects.toThrow('reply is larger than a paid-retrieval message');
    expect(cancelled).toBe(true);
    expect(sent).toBeLessThan(20);
  });

  it('accepts a reply of exactly the largest message', async () => {
    const largest = answering(() => new Response(new Uint8Array(MAX_MESSAGE_BYTES), { status: 200 }));
    const reply = await new HttpPaidBlobTransport(GATEWAY, largest.fetch).exchange(GET);
    expect(reply.length).toBe(MAX_MESSAGE_BYTES);
  });
});
