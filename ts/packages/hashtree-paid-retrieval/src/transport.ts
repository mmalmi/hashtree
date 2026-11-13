/**
 * Delivers one encoded request to one provider and resolves with its encoded
 * reply. Any carrier that can do that fits: a length-prefixed byte stream, a
 * BitTorrent BEP 10 extended message, an HTTP POST.
 *
 * The route stops waiting at its deadline and aborts `signal`; an
 * implementation must stay usable after an abandoned exchange. Reject when the
 * reply cannot be had: a rejection is a failure, never "the provider has no
 * such blob".
 */
export interface PaidBlobTransport {
  exchange(request: Uint8Array, signal?: AbortSignal): Promise<Uint8Array>;
}
