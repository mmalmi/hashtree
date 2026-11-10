/**
 * Bytes in a form WebCrypto accepts.
 *
 * WebCrypto rejects views over a SharedArrayBuffer, so those are copied.
 * A view over an ordinary ArrayBuffer is passed as it is: WebCrypto reads
 * exactly the view's bytes and snapshots them before returning its promise.
 */
export function cryptoInput(data: Uint8Array): BufferSource {
  if (data.buffer instanceof ArrayBuffer) {
    return data as BufferSource;
  }
  const copy = new Uint8Array(new ArrayBuffer(data.length));
  copy.set(data);
  return copy;
}
