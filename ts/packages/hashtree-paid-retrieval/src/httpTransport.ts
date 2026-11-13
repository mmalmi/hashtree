/** The paid exchange over HTTP: what gives a browser a way to pay a gateway. */

import { MAX_MESSAGE_BYTES } from './message.js';
import type { PaidBlobTransport } from './transport.js';

/** Path of the exchange below a gateway's base URL. */
export const PAID_BLOB_HTTP_PATH = '/paid-blob';

function failed(what: string): Error {
  return new Error(`paid route gateway ${what}`);
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function endpoint(gateway: string | URL): string {
  let url: URL;
  try {
    url = new URL(gateway);
  } catch {
    throw new TypeError('paid route gateway URL is invalid');
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new TypeError('paid route gateway URL must be http or https');
  }
  let base = url.pathname;
  while (base.endsWith('/')) base = base.slice(0, -1);
  url.pathname = `${base}${PAID_BLOB_HTTP_PATH}`;
  url.hash = '';
  return url.href;
}

/**
 * `POST <gateway>/paid-blob` with one encoded request as the body, answered
 * by `200` with one encoded reply.
 *
 * Every protocol outcome is a `200`. Any other status, a network error, an
 * abort or a reply above the protocol's size limit rejects: a failure, never
 * "the provider has no such blob". Nothing is retried here; the route's
 * requests are safe to repeat, and it decides when.
 *
 * The request names the account, a bearer secret, so it is sent without
 * cookies or a referrer and is not allowed to follow a redirect to another
 * address.
 */
export class HttpPaidBlobTransport implements PaidBlobTransport {
  /** Where requests are sent. */
  readonly url: string;
  private readonly fetch: typeof fetch;

  /**
   * `gateway` is the gateway's base URL. `fetchImplementation` replaces the
   * global `fetch`, for a test or a runtime without one.
   */
  constructor(gateway: string | URL, fetchImplementation?: typeof fetch) {
    this.url = endpoint(gateway);
    if (!fetchImplementation && typeof globalThis.fetch !== 'function') {
      throw new TypeError('paid route gateway needs a fetch implementation');
    }
    this.fetch = fetchImplementation ?? ((input, init) => globalThis.fetch(input, init));
  }

  async exchange(request: Uint8Array, signal?: AbortSignal): Promise<Uint8Array> {
    if (signal?.aborted) throw failed('request was aborted');
    let response: Response;
    try {
      response = await this.fetch(this.url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/octet-stream' },
        body: new Uint8Array(request),
        signal,
        cache: 'no-store',
        credentials: 'omit',
        redirect: 'error',
        referrerPolicy: 'no-referrer',
      });
    } catch (error) {
      throw failed(signal?.aborted ? 'request was aborted' : `request failed: ${describe(error)}`);
    }
    if (response.status !== 200) {
      void response.body?.cancel().catch(() => undefined);
      throw failed(`answered HTTP ${response.status}`);
    }
    const declared = Number(response.headers.get('Content-Length') ?? '0');
    if (declared > MAX_MESSAGE_BYTES) {
      void response.body?.cancel().catch(() => undefined);
      throw failed('reply is larger than a paid-retrieval message');
    }
    return readReply(response, signal);
  }
}

/** The whole body, given up as soon as it is too large or nobody is waiting. */
async function readReply(response: Response, signal?: AbortSignal): Promise<Uint8Array> {
  const reader = response.body?.getReader();
  if (!reader) return new Uint8Array();
  const abandon = (): void => {
    void reader.cancel().catch(() => undefined);
  };
  signal?.addEventListener('abort', abandon, { once: true });
  const chunks: Uint8Array[] = [];
  let length = 0;
  let unreadable: string | undefined;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done || signal?.aborted) break;
      length += value.length;
      if (length > MAX_MESSAGE_BYTES) break;
      chunks.push(value);
    }
  } catch (error) {
    unreadable = describe(error);
  } finally {
    signal?.removeEventListener('abort', abandon);
  }
  if (signal?.aborted) throw failed('request was aborted');
  if (unreadable !== undefined) throw failed(`reply could not be read: ${unreadable}`);
  if (length > MAX_MESSAGE_BYTES) {
    abandon();
    throw failed('reply is larger than a paid-retrieval message');
  }
  const reply = new Uint8Array(length);
  let at = 0;
  for (const chunk of chunks) {
    reply.set(chunk, at);
    at += chunk.length;
  }
  return reply;
}
